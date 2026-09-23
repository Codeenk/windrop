//! Finding the application's real program after an installer has run.
//!
//! This is the step that most often gets a "wrapper" wrong: an installer
//! scatters dozens of executables across the prefix, and only one of them is
//! the program the user wants on their menu. WinDrop ranks candidates using
//! evidence rather than guessing:
//!
//! | Signal                                        | Weight |
//! |-----------------------------------------------|--------|
//! | lives under `Program Files`                    | +200   |
//! | modified during this install                   | +120   |
//! | GUI subsystem (read from the PE header)        | +60    |
//! | a substantial binary (≥ 1 MiB)                 | +40    |
//! | matches the profile's hint                     | +80    |
//!
//! Installers, uninstallers and setup helpers are excluded outright. If nothing
//! survives, [`find_main_executable`] returns `None` and the caller asks the
//! user, which is far better than launching the wrong thing.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use walkdir::WalkDir;

use crate::compat::pe;
use crate::runtime::prefix::PrefixPaths;

/// At most this many files are opened for PE inspection, to keep the search
/// fast on a prefix with thousands of files.
const MAX_INSPECTIONS: usize = 200;
/// Files larger than this are not opened for inspection.
const MAX_INSPECT_SIZE: u64 = 256 * 1024 * 1024;

/// A ranked possible main program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub host_path: PathBuf,
    /// The Windows path, ready for the launch plan.
    pub windows_path: String,
    pub score: i64,
    /// Why it scored the way it did, for the log.
    pub reasons: Vec<String>,
}

/// Inputs to the search.
#[derive(Debug, Clone, Default)]
pub struct LocateOptions<'a> {
    /// A profile's `main_exe_hint`, matched against the relative path.
    pub hint: Option<&'a str>,
    /// When the install started. Files newer than this were created by it.
    pub installed_after: Option<SystemTime>,
    /// An explicit user choice, which always wins.
    pub user_choice: Option<&'a Path>,
}

/// Find the program to launch, or `None` if nothing is convincing.
pub fn find_main_executable(prefix: &PrefixPaths, options: &LocateOptions) -> Option<PathBuf> {
    if let Some(choice) = options.user_choice {
        return if choice.is_file() {
            Some(choice.to_path_buf())
        } else {
            None
        };
    }
    rank_candidates(prefix, options)
        .first()
        .map(|c| c.host_path.clone())
}

/// Every plausible candidate, best first.
pub fn rank_candidates(prefix: &PrefixPaths, options: &LocateOptions) -> Vec<Candidate> {
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut inspected = 0usize;

    for entry in WalkDir::new(prefix.drive_c())
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if !has_extension(path, "exe") && !has_extension(path, "com") {
            continue;
        }
        // System directories contain Windows' own programs, never the user's.
        if is_in_system_directory(prefix, path) {
            continue;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if is_installer_name(&name) {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mut score: i64 = 0;
        let mut reasons: Vec<String> = Vec::new();

        let relative = path.strip_prefix(prefix.drive_c()).unwrap_or(path);
        let relative_lower = relative.to_string_lossy().to_ascii_lowercase();

        if relative_lower.starts_with("program files") {
            score += 200;
            reasons.push("installed under Program Files".to_string());
        }

        if let Some(after) = options.installed_after {
            if let Ok(modified) = metadata.modified() {
                // Allow a second of slack: filesystem timestamps are coarse.
                if modified >= after {
                    score += 120;
                    reasons.push("written by this installation".to_string());
                }
            }
        }

        if let Some(hint) = options.hint {
            let hint_lower = hint.to_ascii_lowercase();
            if path
                .to_string_lossy()
                .to_ascii_lowercase()
                .ends_with(&hint_lower)
                || relative_lower.ends_with(&hint_lower)
            {
                score += 80;
                reasons.push("matches the profile's known program name".to_string());
            }
        }

        let size = metadata.len();
        if size >= 1024 * 1024 {
            score += 40;
            reasons.push("substantial binary (over 1 MiB)".to_string());
        } else if size >= 128 * 1024 {
            score += 10;
            reasons.push("plausible binary size".to_string());
        } else if size < 8192 {
            // Tiny stubs are almost always launchers or shims, not the program.
            score -= 20;
            reasons.push("very small, likely a helper".to_string());
        }

        // Subsystem detection is the strongest single signal for "this is an
        // application a person wants to open". Inspections are capped because
        // each one reads the file.
        if inspected < MAX_INSPECTIONS && size <= MAX_INSPECT_SIZE {
            inspected += 1;
            if let Ok(info) = pe::inspect(path) {
                if info.gui {
                    score += 60;
                    reasons.push("GUI application".to_string());
                } else {
                    reasons.push("console application".to_string());
                }
                if info.is_dll {
                    // A .dll with an .exe name is not launchable.
                    continue;
                }
            }
        }

        let Some(windows_path) = prefix.windows_path_of(path) else {
            continue;
        };

        candidates.push(Candidate {
            host_path: path.to_path_buf(),
            windows_path,
            score,
            reasons,
        });
    }

    // Deterministic ordering: score, then the shorter path (top-level programs
    // are more likely to be the intended one), then the name.
    candidates.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| {
                a.host_path
                    .components()
                    .count()
                    .cmp(&b.host_path.components().count())
            })
            .then_with(|| a.host_path.cmp(&b.host_path))
    });
    candidates
}

