//! End-to-end tests of the whole pipeline against a mock Wine installation.
//!
//! Wine is deliberately not a dependency of this project's test-suite: it is a
//! large, environment-sensitive component, and CI for a tool that manages Wine
//! should not require Wine to be installed. Instead these tests stand in a
//! small shell script for `wine`, `winetricks`, `wrestool` and `icotool`, and
//! drive the real [`ApplicationManager`] through the real code paths.
//!
//! A mock Wine is a fair stand-in for the parts being verified, because the
//! behaviours under test are WinDrop's own:
//!
//! * the order in which a prefix is created and populated;
//! * which environment variables reach the child process;
//! * which program inside the prefix is chosen for the menu;
//! * what gets written where, and what gets deleted on removal;
//! * whether a failure on one variant falls through to the next.
//!
//! Where Wine's own behaviour matters — actually running Windows code — the
//! tests instead assert on the [`CommandSpec`] that would be executed.

// Setting one field on a default harness is the clearest way to say "the usual
// setup, except this"; the lint is aimed at production code, where it usually
// means a missing derive.
#![allow(clippy::field_reassign_with_default)]

use std::path::{Path, PathBuf};

use windrop_core::compat::profile::{
    AppProfile, DependencySpec, Requirements, RuntimeEnv, WindowsVersion,
};
use windrop_core::compat::Arch;
use windrop_core::config::{Config, SandboxMode};
use windrop_core::fixtures::{self, PeSpec};
use windrop_core::icons::IconExtractor;
use windrop_core::manager::metadata::InstalledApp;
use windrop_core::manager::{ApplicationManager, InstallOptions};
use windrop_core::paths::Paths;
use windrop_core::runtime::{RuntimeManager, WineInstall, WineSource};
use windrop_core::Error;

/// A field from a rendered desktop entry.
fn field(entry: &str, key: &str) -> String {
    windrop_core::desktop::parse_entry(entry)
        .get(key)
        .cloned()
        .unwrap_or_else(|| panic!("the entry has no {key} field"))
}

/// Split an `Exec` value the way a desktop environment does: whitespace
/// separates arguments, except inside double quotes.
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

/// A Wine stand-in.
///
/// * `--version` reports a version, so probing works.
/// * `wineboot --init` fabricates a registry, which is what makes a prefix
///   count as initialised.
/// * Anything addressed through `Z:` is the installer: it "installs" by copying
///   the payload supplied in `$WINDROP_TEST_PAYLOAD`, plus an uninstaller that
///   must never be chosen as the main program.
/// * Anything else is an installed program: it records that it ran.
/// * If `$WINDROP_TEST_INSTALLER_FAILS` is set, the install does nothing and
///   exits non-zero, which is how a failing variant is simulated.
const MOCK_WINE: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  printf '%s\n' "wine-9.0 (WinDrop mock build)"
  exit 0
fi
if [ "$1" = "wineboot" ]; then
  mkdir -p "$WINEPREFIX/drive_c/users/test"
  printf '%s\n' "WINE REGISTRY Version 2" > "$WINEPREFIX/system.reg"
  printf '%s\n' "wineboot" >> "$WINEPREFIX/../../trace.log"
  exit 0
fi
case "$1" in
  Z:*)
    # Every argument, not just the program: the silent flags a profile supplies
    # are the whole point of testing this path.
    # NB: `printf`, not `echo` — Ubuntu's dash interprets backslash escapes in
    # `echo`, turning `Z:\tmp\...` into `Z:<TAB>mp...` and breaking assertions.
    printf 'installer %s\n' "$*" >> "$WINEPREFIX/../../trace.log"
    if [ -n "$WINDROP_TEST_INSTALLER_FAILS" ]; then
      printf '%s\n' "simulated installer failure" >&2
      exit 1
    fi
    mkdir -p "$WINEPREFIX/drive_c/Program Files/TestApp"
    mkdir -p "$WINEPREFIX/drive_c/Program Files/TestApp/resources"
    cat "$WINDROP_TEST_PAYLOAD" > "$WINEPREFIX/drive_c/Program Files/TestApp/testapp.exe"
    cat "$WINDROP_TEST_PAYLOAD" > "$WINEPREFIX/drive_c/Program Files/TestApp/unins000.exe"
    printf '%s\n' "installed" > "$WINEPREFIX/drive_c/Program Files/TestApp/readme.txt"
    exit 0
    ;;
  *)
    # Written two levels up, i.e. into the applications directory: one level up
    # is the application's own directory, which removal deletes.
    printf 'launched %s\n' "$1" >> "$WINEPREFIX/../../launch.log"
    printf 'launch %s\n' "$1" >> "$WINEPREFIX/../../trace.log"
    exit 0
    ;;
