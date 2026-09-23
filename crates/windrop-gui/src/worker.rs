//! Background work.
//!
//! Nothing that can take longer than a frame happens on the GTK main thread:
//! installing an application takes minutes, probing for Wine takes seconds, and
//! a profile lookup can touch the network. Each of those runs on a worker thread
//! and reports back over a channel that the main loop drains.
//!
//! A worker builds its *own* [`ApplicationManager`] rather than sharing one.
//! That is deliberate: the manager owns a SQLite connection, which is `Send` but
//! not `Sync`, so sharing it would not compile — and opening the database per
//! task costs microseconds next to the work each task does.
//!
//! The channel is a plain `std::sync::mpsc`, drained by a timer on the main
//! loop. That keeps the GUI free of an async runtime it has no other use for.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use windrop_core::compat::{InputKind, PeInspection};
use windrop_core::config::Config;
use windrop_core::doctor::Diagnostics;
use windrop_core::logging;
use windrop_core::manager::metadata::InstalledApp;
use windrop_core::manager::{ApplicationManager, InstallOptions, ProgressFn};
use windrop_core::paths::Paths;
use windrop_core::registry::RegistryClient;
use windrop_core::updater::{self, ProfileUpdate};
use windrop_core::Error;

/// What a worker thread tells the window.
#[derive(Debug)]
pub enum Message {
    /// A human-readable line for the status area.
    Stage(String),
    /// The installed-application list, freshly read from disk.
    Apps(Vec<InstalledApp>),
    InstallFinished(Box<Result<Installed, String>>),
    RemoveFinished(Box<Result<String, String>>),
    LaunchFinished(Box<Result<String, String>>),
    Doctor(Box<Diagnostics>),
    Inspection(Box<Result<PeInspection, String>>),
    Updates(Vec<ProfileUpdate>),
    /// One streamed line of the one-click setup run.
    SetupLine(String),
    /// The one-click setup finished: what got installed, or why it failed.
    SetupFinished(Box<Result<String, String>>),
    Log {
        app: String,
        text: String,
    },
    /// A note for the status area: good, or bad.
    Success(String),
    Warning(String),
    Failed(String),
}

/// What an install produced, in the few words the window needs.
#[derive(Debug, Clone)]
pub struct Installed {
    pub name: String,
    pub attempts: usize,
    /// Where the menu entry went, so the window can say whether there is one.
    pub desktop_file: Option<PathBuf>,
}

/// A unit of work for a thread.
#[derive(Debug, Clone)]
pub enum Task {
    Refresh,
    Inspect(PathBuf),
    Install(PathBuf),
    Launch(String),
    Stop(String),
    Remove(String),
    Doctor,
    CheckUpdates,
    ReadLog {
        app: String,
        lines: usize,
    },
    /// Install Wine and the missing helpers in one go (see `setup_install`).
    SetupInstall,
}

/// A failure phrased the way the interface should show it: the message, and the
/// hint underneath when WinDrop has one.
pub fn describe(error: &Error) -> String {
    match error.hint() {
        Some(hint) => format!("{error}\n\n{hint}"),
        None => error.to_string(),
    }
}

/// Run `task` on a worker thread, reporting through `tx`.
pub fn spawn(tx: Sender<Message>, paths: Paths, config: Config, task: Task) {
    std::thread::Builder::new()
        .name("windrop-task".to_string())
        .spawn(move || {
            let note = |message: String| {
                let _ = tx.send(Message::Failed(message));
            };
            let manager = match ApplicationManager::new(paths.clone(), config.clone()) {
                Ok(manager) => manager,
                Err(error) => {
                    note(describe(&error));
                    return;
                }
            };
            match task {
                Task::Refresh => refresh(&tx, &manager),
                Task::Inspect(path) => inspect(&tx, &manager, &path),
                Task::Install(path) => install(&tx, &manager, &config, &path),
                Task::Launch(id) => launch(&tx, &manager, &id),
                Task::Stop(id) => stop(&tx, &manager, &id),
                Task::Remove(id) => remove(&tx, &manager, &id),
                Task::Doctor => doctor(&tx, &paths, &config),
                Task::CheckUpdates => check_updates(&tx, &paths, &config, &manager),
                Task::ReadLog { app, lines } => read_log(&tx, &manager, &app, lines),
                Task::SetupInstall => setup_install(&tx, &paths, &config),
            }
        })
        .expect("a worker thread");
}

fn refresh(tx: &Sender<Message>, manager: &ApplicationManager) {
    match manager.list_apps() {
        Ok(apps) => {
            let _ = tx.send(Message::Apps(apps));
        }
        Err(error) => {
            let _ = tx.send(Message::Failed(describe(&error)));
        }
    }
}

/// Report what a dropped file is, before doing anything with it.
///
/// Showing the name, architecture and size first means a mis-drop — the wrong
/// file, or a DLL — is obvious immediately rather than after a few minutes of
/// installing.
fn inspect(tx: &Sender<Message>, manager: &ApplicationManager, path: &Path) {
    match manager.inspect(path) {
        Ok(inspection) => {
            let _ = tx.send(Message::Inspection(Box::new(Ok(inspection))));
        }
        Err(error) => {
            let _ = tx.send(Message::Inspection(Box::new(Err(describe(&error)))));
        }
    }
}

