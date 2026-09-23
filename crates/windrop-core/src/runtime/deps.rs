//! Installing Windows runtime components with `winetricks`.
//!
//! Each [`DependencySpec`] is one `winetricks` verb. Verbs are run **one at a
//! time** rather than in a single invocation, for two reasons:
//!
//! * a failure is attributable to a specific component, which is what the user
//!   sees in the log and in the error dialog;
//! * optional components (fonts, mostly) can fail without aborting an install
//!   that would otherwise succeed.
//!
//! Verbs already recorded in the prefix's `winetricks.log` are skipped, so
//! retrying a variant does not re-download anything.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::compat::profile::DependencySpec;
use crate::process::CommandSpec;
use crate::runtime::prefix::PrefixPaths;
use crate::runtime::wine::WineInstall;
use crate::{Error, Result};

/// Runs `winetricks` against a specific prefix.
#[derive(Debug, Clone)]
pub struct WinetricksRunner {
    /// The `winetricks` executable.
    pub executable: PathBuf,
    pub prefix: PrefixPaths,
    /// The Wine binary winetricks should drive.
    pub wine: PathBuf,
    /// Directory for per-verb logs.
    pub log_dir: PathBuf,
}

/// What happened to one dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyOutcome {
    pub verb: String,
    pub success: bool,
    /// The verb was already present, so nothing was done.
    pub already_installed: bool,
    pub optional: bool,
    pub log_file: PathBuf,
}

impl DependencyOutcome {
    pub fn skipped(verb: &str, optional: bool, log_file: PathBuf) -> Self {
        DependencyOutcome {
            verb: verb.to_string(),
            success: true,
            already_installed: true,
            optional,
            log_file,
        }
    }

    pub fn summary(&self) -> String {
        if self.already_installed {
            format!("{} (already installed)", self.verb)
        } else if self.success {
            format!("{} (installed)", self.verb)
        } else if self.optional {
            format!("{} (optional, failed)", self.verb)
        } else {
            format!("{} (failed)", self.verb)
        }
    }
}

impl WinetricksRunner {
    pub fn new(
        executable: impl Into<PathBuf>,
        prefix: PrefixPaths,
        wine: &WineInstall,
        log_dir: impl Into<PathBuf>,
    ) -> Self {
        WinetricksRunner {
            executable: executable.into(),
            prefix,
            wine: wine.executable.clone(),
            log_dir: log_dir.into(),
        }
    }

    /// The command that installs `verbs`.
    ///
    /// `-q` makes winetricks non-interactive; `W_OPT_UNATTENDED` stops the
    /// bundled installers it downloads from prompting.
    pub fn command(&self, verbs: &[&str], log_file: &Path) -> CommandSpec {
        let _ = log_file;
        CommandSpec::new(&self.executable)
            .args(verbs.iter().map(|v| v.to_string()))
            .arg("-q")
            .env(
                "WINEPREFIX",
                self.prefix.root().to_string_lossy().to_string(),
            )
            .env("WINE", self.wine.to_string_lossy().to_string())
            .env("W_OPT_UNATTENDED", "1")
            .env("WINEDEBUG", "-all")
            .env_remove("DISPLAY")
            .cwd(self.prefix.drive_c())
    }

    /// Verbs winetricks has already applied to this prefix.
    pub fn installed_verbs(&self) -> Vec<String> {
        let text = std::fs::read_to_string(self.prefix.winetricks_log()).unwrap_or_default();
        text.lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| l.to_ascii_lowercase())
            .collect()
    }

    pub fn is_installed(&self, verb: &str) -> bool {
        let want = verb.trim().to_ascii_lowercase();
        self.installed_verbs().contains(&want)
    }

    fn log_path(&self, verb: &str) -> PathBuf {
        let safe: String = verb
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.log_dir.join(format!("winetricks-{safe}.log"))
    }

    /// Apply a list of dependencies in order.
    ///
    /// Optional dependencies are attempted and reported but never fail the
    /// install. A required dependency that fails aborts with its log attached.
    pub fn apply(
        &self,
        dependencies: &[DependencySpec],
        timeout: Duration,
    ) -> Result<Vec<DependencyOutcome>> {
        let mut outcomes = Vec::new();

        for dep in dependencies {
            let log_file = self.log_path(&dep.verb);

            if self.is_installed(&dep.verb) {
                tracing::debug!(verb = %dep.verb, "already installed in this prefix");
                outcomes.push(DependencyOutcome::skipped(
                    &dep.verb,
                    dep.optional,
                    log_file,
                ));
                continue;
            }

            let spec = self.command(&[dep.verb.as_str()], &log_file);
            tracing::info!(
                verb = %dep.verb,
                reason = %dep.reason,
                "installing dependency"
            );
            let output = spec.run_logged(&log_file, timeout)?;

            if output.success() {
                outcomes.push(DependencyOutcome {
                    verb: dep.verb.clone(),
                    success: true,
                    already_installed: false,
                    optional: dep.optional,
                    log_file,
                });
                continue;
            }

            if dep.optional {
                tracing::warn!(verb = %dep.verb, "optional dependency failed; continuing");
                outcomes.push(DependencyOutcome {
                    verb: dep.verb.clone(),
                    success: false,
                    already_installed: false,
                    optional: true,
                    log_file,
                });
                continue;
            }

            // The log path goes into the message so the window can offer it: a
            // failed winetricks verb is nearly always explained by its output.
            let failure = spec.failure(&output);
            return Err(Error::InstallIncomplete {
                rationale: format!(
                    "the required component '{}' could not be installed ({}). See {}",
                    dep.verb,
                    failure,
                    log_file.display()
                ),
            });
        }

        Ok(outcomes)
    }
}

