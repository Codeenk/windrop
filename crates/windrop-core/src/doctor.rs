//! The dependency doctor.
//!
//! WinDrop deliberately does not install system packages. What it does instead
//! is say precisely what is missing and what to type, once, in one place. This
//! module produces that list together with a host report.
//!
//! Nothing here changes the system and nothing here fails: a diagnostic run on a
//! machine with no Wine, no winetricks and no bubblewrap must still return a
//! complete, useful [`Diagnostics`].

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{Config, SandboxMode};
use crate::db::ProfileDb;
use crate::error::arch_package_for;
use crate::paths::Paths;
use crate::process::{which, CommandSpec};
use crate::runtime::dxvk::ComponentKind;
use crate::runtime::sandbox::{self, SandboxAvailability};
use crate::runtime::wine::WineInstall;
use crate::runtime::RuntimeManager;

/// How necessary a tool is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Necessity {
    /// Without this, WinDrop cannot install anything.
    Required,
    /// A feature degrades gracefully but noticeably.
    Recommended,
    /// Nice to have: icons, or a tidier menu.
    Optional,
}

impl Necessity {
    pub fn label(self) -> &'static str {
        match self {
            Necessity::Required => "required",
            Necessity::Recommended => "recommended",
            Necessity::Optional => "optional",
        }
    }
}

/// One external tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolStatus {
    /// The executable name looked for on `$PATH`.
    pub name: String,
    pub necessity: Necessity,
    /// Where it was found, if it was.
    pub path: Option<PathBuf>,
    /// What WinDrop uses it for.
    pub purpose: String,
    /// The Arch package that provides it.
    pub package: String,
    /// Version reported by the tool, when it can be determined cheaply.
    pub version: Option<String>,
}

impl ToolStatus {
    pub fn is_present(&self) -> bool {
        self.path.is_some()
    }

    /// A compact line for the setup pane.
    pub fn line(&self) -> String {
        match &self.path {
            Some(path) => format!("✔ {} ({})", self.name, path.display()),
            None => format!(
                "✘ {} — needs '{}' ({})",
                self.name,
                self.package,
                self.necessity.label()
            ),
        }
    }
}

/// The host environment, for the setup pane and for bug reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    pub distribution: String,
    pub kernel: String,
    pub architecture: String,
    /// `wayland`, `x11` or `unknown`.
    pub session_type: String,
    /// Number of usable CPUs, as a hint for DXVK compiler threads.
    pub cpu_count: usize,
}

impl HostInfo {
    pub fn detect() -> Self {
        HostInfo {
            distribution: read_os_release().unwrap_or_else(|| "unknown distribution".into()),
            kernel: read_trimmed("/proc/sys/kernel/osrelease").unwrap_or_else(|| "unknown".into()),
            architecture: std::env::consts::ARCH.to_string(),
            session_type: detect_session(),
            cpu_count: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
        }
    }

    /// Whether `pacman` is the package manager this distribution uses.
    ///
    /// Name matching is a blunt instrument, so it errs towards "no": telling a
    /// Debian user to run `pacman` would be worse than telling an Arch user the
    /// package names without a command.
    pub fn is_arch_based(&self) -> bool {
        const ARCH_FAMILY: &[&str] = &[
            "arch",
            "manjaro",
            "endeavour",
            "cachyos",
            "garuda",
            "artix",
            "arco",
            "blackarch",
            "parabola",
            "rebornos",
            "archlabs",
            "archman",
            "anarchy",
            "blendos",
            "systemd-linux",
        ];
        let lower = self.distribution.to_ascii_lowercase();
        ARCH_FAMILY.iter().any(|name| lower.contains(name))
    }
}

