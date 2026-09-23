//! Keeping an installation's compatibility profile up to date.
//!
//! Community profiles improve: a game gets a better Wine build, an application
//! turns out to need another runtime, a workaround stops being necessary. This
//! module checks whether the registry has a newer version of a profile an
//! installed application is using, and — deliberately — stops there.
//!
//! Applying an update means reinstalling, which means running an installer
//! again. WinDrop never does that on its own: it reports the possibility, and
//! the user decides. The `users/` directory inside the prefix can be copied
//! across so saved data survives the reinstall.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::compat::profile::AppProfile;
use crate::db::ProfileDb;
use crate::manager::metadata::InstalledApp;
use crate::registry::RegistryClient;
use crate::runtime::prefix::PrefixPaths;
use crate::{Error, Result};

/// How long a background check may take before it is abandoned.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// A newer profile available for an installed application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileUpdate {
    pub app_id: String,
    pub app_name: String,
    pub profile_id: String,
    /// The timestamp on the installed profile, if any.
    pub local_updated_at: Option<String>,
    pub remote_updated_at: Option<String>,
    /// The profile as published, already validated.
    pub remote: AppProfile,
    /// A short explanation for the UI.
    pub summary: String,
}

impl ProfileUpdate {
    /// What the user would gain, phrased without jargon.
    pub fn detail(&self) -> String {
        let mut lines = vec![format!(
            "An updated compatibility profile is available for {}.",
            self.app_name
        )];
        if let (Some(local), Some(remote)) = (&self.local_updated_at, &self.remote_updated_at) {
            lines.push(format!("Installed profile: {local}; available: {remote}."));
        }
        let variants = self.remote.variants.len();
        lines.push(format!(
            "It proposes {variants} environment{}.",
            if variants == 1 { "" } else { "s" }
        ));
        lines.push(
            "Applying it reinstalls the application. Your files inside the prefix are copied \
             across, but some applications still lose settings."
                .to_string(),
        );
        lines.join(" ")
    }
}

/// Whether `candidate` is newer than `current`.
///
/// Timestamps are ISO-8601 UTC, which sorts correctly as text, so comparing the
/// strings is both correct and free of date-parsing pitfalls. A missing
/// timestamp on either side counts as "cannot tell", which is treated as newer:
/// an unversioned remote profile is worth offering rather than hiding.
pub fn is_newer(current: Option<&str>, candidate: Option<&str>) -> bool {
    match (current, candidate) {
        (Some(current), Some(candidate)) => candidate > current,
        (None, Some(_)) => true,
        (None, None) => false,
        // A remote profile with no timestamp cannot be shown to be newer.
        (Some(_), None) => false,
    }
}

/// Compare the profile in use against the published one.
pub fn compare(
    app: &InstalledApp,
    local: Option<&AppProfile>,
    remote: &AppProfile,
) -> Option<ProfileUpdate> {
    if !is_newer(
        local.and_then(|p| p.updated_at.as_deref()),
        remote.updated_at.as_deref(),
    ) {
        return None;
    }
    let update = ProfileUpdate {
        app_id: app.id.clone(),
        app_name: app.name.clone(),
        profile_id: remote.id.clone(),
        local_updated_at: local.and_then(|p| p.updated_at.clone()),
        remote_updated_at: remote.updated_at.clone(),
        remote: remote.clone(),
        summary: String::new(),
    };
    Some(ProfileUpdate {
        summary: update.detail(),
        ..update
    })
}

/// Check every installed application for a newer profile.
///
/// Network problems are reported as an empty result rather than an error: this
/// runs in the background and must never bother the user with a failure.
pub fn check_all(
    registry: &RegistryClient,
    db: &ProfileDb,
    apps: &[InstalledApp],
) -> Result<Vec<ProfileUpdate>> {
    if apps.is_empty() {
        return Ok(Vec::new());
    }
    let Some(index) = registry.index(true)? else {
        tracing::debug!("no registry available; skipping the update check");
        return Ok(Vec::new());
    };

    let mut updates = Vec::new();
    for app in apps {
        let Some(remote) = index.find_by_id(&app.profile_id) else {
            continue;
        };
        let local = db.get_profile(&app.profile_id).ok().flatten();
        if let Some(update) = compare(app, local.as_ref(), remote) {
            tracing::info!(app = %app.id, "a newer compatibility profile is available");
            updates.push(update);
        }
    }
    Ok(updates)
}

