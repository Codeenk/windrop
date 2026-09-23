//! The compatibility engine: turns a dropped file into a runnable plan.
//!
//! Resolution order, cheapest and most trustworthy first:
//!
//! 1. **Local database, exact digest match.** Instant, offline, already proven.
//! 2. **Local database, name match.** The second release of an application the
//!    user already installed has a new digest, so the digest lookup misses — but
//!    the recipe that worked last time is still the best answer available, and
//!    it is offline and already validated on this machine.
//! 3. **Remote registry, digest then name match.** Cached on disk after the
//!    first fetch, so it costs nothing on repeat installs.
//! 4. **Generated profile.** Built entirely from what the executable says about
//!    itself.
//!
//! Every path ends in a profile with at least one variant, so the user is never
//! told "we don't know how to run this" and left with nothing to try.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::compat::pe::{self, Arch, PeInspection};
use crate::compat::profile::{
    fallback_profile, generic_profile, AppProfile, ProfileSource, Requirements,
};
use crate::config::Config;
use crate::db::ProfileDb;
use crate::registry::{RegistryClient, RegistryIndex};
use crate::{Error, Result};

/// What kind of file the user supplied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputKind {
    /// A native Windows executable: installer or portable application.
    Exe,
    /// A Windows Installer package.
    Msi,
    /// A batch script.
    Bat,
}

impl InputKind {
    /// Classify by file extension.
    ///
    /// Extensions are matched case-insensitively because Windows software is
    /// routinely distributed as `SETUP.EXE`.
    pub fn from_path(path: &Path) -> Result<Self> {
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "exe" | "com" => Ok(InputKind::Exe),
            "msi" => Ok(InputKind::Msi),
            "bat" | "cmd" => Ok(InputKind::Bat),
            other => Err(Error::UnsupportedInput(if other.is_empty() {
                "no file extension".to_string()
            } else {
                format!(".{other}")
            })),
        }
    }

    /// The arguments handed to `wine` to run this input.
    ///
    /// Kept as a pure function of the kind so the whole "how do we execute this"
    /// decision is unit-testable without Wine.
    pub fn wine_args(&self, windows_path: &str) -> Vec<OsString> {
        match self {
            // A bare path is what `wine` expects for an executable.
            InputKind::Exe => vec![OsString::from(windows_path)],
            InputKind::Msi => vec![
                OsString::from("msiexec"),
                OsString::from("/i"),
                OsString::from(windows_path),
            ],
            InputKind::Bat => vec![
                OsString::from("cmd"),
                OsString::from("/c"),
                OsString::from(windows_path),
            ],
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            InputKind::Exe => "executable",
            InputKind::Msi => "installer package",
            InputKind::Bat => "batch script",
        }
    }
}

/// A profile matched to a concrete file, with the facts behind the decision.
#[derive(Debug, Clone)]
pub struct ResolvedProfile {
    pub profile: AppProfile,
    /// `None` for inputs that are not PE images (`.msi`, `.bat`).
    pub inspection: Option<PeInspection>,
    pub input_kind: InputKind,
    /// The file this was resolved for.
    pub path: PathBuf,
    /// SHA-256 of the file.
    pub sha256: String,
    /// Where the profile came from.
    pub source: ProfileSource,
}

impl ResolvedProfile {
    /// A short explanation of how the profile was chosen, for the UI and logs.
    pub fn provenance(&self) -> String {
        match self.source {
            ProfileSource::Generated => format!(
                "no matching profile, so one was generated from the {}",
                self.input_kind.label()
            ),
            other => format!("profile matched {}", other.phrase()),
        }
    }

    /// True when a real recipe was matched, rather than a profile invented from
    /// the file itself.
    pub fn matched_a_profile(&self) -> bool {
        self.source != ProfileSource::Generated
    }