esac
"#;

/// A `winetricks` stand-in that records the verbs it is asked for.
const MOCK_WINETRICKS: &str = r#"#!/bin/sh
for arg in "$@"; do
  case "$arg" in -q) continue ;; esac
  printf '%s\n' "$arg" >> "$WINEPREFIX/winetricks.log"
  printf 'winetricks:%s\n' "$arg" >> "$WINEPREFIX/../../trace.log"
done
exit 0
"#;

/// A `wrestool` stand-in that produces an icon resource file.
const MOCK_WRESTOOL: &str = r#"#!/bin/sh
out="."
while [ $# -gt 0 ]; do
  case "$1" in -o) shift; out="$1" ;; esac
  shift
done
mkdir -p "$out"
printf 'icon' > "$out/app.exe_14_1.ico"
exit 0
"#;

/// An `icotool` stand-in that produces two icon sizes.
const MOCK_ICOTOOL: &str = r#"#!/bin/sh
out="."
while [ $# -gt 0 ]; do
  case "$1" in -o) shift; out="$1" ;; esac
  shift
done
mkdir -p "$out"
printf 'small' > "$out/icon_1_16x16x32.png"
printf 'large!' > "$out/icon_2_64x64x32.png"
exit 0
"#;

fn write_executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Flush the writer's pages before the file is executed: on overlayfs the
    // close alone can leave a freshly written script briefly busy (ETXTBSY).
    if let Ok(file) = std::fs::File::open(path) {
        let _ = file.sync_all();
    }
}

/// A complete WinDrop installation backed by mock tools.
struct Harness {
    dir: tempfile::TempDir,
    paths: Paths,
    config: Config,
    wine: WineInstall,
    winetricks: PathBuf,
    payload: PathBuf,
    installer: PathBuf,
    installer_b: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        // An isolated layout: no test may write into the real menu or home.
        let paths = Paths::isolated(dir.path().join("data"));
        paths.ensure().unwrap();

        let wine_path = dir.path().join("tools/wine");
        write_executable(&wine_path, MOCK_WINE);
        // A wineserver stand-in so prefix shutdown is exercised.
        write_executable(&dir.path().join("tools/wineserver"), "#!/bin/sh\nexit 0\n");

        let winetricks = dir.path().join("tools/winetricks");
        write_executable(&winetricks, MOCK_WINETRICKS);

        write_executable(&dir.path().join("tools/wrestool"), MOCK_WRESTOOL);
        write_executable(&dir.path().join("tools/icotool"), MOCK_ICOTOOL);

        // The program the "installer" will drop into the prefix.
        let payload = dir.path().join("assets/testapp.exe");
        fixtures::write_exe(&payload, &PeSpec::example_gui_large()).unwrap();

        // The file the user "drops".
        let installer = dir.path().join("downloads/TestAppSetup.exe");
        fixtures::write_exe(&installer, &PeSpec::example_installer()).unwrap();

        // A second, genuinely different installer, for tests that need two
        // applications installed side by side.
        let installer_b = dir.path().join("downloads/SecondAppSetup.exe");
        fixtures::write_exe(&installer_b, &PeSpec::example_d3d12_game()).unwrap();

        let wine = WineInstall {
            executable: wine_path,
            version: "9.0".into(),
            flavour: "WinDrop mock build".into(),
            variant_label: "stable".into(),
            source: WineSource::System,
        };

