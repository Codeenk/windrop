//! Confining an application with bubblewrap.
//!
//! Windows software is not trusted code, and a Wine prefix is a full writable
//! filesystem view. In strict mode the process runs inside a bubblewrap sandbox
//! that:
//!
//! * mounts `/usr`, `/etc`, `/bin`, `/sbin`, `/lib*` and `/opt` **read-only**;
//! * mounts `$HOME` **not at all**, replacing it with a private directory inside
//!   the application's own prefix;
//! * makes the prefix and the application directory writable, and nothing else;
//! * gives access to the display and audio sockets, which GUI applications
//!   need, but not to the rest of `/run`;
//! * shares the network only when the caller asks for it.
//!
//! The wrapper is a pure transformation of one [`CommandSpec`] into another, so
//! the exact sandbox arguments are unit-testable and visible in the log.
//!
//! If bubblewrap is missing, strict mode degrades to running without a sandbox
//! and says so, rather than failing the install.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::config::SandboxMode;
use crate::process::{which, CommandSpec};
use crate::runtime::prefix::PrefixPaths;

/// Whether a sandbox can be used at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxAvailability {
    /// bubblewrap was found at this path.
    Available(PathBuf),
    /// No sandbox is possible, with the reason to show the user.
    Unavailable(String),
}

impl SandboxAvailability {
    pub fn is_available(&self) -> bool {
        matches!(self, SandboxAvailability::Available(_))
    }

    pub fn path(&self) -> Option<&Path> {
        match self {
            SandboxAvailability::Available(p) => Some(p),
            SandboxAvailability::Unavailable(_) => None,
        }
    }

    /// Reason text for the UI.
    pub fn reason(&self) -> Option<&str> {
        match self {
            SandboxAvailability::Available(_) => None,
            SandboxAvailability::Unavailable(reason) => Some(reason),
        }
    }
}

/// The file Flatpak writes inside every sandbox it starts.
pub const FLATPAK_MARKER: &str = "/.flatpak-info";

/// Is this process running inside a Flatpak sandbox?
///
/// It matters because bubblewrap cannot be nested inside Flatpak: user
/// namespaces are not available there. Rather than report a missing dependency
/// that is in fact unusable, WinDrop recognises the case and carries on, since
/// Flatpak already isolates the application from the rest of the system.
pub fn inside_flatpak() -> bool {
    Path::new(FLATPAK_MARKER).exists()
}

/// Look for bubblewrap, honouring the requested mode.
pub fn availability(mode: SandboxMode) -> SandboxAvailability {
    // Decisions that need no probe come first, so a switched-off sandbox or a
    // Flatpak build is never described by what is installed on the host.
    match mode {
        SandboxMode::Off => {
            return SandboxAvailability::Unavailable(
                "sandboxing is switched off in settings".into(),
            )
        }
        SandboxMode::Strict if inside_flatpak() => {
            return SandboxAvailability::Unavailable(
                "this build runs inside Flatpak, which already isolates every application".into(),
            )
        }
        SandboxMode::Strict => {}
    }
    match which("bwrap") {
        Some(path) if bubblewrap_works(&path) => SandboxAvailability::Available(path),
        Some(path) => SandboxAvailability::Unavailable(format!(
            "bubblewrap is installed at {} but this kernel forbids user namespaces, \
             which bubblewrap needs",
            path.display()
        )),
        None => SandboxAvailability::Unavailable(
            "bubblewrap is not installed (sudo pacman -S bubblewrap)".into(),
        ),
    }
}

