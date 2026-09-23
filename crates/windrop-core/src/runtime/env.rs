//! Building the exact command that runs an application.
//!
//! The input is a [`RuntimeEnv`] — the *plan* — plus a prefix and a target. The
//! output is a [`LaunchPlan`] holding a fully-formed [`CommandSpec`]: the Wine
//! binary, its arguments, every environment variable, the working directory, and
//! whether the whole thing is wrapped in bubblewrap.
//!
//! No process is started here. That makes the interesting decisions — which
//! overrides to set, which translation layers are active, whether the sandbox
//! applies — assertable in tests on a machine with no Wine installed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::compat::pe::Arch;
use crate::compat::profile::RuntimeEnv;
use crate::compat::InputKind;
use crate::config::{Config, SandboxMode};
use crate::process::{CommandSpec, Output};
use crate::runtime::prefix::PrefixPaths;
use crate::runtime::sandbox::{self, SandboxAvailability, SandboxRequest};
use crate::runtime::wine::{base_prefix_env, WineInstall};
use crate::Result;

/// What the plan should run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchTarget {
    /// A file the user supplied, anywhere on the host filesystem.
    HostFile {
        host_path: PathBuf,
        input_kind: InputKind,
        extra_args: Vec<String>,
    },
    /// A program already installed inside the prefix, addressed by Windows path.
    Installed {
        windows_path: String,
        extra_args: Vec<String>,
    },
}

impl LaunchTarget {
    pub fn host_file(host_path: impl Into<PathBuf>, input_kind: InputKind) -> Self {
        LaunchTarget::HostFile {
            host_path: host_path.into(),
            input_kind,
            extra_args: Vec::new(),
        }
    }

    pub fn installed(windows_path: impl Into<String>) -> Self {
        LaunchTarget::Installed {
            windows_path: windows_path.into(),
            extra_args: Vec::new(),
        }
    }

    pub fn with_args(mut self, args: Vec<String>) -> Self {
        match &mut self {
            LaunchTarget::HostFile { extra_args, .. } => *extra_args = args,
            LaunchTarget::Installed { extra_args, .. } => *extra_args = args,
        }
        self
    }

    fn describe(&self) -> String {
        match self {
            LaunchTarget::HostFile {
                host_path,
                input_kind,
                ..
            } => {
                format!("{} {}", input_kind.label(), host_path.display())
            }
            LaunchTarget::Installed { windows_path, .. } => {
                format!("installed program {windows_path}")
            }
        }
    }
}

/// A ready-to-run command, with the reasoning behind it recorded.
#[derive(Debug, Clone)]
pub struct LaunchPlan {
    pub spec: CommandSpec,
    pub prefix: PrefixPaths,
    pub wine: WineInstall,
    /// Whether bubblewrap is wrapping this invocation.
    pub sandboxed: bool,
    /// Why this environment looks the way it does.
    pub rationale: String,
    /// What is being run, for logs.
    pub target: String,
}

impl LaunchPlan {
    /// Execute, capturing output to the spec's log file.
    ///
    /// The plan is written into the log before the program starts. A Windows
    /// application that prints nothing at all is completely normal, and waking
    /// up to a blank log tells nobody whether the program even launched — which
    /// is the first question anyone debugging a launch needs answered.
    pub fn run_logged(&self, log_file: &Path, timeout: Duration) -> Result<Output> {
        self.write_log_header(log_file);
        self.spec.run_logged(log_file, timeout)
    }

    /// Append a preamble describing what is about to run.
    ///
    /// Best-effort: an unwritable log must not stop an application from
    /// starting, so a failure here is only reported in the tracing output.
    fn write_log_header(&self, log_file: &Path) {
        use std::io::Write;
        if let Some(parent) = log_file.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                tracing::warn!(error = %error, "could not create the log directory");
                return;
            }
        }
        let header = format!(
            "\n=== {} ===\n{}\n  command:  {}\n",
            crate::db::now_iso8601(),
            self.describe(),
            self.spec.display()
        );
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_file)
        {
            Ok(mut file) => {
                if let Err(error) = file.write_all(header.as_bytes()) {
                    tracing::warn!(error = %error, "could not write the log header");
                }
                let _ = file.flush();
            }
            Err(error) => tracing::warn!(error = %error, "could not open the log for writing"),
        }
    }

    /// Execute with inherited stdio, for installers that need a real window.
    pub fn run_interactive(&self) -> Result<Output> {
        self.spec.run_interactive()
    }

    /// A multi-line description for the log pane.
    pub fn describe(&self) -> String {
        format!(
            "{}\n  wine:     {} ({})\n  prefix:   {}\n  sandbox:  {}\n  strategy: {}",
            self.target,
            self.wine.executable.display(),
            self.wine.display(),
            self.prefix.root().display(),
            if self.sandboxed {
                "bubblewrap (strict)"
            } else {
                "none"
            },
            self.rationale
        )
    }

    /// The environment variables this plan sets, sorted.
    pub fn env_pairs(&self) -> Vec<(String, String)> {
        self.spec
            .env
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().to_string(),
                    v.to_string_lossy().to_string(),
                )
            })
            .collect()
    }

    /// The variable a given name resolves to, for diagnostics.
    pub fn env_of(&self, key: &str) -> Option<String> {
        self.spec
            .env_get(key)
            .map(|v| v.to_string_lossy().to_string())
    }
}

