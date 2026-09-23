//! The command-line surface.
//!
//! Every flag that changes how the pipeline runs is *global*: the desktop entry
//! WinDrop writes invokes `windrop launch <id>`, and a user typing
//! `windrop --offline install foo.exe` should not have to remember which flags
//! belong before the subcommand and which after it.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::ui::ColorChoice;

#[derive(Debug, Parser)]
#[command(
    name = "windrop",
    version,
    about = "Run Windows applications on Linux by dropping them on a window",
    long_about = "WinDrop turns a Windows .exe or .msi into a Linux application: it inspects the \
                  file, resolves a compatibility profile, builds a self-contained Wine prefix, \
                  runs the installer, and adds the result to your menu.\n\n\
                  Everything WinDrop creates lives under its own data directory. Nothing is \
                  installed system-wide and it never needs root.",
    max_term_width = 100,
    disable_help_subcommand = true,
    propagate_version = true
)]
pub struct Cli {
    #[command(flatten)]
    pub global: Global,

    #[command(subcommand)]
    pub command: Command,
}

/// Options that apply to whatever subcommand is given.
#[derive(Debug, Clone, Args)]
pub struct Global {
    /// Where WinDrop keeps applications, prefixes and shared runtimes.
    #[arg(long, short = 'd', global = true, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,

    /// Read configuration from FILE instead of the data directory.
    #[arg(long, short = 'c', global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Say more. Repeat for more detail still.
    #[arg(long, short = 'v', global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Say nothing but errors.
    #[arg(long, short = 'q', global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Print machine-readable output on stdout.
    #[arg(long, global = true)]
    pub json: bool,

    /// Never contact the compatibility registry.
    #[arg(long, global = true)]
    pub offline: bool,

    /// Do not use the bubblewrap sandbox.
    #[arg(long, global = true)]
    pub no_sandbox: bool,

    /// Use this Wine binary instead of the one WinDrop would resolve.
    #[arg(long, global = true, value_name = "PATH")]
    pub wine: Option<PathBuf>,

    /// Colourise output.
    #[arg(long, global = true, value_enum, default_value_t = ColorChoice::Auto)]
    pub color: ColorChoice,

    /// Assume yes; never ask a question.
    #[arg(long, short = 'y', global = true)]
    pub yes: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Install one or more Windows programs.
    Install(InstallArgs),

    /// List the applications WinDrop has installed.
    List(ListArgs),

    /// Start an installed application.
    ///
    /// This is what a menu entry calls. It is also the way to start something
    /// from a terminal, and `--wait` is the way to see its output.
    Launch(LaunchArgs),

    /// Remove an application and everything it owns.
    Remove(RemoveArgs),

    /// Report what a Windows file is and how WinDrop would run it.
    Inspect(InspectArgs),

    /// Check the machine and explain what is missing.
    Doctor(DoctorArgs),

    /// Inspect and manage compatibility profiles.
    Profiles(ProfilesArgs),

    /// Check for, and apply, updated compatibility profiles.
    Update(UpdateArgs),

    /// Read and write settings.
    Config(ConfigArgs),

    /// Show an application's log.
    Logs(LogsArgs),

    /// Open the graphical interface.
    Gui(GuiArgs),

    /// Print version and location information.
    Version,
}

#[derive(Debug, Args)]
pub struct InstallArgs {
    /// The installer or portable program to install.
    #[arg(required = true, value_name = "FILE")]
    pub files: Vec<PathBuf>,

    /// Work out everything, then stop without changing anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Name the application, instead of guessing from the file.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Use this id for the application, instead of deriving one.
    #[arg(long, value_name = "ID")]
    pub id: Option<String>,

    /// Apply a specific profile instead of looking one up.
    ///
    /// `windrop profiles bundled` lists the recipes WinDrop ships.
    #[arg(long, value_name = "PROFILE")]
    pub profile: Option<String>,

    /// Run the installer silently, showing no window.
    ///
    /// This is the default when there is no terminal to attach a window to.
    #[arg(long, conflicts_with = "interactive")]
    pub silent: bool,

    /// Hand the installer to you to click through.
    ///
    /// This is the default when a terminal is attached, because most installers
    /// need at least one decision from a human.
    #[arg(long)]
    pub interactive: bool,

    /// Try only the profile's first environment.
    ///
    /// Faster, but gives up where the fallback chain would have found a working
    /// combination.
    #[arg(long)]
    pub no_fallback: bool,

    /// Use this installed program as the application's entry point.
    ///
    /// Paths are read as Windows paths (`C:\...`) or as paths relative to the
    /// prefix's `drive_c`.
    #[arg(long, value_name = "EXE")]
    pub main_exe: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Print one id per line, for use in scripts.
    #[arg(long)]
    pub short: bool,

    /// Show how much disk each application uses. Slower on large prefixes.
    #[arg(long)]
    pub sizes: bool,
}

#[derive(Debug, Args)]
pub struct LaunchArgs {
    /// The application id, as shown by `windrop list`.
    #[arg(value_name = "APP")]
    pub app: String,

