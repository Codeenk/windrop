//! The command implementations.
//!
//! Each command returns a process exit code rather than a `Result`, because
//! some of them have something useful to say whether or not they succeeded:
//! `windrop doctor` prints a full report *and* exits non-zero to tell a script
//! that this machine is not ready. The codes are documented in `--help` and in
//! the README:
//!
//! | Code | Meaning                                            |
//! |------|----------------------------------------------------|
//! | 0    | success                                            |
//! | 1    | the operation failed                              |
//! | 2    | the command line was wrong (from `clap`)          |
//! | 3    | a required component is missing from this machine |
//! | 4    | no such application or profile                     |
//! | 5    | the application is already installed               |

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use windrop_core::compat::engine::ResolvedProfile;
use windrop_core::compat::pe::PeInspection;
use windrop_core::compat::profile::{slugify, AppProfile};
use windrop_core::compat::seed;
use windrop_core::config::{Config, SandboxMode};
use windrop_core::desktop;
use windrop_core::doctor::{Diagnostics, Necessity, ToolStatus};
use windrop_core::logging::{self, LogGuard};
use windrop_core::manager::metadata::InstalledApp;
use windrop_core::manager::{human_bytes, ApplicationManager, InstallOptions, InstallReport};
use windrop_core::paths::Paths;
use windrop_core::registry::{RegistryClient, RegistryIndex};
use windrop_core::runtime::dxvk::ComponentKind;
use windrop_core::runtime::sandbox::SandboxAvailability;
use windrop_core::runtime::{RuntimeManager, WineInstall, WineSource};
use windrop_core::text;
use windrop_core::updater;
use windrop_core::Error;

use crate::cli::{
    Cli, Command, ConfigCommand, DoctorArgs, GuiArgs, InspectArgs, InstallArgs, LaunchArgs,
    ListArgs, LogsArgs, ProfilesCommand, RemoveArgs, UpdateArgs,
};
use crate::settings;
use crate::ui::Ui;

/// Exit code for a failure whose cause is not on this machine.
const EXIT_FAILURE: i32 = 1;
/// Exit code for a missing runtime dependency.
const EXIT_NOT_READY: i32 = 3;
/// Exit code for an unknown application or profile.
const EXIT_NOT_FOUND: i32 = 4;
/// Exit code for an application that is already installed.
const EXIT_ALREADY_INSTALLED: i32 = 5;

/// Everything a command needs, resolved once.
pub struct Context {
    pub ui: Ui,
    pub paths: Paths,
    pub config: Config,
    pub manager: ApplicationManager,
    /// The file the configuration was read from, and would be written back to.
    pub config_path: PathBuf,
    /// `--yes`: answer confirmations in advance.
    pub assume_yes: bool,
    /// Kept alive so the log file keeps being written for the whole command.
    _log: Option<LogGuard>,
}

impl Context {
    /// Resolve the data directory, configuration and application manager.
    ///
    /// Precedence, highest first: `--data-dir`, the `data_dir` setting, the
    /// `WINDROP_DATA_DIR` environment variable, then the XDG default. The
    /// command line wins because a user typing a flag means it.
    pub fn build(global: &crate::cli::Global) -> windrop_core::Result<Self> {
        let cli_data_dir = global.data_dir.clone();
        let provisional = match &cli_data_dir {
            Some(dir) => Paths::with_data_dir(dir),
            None => Paths::discover()?,
        };
        let config_path = global
            .config
            .clone()
            .unwrap_or_else(|| provisional.config_file());
        let mut config = Config::load(&config_path)?;

        let paths = match &cli_data_dir {
            Some(dir) => Paths::with_data_dir(dir),
            None => config.resolve_paths()?,
        };
        paths.ensure()?;

        // Command-line overrides are applied in memory and never written back:
        // `--offline` for one command must not silently change the user's
        // configuration for the next.
        if global.offline {
            config.allow_remote_registry = false;
            config.auto_profile_updates = false;
        }
        if global.no_sandbox {
            config.sandbox = SandboxMode::Off;
        }

        let ui = Ui::new(global.color, global.quiet, global.verbose, global.json);
        // The log file keeps the configured detail; the terminal stays quiet
        // unless asked, so `windrop` remains usable in a pipeline. `-v` raises
        // both, because a user asking for detail wants to see it.
        let (file_level, terminal_level) = if global.quiet {
            ("error".to_string(), "error".to_string())
        } else {
            match global.verbose {
                0 => (config.log_level.clone(), "warn".to_string()),
                1 => ("debug".to_string(), "debug".to_string()),
                _ => ("trace".to_string(), "trace".to_string()),
            }
        };
        let log = logging::init_split(&file_level, &terminal_level, &paths.logs_dir())?;

        let mut runtime = RuntimeManager::new(paths.clone(), config.clone());
        if let Some(path) = &global.wine {
            // Probing now means a bad `--wine` fails immediately, rather than
            // several minutes into a prefix build.
            let wine = WineInstall::probe(path, config.wine_variant.label(), WineSource::System)?;
            runtime = runtime.with_wine(wine);
        }

        let manager = ApplicationManager::new(paths.clone(), config.clone())?.with_runtime(runtime);
        Ok(Context {
            ui,
            paths,
            config,
            manager,
            config_path,
            assume_yes: global.yes,
            _log: log,
        })
    }
}

/// Run the command the user asked for.
pub fn run(cli: Cli) -> i32 {
    let context = match Context::build(&cli.global) {
        Ok(context) => context,
        Err(error) => {
            let ui = Ui::new(
                cli.global.color,
                cli.global.quiet,
                cli.global.verbose,
                cli.global.json,
            );
            return report(&ui, &error);
        }
    };

    match dispatch(&context, cli.command) {
        Ok(code) => code,
        Err(error) => report(&context.ui, &error),
    }
}

/// Print an error the way the rest of the tool prints things, and choose a code.
fn report(ui: &Ui, error: &Error) -> i32 {
    ui.error(&error.to_string());
    if let Some(hint) = error.hint() {
        ui.hint(&hint);
    }
    match error {
        Error::AppNotFound(_) | Error::ProfileNotFound(_) | Error::InputMissing { .. } => {
            EXIT_NOT_FOUND
        }
        Error::AppAlreadyInstalled(_) => EXIT_ALREADY_INSTALLED,
        Error::WineMissing { .. }
        | Error::WinetricksMissing { .. }
        | Error::ToolMissing(_)
        | Error::NoWineVariant(_) => EXIT_NOT_READY,
        _ => EXIT_FAILURE,
    }
}