    /// Suggested display name for the application.
    ///
    /// A matched profile knows what the application is called; a file name
    /// carries version numbers, `Setup`, and the installer's own noise. So the
    /// profile's name wins whenever there is one — `Notepad++` rather than
    /// `Notepad++ 8.6.2 Setup`.
    ///
    /// The application id is derived from this name by the caller, always, so
    /// that renaming an application with `--name` also renames it in the menu
    /// and on the command line. There is exactly one rule for how an id is
    /// chosen, and no special case.
    pub fn suggested_name(&self) -> String {
        if self.matched_a_profile() && !self.profile.name.trim().is_empty() {
            return self.profile.name.trim().to_string();
        }
        name_hint_for(&self.path)
    }
}

/// The registry-facing name slug for an input file.
fn name_hint_for(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    crate::compat::profile::clean_app_name(&stem)
}

/// Resolves profiles for executables.
pub struct CompatibilityEngine<'a> {
    db: &'a ProfileDb,
    config: &'a Config,
    registry: Option<&'a RegistryClient>,
}

impl<'a> CompatibilityEngine<'a> {
    pub fn new(db: &'a ProfileDb, config: &'a Config) -> Self {
        CompatibilityEngine {
            db,
            config,
            registry: None,
        }
    }

    /// Attach a remote registry. Without one, resolution stays fully offline.
    pub fn with_registry(mut self, registry: &'a RegistryClient) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Inspect a file without resolving a profile.
    pub fn inspect(&self, path: &Path) -> Result<PeInspection> {
        pe::inspect(path)
    }

    /// Find or create the profile that fits `path`.
    pub fn resolve(&self, path: &Path) -> Result<ResolvedProfile> {
        let input_kind = InputKind::from_path(path)?;

        let inspection = match input_kind {
            InputKind::Exe => {
                let info = pe::inspect(path)?;
                if info.is_dll {
                    return Err(Error::IsALibrary(path.to_path_buf()));
                }
                // Arch policy lives here rather than in the inspector: reading
                // headers always succeeds, running the result may not.
                if !info.arch.is_supported() {
                    return Err(Error::UnsupportedArch(info.arch.to_string()));
                }
                Some(info)
            }
            // These are not PE images. Hashing still lets us cache the result.
            InputKind::Msi | InputKind::Bat => None,
        };

        let sha256 = match &inspection {
            Some(info) => info.sha256.clone(),
            None => pe::sha256_file(path)?,
        };
        let arch = inspection.as_ref().map(|i| i.arch);

        // 1. Known locally?
        if let Some(profile) = self.db.find_profile_by_hash(&sha256)? {
            tracing::info!(
                profile = %profile.id,
                source = profile.source.label(),
                "matched a profile in the local database"
            );
            return Ok(ResolvedProfile {
                source: profile.source,
                profile,
                inspection,
                input_kind,
                path: path.to_path_buf(),
                sha256,
            });
        }

        // 1b. Known locally by *name*? Applications ship new installers
        //     constantly, so a digest-only local lookup would almost never hit
        //     on the second version of something already installed. Matching a
        //     profile WinDrop already knows — one bundled with it, or one that
        //     worked on this machine before — turns "install an update" into a
        //     solved problem instead of a fresh guess. Generic installer names
        //     are refused inside `best_match`, so this cannot bind an unrelated
        //     recipe.
        let hint = name_hint_for(path);
        if hint.len() >= 3 {
            let known = RegistryIndex {
                source_url: "local database".to_string(),
                fetched_at_unix: 0,
                profiles: self.db.all_profiles()?,
            };
            if let Some(profile) = known.best_match(&sha256, arch, Some(&hint)) {
                let profile = profile.clone();
                tracing::info!(
                    profile = %profile.id,
                    source = profile.source.label(),
                    "matched a profile already known on this machine"
                );
                // Key the profile by this digest too, so the next install of the
                // same file short-circuits on the fast path above.
                let mut keyed = profile.clone();
                if !keyed.matches_hash(&sha256) {
                    keyed.hashes.push(sha256.clone());
                    let _ = self.db.upsert_profile(&keyed);
                }
                return Ok(ResolvedProfile {
                    source: profile.source,
                    profile,
                    inspection,
                    input_kind,
                    path: path.to_path_buf(),
                    sha256,
                });
            }
        }

        // 2. Known to the community? A failure here is never fatal: the whole
        //    registry is an optimisation over the generated fallback.
        if self.config.allow_remote_registry {
            if let Some(client) = self.registry {
                match client.find_by_hash(&sha256, arch, Some(&hint), true) {
                    Ok(Some(profile)) => {
                        // Cache it so the next install needs no network.
                        if let Err(e) = self.db.upsert_profile(&profile) {
                            tracing::warn!(profile = %profile.id, error = %e, "could not cache the remote profile");
                        }
                        tracing::info!(profile = %profile.id, "matched a profile in the registry");
                        return Ok(ResolvedProfile {
                            source: ProfileSource::Remote,
                            profile,
                            inspection,
                            input_kind,
                            path: path.to_path_buf(),
                            sha256,
                        });
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "registry lookup failed; falling back to detection")
                    }
                }
            }
        }

        // 3. Build one from scratch.
        let profile = match &inspection {
            Some(info) => generic_profile(info, self.config),
            None => {
                let name = {
                    let cleaned = name_hint_for(path);
                    if cleaned.is_empty() {
                        "Windows Application".to_string()
                    } else {
                        cleaned
                    }
                };
                fallback_profile(
                    &name,
                    Arch::X86_64,
                    &sha256,
                    vec![format!("{} accepted", input_kind.label())],
                    self.config,
                )
            }
        };
        tracing::info!(
            profile = %profile.id,
            variants = profile.variants.len(),
            "generated a compatibility profile"
        );

        if let Err(e) = self.db.upsert_profile(&profile) {
            // A profile we cannot persist is still usable for this install.
            tracing::warn!(error = %e, "could not store the generated profile");
        }

        Ok(ResolvedProfile {
            source: ProfileSource::Generated,
            profile,
            inspection,
            input_kind,
            path: path.to_path_buf(),
            sha256,
        })
    }
}

