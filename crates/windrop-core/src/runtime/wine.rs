//! Finding and identifying Wine builds.
//!
//! WinDrop can use two kinds of Wine:
//!
//! * **Managed** builds under `<data>/runtime/wine/<name>/`. These are
//!   downloaded by WinDrop, version-pinned, and never require root. A build can
//!   look like a normal installation (`bin/wine`) or a single AppImage (`wine`).
//! * The **system** Wine from `$PATH`, used as a fallback and as the default
//!   when the user already has Wine configured.
//!
//! Nothing here requires Wine to be present: discovery returns a clear
//! [`Error::WineMissing`] with an actionable hint, and the doctor turns that
//! into a setup page.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::compat::pe::Arch;
use crate::config::WineVariant;
use crate::process::{which, CommandSpec};
use crate::runtime::prefix::PrefixPaths;
use crate::{Error, Result};

/// How long to wait for `wine --version`.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// Where a Wine build came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WineSource {
    /// A build managed by WinDrop under the runtime directory.
    Managed(PathBuf),
    /// Whatever `wine` resolves to on `$PATH`.
    System,
}

impl WineSource {
    pub fn label(&self) -> &'static str {
        match self {
            WineSource::Managed(_) => "managed",
            WineSource::System => "system",
        }
    }
}

/// A usable Wine build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WineInstall {
    /// The `wine` binary to execute.
    pub executable: PathBuf,
    /// Parsed version, e.g. `9.0` or `10.0`.
    pub version: String,
    /// Extra version text, e.g. `Staging`.
    pub flavour: String,
    /// Which configured variant this install satisfies.
    pub variant_label: String,
    pub source: WineSource,
}

impl WineInstall {
    /// Run `<wine> --version` and parse the result.
    pub fn probe(executable: &Path, variant_label: &str, source: WineSource) -> Result<Self> {
        let spec = CommandSpec::new(executable).arg("--version");
        let out = spec.run_capture(PROBE_TIMEOUT)?;
        if !out.success() {
            return Err(spec.failure(&out));
        }
        let (version, flavour) = parse_wine_version(&out.stdout);
        Ok(WineInstall {
            executable: executable.to_path_buf(),
            version,
            flavour,
            variant_label: variant_label.to_string(),
            source,
        })
    }

    /// Sibling binary in the same `bin/` directory, when it exists.
    pub fn sibling(&self, name: &str) -> Option<PathBuf> {
        let dir = self.executable.parent()?;
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        None
    }

    /// `wineserver`, used to shut a prefix down cleanly.
    pub fn wineserver(&self) -> Option<PathBuf> {
        self.sibling("wineserver")
    }

    /// A display string such as `wine-9.0 (managed)`.
    pub fn display(&self) -> String {
        if self.flavour.is_empty() {
            format!("wine-{} ({})", self.version, self.source.label())
        } else {
            format!(
                "wine-{} {} ({})",
                self.version,
                self.flavour,
                self.source.label()
            )
        }
    }

    /// Major version number, for feature gates such as built-in WoW64.
    pub fn major(&self) -> Option<u32> {
        self.version.split('.').next()?.parse().ok()
    }

    /// True when this build supports a prefix of the given bitness.
    ///
    /// Any Wine can create a 32-bit prefix; a 64-bit prefix needs a build with
    /// 64-bit support, which every distribution build has had for years.
    pub fn supports(&self, arch: Arch) -> bool {
        match arch {
            Arch::X86_64 | Arch::X86 => true,
            Arch::Arm64 => false,
        }
    }
}

/// Parse `wine --version` output.
///
/// Handles the shapes that actually turn up in the wild:
/// `wine-9.0`, `wine-9.0-rc1`, `wine-9.0 (Staging)`, and
/// `wine-9.21.r0.g1234abcd (wine-tkg)`.
pub fn parse_wine_version(output: &str) -> (String, String) {
    let line = output
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();

    // Everything after "wine-" up to the first space is the version token.
    let after_prefix = line
        .strip_prefix("wine-")
        .or_else(|| line.strip_prefix("Wine "))
        .unwrap_or(line);

    let (token, rest) = match after_prefix.split_once(' ') {
        Some((v, rest)) => (v, rest.trim()),
        None => (after_prefix, ""),
    };

    // A pre-release marker (``9.0-rc1``) is a property of the build, not of the
    // version number, so it moves into the flavour.
    let (version_token, pre_release) = match token.split_once('-') {
        Some((v, suffix)) => (v, suffix.to_string()),
        None => (token, String::new()),
    };

    // Drop distribution noise such as ``.r0.g1234abcd``.
    let version = version_token
        .split(".r")
        .next()
        .unwrap_or(version_token)
        .to_string();

    let mut flavour = rest.trim_matches(['(', ')']).trim().to_string();
    if !pre_release.is_empty() {
        flavour = if flavour.is_empty() {
            pre_release
        } else {
            format!("{pre_release} {flavour}")
        };
    }

    (version, flavour)
}