fn dispatch(context: &Context, command: Command) -> windrop_core::Result<i32> {
    match command {
        Command::Install(args) => cmd_install(context, args),
        Command::List(args) => cmd_list(context, args),
        Command::Launch(args) => cmd_launch(context, args),
        Command::Remove(args) => cmd_remove(context, args),
        Command::Inspect(args) => cmd_inspect(context, args),
        Command::Doctor(args) => cmd_doctor(context, args),
        Command::Profiles(args) => cmd_profiles(context, args.command),
        Command::Update(args) => cmd_update(context, args),
        Command::Config(args) => cmd_config(context, args.command),
        Command::Logs(args) => cmd_logs(context, args),
        Command::Gui(args) => cmd_gui(context, args),
        Command::Version => cmd_version(context),
    }
}

// ---------------------------------------------------------------- questions

/// Ask a yes/no question, or answer it from `--yes`.
///
/// With no terminal to read from, the answer is `false`: an unattended script
/// must not have a destructive operation happen because nobody was there to say
/// no. `--yes` is the way to say yes in advance.
fn confirm(ui: Ui, assume_yes: bool, question: &str) -> bool {
    if assume_yes {
        ui.status(&format!("{question} (assuming yes)"));
        return true;
    }
    if !std::io::stdin().is_terminal() {
        ui.error(&format!(
            "{question} — refusing, because nothing is attached to answer"
        ));
        ui.hint("pass --yes to answer in advance");
        return false;
    }
    eprint!("{question} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

// ------------------------------------------------------------------ install

fn cmd_install(context: &Context, args: InstallArgs) -> windrop_core::Result<i32> {
    if args.files.len() > 1 && (args.name.is_some() || args.id.is_some()) {
        return Err(Error::Config {
            field: "--name/--id".into(),
            reason: "these name a single application; install the files one at a time".into(),
        });
    }
    if args.files.is_empty() {
        return Err(Error::Config {
            field: "FILE".into(),
            reason: "give at least one installer or program to install".into(),
        });
    }

    let mut installed: Vec<InstallReport> = Vec::new();
    let mut failures = 0usize;
    let mut last_code = 0;
    let total = args.files.len();

    for (index, file) in args.files.iter().enumerate() {
        if total > 1 {
            context
                .ui
                .status(&format!("[{}/{}] {}", index + 1, total, file.display()));
        }
        match install_one(context, file, &args) {
            Ok(report) => installed.push(report),
            Err(error) => {
                last_code = report_error(&context.ui, &error);
                failures += 1;
            }
        }
    }

    if installed.len() > 1 && !context.ui.json {
        context.ui.heading("Installed");
        for report in &installed {
            context.ui.bullet(&format!(
                "{} ({}) — {}",
                report.app.name,
                report.app.id,
                report.app.strategy()
            ));
        }
    }

    if failures > 0 {
        context.ui.warn(&format!(
            "{failures} of {total} failed; the rest were installed"
        ));
        return Ok(if last_code == 0 {
            EXIT_FAILURE
        } else {
            last_code
        });
    }
    Ok(0)
}

/// Print an error and return its exit code, for a per-file failure that should
/// not abandon the remaining files.
fn report_error(ui: &Ui, error: &Error) -> i32 {
    report(ui, error)
}

fn install_one(
    context: &Context,
    file: &Path,
    args: &InstallArgs,
) -> windrop_core::Result<InstallReport> {
    let ui = context.ui;
    if !file.is_file() {
        return Err(Error::InputMissing {
            path: file.to_path_buf(),
        });
    }

    let mut options = InstallOptions {
        app_id: args.id.clone(),
        name_hint: args.name.clone(),
        dry_run: args.dry_run,
        single_variant: args.no_fallback,
        main_exe: args.main_exe.clone(),
        profile_id: args.profile.clone(),
        ..Default::default()
    };

    // Who clicks through the installer?
    //
    // Interactive is the better default when a human is present: most installers
    // ask at least one question, and answering it is quicker than guessing.
    // With no terminal — a script, or anything automated — a silent install is
    // the only option that can finish.
    let can_ask = std::io::stdin().is_terminal() && !ui.json;
    let interactive = if args.dry_run {
        false
    } else if args.interactive {
        true
    } else if args.silent {
        false
    } else {
        can_ask && !context.config.install_silently
    };
    options.unattended = !interactive;

    if !ui.json {
        if args.dry_run {
            ui.status(&format!("Planning an install of {}…", file.display()));
        } else if interactive {
            ui.status(&format!(
                "Installing {}. The installer's own window will appear.",
                file.display()
            ));
        } else {
            ui.status(&format!(
                "Installing {} silently. This can take a while.",
                file.display()
            ));
        }
    }

    let report = if interactive {
        context.manager.install_interactively(file, &options)?
    } else {
        let progress: windrop_core::manager::ProgressFn =
            Arc::new(move |stage| ui.status(&stage.label()));
        context
            .manager
            .install_with_progress(file, options, progress)?
    };

    print_install(context, &report, args.dry_run);
    Ok(report)
}

/// What the user is told after an install.
#[derive(Serialize)]
struct InstallOutput {
    dry_run: bool,
    app: InstalledApp,
    profile_id: String,
    profile_source: String,
    prefix: String,
    variants_tried: usize,
    duration_seconds: f64,
    command: String,
}

fn print_install(context: &Context, report: &InstallReport, dry_run: bool) {
    let ui = context.ui;
    let command = format!("windrop launch {}", report.app.id);
    let output = InstallOutput {
        dry_run,
        app: report.app.clone(),
        profile_id: report.profile.id.clone(),
        profile_source: report.profile.source.label().to_string(),
        prefix: report
            .app
            .prefix(&context.paths)
            .root()
            .display()
            .to_string(),
        variants_tried: report.attempts.len(),
        duration_seconds: report.duration.as_secs_f64(),
        command: command.clone(),
    };

    if ui.json {
        let _ = ui.json(&output);
        return;
    }

    if dry_run {
        ui.heading("Plan");
        ui.kv(
            "application",
            &format!("{} ({})", report.app.name, report.app.id),
        );
        ui.kv(
            "profile",
            &format!("{} — {}", report.profile.id, report.profile.source.label()),
        );
        ui.kv("environment", &report.app.strategy());
        ui.kv("architecture", &report.app.arch.to_string());
        if !report.app.dependencies.is_empty() {
            ui.kv("dependencies", &report.app.dependencies.join(", "));
        }
        ui.out("");
        ui.out("Nothing was changed. Drop --dry-run to install for real.");
        return;
    }

    ui.heading("Installed");
    ui.kv("application", &report.app.name);
    ui.kv("id", &report.app.id);
    ui.kv("environment", &report.app.strategy());
    ui.kv("prefix", &report.prefix.summary());
    if report.app.attempts > 0 {
        ui.kv(
            "attempts",
            &format!(
                "{}, {} failed",
                text::count("environment", report.attempts.len()),
                report.app.attempts
            ),
        );
    }
    if !report.app.dependencies.is_empty() {
        ui.kv("dependencies", &report.app.dependencies.join(", "));
    }
    if let Some(path) = &report.app.desktop_file {
        ui.kv("menu entry", &path.display().to_string());
    }
    if ui.verbose() {
        ui.kv("profile", &report.profile.id);
        ui.kv("program", &report.app.main_exe_windows);
        ui.kv("on disk", &report.app.main_exe_host.display().to_string());
        if let Some(plan) = &report.installer_plan {
            ui.out("");
            ui.out("The installer was run as:");
            ui.out(&format!("  {}", plan.spec.display()));
        }
    }
    ui.out("");
    ui.out(&format!("Start it with:  {command}"));
    ui.out(&format!("Remove it with: windrop remove {}", report.app.id));
}

// --------------------------------------------------------------------- list

#[derive(Serialize)]
struct ListOutput<'a> {
    data_dir: String,
    apps: &'a [InstalledApp],
}

fn cmd_list(context: &Context, args: ListArgs) -> windrop_core::Result<i32> {
    let ui = context.ui;
    let apps = context.manager.list_apps()?;

    if ui.json {
        let output = ListOutput {
            data_dir: context.paths.data_dir().display().to_string(),
            apps: &apps,
        };
        ui.json(&output)?;
        return Ok(0);
    }

    if args.short {
        for app in &apps {
            ui.out(&app.id);
        }
        return Ok(0);
    }

    if apps.is_empty() {
        ui.out("No applications are installed.");
        ui.out("");
        ui.out("Drop a .exe on the WinDrop window, or run:");
        ui.out("  windrop install ~/Downloads/something.exe");
        return Ok(0);
    }

    let id_width = apps.iter().map(|a| a.id.len()).max().unwrap_or(2).max(2);
    let name_width = apps
        .iter()
        .map(|a| a.name.chars().count())
        .max()
        .unwrap_or(4)
        .max(4);

    ui.out(&format!(
        "{:<id_width$}  {:<name_width$}  {:>8}  {}",
        "ID", "NAME", "ARCH", "STATUS"
    ));
    for app in &apps {
        let mut status = if app.is_runnable() {
            "ready".to_string()
        } else {
            "program missing — reinstall".to_string()
        };
        if args.sizes {
            status = format!(
                "{}  {:>9}",
                status,
                human_bytes(app.size_on_disk(&context.paths))
            );
        }
        ui.out(&format!(
            "{:<id_width$}  {:<name_width$}  {:>8}  {}",
            app.id, app.name, app.arch, status
        ));
    }

    let freed: u64 = apps.iter().map(|a| a.size_on_disk(&context.paths)).sum();
    ui.out("");
    ui.out(&format!(
        "{} application{} using {} under {}",
        apps.len(),
        if apps.len() == 1 { "" } else { "s" },
        human_bytes(freed),
        context.paths.data_dir().display()
    ));
    Ok(0)
}

// ------------------------------------------------------------------- launch

fn cmd_launch(context: &Context, args: LaunchArgs) -> windrop_core::Result<i32> {
    let ui = context.ui;
    let app = context.manager.get_app(&args.app)?;

    if args.stop {
        let prefix = app.prefix(&context.paths);
        context.manager.runtime().shutdown_prefix(&prefix)?;
        if !ui.json {
            ui.success(&format!("stopped everything running for {}", app.name));
        }
        return Ok(0);
    }

    if args.plan_only {
        let plan = context.manager.launch_plan(&args.app)?;
        if ui.json {
            ui.json(&serde_json::json!({
                "app": app.id,
                "name": app.name,
                "target": plan.target,
                "command": plan.spec.display(),
                "program": plan.spec.program,
                "prefix": plan.prefix.root(),
                "sandboxed": plan.sandboxed,
                "wine": plan.wine.display(),
                "rationale": plan.rationale,
            }))?;
        } else {
            ui.heading("Launch plan");
            ui.kv("application", &app.name);
            ui.kv("program", &plan.target);
            ui.kv("prefix", &plan.prefix.root().display().to_string());
            ui.kv("wine", &plan.wine.display());
            ui.kv("sandbox", if plan.sandboxed { "bubblewrap" } else { "off" });
            ui.out("");
            ui.out(&plan.spec.display());
        }
        return Ok(0);
    }

    if args.wait {
        ui.status(&format!("Running {}. Press Ctrl-C to stop.", app.name));
        let outcome = context.manager.launch(&args.app)?;
        let log = context.manager.log_file_for(&app.id, "launch");
        if ui.json {
            ui.json(&serde_json::json!({
                "app": app.id,
                "name": app.name,
                "exit_code": outcome.code,
                "output_bytes": outcome.output_bytes,
                "log": log,
            }))?;
        } else {
            if outcome.success() {
                ui.success(&format!("{} exited normally", app.name));
            } else {
                // The status is the program's own, so it is reported rather than
                // called a WinDrop failure — but the log is where any real
                // problem will be, so point at it.
                ui.warn(&format!("{} exited with status {}", app.name, outcome.code));
                if outcome.output_bytes == 0 {
                    ui.hint("it produced no output; if it never appeared, see the log below");
                }
            }
            ui.hint(&format!("output was captured in {}", log.display()));
        }
        // The program's exit code is the honest result of `--wait`.
        return Ok(outcome.code.clamp(0, 125));
    }

    // Detached, which is what a menu entry wants: the application must outlive
    // whoever started it.
    context.manager.launch_detached(&args.app)?;
    if !ui.json {
        ui.success(&format!("started {}", app.name));
    }
    Ok(0)
}

// ------------------------------------------------------------------- remove

#[derive(Serialize)]
struct RemovalOutput {
    app_id: String,
    name: String,
    freed_bytes: u64,
    removed_desktop_entry: bool,
    forgot_learning: bool,
}

fn cmd_remove(context: &Context, args: RemoveArgs) -> windrop_core::Result<i32> {
    let ui = context.ui;
    let targets: Vec<InstalledApp> = if args.all {
        context.manager.list_apps()?
    } else {
        let mut apps = Vec::new();
        for id in &args.apps {
            apps.push(context.manager.get_app(id)?);
        }
        apps
    };

    if targets.is_empty() {
        if !ui.json {
            ui.out("Nothing is installed, so there is nothing to remove.");
        } else {
            ui.json(&Vec::<RemovalOutput>::new())?;
        }
        return Ok(0);
    }

    let names: Vec<&str> = targets.iter().map(|a| a.name.as_str()).collect();
    let freed: u64 = targets.iter().map(|a| a.size_on_disk(&context.paths)).sum();
    let question = if targets.len() == 1 {
        format!(
            "Remove {} and everything it owns ({} on disk)?",
            names[0],
            human_bytes(freed)
        )
    } else {
        format!(
            "Remove {} applications ({}) and everything they own?",
            targets.len(),
            names.join(", ")
        )
    };

    if !confirm(ui, context.assume_yes, &question) {
        return Ok(EXIT_FAILURE);
    }

    let mut results = Vec::new();
    for app in &targets {
        let report = context.manager.remove(&app.id)?;
        let forgot = if args.forget_learning {
            context.manager.forget_learning(&app.id).unwrap_or(false)
        } else {
            false
        };
        // The point of a single-click uninstall is that nothing is left behind,
        // so verify it rather than trusting the delete.
        context.manager.verify_no_residue(&app.id)?;

        if !ui.json {
            ui.success(&format!(
                "removed {} ({} freed{})",
                report.name,
                human_bytes(report.freed_bytes),
                if report.removed_desktop_entry {
                    ", menu entry deleted"
                } else {
                    ""
                }
            ));
        }
        results.push(RemovalOutput {
            app_id: report.app_id,
            name: report.name,
            freed_bytes: report.freed_bytes,
            removed_desktop_entry: report.removed_desktop_entry,
            forgot_learning: forgot,
        });
    }

    // The menu database can go stale when an entry disappears; a desktop
    // environment rebuilds it at login anyway, but doing it now avoids a
    // ghost entry until then.
    let _ = desktop::refresh_database(context.paths.applications_dir());

    if ui.json {
        ui.json(&results)?;
    }
    Ok(0)
}

// ------------------------------------------------------------------ inspect

#[derive(Serialize)]
struct InspectOutput {
    path: String,
    inspection: PeInspection,
    profile: Option<String>,
    profile_source: Option<String>,
    environments: Vec<InspectEnvironment>,
}

#[derive(Serialize)]
struct InspectEnvironment {
    index: usize,
    rationale: String,
    wine_build: String,
    arch: String,
    windows_version: String,
    dxvk: bool,
    vkd3d_proton: bool,
    dependencies: Vec<String>,
}

fn cmd_inspect(context: &Context, args: InspectArgs) -> windrop_core::Result<i32> {
    let ui = context.ui;
    if !args.file.is_file() {
        return Err(Error::InputMissing {
            path: args.file.clone(),
        });
    }
    let inspection = context.manager.inspect(&args.file)?;

    let resolved = if args.bare {
        None
    } else {
        Some(context.manager.resolve(&args.file)?)
    };

    if ui.json {
        let environments = resolved
            .as_ref()
            .map(|r| environments_of(&r.profile))
            .unwrap_or_default();
        ui.json(&InspectOutput {
            path: args.file.display().to_string(),
            inspection,
            profile: resolved.as_ref().map(|r| r.profile.id.clone()),
            profile_source: resolved
                .as_ref()
                .map(|r| r.profile.source.label().to_string()),
            environments,
        })?;
        return Ok(0);
    }

    ui.heading("Windows file");
    ui.kv("path", &args.file.display().to_string());
    ui.kv("size", &human_bytes(inspection.size_bytes));
    ui.kv("sha256", &inspection.sha256);
    ui.kv("architecture", &inspection.arch.to_string());
    ui.kv(
        "kind",
        match (inspection.is_dll, inspection.gui) {
            (true, _) => "a library (DLL)",
            (false, true) => "a graphical program",
            (false, false) => "a console program",
        },
    );
    if inspection.dotnet {
        ui.kv("runtime", "a .NET assembly");
    }
    if inspection.signed {
        ui.kv("signature", "signed with an Authenticode certificate");
    }
    if let Some(date) = build_date(&inspection) {
        ui.kv("built", &date);
    }

    if args.imports {
        ui.heading("Imports");
        if !inspection.imports_readable {
            ui.out("  (the import table could not be read; the file may be packed)");
        } else if inspection.imports.is_empty() {
            ui.out("  (this program imports nothing dynamically)");
        } else {
            for library in &inspection.imports {
                ui.out(&format!("  {}", library.dll));
                for function in &library.functions {
                    ui.out(&format!("      {function}"));
                }
            }
        }
    } else if !inspection.imports.is_empty() {
        ui.heading("Imports");
        for library in &inspection.imports {
            ui.out(&format!(
                "  {:<28} {} function{}",
                library.dll,
                library.functions.len(),
                if library.functions.len() == 1 {
                    ""
                } else {
                    "s"
                }
            ));
        }
    }

    if let Some(resolved) = resolved {
        print_resolution(&ui, &resolved);
    } else {
        ui.heading("Compatibility");
        ui.out("  (not resolved; drop --bare to see the profile WinDrop would use)");
    }
    Ok(0)
}

fn environments_of(profile: &AppProfile) -> Vec<InspectEnvironment> {
    profile
        .variants
        .iter()
        .enumerate()
        .map(|(index, variant)| InspectEnvironment {
            index,
            rationale: variant.rationale.clone(),
            wine_build: variant.wine_build.clone(),
            arch: variant.arch.to_string(),
            windows_version: variant.windows_version.label().to_string(),
            dxvk: variant.dxvk,
            vkd3d_proton: variant.vkd3d_proton,
            dependencies: variant
                .dependencies
                .iter()
                .map(|d| d.verb.clone())
                .collect(),
        })
        .collect()
}

fn print_resolution(ui: &Ui, resolved: &ResolvedProfile) {
    ui.heading("Compatibility");
    ui.kv(
        "profile",
        &format!(
            "{} — {}",
            resolved.profile.name,
            resolved.profile.source.label()
        ),
    );
    ui.kv("profile id", &resolved.profile.id);
    if !resolved.profile.version.is_empty() {
        ui.kv("covers", &resolved.profile.version);
    }
    ui.out("");
    ui.out(&format!(
        "  {}, tried in order:",
        text::count("environment", resolved.profile.variants.len())
    ));
    for (index, variant) in resolved.profile.variants.iter().enumerate() {
        let extras: Vec<String> = [
            variant.dxvk.then(|| "DXVK".to_string()),
            variant.vkd3d_proton.then(|| "VKD3D-Proton".to_string()),
        ]
        .into_iter()
        .flatten()
        .chain(variant.dependencies.iter().map(|d| {
            if d.optional {
                format!("{} (optional)", d.verb)
            } else {
                d.verb.clone()
            }
        }))
        .collect();
        ui.out(&format!(
            "  {}. {:<22} {} {}",
            index + 1,
            format!("{} / {}", variant.wine_build, variant.arch),
            variant.windows_version.label(),
            if extras.is_empty() {
                String::new()
            } else {
                format!("+ {}", extras.join(", "))
            }
        ));
        ui.out(&format!("     {}", variant.rationale));
    }
    if !resolved.profile.installer_args.is_empty() {
        ui.out("");
        ui.kv("silent flags", &resolved.profile.installer_args.join(" "));
    }
    if !resolved.profile.notes.trim().is_empty() {
        ui.out("");
        ui.out(&format!("  {}", resolved.profile.notes));
    }
    ui.out("");
    ui.out(&format!("  {}", resolved.provenance()));
}

/// The linker timestamp as a date, when it is plausible.
///
/// Zero and post-dated values are common — reproducible builds zero it, and a
/// dishonest linker can set anything — so an implausible value is omitted
/// rather than printed as a fact.
fn build_date(inspection: &PeInspection) -> Option<String> {
    let seconds = inspection.timestamp as i64;
    if seconds <= 0 {
        return None;
    }
    // 1990-01-01 through 2100: outside that, the field is noise.
    if !(631_152_000..4_102_444_800).contains(&seconds) {
        return None;
    }
    Some(format!(
        "{} (from the PE header)",
        windrop_core::db::format_iso8601(seconds)
    ))
}

// ------------------------------------------------------------------- doctor

#[derive(Serialize)]
struct DoctorOutput<'a> {
    ready: bool,
    summary: String,
    host: &'a windrop_core::doctor::HostInfo,
    data_dir: PathBuf,
    free_space_bytes: Option<u64>,
    profiles: usize,
    installed_apps: usize,
    wine: Option<String>,
    wine_path: Option<PathBuf>,
    wine_error: Option<String>,
    sandbox: String,
    components: &'a Vec<(ComponentKind, Option<String>)>,
    tools: &'a Vec<ToolStatus>,
    missing_recommended: Vec<String>,
    setup_command: Option<String>,
    log_file: PathBuf,
}

