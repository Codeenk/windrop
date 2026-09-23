//! The record WinDrop keeps for each installed application.
//!
//! `metadata.json` lives inside the application's own directory, so the whole
//! installation — prefix, icon, metadata — is one self-contained unit that can
//! be inspected, backed up or deleted as a whole.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::compat::pe::Arch;
use crate::compat::profile::{ProfileSource, RuntimeEnv, WindowsVersion};
use crate::compat::InputKind;
use crate::config::SandboxMode;
use crate::runtime::prefix::PrefixPaths;
use crate::{Error, Result};

/// Where the record is stored inside an application directory.
pub const METADATA_FILE: &str = "metadata.json";
/// Where the profile used for the install is stored, for later inspection.
pub const PROFILE_FILE: &str = "profile.json";

/// One installed Windows application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledApp {
    /// WinDrop's id, also the directory name.
    pub id: String,
    /// Display name.
    pub name: String,
    #[serde(default)]
    pub version: String,
    /// The file the user originally dropped.
    pub source_file: PathBuf,
    /// Digest of that file, which is what profiles are keyed by.
    pub sha256: String,
    pub input_kind: InputKind,
    pub arch: Arch,

    pub profile_id: String,
    pub profile_source: ProfileSource,
    /// The exact environment that worked, replayable at launch time.
    pub variant: RuntimeEnv,
    /// How many variants were tried before one worked.
    #[serde(default)]
    pub attempts: usize,

    /// The program to launch, as Windows sees it.
    pub main_exe_windows: String,
    /// The same program, as the host sees it.
    pub main_exe_host: PathBuf,

    pub installed_at: String,
    #[serde(default)]
    pub icon: Option<PathBuf>,
    #[serde(default)]
    pub desktop_file: Option<PathBuf>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub notes: String,
}

impl InstalledApp {
    pub fn metadata_path(app_dir: &Path) -> PathBuf {
        app_dir.join(METADATA_FILE)
    }

    pub fn profile_path(app_dir: &Path) -> PathBuf {
        app_dir.join(PROFILE_FILE)
    }

    /// This application's prefix.
    pub fn prefix(&self, paths: &crate::paths::Paths) -> PrefixPaths {
        PrefixPaths::new(paths.app_dir(&self.id))
    }

    /// Load the record from an application directory.
    pub fn load(paths: &crate::paths::Paths, id: &str) -> Result<Self> {
        let app_dir = paths.app_dir(id);
        if !app_dir.is_dir() {
            return Err(Error::AppNotFound(id.to_string()));
        }
        let path = Self::metadata_path(&app_dir);
        let text = std::fs::read_to_string(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                // A directory without metadata is not installed, it is residue.
                Error::AppNotFound(id.to_string())
            } else {
                Error::Io(e)
            }
        })?;
        let app: InstalledApp = serde_json::from_str(&text)?;
        if app.id != id {
            return Err(Error::AppNotFound(format!(
                "{id} (the metadata inside belongs to '{}')",
                app.id
            )));
        }
        Ok(app)
    }

    /// Write the record into the application directory.
    pub fn save(&self, paths: &crate::paths::Paths) -> Result<()> {
        let app_dir = paths.app_dir(&self.id);
        std::fs::create_dir_all(&app_dir)?;
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(Self::metadata_path(&app_dir), text)?;
        Ok(())
    }

    /// Save the profile that produced this installation, for diagnosis.
    pub fn save_profile(
        &self,
        paths: &crate::paths::Paths,
        profile: &crate::compat::profile::AppProfile,
    ) -> Result<()> {
        let app_dir = paths.app_dir(&self.id);
        std::fs::create_dir_all(&app_dir)?;
        std::fs::write(Self::profile_path(&app_dir), profile.to_json_pretty()?)?;
        Ok(())
    }

    pub fn windows_version(&self) -> WindowsVersion {
        self.variant.windows_version
    }

    pub fn dxvk(&self) -> bool {
        self.variant.dxvk
    }

    pub fn vkd3d_proton(&self) -> bool {
        self.variant.vkd3d_proton
    }

    /// A one-line description for CLI listings.
    pub fn summary(&self) -> String {
        format!(
            "{} — {} ({}), installed {}",
            self.id, self.name, self.arch, self.installed_at
        )
    }

    /// The strategy that worked, in words.
    pub fn strategy(&self) -> String {
        if self.variant.rationale.is_empty() {
            format!(
                "{} with {}",
                self.variant.wine_build,
                self.windows_version().label()
            )
        } else {
            self.variant.rationale.clone()
        }
    }

    /// Bytes on disk, including the prefix.
    pub fn size_on_disk(&self, paths: &crate::paths::Paths) -> u64 {
        crate::runtime::prefix::directory_size(&paths.app_dir(&self.id))
    }

    /// Confirm the recorded program still exists.
    pub fn is_runnable(&self) -> bool {
        self.main_exe_host.is_file()
    }

    pub fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            return Err(Error::Config {
                field: "installed_app.id".into(),
                reason: "must not be empty".into(),
            });
        }
        if self.main_exe_windows.trim().is_empty() {
            return Err(Error::Config {
                field: "installed_app.main_exe_windows".into(),
                reason: "must record which program to launch".into(),
            });
        }
        Ok(())
    }
}