        let mut config = Config::default();
        // Sandboxing is verified by unit tests on the generated arguments;
        // running bubblewrap here would only test bubblewrap.
        config.sandbox = SandboxMode::Off;
        config.allow_remote_registry = false;

        Harness {
            dir,
            paths,
            config,
            wine,
            winetricks,
            payload,
            installer,
            installer_b,
        }
    }

    fn manager(&self) -> ApplicationManager {
        let runtime = RuntimeManager::new(self.paths.clone(), self.config.clone())
            .with_wine(self.wine.clone())
            .with_winetricks(&self.winetricks);
        let icons = IconExtractor::with_tools(
            Some(self.dir.path().join("tools/wrestool")),
            Some(self.dir.path().join("tools/icotool")),
        );
        ApplicationManager::new(self.paths.clone(), self.config.clone())
            .unwrap()
            .with_runtime(runtime)
            .with_icons(icons)
    }

    fn trace(&self) -> String {
        std::fs::read_to_string(self.paths.apps_dir().join("trace.log")).unwrap_or_default()
    }

    /// Recorded by the mock when an installed program is actually started.
    /// It lives outside the application directory, so it survives removal.
    fn launch_marker(&self) -> String {
        std::fs::read_to_string(self.paths.apps_dir().join("launch.log")).unwrap_or_default()
    }

    /// Register a curated profile for the standard installer, so resolution is
    /// deterministic instead of depending on generated defaults.
    fn curate_profile(&self, manager: &ApplicationManager, variant_env: Vec<(String, String)>) {
        self.curate_profile_with(manager, variant_env, Vec::new())
    }

    /// The same, with the profile's own silent flags.
    fn curate_profile_with(
        &self,
        manager: &ApplicationManager,
        variant_env: Vec<(String, String)>,
        installer_args: Vec<String>,
    ) {
        self.curate_profile_for(
            manager,
            &self.installer,
            "testapp-32",
            "Test App",
            Arch::X86,
            variant_env,
            installer_args,
        )
    }

    /// Register a curated profile for an arbitrary installer file.
    ///
    /// A curated profile carries the application's real name, so it — not the
    /// `TestAppSetup.exe` it arrived in — is what the menu entry should say.
    // A recipe really does have this many independent knobs, and naming them at
    // each call site is clearer than a builder nobody else uses.
    #[allow(clippy::too_many_arguments)]
    fn curate_profile_for(
        &self,
        manager: &ApplicationManager,
        installer: &Path,
        profile_id: &str,
        name: &str,
        arch: Arch,
        variant_env: Vec<(String, String)>,
        installer_args: Vec<String>,
    ) {
        let inspection = windrop_core::compat::pe::inspect(installer).unwrap();
        let mut env = vec![(
            "WINDROP_TEST_PAYLOAD".to_string(),
            self.payload.to_string_lossy().to_string(),
        )];
        env.extend(variant_env);

        let profile = AppProfile {
            id: profile_id.to_string(),
            name: name.to_string(),
            version: "1.0".into(),
            hashes: vec![inspection.sha256.clone()],
            arch: Some(arch),
            requirements: Requirements::default(),
            variants: vec![RuntimeEnv {
                wine_build: "stable".into(),
                arch,
                windows_version: WindowsVersion::Win10,
                dxvk: false,
                vkd3d_proton: false,
                dll_overrides: Vec::new(),
                env,
                dependencies: vec![DependencySpec::new("vcrun2022", "the installer needs it")],
                rationale: "curated profile for the test application".into(),
            }],
            source: windrop_core::compat::profile::ProfileSource::Local,
            installer_args,
            main_exe_hint: None,
            notes: String::new(),
            updated_at: None,
        };
        manager.db().upsert_profile(&profile).unwrap();
    }
}