fn cmd_doctor(context: &Context, args: DoctorArgs) -> windrop_core::Result<i32> {
    let ui = context.ui;
    let diagnostics = Diagnostics::run(&context.paths, &context.config);

    if ui.json {
        let output = DoctorOutput {
            ready: diagnostics.is_ready(),
            summary: diagnostics.summary_line(),
            host: &diagnostics.host,
            data_dir: diagnostics.data_dir.clone(),
            free_space_bytes: diagnostics.free_space_bytes,
            profiles: diagnostics.profiles,
            installed_apps: diagnostics.installed_apps,
            wine: diagnostics.wine.as_ref().map(|w| w.display()),
            wine_path: diagnostics.wine.as_ref().map(|w| w.executable.clone()),
            wine_error: diagnostics.wine_error.clone(),
            sandbox: match &diagnostics.sandbox {
                SandboxAvailability::Available(path) => format!("bubblewrap ({})", path.display()),
                SandboxAvailability::Unavailable(reason) => format!("off ({reason})"),
            },
            components: &diagnostics.components,
            tools: &diagnostics.tools,
            missing_recommended: diagnostics
                .missing(Necessity::Recommended)
                .iter()
                .map(|t| t.name.clone())
                .collect(),
            setup_command: diagnostics.setup_command(),
            log_file: context.paths.log_file(),
        };
        ui.json(&output)?;
        return Ok(if diagnostics.is_ready() {
            0
        } else {
            EXIT_NOT_READY
        });
    }

    if args.guide {
        match diagnostics.setup_command() {
            Some(command) => ui.out(&command),
            None => ui.out("Nothing is missing."),
        }
        return Ok(if diagnostics.is_ready() {
            0
        } else {
            EXIT_NOT_READY
        });
    }

    if args.summary {
        let line = diagnostics.summary_line();
        ui.out(&line);
        return Ok(if diagnostics.is_ready() {
            0
        } else {
            EXIT_NOT_READY
        });
    }

    ui.raw(&diagnostics.report());

    if args.thorough || ui.verbose() {
        ui.heading("Paths");
        ui.kv("data", &context.paths.data_dir().display().to_string());
        ui.kv("config", &context.paths.config_file().display().to_string());
        ui.kv("database", &context.paths.database().display().to_string());
        ui.kv("logs", &context.paths.log_file().display().to_string());
        if let Some(config) = &context.config.data_dir {
            ui.kv("configured", &config.display().to_string());
        }
        let _ = std::io::stdout().flush();
    }

    if let Some(issue) = diagnostics.priority_issue() {
        ui.out("");
        ui.out(&format!("Next step: {issue}"));
    }
    Ok(if diagnostics.is_ready() {
        0
    } else {
        EXIT_NOT_READY
    })
}

