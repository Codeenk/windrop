//! One-click installation of WinDrop's own prerequisites.
//!
//! The doctor says what is missing; this module turns that into something the
//! user can run. [`InstallPlan::for_diagnostics`] collects the missing package
//! names, picks the distribution's package manager, and builds the exact
//! commands — wrapped in `pkexec` so a graphical application can ask for
//! privilege the way the desktop expects, rather than by opening a terminal.
//!
//! Nothing here is Arch-specific: the package names WinDrop needs are the same
//! on Arch, Debian, Fedora and openSUSE, only the manager invocation differs.
//! When the distribution is unknown, or WinDrop runs inside Flatpak where host
//! packages are unreachable, there is no plan — the caller falls back to the
//! copy-paste command from [`Diagnostics::setup_command`].
//!
//! [`Diagnostics::setup_command`]: crate::doctor::Diagnostics::setup_command

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::doctor::{Diagnostics, Necessity};
use crate::process::CommandSpec;
use crate::runtime::sandbox::inside_flatpak;
use crate::{Error, Result};

/// The distribution family, decided by the package manager it uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistroFamily {
    Arch,
    Debian,
    Fedora,
    Suse,
    Unknown,
}

impl DistroFamily {
    /// Read `/etc/os-release` and decide.
    pub fn detect() -> Self {
        let text = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
        Self::from_os_release(&text)
    }

    /// Decide from the text of `os-release`, so it can be tested anywhere.
    ///
    /// Both `ID` and `ID_LIKE` are honoured: Linux Mint is `ubuntu`-like,
    /// EndeavourOS is `arch`-like, and either answer must work. `ID_LIKE`
    /// wins over `ID` — a derivative names its parent there, and the parent
    /// is what owns the package manager.
    pub fn from_os_release(text: &str) -> Self {
        let mut id = String::new();
        let mut id_like: Vec<String> = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if let Some(value) = line.strip_prefix("ID_LIKE=") {
                id_like.extend(
                    value
                        .trim_matches('"')
                        .trim_matches('\'')
                        .split_whitespace()
                        .map(str::to_string),
                );
            } else if let Some(value) = line.strip_prefix("ID=") {
                id = value.trim_matches('"').trim_matches('\'').to_string();
            }
        }
        // Parentage first, identity second.
        let mut candidates = id_like;
        candidates.push(id);
        for candidate in candidates {
            let lower = candidate.to_ascii_lowercase();
            if [
                "arch",
                "archarm",
                "manjaro",
                "endeavouros",
                "cachyos",
                "garuda",
                "artix",
                "arco",
                "blackarch",
                "parabola",
                "rebornos",
            ]
            .contains(&lower.as_str())
            {
                return DistroFamily::Arch;
            }
            if ["debian", "ubuntu", "linuxmint", "pop", "raspbian", "kali"]
                .iter()
                .any(|name| lower.contains(name))
            {
                return DistroFamily::Debian;
            }
            if ["fedora", "rhel", "centos", "rocky", "almalinux", "nobara"]
                .iter()
                .any(|name| lower.contains(name))
            {
                return DistroFamily::Fedora;
            }
            if ["suse", "opensuse", "sled", "sles"]
                .iter()
                .any(|name| lower.contains(name))
            {
                return DistroFamily::Suse;
            }
        }
        DistroFamily::Unknown
    }

    /// The `sudo` form, for copy-paste next to a terminal.
    pub fn sudo_command(&self, packages: &[String]) -> Option<String> {
        if packages.is_empty() {
            return None;
        }
        let list = packages.join(" ");
        match self {
            DistroFamily::Arch => Some(format!("sudo pacman -S --needed {list}")),
            DistroFamily::Debian => Some(format!(
                "sudo apt-get update && sudo apt-get install -y {list}"
            )),
            DistroFamily::Fedora => Some(format!("sudo dnf install -y {list}")),
            DistroFamily::Suse => Some(format!("sudo zypper install {list}")),
            DistroFamily::Unknown => None,
        }
    }
}