/// Assembles [`LaunchPlan`]s.
pub struct EnvironmentBuilder<'a> {
    config: &'a Config,
    wine: WineInstall,
    /// Whether the resulting command has to outlive this process.
    survives_the_launcher: bool,
}

impl<'a> EnvironmentBuilder<'a> {
    pub fn new(config: &'a Config, wine: WineInstall) -> Self {
        EnvironmentBuilder {
            config,
            wine,
            survives_the_launcher: false,
        }
    }

    pub fn wine(&self) -> &WineInstall {
        &self.wine
    }

    pub fn config(&self) -> &Config {
        self.config
    }

    /// The environment variables implied by a plan, before any sandboxing.
    ///
    /// Exposed separately because the GUI shows it and it is the part most
    /// worth testing.
    pub fn base_env(&self, prefix: &PrefixPaths, variant: &RuntimeEnv) -> BTreeMap<String, String> {
        let mut env: BTreeMap<String, String> =
            base_prefix_env(prefix, &self.wine).into_iter().collect();

        // A 64-bit prefix runs 32-bit code through WoW64; a 32-bit prefix is
        // needed only to run without WoW64 support.
        env.insert(
            "WINEARCH".to_string(),
            if variant.arch == Arch::X86 {
                "win32"
            } else {
                "win64"
            }
            .to_string(),
        );

        if let Some(overrides) = variant.dll_overrides_value() {
            env.insert("WINEDLLOVERRIDES".to_string(), overrides);
        }

        if self.config.esync {
            env.insert("WINEESYNC".to_string(), "1".to_string());
        }
        if self.config.fsync {
            env.insert("WINEFSYNC".to_string(), "1".to_string());
        }

        // Keep Wine from writing anything of its own outside the prefix.
        env.insert(
            "HOME".to_string(),
            prefix.sandbox_home().to_string_lossy().to_string(),
        );

        // Point DXVK at this application's own configuration, if one was
        // written when the prefix was prepared.
        if variant.dxvk {
            let conf = prefix.dxvk_conf();
            if conf.is_file() {
                env.insert(
                    "DXVK_CONFIG_FILE".to_string(),
                    conf.to_string_lossy().to_string(),
                );
            }
            for (k, v) in self.config.effective_dxvk_settings().to_env() {
                env.insert(k, v);
            }
        }

        // Explicit per-variant variables always win, so a hand-written profile
        // can override anything.
        for (k, v) in &variant.env {
            env.insert(k.clone(), v.clone());
        }

        env
    }

    /// Build a plan for something that must outlive this process.
    ///
    /// This is what a menu entry needs: the launcher hands the application over
    /// to the desktop and exits. Under a sandbox the difference is decisive —
    /// a sandbox that dies with its parent takes the application with it.
    pub fn surviving_the_launcher(mut self) -> Self {
        self.survives_the_launcher = true;
        self
    }