/// A numeric sort key for a managed Wine build path.
///
/// Comparing the paths as strings would rank `wine-9.0` above `wine-10.0`,
/// which is exactly the wrong way round. The digit runs of the nearest path
/// component that contains any are used instead.
fn version_key(build: &Path) -> Vec<u32> {
    let Some(parent) = build.parent() else {
        return Vec::new();
    };
    for component in parent.components().rev() {
        let name = component.as_os_str().to_string_lossy();
        let mut runs: Vec<u32> = Vec::new();
        let mut current = String::new();
        for ch in name.chars() {
            if ch.is_ascii_digit() {
                current.push(ch);
            } else if !current.is_empty() {
                runs.push(current.parse().unwrap_or(0));
                current.clear();
            }
        }
        if !current.is_empty() {
            runs.push(current.parse().unwrap_or(0));
        }
        if !runs.is_empty() {
            return runs;
        }
    }
    Vec::new()
}

/// Where managed Wine builds live, newest first.
pub fn managed_builds(wine_dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    let Ok(entries) = std::fs::read_dir(wine_dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        // A normal installation.
        let nested = dir.join("bin").join("wine");
        if nested.is_file() {
            found.push(nested);
            continue;
        }
        // A single-file AppImage or wrapper in the build directory.
        let flat = dir.join("wine");
        if flat.is_file() {
            found.push(flat);
        }
    }
    // Newest first, by parsed version number rather than by string order.
    found.sort_by(|a, b| {
        let (ka, kb) = (version_key(a), version_key(b));
        ka.cmp(&kb).then_with(|| a.cmp(b))
    });
    found.reverse();
    found
}

/// Pick the best Wine for a variant.
///
/// Order of preference:
/// 1. A managed build whose directory name mentions the requested flavour.
/// 2. Any managed build.
/// 3. System Wine.
pub fn resolve_wine(
    wine_dir: &Path,
    variant: &WineVariant,
    system_path_lookup: impl Fn(&str) -> Option<PathBuf>,
) -> Result<WineInstall> {
    let label = variant.label().to_string();
    let builds = managed_builds(wine_dir);

    // A named build is addressed directly by directory name, falling back to a
    // path when the user points at one.
    if let WineVariant::Build(name) = variant {
        let direct = PathBuf::from(name);
        if direct.is_file() {
            return WineInstall::probe(
                &direct,
                name,
                WineSource::Managed(direct.parent().unwrap_or(Path::new(".")).to_path_buf()),
            );
        }
        if let Some(hit) = builds.iter().find(|b| {
            b.to_string_lossy()
                .to_ascii_lowercase()
                .contains(&name.to_ascii_lowercase())
        }) {
            return WineInstall::probe(
                hit,
                name,
                WineSource::Managed(hit.parent().unwrap_or(Path::new(".")).to_path_buf()),
            );
        }
    }

    if !matches!(variant, WineVariant::System) {
        // Prefer a build whose path mentions the flavour, e.g. "wine-9.0-staging".
        let preferred = builds.iter().find(|b| {
            b.to_string_lossy()
                .to_ascii_lowercase()
                .contains(&label.to_ascii_lowercase())
        });
        if let Some(hit) = preferred.or_else(|| builds.first()) {
            if let Ok(install) = WineInstall::probe(
                hit,
                &label,
                WineSource::Managed(hit.parent().unwrap_or(Path::new(".")).to_path_buf()),
            ) {
                return Ok(install);
            }
        }
    }

    if let Some(system) = system_path_lookup("wine") {
        if let Ok(install) = WineInstall::probe(&system, "system", WineSource::System) {
            return Ok(install);
        }
    }

    Err(Error::WineMissing {
        hint: "On Arch-based systems run: sudo pacman -S wine, or let WinDrop download a \
               managed build from the settings pane."
            .to_string(),
    })
}

/// Convenience wrapper that looks up `$PATH`.
pub fn resolve_system_wine(variant: &WineVariant, wine_dir: &Path) -> Result<WineInstall> {
    resolve_wine(wine_dir, variant, which)
}

