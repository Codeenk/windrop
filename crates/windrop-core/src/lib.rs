//! # WinDrop Core
//!
//! The engine behind WinDrop: it inspects Windows executables, resolves a
//! compatibility profile for them, builds a self-contained Wine environment,
//! runs the installer inside it, and records the result so the application can
//! later be launched or removed with one click.
//!
//! The crate is deliberately UI-free. Both `windrop` (CLI) and `windrop-gui`
//! are thin shells over it, and every expensive decision is expressed as pure
//! data ([`process::CommandSpec`], [`compat::AppProfile`],
//! [`runtime::RuntimeEnv`]) so it can be tested without Wine being installed.
//!
//! ## Pipeline
//!
//! ```text
//! .exe ──▶ compat::inspect ──▶ AppProfile ──▶ runtime::EnvironmentBuilder
//!                                        │                    │
//!                                        │                    ▼
//!                                        │            CommandSpec (wine + prefix
//!                                        │            + DXVK + bwrap)
//!                                        ▼                    │
//!                              db::ProfileDb (SQLite)         │
//!                                                             ▼
//!                                        fallback::run_chain ──▶ installed exe
//!                                                             │
//!                                        manager::ApplicationManager
//!                                          ├── metadata.json
//!                                          ├── .desktop + menu entry
//!                                          └── icon.png
//! ```

pub mod compat;
pub mod config;
pub mod db;
pub mod desktop;
pub mod doctor;
pub mod error;
pub mod fallback;
pub mod fixtures;
pub mod icons;
pub mod logging;
pub mod manager;
pub mod paths;
pub mod process;
pub mod registry;
pub mod runtime;
pub mod text;
pub mod updater;

pub use config::Config;
pub use error::{Error, Result};
pub use paths::Paths;

/// The crate version, taken from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Version string, for `--version` output and the About pane.
pub fn version() -> &'static str {
    VERSION
}

/// The reverse-DNS namespace used for desktop entries and D-Bus names.
pub const APP_ID: &str = "org.windrop.WinDrop";