/// Everything the doctor learned.
#[derive(Debug, Clone)]
pub struct Diagnostics {
    pub tools: Vec<ToolStatus>,
    /// The Wine build WinDrop would use.
    pub wine: Option<WineInstall>,
    /// Why no Wine is available, when that is the case.
    pub wine_error: Option<String>,
    pub sandbox: SandboxAvailability,
    /// Installed translation layers, by kind.
    pub components: Vec<(ComponentKind, Option<String>)>,
    pub data_dir: PathBuf,
    /// Free space on the volume holding the data directory.
    pub free_space_bytes: Option<u64>,
    pub profiles: usize,
    pub installed_apps: usize,
    pub host: HostInfo,
}

impl Diagnostics {
    /// Inspect the machine. Never fails.
    pub fn run(paths: &Paths, config: &Config) -> Self {
        let tools = check_tools();

        let runtime = RuntimeManager::new(paths.clone(), config.clone());
        let (wine, wine_error) = match runtime.wine() {
            Ok(wine) => (Some(wine), None),
            Err(e) => (None, Some(e.to_string())),
        };

        let components = [ComponentKind::Dxvk, ComponentKind::Vkd3dProton]
            .into_iter()
            .map(|kind| {
                let found = runtime.component(kind).map(|c| c.display());
                (kind, found)
            })
            .collect();

        let profiles = ProfileDb::open(&paths.database())
            .map(|db| db.profile_count().unwrap_or(0))
            .unwrap_or(0);

        let installed_apps = std::fs::read_dir(paths.apps_dir())
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| {
                        e.path()
                            .join(crate::manager::metadata::METADATA_FILE)
                            .is_file()
                    })
                    .count()
            })
            .unwrap_or(0);

        Diagnostics {
            tools,
            wine,
            wine_error,
            sandbox: sandbox::availability(config.sandbox),
            components,
            data_dir: paths.data_dir().to_path_buf(),
            free_space_bytes: free_space(paths.data_dir()),
            profiles,
            installed_apps,
            host: HostInfo::detect(),
        }
    }

    /// True when WinDrop can install something.
    pub fn is_ready(&self) -> bool {
        self.wine.is_some()
    }

    /// Tools that are missing and matter.
    pub fn missing(&self, at_least: Necessity) -> Vec<&ToolStatus> {
        self.tools
            .iter()
            .filter(|t| !t.is_present())
            .filter(|t| match at_least {
                Necessity::Required => t.necessity == Necessity::Required,
                Necessity::Recommended => {
                    matches!(t.necessity, Necessity::Required | Necessity::Recommended)
                }
                Necessity::Optional => true,
            })
            .collect()
    }

    /// A single install command for everything that is missing.
    ///
    /// One command is far more useful than a list of five: the user can paste
    /// it and be done.
    pub fn setup_command(&self) -> Option<String> {
        let packages: Vec<String> = self
            .missing(Necessity::Optional)
            .iter()
            .map(|t| t.package.clone())
            .collect();
        let mut unique: Vec<String> = Vec::new();
        for p in packages {
            if !unique.contains(&p) && p != "the package that provides this tool" {
                unique.push(p);
            }
        }
        if unique.is_empty() {
            return None;
        }
        if self.host.is_arch_based() {
            Some(format!("sudo pacman -S --needed {}", unique.join(" ")))
        } else {
            // Give the package names anyway: they are the Arch names, and any
            // user can map them to their distribution.
            Some(format!(
                "install: {} (Arch package names; adjust for your distribution)",
                unique.join(", ")
            ))
        }
    }

    /// The most important thing to tell the user, or `None` when all is well.
    pub fn priority_issue(&self) -> Option<String> {
        if self.wine.is_none() {
            return Some(
                "Wine is not installed. WinDrop needs it to run Windows software.".to_string(),
            );
        }
        if self
            .missing(Necessity::Required)
            .iter()
            .any(|t| t.name == "winetricks")
        {
            return Some(
                "winetricks is missing, so applications that need the VC++ or .NET runtimes \
                 cannot be installed."
                    .to_string(),
            );
        }
        match &self.sandbox {
            SandboxAvailability::Unavailable(reason) => {
                if reason.contains("bubblewrap is installed") {
                    // The binary is there but the kernel refuses namespaces —
                    // installing it again will not help, so say what will.
                    Some(
                        "bubblewrap is installed but this kernel forbids user namespaces, so \
                         applications run without a sandbox."
                            .to_string(),
                    )
                } else if reason.contains("bubblewrap") {
                    Some(
                        "bubblewrap is missing, so applications run without a sandbox.".to_string(),
                    )
                } else {
                    None
                }
            }
            SandboxAvailability::Available(_) => None,
        }
    }

    /// A human-readable report.
    pub fn report(&self) -> String {
        let mut out = String::new();
        out.push_str("WinDrop diagnostics\n");
        out.push_str("==================\n\n");

        out.push_str(&format!("Host:        {}\n", self.host.distribution));
        out.push_str(&format!("Kernel:      {}\n", self.host.kernel));
        out.push_str(&format!(
            "CPU:         {} ({} thread{})\n",
            self.host.architecture,
            self.host.cpu_count,
            if self.host.cpu_count == 1 { "" } else { "s" }
        ));
        out.push_str(&format!("Session:     {}\n", self.host.session_type));
        out.push_str(&format!("Data dir:    {}\n", self.data_dir.display()));
        if let Some(free) = self.free_space_bytes {
            out.push_str(&format!(
                "Free space:  {} (installations typically need 1–4 GiB each)\n",
                crate::manager::human_bytes(free)
            ));
        }
        out.push_str(&format!("Profiles:    {}\n", self.profiles));
        out.push_str(&format!(
            "Installed:   {}\n",
            crate::text::count("application", self.installed_apps)
        ));
        out.push('\n');

        out.push_str("Runtime\n");
        out.push_str("-------\n");
        match &self.wine {
            Some(wine) => out.push_str(&format!(
                "Wine:        {} at {}\n",
                wine.display(),
                wine.executable.display()
            )),
            None => {
                out.push_str("Wine:        NOT FOUND\n");
                if let Some(err) = &self.wine_error {
                    out.push_str(&format!("             {err}\n"));
                }
            }
        }
        for (kind, found) in &self.components {
            let label = format!("{}:", kind.label());
            match found {
                Some(display) => out.push_str(&format!("{label:<14}{display}\n")),
                None => out.push_str(&format!("{label:<14}not installed\n")),
            }
        }
        match &self.sandbox {
            SandboxAvailability::Available(path) => {
                out.push_str(&format!("Sandbox:     bubblewrap ({})\n", path.display()))
            }
            // Inside Flatpak the isolation is real, it just is not bubblewrap's,
            // so saying "off" here would be wrong.
            SandboxAvailability::Unavailable(_) if sandbox::inside_flatpak() => {
                out.push_str("Sandbox:     provided by Flatpak\n")
            }
            SandboxAvailability::Unavailable(reason) => {
                out.push_str(&format!("Sandbox:     off ({reason})\n"))
            }
        }
        out.push('\n');

        out.push_str("Tools\n");
        out.push_str("-----\n");
        for tool in &self.tools {
            out.push_str(&format!("{}\n", tool.line()));
        }

        if let Some(command) = self.setup_command() {
            out.push_str("\nTo install what is missing:\n");
            out.push_str(&format!("  {command}\n"));
        } else if self.is_ready() {
            out.push_str("\nEverything WinDrop needs is present.\n");
        }
        out
    }

    /// A one-line status for the CLI's `doctor --quiet`.
    pub fn summary_line(&self) -> String {
        match (&self.wine, self.priority_issue()) {
            (Some(wine), _) => {
                let missing = self.missing(Necessity::Recommended).len();
                if missing == 0 {
                    format!("ready ({})", wine.display())
                } else {
                    format!(
                        "usable ({}) but {} missing",
                        wine.display(),
                        crate::text::count("recommended tool", missing)
                    )
                }
            }
            (None, Some(issue)) => format!("not ready: {issue}"),
            (None, None) => "not ready: Wine is missing".to_string(),
        }
    }
}