/// The sandbox mode in force when an application was installed.
///
/// Kept out of [`InstalledApp`] deliberately: sandboxing is a machine-wide
/// policy that the user can change at any time, so it is read from
/// [`crate::Config`] at launch rather than frozen per application.
pub fn sandbox_for(_app: &InstalledApp, config: &crate::Config) -> SandboxMode {
    config.sandbox
}

#[cfg(test)]
mod tests {
    // Setting one field on a default is the clearest way to say "defaults,
    // except this"; the lint is aimed at production code, where it usually
    // means a missing derive.
    #![allow(clippy::field_reassign_with_default)]
    use super::*;
    use crate::compat::profile::DependencySpec;
    use crate::paths::Paths;

    fn sample_app() -> InstalledApp {
        InstalledApp {
            id: "notepadpp".into(),
            name: "Notepad++".into(),
            version: "8.6".into(),
            source_file: PathBuf::from("/home/u/Downloads/npp.8.6.Setup.exe"),
            sha256: "abc123".into(),
            input_kind: InputKind::Exe,
            arch: Arch::X86_64,
            profile_id: "notepadpp-64".into(),
            profile_source: ProfileSource::Remote,
            variant: RuntimeEnv {
                wine_build: "stable".into(),
                arch: Arch::X86_64,
                windows_version: WindowsVersion::Win10,
                dxvk: false,
                vkd3d_proton: false,
                dll_overrides: vec![],
                env: vec![],
                dependencies: vec![DependencySpec::new("vcrun2022", "needed")],
                rationale: "preferred build".into(),
            },
            attempts: 1,
            main_exe_windows: r"C:\Program Files\Notepad++\notepad++.exe".into(),
            main_exe_host: PathBuf::from(
                "/data/apps/notepadpp/prefix/drive_c/Program Files/Notepad++/notepad++.exe",
            ),
            installed_at: "2026-09-23T09:38:00Z".into(),
            icon: Some(PathBuf::from("/data/apps/notepadpp/icon.png")),
            desktop_file: Some(PathBuf::from(
                "/home/u/.local/share/applications/org.windrop.WinDrop.notepadpp.desktop",
            )),
            dependencies: vec!["vcrun2022".into()],
            notes: "Installed without trouble".into(),
        }
    }