// ----------------------------------------------------------------- profiles

fn cmd_profiles(context: &Context, command: ProfilesCommand) -> windrop_core::Result<i32> {
    let ui = context.ui;
    let db = context.manager.db();

    match command {
        ProfilesCommand::List => {
            let profiles = db.all_profiles()?;
            if ui.json {
                ui.json(&profiles)?;
                return Ok(0);
            }
            if profiles.is_empty() {
                ui.out("The local profile database is empty.");
                ui.out("");
                ui.out("Run 'windrop profiles seed' to install the recipes WinDrop ships.");
                return Ok(0);
            }
            ui.out(&format!(
                "{:<26} {:<34} {:>10}  {}",
                "ID", "NAME", "VARIANTS", "SOURCE"
            ));
            for profile in &profiles {
                ui.out(&format!(
                    "{:<26} {:<34} {:>10}  {}",
                    profile.id,
                    truncate(&profile.name, 34),
                    profile.variants.len(),
                    profile.source.label()
                ));
            }
            ui.out("");
            ui.out(&format!(
                "{} in {}",
                text::count("profile", profiles.len()),
                context.paths.database().display()
            ));
            Ok(0)
        }

        ProfilesCommand::Show { id } => {
            // A bundled recipe that has not been seeded should still be
            // viewable, or a user cannot see the thing they are being told to
            // apply.
            let profile = match db.get_profile(&id)? {
                Some(profile) => profile,
                None => seed::bundled_profiles()?
                    .into_iter()
                    .find(|p| p.id == id)
                    .ok_or_else(|| Error::ProfileNotFound(id.clone()))?,
            };
            ui.raw(&profile.to_json_pretty()?);
            Ok(0)
        }

        ProfilesCommand::Bundled => {
            let profiles = seed::bundled_profiles()?;
            if ui.json {
                ui.json(&profiles)?;
                return Ok(0);
            }
            ui.out(&format!(
                "{} recipes ship with WinDrop ({}):",
                profiles.len(),
                context.paths.data_dir().display()
            ));
            ui.out("");
            for line in seed::bundled_summary()? {
                ui.out(&line);
            }
            ui.out("");
            ui.out("Copy them into your profile database with:");
            ui.out("  windrop profiles seed");
            Ok(0)
        }

        ProfilesCommand::Seed => {
            let report = seed::seed(db)?;
            if ui.json {
                ui.json(&report)?;
                return Ok(0);
            }
            ui.success(&report.summary());
            for id in &report.added {
                ui.status(&format!("  added {id}"));
            }
            for id in &report.kept_local {
                ui.status(&format!("  kept {id} (learned on this machine)"));
            }
            Ok(0)
        }

        ProfilesCommand::Export { file, only } => {
            let mut profiles = db.all_profiles()?;
            if !only.is_empty() {
                profiles.retain(|p| only.contains(&p.id));
                let missing: Vec<&String> = only
                    .iter()
                    .filter(|id| !profiles.iter().any(|p| &p.id == *id))
                    .collect();
                if !missing.is_empty() {
                    return Err(Error::ProfileNotFound(
                        missing
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                    ));
                }
            }
            // Exported as a bare array: that is the shape the registry accepts,
            // so what comes out of `export` can be submitted as it stands.
            let text = serde_json::to_string_pretty(&profiles)?;
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&file, format!("{text}\n"))?;
            if !ui.json {
                ui.success(&format!(
                    "wrote {} to {}",
                    text::count("profile", profiles.len()),
                    file.display()
                ));
                // Only worth saying when it is true of something in the file: a
                // recipe with no digest applies to nothing in particular.
                if profiles.iter().any(|p| p.hashes.is_empty()) {
                    ui.hint(
                        "some recipes name no installer; 'windrop profiles attach' pins one, and \
                         a pinned recipe is what the registry can actually match",
                    );
                }
            }
            Ok(0)
        }

        ProfilesCommand::Verify { file } => {
            let text = read_text(&file)?;
            match RegistryIndex::from_json(&text, &file.display().to_string()) {
                Ok(index) => {
                    if ui.json {
                        ui.json(&serde_json::json!({
                            "valid": true,
                            "profiles": index.profiles.len(),
                            "ids": index.profiles.iter().map(|p| p.id.clone()).collect::<Vec<_>>(),
                        }))?;
                    } else {
                        ui.success(&format!(
                            "{} is valid ({})",
                            file.display(),
                            text::count("profile", index.profiles.len())
                        ));
                        for profile in &index.profiles {
                            ui.bullet(&format!(
                                "{} — {} ({})",
                                profile.id,
                                profile.name,
                                text::count("environment", profile.variants.len())
                            ));
                        }
                    }
                    Ok(0)
                }
                Err(error) => {
                    // A rejected file is a normal outcome here, not a crash:
                    // report it and exit non-zero.
                    if ui.json {
                        ui.json(
                            &serde_json::json!({ "valid": false, "error": error.to_string() }),
                        )?;
                    } else {
                        ui.error(&error.to_string());
                        if let Some(hint) = error.hint() {
                            ui.hint(&hint);
                        }
                    }
                    Ok(EXIT_FAILURE)
                }
            }
        }

        ProfilesCommand::Import { file, force } => {
            let text = read_text(&file)?;
            let index = RegistryIndex::from_json(&text, &file.display().to_string())?;
            let mut imported = 0usize;
            let mut skipped: Vec<String> = Vec::new();
            for profile in index.profiles {
                // The file's own provenance is kept, and a file that states none
                // is taken as hand-written — which is what `Local` means, and
                // what makes `export` followed by `import` round-trip exactly.
                // Calling every import "from the community registry" would be a
                // lie about a recipe someone wrote themselves.
                //
                // What this machine has already proved, however, outranks a
                // file: a recipe that successfully installed something here is
                // not silently replaced by a newer copy of it.
                if db.preferred_variant(&profile.id)?.is_some() && !force {
                    skipped.push(profile.id);
                    continue;
                }
                db.upsert_profile(&profile)?;
                imported += 1;
            }
            if !ui.json {
                ui.success(&format!(
                    "imported {} from {}",
                    text::count("profile", imported),
                    file.display()
                ));
                if !skipped.is_empty() {
                    ui.warn(&format!(
                        "kept {} this machine has already installed with: {} (use --force to replace them)",
                        text::count("profile", skipped.len()),
                        skipped.join(", ")
                    ));
                }
            } else {
                ui.json(&serde_json::json!({ "imported": imported, "skipped": skipped }))?;
            }
            Ok(0)
        }

        ProfilesCommand::Delete { id } => {
            if db.get_profile(&id)?.is_none()
                && db.preferred_variant(&id)?.is_none()
                && !seed::bundled_profiles()?.iter().any(|p| p.id == id)
            {
                return Err(Error::ProfileNotFound(id));
            }
            let removed = db.delete_profile(&id)?;
            if !ui.json {
                if removed {
                    ui.success(&format!("deleted profile {id}"));
                } else {
                    ui.out(&format!("there was no profile {id} to delete"));
                }
            }
            Ok(0)
        }

        ProfilesCommand::Attach { id, file } => {
            let mut profile = match db.get_profile(&id)? {
                Some(profile) => profile,
                None => seed::bundled_profiles()?
                    .into_iter()
                    .find(|p| p.id == id)
                    .ok_or_else(|| Error::ProfileNotFound(id.clone()))?,
            };
            if !file.is_file() {
                return Err(Error::InputMissing { path: file });
            }
            let digest = windrop_core::compat::pe::sha256_file(&file)?;
            if profile.matches_hash(&digest) {
                if !ui.json {
                    ui.out(&format!("{id} already records {}", file.display()));
                }
                return Ok(0);
            }
            profile.hashes.push(digest.clone());
            profile.validate()?;
            db.upsert_profile(&profile)?;
            if ui.json {
                ui.json(&serde_json::json!({ "profile": id, "sha256": digest }))?;
            } else {
                ui.success(&format!("{id} now covers {digest}"));
                ui.hint("now run 'windrop profiles export shared-profiles.json' to share it");
            }
            Ok(0)
        }
    }
}