#[test]
fn a_complete_install_launch_and_remove_cycle() {
    let harness = Harness::new();
    let manager = harness.manager();
    harness.curate_profile(&manager, Vec::new());

    // ---------------------------------------------------------------- install
    let report = manager.install(&harness.installer).unwrap();

    // The curated profile supplied the name; the id is its slug.
    assert_eq!(report.app.id, "test-app");
    assert_eq!(report.app.name, "Test App");
    assert_eq!(report.app.profile_id, "testapp-32");
    assert_eq!(
        report.app.attempts, 0,
        "the first variant should have worked"
    );
    assert_eq!(report.profile.variants.len(), 1);

    // The program chosen for the menu is the application, not the uninstaller.
    assert_eq!(
        report.app.main_exe_windows,
        r"C:\Program Files\TestApp\testapp.exe"
    );
    assert!(report.app.main_exe_host.is_file());
    assert!(report.app.is_runnable());

    // ------------------------------------------------------ how it got there
    let trace = harness.trace();
    assert!(
        trace.contains("wineboot"),
        "the prefix must be initialised\n{trace}"
    );
    assert!(
        trace.contains("winetricks:win10"),
        "the Windows version must be pinned\n{trace}"
    );
    assert!(
        trace.contains("winetricks:vcrun2022"),
        "dependencies must be installed\n{trace}"
    );
    assert_eq!(report.prefix.dependencies.len(), 1);
    assert!(report.prefix.dependencies[0].success);
    assert!(
        trace.contains("installer Z:"),
        "the installer must run through wine\n{trace}"
    );

    let order = |needle: &str| trace.find(needle).unwrap_or(usize::MAX);
    assert!(order("wineboot") < order("winetricks:win10"));
    assert!(order("winetricks:win10") < order("installer Z:"));

    // ------------------------------------------------------- what was written
    // (helpers for reading the entry back, defined once and used below)
    let app_dir = harness.paths.app_dir("test-app");
    assert!(app_dir.join("metadata.json").is_file());
    assert!(app_dir.join("profile.json").is_file());
    assert!(app_dir.join("icon.png").is_file());
    assert_eq!(std::fs::read(app_dir.join("icon.png")).unwrap(), b"large!");

    let metadata = InstalledApp::load(&harness.paths, "test-app").unwrap();
    assert_eq!(metadata, report.app);
    assert_eq!(metadata.dependencies, vec!["vcrun2022".to_string()]);
    assert_eq!(
        metadata.sha256,
        windrop_core::compat::pe::inspect(&harness.installer)
            .unwrap()
            .sha256
    );

    // ------------------------------------------------------------ menu entry
    let desktop_path = harness.paths.desktop_file_for("test-app");
    assert!(desktop_path.is_file());
    let desktop = std::fs::read_to_string(&desktop_path).unwrap();
    assert!(desktop.contains("Name=Test App"));
    assert!(desktop.contains("X-WinDrop-AppId=test-app"));
    assert!(
        !desktop.contains("WINEPREFIX"),
        "Wine details must not leak into the menu entry"
    );

    // A launcher must be able to act on this line, so check it the way a
    // desktop environment would: split it into arguments and confirm it names
    // the subcommand, the application, and — because this harness keeps its
    // data in a temporary directory rather than the default one — that
    // directory too.
    let exec = field(&desktop, "Exec");
    let argv = split_exec(&exec);
    assert_eq!(argv.first().map(String::as_str), Some("windrop"));
    assert_eq!(
        &argv[argv.len() - 2..],
        ["launch".to_string(), "test-app".to_string()],
        "the entry must call back with a subcommand and an id: {exec}"
    );
    assert_eq!(
        argv.windows(2)
            .find(|pair| pair[0] == "--data-dir")
            .map(|pair| pair[1].clone()),
        Some(harness.paths.data_dir().to_string_lossy().to_string()),
        "a relocated data directory has to be named, or the entry cannot find the app"
    );

    // The listing sees it.
    let apps = manager.list_apps().unwrap();
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0].id, "test-app");

    // --------------------------------------------------------------- learning
    let learned = manager.db().preferred_variant("test-app").unwrap();
    assert_eq!(
        learned,
        Some(report.app.variant.signature()),
        "the working variant must be remembered"
    );

    // ---------------------------------------------------------------- launch
    let plan = manager.launch_plan("test-app").unwrap();
    assert_eq!(plan.spec.program, harness.wine.executable);
    assert_eq!(
        plan.spec.args[0].to_string_lossy(),
        r"C:\Program Files\TestApp\testapp.exe"
    );
    assert!(plan
        .env_of("WINEPREFIX")
        .unwrap()
        .ends_with("apps/test-app/prefix"));
    assert!(plan
        .env_of("HOME")
        .unwrap()
        .ends_with("apps/test-app/prefix/home"));
    assert_eq!(plan.env_of("WINEARCH").unwrap(), "win32");
    assert!(!plan.sandboxed, "this harness runs with sandboxing off");

    manager.launch("test-app").unwrap();
    // The mock records that it was handed the installed program...
    let marker = harness.launch_marker();
    assert!(
        marker.contains(r"launched C:\Program Files\TestApp\testapp.exe"),
        "the application must actually have been started: {marker}"
    );
    // ...and WinDrop must have captured the run's own output next to it.
    assert!(
        app_dir.join("launch.log").is_file(),
        "the launch log must be kept with the application"
    );

    // ---------------------------------------------------------------- remove
    let removal = manager.remove("test-app").unwrap();
    assert_eq!(removal.app_id, "test-app");
    assert!(removal.removed_desktop_entry);
    assert!(removal.freed_bytes > 0);

    assert!(!app_dir.exists(), "the application directory must be gone");
    assert!(!desktop_path.exists(), "the menu entry must be gone");
    assert!(manager.list_apps().unwrap().is_empty());
    manager
        .verify_no_residue("test-app")
        .expect("removal must leave nothing behind");

    // The user's own download is untouched: WinDrop never deletes input.
    assert!(harness.installer.is_file());
    assert!(harness.payload.is_file());
}