/// The package providing `tool` on `family`.
///
/// Today the names WinDrop needs are the same everywhere — `wine`,
/// `winetricks`, `bubblewrap`, `icoutils`, `cabextract`, `desktop-file-utils`,
/// `tar` — so this is one shared table. It stays a function so a future
/// divergence does not become a hunt through call sites.
pub fn package_for(tool: &str) -> Option<&'static str> {
    match tool {
        "wine" | "wine64" => Some("wine"),
        "winetricks" => Some("winetricks"),
        "bwrap" => Some("bubblewrap"),
        "wrestool" | "icotool" => Some("icoutils"),
        "cabextract" => Some("cabextract"),
        "update-desktop-database" | "xdg-desktop-menu" => Some("desktop-file-utils"),
        "tar" => Some("tar"),
        _ => None,
    }
}

/// How to install everything the doctor found missing, in one go.
#[derive(Debug, Clone)]
pub struct InstallPlan {
    pub family: DistroFamily,
    /// Deduplicated package names, in doctor order.
    pub packages: Vec<String>,
    /// One command per step, each a complete `pkexec …` invocation.
    pub steps: Vec<CommandSpec>,
}

impl InstallPlan {
    /// Build a plan for `packages` on `family`, or `None` when one-click
    /// installation is not possible there.
    pub fn new(family: DistroFamily, packages: Vec<String>) -> Option<Self> {
        if packages.is_empty() || family == DistroFamily::Unknown {
            return None;
        }
        let pkg_args: Vec<String> = packages.clone();
        let steps = match family {
            DistroFamily::Arch => vec![elevated(
                ["pacman", "-S", "--needed", "--noconfirm"]
                    .into_iter()
                    .map(str::to_string)
                    .chain(pkg_args)
                    .collect(),
            )],
            DistroFamily::Debian => vec![
                elevated(vec!["apt-get".into(), "update".into()]),
                elevated(
                    ["apt-get", "install", "-y"]
                        .into_iter()
                        .map(str::to_string)
                        .chain(pkg_args)
                        .collect(),
                ),
            ],
            DistroFamily::Fedora => vec![elevated(
                ["dnf", "install", "-y"]
                    .into_iter()
                    .map(str::to_string)
                    .chain(pkg_args)
                    .collect(),
            )],
            DistroFamily::Suse => vec![elevated(
                ["zypper", "--non-interactive", "install"]
                    .into_iter()
                    .map(str::to_string)
                    .chain(pkg_args)
                    .collect(),
            )],
            DistroFamily::Unknown => return None,
        };
        Some(InstallPlan {
            family,
            packages,
            steps,
        })
    }

    /// Build a plan from a diagnostics run: every missing tool that maps to a
    /// known package, deduplicated, in the order the setup pane shows them.
    ///
    /// `flatpak` is a parameter rather than a fresh check so tests can pin it;
    /// pass [`inside_flatpak()`] in production. Inside Flatpak host packages
    /// are unreachable, so there is never a plan.
    pub fn for_diagnostics(diagnostics: &Diagnostics, flatpak: bool) -> Option<Self> {
        if flatpak {
            return None;
        }
        let mut packages: Vec<String> = Vec::new();
        for tool in diagnostics.missing(Necessity::Optional) {
            let name = package_for(&tool.name)
                .unwrap_or(tool.package.as_str())
                .to_string();
            if name != "the package that provides this tool" && !packages.contains(&name) {
                packages.push(name);
            }
        }
        Self::new(DistroFamily::detect(), packages)
    }

    /// What to show on the button and in the log header.
    pub fn display(&self) -> String {
        self.steps
            .iter()
            .map(|step| step.display())
            .collect::<Vec<_>>()
            .join(" && ")
    }