/// Read a file as text, with a clearer error than a bare io error.
fn read_text(path: &std::path::Path) -> windrop_core::Result<String> {
    if !path.is_file() {
        return Err(Error::InputMissing {
            path: path.to_path_buf(),
        });
    }
    Ok(std::fs::read_to_string(path)?)
}

// ------------------------------------------------------------------- update

fn cmd_update(context: &Context, args: UpdateArgs) -> windrop_core::Result<i32> {
    let ui = context.ui;
    if !context.config.allow_remote_registry {
        if ui.json {
            ui.json(&serde_json::json!({ "updates": [], "reason": "the registry is disabled" }))?;
        } else {
            ui.warn("the compatibility registry is disabled, so there is nothing to check");
            ui.hint("enable it with: windrop config set allow_remote_registry true");
        }
        return Ok(0);
    }

    let registry = match updater::client_for(&context.config, &context.paths) {
        Some(client) => client,
        None => {
            // `auto_profile_updates` is off, but an explicit `update` still
            // means the user wants to know; build a client anyway.
            RegistryClient::new(
                context.config.registry_url.clone(),
                context.paths.cache_dir(),
            )?
        }
    };

    let apps = context.manager.list_apps()?;
    let updates = updater::check_all(&registry, context.manager.db(), &apps)?;

    if ui.json {
        ui.json(&serde_json::json!({
            "checked": apps.len(),
            "updates": updates.iter().map(|u| serde_json::json!({
                "app": u.app_id,
                "name": u.app_name,
                "profile": u.profile_id,
                "installed": u.local_updated_at,
                "available": u.remote_updated_at,
            })).collect::<Vec<_>>(),
            "applied": args.apply,
        }))?;
        if args.apply {
            for update in &updates {
                updater::accept(context.manager.db(), update)?;
            }
        }
        return Ok(0);
    }

    if apps.is_empty() {
        ui.out("No applications are installed, so there is nothing to check.");
        return Ok(0);
    }

    if updates.is_empty() {
        // Spelled out rather than counted: "All 1 application installed are …"
        // is the sort of sentence that reads as a machine talking.
        ui.out(match apps.len() {
            0 => "No applications are installed.",
            1 => "The installed application is on the newest profile available.",
            _ => "Every installed application is on the newest profile available.",
        });
        return Ok(0);
    }

    ui.heading("Updates available");
    for update in &updates {
        ui.out(&format!("  {}", update.app_name));
        ui.out(&format!("     {}", update.detail()));
    }

    if args.apply {
        for update in &updates {
            updater::accept(context.manager.db(), update)?;
        }
        ui.out("");
        ui.success(&format!(
            "stored {}. Removing and reinstalling an application applies them.",
            text::count("updated profile", updates.len())
        ));
    } else {
        ui.out("");
        ui.out("These are not applied automatically: applying one means reinstalling the");
        ui.out("application, and only you can decide whether that is worth it.");
        ui.out("To store them now:");
        ui.out("  windrop update --apply");
        ui.out("To apply one to an application, keeping its data:");
        ui.out("  windrop remove <app> && windrop install <the original installer>");
    }
    Ok(0)
}

