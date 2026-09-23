//! Wine prefix layout and path translation.
//!
//! A prefix is a self-contained Windows filesystem. Living inside it means
//! everything an application writes — registry, caches, saved games, temp
//! files — ends up under the application's own directory.
//!
//! ```text
//! <app>/prefix/
//! ├── drive_c/
//! │   ├── Program Files/
//! │   ├── windows/system32/        <- DXVK dlls land here (64-bit)
//! │   ├── windows/syswow64/        <- and here (32-bit)
//! │   └── users/<user>/
//! ├── dosdevices/                  <- drive letters, including Z: -> /
//! └── winetricks.log
//! ```
//!
//! Wine maps `Z:\` to the real filesystem root, which is why path translation
//! has to handle more than just `C:`.

use std::path::{Path, PathBuf};

use crate::Result;

/// Paths inside one application's Wine prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixPaths {
    root: PathBuf,
}

impl PrefixPaths {
    /// `<app_dir>/prefix`.
    pub fn new(app_dir: impl Into<PathBuf>) -> Self {
        PrefixPaths {
            root: app_dir.into().join("prefix"),
        }
    }

    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        PrefixPaths { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn drive_c(&self) -> PathBuf {
        self.root.join("drive_c")
    }

    pub fn windows_dir(&self) -> PathBuf {
        self.drive_c().join("windows")
    }

    /// 64-bit system directory.
    pub fn system32(&self) -> PathBuf {
        self.windows_dir().join("system32")
    }

    /// 32-bit system directory, as seen from a 64-bit prefix.
    pub fn syswow64(&self) -> PathBuf {
        self.windows_dir().join("syswow64")
    }

    pub fn users_dir(&self) -> PathBuf {
        self.drive_c().join("users")
    }

    /// The current user's profile inside the prefix.
    pub fn user_dir(&self, user: &str) -> PathBuf {
        self.users_dir().join(user)
    }

    pub fn dosdevices(&self) -> PathBuf {
        self.root.join("dosdevices")
    }

    /// A private `$HOME` for the application.
    ///
    /// Wine applications that ignore `WINEPREFIX` and write to `$HOME` still
    /// stay inside the application directory, and inside the sandbox `$HOME` is
    /// redirected here.
    pub fn sandbox_home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// `dxvk.conf` for this prefix.
    pub fn dxvk_conf(&self) -> PathBuf {
        self.root.join("dxvk.conf")
    }

    /// Where `winetricks` records the verbs it has installed.
    pub fn winetricks_log(&self) -> PathBuf {
        self.root.join("winetricks.log")
    }

    /// The `system.reg` file, whose presence means the prefix was created.
    pub fn system_reg(&self) -> PathBuf {
        self.root.join("system.reg")
    }

    /// A prefix only counts as usable once Wine has written its registry.
    pub fn is_initialised(&self) -> bool {
        self.system_reg().is_file()
    }

    pub fn exists(&self) -> bool {
        self.root.is_dir()
    }

    /// Translate a host path into the Windows path an application would see.
    pub fn windows_path_of(&self, host: &Path) -> Option<String> {
        windows_path_of(&self.root, host)
    }

    /// Translate a Windows path into a host path.
    pub fn host_path_of(&self, windows: &str) -> Option<PathBuf> {
        host_path_of(&self.root, windows)
    }

    /// Bytes used on disk, for the application list.
    pub fn size_on_disk(&self) -> u64 {
        directory_size(&self.root)
    }
}

/// Total size of a directory tree, tolerating permission errors.
pub fn directory_size(dir: &Path) -> u64 {
    walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

fn to_windows_separators(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\")
}

/// Host path to Windows path.
///
/// Anything under `prefix/drive_c` becomes `C:\...`; any other absolute path
/// becomes `Z:\...`, matching Wine's own mapping of `Z:` to `/`.
pub fn windows_path_of(prefix_root: &Path, host: &Path) -> Option<String> {
    let drive_c = prefix_root.join("drive_c");
    if let Ok(rel) = host.strip_prefix(&drive_c) {
        let rel = to_windows_separators(rel);
        return Some(if rel.is_empty() {
            "C:\\".to_string()
        } else {
            format!("C:\\{rel}")
        });
    }
    if !host.is_absolute() {
        return None;
    }
    Some(format!("Z:{}", to_windows_separators(host)))
}

/// Windows path to host path.
///
/// `C:` resolves inside the prefix, `Z:` to the filesystem root, and anything
/// else through `dosdevices`, which is where Wine keeps its drive symlinks.
pub fn host_path_of(prefix_root: &Path, windows: &str) -> Option<PathBuf> {
    let (drive, rest) = windows.split_once(':')?;
    let rest = rest.trim_start_matches(['\\', '/']);
    let relative = rest.replace('\\', "/");
    let drive = drive.to_ascii_lowercase();

    Some(match drive.as_str() {
        "c" => prefix_root.join("drive_c").join(relative),
        "z" => PathBuf::from("/").join(relative),
        other => prefix_root.join("dosdevices").join(other).join(relative),
    })
}

/// Create the directory skeleton for a fresh prefix.
pub fn prepare_directories(prefix: &PrefixPaths) -> Result<()> {
    for dir in [
        prefix.root().to_path_buf(),
        prefix.drive_c(),
        prefix.sandbox_home(),
        prefix.dosdevices(),
    ] {
        std::fs::create_dir_all(&dir)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefix() -> PrefixPaths {
        PrefixPaths::new("/data/apps/notepad")
    }

    #[test]
    fn layout_is_derived_from_the_application_directory() {
        let p = prefix();
        assert_eq!(p.root(), Path::new("/data/apps/notepad/prefix"));
        assert_eq!(p.drive_c(), Path::new("/data/apps/notepad/prefix/drive_c"));
        assert_eq!(
            p.system32(),
            Path::new("/data/apps/notepad/prefix/drive_c/windows/system32")
        );
        assert_eq!(
            p.syswow64(),
            Path::new("/data/apps/notepad/prefix/drive_c/windows/syswow64")
        );
        assert_eq!(
            p.user_dir("windrop"),
            Path::new("/data/apps/notepad/prefix/drive_c/users/windrop")
        );
        assert_eq!(
            p.dxvk_conf(),
            Path::new("/data/apps/notepad/prefix/dxvk.conf")
        );
    }

    #[test]
    fn a_prefix_is_only_initialised_once_the_registry_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = PrefixPaths::from_root(tmp.path().join("prefix"));
        prepare_directories(&prefix).unwrap();
        assert!(prefix.exists());
        assert!(!prefix.is_initialised());

        std::fs::write(prefix.system_reg(), "WINE REGISTRY Version 2\n").unwrap();
        assert!(prefix.is_initialised());
    }

    #[test]
    fn drive_c_paths_become_c_drive_windows_paths() {
        let p = prefix();
        let host = p
            .drive_c()
            .join("Program Files")
            .join("App")
            .join("app.exe");
        assert_eq!(
            p.windows_path_of(&host).unwrap(),
            r"C:\Program Files\App\app.exe"
        );
    }

    #[test]
    fn the_prefix_root_itself_maps_to_the_c_drive_root() {
        let p = prefix();
        assert_eq!(p.windows_path_of(&p.drive_c()).unwrap(), "C:\\");
    }

    #[test]
    fn external_paths_become_z_drive_paths() {
        let p = prefix();
        assert_eq!(
            p.windows_path_of(Path::new("/home/u/Downloads/setup.exe"))
                .unwrap(),
            r"Z:\home\u\Downloads\setup.exe"
        );
    }

    #[test]
    fn relative_paths_cannot_be_translated() {
        let p = prefix();
        assert!(p.windows_path_of(Path::new("relative/setup.exe")).is_none());
    }

    #[test]
    fn windows_paths_translate_back_to_host_paths() {
        let p = prefix();
        assert_eq!(
            p.host_path_of(r"C:\Program Files\App\app.exe").unwrap(),
            p.drive_c().join("Program Files/App/app.exe")
        );
        assert_eq!(
            p.host_path_of(r"Z:\usr\bin").unwrap(),
            PathBuf::from("/usr/bin")
        );
        // Other drives go through dosdevices, where Wine keeps its symlinks.
        assert_eq!(
            p.host_path_of(r"D:\Games").unwrap(),
            p.dosdevices().join("d").join("Games")
        );
    }

    #[test]
    fn translation_round_trips() {
        let p = prefix();
        for original in [
            p.drive_c().join("windows/system32/d3d11.dll"),
            p.drive_c().join("Program Files (x86)/Old Game/game.exe"),
            PathBuf::from("/home/u/Desktop/installer.msi"),
        ] {
            let windows = p.windows_path_of(&original).expect("should translate");
            let back = p.host_path_of(&windows).expect("should translate back");
            assert_eq!(back, original, "round trip failed through {windows}");
        }
    }

    #[test]
    fn windows_paths_are_case_insensitive_about_the_drive_letter() {
        let p = prefix();
        assert_eq!(
            p.host_path_of(r"c:\windows").unwrap(),
            p.drive_c().join("windows")
        );
        assert_eq!(
            p.host_path_of(r"C:\windows").unwrap(),
            p.drive_c().join("windows")
        );
    }

    #[test]
    fn malformed_windows_paths_return_none_instead_of_panicking() {
        let p = prefix();
        assert!(p.host_path_of("no-drive").is_none());
        assert!(p.host_path_of("").is_none());
    }

    #[test]
    fn paths_with_spaces_survive_translation() {
        let p = prefix();
        let host = p
            .drive_c()
            .join("Program Files (x86)")
            .join("My App")
            .join("a b.exe");
        let windows = p.windows_path_of(&host).unwrap();
        assert_eq!(windows, r"C:\Program Files (x86)\My App\a b.exe");
        assert_eq!(p.host_path_of(&windows).unwrap(), host);
    }

    #[test]
    fn size_on_disk_sums_file_lengths() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = PrefixPaths::from_root(tmp.path().join("prefix"));
        prepare_directories(&prefix).unwrap();
        std::fs::write(prefix.drive_c().join("a.bin"), vec![0u8; 1000]).unwrap();
        std::fs::write(prefix.root().join("b.bin"), vec![0u8; 500]).unwrap();
        assert_eq!(prefix.size_on_disk(), 1500);
    }

    #[test]
    fn size_of_a_missing_directory_is_zero() {
        assert_eq!(directory_size(Path::new("/definitely/not/here")), 0);
    }
}
