//! Output formatting.
//!
//! Two rules shape everything here:
//!
//! * **Data goes to stdout, everything else goes to stderr.** `windrop list
//!   --json | jq` has to work, and progress chatter must not corrupt it.
//! * **Colour is opt-out, not opt-in.** It is on when stdout is a terminal,
//!   off when it is a pipe or a file, and `NO_COLOR` always wins. `--color`
//!   overrides both.

use std::io::{IsTerminal, Write};

/// Whether to colourise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

/// How much to say.
#[derive(Debug, Clone, Copy)]
pub struct Ui {
    color: bool,
    quiet: bool,
    verbose: u8,
    /// Whether machine-readable output was asked for.
    pub json: bool,
}

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const BLUE: &str = "\x1b[34m";
const CYAN: &str = "\x1b[36m";

impl Ui {
    pub fn new(choice: ColorChoice, quiet: bool, verbose: u8, json: bool) -> Self {
        let color = match choice {
            ColorChoice::Always => true,
            ColorChoice::Never => false,
            ColorChoice::Auto => {
                // https://no-color.org: any value, including the empty string,
                // means "do not colourise".
                std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
            }
        };
        Ui {
            color,
            quiet,
            verbose,
            json,
        }
    }

    pub fn verbose(&self) -> bool {
        self.verbose > 0
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("{code}{text}{RESET}")
        } else {
            text.to_string()
        }
    }

    // ------------------------------------------------------------- to stdout

    /// A line of primary output.
    pub fn out(&self, text: &str) {
        if self.json {
            // Structured output is the whole result; nothing else may share it.
            return;
        }
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{text}");
        let _ = stdout.flush();
    }

    /// Raw output that is not suppressed by `--json` (the JSON itself).
    pub fn raw(&self, text: &str) {
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{text}");
        let _ = stdout.flush();
    }

    /// A section heading.
    pub fn heading(&self, text: &str) {
        self.out("");
        self.out(&self.paint(BOLD, text));
    }

    /// A `label: value` line, aligned.
    pub fn kv(&self, label: &str, value: &str) {
        self.out(&format!(
            "  {}{}",
            self.paint(CYAN, &format!("{label:<14}")),
            value
        ));
    }

    /// A bulleted line.
    pub fn bullet(&self, text: &str) {
        self.out(&format!("  • {text}"));
    }

    // ------------------------------------------------------------- to stderr

    /// Progress and other chatter, suppressed by `--quiet` and `--json`.
    pub fn status(&self, text: &str) {
        if self.quiet || self.json {
            return;
        }
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "{}", self.paint(DIM, text));
    }

    /// A step that succeeded.
    pub fn success(&self, text: &str) {
        if self.quiet {
            return;
        }
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "{} {text}", self.paint(GREEN, "✓"));
    }

    /// Something the user should know about but which did not stop anything.
    pub fn warn(&self, text: &str) {
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "{} {text}", self.paint(YELLOW, "!"));
    }

    /// A failure.
    pub fn error(&self, text: &str) {
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "{} {text}", self.paint(RED, "error:"));
    }

    /// A suggestion printed under an error.
    pub fn hint(&self, text: &str) {
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "  {}", self.paint(BLUE, text));
    }

    /// Emit a JSON value, or an error if it cannot be serialised.
    pub fn json<T: serde::Serialize>(&self, value: &T) -> windrop_core::Result<()> {
        self.raw(&serde_json::to_string_pretty(value)?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_colour_choices_are_honoured() {
        assert!(Ui::new(ColorChoice::Always, false, 0, false).color);
        assert!(!Ui::new(ColorChoice::Never, false, 0, false).color);
    }

    #[test]
    fn painted_text_keeps_the_underlying_string() {
        let plain = Ui::new(ColorChoice::Never, false, 0, false);
        assert_eq!(plain.paint(RED, "boom"), "boom");
        let loud = Ui::new(ColorChoice::Always, false, 0, false);
        assert_eq!(loud.paint(RED, "boom"), "\x1b[31mboom\x1b[0m");
    }

    #[test]
    fn no_color_environment_variable_disables_automatic_colour() {
        // The variable only matters for `auto`, which also needs a terminal, so
        // a test cannot assert the "on" case portably. It can assert the "off"
        // case, which is the one that matters for scripts.
        // SAFETY: single-threaded within this test; `std::env` is process-global
        // but no other test in this crate reads NO_COLOR.
        std::env::set_var("NO_COLOR", "1");
        assert!(!Ui::new(ColorChoice::Auto, false, 0, false).color);
        std::env::remove_var("NO_COLOR");
    }
}