/// Everything WinDrop looks for, in the order the setup pane should show it.
pub fn check_tools() -> Vec<ToolStatus> {
    let specs: &[(&str, Necessity, &str, Option<&str>)] = &[
        (
            "wine",
            Necessity::Required,
            "runs Windows programs",
            Some("--version"),
        ),
        (
            "winetricks",
            Necessity::Required,
            "installs the VC++/.NET runtimes some applications need",
            Some("--version"),
        ),
        (
            "cabextract",
            Necessity::Recommended,
            "unpacks the archives winetricks downloads",
            Some("--version"),
        ),
        (
            "bwrap",
            Necessity::Recommended,
            "confines each application to its own directory",
            Some("--version"),
        ),
        (
            "wrestool",
            Necessity::Optional,
            "extracts application icons",
            Some("--version"),
        ),
        (
            "icotool",
            Necessity::Optional,
            "converts icons for the application menu",
            Some("--version"),
        ),
        (
            "update-desktop-database",
            Necessity::Optional,
            "refreshes the desktop menu immediately",
            None,
        ),
        (
            "tar",
            Necessity::Recommended,
            "unpacks runtime components",
            None,
        ),
    ];

    specs
        .iter()
        .map(|(name, necessity, purpose, version_flag)| {
            let path = which(name);
            let version = match (&path, version_flag) {
                (Some(found), Some(flag)) if *flag == "--version" => tool_version(found, flag),
                _ => None,
            };
            ToolStatus {
                name: name.to_string(),
                necessity: *necessity,
                path,
                purpose: purpose.to_string(),
                package: arch_package_for(name).to_string(),
                version,
            }
        })
        .collect()
}