#[test]
fn a_failing_variant_falls_through_to_the_next_one() {
    let harness = Harness::new();
    let manager = harness.manager();
    harness.curate_profile(&manager, Vec::new());

    // Replace the curated profile with a two-step chain whose first variant
    // makes the installer fail.
    let inspection = windrop_core::compat::pe::inspect(&harness.installer).unwrap();
    let base = RuntimeEnv {
        wine_build: "stable".into(),
        arch: Arch::X86,
        windows_version: WindowsVersion::Win10,
        dxvk: false,
        vkd3d_proton: false,
        dll_overrides: Vec::new(),
        env: vec![(
            "WINDROP_TEST_PAYLOAD".to_string(),
            harness.payload.to_string_lossy().to_string(),
        )],
        dependencies: Vec::new(),
        rationale: String::new(),
    };
    let doomed = RuntimeEnv {
        env: {
            let mut env = base.env.clone();
            env.push(("WINDROP_TEST_INSTALLER_FAILS".to_string(), "1".to_string()));
            env
        },
        rationale: "a doomed attempt".into(),
        ..base.clone()
    };
    let working = RuntimeEnv {
        rationale: "the working attempt".into(),
        ..base.clone()
    };

    let mut profile = manager
        .db()
        .find_profile_by_hash(&inspection.sha256)
        .unwrap()
        .unwrap();
    profile.id = "testapp-32-chain".into();
    profile.variants = vec![doomed.clone(), working.clone()];
    manager.db().upsert_profile(&profile).unwrap();

    let report = manager.install(&harness.installer).unwrap();

    assert_eq!(
        report.app.attempts, 1,
        "one variant should have failed first"
    );
    assert_eq!(report.app.variant.rationale, "the working attempt");
    assert_eq!(report.attempts.len(), 2);
    assert!(report.attempts[0].contains("a doomed attempt"));
    assert!(report.attempts[0].contains("did not install correctly"));
    assert!(report.attempts[1].contains("the working attempt"));
    assert!(report.app.is_runnable());
}

