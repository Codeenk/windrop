//! The application manager: the pipeline that turns a dropped executable into
//! a launchable menu entry, and back again.
//!
//! ```text
//! install()
//!   ├─ resolve a profile            (compat::CompatibilityEngine)
//!   ├─ pick an application id
//!   ├─ for each variant             (fallback::run_chain)
//!   │    ├─ prepare the prefix      (runtime::RuntimeManager)
//!   │    ├─ run the installer
//!   │    └─ find the real program   (manager::locate)
//!   ├─ extract the icon             (icons)
//!   ├─ write metadata.json
//!   └─ install the desktop entry    (desktop)
//!
//! launch()  -> rebuild the winning environment and run the recorded program
//! remove()  -> stop Wine, delete the application directory and the entry
//! ```
//!
//! Everything an install writes lives under `<data>/apps/<id>/` plus one file in
//! `~/.local/share/applications`, which is what makes `remove()` able to
//! guarantee that nothing is left behind.

pub mod locate;
pub mod metadata;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::compat::engine::CompatibilityEngine;
use crate::compat::profile::{AppProfile, ProfileSource, RuntimeEnv};
use crate::compat::{InputKind, ResolvedProfile};
use crate::config::Config;
use crate::db::ProfileDb;
use crate::desktop::{self, DesktopEntry};
use crate::fallback::{self, ChainOutcome};
use crate::icons::IconExtractor;
use crate::paths::Paths;
use crate::registry::RegistryClient;
use crate::runtime::{
    prefix::PrefixPaths, EnvironmentBuilder, LaunchPlan, LaunchTarget, PrefixReport, RuntimeManager,
};
use crate::{Error, Result};

pub use metadata::InstalledApp;

/// A progress callback, called from the installing thread.
pub type ProgressFn = Arc<dyn Fn(InstallStage) + Send + Sync>;

/// Milestones an install passes through, for the GUI's progress display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallStage {
    Inspecting,
    Resolved {
        profile_id: String,
        source: ProfileSource,
        variants: usize,
    },
    PreparingPrefix {
        attempt: usize,
        of: usize,
        strategy: String,
    },
    RunningInstaller {
        attempt: usize,
        of: usize,
    },
    LocatingProgram {
        attempt: usize,
    },
    ExtractionDone,
    Finished {
        app_id: String,
        name: String,
        attempts: usize,
    },
}

impl InstallStage {
    /// A short line for the status label.
    pub fn label(&self) -> String {
        match self {
            InstallStage::Inspecting => "Reading the executable…".to_string(),
            InstallStage::Resolved {
                variants, source, ..
            } => format!(
                "Found a compatibility profile ({}, {variants} option{})",
                source.label(),
                if *variants == 1 { "" } else { "s" }
            ),
            InstallStage::PreparingPrefix {
                attempt,
                of,
                strategy,
            } => {
                format!("Preparing environment {attempt}/{of}: {strategy}")
            }
            InstallStage::RunningInstaller { attempt, of } => {
                format!("Running the installer ({attempt}/{of})…")
            }
            InstallStage::LocatingProgram { .. } => "Finding the installed program…".to_string(),
            InstallStage::ExtractionDone => "Creating the menu entry…".to_string(),
            InstallStage::Finished { name, .. } => format!("{name} is ready to use"),
        }
    }
}

/// Knobs for a single install.
#[derive(Default)]
pub struct InstallOptions {
    /// Force an application id instead of deriving one.
    pub app_id: Option<String>,
    /// Force a display name.
    pub name_hint: Option<String>,
    /// Pass the profile's silent flags to the installer.
    ///
    /// Attended installs run with inherited stdio so the user can click through;
    /// those are driven by the caller (the GUI), which knows whether a terminal
    /// or a window is available.
    pub unattended: bool,
    /// Resolve everything and report, without running anything.
    pub dry_run: bool,
    /// Only try the first variant.
    pub single_variant: bool,
    /// Use only this variant, matched by signature.
    pub force_variant: Option<String>,
    /// Choose the program explicitly instead of relying on heuristics.
    pub main_exe: Option<PathBuf>,
    /// Use this profile instead of resolving one for the file.
    ///
    /// This is how a bundled recipe can be applied to an application WinDrop
    /// would never have matched by itself — a Windows XP era game, or an
    /// installer whose name says nothing.
    pub profile_id: Option<String>,
    /// Index into the variant chain, used by the GUI's "try another approach".
    pub progress: Option<ProgressFn>,
}