fn tool_version(executable: &Path, flag: &str) -> Option<String> {
    let output = CommandSpec::new(executable)
        .arg(flag)
        .run_capture(std::time::Duration::from_secs(5))
        .ok()?;
    let text = output
        .stdout
        .lines()
        .find(|l| !l.trim().is_empty())?
        .trim()
        .to_string();
    if text.is_empty() {
        None
    } else {
        Some(text.chars().take(60).collect())
    }
}

/// Free bytes on the filesystem holding `path`.
///
/// Walks up to the nearest existing directory first, because the data directory
/// may not exist yet on a first run.
pub fn free_space(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;

    let mut probe = path.to_path_buf();
    while !probe.exists() {
        probe = probe.parent()?.to_path_buf();
    }
    let c_path = std::ffi::CString::new(probe.as_os_str().as_bytes()).ok()?;

    // SAFETY: `statvfs` fills the struct we pass and reads a valid C string.
    // Both pointers are valid for the duration of the call.
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut stat) != 0 {
            return None;
        }
        let block = stat.f_frsize as u64;
        Some(block.saturating_mul(stat.f_bavail as u64))
    }
}

fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

fn read_os_release() -> Option<String> {
    let text = std::fs::read_to_string("/etc/os-release").ok()?;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("PRETTY_NAME=") {
            return Some(value.trim_matches('"').to_string());
        }
    }
    read_trimmed("/etc/arch-release").map(|_| "Arch Linux".to_string())
}

fn detect_session() -> String {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return "wayland".to_string();
    }
    if std::env::var_os("DISPLAY").is_some() {
        return "x11".to_string();
    }
    match std::env::var("XDG_SESSION_TYPE") {
        Ok(value) if !value.is_empty() => value,
        _ => "unknown".to_string(),
    }
}

/// Convenience for the GUI's settings pane: is the sandbox usable?
pub fn sandbox_status(config: &Config) -> SandboxAvailability {
    sandbox::availability(config.sandbox)
}