    /// Run every step, streaming each output line to `on_line` and appending
    /// everything to `log_file`. Stops at the first failing step.
    pub fn run(&self, log_file: &Path, on_line: &dyn Fn(&str)) -> Result<()> {
        if let Some(parent) = log_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_file)?;
        for step in &self.steps {
            let header = format!("$ {}", step.display());
            on_line(&header);
            let _ = writeln!(log, "{header}");
            run_streaming(step, &mut log, on_line)?;
        }
        Ok(())
    }
}

/// Wrap a manager invocation so it asks for privilege graphically.
///
/// `WINDROP_SETUP_INSTALLER` overrides the wrapper (not the manager): when set,
/// its value is used as the elevating program instead of `pkexec`. That is the
/// seam the test-suite drives a mock installer through, and the escape hatch
/// for containers or managers `pkexec` cannot reach.
fn elevated(manager_args: Vec<String>) -> CommandSpec {
    let wrapper = std::env::var("WINDROP_SETUP_INSTALLER")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "pkexec".to_string());
    CommandSpec::new(wrapper).args(manager_args)
}

/// Run one step, streaming stdout and stderr line by line.
fn run_streaming(
    spec: &CommandSpec,
    log: &mut std::fs::File,
    on_line: &dyn Fn(&str),
) -> Result<()> {
    let mut command = spec.to_command();
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::ToolMissing(spec.program.to_string_lossy().to_string())
        } else {
            Error::Io(e)
        }
    })?;

    // Both pipes are drained on threads so a chatty installer cannot deadlock
    // by filling the pipe buffer while the parent waits.
    let out_lines = child.stdout.take().map(|pipe| {
        std::thread::spawn(move || {
            std::io::BufReader::new(pipe)
                .lines()
                .map_while(std::result::Result::ok)
                .collect::<Vec<_>>()
        })
    });
    let err_lines = child.stderr.take().map(|pipe| {
        std::thread::spawn(move || {
            std::io::BufReader::new(pipe)
                .lines()
                .map_while(std::result::Result::ok)
                .collect::<Vec<_>>()
        })
    });

    let status = child.wait()?;
    let mut lines = Vec::new();
    if let Some(handle) = out_lines {
        lines.extend(handle.join().unwrap_or_default());
    }
    if let Some(handle) = err_lines {
        lines.extend(handle.join().unwrap_or_default());
    }
    for line in &lines {
        let _ = writeln!(log, "{line}");
    }
    log.flush()?;
    for line in &lines {
        on_line(line);
    }
    if status.success() {
        Ok(())
    } else {
        Err(Error::CommandFailed {
            command: spec.display(),
            code: status.code().unwrap_or(-1),
            stderr: lines.last().cloned().unwrap_or_default(),
        })
    }
}

/// Where the setup run's own log lives.
pub fn setup_log_file(data_dir: &Path) -> PathBuf {
    data_dir.join("logs").join("setup-install.log")
}

/// What the setup pane can offer for a given diagnosis.
#[derive(Debug)]
pub enum SetupOffer {
    /// Everything is present; there is nothing to install.
    NothingMissing,
    /// One-click installation is available.
    Ready(InstallPlan),
    /// One-click installation is unavailable, with the reason to show.
    Unavailable(String),
}