#[test]
fn every_variant_failing_reports_the_last_error_and_installs_nothing() {
    let harness = Harness::new();
    let manager = harness.manager();
    harness.curate_profile(&manager, Vec::new());

    let inspection = windrop_core::compat::pe::inspect(&harness.installer).unwrap();
    let mut profile = manager
        .db()
        .find_profile_by_hash(&inspection.sha256)
        .unwrap()
        .unwrap();
    let mut first = profile.variants[0].clone();
    first
        .env
        .push(("WINDROP_TEST_INSTALLER_FAILS".to_string(), "1".to_string()));
    first.rationale = "first doomed".into();
    let mut second = first.clone();
    second.rationale = "second doomed".into();
    profile.variants = vec![first, second];
    manager.db().upsert_profile(&profile).unwrap();

    let err = manager.install(&harness.installer).unwrap_err();
    match err {
        Error::AllVariantsFailed { app, last_error } => {
            assert_eq!(app, "Test App");
            assert!(
                last_error.contains("could not tell which program"),
                "{last_error}"
            );
        }
        other => panic!("expected AllVariantsFailed, got {other:?}"),
    }

    // A failed install must not leave a half-installed application behind.
    assert!(!harness.paths.desktop_file_for("test-app").exists());
    assert!(manager.list_apps().unwrap().is_empty());
}

#[test]
fn a_dry_run_reports_the_plan_without_touching_anything() {
    let harness = Harness::new();
    let manager = harness.manager();
    harness.curate_profile(&manager, Vec::new());

    let options = InstallOptions {
        dry_run: true,
        ..Default::default()
    };
    let report = manager
        .install_with(&harness.installer, &options, |_, _| Ok(()))
        .unwrap();

    assert_eq!(report.app.id, "test-app");
    assert!(!harness.paths.app_dir("test-app").exists());
    assert!(!harness.paths.desktop_file_for("test-app").exists());
    assert!(harness.trace().is_empty(), "no process may be started");

    // Nothing was learned either, because nothing was proven to work.
    assert_eq!(manager.db().preferred_variant("test-app").unwrap(), None);
}

#[test]
fn installing_the_same_application_twice_is_refused() {
    let harness = Harness::new();
    let manager = harness.manager();
    harness.curate_profile(&manager, Vec::new());

    manager.install(&harness.installer).unwrap();
    match manager.install(&harness.installer) {
        Err(Error::AppAlreadyInstalled(id)) => assert_eq!(id, "test-app"),
        other => panic!("expected AppAlreadyInstalled, got {other:?}"),
    }
    // The first installation is untouched.
    assert!(manager.get_app("test-app").unwrap().is_runnable());
}