fn install(tx: &Sender<Message>, manager: &ApplicationManager, config: &Config, path: &Path) {
    // Fail fast and clearly on something that is not an installer at all.
    if let Err(error) = InputKind::from_path(path) {
        let _ = tx.send(Message::Failed(describe(&error)));
        return;
    }

    let options = InstallOptions {
        unattended: config.install_silently,
        ..Default::default()
    };

    // An installer with a window is the default: most of them ask at least one
    // question, and answering it takes seconds where guessing goes wrong
    // silently. The setting exists for someone who wants it to just happen.
    let result = if config.install_silently {
        let progress: ProgressFn = {
            let tx = tx.clone();
            Arc::new(move |stage| {
                let _ = tx.send(Message::Stage(stage.label()));
            })
        };
        manager.install_with_progress(path, options, progress)
    } else {
        let _ = tx.send(Message::Stage(
            "Waiting for the installer to finish…".to_string(),
        ));
        manager.install_interactively(path, &options)
    };

    let outcome = result
        .map(|report| Installed {
            name: report.app.name.clone(),
            attempts: report.app.attempts,
            desktop_file: report.app.desktop_file.clone(),
        })
        .map_err(|error| describe(&error));
    let _ = tx.send(Message::InstallFinished(Box::new(outcome)));
}

fn launch(tx: &Sender<Message>, manager: &ApplicationManager, id: &str) {
    let name = manager
        .get_app(id)
        .map(|app| app.name)
        .unwrap_or_else(|_| id.to_string());
    let result = manager
        .launch_detached(id)
        .map(|_| name.clone())
        .map_err(|error| describe(&error));
    let _ = tx.send(Message::LaunchFinished(Box::new(result)));
}

fn stop(tx: &Sender<Message>, manager: &ApplicationManager, id: &str) {
    let prefix = match manager.get_app(id) {
        Ok(app) => app.prefix(manager.paths()),
        Err(error) => {
            let _ = tx.send(Message::Failed(describe(&error)));
            return;
        }
    };
    match manager.runtime().shutdown_prefix(&prefix) {
        Ok(()) => {
            let _ = tx.send(Message::Success(format!("Stopped everything for {id}.")));
        }
        Err(error) => {
            let _ = tx.send(Message::Failed(describe(&error)));
        }
    }
}

fn remove(tx: &Sender<Message>, manager: &ApplicationManager, id: &str) {
    let result = manager
        .remove(id)
        .and_then(|report| {
            // The point of one-click removal is that nothing is left behind, so
            // it is verified rather than assumed.
            manager.verify_no_residue(id)?;
            Ok(report.name)
        })
        .map_err(|error| describe(&error));
    let _ = tx.send(Message::RemoveFinished(Box::new(result)));
}

fn doctor(tx: &Sender<Message>, paths: &Paths, config: &Config) {
    // `Diagnostics::run` never fails; it reports what it could not find.
    let _ = tx.send(Message::Doctor(Box::new(Diagnostics::run(paths, config))));
}

/// Install Wine and every missing helper, streaming progress.
///
/// The plan is rebuilt here rather than handed in: the minutes between the
/// check and the click are exactly when a user might have installed something
/// by hand, and installing what is already there would be pure noise.
fn setup_install(tx: &Sender<Message>, paths: &Paths, config: &Config) {
    use windrop_core::setup::{setup_log_file, SetupOffer};

    let diagnostics = Diagnostics::run(paths, config);
    let plan = match diagnostics.setup_offer() {
        SetupOffer::Ready(plan) => plan,
        SetupOffer::NothingMissing => {
            let _ = tx.send(Message::SetupFinished(Box::new(Ok(
                "Everything WinDrop needs is already present.".to_string(),
            ))));
            return;
        }
        SetupOffer::Unavailable(reason) => {
            let _ = tx.send(Message::SetupFinished(Box::new(Err(reason))));
            return;
        }
    };
    let log = setup_log_file(paths.data_dir());
    let lines = tx.clone();
    let ran = plan.run(&log, &|line| {
        let _ = lines.send(Message::SetupLine(line.to_string()));
    });
    match ran {
        Ok(()) => {
            let _ = tx.send(Message::SetupFinished(Box::new(Ok(format!(
                "Installed {}. Checking again…",
                plan.packages.join(", ")
            )))));
        }
        Err(error) => {
            let _ = tx.send(Message::SetupFinished(Box::new(Err(format!(
                "{}\n\nFull log: {}",
                describe(&error),
                log.display()
            )))));
        }
    }
}

fn check_updates(
    tx: &Sender<Message>,
    paths: &Paths,
    config: &Config,
    manager: &ApplicationManager,
) {
    if !config.allow_remote_registry {
        let _ = tx.send(Message::Updates(Vec::new()));
        return;
    }
    let client = match RegistryClient::new(config.registry_url.clone(), paths.cache_dir()) {
        Ok(client) => client,
        Err(error) => {
            let _ = tx.send(Message::Warning(describe(&error)));
            return;
        }
    };
    let apps = match manager.list_apps() {
        Ok(apps) => apps,
        Err(error) => {
            let _ = tx.send(Message::Failed(describe(&error)));
            return;
        }
    };
    match updater::check_all(&client, manager.db(), &apps) {
        Ok(updates) => {
            let _ = tx.send(Message::Updates(updates));
        }
        Err(error) => {
            let _ = tx.send(Message::Warning(format!(
                "Could not check for updated profiles: {error}"
            )));
        }
    }
}

fn read_log(tx: &Sender<Message>, manager: &ApplicationManager, app: &str, lines: usize) {
    let Some(path) = manager.logs_for(app).into_iter().next() else {
        let _ = tx.send(Message::Log {
            app: app.to_string(),
            text: "This application has not been run yet, so there is no log.".to_string(),
        });
        return;
    };
    let _ = tx.send(Message::Log {
        app: app.to_string(),
        text: logging::read_log_tail(&path, lines),
    });
}