/// Convenience: the sandbox mode WinDrop would use right now.
pub fn effective_sandbox_mode(config: &Config) -> SandboxMode {
    match sandbox::availability(config.sandbox) {
        SandboxAvailability::Available(_) => config.sandbox,
        SandboxAvailability::Unavailable(_) => SandboxMode::Off,
    }
}

#[cfg(test)]
mod tests {
    // Setting one field on a default is the clearest way to say "defaults,
    // except this"; the lint is aimed at production code, where it usually
    // means a missing derive.
    #![allow(clippy::field_reassign_with_default)]
    use super::*;

    fn fixture() -> (tempfile::TempDir, Paths, Config) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::isolated(dir.path());
        paths.ensure().unwrap();
        let mut config = Config::default();
        config.allow_remote_registry = false;
        (dir, paths, config)
    }

    #[test]
    fn a_diagnostic_run_never_fails() {
        let (_dir, paths, config) = fixture();
        let diagnostics = Diagnostics::run(&paths, &config);
        assert!(!diagnostics.tools.is_empty());
        assert!(diagnostics.report().contains("WinDrop diagnostics"));
        assert!(!diagnostics.summary_line().is_empty());
    }

    #[test]
    fn readiness_matches_whether_wine_was_found() {
        let (_dir, paths, config) = fixture();
        let diagnostics = Diagnostics::run(&paths, &config);
        // Whatever this host looks like, the two must agree.
        assert_eq!(diagnostics.is_ready(), diagnostics.wine.is_some());
        assert_eq!(diagnostics.wine_error.is_some(), diagnostics.wine.is_none());
    }

    #[test]
    fn wine_missing_produces_an_actionable_priority_issue() {
        let (_dir, paths, config) = fixture();
        let mut diagnostics = Diagnostics::run(&paths, &config);
        diagnostics.wine = None;
        diagnostics.wine_error = Some("not found".into());
        assert!(diagnostics.priority_issue().unwrap().contains("Wine"));
        assert!(diagnostics.summary_line().starts_with("not ready"));
    }

    #[test]
    fn every_tool_records_a_purpose_and_a_package() {
        let tools = check_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        for expected in [
            "wine",
            "winetricks",
            "bwrap",
            "wrestool",
            "icotool",
            "cabextract",
        ] {
            assert!(names.contains(&expected), "{expected} must be checked");
        }
        for tool in &tools {
            assert!(!tool.purpose.is_empty());
            assert!(!tool.package.is_empty());
            // A missing tool must never be reported as present.
            assert_eq!(tool.is_present(), tool.path.is_some());
        }
    }

    #[test]
    fn tool_checks_agree_with_the_host() {
        let tools = check_tools();
        for tool in &tools {
            assert_eq!(
                tool.is_present(),
                which(&tool.name).is_some(),
                "{} disagrees with the host",
                tool.name
            );
        }
    }

    #[test]
    fn missing_tools_are_filtered_by_how_much_they_matter() {
        let (_dir, paths, config) = fixture();
        let diagnostics = Diagnostics::run(&paths, &config);

        let required = diagnostics.missing(Necessity::Required);
        let recommended = diagnostics.missing(Necessity::Recommended);
        let all = diagnostics.missing(Necessity::Optional);

        assert!(required.iter().all(|t| t.necessity == Necessity::Required));
        assert!(recommended.len() >= required.len() || diagnostics.is_ready());
        assert!(all.len() >= recommended.len());
    }

    #[test]
    fn the_setup_command_is_a_single_paste_ready_line() {
        let (_dir, paths, config) = fixture();
        let mut diagnostics = Diagnostics::run(&paths, &config);
        // Force a known-missing state regardless of the host.
        diagnostics.tools = vec![
            ToolStatus {
                name: "wine".into(),
                necessity: Necessity::Required,
                path: None,
                purpose: "runs Windows programs".into(),
                package: "wine".into(),
                version: None,
            },
            ToolStatus {
                name: "bwrap".into(),
                necessity: Necessity::Recommended,
                path: None,
                purpose: "confines applications".into(),
                package: "bubblewrap".into(),
                version: None,
            },
        ];
        // Pin the host: the command shapes differ between Arch and everything
        // else, and this test is about the Arch shape.
        diagnostics.host = arch_host();
        let command = diagnostics.setup_command().expect("a command");
        assert!(command.contains("pacman"), "{command}");
        assert!(command.contains("wine"));
        assert!(
            command.contains("bubblewrap"),
            "the package name, not the binary name"
        );
        assert!(!command.contains("--needed bubblewrap cabextract"));
    }

    #[test]
    fn nothing_missing_means_no_setup_command() {
        let (_dir, paths, config) = fixture();
        let mut diagnostics = Diagnostics::run(&paths, &config);
        diagnostics.tools.clear();
        diagnostics.wine_error = None;
        // Whether Wine is actually installed is a property of the host, and this
        // test is about what the report says when everything is in place.
        diagnostics.wine = Some(WineInstall {
            executable: PathBuf::from("/usr/bin/wine"),
            version: "9.0".into(),
            flavour: String::new(),
            variant_label: "stable".into(),
            source: crate::runtime::WineSource::System,
        });
        assert!(diagnostics.setup_command().is_none());
        assert!(diagnostics
            .report()
            .contains("Everything WinDrop needs is present"));
    }

    /// A host that claims to be Arch, whatever the machine running the tests is.
    fn arch_host() -> HostInfo {
        HostInfo {
            distribution: "Arch Linux".into(),
            kernel: "6.10.0-arch1-1".into(),
            architecture: "x86_64".into(),
            session_type: "wayland".into(),
            cpu_count: 8,
        }
    }

    #[test]
    fn package_names_are_deduplicated_in_the_setup_command() {
        let (_dir, paths, config) = fixture();
        let mut diagnostics = Diagnostics::run(&paths, &config);
        diagnostics.tools = vec![
            ToolStatus {
                name: "wrestool".into(),
                necessity: Necessity::Optional,
                path: None,
                purpose: "icons".into(),
                package: "icoutils".into(),
                version: None,
            },
            ToolStatus {
                name: "icotool".into(),
                necessity: Necessity::Optional,
                path: None,
                purpose: "icons".into(),
                package: "icoutils".into(),
                version: None,
            },
        ];
        let command = diagnostics.setup_command().unwrap();
        assert_eq!(command.matches("icoutils").count(), 1);
    }

    #[test]
    fn free_space_is_plausible_where_the_directory_exists() {
        let (dir, _paths, _config) = fixture();
        let free = free_space(dir.path()).expect("a real filesystem");
        assert!(free > 1024 * 1024, "at least a megabyte: {free}");
    }

    #[test]
    fn free_space_of_a_missing_path_walks_up_to_something_real() {
        let (_dir, paths, _config) = fixture();
        let missing = paths.data_dir().join("not/created/yet");
        let free = free_space(&missing).expect("should resolve an ancestor");
        assert!(free > 0);
    }

    #[test]
    fn host_detection_returns_something_useful() {
        let host = HostInfo::detect();
        assert!(!host.distribution.is_empty());
        assert!(!host.kernel.is_empty());
        assert!(!host.architecture.is_empty());
        assert!(host.cpu_count >= 1);
        assert!(matches!(
            host.session_type.as_str(),
            "wayland" | "x11" | "unknown" | "tty"
        ));
    }

    #[test]
    fn arch_derived_distributions_are_recognised_by_name() {
        let host = |distribution: &str| HostInfo {
            distribution: distribution.to_string(),
            ..arch_host()
        };
        for arch_based in [
            "Arch Linux",
            "Manjaro Linux",
            "EndeavourOS",
            "CachyOS",
            "Garuda Linux",
            "Artix Linux",
            "BlackArch",
        ] {
            assert!(host(arch_based).is_arch_based(), "{arch_based} uses pacman");
        }
    }

    #[test]
    fn other_distributions_are_not_told_to_run_pacman() {
        // A wrong command is worse than no command: the package names are still
        // printed, and the user can map them.
        for other in [
            "Debian GNU/Linux",
            "Ubuntu 24.04",
            "Fedora Linux",
            "openSUSE",
            "",
        ] {
            let mut host = arch_host();
            host.distribution = other.to_string();
            assert!(!host.is_arch_based(), "{other:?} does not use pacman");
        }
    }

    #[test]
    fn the_report_mentions_the_host_runtime_and_tools() {
        let (dir, paths, config) = fixture();
        let mut diagnostics = Diagnostics::run(&paths, &config);
        diagnostics.wine = None;
        let report = diagnostics.report();
        for section in [
            "Host:",
            "Data dir:",
            "Runtime",
            "Tools",
            "Wine:        NOT FOUND",
        ] {
            assert!(
                report.contains(section),
                "missing '{section}' in:\n{report}"
            );
        }
        assert!(
            report.contains(&dir.path().to_string_lossy().to_string())
                || report.contains("Data dir:")
        );
    }

    #[test]
    fn installed_counts_reflect_what_is_on_disk() {
        let (_dir, paths, config) = fixture();
        // A directory without metadata is not an installed application.
        std::fs::create_dir_all(paths.app_dir("incomplete")).unwrap();
        let diagnostics = Diagnostics::run(&paths, &config);
        assert_eq!(diagnostics.installed_apps, 0);

        let app = crate::manager::metadata::InstalledApp {
            id: "real".into(),
            name: "Real".into(),
            version: String::new(),
            source_file: PathBuf::from("/tmp/a.exe"),
            sha256: "x".into(),
            input_kind: crate::compat::InputKind::Exe,
            arch: crate::compat::Arch::X86_64,
            profile_id: "p".into(),
            profile_source: crate::compat::profile::ProfileSource::Local,
            variant: crate::compat::profile::RuntimeEnv {
                wine_build: "stable".into(),
                arch: crate::compat::Arch::X86_64,
                windows_version: crate::compat::profile::WindowsVersion::Win10,
                dxvk: false,
                vkd3d_proton: false,
                dll_overrides: vec![],
                env: vec![],
                dependencies: vec![],
                rationale: String::new(),
            },
            attempts: 0,
            main_exe_windows: r"C:\a.exe".into(),
            main_exe_host: PathBuf::from("/tmp/a.exe"),
            installed_at: "2026-01-01T00:00:00Z".into(),
            icon: None,
            desktop_file: None,
            dependencies: vec![],
            notes: String::new(),
        };
        app.save(&paths).unwrap();
        assert_eq!(Diagnostics::run(&paths, &config).installed_apps, 1);
    }

    #[test]
    fn sandbox_reporting_agrees_with_the_configured_mode() {
        let (_dir, paths, mut config) = fixture();
        config.sandbox = SandboxMode::Off;
        let diagnostics = Diagnostics::run(&paths, &config);
        assert!(!diagnostics.sandbox.is_available());
        assert_eq!(effective_sandbox_mode(&config), SandboxMode::Off);

        config.sandbox = SandboxMode::Strict;
        let strict = Diagnostics::run(&paths, &config);
        // Either bubblewrap exists, or the user is told what to install.
        match &strict.sandbox {
            SandboxAvailability::Available(_) => {
                assert_eq!(effective_sandbox_mode(&config), SandboxMode::Strict)
            }
            SandboxAvailability::Unavailable(reason) => {
                assert!(reason.contains("bubblewrap"));
                assert_eq!(effective_sandbox_mode(&config), SandboxMode::Off);
            }
        }
    }
}