impl<'a> CompatibilityEngine<'a> {
    /// Resolve a file against a profile the caller has already chosen.
    ///
    /// This is what `--profile` and `profiles attach` use. The file is still
    /// inspected, because the digest, the input kind and the architecture are
    /// properties of the file rather than of the recipe — but no lookup happens,
    /// so a user can apply a recipe to an application WinDrop would never have
    /// matched on its own.
    ///
    /// A profile whose architecture contradicts the executable is refused: the
    /// mismatch would produce a prefix that cannot contain the program, and the
    /// resulting install would fail in a confusing way several minutes in.
    pub fn resolve_with(&self, path: &Path, profile: AppProfile) -> Result<ResolvedProfile> {
        profile.validate()?;
        let input_kind = InputKind::from_path(path)?;
        let inspection = match input_kind {
            InputKind::Exe => {
                let info = pe::inspect(path)?;
                if info.is_dll {
                    return Err(Error::IsALibrary(path.to_path_buf()));
                }
                Some(info)
            }
            InputKind::Msi | InputKind::Bat => None,
        };

        if let Some(info) = &inspection {
            let usable = profile
                .variants
                .iter()
                .any(|v| v.arch == info.arch || v.arch == Arch::X86_64 && info.arch == Arch::X86);
            if !usable {
                return Err(Error::Config {
                    field: "profile.arch".into(),
                    reason: format!(
                        "profile '{}' offers only {} environments, but '{}' is a {} binary",
                        profile.id,
                        profile
                            .variants
                            .first()
                            .map(|v| v.arch.to_string())
                            .unwrap_or_else(|| "no".into()),
                        path.display(),
                        info.arch
                    ),
                });
            }
        }

        let sha256 = match &inspection {
            Some(info) => info.sha256.clone(),
            None => pe::sha256_file(path)?,
        };

        tracing::info!(profile = %profile.id, "using the profile the user selected");
        Ok(ResolvedProfile {
            source: profile.source,
            profile,
            inspection,
            input_kind,
            path: path.to_path_buf(),
            sha256,
        })
    }
}