    /// Host directories the sandbox has to expose for this command to work.
    ///
    /// The sandbox hides the rest of `$HOME`, which is the point of it — but it
    /// hides two things Wine genuinely needs:
    ///
    /// * the Wine build itself, when WinDrop downloaded one instead of the
    ///   distribution installing it, because managed builds live under the data
    ///   directory rather than under `/usr`;
    /// * the installer the user just dropped, which is normally in their
    ///   downloads directory — or in `/tmp`, which is where a browser's "Save
    ///   as" dialog and most file managers put it.
    ///
    /// Both are shared read-only, which is all either of them needs.
    fn paths_needed_inside_the_sandbox(&self, target: &LaunchTarget) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Some(root) = wine_installation_root(&self.wine.executable) {
            dirs.push(root);
        }
        if let LaunchTarget::HostFile { host_path, .. } = target {
            if let Some(parent) = host_path.parent() {
                dirs.push(parent.to_path_buf());
            }
        }
        dirs.sort();
        dirs.dedup();
        dirs
    }

    /// Build a plan using the configured sandbox mode.
    pub fn build(
        &self,
        prefix: &PrefixPaths,
        variant: &RuntimeEnv,
        target: LaunchTarget,
        log_file: &Path,
    ) -> Result<LaunchPlan> {
        self.build_with(
            prefix,
            variant,
            target,
            log_file,
            sandbox::availability(self.config.sandbox),
        )
    }

    /// Build a plan with an explicit sandbox status, for tests and for the
    /// GUI's "what would happen if..." preview.
    pub fn build_with(
        &self,
        prefix: &PrefixPaths,
        variant: &RuntimeEnv,
        target: LaunchTarget,
        log_file: &Path,
        sandbox_status: SandboxAvailability,
    ) -> Result<LaunchPlan> {
        if !self.wine.supports(variant.arch) {
            return Err(crate::Error::NoWineVariant(format!(
                "{} Wine for {}",
                self.wine.display(),
                variant.arch
            )));
        }
        let _ = log_file;

        let env = self.base_env(prefix, variant);

        // Translate the target into Wine's argument list.
        let (wine_args, cwd) = match &target {
            LaunchTarget::HostFile {
                host_path,
                input_kind,
                extra_args,
            } => {
                // The file lives outside the prefix, so it is addressed through
                // Wine's Z: drive, which maps to the host filesystem root.
                let windows_path =
                    target
                        .windows_path(prefix)
                        .ok_or_else(|| crate::Error::InputMissing {
                            path: host_path.clone(),
                        })?;
                let mut args: Vec<std::ffi::OsString> = input_kind.wine_args(&windows_path);
                args.extend(extra_args.iter().map(std::ffi::OsString::from));
                (args, host_path.parent().map(|p| p.to_path_buf()))
            }
            LaunchTarget::Installed {
                windows_path,
                extra_args,
            } => {
                let mut args = vec![std::ffi::OsString::from(windows_path)];
                args.extend(extra_args.iter().map(std::ffi::OsString::from));
                // Many applications only work when started from their own
                // directory, so make that the working directory.
                let cwd = prefix
                    .host_path_of(windows_path)
                    .and_then(|p| p.parent().map(|p| p.to_path_buf()));
                (args, cwd)
            }
        };

        let mut spec = CommandSpec::new(&self.wine.executable).args(wine_args);
        for (k, v) in &env {
            spec = spec.env(k, v);
        }
        // Never inherit the host's Wine configuration by accident.
        spec = spec.env_remove("WINEARCH");
        if let Some(arch) = env.get("WINEARCH") {
            spec = spec.env("WINEARCH", arch.clone());
        }
        if let Some(dir) = cwd {
            spec = spec.cwd(dir);
        }

        let rationale = variant.rationale.clone();
        let (spec, sandboxed) = match &sandbox_status {
            SandboxAvailability::Available(bwrap) => {
                let request = SandboxRequest::new(bwrap.clone(), prefix.clone())
                    // Network access is granted to everything, installer or
                    // program. A browser, a chat client, a game or anything with
                    // an updater is broken without it, and it is broken in a way
                    // the user cannot diagnose — "the internet does not work in
                    // this one application". What the sandbox withholds is the
                    // *filesystem*, which is what WinDrop promises.
                    .with_network(true)
                    .with_shared_folders(self.config.shared_folders.clone())
                    .with_read_only(self.paths_needed_inside_the_sandbox(&target));
                // A detached launch must survive the process that started it.
                let request = if self.survives_the_launcher {
                    request.surviving_the_launcher()
                } else {
                    request
                };
                (sandbox::wrap(spec, &request), true)
            }
            SandboxAvailability::Unavailable(reason) => {
                if self.config.sandbox == SandboxMode::Strict {
                    tracing::warn!(reason = %reason, "running without a sandbox");
                }
                (spec, false)
            }
        };

        Ok(LaunchPlan {
            spec,
            prefix: prefix.clone(),
            wine: self.wine.clone(),
            sandboxed,
            rationale,
            target: target.describe(),
        })
    }
}

