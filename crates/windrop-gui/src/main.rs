//! `windrop-gui` — the window.
//!
//! A thin shell around `windrop-core`: resolve where the data lives, load the
//! configuration, open a log, hand a channel to the window, and run the GTK main
//! loop. Every decision the window makes is a call into the core, so the GUI and
//! the command line cannot drift apart.
//!
//! Two command-line forms matter:
//!
//! ```text
//! windrop-gui                     # just open the window
//! windrop-gui Setup.exe           # open it and offer to install this file
//! ```
//!
//! The second is what makes WinDrop usable as a file manager's "Open with" target
//! and from a shell — dragging onto the window stays the primary path, but it
//! should not be the only one.

mod app;
mod worker;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::channel;

use gtk::gio;
use gtk::prelude::*;

use windrop_core::config::Config;
use windrop_core::paths::Paths;
use windrop_core::{logging, Result};

const APPLICATION_ID: &str = "org.windrop.WinDrop";

/// The command line, which is deliberately tiny: this is a GUI, and everything
/// else is a setting.
#[derive(Debug, Default)]
struct Args {
    data_dir: Option<PathBuf>,
    config: Option<PathBuf>,
    files: Vec<PathBuf>,
    print_version: bool,
    print_help: bool,
}

fn main() {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("windrop-gui: {message}");
            eprintln!("Try `windrop-gui --help`.");
            std::process::exit(2);
        }
    };

    if args.print_help {
        print!("{HELP}");
        return;
    }
    if args.print_version {
        println!("windrop-gui {}", windrop_core::version());
        return;
    }

    if let Err(error) = run(args) {
        // The window may never have appeared, so the terminal is the only place
        // left to explain what went wrong.
        eprintln!("windrop-gui: {error}");
        if let Some(hint) = error.hint() {
            eprintln!("\n{hint}");
        }
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<()> {
    // The configuration file itself lives inside the data directory, so the
    // path has to be guessed before it can be read — unless the user or the
    // environment said where it is.
    let provisional = match &args.data_dir {
        Some(dir) => Paths::with_data_dir(dir),
        None => Paths::discover()?,
    };
    let config_path = args
        .config
        .clone()
        .unwrap_or_else(|| provisional.config_file());
    let config = Config::load(&config_path)?;

    let paths = match &args.data_dir {
        Some(dir) => Paths::with_data_dir(dir),
        // `data_dir` in the configuration wins over the default, which is what
        // lets a user move their whole installation.
        None => config.resolve_paths()?,
    };
    paths.ensure()?;

    // A GUI is usually started from a menu, where nothing reads stderr; the file
    // log is therefore the one that matters. Debug-level noise goes to the log
    // and warnings to the terminal.
    let _log = logging::init_split(&config.log_level, "warn", &paths.logs_dir())?;

    tracing::info!(
        data_dir = %paths.data_dir().display(),
        config = %config_path.display(),
        "windrop-gui starting"
    );

    let (sender, receiver) = channel();

    // HANDLES_OPEN is what makes `windrop-gui Setup.exe` work, and what lets a
    // second invocation hand its file to the window that is already open.
    let application = gtk::Application::builder()
        .application_id(APPLICATION_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();

    let session = Rc::new(RefCell::new(Session {
        window: None,
        // Taken by whichever of `activate` / `open` creates the window first.
        receiver: Some(receiver),
    }));

    {
        let session = session.clone();
        let paths = paths.clone();
        let config = config.clone();
        let sender = sender.clone();
        application.connect_activate(move |application| {
            let window = session
                .borrow_mut()
                .window(application, &paths, &config, &sender);
            window.present();
        });
    }

    {
        let session = session.clone();
        let paths = paths.clone();
        let config = config.clone();
        application.connect_open(move |application, files, _hint| {
            let window = session
                .borrow_mut()
                .window(application, &paths, &config, &sender);
            window.present();
            for file in files {
                if let Some(path) = file.path() {
                    window.offer_file(path);
                }
            }
        });
    }

    application.run_with_args::<&str>(&[]);
    Ok(())
}

/// The one window this process owns, plus the worker channel it listens on.
struct Session {
    window: Option<Rc<app::Window>>,
    receiver: Option<std::sync::mpsc::Receiver<worker::Message>>,
}

impl Session {
    /// The window, opened if it is not open yet.
    fn window(
        &mut self,
        application: &gtk::Application,
        paths: &Paths,
        config: &Config,
        sender: &std::sync::mpsc::Sender<worker::Message>,
    ) -> Rc<app::Window> {
        if let Some(window) = &self.window {
            return window.clone();
        }
        let window = app::Window::new(application, paths.clone(), config.clone(), sender.clone());
        // Listening first: anything queued before the main loop starts draining
        // the channel would still be delivered, but the ordering is easier to
        // reason about this way.
        if let Some(receiver) = self.receiver.take() {
            window.run(receiver);
        }
        self.window = Some(window.clone());
        window
    }
}

const HELP: &str = "\
windrop-gui — run Windows programs on Linux

Usage:
  windrop-gui [OPTIONS] [FILE]...

Running it with a FILE opens the window and offers to install that program.

Options:
      --data-dir <DIR>   keep applications in DIR instead of the default
      --config <FILE>    read settings from FILE
  -h, --help             show this help
  -V, --version          show the version

The command line (`windrop`) does everything the window does, and more.
";

fn parse_args(args: impl Iterator<Item = String>) -> std::result::Result<Args, String> {
    let mut parsed = Args::default();
    let mut args = args;

    while let Some(arg) = args.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) if name.starts_with("--") => {
                (name.to_string(), Some(value.to_string()))
            }
            _ => (arg.clone(), None),
        };

        let mut value = |name: &str| -> std::result::Result<String, String> {
            match &inline {
                Some(value) => Ok(value.clone()),
                None => args.next().ok_or_else(|| format!("{name} needs a value")),
            }
        };

        match name.as_str() {
            "-h" | "--help" => parsed.print_help = true,
            "-V" | "--version" => parsed.print_version = true,
            "--data-dir" => parsed.data_dir = Some(PathBuf::from(value("--data-dir")?)),
            "--config" => parsed.config = Some(PathBuf::from(value("--config")?)),
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown option '{other}'"));
            }
            // Anything else is a file to open, which is how a file manager
            // hands WinDrop a program.
            _ => parsed.files.push(PathBuf::from(arg)),
        }
    }

    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> std::result::Result<Args, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn the_common_case_is_a_bare_launch() {
        let args = parse(&[]).unwrap();
        assert!(args.data_dir.is_none());
        assert!(args.files.is_empty());
        assert!(!args.print_version);
    }

    #[test]
    fn a_data_directory_can_be_given_either_way() {
        assert_eq!(
            parse(&["--data-dir", "/tmp/wd"]).unwrap().data_dir,
            Some(PathBuf::from("/tmp/wd"))
        );
        assert_eq!(
            parse(&["--data-dir=/tmp/wd"]).unwrap().data_dir,
            Some(PathBuf::from("/tmp/wd"))
        );
    }

    #[test]
    fn a_missing_value_is_an_error_rather_than_a_silent_default() {
        assert!(parse(&["--data-dir"]).is_err());
        assert!(parse(&["--config"]).is_err());
    }

    #[test]
    fn an_unknown_option_is_refused() {
        assert!(parse(&["--threads", "4"]).is_err());
    }

    #[test]
    fn files_are_collected_in_order_and_alongside_options() {
        let args = parse(&["a.exe", "--data-dir=/tmp/x", "b.msi"]).unwrap();
        assert_eq!(
            args.files,
            vec![PathBuf::from("a.exe"), PathBuf::from("b.msi")]
        );
        assert_eq!(args.data_dir, Some(PathBuf::from("/tmp/x")));
    }

    #[test]
    fn help_and_version_do_not_need_a_display() {
        assert!(parse(&["--help"]).unwrap().print_help);
        assert!(parse(&["-h"]).unwrap().print_help);
        assert!(parse(&["--version"]).unwrap().print_version);
        assert!(parse(&["-V"]).unwrap().print_version);
    }
}