/// Find `winetricks`, preferring a copy managed by WinDrop.
pub fn find_winetricks(managed_dir: &Path) -> Option<PathBuf> {
    let managed = managed_dir.join("winetricks");
    if managed.is_file() {
        return Some(managed);
    }
    crate::process::which("winetricks")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_runner(dir: &Path, behaviour: &str) -> (WinetricksRunner, PrefixPaths) {
        use std::os::unix::fs::PermissionsExt;

        let script = dir.join("winetricks");
        std::fs::write(&script, behaviour).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let prefix = PrefixPaths::from_root(dir.join("prefix"));
        crate::runtime::prefix::prepare_directories(&prefix).unwrap();

        let wine = WineInstall {
            executable: PathBuf::from("/fake/bin/wine"),
            version: "9.0".into(),
            flavour: String::new(),
            variant_label: "stable".into(),
            source: crate::runtime::wine::WineSource::System,
        };
        let runner = WinetricksRunner::new(&script, prefix.clone(), &wine, dir.join("logs"));
        (runner, prefix)
    }

    /// A winetricks that records each verb and succeeds.
    const SUCCEEDS: &str = "#!/bin/sh\nfor arg in \"$@\"; do\n  [ \"$arg\" = \"-q\" ] && continue\n  echo \"$arg\" >> \"$WINEPREFIX/winetricks.log\"\ndone\nexit 0\n";

    /// A winetricks that fails for any verb named `dotnet48`.
    const FAILS_ON_DOTNET: &str = "#!/bin/sh\nfor arg in \"$@\"; do\n  [ \"$arg\" = \"-q\" ] && continue\n  if [ \"$arg\" = \"dotnet48\" ]; then echo 'dotnet install failed' >&2; exit 1; fi\n  echo \"$arg\" >> \"$WINEPREFIX/winetricks.log\"\ndone\nexit 0\n";

    fn dep(verb: &str) -> DependencySpec {
        DependencySpec::new(verb, "test")
    }

    #[test]
    fn the_command_pins_the_prefix_and_the_wine_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let (runner, prefix) = fake_runner(tmp.path(), SUCCEEDS);
        let spec = runner.command(&["vcrun2022"], &tmp.path().join("l.log"));

        assert_eq!(
            spec.env_get("WINEPREFIX").unwrap(),
            prefix.root().as_os_str()
        );
        assert_eq!(
            spec.env_get("WINE").unwrap(),
            std::ffi::OsStr::new("/fake/bin/wine")
        );
        assert_eq!(
            spec.env_get("W_OPT_UNATTENDED").unwrap(),
            std::ffi::OsStr::new("1")
        );
        assert!(spec.args.iter().any(|a| a == "vcrun2022"));
        assert!(
            spec.args.iter().any(|a| a == "-q"),
            "must be non-interactive"
        );
        assert_eq!(spec.cwd.as_deref(), Some(prefix.drive_c().as_path()));
    }

    #[test]
    fn a_display_is_deliberately_not_exposed_to_winetricks() {
        let tmp = tempfile::tempdir().unwrap();
        let (runner, _) = fake_runner(tmp.path(), SUCCEEDS);
        let spec = runner.command(&["corefonts"], &tmp.path().join("l.log"));
        assert!(runner
            .command(&["corefonts"], Path::new("/tmp/l"))
            .env_remove
            .iter()
            .any(|k| k == "DISPLAY"));
        let _ = spec;
    }

    #[test]
    fn dependencies_are_installed_and_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let (runner, prefix) = fake_runner(tmp.path(), SUCCEEDS);
        let outcomes = runner
            .apply(
                &[dep("vcrun2022"), dep("corefonts")],
                Duration::from_secs(20),
            )
            .unwrap();

        assert_eq!(outcomes.len(), 2);
        assert!(outcomes.iter().all(|o| o.success));
        assert!(!outcomes[0].already_installed);

        let log = std::fs::read_to_string(prefix.winetricks_log()).unwrap();
        assert!(log.contains("vcrun2022"));
        assert!(log.contains("corefonts"));
    }

    #[test]
    fn already_installed_verbs_are_skipped_on_a_second_run() {
        let tmp = tempfile::tempdir().unwrap();
        let (runner, _) = fake_runner(tmp.path(), SUCCEEDS);
        runner
            .apply(&[dep("vcrun2022")], Duration::from_secs(20))
            .unwrap();

        let second = runner
            .apply(&[dep("vcrun2022")], Duration::from_secs(20))
            .unwrap();
        assert!(second[0].already_installed);
        assert!(second[0].summary().contains("already installed"));
    }

    #[test]
    fn a_failing_optional_dependency_does_not_abort_the_install() {
        let tmp = tempfile::tempdir().unwrap();
        let (runner, _) = fake_runner(tmp.path(), FAILS_ON_DOTNET);
        let optional = DependencySpec::new("dotnet48", "optional here").optional();
        let outcomes = runner
            .apply(&[optional, dep("vcrun2022")], Duration::from_secs(20))
            .unwrap();

        assert_eq!(outcomes.len(), 2);
        assert!(!outcomes[0].success && outcomes[0].optional);
        assert!(outcomes[0].summary().contains("optional"));
        assert!(outcomes[1].success, "the install must continue");
    }

    #[test]
    fn a_failing_required_dependency_aborts_with_the_log_referenced() {
        let tmp = tempfile::tempdir().unwrap();
        let (runner, _) = fake_runner(tmp.path(), FAILS_ON_DOTNET);
        match runner.apply(&[dep("dotnet48")], Duration::from_secs(20)) {
            Err(Error::InstallIncomplete { rationale }) => {
                assert!(rationale.contains("dotnet48"));
                assert!(rationale.contains("winetricks-dotnet48.log"));
            }
            other => panic!("expected InstallIncomplete, got {other:?}"),
        }
    }

    #[test]
    fn a_failure_stops_processing_later_required_verbs() {
        let tmp = tempfile::tempdir().unwrap();
        let (runner, prefix) = fake_runner(tmp.path(), FAILS_ON_DOTNET);
        let err = runner
            .apply(
                &[dep("dotnet48"), dep("vcrun2022")],
                Duration::from_secs(20),
            )
            .unwrap_err();
        assert!(matches!(err, Error::InstallIncomplete { .. }));
        // The verb after the failure must not have run.
        let log = std::fs::read_to_string(prefix.winetricks_log()).unwrap_or_default();
        assert!(!log.contains("vcrun2022"));
    }

    #[test]
    fn per_verb_logs_are_kept_separately() {
        let tmp = tempfile::tempdir().unwrap();
        let (runner, _) = fake_runner(tmp.path(), SUCCEEDS);
        let outcomes = runner
            .apply(
                &[dep("vcrun2022"), dep("corefonts")],
                Duration::from_secs(20),
            )
            .unwrap();
        assert!(outcomes[0].log_file.ends_with("winetricks-vcrun2022.log"));
        assert!(outcomes[1].log_file.ends_with("winetricks-corefonts.log"));
        assert_ne!(outcomes[0].log_file, outcomes[1].log_file);
    }

    #[test]
    fn an_empty_dependency_list_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let (runner, _) = fake_runner(tmp.path(), SUCCEEDS);
        assert!(runner
            .apply(&[], Duration::from_secs(5))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn installed_verbs_parses_the_winetricks_log() {
        let tmp = tempfile::tempdir().unwrap();
        let (runner, prefix) = fake_runner(tmp.path(), SUCCEEDS);
        std::fs::write(
            prefix.winetricks_log(),
            "# comment\nvcrun2022\n\n  corefonts  \n",
        )
        .unwrap();

        let verbs = runner.installed_verbs();
        assert_eq!(
            verbs,
            vec!["vcrun2022".to_string(), "corefonts".to_string()]
        );
        assert!(
            runner.is_installed("VCRUN2022"),
            "matching is case-insensitive"
        );
        assert!(!runner.is_installed("dotnet48"));
    }

    #[test]
    fn verb_names_are_sanitised_into_log_filenames() {
        let tmp = tempfile::tempdir().unwrap();
        let (runner, _) = fake_runner(tmp.path(), SUCCEEDS);
        let outcomes = runner
            .apply(
                &[DependencySpec::new("../evil verb", "t")],
                Duration::from_secs(20),
            )
            .unwrap();
        let name = outcomes[0].log_file.file_name().unwrap().to_string_lossy();
        assert!(
            !name.contains(".."),
            "path traversal must be impossible: {name}"
        );
        assert!(!name.contains(' '));
    }

    #[test]
    fn winetricks_lookup_prefers_a_managed_copy() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(find_winetricks(tmp.path()).is_none() || find_winetricks(tmp.path()).is_some());
        let managed = tmp.path().join("winetricks");
        std::fs::write(&managed, "#!/bin/sh\nexit 0\n").unwrap();
        assert_eq!(find_winetricks(tmp.path()).unwrap(), managed);
    }
}