/// The installation root of a Wine build.
///
/// Wine is laid out as `<root>/bin/wine` with its libraries and data beside
/// `bin`, so exposing only the binary's own directory would leave the build
/// unable to find itself. Every build WinDrop meets uses this layout: the
/// distribution package, a `wine-tkg` build, and a managed download.
pub fn wine_installation_root(wine_binary: &Path) -> Option<PathBuf> {
    let parent = wine_binary.parent()?;
    if parent
        .file_name()
        .map(|name| name == "bin")
        .unwrap_or(false)
    {
        return parent.parent().map(|root| root.to_path_buf());
    }
    Some(parent.to_path_buf())
}

impl LaunchTarget {
    /// The Windows path this target is addressed by inside the prefix.
    pub fn windows_path(&self, prefix: &PrefixPaths) -> Option<String> {
        match self {
            LaunchTarget::HostFile { host_path, .. } => prefix.windows_path_of(host_path),
            LaunchTarget::Installed { windows_path, .. } => Some(windows_path.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    // Setting one field on a default is the clearest way to say "defaults,
    // except this"; the lint is aimed at production code, where it usually
    // means a missing derive.
    #![allow(clippy::field_reassign_with_default)]
    use super::*;
    use crate::compat::profile::{DependencySpec, WindowsVersion};
    use crate::runtime::wine::WineSource;

    fn wine() -> WineInstall {
        WineInstall {
            executable: PathBuf::from("/data/runtime/wine/wine-9.0/bin/wine"),
            version: "9.0".into(),
            flavour: String::new(),
            variant_label: "stable".into(),
            source: WineSource::System,
        }
    }

    fn variant(arch: Arch) -> RuntimeEnv {
        RuntimeEnv {
            wine_build: "stable".into(),
            arch,
            windows_version: WindowsVersion::Win10,
            dxvk: false,
            vkd3d_proton: false,
            dll_overrides: vec![],
            env: vec![],
            dependencies: vec![DependencySpec::new("vcrun2022", "test")],
            rationale: "test variant".into(),
        }
    }

    fn prefix() -> PrefixPaths {
        PrefixPaths::new("/data/apps/app")
    }

    fn no_sandbox() -> SandboxAvailability {
        SandboxAvailability::Unavailable("test".into())
    }

    fn build(config: &Config, variant: &RuntimeEnv, target: LaunchTarget) -> LaunchPlan {
        let builder = EnvironmentBuilder::new(config, wine());
        builder
            .build_with(
                &prefix(),
                variant,
                target,
                Path::new("/tmp/run.log"),
                no_sandbox(),
            )
            .unwrap()
    }

    #[test]
    fn an_installer_runs_through_the_wine_binary() {
        let config = Config::default();
        let plan = build(
            &config,
            &variant(Arch::X86_64),
            LaunchTarget::host_file("/home/u/setup.exe", InputKind::Exe),
        );
        assert_eq!(
            plan.spec.program,
            PathBuf::from("/data/runtime/wine/wine-9.0/bin/wine")
        );
        assert_eq!(plan.spec.args.len(), 1);
        assert_eq!(
            plan.spec.args[0],
            std::ffi::OsString::from("Z:\\home\\u\\setup.exe")
        );
    }

    #[test]
    fn an_msi_is_invoked_through_msiexec() {
        let plan = build(
            &Config::default(),
            &variant(Arch::X86_64),
            LaunchTarget::host_file("/home/u/pkg.msi", InputKind::Msi),
        );
        let args: Vec<String> = plan
            .spec
            .args
            .iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args[0], "msiexec");
        assert_eq!(args[1], "/i");
    }

    #[test]
    fn the_prefix_is_pinned_and_wine_is_quiet() {
        let plan = build(
            &Config::default(),
            &variant(Arch::X86_64),
            LaunchTarget::host_file("/home/u/a.exe", InputKind::Exe),
        );
        assert_eq!(plan.env_of("WINEPREFIX").unwrap(), "/data/apps/app/prefix");
        assert_eq!(plan.env_of("WINEDEBUG").unwrap(), "-all");
        assert_eq!(plan.env_of("WINEARCH").unwrap(), "win64");
    }

    #[test]
    fn a_32_bit_variant_asks_for_a_win32_prefix() {
        let plan = build(
            &Config::default(),
            &variant(Arch::X86),
            LaunchTarget::host_file("/home/u/a.exe", InputKind::Exe),
        );
        assert_eq!(plan.env_of("WINEARCH").unwrap(), "win32");
    }

    #[test]
    fn the_home_directory_is_redirected_into_the_prefix() {
        let plan = build(
            &Config::default(),
            &variant(Arch::X86_64),
            LaunchTarget::host_file("/home/u/a.exe", InputKind::Exe),
        );
        assert_eq!(plan.env_of("HOME").unwrap(), "/data/apps/app/prefix/home");
    }

    #[test]
    fn dll_overrides_are_passed_through() {
        let mut v = variant(Arch::X86_64);
        v.dll_overrides = vec!["d3d11=n,b".into(), "dxgi=n,b".into()];
        let plan = build(
            &Config::default(),
            &v,
            LaunchTarget::host_file("/home/u/a.exe", InputKind::Exe),
        );
        assert_eq!(
            plan.env_of("WINEDLLOVERRIDES").unwrap(),
            "d3d11=n,b;dxgi=n,b"
        );
    }

    #[test]
    fn esync_and_fsync_follow_the_configuration() {
        let mut config = Config::default();
        config.esync = false;
        config.fsync = true;
        let plan = build(
            &config,
            &variant(Arch::X86_64),
            LaunchTarget::host_file("/home/u/a.exe", InputKind::Exe),
        );
        assert!(plan.env_of("WINEESYNC").is_none());
        assert_eq!(plan.env_of("WINEFSYNC").unwrap(), "1");
    }

    #[test]
    fn dxvk_variables_only_appear_when_dxvk_is_active() {
        let mut config = Config::default();
        config.dxvk_settings.hud = "fps".into();
        config.dxvk_settings.max_frame_rate = 60;

        let off = build(
            &config,
            &variant(Arch::X86_64),
            LaunchTarget::host_file("/home/u/a.exe", InputKind::Exe),
        );
        assert!(off.env_of("DXVK_HUD").is_none());

        let mut v = variant(Arch::X86_64);
        v.dxvk = true;
        let on = build(
            &config,
            &v,
            LaunchTarget::host_file("/home/u/a.exe", InputKind::Exe),
        );
        assert_eq!(on.env_of("DXVK_HUD").unwrap(), "fps");
        assert_eq!(on.env_of("DXVK_FRAME_RATE").unwrap(), "60");
    }

    #[test]
    fn the_dxvk_config_file_is_only_referenced_when_it_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = PrefixPaths::new(tmp.path().join("app"));
        crate::runtime::prefix::prepare_directories(&prefix).unwrap();
        let mut v = variant(Arch::X86_64);
        v.dxvk = true;
        let config = Config::default();
        let builder = EnvironmentBuilder::new(&config, wine());

        let absent = builder
            .build_with(
                &prefix,
                &v,
                LaunchTarget::installed("C:\\a.exe"),
                Path::new("/tmp/l"),
                no_sandbox(),
            )
            .unwrap();
        assert!(absent.env_of("DXVK_CONFIG_FILE").is_none());

        std::fs::write(prefix.dxvk_conf(), "dxgi.maxFrameRate = 60\n").unwrap();
        let present = builder
            .build_with(
                &prefix,
                &v,
                LaunchTarget::installed("C:\\a.exe"),
                Path::new("/tmp/l"),
                no_sandbox(),
            )
            .unwrap();
        assert!(present
            .env_of("DXVK_CONFIG_FILE")
            .unwrap()
            .ends_with("dxvk.conf"));
    }

    #[test]
    fn variant_environment_overrides_win() {
        let mut v = variant(Arch::X86_64);
        v.env = vec![
            ("WINEDEBUG".to_string(), "+d3d".to_string()),
            ("VKD3D_CONFIG".to_string(), "dxr".to_string()),
        ];
        let plan = build(
            &Config::default(),
            &v,
            LaunchTarget::host_file("/home/u/a.exe", InputKind::Exe),
        );
        assert_eq!(plan.env_of("WINEDEBUG").unwrap(), "+d3d");
        assert_eq!(plan.env_of("VKD3D_CONFIG").unwrap(), "dxr");
    }

    #[test]
    fn an_installed_program_runs_from_its_own_directory() {
        let plan = build(
            &Config::default(),
            &variant(Arch::X86_64),
            LaunchTarget::installed("C:\\Program Files\\App\\app.exe"),
        );
        assert_eq!(
            plan.spec.args[0],
            std::ffi::OsString::from("C:\\Program Files\\App\\app.exe")
        );
        assert_eq!(
            plan.spec.cwd.as_deref(),
            Some(Path::new("/data/apps/app/prefix/drive_c/Program Files/App"))
        );
    }

    #[test]
    fn extra_arguments_are_appended_after_the_target() {
        let plan = build(
            &Config::default(),
            &variant(Arch::X86_64),
            LaunchTarget::host_file("/home/u/setup.exe", InputKind::Exe)
                .with_args(vec!["/S".into(), "/D=C:\\App".into()]),
        );
        let args: Vec<String> = plan
            .spec
            .args
            .iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args[1], "/S");
        assert_eq!(args[2], "/D=C:\\App");
    }

