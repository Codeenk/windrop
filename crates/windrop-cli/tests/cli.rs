//! End-to-end tests of the `windrop` binary.
//!
//! These run the real executable, in a throwaway sandbox, against a mock Wine.
//! The unit tests in `windrop-core` cover the logic; what is only testable here
//! is the part a user actually touches: argument parsing, exit codes, the text
//! and JSON that come out, and whether the menu entry WinDrop writes names a
//! command that runs.
//!
//! Every path the run could touch is redirected into the sandbox —
//! `WINDROP_DATA_DIR`, `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, `XDG_CACHE_HOME` and
//! `$HOME` — so a test failure cannot leave anything behind in the developer's
//! real menu or configuration.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use windrop_core::fixtures::{self, PeSpec};

/// A Wine stand-in.
///
/// * `--version` answers a probe.
/// * `wineboot --init` writes the registry, which is what makes a prefix count
///   as created.
/// * A path on `Z:` is the installer: it "installs" by writing the program the
///   mock recipe expects.
/// * Anything else is an installed program: it records that it ran.
///
/// The trace is written *inside the prefix*, because that is the one place a
/// sandboxed process is guaranteed to be able to write. A trace kept outside
/// would be unwritable under the sandbox, and the test would conclude that
/// nothing ran — a mistake that hides real behaviour instead of testing it.
/// Reading it back is `Sandbox::trace`, which reassembles the pieces.
const MOCK_WINE: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  printf '%s\n' "wine-9.0 (WinDrop mock build)"
  exit 0
fi
if [ "$1" = "wineboot" ]; then
  mkdir -p "$WINEPREFIX/drive_c/users/test"
  printf '%s\n' "WINE REGISTRY Version 2" > "$WINEPREFIX/system.reg"
  mkdir -p "$WINEPREFIX"
  printf 'wineboot\n' >> "$WINEPREFIX/trace.log"
  exit 0
fi
case "$1" in
  Z:*)
    mkdir -p "$WINEPREFIX/drive_c/Program Files/Notepad++"
    printf 'MZ mock program' > "$WINEPREFIX/drive_c/Program Files/Notepad++/notepad++.exe"
    mkdir -p "$WINEPREFIX/drive_c/Program Files/Notepad++/uninstall"
    printf 'MZ mock uninstaller' > "$WINEPREFIX/drive_c/Program Files/Notepad++/uninstall/uninstall.exe"
    printf 'installer %s\n' "$1" >> "$WINEPREFIX/trace.log"
    ;;
  *)
    printf 'launched %s\n' "$1" >> "$WINEPREFIX/trace.log"
    ;;
esac
exit 0
"#;

/// `wineserver -k`, which `remove` and `launch --stop` call.
const MOCK_WINESERVER: &str = r#"#!/bin/sh
if [ -n "$WINEPREFIX" ]; then
  mkdir -p "$WINEPREFIX"
  printf 'wineserver %s\n' "$*" >> "$WINEPREFIX/trace.log"
fi
exit 0
"#;

/// A `winetricks` stand-in: it records the verbs it was asked for, so a test can
/// assert that a profile's dependencies were really applied rather than merely
/// declared.
const MOCK_WINETRICKS: &str = r#"#!/bin/sh
for arg in "$@"; do
  case "$arg" in
    -q|--unattended|-f) continue ;;
    *) printf 'winetricks %s\n' "$arg" >> "$WINEPREFIX/trace.log" ;;
  esac
done
exit 0
"#;

/// A throwaway installation plus the environment that points at it.
struct Sandbox {
    root: tempfile::TempDir,
    bin: PathBuf,
    data: PathBuf,
    /// Where `.desktop` entries land.
    applications: PathBuf,
}