    fn fixture() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_data_dir(dir.path());
        paths.ensure().unwrap();
        (dir, paths)
    }

    #[test]
    fn a_saved_record_loads_back_identically() {
        let (_dir, paths) = fixture();
        let app = sample_app();
        app.save(&paths).unwrap();

        let loaded = InstalledApp::load(&paths, "notepadpp").unwrap();
        assert_eq!(loaded, app);
        assert_eq!(loaded.variant.dependencies, app.variant.dependencies);
    }

    #[test]
    fn the_record_lives_inside_the_application_directory() {
        let (_dir, paths) = fixture();
        let app = sample_app();
        app.save(&paths).unwrap();
        assert!(paths.app_dir("notepadpp").join(METADATA_FILE).is_file());
    }

    #[test]
    fn loading_an_unknown_application_reports_not_found() {
        let (_dir, paths) = fixture();
        assert!(matches!(
            InstalledApp::load(&paths, "ghost"),
            Err(Error::AppNotFound(_))
        ));
    }

    #[test]
    fn a_directory_without_metadata_is_not_an_installed_application() {
        let (_dir, paths) = fixture();
        std::fs::create_dir_all(paths.app_dir("half-deleted")).unwrap();
        assert!(matches!(
            InstalledApp::load(&paths, "half-deleted"),
            Err(Error::AppNotFound(_))
        ));
    }

    #[test]
    fn mismatched_metadata_is_refused() {
        let (_dir, paths) = fixture();
        let app = sample_app();
        app.save(&paths).unwrap();
        // Move the directory to a different name.
        std::fs::rename(paths.app_dir("notepadpp"), paths.app_dir("renamed")).unwrap();
        match InstalledApp::load(&paths, "renamed") {
            Err(Error::AppNotFound(msg)) => assert!(msg.contains("belongs to")),
            other => panic!("expected AppNotFound, got {other:?}"),
        }
    }

    #[test]
    fn the_profile_is_stored_alongside_for_diagnosis() {
        let (_dir, paths) = fixture();
        let app = sample_app();
        app.save(&paths).unwrap();

        let inspection = crate::compat::pe::inspect_bytes(
            &crate::fixtures::synthetic_pe(&crate::fixtures::PeSpec::example_installer()),
            Path::new("app.exe"),
        )
        .unwrap();
        let profile =
            crate::compat::profile::generic_profile(&inspection, &crate::Config::default());
        app.save_profile(&paths, &profile).unwrap();

        let text =
            std::fs::read_to_string(InstalledApp::profile_path(&paths.app_dir(&app.id))).unwrap();
        let parsed = crate::compat::profile::AppProfile::from_json(&text).unwrap();
        assert_eq!(parsed.id, profile.id);
    }

    #[test]
    fn convenience_accessors_read_the_stored_variant() {
        let app = sample_app();
        assert_eq!(app.windows_version(), WindowsVersion::Win10);
        assert!(!app.dxvk());
        assert!(!app.vkd3d_proton());
        assert_eq!(app.strategy(), "preferred build");
    }

    #[test]
    fn an_empty_rationale_still_produces_a_readable_strategy() {
        let mut app = sample_app();
        app.variant.rationale.clear();
        let text = app.strategy();
        assert!(text.contains("stable"));
        assert!(text.contains("Windows 10"));
    }

    #[test]
    fn runnability_reflects_whether_the_program_exists() {
        let (dir, _paths) = fixture();
        let mut app = sample_app();
        app.main_exe_host = dir.path().join("missing.exe");
        assert!(!app.is_runnable());

        std::fs::write(&app.main_exe_host, b"MZ").unwrap();
        assert!(app.is_runnable());
    }

    #[test]
    fn validation_catches_records_that_could_not_be_launched() {
        let mut app = sample_app();
        app.main_exe_windows = "   ".into();
        assert!(matches!(app.validate(), Err(Error::Config { .. })));

        let mut app = sample_app();
        app.id = String::new();
        assert!(app.validate().is_err());

        assert!(sample_app().validate().is_ok());
    }

    #[test]
    fn the_summary_names_the_application_and_its_architecture() {
        let text = sample_app().summary();
        assert!(text.contains("notepadpp"));
        assert!(text.contains("64-bit"));
        assert!(text.contains("2026"));
    }

    #[test]
    fn size_on_disk_includes_the_prefix() {
        let (_dir, paths) = fixture();
        let app = sample_app();
        app.save(&paths).unwrap();
        let prefix = app.prefix(&paths);
        crate::runtime::prefix::prepare_directories(&prefix).unwrap();
        std::fs::write(prefix.drive_c().join("blob.bin"), vec![0u8; 4096]).unwrap();

        assert!(app.size_on_disk(&paths) >= 4096);
    }

    #[test]
    fn metadata_survives_json_round_tripping_without_losing_enums() {
        let (_dir, paths) = fixture();
        let app = sample_app();
        app.save(&paths).unwrap();
        let text =
            std::fs::read_to_string(InstalledApp::metadata_path(&paths.app_dir(&app.id))).unwrap();

        assert!(text.contains("\"exe\""), "input kind serialises readably");
        assert!(
            text.contains("\"remote\""),
            "profile source serialises readably"
        );
        assert!(
            text.contains("\"x86_64\""),
            "architecture serialises readably"
        );
        assert!(
            text.contains("\"win10\""),
            "windows version serialises readably"
        );
    }

    #[test]
    fn sandbox_policy_is_read_from_configuration_not_frozen_per_app() {
        let app = sample_app();
        let mut config = crate::Config::default();
        config.sandbox = SandboxMode::Off;
        assert_eq!(sandbox_for(&app, &config), SandboxMode::Off);
        config.sandbox = SandboxMode::Strict;
        assert_eq!(sandbox_for(&app, &config), SandboxMode::Strict);
    }
}