    #[test]
    fn sandboxing_wraps_the_command_when_available() {
        let config = Config::default();
        let builder = EnvironmentBuilder::new(&config, wine());
        let plan = builder
            .build_with(
                &prefix(),
                &variant(Arch::X86_64),
                LaunchTarget::host_file("/home/u/setup.exe", InputKind::Exe),
                Path::new("/tmp/l"),
                SandboxAvailability::Available(PathBuf::from("/usr/bin/bwrap")),
            )
            .unwrap();
        assert!(plan.sandboxed);
        assert_eq!(plan.spec.program, PathBuf::from("/usr/bin/bwrap"));
        assert!(plan.describe().contains("bubblewrap (strict)"));
    }

    #[test]
    fn a_strict_mode_without_bubblewrap_runs_unsandboxed_and_says_so() {
        let config = Config::default();
        assert_eq!(config.sandbox, SandboxMode::Strict);
        let plan = build(
            &config,
            &variant(Arch::X86_64),
            LaunchTarget::host_file("/home/u/a.exe", InputKind::Exe),
        );
        assert!(!plan.sandboxed);
        assert!(plan.describe().contains("sandbox:  none"));
    }

    #[test]
    fn installing_needs_the_network_but_launching_does_not() {
        let config = Config::default();
        let builder = EnvironmentBuilder::new(&config, wine());

        let installer = builder
            .build_with(
                &prefix(),
                &variant(Arch::X86_64),
                LaunchTarget::host_file("/home/u/setup.exe", InputKind::Exe),
                Path::new("/tmp/l"),
                SandboxAvailability::Available(PathBuf::from("/usr/bin/bwrap")),
            )
            .unwrap();
        assert!(!installer.spec.display().contains("--unshare-net"));

        let installed = builder
            .build_with(
                &prefix(),
                &variant(Arch::X86_64),
                LaunchTarget::installed("C:\\a.exe"),
                Path::new("/tmp/l"),
                SandboxAvailability::Available(PathBuf::from("/usr/bin/bwrap")),
            )
            .unwrap();
        assert!(
            !installed.spec.display().contains("--unshare-net"),
            "a program without the network is useless and the reason is invisible"
        );
    }