/// Can this bubblewrap actually start a sandbox?
///
/// Finding the binary is not the same as being able to use it: bubblewrap needs
/// unprivileged user namespaces, and hardened kernels (and Docker containers,
/// CI runners among them) disable them. Then every launch would die with
/// `setting up uid map: Permission denied` — a message the user should never
/// have to translate. One cheap probe, run once, turns that into an honest
/// report from the doctor and a clear log line instead.
pub fn bubblewrap_works(bwrap: &Path) -> bool {
    use std::process::{Command, Stdio};
    // An empty namespace that runs `true` costs about a millisecond. It touches
    // nothing, so probing is safe in any environment.
    Command::new(bwrap)
        .args(["--ro-bind", "/", "/", "--", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// The decision, with its inputs passed in so it can be tested anywhere.
pub fn availability_with(
    mode: SandboxMode,
    flatpak: bool,
    bwrap: Option<PathBuf>,
) -> SandboxAvailability {
    match mode {
        SandboxMode::Off => {
            SandboxAvailability::Unavailable("sandboxing is switched off in settings".into())
        }
        SandboxMode::Strict if flatpak => SandboxAvailability::Unavailable(
            "this build runs inside Flatpak, which already isolates every application".into(),
        ),
        SandboxMode::Strict => match bwrap {
            Some(path) => SandboxAvailability::Available(path),
            None => SandboxAvailability::Unavailable(
                "bubblewrap is not installed (sudo pacman -S bubblewrap)".into(),
            ),
        },
    }
}

/// Everything the wrapper needs to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxRequest {
    /// Path to the `bwrap` executable. Injected so wrapping is a pure function.
    pub bwrap: PathBuf,
    /// The application's prefix, mounted read-write.
    pub prefix: PrefixPaths,
    /// Additional host directories the user chose to expose read-write.
    pub shared_folders: Vec<PathBuf>,
    /// Whether the program can reach the network.
    ///
    /// True by default, and by default nothing turns it off: an installer needs
    /// to download its payload, and a browser, a chat client or a game without
    /// the network is broken in a way the user cannot see the cause of. The
    /// sandbox withholds the filesystem; it does not withhold the internet.
    pub allow_network: bool,
    /// Host directories that must be visible read-only because Wine needs them.
    pub extra_read_only: Vec<PathBuf>,
    /// Tear the sandbox down when the process that started it exits.
    ///
    /// This is right for an installer, which should not outlive the WinDrop run
    /// that started it. It is *wrong* for a detached launch: the launcher exits
    /// immediately by design, so tying the application's lifetime to it would
    /// kill the application the moment it appeared in the menu. The caller
    /// says which it wants.
    pub die_with_parent: bool,
}

impl SandboxRequest {
    pub fn new(bwrap: impl Into<PathBuf>, prefix: PrefixPaths) -> Self {
        SandboxRequest {
            bwrap: bwrap.into(),
            prefix,
            shared_folders: Vec::new(),
            allow_network: true,
            extra_read_only: Vec::new(),
            die_with_parent: true,
        }
    }

    pub fn with_network(mut self, allow: bool) -> Self {
        self.allow_network = allow;
        self
    }

    pub fn with_shared_folders(mut self, folders: Vec<PathBuf>) -> Self {
        self.shared_folders = folders;
        self
    }

    /// Host directories the command cannot work without.
    ///
    /// The sandbox deliberately hides the rest of `$HOME`, which means it also
    /// hides things Wine genuinely needs: a managed Wine build lives under the
    /// data directory, and an installer usually lives in the user's downloads
    /// directory. Without these the sandbox would not be a safety feature but a
    /// broken one.
    pub fn with_read_only(mut self, dirs: Vec<PathBuf>) -> Self {
        self.extra_read_only = dirs;
        self
    }

    /// Let the sandbox outlive whoever started it.
    ///
    /// Used for a launch from a menu entry, where the launcher's job is to hand
    /// over and exit.
    pub fn surviving_the_launcher(mut self) -> Self {
        self.die_with_parent = false;
        self
    }
}

/// Directories mounted read-only into the sandbox when they exist.
const READ_ONLY_ROOTS: &[&str] = &[
    "/usr", "/etc", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32", "/opt", "/nix",
];

/// Whether a directory is already visible through one of [`READ_ONLY_ROOTS`].
///
/// A duplicate bind is harmless but noisy: it lengthens every rendered command
/// and every log line for no benefit, and most Wine installations are under
/// `/usr` anyway.
fn already_visible(dir: &Path) -> bool {
    READ_ONLY_ROOTS.iter().any(|root| {
        let root = Path::new(root);
        root.exists() && dir.starts_with(root)
    })
}

/// Wrap `inner` so it runs inside the sandbox.
///
/// The inner command's environment is preserved by handing it to the `bwrap`
/// process, which passes its environment through to the child. That keeps the
/// rendered command readable while remaining exactly equivalent to a long list
/// of `--setenv` flags.
pub fn wrap(inner: CommandSpec, request: &SandboxRequest) -> CommandSpec {
    let mut args: Vec<OsString> = Vec::new();
    let mut push = |a: &str| args.push(OsString::from(a));

    if request.die_with_parent {
        push("--die-with-parent");
    }
    push("--new-session");
    push("--unshare-pid");
    push("--unshare-ipc");
    push("--unshare-uts");
    push("--unshare-cgroup-try");
    if !request.allow_network {
        push("--unshare-net");
    }

    // System directories, read-only.
    for dir in READ_ONLY_ROOTS {
        if Path::new(dir).exists() {
            push("--ro-bind");
            push(dir);
            push(dir);
        }
    }
    // A working /proc, /dev and a private /tmp.
    push("--proc");
    push("/proc");
    push("--dev");
    push("/dev");
    push("--tmpfs");
    push("/tmp");

    // Paths the caller asked for explicitly, after the private /tmp so that a
    // download in `/tmp` — which is exactly where a browser puts a `Save as`
    // file, and where a test harness lives — is visible rather than shadowed.
    for dir in &request.extra_read_only {
        if already_visible(dir) || !dir.exists() {
            continue;
        }
        if let Some(s) = dir.to_str() {
            push("--ro-bind");
            push(s);
            push(s);
        }
    }

    // Display sockets, mounted after the tmpfs so they are visible again.
    if Path::new("/tmp/.X11-unix").exists() {
        push("--ro-bind");
        push("/tmp/.X11-unix");
        push("/tmp/.X11-unix");
    }
    // The per-user runtime directory carries the Wayland, PulseAudio and D-Bus
    // sockets a GUI application needs.
    if let Some(run_user) = session_runtime_dir() {
        if run_user.exists() {
            if let Some(s) = run_user.to_str() {
                push("--bind");
                push(s);
                push(s);
            }
        }
    }

    // sysfs exposes GPU and DRM information; read-only is enough.
    if Path::new("/sys").exists() {
        push("--ro-bind");
        push("/sys");
        push("/sys");
    }

    // The application's own tree, writable.
    if let Some(root) = request.prefix.root().to_str() {
        push("--bind");
        push(root);
        push(root);
    }

    // User-approved shared folders.
    for folder in &request.shared_folders {
        if let Some(s) = folder.to_str() {
            if folder.exists() {
                push("--bind-try");
                push(s);
                push(s);
            }
        }
    }

    // Enter the sandbox in the same directory as the Wine invocation.
    if let Some(cwd) = inner.cwd.as_ref().and_then(|c| c.to_str()) {
        push("--chdir");
        push(cwd);
    }

    push("--");
    if let Some(program) = inner.program.to_str() {
        push(program);
    }
    for arg in &inner.args {
        args.push(arg.clone());
    }

    let mut spec = CommandSpec::new(&request.bwrap).args(args);
    spec.env = inner.env;
    spec.env_remove = inner.env_remove;
    spec.cwd = inner.cwd;
    spec
}

/// `$XDG_RUNTIME_DIR`, or `/run/user/<uid>` as a fallback.
pub fn session_runtime_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let candidate = PathBuf::from(format!("/run/user/{uid}"));
    if candidate.exists() {
        Some(candidate)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> SandboxRequest {
        SandboxRequest::new("/usr/bin/bwrap", PrefixPaths::new("/data/apps/app"))
    }

    fn inner() -> CommandSpec {
        CommandSpec::new("/data/runtime/wine/bin/wine")
            .arg("C:\\setup.exe")
            .env("WINEPREFIX", "/data/apps/app/prefix")
            .env("HOME", "/data/apps/app/prefix/home")
    }

    fn args_of(spec: &CommandSpec) -> Vec<String> {
        spec.args
            .iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn the_sandbox_runs_bwrap_with_the_inner_command_last() {
        let wrapped = wrap(inner(), &request());
        assert_eq!(wrapped.program, PathBuf::from("/usr/bin/bwrap"));
        let args = args_of(&wrapped);
        let separator = args.iter().position(|a| a == "--").expect("separator");
        assert_eq!(args[separator + 1], "/data/runtime/wine/bin/wine");
        assert_eq!(args[separator + 2], "C:\\setup.exe");
        assert!(
            separator + 3 == args.len(),
            "nothing may follow the inner command"
        );
    }

    #[test]
    fn the_sandbox_can_be_told_to_outlive_its_launcher() {
        // A detached launch: the launcher hands over and exits, and tying the
        // application's life to it would kill the application instantly.
        let with_parent = args_of(&wrap(inner(), &request()));
        assert!(with_parent.iter().any(|a| a == "--die-with-parent"));

        let detached = args_of(&wrap(inner(), &request().surviving_the_launcher()));
        assert!(
            !detached.iter().any(|a| a == "--die-with-parent"),
            "a detached application must survive its launcher: {detached:?}"
        );
    }

    #[test]
    fn extra_paths_are_mounted_after_the_private_tmp() {
        // Order matters: a bind placed before `--tmpfs /tmp` is wiped by it, so
        // an installer sitting in `/tmp` — where a browser's "Save as" puts it —
        // would silently disappear.
        let tmp = tempfile::tempdir().unwrap();
        let downloads = tmp.path().join("downloads");
        std::fs::create_dir_all(&downloads).unwrap();

        let request = request().with_read_only(vec![downloads.clone()]);
        let args = args_of(&wrap(inner(), &request));
        let tmpfs = args.iter().position(|a| a == "--tmpfs").expect("--tmpfs");
        let bind = args
            .iter()
            .position(|a| *a == downloads.to_string_lossy())
            .expect("the requested path must be bound");
        assert!(
            bind > tmpfs,
            "the bind must come after the private /tmp: {args:?}"
        );
        assert_eq!(args[bind - 1], "--ro-bind");
    }

    #[test]
    fn paths_inside_a_system_root_are_not_bound_twice() {
        // `/usr` is mounted by READ_ONLY_ROOTS, so asking for it (or anything
        // inside it) again would only lengthen every command line and log line.
        let request = request().with_read_only(vec![
            PathBuf::from("/usr"),
            PathBuf::from("/usr/lib"),
            PathBuf::from("/nope/not/here"),
        ]);
        let args = args_of(&wrap(inner(), &request));
        assert!(
            !args.iter().any(|a| a == "/usr/lib"),
            "a path already visible must not be bound again: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "/nope/not/here"),
            "a path that does not exist cannot be bound"
        );
        // `/usr` still appears, exactly once as a source and once as a target.
        assert_eq!(args.iter().filter(|a| *a == "/usr").count(), 2);
    }

    #[test]
    fn a_managed_wine_tree_can_be_bound_inside_home() {
        // The case that matters in practice: Wine downloaded by WinDrop rather
        // than installed by the distribution lives under the data directory.
        let home = tempfile::tempdir().unwrap();
        let wine = home
            .path()
            .join(".local/share/windrop/runtime/wine/wine-9.0");
        std::fs::create_dir_all(&wine).unwrap();

        let request = request().with_read_only(vec![wine.clone()]);
        let args = args_of(&wrap(inner(), &request));
        assert!(args.contains(&wine.to_string_lossy().to_string()));
    }

    #[test]
    fn the_inner_environment_survives_wrapping() {
        let wrapped = wrap(inner(), &request());
        assert_eq!(
            wrapped.env_get("WINEPREFIX").unwrap(),
            std::ffi::OsStr::new("/data/apps/app/prefix")
        );
        assert!(wrapped.env_has("HOME"));
    }

    #[test]
    fn the_prefix_is_the_only_writable_window_into_the_data_tree() {
        let wrapped = wrap(inner(), &request());
        assert!(wrapped.args.windows(3).any(|w| {
            w[0] == "--bind" && w[1] == "/data/apps/app/prefix" && w[2] == "/data/apps/app/prefix"
        }));
    }

    #[test]
    fn the_home_directory_is_not_exposed() {
        let wrapped = wrap(inner(), &request());
        let rendered = wrapped.display();
        assert!(
            !rendered.contains("--bind /home"),
            "the user's home must not be mounted"
        );
        assert!(!rendered.contains("--ro-bind /home"));
    }

    #[test]
    fn system_directories_are_mounted_read_only() {
        let wrapped = wrap(inner(), &request());
        let args = args_of(&wrapped);
        for dir in ["/usr", "/etc"] {
            let count = args
                .windows(3)
                .filter(|w| w[0] == "--ro-bind" && w[1] == dir && w[2] == dir)
                .count();
            assert!(count >= 1, "{dir} should be read-only bound");
        }
        // No path outside the prefix may be writable.
        for w in args.windows(3) {
            if w[0] == "--bind" {
                assert!(
                    w[1].starts_with("/data/apps/app") || w[1].starts_with("/run"),
                    "unexpected writable mount: {}",
                    w[1]
                );
            }
        }
    }

    #[test]
    fn network_is_shared_by_default_and_unshared_on_request() {
        assert!(!args_of(&wrap(inner(), &request())).contains(&"--unshare-net".to_string()));
        let offline = request().with_network(false);
        assert!(args_of(&wrap(inner(), &offline)).contains(&"--unshare-net".to_string()));
    }

    #[test]
    fn shared_folders_are_mounted_read_write() {
        let tmp = tempfile::tempdir().unwrap();
        let shared = tmp.path().join("Documents");
        std::fs::create_dir_all(&shared).unwrap();

        let req = request().with_shared_folders(vec![shared.clone(), tmp.path().join("gone")]);
        let wrapped = wrap(inner(), &req);
        let args = args_of(&wrapped);
        let s = shared.to_string_lossy().to_string();

        assert!(args
            .windows(3)
            .any(|w| w[0] == "--bind-try" && w[1] == s && w[2] == s));
        // A folder that does not exist is skipped rather than breaking the run.
        assert!(!args.iter().any(|a| a.contains("gone")));
    }

    #[test]
    fn the_working_directory_is_carried_into_the_sandbox() {
        let mut cmd = inner();
        cmd.cwd = Some(PathBuf::from("/data/apps/app/prefix/drive_c/game"));
        let wrapped = wrap(cmd, &request());
        let args = args_of(&wrapped);
        let at = args.iter().position(|a| a == "--chdir").unwrap();
        assert_eq!(args[at + 1], "/data/apps/app/prefix/drive_c/game");
    }

    #[test]
    fn the_process_is_isolated_and_dies_with_its_parent() {
        let args = args_of(&wrap(inner(), &request()));
        for flag in [
            "--die-with-parent",
            "--new-session",
            "--unshare-pid",
            "--proc",
            "--dev",
        ] {
            assert!(args.contains(&flag.to_string()), "missing {flag}");
        }
        assert!(args.contains(&"/proc".to_string()));
        assert!(args.contains(&"/dev".to_string()));
    }

    #[test]
    fn availability_reports_off_mode_without_probing() {
        let status = availability(SandboxMode::Off);
        assert!(!status.is_available());
        assert!(status.reason().unwrap().contains("switched off"));
    }

    #[test]
    fn availability_finds_bubblewrap_or_explains_how_to_get_it() {
        // The live environment varies: CI containers ship bubblewrap but forbid
        // namespaces, desktop boxes have it working, bare images lack it.
        // Whatever is true here, the report must name the actual state.
        let status = availability(SandboxMode::Strict);
        match which("bwrap") {
            None => {
                assert!(!status.is_available());
                assert!(status.reason().unwrap().contains("pacman"));
            }
            Some(bwrap) if bubblewrap_works(&bwrap) => {
                assert_eq!(status.path(), Some(bwrap.as_path()));
            }
            Some(_) => {
                assert!(!status.is_available());
                let reason = status.reason().unwrap();
                assert!(reason.contains("namespaces"), "{reason}");
            }
        }
    }

    #[test]
    fn bubblewrap_is_found_when_it_is_there_and_its_absence_is_explained() {
        let found = availability_with(
            SandboxMode::Strict,
            false,
            Some(PathBuf::from("/usr/bin/bwrap")),
        );
        assert_eq!(found.path(), Some(Path::new("/usr/bin/bwrap")));

        let missing = availability_with(SandboxMode::Strict, false, None);
        assert!(!missing.is_available());
        assert!(missing.reason().unwrap().contains("pacman"));
    }

    #[test]
    fn inside_flatpak_the_extra_layer_is_skipped_rather_than_missed() {
        // Namespaces are unavailable inside Flatpak, so pretending bubblewrap is
        // missing would send the user off to install something they cannot use.
        let status = availability_with(
            SandboxMode::Strict,
            true,
            Some(PathBuf::from("/usr/bin/bwrap")),
        );
        assert!(!status.is_available());
        let reason = status.reason().unwrap();
        assert!(reason.contains("Flatpak"), "{reason}");
        assert!(!reason.contains("pacman"), "{reason}");
    }

    #[test]
    fn switching_the_sandbox_off_outranks_everything_else() {
        for flatpak in [true, false] {
            for bwrap in [None, Some(PathBuf::from("/usr/bin/bwrap"))] {
                let status = availability_with(SandboxMode::Off, flatpak, bwrap);
                assert!(status.reason().unwrap().contains("switched off"));
            }
        }
    }

    #[test]
    fn the_wrapped_command_is_renderable_for_logs() {
        let rendered = wrap(inner(), &request()).display();
        assert!(rendered.starts_with("/usr/bin/bwrap"));
        assert!(rendered.contains("--die-with-parent"));
        assert!(rendered.contains("WINEPREFIX="));
    }

    #[test]
    fn the_probe_rejects_a_missing_binary() {
        assert!(!bubblewrap_works(Path::new("/nonexistent/bwrap")));
        // The positive case cannot be asserted here: whether a given host lets
        // bubblewrap create namespaces is exactly what the probe measures, and
        // some hardened kernels (Ubuntu 24.04's AppArmor restriction among
        // them) allow `unshare` while denying unprofiled binaries.
        // availability_finds_bubblewrap_or_explains_how_to_get_it above
        // already branches on whatever the probe finds.
    }
}