impl SetupOffer {
    /// Decide from a diagnostics run.
    pub fn for_diagnostics(diagnostics: &Diagnostics) -> Self {
        if diagnostics.missing(Necessity::Optional).is_empty() {
            return SetupOffer::NothingMissing;
        }
        if inside_flatpak() {
            return SetupOffer::Unavailable(
                "this build runs inside Flatpak: install Wine and the helpers \
                 on the host system, then come back"
                    .to_string(),
            );
        }
        match InstallPlan::for_diagnostics(diagnostics, false) {
            Some(plan) => {
                if crate::process::which("pkexec").is_none()
                    && std::env::var("WINDROP_SETUP_INSTALLER")
                        .ok()
                        .filter(|value| !value.is_empty())
                        .is_none()
                {
                    SetupOffer::Unavailable(
                        "no privilege helper found: install polkit (pkexec) \
                         or use the command below"
                            .to_string(),
                    )
                } else {
                    SetupOffer::Ready(plan)
                }
            }
            None => SetupOffer::Unavailable(
                "this distribution is not recognised: use the command below".to_string(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::{HostInfo, ToolStatus};

    fn tool(name: &str, package: &str) -> ToolStatus {
        ToolStatus {
            name: name.into(),
            necessity: Necessity::Required,
            path: None,
            purpose: "test".into(),
            package: package.into(),
            version: None,
        }
    }

    #[test]
    fn arch_derivatives_resolve_by_id_or_like() {
        for text in [
            "ID=arch\nPRETTY_NAME=\"Arch Linux\"\n",
            "ID=endeavouros\nID_LIKE=arch\n",
            "ID=cachyos\nID_LIKE=arch\n",
            "ID=manjaro\nID_LIKE=\"arch\"\n",
        ] {
            assert_eq!(
                DistroFamily::from_os_release(text),
                DistroFamily::Arch,
                "{text:?}"
            );
        }
    }

    #[test]
    fn debian_fedora_and_suse_families_resolve() {
        assert_eq!(
            DistroFamily::from_os_release("ID=debian\nPRETTY_NAME=\"Debian GNU/Linux\"\n"),
            DistroFamily::Debian
        );
        assert_eq!(
            DistroFamily::from_os_release("ID=ubuntu\nID_LIKE=debian\n"),
            DistroFamily::Debian
        );
        assert_eq!(
            DistroFamily::from_os_release("ID=linuxmint\nID_LIKE=\"ubuntu debian\"\n"),
            DistroFamily::Debian
        );
        assert_eq!(
            DistroFamily::from_os_release("ID=fedora\n"),
            DistroFamily::Fedora
        );
        assert_eq!(
            DistroFamily::from_os_release("ID=opensuse-tumbleweed\nID_LIKE=\"opensuse suse\"\n"),
            DistroFamily::Suse
        );
    }

    #[test]
    fn unknown_distributions_have_no_family() {
        assert_eq!(DistroFamily::from_os_release(""), DistroFamily::Unknown);
        assert_eq!(
            DistroFamily::from_os_release("ID=nixos\n"),
            DistroFamily::Unknown
        );
    }

    #[test]
    fn package_names_cover_every_checked_tool() {
        for tool in [
            "wine",
            "winetricks",
            "bwrap",
            "wrestool",
            "icotool",
            "cabextract",
        ] {
            assert!(package_for(tool).is_some(), "{tool}");
        }
    }

    #[test]
    fn arch_plan_installs_everything_noninteractively() {
        let plan =
            InstallPlan::new(DistroFamily::Arch, vec!["wine".into(), "bubblewrap".into()]).unwrap();
        assert_eq!(plan.steps.len(), 1);
        let shown = plan.display();
        assert!(shown.contains("pacman"), "{shown}");
        assert!(shown.contains("--noconfirm"), "{shown}");
        assert!(shown.contains("wine"));
    }

    #[test]
    fn debian_plan_refreshes_before_installing() {
        let plan = InstallPlan::new(DistroFamily::Debian, vec!["wine".into()]).unwrap();
        assert_eq!(plan.steps.len(), 2);
        let shown = plan.display();
        assert!(shown.contains("update"), "{shown}");
        assert!(shown.contains("install"), "{shown}");
    }

    #[test]
    fn empty_or_unknown_plans_do_not_exist() {
        assert!(InstallPlan::new(DistroFamily::Arch, Vec::new()).is_none());
        assert!(InstallPlan::new(DistroFamily::Unknown, vec!["wine".into()]).is_none());
    }

    #[test]
    fn sudo_commands_are_paste_ready_per_family() {
        let pkgs = vec!["wine".into()];
        assert!(DistroFamily::Arch
            .sudo_command(&pkgs)
            .unwrap()
            .contains("pacman"));
        assert!(DistroFamily::Debian
            .sudo_command(&pkgs)
            .unwrap()
            .contains("apt-get"));
        assert!(DistroFamily::Fedora
            .sudo_command(&pkgs)
            .unwrap()
            .contains("dnf"));
        assert!(DistroFamily::Suse
            .sudo_command(&pkgs)
            .unwrap()
            .contains("zypper"));
        assert!(DistroFamily::Unknown.sudo_command(&pkgs).is_none());
    }

    #[test]
    fn diagnostics_with_nothing_missing_have_no_plan() {
        let plan = InstallPlan::new(DistroFamily::Arch, Vec::new());
        assert!(plan.is_none());
        let _ = HostInfo::detect();
    }

    #[test]
    fn for_diagnostics_collects_and_deduplicates_packages() {
        let diagnostics = diagnostics_with(vec![
            tool("wine", "wine"),
            tool("wrestool", "icoutils"),
            tool("icotool", "icoutils"),
        ]);
        // Force the Arch shape regardless of the test host.
        let mut packages: Vec<String> = Vec::new();
        for t in diagnostics.missing(Necessity::Optional) {
            let name = package_for(&t.name).unwrap_or(t.package.as_str());
            if !packages.contains(&name.to_string()) {
                packages.push(name.to_string());
            }
        }
        assert_eq!(packages, vec!["wine", "icoutils"]);
    }

    #[test]
    fn the_executor_runs_steps_and_streams_lines() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("mock-installer");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf 'doing things\\n';\nprintf 'done\\n'\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let plan = InstallPlan {
            family: DistroFamily::Arch,
            packages: vec!["wine".into()],
            steps: vec![CommandSpec::new(&script).arg("--noconfirm")],
        };
        let log = dir.path().join("setup.log");
        let seen = std::cell::RefCell::new(Vec::new());
        plan.run(&log, &|line| seen.borrow_mut().push(line.to_string()))
            .unwrap();
        let seen = seen.borrow();
        assert!(seen.contains(&"doing things".to_string()), "{seen:?}");
        assert!(seen.contains(&"done".to_string()), "{seen:?}");
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(logged.contains("doing things"), "{logged}");
    }

    #[test]
    fn a_failing_step_stops_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("mock-installer");
        std::fs::write(&script, "#!/bin/sh\nprintf 'nope\\n' >&2\nexit 3\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let plan = InstallPlan {
            family: DistroFamily::Arch,
            packages: vec!["wine".into()],
            steps: vec![CommandSpec::new(&script)],
        };
        let log = dir.path().join("setup.log");
        let result = plan.run(&log, &|_| {});
        assert!(
            matches!(result, Err(Error::CommandFailed { code: 3, .. })),
            "{result:?}"
        );
    }

    fn diagnostics_with(tools: Vec<ToolStatus>) -> Diagnostics {
        Diagnostics {
            tools,
            wine: None,
            wine_error: None,
            sandbox: crate::runtime::sandbox::SandboxAvailability::Unavailable("test".into()),
            components: Vec::new(),
            data_dir: PathBuf::from("/tmp/windrop-test"),
            free_space_bytes: None,
            profiles: 0,
            installed_apps: 0,
            host: HostInfo {
                distribution: "Test Linux".into(),
                kernel: "test".into(),
                architecture: "x86_64".into(),
                session_type: "unknown".into(),
                cpu_count: 1,
            },
        }
    }

    #[test]
    fn inside_flatpak_there_is_never_a_plan() {
        let diagnostics = diagnostics_with(vec![tool("wine", "wine")]);
        assert!(InstallPlan::for_diagnostics(&diagnostics, true).is_none());
    }
}
