//! The on-disk layout of WinDrop.
//!
//! Everything WinDrop creates for an application lives under
//! `<data_dir>/apps/<app-id>/`. Shared, version-pinned runtimes live under
//! `<data_dir>/runtime/`. Nothing is written outside these directories and the
//! handful of XDG locations needed for menu integration.
//!
//! ```text
//! ~/.local/share/windrop/
//! ├── apps/
//! │   └── <app-id>/
//! │       ├── metadata.json
//! │       ├── profile.json
//! │       ├── icon.png
//! │       └── prefix/            <- WINEPREFIX, self-contained
//! │           └── drive_c/...
//! ├── runtime/
//! │   ├── wine/<version>/        <- wine builds
//! │   ├── dxvk/<version>/
//! │   └── vkd3d-proton/<version>/
//! ├── profiles.db               <- SQLite compatibility database
//! ├── logs/windrop.log
//! └── downloads/                <- cached downloads (safe to delete)
//! ```

use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Environment variable that overrides the data directory. Honoured by the
/// CLI (`--data-dir`), the test-suite, and power users.
pub const DATA_DIR_ENV: &str = "WINDROP_DATA_DIR";

/// Resolved locations for every piece of WinDrop state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    data_dir: PathBuf,
    config_dir: PathBuf,
    cache_dir: PathBuf,
    applications_dir: PathBuf,
}

impl Paths {
    /// Build the layout for an explicit data directory (used by `--data-dir`
    /// and by tests, which point it at a temporary directory).
    ///
    /// A *relocated* installation — anything other than the default — keeps its
    /// configuration and cache inside the data directory it was given. That is
    /// what makes a portable installation portable: the whole thing can be
    /// moved, copied to a USB stick, or deleted as one unit, and
    /// `--data-dir /tmp/scratch` cannot end up rewriting the settings of the
    /// installation the user actually uses.
    ///
    /// The default installation instead follows XDG, which is where every other
    /// application keeps its configuration.
    pub fn with_data_dir(data_dir: impl Into<PathBuf>) -> Self {
        let data_dir = data_dir.into();
        let default_data_dir = xdg_dir("XDG_DATA_HOME", ".local/share").join("windrop");
        let relocated = data_dir != default_data_dir;

        let (config_dir, cache_dir) = if relocated {
            (data_dir.join("config"), data_dir.join("cache"))
        } else {
            (
                xdg_dir("XDG_CONFIG_HOME", ".config").join("windrop"),
                xdg_dir("XDG_CACHE_HOME", ".cache").join("windrop"),
            )
        };
        let applications_dir = xdg_dir("XDG_DATA_HOME", ".local/share").join("applications");
        Paths {
            data_dir,
            config_dir,
            cache_dir,
            applications_dir,
        }
    }