impl InstallOptions {
    fn report(&self, stage: InstallStage) {
        if let Some(callback) = &self.progress {
            callback(stage);
        }
    }

    fn unattended() -> Self {
        InstallOptions {
            unattended: true,
            ..Default::default()
        }
    }
}

impl std::fmt::Debug for InstallOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstallOptions")
            .field("app_id", &self.app_id)
            .field("name_hint", &self.name_hint)
            .field("unattended", &self.unattended)
            .field("dry_run", &self.dry_run)
            .field("single_variant", &self.single_variant)
            .field("force_variant", &self.force_variant)
            .field("main_exe", &self.main_exe)
            .field("profile_id", &self.profile_id)
            .field("has_progress", &self.progress.is_some())
            .finish()
    }
}

/// What an install produced.
#[derive(Debug, Clone)]
pub struct InstallReport {
    pub app: InstalledApp,
    pub profile: AppProfile,
    pub prefix: PrefixReport,
    /// Every variant tried, including failures.
    pub attempts: Vec<String>,
    /// The plan that ran the installer, for the log.
    pub installer_plan: Option<LaunchPlan>,
    pub duration: Duration,
}

impl InstallReport {
    pub fn summary(&self) -> String {
        format!(
            "{} installed as '{}' in {:.1}s using {}",
            self.app.name,
            self.app.id,
            self.duration.as_secs_f64(),
            self.app.strategy()
        )
    }
}

/// What a foreground launch did.
#[derive(Debug, Clone)]
pub struct LaunchOutcome {
    pub plan: LaunchPlan,
    /// The program's exit status.
    ///
    /// Reported rather than interpreted. Windows applications are free to exit
    /// with any status they like, and WinDrop has no business deciding that a
    /// non-zero one means the launch failed — but a launcher that always claims
    /// success is worse, because it hides a program that never started.
    pub code: i32,
    /// Bytes of output the program produced, for the "it exited silently"
    /// diagnosis.
    pub output_bytes: usize,
}

impl LaunchOutcome {
    pub fn success(&self) -> bool {
        self.code == 0
    }
}

/// What a removal did.
#[derive(Debug, Clone)]
pub struct RemovalReport {
    pub app_id: String,
    pub name: String,
    pub freed_bytes: u64,
    pub removed_desktop_entry: bool,
}

impl RemovalReport {
    pub fn summary(&self) -> String {
        format!(
            "removed '{}' ({}), {} desktop {}",
            self.name,
            human_bytes(self.freed_bytes),
            if self.removed_desktop_entry {
                "and its"
            } else {
                "no"
            },
            if self.removed_desktop_entry {
                "entry"
            } else {
                "entry to remove"
            }
        )
    }
}

/// Owns the pieces the pipeline needs.
pub struct ApplicationManager {
    paths: Paths,
    config: Config,
    runtime: RuntimeManager,
    db: ProfileDb,
    icons: IconExtractor,
}

impl ApplicationManager {
    /// Open the manager, creating the data directory and database if needed.
    pub fn new(paths: Paths, config: Config) -> Result<Self> {
        paths.ensure()?;
        let db = ProfileDb::open(&paths.database())?;
        let runtime = RuntimeManager::new(paths.clone(), config.clone());
        Ok(ApplicationManager {
            paths,
            config,
            runtime,
            db,
            icons: IconExtractor::from_system(),
        })
    }

    /// Substitute a prepared runtime manager (tests, or `--wine` on the CLI).
    pub fn with_runtime(mut self, runtime: RuntimeManager) -> Self {
        self.runtime = runtime;
        self
    }

    /// Substitute an icon extractor.
    pub fn with_icons(mut self, icons: IconExtractor) -> Self {
        self.icons = icons;
        self
    }