// ------------------------------------------------------------------- config

fn cmd_config(context: &Context, command: ConfigCommand) -> windrop_core::Result<i32> {
    let ui = context.ui;
    let path = context.config_path.clone();
    match command {
        ConfigCommand::Show => {
            let text = serde_json::to_string_pretty(&context.config)?;
            ui.raw(&text);
            Ok(0)
        }
        ConfigCommand::Path => {
            ui.raw(&path.display().to_string());
            Ok(0)
        }
        ConfigCommand::Keys => {
            for key in settings::KEYS {
                ui.out(&format!("{:<32} {}", key.path, key.description));
            }
            Ok(0)
        }
        ConfigCommand::Get { key } => {
            let value = settings::get(&context.config, &key)?;
            ui.raw(&settings::render(&value));
            Ok(0)
        }
        ConfigCommand::Set { key, value } => {
            let mut config = context.config.clone();
            let changed = settings::set(&mut config, &key, &value)?;
            config.validate()?;
            config.save(&path)?;
            if !ui.json {
                ui.success(&format!("{key} = {}", settings::render(&changed)));
                ui.status(&format!("written to {}", path.display()));
            } else {
                ui.json(&serde_json::json!({ "key": key, "value": changed }))?;
            }
            Ok(0)
        }
        ConfigCommand::Edit => {
            let editor = std::env::var("EDITOR")
                .or_else(|_| std::env::var("VISUAL"))
                .unwrap_or_else(|_| "vi".to_string());
            if !path.exists() {
                context.config.save(&path)?;
            }
            let status = std::process::Command::new(&editor).arg(&path).status();
            match status {
                Ok(status) if status.success() => {
                    // Re-read so a typo is caught now rather than on the next run.
                    match Config::load(&path) {
                        Ok(_) => {
                            if !ui.json {
                                ui.success("configuration is valid");
                            }
                            Ok(0)
                        }
                        Err(error) => {
                            ui.error(&format!("the edited configuration is not valid: {error}"));
                            Ok(EXIT_FAILURE)
                        }
                    }
                }
                Ok(status) => Ok(status.code().unwrap_or(EXIT_FAILURE)),
                Err(error) => Err(Error::ToolMissing(format!(
                    "{editor} (could not start it: {error}); set $EDITOR to your editor"
                ))),
            }
        }
        ConfigCommand::Reset => {
            if !confirm(
                ui,
                context.assume_yes,
                "Replace every setting with its default?",
            ) {
                return Ok(EXIT_FAILURE);
            }
            Config::default().save(&path)?;
            ui.success(&format!("restored defaults in {}", path.display()));
            Ok(0)
        }
    }
}

