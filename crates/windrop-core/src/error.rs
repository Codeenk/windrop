//! Error types shared across every WinDrop subsystem.

use std::path::PathBuf;

/// Everything that can go wrong while installing, running or removing a
/// Windows application.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    // ---------------------------------------------------------------- input
    #[error("{path} does not exist or is not a regular file")]
    InputMissing { path: PathBuf },

    #[error("unsupported input file type: {0} (WinDrop accepts .exe, .msi and .bat installers)")]
    UnsupportedInput(String),

    #[error("'{path}' is not a valid Windows executable: {reason}")]
    NotAPeFile { path: PathBuf, reason: String },

    #[error("'{0}' is a DLL, not an application. Drop the program that uses it instead.")]
    IsALibrary(PathBuf),

    #[error("unsupported CPU architecture '{0}' (WinDrop supports 32-bit x86 and 64-bit x86 Windows binaries)")]
    UnsupportedArch(String),

    // -------------------------------------------------------------- runtime
    #[error("Wine was not found on this system. Install it first ({hint}).")]
    WineMissing { hint: String },

    #[error("no usable Wine build is available for '{0}' workloads")]
    NoWineVariant(String),

    #[error("'winetricks' was not found. It is required to install {dependency}.")]
    WinetricksMissing { dependency: String },

    #[error("could not find the '{0}' command required for this operation")]
    ToolMissing(String),

    #[error("external command failed (exit {code}): {command}")]
    CommandFailed {
        command: String,
        code: i32,
        stderr: String,
    },

    #[error("timed out after {seconds}s waiting for: {command}")]
    Timeout { command: String, seconds: u64 },

    #[error("the application did not install correctly: {rationale}")]
    InstallIncomplete { rationale: String },

    #[error("every compatibility profile failed to run '{app}'. Last failure: {last_error}")]
    AllVariantsFailed { app: String, last_error: String },

    // ------------------------------------------------------------- data/io
    #[error("unknown application id '{0}'")]
    AppNotFound(String),

    #[error("'{0}' is already installed")]
    AppAlreadyInstalled(String),

    #[error("profile '{0}' not found in the local database or the remote registry")]
    ProfileNotFound(String),

    #[error(
        "profile {index} in {document} is not valid{}: {reason}",
        if id_hint.is_empty() { String::new() } else { format!(" ('{id_hint}')") }
    )]
    InvalidProfile {
        document: String,
        index: usize,
        id_hint: String,
        reason: String,
    },

    #[error(
        "cannot start a download: {0}. For private mirrors, disable profile updates in settings."
    )]
    DownloadBlocked(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("malformed JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("network error while contacting the profile registry: {0}")]
    Http(Box<reqwest::Error>),

    #[error("invalid configuration value for '{field}': {reason}")]
    Config { field: String, reason: String },

    #[error("archive error: {0}")]
    Archive(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Http(Box::new(e))
    }
}

impl Error {
    /// A short, human-actionable hint shown under the message in the GUI.
    pub fn hint(&self) -> Option<String> {
        match self {
            Error::WineMissing { hint } => Some(hint.clone()),
            Error::WinetricksMissing { .. } => {
                Some("Install it with: sudo pacman -S winetricks".to_string())
            }
            Error::ToolMissing(tool) => Some(format!(
                "On Arch-based systems: sudo pacman -S {}",
                arch_package_for(tool)
            )),
            Error::UnsupportedArch(_) => Some(
                "WinDrop targets x86 and x86_64 Windows binaries. ARM-only binaries need an \
                 ARM Wine build and are not supported yet."
                    .to_string(),
            ),
            Error::IsALibrary(_) => Some(
                "DLLs are installed as dependencies of an application, not on their own.".into(),
            ),
            Error::InvalidProfile { .. } => Some(
                "A profile is a JSON object with an id, a name and at least one runtime variant. \
                 Entries are numbered from zero."
                    .to_string(),
            ),
            Error::DownloadBlocked(_) => Some("Nothing was changed on your system.".to_string()),
            Error::CommandFailed { stderr, .. } if !stderr.is_empty() => {
                Some(format!("Last output: {}", first_line(stderr)))
            }
            _ => None,
        }
    }

    /// True when re-running the operation with a different Wine variant could
    /// plausibly succeed. Used by the fallback chain executor.
    pub fn is_recoverable_by_retry(&self) -> bool {
        matches!(
            self,
            Error::CommandFailed { .. }
                | Error::InstallIncomplete { .. }
                | Error::Timeout { .. }
                | Error::NoWineVariant(_)
        )
    }
}

fn first_line(s: &str) -> String {
    let line = s
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut out: String = line.chars().take(200).collect();
    if line.chars().count() > 200 {
        out.push('…');
    }
    out
}

/// Map a missing CLI tool to the Arch package that provides it.
pub fn arch_package_for(tool: &str) -> &'static str {
    match tool {
        "wine" | "wine64" => "wine",
        "winetricks" => "winetricks",
        "bwrap" => "bubblewrap",
        "wrestool" | "icotool" => "icoutils",
        "cabextract" => "cabextract",
        "update-desktop-database" | "xdg-desktop-menu" => "desktop-file-utils",
        "7z" | "7za" => "p7zip",
        _ => tool_box(tool),
    }
}

// `arch_package_for` must be const-friendly for the match above, so the
// fallback returns a static string chosen at compile time.
const fn tool_box(_tool: &str) -> &'static str {
    "the package that provides this tool"
}

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hint_for_wine_missing_is_actionable() {
        let e = Error::WineMissing {
            hint: "sudo pacman -S wine".into(),
        };
        assert!(e.hint().unwrap().contains("pacman"));
    }

    #[test]
    fn command_failed_hint_shows_last_output_line() {
        let e = Error::CommandFailed {
            command: "wine x.exe".into(),
            code: 1,
            stderr: "fixme: thing\nsome real error\n\n".into(),
        };
        assert_eq!(e.hint().unwrap(), "Last output: some real error");
    }

    #[test]
    fn long_output_lines_are_truncated_in_hints() {
        let e = Error::CommandFailed {
            command: "x".into(),
            code: 1,
            stderr: "y".repeat(400),
        };
        let hint = e.hint().unwrap();
        assert!(hint.ends_with('…'));
        assert!(hint.chars().count() < 240);
    }

    #[test]
    fn recoverable_classification_matches_fallback_semantics() {
        assert!(Error::InstallIncomplete {
            rationale: "no exe found".into()
        }
        .is_recoverable_by_retry());
        assert!(!Error::UnsupportedArch("ARM64".into()).is_recoverable_by_retry());
    }

    #[test]
    fn tool_package_mapping_covers_sandbox_and_icons() {
        assert_eq!(arch_package_for("bwrap"), "bubblewrap");
        assert_eq!(arch_package_for("wrestool"), "icoutils");
    }
}