    /// Stay in the foreground until the program exits.
    #[arg(long, short = 'w', conflicts_with = "plan_only")]
    pub wait: bool,

    /// Print the exact command that would run, and stop.
    #[arg(long)]
    pub plan_only: bool,

    /// Stop the application's Wine server, leaving nothing running.
    #[arg(long, conflicts_with_all = ["wait", "plan_only"])]
    pub stop: bool,
}

#[derive(Debug, Args)]
pub struct RemoveArgs {
    /// The application id, as shown by `windrop list`.
    #[arg(value_name = "APP", required_unless_present = "all")]
    pub apps: Vec<String>,

    /// Remove every installed application.
    #[arg(long)]
    pub all: bool,

    /// Also forget the compatibility profile learned for this application.
    #[arg(long)]
    pub forget_learning: bool,
}

#[derive(Debug, Args)]
pub struct InspectArgs {
    /// A Windows file: `.exe`, `.msi` or `.bat`.
    #[arg(value_name = "FILE")]
    pub file: PathBuf,

    /// List every imported function, not just the libraries.
    #[arg(long)]
    pub imports: bool,

    /// Do not resolve a compatibility profile.
    #[arg(long)]
    pub bare: bool,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Print only the one-line summary.
    #[arg(long, short = 's', conflicts_with = "guide")]
    pub summary: bool,

    /// Print only the command that installs what is missing.
    #[arg(long)]
    pub guide: bool,

    /// Check the parts of the machine the doctor normally skips.
    #[arg(long)]
    pub thorough: bool,
}

#[derive(Debug, Args)]
pub struct ProfilesArgs {
    #[command(subcommand)]
    pub command: ProfilesCommand,
}

#[derive(Debug, Subcommand)]
pub enum ProfilesCommand {
    /// List the profiles in the local database.
    List,

    /// Show one profile in full.
    Show {
        #[arg(value_name = "PROFILE")]
        id: String,
    },

    /// List the recipes that ship with WinDrop.
    Bundled,

    /// Copy the bundled recipes into the local database.
    ///
    /// This is what makes `notepadpp` and the others findable without a network
    /// connection. Local knowledge is never overwritten.
    Seed,

    /// Write the local database to a file, for sharing.
    Export {
        #[arg(value_name = "FILE")]
        file: PathBuf,

        /// Export only these profiles.
        #[arg(long, value_name = "PROFILE")]
        only: Vec<String>,
    },

    /// Read profiles from a file into the local database.
    Import {
        #[arg(value_name = "FILE")]
        file: PathBuf,

        /// Overwrite profiles that were learned on this machine.
        #[arg(long)]
        force: bool,
    },

    /// Check that a profile file is valid, without importing it.
    Verify {
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },

    /// Remove a profile from the local database.
    Delete {
        #[arg(value_name = "PROFILE")]
        id: String,
    },

    /// Record an installer's digest against a profile.
    ///
    /// The next time you drop that exact file, WinDrop uses this profile
    /// immediately. This is the first half of contributing a profile.
    Attach {
        #[arg(value_name = "PROFILE")]
        id: String,

        #[arg(value_name = "FILE")]
        file: PathBuf,
    },
}

#[derive(Debug, Args)]
pub struct UpdateArgs {
    /// Store the newer profiles without reinstalling anything.
    #[arg(long)]
    pub apply: bool,
}

#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the effective configuration.
    Show,

    /// Print the path of the configuration file.
    Path,

    /// Print every setting that can be read or written.
    Keys,

    /// Print one setting.
    Get {
        #[arg(value_name = "KEY")]
        key: String,
    },

    /// Change one setting.
    ///
    /// Values are read as JSON when they look like JSON, and as text otherwise,
    /// so `true`, `3`, `["/home/me/Shared"]` and `staging` all do what you mean.
    Set {
        #[arg(value_name = "KEY")]
        key: String,

        #[arg(value_name = "VALUE")]
        value: String,
    },

    /// Open the configuration file in $EDITOR.
    Edit,

    /// Restore every setting to its default.
    Reset,
}

#[derive(Debug, Args)]
pub struct LogsArgs {
    /// The application id, as shown by `windrop list`.
    #[arg(value_name = "APP")]
    pub app: String,

    /// How many lines to show.
    #[arg(long, short = 'n', default_value_t = 80, value_name = "LINES")]
    pub lines: usize,

    /// Which log to read: `launch`, `installer`, or `wineboot` (the default is
    /// the most recent one written).
    #[arg(long, value_name = "KIND")]
    pub kind: Option<String>,

    /// Keep printing as the file grows. Stop with Ctrl-C.
    #[arg(long, short = 'f')]
    pub follow: bool,
}

#[derive(Debug, Args)]
pub struct GuiArgs {
    /// Files to offer to the GUI as soon as it opens.
    #[arg(value_name = "FILE")]
    pub files: Vec<PathBuf>,
}