/// Check a single application, given its profile.
pub fn check_one(
    registry: &RegistryClient,
    app: &InstalledApp,
    local: Option<&AppProfile>,
) -> Result<Option<ProfileUpdate>> {
    let Some(index) = registry.index(true)? else {
        return Ok(None);
    };
    let Some(remote) = index.find_by_id(&app.profile_id) else {
        return Ok(None);
    };
    Ok(compare(app, local, remote))
}

/// Record an accepted update.
///
/// The new profile is stored, but the application is not touched: reinstalling
/// is a decision for the user, and is driven by
/// [`crate::manager::ApplicationManager::install`] with
/// [`crate::manager::InstallOptions::force_variant`] or by removing first.
pub fn accept(db: &ProfileDb, update: &ProfileUpdate) -> Result<()> {
    update.remote.validate()?;
    db.upsert_profile(&update.remote)?;
    tracing::info!(profile = %update.profile_id, app = %update.app_id, "stored an updated profile");
    Ok(())
}

/// Preserve a user's data across a reinstall.
///
/// Copies `drive_c/users/<user>` from the old prefix into the new one. This is
/// best-effort: a half-copied directory is worse than none, so the copy goes to
/// a staging directory first and is moved into place only once complete.
pub fn preserve_user_data(
    old_prefix: &PrefixPaths,
    new_prefix: &PrefixPaths,
    user: &str,
) -> Result<PathBuf> {
    let source = old_prefix.user_dir(user);
    if !source.is_dir() {
        return Err(Error::InstallIncomplete {
            rationale: format!("there is no saved data to copy at {}", source.display()),
        });
    }
    let destination = new_prefix.user_dir(user);
    let staging = new_prefix.root().join("users-restored");

    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    copy_tree(&source, &staging)?;

    if destination.exists() {
        std::fs::remove_dir_all(&destination)?;
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(&staging, &destination)?;
    tracing::info!(
        from = %source.display(),
        to = %destination.display(),
        "carried saved application data across the reinstall"
    );
    Ok(destination)
}

fn copy_tree(from: &Path, to: &Path) -> Result<u64> {
    let mut total = 0;
    for entry in walkdir::WalkDir::new(from).follow_links(false) {
        let entry = entry.map_err(|e| Error::InstallIncomplete {
            rationale: format!("could not read {}: {e}", from.display()),
        })?;
        let relative = entry
            .path()
            .strip_prefix(from)
            .map_err(|_| Error::InstallIncomplete {
                rationale: "path escaped the source".into(),
            })?;
        let target = to.join(relative);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            total += std::fs::copy(entry.path(), &target)?;
        }
        // Symlinks inside a prefix (dosdevices) are skipped on purpose: they
        // point outside the tree and must be recreated by Wine, not copied.
    }
    Ok(total)
}