#[test]
fn removing_one_application_leaves_another_intact() {
    let harness = Harness::new();
    let manager = harness.manager();
    harness.curate_profile(&manager, Vec::new());
    manager.install(&harness.installer).unwrap();

    // Install a second, genuinely different application.
    harness.curate_profile_for(
        &manager,
        &harness.installer_b,
        "secondapp-64",
        "Second App",
        Arch::X86_64,
        Vec::new(),
        Vec::new(),
    );
    let options = InstallOptions {
        app_id: Some("secondapp".into()),
        ..Default::default()
    };
    manager
        .install_with(&harness.installer_b, &options, |plan, _| {
            plan.run_logged(
                &harness.paths.logs_dir().join("second.log"),
                std::time::Duration::from_secs(30),
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(manager.list_apps().unwrap().len(), 2);

    manager.remove("test-app").unwrap();

    let remaining = manager.list_apps().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, "secondapp");
    assert!(remaining[0].is_runnable());
    assert!(harness.paths.desktop_file_for("secondapp").is_file());
    assert!(!harness.paths.desktop_file_for("test-app").exists());
}

#[test]
fn a_curated_profile_survives_removal_so_reinstalling_is_instant() {
    let harness = Harness::new();
    let manager = harness.manager();
    harness.curate_profile(&manager, Vec::new());

    let first = manager.install(&harness.installer).unwrap();
    let learned = manager.db().preferred_variant("test-app").unwrap();
    manager.remove("test-app").unwrap();

    // The profile is knowledge, not installation state, so it is kept.
    let profile = manager.db().get_profile("testapp-32").unwrap().unwrap();
    assert_eq!(profile.variants.len(), first.profile.variants.len());

    // Reinstalling reuses what was learned and starts with the known-good
    // variant rather than trying the whole chain again.
    let second = manager.install(&harness.installer).unwrap();
    assert_eq!(second.app.attempts, 0);
    assert_eq!(second.app.variant.signature(), learned.unwrap());
}

#[test]
fn an_executable_the_engine_has_never_seen_still_installs() {
    // No curated profile at all: this exercises inspection, dependency
    // inference and the generated chain end to end.
    let harness = Harness::new();
    let manager = harness.manager();

    // The generated profile cannot know about the mock payload, so add it to the
    // variants' own environment. This also proves that per-variant environment
    // variables reach the child process.
    let report = {
        let resolved = manager.resolve(&harness.installer).unwrap();
        assert_eq!(
            resolved.source,
            windrop_core::compat::profile::ProfileSource::Generated
        );
        assert!(
            resolved.profile.variants[0]
                .dependencies
                .iter()
                .any(|d| d.verb == "vcrun2022"),
            "VC++ runtime imports must be inferred"
        );

        let mut profile = resolved.profile.clone();
        for variant in &mut profile.variants {
            variant.env.push((
                "WINDROP_TEST_PAYLOAD".to_string(),
                harness.payload.to_string_lossy().to_string(),
            ));
        }
        manager.db().upsert_profile(&profile).unwrap();
        manager.install(&harness.installer).unwrap()
    };

    assert!(!report.app.variant.dependencies.is_empty());
    assert!(report.app.is_runnable());
    // The generated chain offers alternatives, which is the whole point.
    assert!(report.profile.variants.len() > 1);
}

#[test]
fn launch_plans_never_depend_on_a_sandbox_being_present() {
    let harness = Harness::new();
    let manager = harness.manager();
    harness.curate_profile(&manager, Vec::new());
    manager.install(&harness.installer).unwrap();

    // Strict mode, whatever the host has installed.
    let mut strict = harness.config.clone();
    strict.sandbox = SandboxMode::Strict;
    let runtime = RuntimeManager::new(harness.paths.clone(), strict.clone())
        .with_wine(harness.wine.clone())
        .with_winetricks(&harness.winetricks);
    let manager = ApplicationManager::new(harness.paths.clone(), strict)
        .unwrap()
        .with_runtime(runtime);

    let plan = manager.launch_plan("test-app").unwrap();
    assert!(
        plan.env_of("WINEPREFIX").is_some(),
        "the plan must be complete whether or not a sandbox is used"
    );
    if plan.sandboxed {
        assert_eq!(plan.spec.program, PathBuf::from("/usr/bin/bwrap"));
        assert!(plan.spec.display().contains("--die-with-parent"));
    } else {
        assert_eq!(plan.spec.program, harness.wine.executable);
    }
}

#[test]
fn an_application_whose_program_disappears_is_reported_clearly() {
    let harness = Harness::new();
    let manager = harness.manager();
    harness.curate_profile(&manager, Vec::new());
    let report = manager.install(&harness.installer).unwrap();

    std::fs::remove_file(&report.app.main_exe_host).unwrap();

    match manager.launch_plan("test-app") {
        Err(Error::AppNotFound(message)) => assert!(message.contains("missing"), "{message}"),
        other => panic!("expected a clear AppNotFound, got {other:?}"),
    }
}

#[test]
fn listing_is_stable_and_reports_what_is_on_disk() {
    let harness = Harness::new();
    let manager = harness.manager();
    harness.curate_profile(&manager, Vec::new());
    harness.curate_profile_for(
        &manager,
        &harness.installer_b,
        "secondapp-64",
        "Second App",
        Arch::X86_64,
        Vec::new(),
        Vec::new(),
    );
    assert!(manager.list_apps().unwrap().is_empty());

    manager.install(&harness.installer).unwrap();

    let options = InstallOptions {
        app_id: Some("aaa-first".into()),
        ..Default::default()
    };
    manager
        .install_with(&harness.installer_b, &options, |plan, _| {
            plan.run_logged(
                &harness.paths.logs_dir().join("b.log"),
                std::time::Duration::from_secs(30),
            )?;
            Ok(())
        })
        .unwrap();

    let apps = manager.list_apps().unwrap();
    assert_eq!(apps.len(), 2);
    // Sorted by display name, so the order is predictable for the UI.
    let names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, vec!["Second App", "Test App"]);

    for app in &apps {
        assert!(app.is_runnable());
        assert!(app.size_on_disk(&harness.paths) > 0);
        assert!(!app.strategy().is_empty());
    }
}

#[test]
fn residue_from_an_interrupted_install_does_not_break_listing() {
    let harness = Harness::new();
    let manager = harness.manager();
    harness.curate_profile(&manager, Vec::new());
    manager.install(&harness.installer).unwrap();

    // A directory that appeared but never got metadata, e.g. a crash mid-install.
    std::fs::create_dir_all(harness.paths.app_dir("half-installed")).unwrap();

    let apps = manager.list_apps().unwrap();
    assert_eq!(apps.len(), 1, "the incomplete entry must not appear");
    // It is not treated as an application, so it cannot be launched...
    assert!(matches!(
        manager.launch_plan("half-installed"),
        Err(Error::AppNotFound(_))
    ));
    // ...nor silently deleted as if it were one.
    assert!(matches!(
        manager.remove("half-installed"),
        Err(Error::AppNotFound(_))
    ));
    assert!(harness.paths.app_dir("half-installed").exists());
}

/// The installer command the mock recorded, i.e. the wine line in the trace.
fn installer_line(trace: &str) -> &str {
    trace
        .lines()
        .rfind(|line| line.starts_with("installer "))
        .unwrap_or("")
}

#[test]
fn a_profiles_silent_flags_are_passed_to_the_installer() {
    let harness = Harness::new();
    let manager = harness.manager();
    harness.curate_profile_with(&manager, Vec::new(), vec!["/S".into(), "/NORESTART".into()]);

    // `install` is the unattended path, which is when silent flags are wanted.
    let report = manager.install(&harness.installer).unwrap();
    let line = installer_line(&harness.trace()).to_string();

    assert!(
        line.contains("/S"),
        "the silent flag must be passed: {line}"
    );
    assert!(
        line.contains("/NORESTART"),
        "both flags must be passed: {line}"
    );
    // In the order the profile gave them, and after the program itself: an
    // installer handed its flags before the file name reads them as a program.
    let program = line.find("Z:").expect("the installer path must be named");
    assert!(program < line.find("/S").unwrap(), "wrong order: {line}");
    assert_eq!(report.app.attempts, 0);
}

#[test]
fn an_interactive_install_does_not_pass_silent_flags() {
    let harness = Harness::new();
    let manager = harness.manager();
    harness.curate_profile_with(&manager, Vec::new(), vec!["/S".into(), "/NORESTART".into()]);

    // Installing with the installer's window shown is the default because most
    // installers need at least one answer; passing the silent flags anyway would
    // skip exactly the questions the user asked to see.
    let options = InstallOptions::default();
    manager
        .install_interactively(&harness.installer, &options)
        .unwrap();

    let line = installer_line(&harness.trace()).to_string();
    assert!(!line.is_empty(), "the installer must still have run");
    assert!(
        !line.contains("/S"),
        "silent flags must not be passed: {line}"
    );
}

#[test]
fn a_profile_without_silent_flags_installs_in_any_mode() {
    let harness = Harness::new();
    let manager = harness.manager();
    harness.curate_profile(&manager, Vec::new());

    // No flags to pass is the common case, and it must not add empty arguments
    // to the command line.
    manager.install(&harness.installer).unwrap();
    let line = installer_line(&harness.trace()).to_string();
    assert!(line.contains("Z:"), "{line}");
    assert_eq!(
        line.split_whitespace().count(),
        2,
        "only the program: {line}"
    );
}