    /// Redirect the applications directory, where `.desktop` files are written.
    ///
    /// The default is the user's real `~/.local/share/applications`, because
    /// that is what makes menu integration work. Pointing it elsewhere is useful
    /// for a test run or a portable installation that must not touch the user's
    /// menu.
    pub fn with_applications_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.applications_dir = dir.into();
        self
    }

    /// The data directory, when it is not the one WinDrop would pick by default.
    ///
    /// `None` means the default, which lets a launcher command stay a plain
    /// `windrop launch <id>`. When WinDrop lives somewhere else the launcher has
    /// to say so: a menu entry is started by the desktop environment, which
    /// inherits no `WINDROP_DATA_DIR` from the shell that installed the
    /// application, and would otherwise look for it in an empty directory.
    pub fn non_default_data_dir(&self) -> Option<PathBuf> {
        let default = xdg_dir("XDG_DATA_HOME", ".local/share").join("windrop");
        if self.data_dir == default {
            None
        } else {
            Some(self.data_dir.clone())
        }
    }

    /// A layout whose every location lives under one directory.
    ///
    /// Nothing outside the returned root is written to. This is what the
    /// test-suite uses, and what a portable or experimental installation wants.
    pub fn isolated(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Paths {
            data_dir: root.clone(),
            config_dir: root.join("config"),
            cache_dir: root.join("cache"),
            applications_dir: root.join("applications"),
        }
    }

    /// Discover the layout from the environment: `$WINDROP_DATA_DIR`, then
    /// `$XDG_DATA_HOME/windrop`, then `~/.local/share/windrop`.
    pub fn discover() -> Result<Self> {
        if let Some(dir) = std::env::var_os(DATA_DIR_ENV) {
            if !dir.is_empty() {
                return Ok(Paths::with_data_dir(PathBuf::from(dir)));
            }
        }
        Ok(Paths::with_data_dir(
            xdg_dir("XDG_DATA_HOME", ".local/share").join("windrop"),
        ))
    }

    /// Root of all WinDrop state.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// `~/.local/share/applications`, where per-user `.desktop` files belong.
    pub fn applications_dir(&self) -> &Path {
        &self.applications_dir
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.json")
    }

    pub fn database(&self) -> PathBuf {
        self.data_dir.join("profiles.db")
    }

    pub fn apps_dir(&self) -> PathBuf {
        self.data_dir.join("apps")
    }

    pub fn app_dir(&self, id: &str) -> PathBuf {
        self.apps_dir().join(id)
    }

    pub fn runtime_dir(&self) -> PathBuf {
        self.data_dir.join("runtime")
    }

    pub fn wine_dir(&self) -> PathBuf {
        self.runtime_dir().join("wine")
    }

    pub fn dxvk_dir(&self) -> PathBuf {
        self.runtime_dir().join("dxvk")
    }

    pub fn vkd3d_dir(&self) -> PathBuf {
        self.runtime_dir().join("vkd3d-proton")
    }

    pub fn downloads_dir(&self) -> PathBuf {
        self.data_dir.join("downloads")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }

    pub fn log_file(&self) -> PathBuf {
        self.logs_dir().join("windrop.log")
    }

    /// The `.desktop` file WinDrop installed for `id`, if any.
    pub fn desktop_file_for(&self, id: &str) -> PathBuf {
        self.applications_dir.join(desktop_file_name(id))
    }

    /// Create every directory WinDrop needs. Safe to call repeatedly.
    pub fn ensure(&self) -> Result<()> {
        for dir in [
            self.data_dir.clone(),
            self.apps_dir(),
            self.runtime_dir(),
            self.wine_dir(),
            self.dxvk_dir(),
            self.vkd3d_dir(),
            self.downloads_dir(),
            self.logs_dir(),
            self.config_dir.clone(),
            self.cache_dir.clone(),
        ] {
            std::fs::create_dir_all(&dir).map_err(Error::Io)?;
        }
        Ok(())
    }
}

/// `org.windrop.WinDrop.<id>.desktop`.
pub fn desktop_file_name(id: &str) -> String {
    format!("org.windrop.WinDrop.{id}.desktop")
}

/// `org.windrop.WinDrop.<id>` — the XDG desktop-entry id used by
/// `xdg-desktop-menu install`, which expects a filename *without* the
/// `.desktop` suffix.
pub fn desktop_entry_id(id: &str) -> String {
    format!("org.windrop.WinDrop.{id}")
}

fn xdg_dir(var: &str, fallback_relative: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home_dir().join(fallback_relative),
    }
}