/// Convenience for callers that only need the requirements (used by the CLI's
/// `inspect` command and by the GUI's preview pane).
pub fn describe_requirements(inspection: &PeInspection) -> Requirements {
    crate::compat::profile::infer_requirements(inspection)
}

#[cfg(test)]
mod tests {
    // Setting one field on a default is the clearest way to say "defaults,
    // except this"; the lint is aimed at production code, where it usually
    // means a missing derive.
    #![allow(clippy::field_reassign_with_default)]
    use super::*;
    use crate::fixtures::{self, PeSpec};
    use std::time::Duration;

    fn engine_setup() -> (tempfile::TempDir, ProfileDb, Config) {
        let dir = tempfile::tempdir().unwrap();
        let db = ProfileDb::open_in_memory().unwrap();
        let mut config = Config::default();
        // Keep tests offline and deterministic.
        config.allow_remote_registry = false;
        config.data_dir = Some(dir.path().to_path_buf());
        (dir, db, config)
    }

    fn write_installer(dir: &Path, name: &str, spec: &PeSpec) -> PathBuf {
        let path = dir.join(name);
        fixtures::write_exe(&path, spec).unwrap();
        path
    }

    #[test]
    fn classifies_supported_inputs_case_insensitively() {
        assert_eq!(
            InputKind::from_path(Path::new("a.exe")).unwrap(),
            InputKind::Exe
        );
        assert_eq!(
            InputKind::from_path(Path::new("A.EXE")).unwrap(),
            InputKind::Exe
        );
        assert_eq!(
            InputKind::from_path(Path::new("pkg.msi")).unwrap(),
            InputKind::Msi
        );
        assert_eq!(
            InputKind::from_path(Path::new("go.bat")).unwrap(),
            InputKind::Bat
        );
        assert_eq!(
            InputKind::from_path(Path::new("script.cmd")).unwrap(),
            InputKind::Bat
        );
    }

    #[test]
    fn rejects_unsupported_inputs_by_name() {
        match InputKind::from_path(Path::new("archive.tar.gz")) {
            Err(Error::UnsupportedInput(ext)) => assert_eq!(ext, ".gz"),
            other => panic!("expected UnsupportedInput, got {other:?}"),
        }
        match InputKind::from_path(Path::new("README")) {
            Err(Error::UnsupportedInput(ext)) => assert!(ext.contains("no file extension")),
            other => panic!("expected UnsupportedInput, got {other:?}"),
        }
    }

    #[test]
    fn wine_arguments_match_the_input_kind() {
        assert_eq!(
            InputKind::Exe.wine_args(r"C:\setup.exe"),
            vec![OsString::from(r"C:\setup.exe")]
        );
        assert_eq!(
            InputKind::Msi.wine_args(r"C:\pkg.msi"),
            vec![
                OsString::from("msiexec"),
                OsString::from("/i"),
                OsString::from(r"C:\pkg.msi")
            ]
        );
        assert_eq!(
            InputKind::Bat.wine_args(r"C:\go.bat"),
            vec![
                OsString::from("cmd"),
                OsString::from("/c"),
                OsString::from(r"C:\go.bat")
            ]
        );
    }

    #[test]
    fn a_fresh_executable_gets_a_generated_profile_with_a_usable_chain() {
        let (dir, db, config) = engine_setup();
        let path = write_installer(dir.path(), "FancySetup.exe", &PeSpec::example_d3d12_game());

        let engine = CompatibilityEngine::new(&db, &config);
        let resolved = engine.resolve(&path).unwrap();

        assert_eq!(resolved.source, ProfileSource::Generated);
        assert_eq!(resolved.input_kind, InputKind::Exe);
        assert!(resolved.inspection.is_some());
        assert!(!resolved.profile.variants.is_empty());
        assert_eq!(resolved.profile.hashes, vec![resolved.sha256.clone()]);
        assert!(resolved.provenance().contains("generated"));
    }

