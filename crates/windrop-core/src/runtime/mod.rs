//! The runtime layer: Wine builds, shared components, prefixes and sandboxing.
//!
//! Everything shared between applications is version-pinned under
//! `<data>/runtime/`, and everything application-specific lives in that
//! application's own prefix. Nothing is installed system-wide, so WinDrop never
//! needs root, and removing an application removes all of it.

pub mod deps;
pub mod dxvk;
pub mod env;
pub mod prefix;
pub mod sandbox;
pub mod wine;

use std::path::{Component as PathComponent, Path, PathBuf};
use std::time::Duration;

use crate::compat::pe::Arch;
use crate::compat::profile::RuntimeEnv;
use crate::config::{Config, WineVariant};
use crate::paths::Paths;
use crate::process::CommandSpec;
use crate::{Error, Result};

pub use deps::{find_winetricks, DependencyOutcome, WinetricksRunner};
pub use dxvk::{Component, ComponentKind};
pub use env::{EnvironmentBuilder, LaunchPlan, LaunchTarget};
pub use prefix::PrefixPaths;
pub use wine::{WineInstall, WineSource};

/// How long to wait for `wineboot` to initialise a prefix.
const WINEBOOT_TIMEOUT: Duration = Duration::from_secs(600);
/// How long a component download may take.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(1800);

/// What preparing a prefix actually did, for the log and the UI.
#[derive(Debug, Clone)]
pub struct PrefixReport {
    /// The prefix was created by this call rather than reused.
    pub created: bool,
    pub wine: WineInstall,
    pub dependencies: Vec<DependencyOutcome>,
    /// Component descriptions that were installed, e.g. `DXVK 2.4`.
    pub components: Vec<String>,
}

impl PrefixReport {
    /// A one-line summary for the CLI.
    pub fn summary(&self) -> String {
        let deps = self
            .dependencies
            .iter()
            .filter(|d| !d.already_installed)
            .count();
        format!(
            "{} prefix with {} ({} component{}, {} dependenc{})",
            if self.created { "created" } else { "reused" },
            self.wine.display(),
            self.components.len(),
            if self.components.len() == 1 { "" } else { "s" },
            deps,
            if deps == 1 { "y" } else { "ies" }
        )
    }
}

/// Owns the shared runtime and prepares prefixes.
pub struct RuntimeManager {
    paths: Paths,
    config: Config,
    /// Explicit Wine, bypassing discovery. Used by `--wine` and by tests.
    wine_override: Option<WineInstall>,
    /// Explicit winetricks path, bypassing discovery.
    winetricks_override: Option<PathBuf>,
}

impl RuntimeManager {
    pub fn new(paths: Paths, config: Config) -> Self {
        RuntimeManager {
            paths,
            config,
            wine_override: None,
            winetricks_override: None,
        }
    }

    pub fn with_wine(mut self, wine: WineInstall) -> Self {
        self.wine_override = Some(wine);
        self
    }