    /// Substitute the profile database.
    pub fn with_db(mut self, db: ProfileDb) -> Self {
        self.db = db;
        self
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn runtime(&self) -> &RuntimeManager {
        &self.runtime
    }

    pub fn db(&self) -> &ProfileDb {
        &self.db
    }

    // -------------------------------------------------------------- listing

    /// Every installed application, sorted by name.
    pub fn list_apps(&self) -> Result<Vec<InstalledApp>> {
        let mut apps = Vec::new();
        let entries = match std::fs::read_dir(self.paths.apps_dir()) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(apps),
            Err(e) => return Err(Error::Io(e)),
        };
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let Some(id) = entry.file_name().to_str().map(|s| s.to_string()) else {
                continue;
            };
            match InstalledApp::load(&self.paths, &id) {
                Ok(app) => apps.push(app),
                Err(Error::AppNotFound(_)) => {
                    // A directory without metadata is residue from an
                    // interrupted install. Do not hide it, but do not crash.
                    tracing::warn!(app_id = %id, "application directory has no metadata; skipping");
                }
                Err(e) => return Err(e),
            }
        }
        apps.sort_by_key(|a| a.name.to_lowercase());
        Ok(apps)
    }

    pub fn get_app(&self, id: &str) -> Result<InstalledApp> {
        InstalledApp::load(&self.paths, id)
    }

    pub fn is_installed(&self, id: &str) -> bool {
        InstalledApp::load(&self.paths, id).is_ok()
    }

    /// Log file for an application's most recent run.
    pub fn log_file_for(&self, id: &str, name: &str) -> PathBuf {
        self.paths.apps_dir().join(id).join(format!("{name}.log"))
    }

    /// All log files for an application, newest first.
    pub fn logs_for(&self, id: &str) -> Vec<PathBuf> {
        let mut logs: Vec<PathBuf> = std::fs::read_dir(self.paths.apps_dir().join(id))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().map(|e| e == "log").unwrap_or(false))
                    .collect()
            })
            .unwrap_or_default();
        logs.sort_by_key(|p| {
            std::cmp::Reverse(std::fs::metadata(p).and_then(|m| m.modified()).ok())
        });
        logs
    }

    // -------------------------------------------------------------- resolve

    /// Build a registry client for this configuration, if enabled.
    fn registry_client(&self) -> Option<RegistryClient> {
        if !self.config.allow_remote_registry {
            return None;
        }
        match RegistryClient::new(self.config.registry_url.clone(), self.paths.cache_dir()) {
            Ok(client) => Some(client),
            Err(e) => {
                tracing::warn!(error = %e, "the configured registry URL is unusable");
                None
            }
        }
    }

    /// Resolve a file to a profile without installing anything.
    pub fn resolve(&self, source: &Path) -> Result<ResolvedProfile> {
        let registry = self.registry_client();
        let engine = CompatibilityEngine::new(&self.db, &self.config);
        let engine = match &registry {
            Some(client) => engine.with_registry(client),
            None => engine,
        };
        engine.resolve(source)
    }

    /// Inspect a file without touching the database or the network.
    pub fn inspect(&self, source: &Path) -> Result<crate::compat::PeInspection> {
        crate::compat::pe::inspect(source)
    }

    /// The profile recorded for an installed application.
    pub fn profile_of(&self, id: &str) -> Result<Option<AppProfile>> {
        let app = self.get_app(id)?;
        self.db.get_profile(&app.profile_id)
    }

    // -------------------------------------------------------------- install

    /// Install an executable, `.msi` or `.bat`.
    ///
    /// `run_installer` is the one step the manager cannot do for itself: an
    /// installer may need a terminal, a window, or silent flags, and only the
    /// caller knows which. It receives the plan to execute and must return once
    /// the installer has finished.
    pub fn install_with<F>(
        &self,
        source: &Path,
        options: &InstallOptions,
        mut run_installer: F,
    ) -> Result<InstallReport>
    where
        F: FnMut(&LaunchPlan, &InstallOptions) -> Result<()>,
    {
        let started = SystemTime::now();
        options.report(InstallStage::Inspecting);

        let resolved = match &options.profile_id {
            Some(id) => {
                let profile = self.db.get_profile(id)?.ok_or_else(|| {
                    Error::ProfileNotFound(format!(
                        "{id} (run 'windrop profiles bundled' to see the recipes WinDrop ships)"
                    ))
                })?;
                let registry = self.registry_client();
                let engine = CompatibilityEngine::new(&self.db, &self.config);
                let engine = match &registry {
                    Some(client) => engine.with_registry(client),
                    None => engine,
                };
                engine.resolve_with(source, profile)?
            }
            None => self.resolve(source)?,
        };
        options.report(InstallStage::Resolved {
            profile_id: resolved.profile.id.clone(),
            source: resolved.source,
            variants: resolved.profile.variants.len(),
        });

        let name = options
            .name_hint
            .clone()
            .unwrap_or_else(|| resolved.suggested_name());
        // Dropping the very same file twice is a mistake, not a request for a
        // second copy. A *different* installer whose name happens to slugify to
        // a taken id is a genuine collision, and gets a distinct id instead.
        if let Some(existing) = self.find_by_hash(&resolved.sha256)? {
            return Err(Error::AppAlreadyInstalled(existing.id));
        }
        let app_id = match &options.app_id {
            Some(id) => {
                let id = self.validate_app_id(id)?;
                if self.paths.app_dir(&id).exists() {
                    return Err(Error::AppAlreadyInstalled(id));
                }
                id
            }
            None => self.choose_app_id(&name),
        };

        // Order the chain, putting a previously successful variant first.
        let preferred = self.db.preferred_variant(&app_id)?;
        let mut variants =
            fallback::order_variants(&resolved.profile.variants, preferred.as_deref());
        if let Some(signature) = &options.force_variant {
            variants.retain(|v| v.signature() == *signature);
            if variants.is_empty() {
                return Err(Error::NoWineVariant(format!(
                    "the requested variant is not part of the profile for '{}'",
                    app_id
                )));
            }
        }
        if options.single_variant {
            variants.truncate(1);
        }

        if options.dry_run {
            let app = self.record_for_dry_run(&app_id, &name, source, &resolved, &variants);
            return Ok(InstallReport {
                app,
                profile: resolved.profile.clone(),
                prefix: PrefixReport {
                    created: false,
                    wine: self.runtime.wine()?,
                    dependencies: Vec::new(),
                    components: Vec::new(),
                },
                attempts: Vec::new(),
                installer_plan: None,
                duration: started.elapsed().unwrap_or_default(),
            });
        }

        let app_dir = self.paths.app_dir(&app_id);
        let prefix = PrefixPaths::new(&app_dir);
        let total = variants.len();
        let profile = resolved.profile.clone();
        let input_kind = resolved.input_kind;
        let sha256 = resolved.sha256.clone();
        let inspection_arch = resolved.inspection.as_ref().map(|i| i.arch);

        // Silent flags come from the profile, because only it knows how a given
        // installer wants to be told to keep quiet. When the installer is shown
        // to the user instead, passing them would skip the questions it exists
        // to ask.
        let installer_args = if options.unattended {
            profile.installer_args.clone()
        } else {
            Vec::new()
        };

        let outcome: ChainOutcome<(PrefixReport, LaunchPlan, PathBuf)> =
            fallback::run_chain(&name, &variants, |variant, index| {
                let attempt = index + 1;
                options.report(InstallStage::PreparingPrefix {
                    attempt,
                    of: total,
                    strategy: variant.rationale.clone(),
                });
                let prefix_report =
                    self.runtime
                        .prepare_prefix(&prefix, variant, &self.paths.logs_dir())?;

                let plan =
                    self.build_plan(&prefix, variant, source, input_kind, &installer_args)?;
                options.report(InstallStage::RunningInstaller { attempt, of: total });
                run_installer(&plan, options)?;

                options.report(InstallStage::LocatingProgram { attempt });
                let main_exe = self.locate_main_exe(&prefix, &profile, options)?;
                Ok((prefix_report, plan, main_exe))
            })?;

        // Pull everything worth keeping out of the outcome before the value is
        // moved out of it.
        let retries = outcome.retries();
        let winning_variant = outcome.winning_variant.clone();
        let chain_summary = outcome.summary();
        let attempt_summaries: Vec<String> = outcome
            .attempts
            .iter()
            .map(|a| {
                format!(
                    "{}: {}",
                    a.variant.rationale,
                    a.outcome.message().unwrap_or("succeeded")
                )
            })
            .collect();
        let (prefix_report, installer_plan, main_exe_host) = outcome.value;

        // Remember what worked, so the next install of this application is fast.
        if let Err(e) =
            self.db
                .record_variant_success(&app_id, &profile.id, &winning_variant.signature())
        {
            tracing::warn!(error = %e, "could not record the working variant");
        }

        options.report(InstallStage::ExtractionDone);

        // Icon, metadata and desktop entry.
        let icon = match self.icons.extract(source, &app_dir) {
            Ok(found) => found,
            Err(e) => {
                tracing::warn!(error = %e, "icon extraction failed");
                None
            }
        };

        let windows_path =
            prefix
                .windows_path_of(&main_exe_host)
                .ok_or_else(|| Error::InstallIncomplete {
                    rationale: format!(
                        "the installed program at {} cannot be addressed from inside the prefix",
                        main_exe_host.display()
                    ),
                })?;

        let app = InstalledApp {
            id: app_id.clone(),
            name: name.clone(),
            version: profile.version.clone(),
            source_file: source.to_path_buf(),
            sha256,
            input_kind,
            // The executable's own architecture is a fact; the variant's is a
            // plan. A hand-written recipe can disagree with the file it was
            // written for, and the record should describe what was installed.
            arch: inspection_arch.unwrap_or(winning_variant.arch),
            profile_id: profile.id.clone(),
            profile_source: profile.source,
            variant: winning_variant.clone(),
            attempts: retries,
            main_exe_windows: windows_path,
            main_exe_host: main_exe_host.clone(),
            installed_at: crate::db::now_iso8601(),
            icon: icon.clone(),
            desktop_file: None,
            dependencies: winning_variant
                .dependencies
                .iter()
                .map(|d| d.verb.clone())
                .collect(),
            notes: self.install_notes(&resolved, &chain_summary),
        };
        app.validate()?;

        let entry = DesktopEntry::new(
            &app.id,
            &app.name,
            icon.as_deref()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| crate::icons::FALLBACK_ICON_NAME.to_string()),
        )
        .with_categories(desktop::categories_for(profile.requirements.graphics))
        .with_comment(format!("Run {} with WinDrop", app.name));

        let desktop_path = desktop::install(&self.paths, &entry)?;
        let app = InstalledApp {
            desktop_file: Some(desktop_path),
            ..app
        };

        app.save_profile(&self.paths, &profile)?;
        app.save(&self.paths)?;

        tracing::info!(app = %app.id, "{chain_summary}");
        options.report(InstallStage::Finished {
            app_id: app.id.clone(),
            name: app.name.clone(),
            attempts: retries,
        });

        Ok(InstallReport {
            app,
            profile,
            prefix: prefix_report,
            attempts: attempt_summaries,
            installer_plan: Some(installer_plan),
            duration: started.elapsed().unwrap_or_default(),
        })
    }

    /// Install a file by running the installer with output captured to a log.
    ///
    /// This is the unattended path, used by the CLI and by any GUI flow that
    /// does not hand the installer straight to the user.
    pub fn install(&self, source: &Path) -> Result<InstallReport> {
        let options = InstallOptions::unattended();
        self.install_with(source, &options, |plan, options| {
            let log = self
                .paths
                .logs_dir()
                .join(format!("installer-{}.log", sanitise(&plan.target)));
            let timeout = self.config.install_timeout();
            let output = plan.run_logged(&log, timeout)?;
            if output.success() {
                return Ok(());
            }
            // A non-zero exit from an installer is common even on success (many
            // demand a reboot), so the real test is whether the program turned
            // up. Report the exit code and let the locator decide.
            tracing::warn!(
                code = output.code(),
                log = %log.display(),
                "the installer exited non-zero; checking whether it worked anyway"
            );
            let _ = options;
            Ok(())
        })
    }

    /// Install with a `ProgressFn`.
    pub fn install_with_progress(
        &self,
        source: &Path,
        mut options: InstallOptions,
        progress: ProgressFn,
    ) -> Result<InstallReport> {
        options.progress = Some(progress);
        self.install_with(source, &options, |plan, _| {
            let log = self
                .paths
                .logs_dir()
                .join(format!("installer-{}.log", sanitise(&plan.target)));
            let output = plan.run_logged(&log, self.config.install_timeout())?;
            if !output.success() {
                tracing::warn!(
                    code = output.code(),
                    log = %log.display(),
                    "the installer exited non-zero; checking whether it worked anyway"
                );
            }
            Ok(())
        })
    }

    /// Run an installer interactively, for the attended flow.
    pub fn install_interactively(
        &self,
        source: &Path,
        options: &InstallOptions,
    ) -> Result<InstallReport> {
        self.install_with(source, options, |plan, _| {
            let output = plan.run_interactive()?;
            // Interactive installers routinely exit non-zero on cancel; the
            // locator is the arbiter of success.
            tracing::info!(code = output.code(), "the installer exited");
            Ok(())
        })
    }

    fn build_plan(
        &self,
        prefix: &PrefixPaths,
        variant: &RuntimeEnv,
        source: &Path,
        input_kind: InputKind,
        installer_args: &[String],
    ) -> Result<LaunchPlan> {
        let wine = self.runtime.wine_for(&variant.wine_build)?;
        let builder = EnvironmentBuilder::new(&self.config, wine);
        let target = LaunchTarget::host_file(source, input_kind).with_args(installer_args.to_vec());
        builder.build(
            prefix,
            variant,
            target,
            &self.paths.logs_dir().join("installer.log"),
        )
    }

    fn locate_main_exe(
        &self,
        prefix: &PrefixPaths,
        profile: &AppProfile,
        options: &InstallOptions,
    ) -> Result<PathBuf> {
        let locate_options = locate::LocateOptions {
            hint: profile.main_exe_hint.as_deref(),
            installed_after: None,
            user_choice: options.main_exe.as_deref(),
        };
        locate::find_main_executable(prefix, &locate_options).ok_or_else(|| {
            Error::InstallIncomplete {
                rationale: locate::no_candidate_help(prefix),
            }
        })
    }

    fn install_notes(&self, resolved: &ResolvedProfile, chain_summary: &str) -> String {
        let mut notes = vec![resolved.provenance()];
        notes.push(chain_summary.to_string());
        if !resolved.profile.notes.trim().is_empty() {
            notes.push(resolved.profile.notes.clone());
        }
        notes.join(". ")
    }

    /// An already-installed application whose source file has this digest.
    pub fn find_by_hash(&self, sha256: &str) -> Result<Option<InstalledApp>> {
        let want = sha256.to_ascii_lowercase();
        Ok(self
            .list_apps()?
            .into_iter()
            .find(|app| app.sha256.eq_ignore_ascii_case(&want)))
    }

    /// A unique application id derived from a display name.
    pub fn choose_app_id(&self, name: &str) -> String {
        let base = crate::compat::profile::slugify(name);
        if !self.paths.app_dir(&base).exists() {
            return base;
        }
        for suffix in 2..1000 {
            let candidate = format!("{base}-{suffix}");
            if !self.paths.app_dir(&candidate).exists() {
                return candidate;
            }
        }
        // Practically unreachable; keeps the function total.
        format!(
            "{base}-{}",
            &crate::compat::pe::sha256_bytes(name.as_bytes())[..8]
        )
    }

    fn validate_app_id(&self, id: &str) -> Result<String> {
        let slug = crate::compat::profile::slugify(id);
        if slug != id {
            return Err(Error::Config {
                field: "app_id".into(),
                reason: format!("'{id}' is not a valid application id (try '{slug}')"),
            });
        }
        Ok(id.to_string())
    }

    fn record_for_dry_run(
        &self,
        app_id: &str,
        name: &str,
        source: &Path,
        resolved: &ResolvedProfile,
        variants: &[RuntimeEnv],
    ) -> InstalledApp {
        let chosen = variants.first().cloned().unwrap_or_else(|| RuntimeEnv {
            wine_build: "stable".into(),
            arch: resolved
                .inspection
                .as_ref()
                .map(|i| i.arch)
                .unwrap_or(crate::compat::Arch::X86_64),
            windows_version: crate::compat::profile::WindowsVersion::Win10,
            dxvk: false,
            vkd3d_proton: false,
            dll_overrides: Vec::new(),
            env: Vec::new(),
            dependencies: Vec::new(),
            rationale: "dry run".into(),
        });
        InstalledApp {
            id: app_id.to_string(),
            name: name.to_string(),
            version: resolved.profile.version.clone(),
            source_file: source.to_path_buf(),
            sha256: resolved.sha256.clone(),
            input_kind: resolved.input_kind,
            arch: chosen.arch,
            profile_id: resolved.profile.id.clone(),
            profile_source: resolved.source,
            variant: chosen,
            attempts: 0,
            main_exe_windows: String::new(),
            main_exe_host: PathBuf::new(),
            installed_at: crate::db::now_iso8601(),
            icon: None,
            desktop_file: None,
            dependencies: Vec::new(),
            notes: "dry run: nothing was installed".to_string(),
        }
    }

    // --------------------------------------------------------------- launch

    /// The plan that would launch an installed application in the foreground.
    pub fn launch_plan(&self, id: &str) -> Result<LaunchPlan> {
        self.build_launch_plan(id, false)
    }

    /// The plan that would launch an installed application, detached.
    ///
    /// The difference is the sandbox's lifetime: a detached application must
    /// not be torn down when the launcher that started it exits, which is the
    /// whole point of being detached.
    pub fn detached_launch_plan(&self, id: &str) -> Result<LaunchPlan> {
        self.build_launch_plan(id, true)
    }

    fn build_launch_plan(&self, id: &str, detached: bool) -> Result<LaunchPlan> {
        let app = self.get_app(id)?;
        if !app.is_runnable() {
            return Err(Error::AppNotFound(format!(
                "{id} (the recorded program {} is missing; the application may need reinstalling)",
                app.main_exe_host.display()
            )));
        }
        let prefix = app.prefix(&self.paths);
        let wine = self.runtime.wine_for(&app.variant.wine_build)?;
        let mut builder = EnvironmentBuilder::new(&self.config, wine);
        if detached {
            builder = builder.surviving_the_launcher();
        }
        let target = LaunchTarget::installed(&app.main_exe_windows);
        builder.build(
            &prefix,
            &app.variant,
            target,
            &self.log_file_for(id, "launch"),
        )
    }

    /// Launch an installed application in the foreground, capturing output.
    pub fn launch(&self, id: &str) -> Result<LaunchOutcome> {
        let plan = self.launch_plan(id)?;
        let app = self.get_app(id)?;
        let log = self.log_file_for(id, "launch");
        tracing::info!(app = %app.id, target = %app.main_exe_windows, "launching");
        let output = plan.run_logged(&log, Duration::from_secs(60 * 60 * 12))?;
        if !output.success() {
            tracing::warn!(
                app = %app.id,
                code = output.code(),
                log = %log.display(),
                "the application exited with a non-zero status"
            );
        }
        Ok(LaunchOutcome {
            code: output.code(),
            output_bytes: output.stdout.len() + output.stderr.len(),
            plan,
        })
    }

    /// Detach an application so it keeps running after WinDrop exits.
    ///
    /// This is what the desktop entry's `windrop launch` ultimately does for a
    /// GUI application: the process must not be a child of the menu handler.
    pub fn launch_detached(&self, id: &str) -> Result<LaunchPlan> {
        let plan = self.detached_launch_plan(id)?;

        // A detached program's output has nowhere else to go: it is not attached
        // to a terminal, because the launcher exits immediately. Sending it to
        // /dev/null would leave "Show log" empty for exactly the launches that
        // need explaining — the ones that failed to appear. So it goes to the
        // application's launch log, truncated each time, with the command that
        // was run at the top: an empty log still has to say what was attempted.
        let log_path = self.log_file_for(id, "launch");
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let log = std::fs::File::create(&log_path)?;
        {
            use std::io::Write;
            let mut header = log.try_clone()?;
            writeln!(header, "# {}", plan.spec.display())?;
        }

        let mut command = plan.spec.to_command();
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(log.try_clone()?))
            .stderr(std::process::Stdio::from(log));
        // SAFETY: `setsid` is async-signal-safe and the only work done between
        // fork and exec, which is the requirement for `pre_exec`.
        unsafe {
            use std::os::unix::process::CommandExt;
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    // Already a session leader: continue in place.
                }
                Ok(())
            });
        }
        command.spawn().map_err(|e| Error::InstallIncomplete {
            rationale: format!("could not start the application: {e}"),
        })?;
        Ok(plan)
    }

    // --------------------------------------------------------------- remove

    /// Remove an application, its prefix and its menu entry.
    pub fn remove(&self, id: &str) -> Result<RemovalReport> {
        let app = self.get_app(id)?;
        let app_dir = self.paths.app_dir(id);
        let freed_bytes = crate::runtime::prefix::directory_size(&app_dir);

        // Stop Wine first so its files are not in use while we delete them.
        // A refusal here is not fatal: the tree is about to be deleted anyway.
        if let Err(e) = self.runtime.shutdown_prefix(&PrefixPaths::new(&app_dir)) {
            tracing::debug!(error = %e, "could not shut the Wine server down before removal");
        }

        let removed_desktop_entry = desktop::uninstall(&self.paths, id)?;

        match std::fs::remove_dir_all(&app_dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::Io(e)),
        }

        tracing::info!(
            app = id,
            freed = freed_bytes,
            "removed the application and everything it owned"
        );
        Ok(RemovalReport {
            app_id: id.to_string(),
            name: app.name,
            freed_bytes,
            removed_desktop_entry,
        })
    }

    /// Forget what WinDrop learned about an application's working variant.
    pub fn forget_learning(&self, id: &str) -> Result<bool> {
        self.db.forget_learning(id)
    }

    /// Check that a removal left nothing behind.
    ///
    /// Used by the removal command and by the test-suite; an installation must
    /// not touch anything outside its own directory and the applications
    /// directory.
    pub fn verify_no_residue(&self, id: &str) -> Result<()> {
        let mut leftovers = Vec::new();
        if self.paths.app_dir(id).exists() {
            leftovers.push(self.paths.app_dir(id).display().to_string());
        }
        if self.paths.desktop_file_for(id).exists() {
            leftovers.push(self.paths.desktop_file_for(id).display().to_string());
        }
        if leftovers.is_empty() {
            Ok(())
        } else {
            Err(Error::InstallIncomplete {
                rationale: format!("removal left files behind: {}", leftovers.join(", ")),
            })
        }
    }
}

/// Make a string safe to use inside a filename.
fn sanitise(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .take(80)
        .collect()
}

/// Human-readable byte count for reports.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_counts_are_readable() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn filenames_are_sanitised() {
        assert_eq!(sanitise("Z:\\home\\u\\setup.exe"), "Z__home_u_setup.exe");
        assert_eq!(sanitise("../../etc/passwd"), ".._.._etc_passwd");
        assert!(sanitise(&"x".repeat(500)).len() <= 80);
    }

    #[test]
    fn install_stage_labels_are_human_readable() {
        assert!(InstallStage::Inspecting.label().contains("executable"));
        assert!(InstallStage::Resolved {
            profile_id: "a".into(),
            source: ProfileSource::Remote,
            variants: 3
        }
        .label()
        .contains("community registry"));
        assert!(InstallStage::RunningInstaller { attempt: 2, of: 4 }
            .label()
            .contains("2/4"));
        assert!(InstallStage::Finished {
            app_id: "a".into(),
            name: "Notepad".into(),
            attempts: 0
        }
        .label()
        .contains("Notepad"));
    }
}