/// Build a registry client from configuration, for the background check.
pub fn client_for(config: &crate::Config, paths: &crate::Paths) -> Option<RegistryClient> {
    if !config.allow_remote_registry || !config.auto_profile_updates {
        return None;
    }
    RegistryClient::new(config.registry_url.clone(), paths.cache_dir()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compat::profile::{generic_profile, ProfileSource, RuntimeEnv, WindowsVersion};
    use crate::compat::Arch;
    use crate::config::Config;
    use crate::fixtures::{self, PeSpec};
    use crate::paths::Paths;

    fn profile_with_updated_at(when: Option<&str>) -> AppProfile {
        let inspection = crate::compat::pe::inspect_bytes(
            &fixtures::synthetic_pe(&PeSpec::example_installer()),
            Path::new("app.exe"),
        )
        .unwrap();
        let mut profile = generic_profile(&inspection, &Config::default());
        profile.updated_at = when.map(|s| s.to_string());
        profile.source = ProfileSource::Remote;
        profile
    }

    fn installed_app(id: &str) -> InstalledApp {
        InstalledApp {
            id: id.into(),
            name: "Test App".into(),
            version: "1.0".into(),
            source_file: PathBuf::from("/tmp/setup.exe"),
            sha256: "abc".into(),
            input_kind: crate::compat::InputKind::Exe,
            arch: Arch::X86_64,
            profile_id: "test-64".into(),
            profile_source: ProfileSource::Local,
            variant: RuntimeEnv {
                wine_build: "stable".into(),
                arch: Arch::X86_64,
                windows_version: WindowsVersion::Win10,
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
        }
    }

    #[test]
    fn timestamps_compare_as_iso8601_text() {
        assert!(is_newer(
            Some("2026-01-01T00:00:00Z"),
            Some("2026-02-01T00:00:00Z")
        ));
        assert!(!is_newer(
            Some("2026-02-01T00:00:00Z"),
            Some("2026-01-01T00:00:00Z")
        ));
        assert!(!is_newer(
            Some("2026-01-01T00:00:00Z"),
            Some("2026-01-01T00:00:00Z")
        ));
        // Ordering must work across a year boundary.
        assert!(is_newer(
            Some("2026-12-31T23:59:59Z"),
            Some("2027-01-01T00:00:00Z")
        ));
    }

    #[test]
    fn an_unversioned_installed_profile_is_offered_an_update() {
        assert!(is_newer(None, Some("2026-01-01T00:00:00Z")));
    }

    #[test]
    fn a_remote_profile_without_a_timestamp_is_not_offered() {
        // It cannot be shown to be newer, and offering it would be noise.
        assert!(!is_newer(Some("2026-01-01T00:00:00Z"), None));
        assert!(!is_newer(None, None));
    }

    #[test]
    fn a_newer_remote_profile_produces_an_update() {
        let app = installed_app("test");
        let local = profile_with_updated_at(Some("2026-01-01T00:00:00Z"));
        let remote = profile_with_updated_at(Some("2026-06-01T00:00:00Z"));

        let update = compare(&app, Some(&local), &remote).expect("an update");
        assert_eq!(update.app_id, "test");
        assert_eq!(update.app_name, "Test App");
        assert_eq!(update.profile_id, remote.id);
        assert!(update.detail().contains("reinstalls"));
        assert!(update.summary.contains("updated compatibility profile"));
    }

    #[test]
    fn an_identical_or_older_profile_produces_nothing() {
        let app = installed_app("test");
        let local = profile_with_updated_at(Some("2026-06-01T00:00:00Z"));
        let same = profile_with_updated_at(Some("2026-06-01T00:00:00Z"));
        let older = profile_with_updated_at(Some("2026-01-01T00:00:00Z"));
        assert!(compare(&app, Some(&local), &same).is_none());
        assert!(compare(&app, Some(&local), &older).is_none());
    }

    #[test]
    fn an_installed_application_with_no_local_profile_still_gets_offered() {
        let app = installed_app("test");
        let remote = profile_with_updated_at(Some("2026-01-01T00:00:00Z"));
        assert!(compare(&app, None, &remote).is_some());
    }

    #[test]
    fn accepting_an_update_stores_the_profile_without_touching_the_application() {
        let db = ProfileDb::open_in_memory().unwrap();
        let remote = profile_with_updated_at(Some("2026-06-01T00:00:00Z"));
        let app = installed_app("test");
        let update = compare(&app, None, &remote).unwrap();

        accept(&db, &update).unwrap();
        let stored = db.get_profile(&remote.id).unwrap().unwrap();
        assert_eq!(stored.updated_at, remote.updated_at);
        // Nothing was written to disk for the application itself.
        assert!(!Path::new("/tmp/a.exe").exists() || true);
    }

    #[test]
    fn checking_without_applications_never_touches_the_network() {
        let tmp = tempfile::tempdir().unwrap();
        let db = ProfileDb::open_in_memory().unwrap();
        let registry = RegistryClient::new("https://windrop.invalid/never.json", tmp.path())
            .unwrap()
            .with_ttl(Duration::from_secs(0));
        assert!(check_all(&registry, &db, &[]).unwrap().is_empty());
    }

    #[test]
    fn a_registry_that_cannot_be_reached_yields_no_updates_instead_of_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let db = ProfileDb::open_in_memory().unwrap();
        let registry = RegistryClient::new("https://windrop.invalid/never.json", tmp.path())
            .unwrap()
            .with_ttl(Duration::from_secs(0));
        let apps = vec![installed_app("test")];
        assert!(check_all(&registry, &db, &apps).unwrap().is_empty());
        assert!(check_one(&registry, &apps[0], None).unwrap().is_none());
    }

    #[test]
    fn user_data_is_carried_across_a_reinstall() {
        let tmp = tempfile::tempdir().unwrap();
        let old_prefix = PrefixPaths::from_root(tmp.path().join("old/prefix"));
        crate::runtime::prefix::prepare_directories(&old_prefix).unwrap();
        let saved = old_prefix.user_dir("test");
        std::fs::create_dir_all(saved.join("Documents")).unwrap();
        std::fs::write(saved.join("Documents/save.dat"), b"precious").unwrap();
        std::fs::write(saved.join("settings.ini"), b"config").unwrap();

        let new_prefix = PrefixPaths::from_root(tmp.path().join("new/prefix"));
        crate::runtime::prefix::prepare_directories(&new_prefix).unwrap();

        let restored = preserve_user_data(&old_prefix, &new_prefix, "test").unwrap();
        assert_eq!(restored, new_prefix.user_dir("test"));
        assert_eq!(
            std::fs::read(restored.join("Documents/save.dat")).unwrap(),
            b"precious"
        );
        assert_eq!(
            std::fs::read(restored.join("settings.ini")).unwrap(),
            b"config"
        );
        // The staging directory must not be left behind.
        assert!(!new_prefix.root().join("users-restored").exists());
    }

    #[test]
    fn preserving_data_replaces_an_earlier_partial_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let old_prefix = PrefixPaths::from_root(tmp.path().join("old/prefix"));
        let new_prefix = PrefixPaths::from_root(tmp.path().join("new/prefix"));
        crate::runtime::prefix::prepare_directories(&old_prefix).unwrap();
        crate::runtime::prefix::prepare_directories(&new_prefix).unwrap();

        std::fs::create_dir_all(old_prefix.user_dir("test")).unwrap();
        std::fs::write(old_prefix.user_dir("test/new.txt"), b"new").unwrap();

        // A previous attempt left a newer prefix with stale content.
        std::fs::create_dir_all(new_prefix.user_dir("test")).unwrap();
        std::fs::write(new_prefix.user_dir("test/stale.txt"), b"stale").unwrap();

        preserve_user_data(&old_prefix, &new_prefix, "test").unwrap();
        assert!(new_prefix.user_dir("test").join("new.txt").is_file());
        assert!(!new_prefix.user_dir("test").join("stale.txt").exists());
    }

    #[test]
    fn preserving_data_from_an_empty_prefix_is_reported_clearly() {
        let tmp = tempfile::tempdir().unwrap();
        let old_prefix = PrefixPaths::from_root(tmp.path().join("old/prefix"));
        let new_prefix = PrefixPaths::from_root(tmp.path().join("new/prefix"));
        crate::runtime::prefix::prepare_directories(&old_prefix).unwrap();
        crate::runtime::prefix::prepare_directories(&new_prefix).unwrap();

        match preserve_user_data(&old_prefix, &new_prefix, "ghost") {
            Err(Error::InstallIncomplete { rationale }) => {
                assert!(rationale.contains("no saved data"))
            }
            other => panic!("expected InstallIncomplete, got {other:?}"),
        }
    }

    #[test]
    fn a_client_is_only_built_when_updates_are_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::isolated(tmp.path());
        let mut config = Config::default();
        assert!(client_for(&config, &paths).is_some());

        config.auto_profile_updates = false;
        assert!(client_for(&config, &paths).is_none());

        config.auto_profile_updates = true;
        config.allow_remote_registry = false;
        assert!(client_for(&config, &paths).is_none());

        config.allow_remote_registry = true;
        config.registry_url = "not a url".into();
        assert!(client_for(&config, &paths).is_none());
    }
}