    #[test]
    fn the_generated_profile_is_cached_so_the_second_lookup_matches_locally() {
        let (dir, db, config) = engine_setup();
        let path = write_installer(dir.path(), "App.exe", &PeSpec::example_installer());

        let engine = CompatibilityEngine::new(&db, &config);
        let first = engine.resolve(&path).unwrap();
        let second = engine.resolve(&path).unwrap();

        assert_eq!(first.profile.id, second.profile.id);
        assert_eq!(second.source, ProfileSource::Generated);
        assert_eq!(db.profile_count().unwrap(), 1);
        assert!(db.find_profile_by_hash(&first.sha256).unwrap().is_some());
    }

    #[test]
    fn a_locally_known_digest_wins_over_generation() {
        let (dir, db, config) = engine_setup();
        let path = write_installer(dir.path(), "App.exe", &PeSpec::example_installer());
        let inspection = pe::inspect(&path).unwrap();

        // Pretend a human tuned a profile for exactly this file.
        let mut curated = generic_profile(&inspection, &config);
        curated.name = "Curated Name".into();
        curated.source = ProfileSource::Local;
        curated.notes = "hand-tuned".into();
        db.upsert_profile(&curated).unwrap();

        let engine = CompatibilityEngine::new(&db, &config);
        let resolved = engine.resolve(&path).unwrap();
        assert_eq!(resolved.profile.name, "Curated Name");
        assert_eq!(resolved.source, ProfileSource::Local);
        // The sentence has to read as prose: "matched a profile local to this
        // machine", not "matched local to this machine".
        let provenance = resolved.provenance();
        assert!(provenance.contains("matched a profile"), "{provenance}");
        assert!(provenance.contains("local"), "{provenance}");
    }

    #[test]
    fn a_dll_is_refused_with_a_helpful_message() {
        let (dir, db, config) = engine_setup();
        let path = write_installer(dir.path(), "helper.dll.exe", &PeSpec::example_dll());
        let engine = CompatibilityEngine::new(&db, &config);
        match engine.resolve(&path) {
            Err(Error::IsALibrary(p)) => assert_eq!(p, path),
            other => panic!("expected IsALibrary, got {other:?}"),
        }
    }

    #[test]
    fn an_msi_gets_a_generic_profile_without_an_inspection() {
        let (dir, db, config) = engine_setup();
        let path = dir.path().join("package.msi");
        std::fs::write(&path, b"not really an msi, but it hashes").unwrap();

        let engine = CompatibilityEngine::new(&db, &config);
        let resolved = engine.resolve(&path).unwrap();
        assert_eq!(resolved.input_kind, InputKind::Msi);
        assert!(resolved.inspection.is_none());
        assert!(!resolved.profile.variants.is_empty());
        assert_eq!(resolved.profile.hashes.len(), 1);
        // The digest is still recorded, so the result is cached.
        assert!(db.find_profile_by_hash(&resolved.sha256).unwrap().is_some());
    }

    #[test]
    fn a_bat_script_resolves_to_cmd_invocation_metadata() {
        let (dir, db, config) = engine_setup();
        let path = dir.path().join("install.bat");
        fixtures::write_bat(&path, "@echo off\r\nrem do things\r\n").unwrap();

        let engine = CompatibilityEngine::new(&db, &config);
        let resolved = engine.resolve(&path).unwrap();
        assert_eq!(resolved.input_kind, InputKind::Bat);
        assert_eq!(
            resolved.input_kind.wine_args("install.bat")[0],
            OsString::from("cmd")
        );
    }