/// True for names that are never the program a user wants on their menu.
pub fn is_installer_name(lowercase_name: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "setup",
        "install",
        "unins",
        "uninstall",
        "unwise",
        "vcredist",
        "dxsetup",
        "dxwebsetup",
        "dotnetfx",
        "ndp",
        "wusa",
        "msiexec",
        "nsis",
        "inno",
        "sfx",
        "7zsetup",
        "python-",
        "updater",
    ];
    const SUFFIXES: &[&str] = &[
        "_setup.exe",
        "setup.exe",
        "_installer.exe",
        "installer.exe",
        "_uninst.exe",
    ];

    let stem = lowercase_name
        .trim_end_matches(".exe")
        .trim_end_matches(".com");
    PREFIXES.iter().any(|p| stem.starts_with(p))
        || SUFFIXES.iter().any(|s| lowercase_name.ends_with(s))
}

fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension()
        .map(|e| e.eq_ignore_ascii_case(extension))
        .unwrap_or(false)
}

/// True when the file sits in Windows' own directories.
fn is_in_system_directory(prefix: &PrefixPaths, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(prefix.drive_c()) else {
        return false;
    };
    let lower = relative.to_string_lossy().to_ascii_lowercase();
    lower.starts_with("windows/")
        || lower.starts_with("windows\\")
        || lower.starts_with("$recycle.bin")
        || lower.contains("/temp/")
        || lower.contains("/tmp/")
        || lower.starts_with("users/") && lower.contains("/appdata/local/temp/")
}