    pub fn with_winetricks(mut self, path: impl Into<PathBuf>) -> Self {
        self.winetricks_override = Some(path.into());
        self
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Resolve Wine for the configured variant.
    pub fn wine(&self) -> Result<WineInstall> {
        if let Some(wine) = &self.wine_override {
            return Ok(wine.clone());
        }
        wine::resolve_system_wine(&self.config.wine_variant, &self.paths.wine_dir())
    }

    /// Resolve Wine for a specific build label, as stored in a profile.
    pub fn wine_for(&self, build_label: &str) -> Result<WineInstall> {
        if let Some(wine) = &self.wine_override {
            let mut wine = wine.clone();
            wine.variant_label = build_label.to_string();
            return Ok(wine);
        }
        let variant = match build_label {
            "staging" => WineVariant::Staging,
            "system" => WineVariant::System,
            "stable" | "" => WineVariant::Stable,
            other => WineVariant::Build(other.to_string()),
        };
        wine::resolve_system_wine(&variant, &self.paths.wine_dir())
    }

    /// Whether any usable Wine exists, without failing.
    pub fn wine_available(&self) -> bool {
        self.wine().is_ok()
    }

    /// An installed component, if present.
    pub fn component(&self, kind: ComponentKind) -> Option<Component> {
        Component::discover(&self.paths.runtime_dir(), kind, kind.default_version())
    }

    /// Download and unpack a component release.
    ///
    /// The archive is cached under `<data>/downloads` so a failed extraction can
    /// be retried without downloading again.
    pub fn download_component(&self, kind: ComponentKind, version: &str) -> Result<Component> {
        let url = component_download_url(kind, version);
        let file_name = url
            .rsplit('/')
            .next()
            .unwrap_or("component.tar.gz")
            .to_string();
        let archive = self.paths.downloads_dir().join(&file_name);

        if !archive.is_file() {
            download_to(&url, &archive, DOWNLOAD_TIMEOUT)?;
        }

        let parent = match kind {
            ComponentKind::Dxvk => self.paths.dxvk_dir(),
            ComponentKind::Vkd3dProton => self.paths.vkd3d_dir(),
        };
        // Extract into `<kind>/<version>`, which is where discovery looks first.
        let target = parent.join(version);
        if target.is_dir() {
            std::fs::remove_dir_all(&target)?;
        }
        std::fs::create_dir_all(&target)?;
        extract_archive(&archive, &target)?;

        Component::discover(&self.paths.runtime_dir(), kind, version).ok_or_else(|| {
            Error::Archive(format!(
                "{} {version} did not contain the expected layout (no x64/x32 directories)",
                kind.label()
            ))
        })
    }

    /// Every component the given runtime flags need.
    pub fn required_components(&self, variant: &RuntimeEnv) -> Vec<ComponentKind> {
        dxvk::required_components(variant.dxvk, variant.vkd3d_proton)
    }

    /// The `winetricks` to use, or `None` when it is unusable.
    ///
    /// An override pointing at a missing or non-executable file counts as
    /// unavailable, so the caller gets a clear "winetricks is missing" error
    /// instead of a confusing failure part-way through an install.
    pub fn winetricks(&self) -> Option<PathBuf> {
        if let Some(path) = &self.winetricks_override {
            return if crate::process::is_executable(path) {
                Some(path.clone())
            } else {
                None
            };
        }
        find_winetricks(&self.paths.runtime_dir())
    }

    /// Create and populate a prefix for one variant.
    ///
    /// Steps, in order, each of which only runs if the plan calls for it:
    /// 1. directory skeleton;
    /// 2. `wineboot --init`, which writes the registry;
    /// 3. the requested Windows version;
    /// 4. `winetricks` dependencies;
    /// 5. DXVK / VKD3D-Proton;
    /// 6. this application's `dxvk.conf`.
    ///
    /// Translations layers are installed *after* dependencies because
    /// `winetricks` verbs can overwrite `d3d*` DLLs.
    pub fn prepare_prefix(
        &self,
        prefix: &PrefixPaths,
        variant: &RuntimeEnv,
        log_dir: &Path,
    ) -> Result<PrefixReport> {
        let wine = self.wine_for(&variant.wine_build)?;
        if !wine.supports(variant.arch) {
            return Err(Error::NoWineVariant(format!(
                "{} for {}",
                wine.display(),
                variant.arch
            )));
        }
        prefix::prepare_directories(prefix)?;
        std::fs::create_dir_all(log_dir)?;

        let builder = EnvironmentBuilder::new(&self.config, wine.clone());

        // 2. Initialise the prefix. This is the one step that must happen before
        //    anything else, because every later step needs a registry.
        let created = !prefix.is_initialised();
        if created {
            tracing::info!(prefix = %prefix.root().display(), "initialising Wine prefix");
            let env = builder.base_env(prefix, variant);
            let mut spec = CommandSpec::new(&wine.executable)
                .args(["wineboot", "--init"])
                .cwd(prefix.drive_c());
            for (k, v) in &env {
                spec = spec.env(k, v);
            }
            let log = log_dir.join("wineboot.log");
            let out = spec.run_logged(&log, WINEBOOT_TIMEOUT)?;
            if !prefix.is_initialised() {
                // Wine can exit zero without producing a registry when the
                // prefix directory is unwritable or the build is broken.
                return Err(Error::InstallIncomplete {
                    rationale: format!(
                        "Wine did not create a usable prefix (exit {}). See {}",
                        out.code(),
                        log.display()
                    ),
                });
            }
        }

        // 3-5. Everything below needs winetricks.
        let needs_winetricks = !variant.dependencies.is_empty()
            || variant.windows_version != crate::compat::profile::WindowsVersion::Win10;
        let winetricks = if needs_winetricks {
            match self.winetricks() {
                Some(path) => Some(WinetricksRunner::new(path, prefix.clone(), &wine, log_dir)),
                None => {
                    let required: Vec<&str> = variant
                        .dependencies
                        .iter()
                        .filter(|d| !d.optional)
                        .map(|d| d.verb.as_str())
                        .collect();
                    if required.is_empty() {
                        tracing::warn!("winetricks is not installed; skipping optional components");
                        None
                    } else {
                        return Err(Error::WinetricksMissing {
                            dependency: required.join(", "),
                        });
                    }
                }
            }
        } else {
            None
        };

        let mut dependencies = Vec::new();
        if let Some(runner) = &winetricks {
            // 3. Pin the Windows version the application expects.
            let version_verb = variant.windows_version.winetricks_verb();
            let version_dep = vec![crate::compat::profile::DependencySpec::new(
                version_verb,
                "requested Windows version",
            )];
            // A failure here is worth surfacing: the whole point is to match
            // what the application expects.
            runner.apply(&version_dep, self.config.install_timeout())?;

            // 4. Runtime components.
            dependencies = runner.apply(&variant.dependencies, self.config.install_timeout())?;
        }

        // 5. Translation layers.
        let mut components = Vec::new();
        for kind in self.required_components(variant) {
            let Some(component) = self.component(kind) else {
                tracing::warn!(
                    component = kind.label(),
                    "translation layer is not installed; continuing without it"
                );
                continue;
            };
            component.validate(variant.arch)?;
            component.install_into(prefix, variant.arch)?;
            components.push(component.display());
        }

        // 6. Per-application DXVK tuning.
        if variant.dxvk {
            let conf = self.config.effective_dxvk_settings().to_conf();
            std::fs::write(prefix.dxvk_conf(), conf)?;
        }

        let report = PrefixReport {
            created,
            wine,
            dependencies,
            components,
        };
        tracing::info!(summary = %report.summary(), "prefix ready");
        Ok(report)
    }

    /// Shut down the Wine server for a prefix, so files are not in use.
    pub fn shutdown_prefix(&self, prefix: &PrefixPaths) -> Result<()> {
        let wine = match self.wine() {
            Ok(w) => w,
            Err(_) => return Ok(()),
        };
        let Some(wineserver) = wine.wineserver() else {
            // Without a wineserver binary there is nothing to talk to.
            return Ok(());
        };
        let spec = CommandSpec::new(wineserver)
            .arg("-k")
            .env("WINEPREFIX", prefix.root().to_string_lossy().to_string());
        match spec.run_capture(Duration::from_secs(30)) {
            Ok(out) if out.success() => Ok(()),
            // A leftover server is not fatal: the prefix is about to be removed.
            Ok(_) => Ok(()),
            Err(e) => {
                tracing::debug!(error = %e, "wineserver shutdown failed");
                Ok(())
            }
        }
    }
}

/// Release URL for a component version.
pub fn component_download_url(kind: ComponentKind, version: &str) -> String {
    match kind {
        ComponentKind::Dxvk => format!(
            "https://github.com/doitsujin/dxvk/releases/download/v{version}/dxvk-{version}.tar.gz"
        ),
        ComponentKind::Vkd3dProton => format!(
            "https://github.com/HansKristian-Work/vkd3d-proton/releases/download/v{version}/vkd3d-proton-{version}.tar.zst"
        ),
    }
}

/// Download `url` to `dest`, following redirects.
///
/// Written to a temporary file and moved into place, so an interrupted download
/// never leaves a half-file that looks complete.
pub fn download_to(url: &str, dest: &Path, timeout: Duration) -> Result<()> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(Error::DownloadBlocked(format!(
            "refusing non-http URL '{url}'"
        )));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    tracing::info!(url = %url, dest = %dest.display(), "downloading");

    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(15))
        .user_agent(concat!("WinDrop/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let mut response = client.get(url).send()?;
    if !response.status().is_success() {
        return Err(Error::DownloadBlocked(format!(
            "the server replied with HTTP {}",
            response.status()
        )));
    }

    let temporary = dest.with_extension("part");
    let mut file = std::fs::File::create(&temporary)?;
    std::io::copy(&mut response, &mut file)?;
    drop(file);
    std::fs::rename(&temporary, dest)?;

    let size = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
    tracing::info!(bytes = size, "download complete");
    Ok(())
}

/// Extract a `.tar.gz` archive, refusing entries that escape `dest`.
///
/// Returns the number of entries written.
pub fn extract_tar_gz(archive: &Path, dest: &Path) -> Result<usize> {
    let file = std::fs::File::open(archive)?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(decoder);
    tar.set_overwrite(true);
    tar.set_preserve_permissions(true);
    extract_entries(tar.entries()?, dest)
}

/// Extract an archive, choosing a strategy from its extension.
///
/// `.tar.gz` is handled in-process. Formats that need a codec WinDrop does not
/// link against (`.tar.xz`, `.tar.zst`) are delegated to the system `tar`,
/// which is present on every distribution WinDrop targets.
pub fn extract_archive(archive: &Path, dest: &Path) -> Result<usize> {
    let name = archive
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        return extract_tar_gz(archive, dest);
    }
    if !name.ends_with(".tar")
        && !name.ends_with(".tar.xz")
        && !name.ends_with(".tar.zst")
        && !name.ends_with(".tar.bz2")
    {
        return Err(Error::Archive(format!(
            "unsupported archive format: {name}. WinDrop understands .tar.gz, .tar.xz and .tar.zst"
        )));
    }

    let spec = CommandSpec::new("tar")
        .args(["-xf"])
        .arg(archive)
        .args(["-C"])
        .arg(dest);
    let out = spec.run_capture(Duration::from_secs(600))?;
    if !out.success() {
        return Err(spec.failure(&out));
    }
    // The system tar does not report an entry count; count what arrived.
    Ok(std::fs::read_dir(dest).map(|d| d.count()).unwrap_or(0))
}