/// The environment that makes a prefix self-contained.
///
/// Shared by prefix creation, installers and launching so a prefix is never
/// touched with a different `WINEPREFIX` than the one it was built with.
pub fn base_prefix_env(prefix: &PrefixPaths, install: &WineInstall) -> Vec<(String, String)> {
    let mut env = vec![
        (
            "WINEPREFIX".to_string(),
            prefix.root().to_string_lossy().to_string(),
        ),
        // Keep Wine quiet; the interesting output is the application's.
        ("WINEDEBUG".to_string(), "-all".to_string()),
        // Some installers refuse to run when they detect a portable/Steam
        // environment variable inherited from the host.
        ("STEAM_COMPAT_DATA_PATH".to_string(), String::new()),
    ];
    if let Some(dir) = install.executable.parent() {
        if let Some(path) = std::env::var_os("PATH") {
            let mut paths = vec![dir.to_path_buf()];
            paths.extend(std::env::split_paths(&path));
            if let Ok(joined) = std::env::join_paths(paths) {
                env.push(("PATH".to_string(), joined.to_string_lossy().to_string()));
            }
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a fake `wine` executable that reports a chosen version.
    fn fake_wine(dir: &Path, relative: &str, version_line: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // NB: `printf`, not `echo` — dash (Ubuntu's /bin/sh) interprets
        // backslash escapes in `echo`.
        std::fs::write(
            &path,
            format!("#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf '%s\\n' '{version_line}'; exit 0; fi\nexit 1\n"),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn parses_plain_and_flavoured_versions() {
        assert_eq!(parse_wine_version("wine-9.0\n"), ("9.0".into(), "".into()));
        assert_eq!(
            parse_wine_version("wine-9.0 (Staging)"),
            ("9.0".into(), "Staging".into())
        );
        // A release candidate keeps its identity but not in the version number.
        assert_eq!(
            parse_wine_version("wine-9.0-rc1"),
            ("9.0".into(), "rc1".into())
        );
    }

    #[test]
    fn parses_tkg_and_odd_builds_without_losing_the_version() {
        let (v, f) = parse_wine_version("wine-10.0.r0.g1234abcd (wine-tkg)");
        assert_eq!(v, "10.0");
        assert_eq!(f, "wine-tkg");

        let (v, _) = parse_wine_version("wine-8.21");
        assert_eq!(v, "8.21");
    }

    #[test]
    fn handles_empty_and_unexpected_output() {
        assert_eq!(parse_wine_version(""), ("".into(), "".into()));
        let (v, _) = parse_wine_version("something else entirely");
        assert_eq!(v, "something");
    }

    #[test]
    fn probe_reads_the_version_from_a_real_process() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = fake_wine(tmp.path(), "bin/wine", "wine-9.7 (Staging)");
        let install = WineInstall::probe(&exe, "stable", WineSource::System).unwrap();
        assert_eq!(install.version, "9.7");
        assert_eq!(install.flavour, "Staging");
        assert_eq!(install.major(), Some(9));
        assert!(install.display().contains("wine-9.7 Staging"));
        assert!(install.supports(Arch::X86_64));
        assert!(install.supports(Arch::X86));
        assert!(!install.supports(Arch::Arm64));
    }

    #[test]
    fn probe_fails_cleanly_when_the_binary_cannot_run() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = fake_wine(tmp.path(), "bin/wine", "wine-9.0");
        // Replace it with something that always fails.
        std::fs::write(&exe, "#!/bin/sh\nexit 3\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            WineInstall::probe(&exe, "stable", WineSource::System),
            Err(Error::CommandFailed { .. })
        ));
    }

    #[test]
    fn managed_builds_are_found_in_both_layouts() {
        let tmp = tempfile::tempdir().unwrap();
        fake_wine(tmp.path(), "wine-9.0/bin/wine", "wine-9.0");
        // An AppImage-style single file in a version directory.
        fake_wine(tmp.path(), "wine-10.0/wine", "wine-10.0");
        // A stray file that is not a build must be ignored.
        std::fs::write(tmp.path().join("README"), "notes").unwrap();

        let builds = managed_builds(tmp.path());
        assert_eq!(builds.len(), 2);
        assert!(
            builds[0].to_string_lossy().contains("wine-10.0"),
            "10.0 must rank above 9.0, not below it: {builds:?}"
        );
    }

    #[test]
    fn version_ordering_is_numeric_not_lexicographic() {
        let tmp = tempfile::tempdir().unwrap();
        for version in ["wine-9.0", "wine-10.0", "wine-9.21", "wine-10.1"] {
            fake_wine(tmp.path(), &format!("{version}/bin/wine"), version);
        }
        let names: Vec<String> = managed_builds(tmp.path())
            .iter()
            .map(|p| {
                p.parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        assert_eq!(
            names,
            vec!["wine-10.1", "wine-10.0", "wine-9.21", "wine-9.0"]
        );
    }

    #[test]
    fn managed_builds_of_a_missing_directory_is_empty_not_an_error() {
        assert!(managed_builds(Path::new("/no/such/wine/dir")).is_empty());
    }

    #[test]
    fn resolution_prefers_a_matching_managed_build() {
        let tmp = tempfile::tempdir().unwrap();
        fake_wine(tmp.path(), "wine-9.0-stable/bin/wine", "wine-9.0");
        fake_wine(
            tmp.path(),
            "wine-9.5-staging/bin/wine",
            "wine-9.5 (Staging)",
        );

        let stable = resolve_wine(tmp.path(), &WineVariant::Stable, |_| None).unwrap();
        assert_eq!(stable.variant_label, "stable");
        assert!(stable.executable.to_string_lossy().contains("stable"));

        let staging = resolve_wine(tmp.path(), &WineVariant::Staging, |_| None).unwrap();
        assert!(staging.executable.to_string_lossy().contains("staging"));
        assert_eq!(staging.flavour, "Staging");
    }

    #[test]
    fn a_named_build_is_addressed_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        fake_wine(tmp.path(), "lutris-ge-8-26/bin/wine", "wine-8.26 (GE)");
        let install = resolve_wine(
            tmp.path(),
            &WineVariant::Build("lutris-ge-8-26".into()),
            |_| None,
        )
        .unwrap();
        assert_eq!(install.variant_label, "lutris-ge-8-26");
        assert_eq!(install.version, "8.26");
    }

    #[test]
    fn system_wine_is_used_when_no_managed_build_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let system = fake_wine(tmp.path(), "systemwine", "wine-9.0");
        let install = resolve_wine(tmp.path(), &WineVariant::Stable, |name| {
            if name == "wine" {
                Some(system.clone())
            } else {
                None
            }
        })
        .unwrap();
        assert_eq!(install.source, WineSource::System);
        assert_eq!(install.variant_label, "system");
    }

    #[test]
    fn a_managed_build_beats_system_wine() {
        let tmp = tempfile::tempdir().unwrap();
        fake_wine(tmp.path(), "wine-9.0/bin/wine", "wine-9.0");
        let system = fake_wine(tmp.path(), "systemwine", "wine-8.0");
        let install =
            resolve_wine(tmp.path(), &WineVariant::Stable, |_| Some(system.clone())).unwrap();
        assert_eq!(install.source.label(), "managed");
        assert_eq!(install.version, "9.0");
    }

    #[test]
    fn system_variant_skips_managed_builds() {
        let tmp = tempfile::tempdir().unwrap();
        fake_wine(tmp.path(), "wine-9.0/bin/wine", "wine-9.0");
        let system = fake_wine(tmp.path(), "systemwine", "wine-8.0");
        let install =
            resolve_wine(tmp.path(), &WineVariant::System, |_| Some(system.clone())).unwrap();
        assert_eq!(install.source, WineSource::System);
    }

    #[test]
    fn nothing_available_yields_an_actionable_error() {
        let tmp = tempfile::tempdir().unwrap();
        match resolve_wine(tmp.path(), &WineVariant::Stable, |_| None) {
            Err(Error::WineMissing { hint }) => assert!(hint.contains("pacman")),
            other => panic!("expected WineMissing, got {other:?}"),
        }
    }

    #[test]
    fn a_broken_managed_build_falls_through_to_system_wine() {
        let tmp = tempfile::tempdir().unwrap();
        let broken = fake_wine(tmp.path(), "wine-9.0/bin/wine", "wine-9.0");
        std::fs::write(&broken, "#!/bin/sh\nexit 1\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o755)).unwrap();

        let system = fake_wine(tmp.path(), "systemwine", "wine-8.0");
        let install =
            resolve_wine(tmp.path(), &WineVariant::Stable, |_| Some(system.clone())).unwrap();
        assert_eq!(install.variant_label, "system");
    }

    #[test]
    fn wineserver_is_found_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = fake_wine(tmp.path(), "bin/wine", "wine-9.0");
        assert!(WineInstall::probe(&exe, "stable", WineSource::System)
            .unwrap()
            .wineserver()
            .is_none());

        fake_wine(tmp.path(), "bin/wineserver", "wineserver-9.0");
        assert!(WineInstall::probe(&exe, "stable", WineSource::System)
            .unwrap()
            .wineserver()
            .is_some());
    }

    #[test]
    fn prefix_environment_pins_the_prefix_and_expands_path() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = fake_wine(tmp.path(), "bin/wine", "wine-9.0");
        let install = WineInstall::probe(&exe, "stable", WineSource::System).unwrap();
        let prefix = PrefixPaths::from_root(tmp.path().join("prefix"));

        let env = base_prefix_env(&prefix, &install);
        let get = |k: &str| env.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());

        assert_eq!(get("WINEPREFIX").unwrap(), prefix.root().to_string_lossy());
        assert_eq!(get("WINEDEBUG").unwrap(), "-all");
        assert!(get("PATH").unwrap().contains("bin"));
    }
}
