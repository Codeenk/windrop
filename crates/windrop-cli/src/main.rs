//! `windrop` — the command-line half of WinDrop.
//!
//! The binary has two jobs. It is the way to drive WinDrop from a terminal, and
//! — because it is small, has no GUI toolkit linked into it, and works over
//! SSH — it is also what every menu entry WinDrop writes calls back into:
//!
//! ```ini
//! Exec=windrop launch notepadpp
//! ```
//!
//! All the work lives in `windrop-core`; this crate is argument parsing, output
//! formatting and process exit codes.

mod cli;
mod commands;
mod settings;
mod ui;

use clap::Parser;

fn main() {
    let parsed = cli::Cli::parse();
    // `run` owns the logging guard, so it is dropped — and the log flushed —
    // before the process ends.
    let code = commands::run(parsed);
    std::process::exit(code);
}