    #[test]
    fn garbage_with_an_exe_extension_is_rejected_before_any_profile_is_written() {
        let (dir, db, config) = engine_setup();
        let path = dir.path().join("fake.exe");
        fixtures::corrupt::write_bad_exe(&path).unwrap();

        let engine = CompatibilityEngine::new(&db, &config);
        assert!(matches!(
            engine.resolve(&path),
            Err(Error::NotAPeFile { .. })
        ));
        assert_eq!(db.profile_count().unwrap(), 0, "nothing should be cached");
    }

    #[test]
    fn a_missing_file_is_reported_clearly() {
        let (_dir, db, config) = engine_setup();
        let engine = CompatibilityEngine::new(&db, &config);
        assert!(matches!(
            engine.resolve(Path::new("/nope/missing.exe")),
            Err(Error::InputMissing { .. })
        ));
    }

    #[test]
    fn two_different_executables_get_two_profiles() {
        let (dir, db, config) = engine_setup();
        let a = write_installer(dir.path(), "alpha.exe", &PeSpec::example_installer());
        let b = write_installer(dir.path(), "beta.exe", &PeSpec::example_d3d12_game());

        let engine = CompatibilityEngine::new(&db, &config);
        let ra = engine.resolve(&a).unwrap();
        let rb = engine.resolve(&b).unwrap();
        assert_ne!(ra.profile.id, rb.profile.id);
        assert_eq!(db.profile_count().unwrap(), 2);
    }

    #[test]
    fn an_unreachable_registry_does_not_break_resolution() {
        let (dir, db, mut config) = engine_setup();
        config.allow_remote_registry = true;
        config.wine_variant = crate::config::WineVariant::Stable;
        let client = RegistryClient::new("https://windrop.invalid/never.json", dir.path())
            .unwrap()
            .with_ttl(Duration::from_secs(0));
        let path = write_installer(dir.path(), "App.exe", &PeSpec::example_installer());

        let engine = CompatibilityEngine::new(&db, &config).with_registry(&client);
        let resolved = engine.resolve(&path).unwrap();
        assert_eq!(resolved.source, ProfileSource::Generated);
        assert!(!resolved.profile.variants.is_empty());
    }

    #[test]
    fn suggested_names_strip_installer_noise() {
        let mk = |name: &str| {
            let (dir, db, config) = engine_setup();
            let path = write_installer(dir.path(), name, &PeSpec::example_installer());
            CompatibilityEngine::new(&db, &config)
                .resolve(&path)
                .unwrap()
                .suggested_name()
        };
        assert_eq!(mk("NotepadSetup.exe"), "Notepad");
        assert_eq!(mk("npp.8.6.2.Installer.exe"), "npp 8 6 2");
        assert_eq!(mk("vcredist_x86.exe"), "vcredist");
        // A name made entirely of noise falls back to the raw stem.
        assert_eq!(mk("setup.exe"), "setup");
    }

    #[test]
    fn unsupported_architectures_are_refused_by_the_engine_not_the_inspector() {
        let (dir, db, config) = engine_setup();
        let path = dir.path().join("arm.exe");
        let mut bytes = fixtures::synthetic_pe(&PeSpec::example_installer());
        bytes[0x44..0x46].copy_from_slice(&crate::compat::pe::MACHINE_ARM64.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();

        let engine = CompatibilityEngine::new(&db, &config);
        match engine.resolve(&path) {
            Err(Error::UnsupportedArch(name)) => assert!(name.contains("ARM64")),
            other => panic!("expected UnsupportedArch, got {other:?}"),
        }
        assert_eq!(db.profile_count().unwrap(), 0);
    }

    #[test]
    fn description_helper_reports_graphics_requirements() {
        let inspection = pe::inspect_bytes(
            &fixtures::synthetic_pe(&PeSpec::example_d3d12_game()),
            Path::new("game.exe"),
        )
        .unwrap();
        let req = describe_requirements(&inspection);
        assert_eq!(
            req.graphics,
            Some(crate::compat::profile::GraphicsApi::D3D12)
        );
    }
}