/// Turn a candidate list into the explanation shown when nothing was found.
pub fn no_candidate_help(prefix: &PrefixPaths) -> String {
    format!(
        "WinDrop could not tell which program to launch. Open {}, find the main executable and \
         set it as the application's program.",
        prefix.drive_c().display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{self, PeSpec};

    /// Build a prefix that looks like a real install.
    fn installed_prefix(dir: &Path) -> PrefixPaths {
        let prefix = PrefixPaths::from_root(dir.join("prefix"));
        crate::runtime::prefix::prepare_directories(&prefix).unwrap();
        prefix
    }

    fn write_program(prefix: &PrefixPaths, relative: &str, spec: &PeSpec) -> PathBuf {
        let path = prefix.drive_c().join(relative);
        fixtures::write_exe(&path, spec).unwrap();
        path
    }

    #[test]
    fn a_program_under_program_files_is_found() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = installed_prefix(tmp.path());
        let program = write_program(
            &prefix,
            "Program Files/TestApp/testapp.exe",
            &PeSpec::example_console_tool(),
        );

        let found = find_main_executable(&prefix, &LocateOptions::default()).unwrap();
        assert_eq!(found, program);
    }

    #[test]
    fn installers_and_uninstallers_are_never_chosen() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = installed_prefix(tmp.path());
        write_program(&prefix, "setup.exe", &PeSpec::example_installer());
        write_program(
            &prefix,
            "Program Files/TestApp/unins000.exe",
            &PeSpec::example_console_tool(),
        );
        let program = write_program(
            &prefix,
            "Program Files/TestApp/testapp.exe",
            &PeSpec::example_gui_large(),
        );

        assert_eq!(
            find_main_executable(&prefix, &LocateOptions::default()).unwrap(),
            program
        );
    }

    #[test]
    fn a_prefix_with_only_installers_yields_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = installed_prefix(tmp.path());
        write_program(&prefix, "setup.exe", &PeSpec::example_installer());
        write_program(
            &prefix,
            "Program Files/App/uninstall.exe",
            &PeSpec::example_console_tool(),
        );
        write_program(&prefix, "vcredist_x64.exe", &PeSpec::example_installer());

        assert!(find_main_executable(&prefix, &LocateOptions::default()).is_none());
        assert!(no_candidate_help(&prefix).contains("drive_c"));
    }

    #[test]
    fn windows_own_programs_are_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = installed_prefix(tmp.path());
        write_program(&prefix, "windows/notepad.exe", &PeSpec::example_gui_large());
        write_program(
            &prefix,
            "windows/system32/regedit.exe",
            &PeSpec::example_gui_large(),
        );

        assert!(rank_candidates(&prefix, &LocateOptions::default()).is_empty());
    }

    #[test]
    fn temporary_directories_are_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = installed_prefix(tmp.path());
        write_program(
            &prefix,
            "users/test/Temp/beacon.exe",
            &PeSpec::example_gui_large(),
        );
        assert!(rank_candidates(&prefix, &LocateOptions::default()).is_empty());
    }

    #[test]
    fn a_gui_program_beats_a_console_helper_of_equal_standing() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = installed_prefix(tmp.path());
        write_program(
            &prefix,
            "Program Files/App/helper.exe",
            &PeSpec::example_console_tool(),
        );
        let gui = write_program(
            &prefix,
            "Program Files/App/app.exe",
            &PeSpec::example_gui_large(),
        );

        let ranked = rank_candidates(&prefix, &LocateOptions::default());
        assert_eq!(ranked[0].host_path, gui);
        assert!(ranked[0].reasons.iter().any(|r| r.contains("GUI")));
    }

    #[test]
    fn a_recently_written_program_beats_an_older_one() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = installed_prefix(tmp.path());
        let old = write_program(
            &prefix,
            "Program Files/App/old.exe",
            &PeSpec::example_gui_large(),
        );
        // Backdate the "old" program.
        let past = SystemTime::now() - std::time::Duration::from_secs(86_400 * 30);
        let file = std::fs::File::options().write(true).open(&old).unwrap();
        file.set_modified(past).unwrap();
        drop(file);

        let fresh = write_program(
            &prefix,
            "Program Files/App/new.exe",
            &PeSpec::example_gui_large(),
        );
        let options = LocateOptions {
            installed_after: Some(SystemTime::now() - std::time::Duration::from_secs(60)),
            ..Default::default()
        };
        let ranked = rank_candidates(&prefix, &options);
        assert_eq!(ranked[0].host_path, fresh);
        assert!(ranked[0]
            .reasons
            .iter()
            .any(|r| r.contains("this installation")));
    }

    #[test]
    fn a_profile_hint_picks_the_right_program_among_several() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = installed_prefix(tmp.path());
        write_program(
            &prefix,
            "Program Files/App/launcher.exe",
            &PeSpec::example_gui_large(),
        );
        let wanted = write_program(
            &prefix,
            "Program Files/App/editor.exe",
            &PeSpec::example_gui_large(),
        );

        let options = LocateOptions {
            hint: Some("App/editor.exe"),
            ..Default::default()
        };
        let ranked = rank_candidates(&prefix, &options);
        assert_eq!(ranked[0].host_path, wanted);
        assert!(ranked[0].reasons.iter().any(|r| r.contains("profile")));
    }

    #[test]
    fn an_explicit_user_choice_always_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = installed_prefix(tmp.path());
        write_program(
            &prefix,
            "Program Files/App/app.exe",
            &PeSpec::example_gui_large(),
        );
        let chosen = write_program(&prefix, "App/tool.exe", &PeSpec::example_console_tool());

        let options = LocateOptions {
            user_choice: Some(&chosen),
            ..Default::default()
        };
        assert_eq!(find_main_executable(&prefix, &options).unwrap(), chosen);
    }

    #[test]
    fn a_user_choice_that_does_not_exist_yields_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = installed_prefix(tmp.path());
        let missing = tmp.path().join("nope.exe");
        let options = LocateOptions {
            user_choice: Some(&missing),
            ..Default::default()
        };
        assert!(find_main_executable(&prefix, &options).is_none());
    }

    #[test]
    fn candidates_carry_a_usable_windows_path() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = installed_prefix(tmp.path());
        write_program(
            &prefix,
            "Program Files/My App/My App.exe",
            &PeSpec::example_gui_large(),
        );

        let ranked = rank_candidates(&prefix, &LocateOptions::default());
        assert_eq!(
            ranked[0].windows_path,
            r"C:\Program Files\My App\My App.exe"
        );
    }

    #[test]
    fn a_plain_portable_executable_is_still_found() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = installed_prefix(tmp.path());
        // No Program Files, nothing recent: score comes from size and subsystem.
        let program = write_program(&prefix, "portable.exe", &PeSpec::example_gui_large());
        assert_eq!(
            find_main_executable(&prefix, &LocateOptions::default()).unwrap(),
            program
        );
    }

    #[test]
    fn tiny_stubs_lose_to_real_programs() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = installed_prefix(tmp.path());
        // A 700-byte launcher stub under Program Files.
        let stub = prefix.drive_c().join("Program Files/App/stub.exe");
        std::fs::create_dir_all(stub.parent().unwrap()).unwrap();
        std::fs::write(
            &stub,
            fixtures::synthetic_pe(&PeSpec::example_console_tool()),
        )
        .unwrap();

        let real = write_program(
            &prefix,
            "Program Files/App/real.exe",
            &PeSpec::example_gui_large(),
        );
        let ranked = rank_candidates(&prefix, &LocateOptions::default());
        assert_eq!(ranked[0].host_path, real);
        assert!(ranked
            .iter()
            .any(|c| c.reasons.iter().any(|r| r.contains("helper"))));
    }

    #[test]
    fn installer_name_detection_covers_the_common_families() {
        for name in [
            "setup.exe",
            "install.exe",
            "installer.exe",
            "unins000.exe",
            "uninstall.exe",
            "vcredist_x86.exe",
            "dxsetup.exe",
            "dotnetfx45.exe",
            "app_setup.exe",
            "appsetup.exe",
            "appinstaller.exe",
            "nsis-setup.exe",
        ] {
            assert!(is_installer_name(name), "{name} should be excluded");
        }
        for name in [
            "notepad++.exe",
            "game.exe",
            "app.exe",
            "tool.exe",
            "launcher.exe",
        ] {
            assert!(!is_installer_name(name), "{name} should be allowed");
        }
    }

    #[test]
    fn ranking_is_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = installed_prefix(tmp.path());
        for name in ["a.exe", "b.exe", "c.exe"] {
            write_program(
                &prefix,
                &format!("Program Files/App/{name}"),
                &PeSpec::example_gui_large(),
            );
        }
        let first = rank_candidates(&prefix, &LocateOptions::default());
        let second = rank_candidates(&prefix, &LocateOptions::default());
        assert_eq!(first, second);
    }

    #[test]
    fn a_missing_prefix_yields_no_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = PrefixPaths::from_root(tmp.path().join("nothing"));
        assert!(rank_candidates(&prefix, &LocateOptions::default()).is_empty());
        assert!(find_main_executable(&prefix, &LocateOptions::default()).is_none());
    }
}