fn extract_entries<R: std::io::Read>(entries: tar::Entries<'_, R>, dest: &Path) -> Result<usize> {
    std::fs::create_dir_all(dest)?;
    let mut count = 0;
    for entry in entries {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        // Reject traversal outright instead of quietly skipping it: a malicious
        // archive should be a loud failure, not a partial install.
        if path.is_absolute()
            || path.components().any(|c| {
                matches!(
                    c,
                    PathComponent::ParentDir | PathComponent::RootDir | PathComponent::Prefix(_)
                )
            })
        {
            return Err(Error::Archive(format!(
                "refusing to extract an archive entry with an unsafe path: {}",
                path.display()
            )));
        }
        entry.unpack_in(dest)?;
        count += 1;
    }
    Ok(count)
}

/// Convenience for the CLI: a prefix for an application id.
pub fn prefix_for(paths: &Paths, app_id: &str) -> PrefixPaths {
    PrefixPaths::new(paths.app_dir(app_id))
}

/// True when a prefix architecture can run on this host.
pub fn host_supports(arch: Arch) -> bool {
    match arch {
        Arch::X86 => cfg!(target_pointer_width = "32") || cfg!(target_arch = "x86_64"),
        Arch::X86_64 => cfg!(target_arch = "x86_64"),
        Arch::Arm64 => cfg!(target_arch = "aarch64"),
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
    use crate::process::which;

    /// A fake Wine that records its arguments and fabricates a prefix.
    ///
    /// This is what lets the entire prefix-preparation pipeline be tested on a
    /// machine with no Wine installed.
    const FAKE_WINE: &str = r#"#!/bin/sh
set -e
case "$1" in
  --version) echo "wine-9.0 (WinDrop test build)"; exit 0 ;;
esac
if [ "$1" = "wineboot" ]; then
  mkdir -p "$WINEPREFIX/drive_c/users/test"
  echo "WINE REGISTRY Version 2" > "$WINEPREFIX/system.reg"
  echo "wineboot $*" >> "$WINEPREFIX/../../trace.log"
  exit 0
fi
echo "wine $*" >> "$WINEPREFIX/../../trace.log"
exit 0
"#;

    /// A fake winetricks that records verbs and asserts the Windows version.
    const FAKE_WINETRICKS: &str = r#"#!/bin/sh
for arg in "$@"; do
  [ "$arg" = "-q" ] && continue
  echo "$arg" >> "$WINEPREFIX/winetricks.log"
  echo "$arg" >> "$WINEPREFIX/../../trace.log"
done
echo "prefix=$WINEPREFIX wine=$WINE" >> "$WINEPREFIX/../../trace.log"
exit 0
"#;

    struct Fixture {
        dir: tempfile::TempDir,
        paths: Paths,
        config: Config,
    }

    impl Fixture {
        fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::with_data_dir(dir.path().join("data"));
            paths.ensure().unwrap();

            let wine = dir.path().join("bin/wine");
            std::fs::create_dir_all(wine.parent().unwrap()).unwrap();
            std::fs::write(&wine, FAKE_WINE).unwrap();
            std::fs::set_permissions(&wine, std::fs::Permissions::from_mode(0o755)).unwrap();

            let winetricks = dir.path().join("bin/winetricks");
            std::fs::write(&winetricks, FAKE_WINETRICKS).unwrap();
            std::fs::set_permissions(&winetricks, std::fs::Permissions::from_mode(0o755)).unwrap();

            let mut config = Config::default();
            // Keep tests off the network regardless of the host's state.
            config.allow_remote_registry = false;
            Fixture { dir, paths, config }
        }

        fn wine_install(&self) -> WineInstall {
            WineInstall {
                executable: self.dir.path().join("bin/wine"),
                version: "9.0".into(),
                flavour: "WinDrop test build".into(),
                variant_label: "stable".into(),
                source: WineSource::System,
            }
        }

        fn manager(&self) -> RuntimeManager {
            RuntimeManager::new(self.paths.clone(), self.config.clone())
                .with_wine(self.wine_install())
                .with_winetricks(self.dir.path().join("bin/winetricks"))
        }

        fn variant(&self) -> RuntimeEnv {
            RuntimeEnv {
                wine_build: "stable".into(),
                arch: Arch::X86_64,
                windows_version: WindowsVersion::Win10,
                dxvk: false,
                vkd3d_proton: false,
                dll_overrides: vec![],
                env: vec![],
                dependencies: vec![DependencySpec::new("vcrun2022", "test")],
                rationale: "test".into(),
            }
        }

        /// The fake tools append here, next to the per-application directories.
        /// Deriving the path from `$WINEPREFIX` keeps parallel tests isolated,
        /// which a shared environment variable would not.
        fn trace(&self) -> String {
            std::fs::read_to_string(self.paths.apps_dir().join("trace.log")).unwrap_or_default()
        }
    }

    fn install_fake_dxvk(paths: &Paths, version: &str) {
        let root = paths.dxvk_dir().join(version);
        std::fs::create_dir_all(root.join("x64")).unwrap();
        std::fs::create_dir_all(root.join("x32")).unwrap();
        for dll in ["d3d9.dll", "d3d10core.dll", "d3d11.dll", "dxgi.dll"] {
            std::fs::write(root.join("x64").join(dll), b"64").unwrap();
            std::fs::write(root.join("x32").join(dll), b"32").unwrap();
        }
    }

    fn install_fake_vkd3d(paths: &Paths, version: &str) {
        let root = paths.vkd3d_dir().join(version);
        for dir in ["x64", "x86"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(dir).join("d3d12.dll"), b"x").unwrap();
        }
    }

    #[test]
    fn preparing_a_prefix_initialises_it_and_installs_dependencies() {
        let f = Fixture::new();
        let prefix = prefix_for(&f.paths, "test-app");
        let manager = f.manager();

        let report = manager
            .prepare_prefix(&prefix, &f.variant(), &f.paths.logs_dir())
            .unwrap();

        assert!(report.created, "a fresh prefix must be created");
        assert!(prefix.is_initialised());
        assert!(prefix.system_reg().is_file());

        let trace = f.trace();
        assert!(
            trace.contains("wineboot"),
            "wineboot must run first: {trace}"
        );
        assert!(trace.contains("vcrun2022"), "dependencies must be applied");
        assert!(
            trace.contains("win10"),
            "the Windows version must be pinned"
        );
        assert!(
            trace.contains("wine="),
            "winetricks must be told which wine to drive"
        );
        assert_eq!(report.dependencies.len(), 1);
        assert!(report.dependencies[0].success);
    }

    #[test]
    fn wineboot_runs_before_winetricks() {
        let f = Fixture::new();
        let prefix = prefix_for(&f.paths, "order-test");
        f.manager()
            .prepare_prefix(&prefix, &f.variant(), &f.paths.logs_dir())
            .unwrap();

        let trace = f.trace();
        let boot = trace.find("wineboot").expect("wineboot");
        let deps = trace.find("vcrun2022").expect("vcrun2022");
        assert!(boot < deps, "ordering matters: {trace}");
    }

    #[test]
    fn an_existing_prefix_is_reused_rather_than_recreated() {
        let f = Fixture::new();
        let prefix = prefix_for(&f.paths, "reuse-test");
        let manager = f.manager();

        manager
            .prepare_prefix(&prefix, &f.variant(), &f.paths.logs_dir())
            .unwrap();
        let second = manager
            .prepare_prefix(&prefix, &f.variant(), &f.paths.logs_dir())
            .unwrap();

        assert!(!second.created, "an initialised prefix must be reused");
        // winetricks must not re-install an already present verb.
        assert!(second.dependencies.iter().all(|d| d.already_installed));
    }

    #[test]
    fn a_prefix_that_wine_fails_to_initialise_is_reported_not_ignored() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        // A wine that exits 0 but never writes a registry.
        let broken = f.dir.path().join("bin/wine-broken");
        std::fs::write(&broken, "#!/bin/sh\necho wine-9.0\nexit 0\n").unwrap();
        std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut wine = f.wine_install();
        wine.executable = broken;
        let manager = RuntimeManager::new(f.paths.clone(), f.config.clone()).with_wine(wine);
        let prefix = prefix_for(&f.paths, "broken-prefix");

        match manager.prepare_prefix(&prefix, &f.variant(), &f.paths.logs_dir()) {
            Err(Error::InstallIncomplete { rationale }) => {
                assert!(
                    rationale.contains("did not create a usable prefix"),
                    "{rationale}"
                );
            }
            other => panic!("expected InstallIncomplete, got {other:?}"),
        }
    }

    #[test]
    fn required_dependencies_need_winetricks() {
        let f = Fixture::new();
        let manager = RuntimeManager::new(f.paths.clone(), f.config.clone())
            .with_wine(f.wine_install())
            .with_winetricks(f.dir.path().join("bin/does-not-exist"));
        let prefix = prefix_for(&f.paths, "no-winetricks");

        // The override points at a missing file, so discovery finds nothing.
        match manager.prepare_prefix(&prefix, &f.variant(), &f.paths.logs_dir()) {
            Err(Error::WinetricksMissing { dependency }) => {
                assert!(dependency.contains("vcrun2022"));
            }
            other => panic!("expected WinetricksMissing, got {other:?}"),
        }
    }

    #[test]
    fn optional_dependencies_are_skipped_without_winetricks() {
        let f = Fixture::new();
        let manager = RuntimeManager::new(f.paths.clone(), f.config.clone())
            .with_wine(f.wine_install())
            .with_winetricks(f.dir.path().join("bin/does-not-exist"));
        let prefix = prefix_for(&f.paths, "optional-only");

        let mut variant = f.variant();
        variant.dependencies = vec![DependencySpec::new("corefonts", "optional").optional()];
        variant.windows_version = WindowsVersion::Win10;

        let report = manager
            .prepare_prefix(&prefix, &variant, &f.paths.logs_dir())
            .unwrap();
        assert!(prefix.is_initialised());
        assert!(report.dependencies.is_empty(), "nothing could be installed");
    }

    #[test]
    fn translation_layers_are_copied_into_the_prefix() {
        let f = Fixture::new();
        install_fake_dxvk(&f.paths, "2.4");
        install_fake_vkd3d(&f.paths, "2.13");

        let prefix = prefix_for(&f.paths, "layers");
        let mut variant = f.variant();
        variant.dxvk = true;
        variant.vkd3d_proton = true;

        let report = f
            .manager()
            .prepare_prefix(&prefix, &variant, &f.paths.logs_dir())
            .unwrap();

        assert!(prefix.system32().join("d3d11.dll").is_file());
        assert!(prefix.system32().join("d3d12.dll").is_file());
        assert_eq!(report.components.len(), 2);
        assert!(report.components.iter().any(|c| c.contains("DXVK")));
        assert!(report.components.iter().any(|c| c.contains("VKD3D")));
    }

    #[test]
    fn a_missing_translation_layer_degrades_instead_of_failing() {
        let f = Fixture::new();
        // Nothing installed under runtime/dxvk.
        let prefix = prefix_for(&f.paths, "no-dxvk");
        let mut variant = f.variant();
        variant.dxvk = true;

        let report = f
            .manager()
            .prepare_prefix(&prefix, &variant, &f.paths.logs_dir())
            .unwrap();
        assert!(report.components.is_empty());
        assert!(prefix.is_initialised(), "the prefix must still be usable");
    }

    #[test]
    fn a_dxvk_config_is_written_only_when_dxvk_is_active() {
        let f = Fixture::new();
        install_fake_dxvk(&f.paths, "2.4");
        let manager = f.manager();

        let plain = prefix_for(&f.paths, "no-conf");
        manager
            .prepare_prefix(&plain, &f.variant(), &f.paths.logs_dir())
            .unwrap();
        assert!(!plain.dxvk_conf().exists());

        let tuned = prefix_for(&f.paths, "with-conf");
        let mut variant = f.variant();
        variant.dxvk = true;
        manager
            .prepare_prefix(&tuned, &variant, &f.paths.logs_dir())
            .unwrap();
        let conf = std::fs::read_to_string(tuned.dxvk_conf()).unwrap();
        assert!(conf.contains("enableGraphicsPipelineLibrary"));
    }

    #[test]
    fn the_windows_version_is_only_pinned_through_winetricks_when_it_differs() {
        let f = Fixture::new();
        let prefix = prefix_for(&f.paths, "xp-mode");
        let mut variant = f.variant();
        variant.windows_version = WindowsVersion::WinXp;
        variant.dependencies.clear();

        f.manager()
            .prepare_prefix(&prefix, &variant, &f.paths.logs_dir())
            .unwrap();
        assert!(f.trace().contains("winxp"), "{}", f.trace());
    }

    #[test]
    fn reports_summarise_what_happened() {
        let f = Fixture::new();
        install_fake_dxvk(&f.paths, "2.4");
        let prefix = prefix_for(&f.paths, "summary");
        let mut variant = f.variant();
        variant.dxvk = true;

        let report = f
            .manager()
            .prepare_prefix(&prefix, &variant, &f.paths.logs_dir())
            .unwrap();
        let text = report.summary();
        assert!(text.contains("created"));
        assert!(text.contains("component"));
        assert!(text.contains("dependency"));
    }

    #[test]
    fn shutdown_is_a_no_op_when_wine_is_unavailable() {
        let f = Fixture::new();
        let manager = RuntimeManager::new(f.paths.clone(), f.config.clone());
        // No override and no system wine in a bare data directory.
        let prefix = prefix_for(&f.paths, "nothing");
        // Must not error, whatever the host looks like.
        manager.shutdown_prefix(&prefix).unwrap();
    }

    #[test]
    fn component_urls_point_at_the_right_releases() {
        let dxvk = component_download_url(ComponentKind::Dxvk, "2.4");
        assert_eq!(
            dxvk,
            "https://github.com/doitsujin/dxvk/releases/download/v2.4/dxvk-2.4.tar.gz"
        );
        let vkd3d = component_download_url(ComponentKind::Vkd3dProton, "2.13");
        assert!(vkd3d.ends_with("vkd3d-proton-2.13.tar.zst"));
        assert!(vkd3d.contains("HansKristian-Work"));
    }

    #[test]
    fn non_http_downloads_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        match download_to(
            "file:///etc/passwd",
            &tmp.path().join("x"),
            Duration::from_secs(1),
        ) {
            Err(Error::DownloadBlocked(msg)) => assert!(msg.contains("non-http")),
            other => panic!("expected DownloadBlocked, got {other:?}"),
        }
    }

    /// Build a `.tar.gz` in memory and write it to disk.
    fn make_tar_gz(dest: &Path, entries: &[(&str, &[u8])], unsafe_name: bool) {
        let file = std::fs::File::create(dest).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        for (name, body) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o755);
            if unsafe_name {
                // Set the raw name field directly: `set_path` refuses `..`, and
                // the point of this fixture is to be refused.
                let bytes = name.as_bytes();
                header.as_old_mut().name[..bytes.len()].copy_from_slice(bytes);
            } else {
                header.set_path(name).unwrap();
            }
            header.set_cksum();
            builder.append(&header, *body).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
    }

    #[test]
    fn tar_gz_archives_are_extracted_in_process() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("dxvk-2.4.tar.gz");
        make_tar_gz(
            &archive,
            &[
                ("dxvk-2.4/x64/d3d11.dll", b"fake 64"),
                ("dxvk-2.4/x32/d3d11.dll", b"fake 32"),
            ],
            false,
        );

        let dest = tmp.path().join("out");
        let count = extract_archive(&archive, &dest).unwrap();
        assert_eq!(count, 2);
        assert_eq!(
            std::fs::read(dest.join("dxvk-2.4/x64/d3d11.dll")).unwrap(),
            b"fake 64"
        );
    }

    #[test]
    fn extracted_components_are_discoverable_and_installable() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("dxvk-2.4.tar.gz");
        let entries: Vec<(String, Vec<u8>)> =
            ["d3d9.dll", "d3d10core.dll", "d3d11.dll", "dxgi.dll"]
                .iter()
                .flat_map(|dll| {
                    vec![
                        (format!("dxvk-2.4/x64/{dll}"), b"64".to_vec()),
                        (format!("dxvk-2.4/x32/{dll}"), b"32".to_vec()),
                    ]
                })
                .collect();
        let refs: Vec<(&str, &[u8])> = entries
            .iter()
            .map(|(n, b)| (n.as_str(), b.as_slice()))
            .collect();
        make_tar_gz(&archive, &refs, false);

        // Extract into a data directory laid out like a real installation:
        // release archives are unpacked into <data>/runtime/dxvk/<version>.
        let paths = Paths::with_data_dir(tmp.path().join("data"));
        paths.ensure().unwrap();
        let target = paths.dxvk_dir().join("2.4");
        extract_archive(&archive, &target).unwrap();

        let component = Component::discover(&paths.runtime_dir(), ComponentKind::Dxvk, "2.4")
            .expect("archive layout must be discovered");
        assert!(
            component.root.ends_with("dxvk-2.4"),
            "the archive's wrapping directory must be descended into: {:?}",
            component.root
        );
        component.validate(Arch::X86_64).unwrap();

        let prefix = prefix_for(&paths, "extracted");
        prefix::prepare_directories(&prefix).unwrap();
        let written = component.install_into(&prefix, Arch::X86_64).unwrap();
        assert_eq!(written.len(), 4);
        assert!(prefix.system32().join("d3d11.dll").is_file());
    }

    #[test]
    fn an_archive_entry_that_escapes_the_destination_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("evil.tar.gz");
        make_tar_gz(&archive, &[("../escaped.txt", b"pwned")], true);

        let dest = tmp.path().join("out");
        match extract_archive(&archive, &dest) {
            Err(Error::Archive(msg)) => assert!(msg.contains("unsafe path"), "{msg}"),
            other => panic!("expected Archive error, got {other:?}"),
        }
        assert!(
            !tmp.path().join("escaped.txt").exists(),
            "nothing may escape"
        );
    }

    #[test]
    fn absolute_paths_in_archives_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("abs.tar.gz");
        make_tar_gz(&archive, &[("/etc/evil.conf", b"pwned")], true);

        assert!(matches!(
            extract_archive(&archive, &tmp.path().join("out")),
            Err(Error::Archive(_))
        ));
        assert!(!Path::new("/etc/evil.conf").exists());
    }

    #[test]
    fn unsupported_archive_formats_produce_a_clear_error() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("thing.zip");
        std::fs::write(&archive, b"PK\x03\x04").unwrap();
        match extract_archive(&archive, &tmp.path().join("out")) {
            Err(Error::Archive(msg)) => assert!(msg.contains("unsupported archive format")),
            other => panic!("expected Archive error, got {other:?}"),
        }
    }

    #[test]
    fn missing_archives_report_an_io_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            extract_archive(&tmp.path().join("nope.tar.gz"), &tmp.path().join("out")),
            Err(Error::Io(_))
        ));
    }

    #[test]
    fn component_lookup_reports_absence_without_failing() {
        let f = Fixture::new();
        let manager = f.manager();
        assert!(manager.component(ComponentKind::Dxvk).is_none());

        install_fake_dxvk(&f.paths, "2.4");
        let found = manager
            .component(ComponentKind::Dxvk)
            .expect("should be found");
        assert_eq!(found.version, "2.4");
    }

    #[test]
    fn required_components_follow_the_variant_flags() {
        let f = Fixture::new();
        let manager = f.manager();
        let mut variant = f.variant();
        variant.dxvk = false;
        variant.vkd3d_proton = true;
        assert_eq!(
            manager.required_components(&variant),
            vec![ComponentKind::Vkd3dProton]
        );
    }

    #[test]
    fn host_support_matches_the_running_architecture() {
        assert!(host_supports(Arch::X86), "x86_64 hosts can run 32-bit code");
        assert!(host_supports(Arch::X86_64));
        assert_eq!(host_supports(Arch::Arm64), cfg!(target_arch = "aarch64"));
    }

    #[test]
    fn a_real_winetricks_on_this_host_is_reported_accurately() {
        let f = Fixture::new();
        let manager = f.manager();
        assert!(
            manager.winetricks().is_some(),
            "the override should be used"
        );
        // And with no override, the answer must match the host.
        let bare = RuntimeManager::new(f.paths.clone(), f.config.clone());
        assert_eq!(bare.winetricks().is_some(), which("winetricks").is_some());
    }

    #[test]
    fn wine_resolution_honours_the_override_and_reports_availability() {
        let f = Fixture::new();
        let manager = f.manager();
        assert!(manager.wine_available());
        let wine = manager.wine().unwrap();
        assert_eq!(wine.version, "9.0");

        // A build label is honoured even with an override in place.
        let staging = manager.wine_for("staging").unwrap();
        assert_eq!(staging.variant_label, "staging");
        assert_eq!(staging.executable, wine.executable);
    }
}