// --------------------------------------------------------------------- logs

fn cmd_logs(context: &Context, args: LogsArgs) -> windrop_core::Result<i32> {
    let ui = context.ui;
    // Accept either an application id or its display name, because a user
    // reading a menu entry sees the name.
    let app = match context.manager.get_app(&args.app) {
        Ok(app) => app,
        Err(error) => {
            let slug = slugify(&args.app);
            let apps = context.manager.list_apps()?;
            match apps
                .iter()
                .find(|a| slugify(&a.name) == slug || a.id == args.app)
            {
                Some(app) => app.clone(),
                None => return Err(error),
            }
        }
    };

    let mut logs = context.manager.logs_for(&app.id);
    if let Some(kind) = &args.kind {
        let wanted = format!("{kind}.log");
        logs.retain(|p| p.file_name().map(|n| n == wanted.as_str()).unwrap_or(false));
    }
    let Some(log) = logs.first() else {
        ui.out(&format!(
            "{} has not been run yet, so there is no log.",
            app.name
        ));
        return Ok(0);
    };

    if args.follow {
        if ui.json {
            return Err(Error::Config {
                field: "--follow".into(),
                reason: "cannot be combined with --json".into(),
            });
        }
        ui.status(&format!("following {} (Ctrl-C to stop)", log.display()));
        let file = std::fs::File::open(log)?;
        let mut reader = std::io::BufReader::new(file);
        let mut line = String::new();
        // Print what is there, then wait for more. This is deliberately a plain
        // poll loop: a log can be truncated and reopened by the next launch, and
        // a watched file handle would silently stop reporting.
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => std::thread::sleep(Duration::from_millis(250)),
                Ok(_) => {
                    print!("{line}");
                    let _ = std::io::stdout().flush();
                }
                Err(error) => return Err(Error::Io(error)),
            }
        }
    }

    ui.status(&log.display().to_string());
    let tail = logging::read_log_tail(log, args.lines);
    if tail.is_empty() {
        ui.out("(the log is empty)");
    } else {
        ui.raw(&tail);
    }
    Ok(0)
}