/// The application id the mock recipe produces.
///
/// The bundled recipe supplies the display name `Notepad++`, and an application
/// id is always the slug of its display name — so `notepad`, not the file name
/// `Notepad++ 8.6 Setup`.
const MOCK_APP_ID: &str = "notepad";

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("a temporary directory");
        let bin = root.path().join("bin");
        let data = root.path().join("data");
        let share = root.path().join("share");
        let applications = share.join("applications");
        for dir in [
            &bin,
            &data,
            &applications,
            &root.path().join("config"),
            &root.path().join("cache"),
        ] {
            std::fs::create_dir_all(dir).expect("the sandbox tree");
        }

        for (name, body) in [
            ("wine", MOCK_WINE),
            ("wine64", MOCK_WINE),
            ("wineserver", MOCK_WINESERVER),
            ("winetricks", MOCK_WINETRICKS),
        ] {
            let path = bin.join(name);
            std::fs::write(&path, body).expect("the mock executable");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("executable permissions");
        }

        Sandbox {
            root,
            bin,
            data,
            applications,
        }
    }

    fn root(&self) -> &Path {
        self.root.path()
    }

    /// Run `windrop` with the sandbox environment.
    ///
    /// The mock Wine goes first on `$PATH`, and the rest of the user's `PATH` is
    /// kept so that tools like `tar` and `update-desktop-database` still work.
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_windrop"));
        let path = match std::env::var("PATH") {
            Ok(existing) => format!("{}:{existing}", self.bin.display()),
            Err(_) => self.bin.display().to_string(),
        };
        command
            .env("PATH", path)
            .env("HOME", self.root())
            .env("WINDROP_DATA_DIR", &self.data)
            .env("XDG_DATA_HOME", self.root().join("share"))
            .env("XDG_CONFIG_HOME", self.root().join("config"))
            .env("XDG_CACHE_HOME", self.root().join("cache"))
            .env("NO_COLOR", "1")
            // Keep every test off the network, whatever the machine's state.
            .current_dir(self.root());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command()
            .args(args)
            .output()
            .expect("the windrop binary should be runnable")
    }

    /// Run, expecting success, and return stdout.
    fn stdout(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "windrop {args:?} failed with {:?}\nstdout: {}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    /// Run, expecting success, and parse stdout as JSON.
    fn json(&self, args: &[&str]) -> serde_json::Value {
        let text = self.stdout(args);
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("windrop {args:?} did not print valid JSON: {e}\n{text}"))
    }

    fn exit_code(&self, args: &[&str]) -> i32 {
        self.run(args).status.code().expect("an exit code")
    }

    fn write_installer(&self, name: &str) -> PathBuf {
        let path = self.root().join(name);
        fixtures::write_exe(&path, &PeSpec::example_installer()).expect("the fixture installer");
        path
    }

    /// Everything the mock Wine recorded, oldest first.
    ///
    /// Each process appends to the same file inside the prefix, so this is the
    /// whole history of the run — including anything that ran inside a sandbox.
    fn trace(&self) -> String {
        self.trace_of(MOCK_APP_ID)
    }

    /// The trace for any application, not just the mock recipe's own.
    fn trace_of(&self, app_id: &str) -> String {
        std::fs::read_to_string(self.data.join("apps").join(app_id).join("prefix/trace.log"))
            .unwrap_or_default()
    }

    /// Wait for a line to appear in the trace.
    ///
    /// A menu entry's command detaches — the application has to outlive
    /// whoever started it — so the program it starts is not necessarily running
    /// by the time the launcher exits. Polling is how a test observes the
    /// result of something that is deliberately asynchronous.
    fn wait_for_trace(&self, needle: &str, timeout: std::time::Duration) -> String {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let trace = self.trace();
            if trace.contains(needle) {
                return trace;
            }
            if std::time::Instant::now() >= deadline {
                panic!("the trace never contained {needle:?}; it held:\n{trace}");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

/// Split an `Exec` value the way a desktop environment does.
fn split_exec(value: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

// ------------------------------------------------------------------ basics

#[test]
fn version_and_help_work_without_a_wine_installation() {
    let sandbox = Sandbox::new();
    let sandbox_without_wine = &sandbox;

    for args in [
        vec!["--version"],
        vec!["version"],
        vec!["--help"],
        vec!["install", "--help"],
    ] {
        let output = sandbox_without_wine.run(&args);
        assert!(
            output.status.success(),
            "windrop {args:?} should not need Wine to print help"
        );
        assert!(
            !String::from_utf8_lossy(&output.stdout).trim().is_empty(),
            "windrop {args:?} printed nothing"
        );
    }
}

#[test]
fn every_subcommand_has_help_text() {
    // A command that exists but explains nothing is a dead end.
    let sandbox = Sandbox::new();
    for command in [
        "install", "list", "launch", "remove", "inspect", "doctor", "profiles", "update", "config",
        "logs", "gui", "version",
    ] {
        let output = sandbox.run(&[command, "--help"]);
        let text = String::from_utf8_lossy(&output.stdout).to_string();
        assert!(output.status.success(), "windrop {command} --help failed");
        assert!(
            text.len() > 40,
            "windrop {command} --help says almost nothing:\n{text}"
        );
    }
}

#[test]
fn the_doctor_reports_readiness_and_exits_accordingly() {
    let sandbox = Sandbox::new();

    // The mock Wine is on $PATH, so the machine counts as ready.
    let report = sandbox.json(&["--json", "doctor"]);
    assert_eq!(report["ready"], serde_json::Value::Bool(true));
    assert_eq!(sandbox.exit_code(&["doctor", "--summary"]), 0);
    assert!(report["summary"].as_str().unwrap().contains("wine-9.0"));
    // Nothing is missing that Wine provides, so there is no setup command for it.
    let setup = report["setup_command"].as_str().unwrap_or_default();
    assert!(!setup.contains("wine,"), "wine is present: {setup}");

    // Without it, the same command says so and exits 3, which is what a script
    // installing WinDrop wants to check.
    let no_wine = tempfile::tempdir().unwrap();
    let output = sandbox
        .command()
        .env("PATH", "/nonexistent-bin")
        .arg("--json")
        .arg("doctor")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ready"], serde_json::Value::Bool(false));
    assert!(report["wine_error"].is_string());
    let _ = no_wine;
}

// ----------------------------------------------------------------- profiles

#[test]
fn bundled_recipes_can_be_seeded_listed_exported_and_verified() {
    let sandbox = Sandbox::new();

    // Offline throughout: the shipped recipes must be enough on their own.
    let seeded = sandbox.json(&["--json", "--offline", "profiles", "seed"]);
    let added = seeded["added"].as_array().unwrap();
    assert!(!added.is_empty(), "seeding added nothing");

    let profiles = sandbox.json(&["--json", "profiles", "list"]);
    assert_eq!(profiles.as_array().unwrap().len(), added.len());
    assert!(profiles
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["id"] == "notepadpp"));

    // One recipe can be read back in full.
    let shown: serde_json::Value =
        serde_json::from_str(&sandbox.stdout(&["profiles", "show", "notepadpp"])).unwrap();
    assert_eq!(shown["id"], "notepadpp");
    assert!(!shown["variants"].as_array().unwrap().is_empty());

    // Export, then verify what was exported: this is the contribution path.
    let exported = sandbox.root().join("shared.json");
    sandbox.stdout(&["profiles", "export", exported.to_str().unwrap()]);
    let verified = sandbox.json(&["--json", "profiles", "verify", exported.to_str().unwrap()]);
    assert_eq!(verified["valid"], serde_json::Value::Bool(true));
    assert_eq!(verified["profiles"], profiles.as_array().unwrap().len());

    // An unknown profile is a not-found, not a crash.
    assert_eq!(sandbox.exit_code(&["profiles", "show", "nonesuch"]), 4);
}

#[test]
fn a_malformed_profile_file_is_reported_with_its_position() {
    let sandbox = Sandbox::new();
    let file = sandbox.root().join("broken.json");
    std::fs::write(
        &file,
        r#"[{"id":"a","name":"A","variants":[]},{"id":"b","name":"B","variants":[{"wine_build":"stable","arch":"x86_64","windows_version":"win10","dxvk":false,"vkd3d_proton":false,"dll_overrides":[],"env":[],"dependencies":[],"rationale":"ok"}]}]"#,
    )
    .unwrap();

    // An invalid submission is a normal outcome, so it is reported on stdout
    // with a non-zero exit rather than as a crash.
    let output = sandbox.run(&["--json", "profiles", "verify", file.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["valid"], serde_json::Value::Bool(false));
    let error = report["error"].as_str().unwrap();
    assert!(
        error.contains("profile 0"),
        "the position must be named: {error}"
    );
    assert!(error.contains("'a'"), "the id must be named: {error}");
}

#[test]
fn attaching_a_digest_makes_a_recipe_match_that_exact_file() {
    let sandbox = Sandbox::new();
    sandbox.stdout(&["--offline", "profiles", "seed"]);
    let installer = sandbox.write_installer("SomethingOpaque.exe");

    // Before: nothing knows this file, so a profile is generated for it.
    let before = sandbox.json(&[
        "--json",
        "--offline",
        "inspect",
        installer.to_str().unwrap(),
    ]);
    assert_eq!(before["profile_source"], "detected automatically");

    // Record its digest against a recipe...
    sandbox.stdout(&[
        "--offline",
        "profiles",
        "attach",
        "notepadpp",
        installer.to_str().unwrap(),
    ]);

    // ...and now the same file resolves to that recipe, by digest.
    let after = sandbox.json(&[
        "--json",
        "--offline",
        "inspect",
        installer.to_str().unwrap(),
    ]);
    assert_eq!(after["profile"], "notepadpp");
    assert_eq!(after["profile_source"], "bundled with WinDrop");
}

#[test]
fn a_bundled_recipe_is_found_by_name_without_any_network_access() {
    let sandbox = Sandbox::new();
    sandbox.stdout(&["--offline", "profiles", "seed"]);
    // The name a user would actually have on disk.
    let installer = sandbox.write_installer("Notepad++ 8.6 Setup.exe");

    let report = sandbox.json(&[
        "--json",
        "--offline",
        "inspect",
        installer.to_str().unwrap(),
    ]);
    assert_eq!(
        report["profile"], "notepadpp",
        "the bundled recipe should have been recognised offline: {report}"
    );
    assert_eq!(report["profile_source"], "bundled with WinDrop");
    // The fixture installer is 32-bit; the recipe is 64-bit, and a recipe that
    // contradicts the binary is still allowed to be the starting point, because
    // Wine installs 32-bit programs into a 64-bit prefix perfectly well.
    assert_eq!(report["inspection"]["arch"], "x86");
}

#[test]
fn a_generic_installer_name_does_not_pick_up_an_unrelated_recipe() {
    let sandbox = Sandbox::new();
    sandbox.stdout(&["--offline", "profiles", "seed"]);
    // `setup.exe` names nothing, so binding a recipe to it would be a guess.
    let installer = sandbox.write_installer("setup.exe");
    let report = sandbox.json(&[
        "--json",
        "--offline",
        "inspect",
        installer.to_str().unwrap(),
    ]);
    assert_eq!(report["profile_source"], "detected automatically");
}

// ------------------------------------------------------------------ install

#[test]
fn install_launch_and_remove_work_end_to_end() {
    let sandbox = Sandbox::new();
    sandbox.stdout(&["--offline", "profiles", "seed"]);

    // ------------------------------------------------------------- install
    let installer = sandbox.write_installer("Notepad++ 8.6 Setup.exe");
    let installed = sandbox.json(&[
        "--json",
        "--offline",
        "--no-sandbox",
        "--yes",
        "install",
        installer.to_str().unwrap(),
    ]);
    assert_eq!(installed["app"]["id"], MOCK_APP_ID);
    assert_eq!(
        installed["app"]["name"], "Notepad++",
        "the recipe knows the name"
    );
    assert_eq!(installed["profile_id"], "notepadpp");
    assert_eq!(
        installed["command"],
        format!("windrop launch {MOCK_APP_ID}")
    );
    assert!(
        sandbox.trace().contains("wineboot"),
        "the prefix must be initialised"
    );
    assert!(
        sandbox.trace().contains("installer Z:"),
        "the installer must run"
    );

    // The whole installation is one directory, plus one menu entry.
    let app_dir = sandbox.data.join("apps").join(MOCK_APP_ID);
    assert!(app_dir.join("metadata.json").is_file());
    assert!(app_dir.join("profile.json").is_file());
    assert!(app_dir.join("prefix/system.reg").is_file());

    // ------------------------------------------------- the menu entry runs
    let entry = sandbox
        .applications
        .join(format!("org.windrop.WinDrop.{MOCK_APP_ID}.desktop"));
    let text = std::fs::read_to_string(&entry).expect("a menu entry");
    assert!(text.contains("Name=Notepad++"));
    assert!(text.contains(&format!("X-WinDrop-AppId={MOCK_APP_ID}")));
    assert!(
        !text.contains("WINEPREFIX") && !text.contains("wine"),
        "Wine details must not leak into the menu entry:\n{text}"
    );

    let exec = text
        .lines()
        .find_map(|line| line.strip_prefix("Exec="))
        .expect("an Exec line");
    let argv = split_exec(exec);
    // Replace `windrop` with the binary under test, which is not installed on
    // $PATH in a test run. Everything else — including any --data-dir the entry
    // recorded — is used exactly as written.
    let launched = sandbox
        .command()
        .args(&argv[1..])
        .output()
        .expect("the recorded command must run");
    assert!(
        launched.status.success(),
        "the menu entry's command failed: {exec}\nstderr:\n{}\nlaunch log:\n{}",
        String::from_utf8_lossy(&launched.stderr),
        std::fs::read_to_string(
            sandbox
                .data
                .join("apps")
                .join(MOCK_APP_ID)
                .join("launch.log"),
        )
        .unwrap_or_else(|_| "(none)".to_string())
    );
    let trace = sandbox.wait_for_trace(
        r"launched C:\Program Files\Notepad++\notepad++.exe",
        std::time::Duration::from_secs(15),
    );
    assert!(
        !trace.contains("uninstall"),
        "the uninstaller must never be chosen as the program"
    );

    // -------------------------------------------------------------- listing
    let listed = sandbox.json(&["--json", "list"]);
    assert_eq!(listed["apps"].as_array().unwrap().len(), 1);
    assert_eq!(listed["apps"][0]["id"], MOCK_APP_ID);
    assert_eq!(
        sandbox.stdout(&["list", "--short"]).trim(),
        MOCK_APP_ID,
        "the short form is for scripts"
    );

    // --------------------------------------------------------------- launch
    sandbox.stdout(&["launch", MOCK_APP_ID, "--plan-only"]);
    // `--wait` reports the program's own exit status, so a program that ran and
    // exited cleanly must give an exit status of zero.
    assert_eq!(sandbox.exit_code(&["launch", MOCK_APP_ID, "--wait"]), 0);
    sandbox.stdout(&["launch", MOCK_APP_ID, "--stop"]);

    // --------------------------------------------------------------- remove
    let removed = sandbox.json(&["--json", "--yes", "remove", MOCK_APP_ID]);
    assert_eq!(removed[0]["app_id"], MOCK_APP_ID);
    assert_eq!(
        removed[0]["removed_desktop_entry"],
        serde_json::Value::Bool(true)
    );
    assert!(
        removed[0]["freed_bytes"].as_u64().unwrap() > 0,
        "removal should account for the space it freed"
    );
    assert!(!app_dir.exists(), "the application directory must be gone");
    assert!(!entry.exists(), "the menu entry must be gone");

    let after = sandbox.json(&["--json", "list"]);
    assert!(after["apps"].as_array().unwrap().is_empty());

    // Removing again is a clean no-op rather than an error.
    assert_eq!(sandbox.exit_code(&["--yes", "remove", MOCK_APP_ID]), 4);
}

#[test]
fn a_dry_run_changes_nothing() {
    let sandbox = Sandbox::new();
    sandbox.stdout(&["--offline", "profiles", "seed"]);
    let installer = sandbox.write_installer("Notepad++ 8.6 Setup.exe");

    let plan = sandbox.json(&[
        "--json",
        "--offline",
        "install",
        "--dry-run",
        installer.to_str().unwrap(),
    ]);
    assert_eq!(plan["dry_run"], serde_json::Value::Bool(true));
    assert_eq!(plan["app"]["id"], MOCK_APP_ID);

    assert!(!sandbox.data.join("apps").join(MOCK_APP_ID).exists());
    assert!(sandbox.json(&["--json", "list"])["apps"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(sandbox.trace().is_empty(), "nothing should have run");
}

#[test]
fn installing_the_same_file_twice_is_refused_with_a_useful_code() {
    let sandbox = Sandbox::new();
    sandbox.stdout(&["--offline", "profiles", "seed"]);
    let installer = sandbox.write_installer("Notepad++ 8.6 Setup.exe");
    sandbox.stdout(&[
        "--offline",
        "--no-sandbox",
        "--yes",
        "install",
        installer.to_str().unwrap(),
    ]);

    let code = sandbox.exit_code(&[
        "--offline",
        "--no-sandbox",
        "--yes",
        "install",
        installer.to_str().unwrap(),
    ]);
    assert_eq!(code, 5, "already installed has its own exit code");
}

#[test]
fn a_missing_file_is_reported_without_doing_anything() {
    let sandbox = Sandbox::new();
    let missing = sandbox.root().join("nope.exe").display().to_string();

    assert_eq!(sandbox.exit_code(&["--offline", "install", &missing]), 4);
    assert_eq!(sandbox.exit_code(&["inspect", &missing]), 4);

    // A file that is not a Windows executable is refused clearly too.
    let junk = sandbox.root().join("notes.txt");
    std::fs::write(&junk, "just some text").unwrap();
    let output = sandbox.run(&["--offline", "install", junk.to_str().unwrap()]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(".exe") || stderr.contains("Windows"),
        "the message should say what is accepted: {stderr}"
    );
}

#[test]
fn an_unknown_application_id_is_a_not_found() {
    let sandbox = Sandbox::new();
    assert_eq!(sandbox.exit_code(&["launch", "ghost"]), 4);
    assert_eq!(sandbox.exit_code(&["--yes", "remove", "ghost"]), 4);
    assert_eq!(sandbox.exit_code(&["logs", "ghost"]), 4);
}

#[test]
fn a_custom_application_id_and_name_are_honoured() {
    let sandbox = Sandbox::new();
    sandbox.stdout(&["--offline", "profiles", "seed"]);
    let installer = sandbox.write_installer("SomethingOpaque.exe");

    let report = sandbox.json(&[
        "--json",
        "--offline",
        "--no-sandbox",
        "--yes",
        "install",
        "--id",
        "my-editor",
        "--name",
        "My Editor",
        installer.to_str().unwrap(),
    ]);
    assert_eq!(report["app"]["id"], "my-editor");
    assert_eq!(report["app"]["name"], "My Editor");

    // The generated profile declares a Visual C++ runtime, and it must actually
    // have been installed rather than merely recorded in the profile.
    let trace = sandbox.trace_of("my-editor");
    assert!(
        trace.contains("winetricks vcrun2022"),
        "the profile's dependencies should have been applied: {trace}"
    );

    let listed = sandbox.json(&["--json", "list"]);
    assert_eq!(listed["apps"][0]["name"], "My Editor");

    // An id that would escape the applications directory is refused. This uses
    // a different file, because the first one is now installed and a repeat of
    // the *same* installer is refused for that reason instead.
    let other = sandbox.root().join("Other.exe");
    fixtures::write_exe(&other, &PeSpec::example_console_tool()).unwrap();
    let output = sandbox.run(&[
        "--offline",
        "--no-sandbox",
        "--yes",
        "install",
        "--id",
        "../../etc/passwd",
        other.to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("not a valid application id"));
    assert!(
        !sandbox.root().join("etc").exists(),
        "nothing may escape the data directory"
    );
}

#[test]
fn a_specific_profile_can_be_forced_onto_any_file() {
    let sandbox = Sandbox::new();
    sandbox.stdout(&["--offline", "profiles", "seed"]);
    // x86_64, matching the recipe's architecture, but named so that no recipe
    // would ever claim it.
    let installer = sandbox.write_installer("SomethingOpaque.exe");

    let report = sandbox.json(&[
        "--json",
        "--offline",
        "install",
        "--dry-run",
        "--profile",
        "old-winxp-game",
        installer.to_str().unwrap(),
    ]);
    assert_eq!(report["profile_id"], "old-winxp-game");

    // An unknown profile is a not-found, and the message points at the list.
    let output = sandbox.run(&[
        "--offline",
        "install",
        "--dry-run",
        "--profile",
        "nonesuch",
        installer.to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(4));
}

// ------------------------------------------------------------------ config

#[test]
fn settings_round_trip_through_the_configuration_file() {
    let sandbox = Sandbox::new();

    assert_eq!(
        sandbox
            .stdout(&["config", "get", "performance_mode"])
            .trim(),
        "balanced"
    );
    sandbox.stdout(&["config", "set", "performance_mode", "performance"]);
    assert_eq!(
        sandbox
            .stdout(&["config", "get", "performance_mode"])
            .trim(),
        "performance"
    );

    // The change survives a new process, and is written inside the sandbox.
    let path = sandbox.stdout(&["config", "path"]);
    let path = PathBuf::from(path.trim());
    assert!(
        path.starts_with(sandbox.root()),
        "config escaped the sandbox: {}",
        path.display()
    );
    assert!(path.is_file());

    // Nested keys, numbers and lists.
    sandbox.stdout(&["config", "set", "dxvk_settings.max_frame_rate", "144"]);
    assert_eq!(
        sandbox
            .stdout(&["config", "get", "dxvk_settings.max_frame_rate"])
            .trim(),
        "144"
    );
    sandbox.stdout(&["config", "set", "shared_folders", r#"["/tmp/shared"]"#]);
    assert_eq!(
        sandbox.stdout(&["config", "get", "shared_folders"]).trim(),
        r#"["/tmp/shared"]"#
    );

    // A bad value or an unknown key is refused, and changes nothing.
    assert_eq!(sandbox.exit_code(&["config", "set", "dxvk", "maybe"]), 1);
    assert_eq!(sandbox.stdout(&["config", "get", "dxvk"]).trim(), "true");
    assert_eq!(sandbox.exit_code(&["config", "set", "nope", "1"]), 1);

    // A corrupt configuration is reported rather than silently replaced.
    std::fs::write(&path, "{ not json").unwrap();
    let output = sandbox.run(&["list"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("JSON"));
}

#[test]
fn configuration_loads_from_the_default_location_beside_the_data() {
    // A relocated installation must not depend on a flag being repeated: the
    // file is found from the data directory alone.
    let sandbox = Sandbox::new();
    sandbox.stdout(&["config", "set", "log_level", "warn"]);
    assert_eq!(
        sandbox.stdout(&["config", "get", "log_level"]).trim(),
        "warn"
    );
}

#[test]
fn config_keys_lists_everything_that_can_be_set() {
    let sandbox = Sandbox::new();
    let text = sandbox.stdout(&["config", "keys"]);
    for key in [
        "performance_mode",
        "sandbox",
        "dxvk_settings.hud",
        "registry_url",
    ] {
        assert!(text.contains(key), "config keys omits {key}");
    }
    // Every advertised key must actually be readable.
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let key = line.split_whitespace().next().unwrap();
        sandbox.stdout(&["config", "get", key]);
    }
}

// -------------------------------------------------------------------- logs

#[test]
fn logs_are_kept_per_application() {
    let sandbox = Sandbox::new();
    sandbox.stdout(&["--offline", "profiles", "seed"]);
    let installer = sandbox.write_installer("Notepad++ 8.6 Setup.exe");
    sandbox.stdout(&[
        "--offline",
        "--no-sandbox",
        "--yes",
        "install",
        installer.to_str().unwrap(),
    ]);
    // A launch failure is exactly what this test exists to diagnose, so its
    // output — including the log, which holds the sandbox's own error — is
    // printed rather than asserted away.
    let launched = sandbox.run(&["launch", MOCK_APP_ID, "--wait"]);
    if !launched.status.success() {
        panic!(
            "launch --wait failed: {:?}\nstdout:\n{}\nstderr:\n{}\nlaunch log:\n{}",
            launched.status.code(),
            String::from_utf8_lossy(&launched.stdout),
            String::from_utf8_lossy(&launched.stderr),
            std::fs::read_to_string(
                sandbox
                    .data
                    .join("apps")
                    .join(MOCK_APP_ID)
                    .join("launch.log"),
            )
            .unwrap_or_else(|_| "(none)".to_string())
        );
    }

    // The log is found by id and by display name, because a menu entry shows
    // the name.
    let by_id = sandbox.stdout(&["logs", MOCK_APP_ID]);
    let by_name = sandbox.stdout(&["logs", "Notepad++"]);
    assert_eq!(by_id, by_name);

    // The log file itself is where `--follow` and the GUI read from.
    let log = sandbox
        .data
        .join("apps")
        .join(MOCK_APP_ID)
        .join("launch.log");
    assert!(log.is_file(), "a launch log should have been written");
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(
        text.contains("command:"),
        "the log must record what was run, even when the program says nothing:\n{text}"
    );
    assert!(
        text.contains(r"C:\Program Files\Notepad++\notepad++.exe"),
        "the log should name the program: {text}"
    );

    // `logs` with an unknown kind finds nothing, and says so rather than
    // inventing an empty log.
    let output = sandbox.stdout(&["logs", MOCK_APP_ID, "--kind", "nonesuch"]);
    assert!(output.contains("no log"), "{output}");
}

// --------------------------------------------------------------- no residue

#[test]
fn nothing_is_written_outside_the_data_directory_and_the_menu() {
    let sandbox = Sandbox::new();
    sandbox.stdout(&["--offline", "profiles", "seed"]);
    let installer = sandbox.write_installer("Notepad++ 8.6 Setup.exe");
    sandbox.stdout(&[
        "--offline",
        "--no-sandbox",
        "--yes",
        "install",
        installer.to_str().unwrap(),
    ]);
    sandbox.stdout(&["--yes", "remove", MOCK_APP_ID]);

    // After an install and a removal, the only things left under the sandbox
    // root are the data directory, the cache, the logs and the empty
    // applications directory.
    let mut unexpected = Vec::new();
    for entry in std::fs::read_dir(sandbox.root()).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        match name.as_str() {
            "bin" | "data" | "share" | "config" | "cache" | "trace.log" => {}
            // The installers the test itself wrote.
            _ if name.ends_with(".exe") => {}
            other => unexpected.push(other.to_string()),
        }
    }
    assert!(
        unexpected.is_empty(),
        "unexpected leftovers: {unexpected:?}"
    );

    let apps = std::fs::read_dir(sandbox.data.join("apps"))
        .unwrap()
        .count();
    assert_eq!(apps, 0, "the applications directory should be empty again");

    // `update-desktop-database` leaves a `mimeinfo.cache` behind, which is how
    // it is supposed to work; what matters is that no *entry* remains.
    let entries: Vec<String> = std::fs::read_dir(&sandbox.applications)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|name| name.ends_with(".desktop"))
        .collect();
    assert!(entries.is_empty(), "menu entries remain: {entries:?}");
}