/// Best-effort home directory lookup that does not panic when `$HOME` is unset.
pub fn home_dir() -> PathBuf {
    if let Some(h) = std::env::var_os("HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    // Fall back to passwd entry via libc, then to a relative ".", so that
    // discovery never panics in a stripped container.
    // SAFETY: getpwuid returns a pointer into a static buffer owned by libc;
    // we read it immediately and copy the string out.
    unsafe {
        let uid = libc::getuid();
        let pw = libc::getpwuid(uid);
        if !pw.is_null() {
            let dir = (*pw).pw_dir;
            if !dir.is_null() {
                let bytes = std::ffi::CStr::from_ptr(dir).to_bytes();
                if !bytes.is_empty() {
                    use std::os::unix::ffi::OsStrExt;
                    return PathBuf::from(std::ffi::OsStr::from_bytes(bytes));
                }
            }
        }
    }
    PathBuf::from(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_self_contained_under_one_data_dir() {
        let p = Paths::with_data_dir("/tmp/wd");
        assert_eq!(p.app_dir("notepad"), PathBuf::from("/tmp/wd/apps/notepad"));
        assert_eq!(p.wine_dir(), PathBuf::from("/tmp/wd/runtime/wine"));
        assert_eq!(p.database(), PathBuf::from("/tmp/wd/profiles.db"));
        // Every app artefact must live under data_dir.
        assert!(p.app_dir("x").starts_with(p.data_dir()));
        assert!(p.downloads_dir().starts_with(p.data_dir()));
    }

    #[test]
    fn desktop_entry_ids_are_reverse_dns_and_stable() {
        assert_eq!(desktop_entry_id("notepad"), "org.windrop.WinDrop.notepad");
        assert_eq!(
            desktop_file_name("notepad"),
            "org.windrop.WinDrop.notepad.desktop"
        );
    }

    #[test]
    fn an_isolated_layout_keeps_everything_under_one_root() {
        let p = Paths::isolated("/tmp/portable");
        assert_eq!(p.data_dir(), Path::new("/tmp/portable"));
        assert_eq!(p.config_dir(), Path::new("/tmp/portable/config"));
        assert_eq!(p.cache_dir(), Path::new("/tmp/portable/cache"));
        assert_eq!(
            p.applications_dir(),
            Path::new("/tmp/portable/applications")
        );
        // The desktop entry must not escape either.
        assert!(p.desktop_file_for("app").starts_with("/tmp/portable"));
    }

    #[test]
    fn the_applications_directory_can_be_redirected() {
        let p = Paths::with_data_dir("/tmp/wd").with_applications_dir("/tmp/menu");
        assert_eq!(p.applications_dir(), Path::new("/tmp/menu"));
        assert_eq!(
            p.desktop_file_for("x"),
            PathBuf::from("/tmp/menu/org.windrop.WinDrop.x.desktop")
        );
        // The rest of the layout is unaffected.
        assert_eq!(p.data_dir(), Path::new("/tmp/wd"));
    }

    #[test]
    fn configuration_stays_inside_a_relocated_data_directory() {
        // The point of `--data-dir` is that it changes where everything goes.
        // Leaving configuration behind in the user's real home would let a
        // throwaway run rewrite the settings of their actual installation.
        let relocated = Paths::with_data_dir("/tmp/portable-windrop");
        assert!(relocated.config_dir().starts_with("/tmp/portable-windrop"));
        assert!(relocated.cache_dir().starts_with("/tmp/portable-windrop"));
        assert!(relocated.config_file().starts_with("/tmp/portable-windrop"));

        // The default installation still follows XDG.
        let default =
            Paths::with_data_dir(xdg_dir("XDG_DATA_HOME", ".local/share").join("windrop"));
        assert_eq!(
            default.config_dir(),
            xdg_dir("XDG_CONFIG_HOME", ".config")
                .join("windrop")
                .as_path()
        );
    }

    #[test]
    fn a_relocated_data_directory_is_reported_so_launchers_can_name_it() {
        // A test or portable layout is never the default, so it has to be named
        // explicitly for a menu entry to find it again.
        assert_eq!(
            Paths::isolated("/tmp/portable").non_default_data_dir(),
            Some(PathBuf::from("/tmp/portable"))
        );
        let default =
            Paths::with_data_dir(xdg_dir("XDG_DATA_HOME", ".local/share").join("windrop"));
        assert_eq!(default.non_default_data_dir(), None);
    }

    #[test]
    fn ensure_creates_the_full_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::with_data_dir(tmp.path());
        p.ensure().unwrap();
        for d in [p.apps_dir(), p.runtime_dir(), p.wine_dir(), p.logs_dir()] {
            assert!(d.is_dir(), "{d:?} should exist");
        }
        // Idempotent.
        p.ensure().unwrap();
    }
}