    #[test]
    fn an_arm64_variant_is_refused_before_building_a_command() {
        let config = Config::default();
        let builder = EnvironmentBuilder::new(&config, wine());
        let mut v = variant(Arch::X86_64);
        v.arch = Arch::Arm64;
        assert!(matches!(
            builder.build_with(
                &prefix(),
                &v,
                LaunchTarget::installed("C:\\a.exe"),
                Path::new("/tmp/l"),
                no_sandbox()
            ),
            Err(crate::Error::NoWineVariant(_))
        ));
    }

    #[test]
    fn the_builder_does_not_leak_the_hosts_wine_architecture() {
        let plan = build(
            &Config::default(),
            &variant(Arch::X86_64),
            LaunchTarget::host_file("/home/u/a.exe", InputKind::Exe),
        );
        // WINEARCH is removed from the inherited environment and re-set to the
        // variant's value exactly once.
        assert!(plan.spec.env_remove.iter().any(|k| k == "WINEARCH"));
        assert_eq!(plan.env_of("WINEARCH").unwrap(), "win64");
    }

    #[test]
    fn env_pairs_are_sorted_for_stable_display() {
        let plan = build(
            &Config::default(),
            &variant(Arch::X86_64),
            LaunchTarget::host_file("/home/u/a.exe", InputKind::Exe),
        );
        let keys: Vec<String> = plan.env_pairs().into_iter().map(|(k, _)| k).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "BTreeMap ordering must survive into the UI");
    }

    #[test]
    fn the_description_names_the_wine_build_and_prefix() {
        let plan = build(
            &Config::default(),
            &variant(Arch::X86_64),
            LaunchTarget::host_file("/home/u/a.exe", InputKind::Exe),
        );
        let text = plan.describe();
        assert!(text.contains("/data/runtime/wine/wine-9.0/bin/wine"));
        assert!(text.contains("/data/apps/app/prefix"));
        assert!(text.contains("test variant"));
    }
}