// ---------------------------------------------------------------------- gui

fn cmd_gui(context: &Context, args: GuiArgs) -> windrop_core::Result<i32> {
    let ui = context.ui;
    // The GUI is a separate binary: keeping it out of the CLI means the CLI
    // stays a few hundred kilobytes with no GTK in it, and works over SSH.
    let candidates = ["windrop-gui", "windrop_gui"];
    let mut command = None;
    for name in candidates {
        if let Some(path) = windrop_core::process::which(name) {
            command = Some(path);
            break;
        }
    }
    // Not on PATH is normal in a development tree; try the sibling binary.
    let program = match command {
        Some(path) => path,
        None => {
            let sibling = std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|dir| dir.join("windrop-gui")))
                .filter(|path| path.is_file());
            sibling.ok_or_else(|| {
                Error::ToolMissing(
                    "windrop-gui (install the windrop package, which contains both)".into(),
                )
            })?
        }
    };

    let mut child = std::process::Command::new(&program);
    // The GUI must talk to the same installation the CLI was pointed at.
    child.env("WINDROP_DATA_DIR", context.paths.data_dir());
    if context.config.data_dir.is_none() && context.paths.non_default_data_dir().is_some() {
        child.arg("--data-dir").arg(context.paths.data_dir());
    }
    for file in &args.files {
        child.arg(file);
    }
    let status = child.status()?;
    let _ = ui;
    Ok(status.code().unwrap_or(0))
}

// ------------------------------------------------------------------ version

#[derive(Serialize)]
struct VersionOutput {
    version: String,
    data_dir: PathBuf,
    config_file: PathBuf,
    database: PathBuf,
    applications_dir: PathBuf,
    wine: Option<String>,
}

fn cmd_version(context: &Context) -> windrop_core::Result<i32> {
    let ui = context.ui;
    let wine = context.manager.runtime().wine().ok();
    if ui.json {
        ui.json(&VersionOutput {
            version: windrop_core::version().to_string(),
            data_dir: context.paths.data_dir().to_path_buf(),
            config_file: context.paths.config_file(),
            database: context.paths.database(),
            applications_dir: context.paths.applications_dir().to_path_buf(),
            wine: wine.as_ref().map(|w| w.display()),
        })?;
        return Ok(0);
    }

    ui.out(&format!("windrop {}", windrop_core::version()));
    ui.out("");
    ui.kv("data", &context.paths.data_dir().display().to_string());
    ui.kv("config", &context.paths.config_file().display().to_string());
    ui.kv("database", &context.paths.database().display().to_string());
    ui.kv(
        "menu entries",
        &context.paths.applications_dir().display().to_string(),
    );
    match &wine {
        Some(wine) => ui.kv(
            "wine",
            &format!("{} at {}", wine.display(), wine.executable.display()),
        ),
        None => ui.kv("wine", "not found — run 'windrop doctor'"),
    }
    Ok(0)
}

// ------------------------------------------------------------------ helpers

/// Shorten a string for a fixed-width column, marking the cut.
fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    let mut out: String = value.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_marks_what_it_removed() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("exactly-ten", 11), "exactly-ten");
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
        // Wide characters must not be sliced in half.
        assert_eq!(truncate("ααααααα", 3), "αα…");
    }

    #[test]
    fn a_plausible_build_date_is_rendered() {
        let inspection = PeInspection {
            machine: 0,
            arch: windrop_core::compat::Arch::X86_64,
            gui: true,
            subsystem: 2,
            is_dll: false,
            dotnet: false,
            signed: false,
            imports: vec![],
            imports_readable: true,
            timestamp: 1_700_000_000,
            size_bytes: 0,
            sha256: String::new(),
            path: None,
        };
        let date = build_date(&inspection).expect("a date");
        assert!(date.starts_with("2023-11-14"), "{date}");
    }

    #[test]
    fn implausible_build_dates_are_omitted_rather_than_guessed() {
        let mut inspection = PeInspection {
            machine: 0,
            arch: windrop_core::compat::Arch::X86_64,
            gui: false,
            subsystem: 3,
            is_dll: false,
            dotnet: false,
            signed: false,
            imports: vec![],
            imports_readable: true,
            timestamp: 0,
            size_bytes: 0,
            sha256: String::new(),
            path: None,
        };
        assert!(
            build_date(&inspection).is_none(),
            "zero means the linker did not record it"
        );

        inspection.timestamp = u32::MAX;
        assert!(
            build_date(&inspection).is_none(),
            "2106 is not a build date"
        );

        inspection.timestamp = 1;
        assert!(
            build_date(&inspection).is_none(),
            "1970 is not a build date"
        );
    }
}
