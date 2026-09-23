//! The community compatibility registry.
//!
//! The registry is a single JSON document listing profiles. WinDrop fetches it
//! at most once per TTL, caches it on disk, and searches the cache locally —
//! which means a profile lookup never blocks an install on the network, and a
//! registry that supports hash lookup directly can be added later without
//! changing callers.
//!
//! Accepted document shapes:
//!
//! ```json
//! [ { "id": "...", "name": "...", "variants": [ ... ] } ]
//! ```
//!
//! ```json
//! { "profiles": [ { "id": "...", ... } ] }
//! ```
//!
//! Nothing here is required for WinDrop to work. When the registry is
//! unreachable the engine falls back to generated profiles.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::compat::pe::Arch;
use crate::compat::profile::{slugify, AppProfile, ProfileSource};
use crate::{Error, Result};

/// How long a cached registry document stays fresh.
pub const DEFAULT_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// The cached registry document plus bookkeeping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryIndex {
    pub source_url: String,
    pub fetched_at_unix: i64,
    pub profiles: Vec<AppProfile>,
}

impl RegistryIndex {
    /// Parse a registry document, accepting both container shapes.
    ///
    /// Entries are converted one at a time so that a single malformed profile
    /// is reported with its position and id. A document that fails as a whole
    /// would say only "did not match any variant", which is useless to whoever
    /// has to fix it — and this parser is the gate for community submissions
    /// as well as for WinDrop's own bundled recipes.
    pub fn from_json(text: &str, source_url: &str) -> Result<Self> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Doc {
            Wrapped { profiles: Vec<serde_json::Value> },
            Bare(Vec<serde_json::Value>),
        }

        let entries = match serde_json::from_str::<Doc>(text)? {
            Doc::Wrapped { profiles } | Doc::Bare(profiles) => profiles,
        };

        let mut profiles = Vec::with_capacity(entries.len());
        for (index, entry) in entries.into_iter().enumerate() {
            let id_hint = entry
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let profile: AppProfile =
                serde_json::from_value(entry).map_err(|e| Error::InvalidProfile {
                    document: document_label(source_url),
                    index,
                    id_hint: id_hint.clone(),
                    reason: e.to_string(),
                })?;
            profile.validate().map_err(|e| Error::InvalidProfile {
                document: document_label(source_url),
                index,
                id_hint,
                reason: e.to_string(),
            })?;
            profiles.push(profile);
        }

        Ok(RegistryIndex {
            source_url: source_url.to_string(),
            fetched_at_unix: now_unix(),
            profiles,
        })
    }

    /// The best profile for a specific installer.
    ///
    /// An exact digest match always wins. Failing that, a `name_hint` lets a
    /// profile be recognised from its name: applications ship new installers
    /// constantly, so a digest-only registry would rarely hit.
    ///
    /// Matching by name requires the hint to be at least three characters, so
    /// short or generic hints cannot pull in an unrelated profile.
    pub fn matching_hash(
        &self,
        sha256: &str,
        arch: Option<Arch>,
        name_hint: Option<&str>,
    ) -> Option<AppProfile> {
        self.best_match(sha256, arch, name_hint)
            .map(with_remote_source)
    }

    /// The best profile for an installer, without relabelling its provenance.
    ///
    /// Kept separate from [`RegistryIndex::matching_hash`] so the same ranking
    /// can be used against the *local* database, where a match must keep
    /// whatever source it originally had instead of being marked remote.
    pub fn best_match(
        &self,
        sha256: &str,
        arch: Option<Arch>,
        name_hint: Option<&str>,
    ) -> Option<&AppProfile> {
        let want = sha256.to_ascii_lowercase();
        if let Some(exact) = self
            .profiles
            .iter()
            .find(|p| p.hashes.iter().any(|h| h.eq_ignore_ascii_case(&want)))
        {
            return Some(exact);
        }

        // Normalise the hint the same way profile names are normalised, so
        // `Notepad++` and `notepad` are recognised as the same application.
        let hint = slugify(name_hint?);
        // A hint that names nothing in particular cannot identify an
        // application, and matching on it would bind an unrelated profile.
        if hint.len() < 3 || is_generic_installer_name(&hint) {
            return None;
        }
        let arch_matches = |p: &AppProfile| arch.is_none() || p.arch.is_none() || p.arch == arch;

        self.profiles
            .iter()
            .filter_map(|p| {
                let slug = slugify(&p.name);
                let id = slugify(&p.id);
                let strength = if slug == hint || id == hint {
                    3
                } else if (slug.len() >= 3
                    && (slug.contains(&hint) || hint.contains(slug.as_str())))
                    || (id.len() >= 3 && id.contains(&hint))
                {
                    2
                } else {
                    return None;
                };
                // A profile that contradicts the architecture is allowed but
                // always loses to one that does not.
                let score = if arch_matches(p) {
                    strength
                } else {
                    strength - 1
                };
                Some((score, p))
            })
            .max_by_key(|(score, _)| *score)
            .map(|(_, p)| p)
    }

    pub fn find_by_id(&self, id: &str) -> Option<&AppProfile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    pub fn is_fresh(&self, ttl: Duration) -> bool {
        let age = now_unix() - self.fetched_at_unix;
        age >= 0 && (age as u64) < ttl.as_secs()
    }
}

/// How a registry document should be named in an error message.
fn document_label(source_url: &str) -> String {
    if source_url.is_empty() {
        return "the profile document".to_string();
    }
    match source_url.split_once(':') {
        // A scheme we do not fetch: show the whole thing, it is a local path or
        // a label such as `bundled:profiles.json`.
        Some(("http", _)) | Some(("https", _)) => "the registry document".to_string(),
        _ => source_url.to_string(),
    }
}

fn with_remote_source(profile: &AppProfile) -> AppProfile {
    let mut p = profile.clone();
    p.source = ProfileSource::Remote;
    p
}

/// File names that say nothing about which application they install.
///
/// Installers are very often shipped as `setup.exe`, `install.exe` or
/// `setup_x64.exe`, and matching a profile on one of those would bind an
/// unrelated recipe to an application. Refusing to match on them costs nothing
/// — the generated profile is a perfectly good starting point — and prevents a
/// whole class of confidently wrong guesses.
const GENERIC_NAME_HINTS: &[&str] = &[
    "setup",
    "install",
    "installer",
    "uninstall",
    "update",
    "updater",
    "patch",
    "launcher",
    "loader",
    "app",
    "application",
    "program",
    "run",
    "start",
    "download",
    "demo",
    "trial",
    "beta",
    "portable",
    "online",
    "offline",
    "web",
    "full",
    "minimal",
    "snapshot",
];

/// Whether a name hint identifies nothing (`setup`, `setup64`, `install-x64`).
///
/// Comparison ignores case and punctuation, and tolerates the architecture and
/// version suffixes installers habitually carry.
///
/// ```
/// # use windrop_core::registry::is_generic_installer_name;
/// assert!(is_generic_installer_name("setup"));
/// assert!(is_generic_installer_name("Setup_x64"));
/// assert!(is_generic_installer_name("installer-32"));
/// assert!(!is_generic_installer_name("notepad"));
/// assert!(!is_generic_installer_name("7-zip"));
/// ```
pub fn is_generic_installer_name(name: &str) -> bool {
    let mut normalized: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    // `Install.exe` normalises to `installexe`, and the extension carries no
    // meaning: strip it so the check sees the word the user would recognise.
    if normalized.len() > 3 {
        if let Some(stripped) = normalized.strip_suffix("exe") {
            normalized = stripped.to_string();
        }
    }
    if normalized.is_empty() {
        return true;
    }
    GENERIC_NAME_HINTS.iter().any(|word| {
        if normalized == *word {
            return true;
        }
        match normalized.strip_prefix(word) {
            // `setup64`, `setupx64`, `install32` and friends.
            Some(rest) => !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit() || c == 'x'),
            None => false,
        }
    })
}

/// Client for a registry endpoint.
pub struct RegistryClient {
    url: String,
    cache_path: PathBuf,
    ttl: Duration,
    timeout: Duration,
    client: reqwest::blocking::Client,
}

impl RegistryClient {
    /// Build a client. `cache_dir` receives the cached document.
    pub fn new(url: impl Into<String>, cache_dir: &Path) -> Result<Self> {
        let url = url.into();
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(Error::Config {
                field: "registry_url".into(),
                reason: "must be an http(s) URL".into(),
            });
        }
        let timeout = Duration::from_secs(20);
        let client = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .connect_timeout(Duration::from_secs(8))
            .user_agent(concat!("WinDrop/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(Error::from)?;
        Ok(RegistryClient {
            url,
            cache_path: cache_dir.join("registry.json"),
            ttl: DEFAULT_TTL,
            timeout,
            client,
        })
    }

    /// Override the cache freshness window (tests use a zero TTL).
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn cache_path(&self) -> &Path {
        &self.cache_path
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Read the cached document without any network access.
    pub fn cached(&self) -> Result<Option<RegistryIndex>> {
        match std::fs::read_to_string(&self.cache_path) {
            Ok(text) => match RegistryIndex::from_json(&text, &self.url) {
                Ok(index) => Ok(Some(index)),
                Err(e) => {
                    // A corrupt cache must not be fatal: drop it and move on.
                    tracing::warn!(
                        path = %self.cache_path.display(),
                        error = %e,
                        "cached registry document is unreadable; discarding"
                    );
                    let _ = std::fs::remove_file(&self.cache_path);
                    Ok(None)
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// Fetch the document from the network and cache it.
    pub fn refresh(&self) -> Result<RegistryIndex> {
        tracing::info!(url = %self.url, "fetching compatibility registry");
        let response = self.client.get(&self.url).send().map_err(Error::from)?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::DownloadBlocked(format!(
                "the registry replied with HTTP {status}"
            )));
        }
        let text = response.text().map_err(Error::from)?;
        let index = RegistryIndex::from_json(&text, &self.url).map_err(|e| match e {
            Error::Json(e) => {
                Error::DownloadBlocked(format!("the registry document could not be parsed: {e}"))
            }
            other => other,
        })?;

        if let Some(parent) = self.cache_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.cache_path, serde_json::to_string(&index)?)?;
        tracing::info!(profiles = index.profiles.len(), "registry refreshed");
        Ok(index)
    }

    /// Get the index, refreshing only when the cache is missing or stale.
    ///
    /// Network failures degrade to a stale cache, and to `None` when there is
    /// no cache at all. They are never fatal.
    pub fn index(&self, allow_network: bool) -> Result<Option<RegistryIndex>> {
        let cached = self.cached()?;
        match &cached {
            Some(index) if index.is_fresh(self.ttl) => return Ok(cached),
            Some(index) => {
                tracing::debug!(
                    cache = %self.cache_path.display(),
                    "registry cache is stale"
                );
                let _ = index;
            }
            None => {}
        }

        if !allow_network {
            return Ok(cached);
        }

        match self.refresh() {
            Ok(fresh) => Ok(Some(fresh)),
            Err(e) => {
                if cached.is_some() {
                    tracing::warn!(error = %e, "registry refresh failed; using cached profiles");
                } else {
                    tracing::warn!(error = %e, "registry unavailable and nothing is cached");
                }
                Ok(cached)
            }
        }
    }

    /// Look up a profile for an installer digest, with an optional name hint.
    pub fn find_by_hash(
        &self,
        sha256: &str,
        arch: Option<Arch>,
        name_hint: Option<&str>,
        allow_network: bool,
    ) -> Result<Option<AppProfile>> {
        Ok(self
            .index(allow_network)?
            .and_then(|index| index.matching_hash(sha256, arch, name_hint)))
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::fixtures::{self, PeSpec};

    fn profile_json(id: &str, name: &str, hash: &str) -> String {
        let inspection = crate::compat::pe::inspect_bytes(
            &fixtures::synthetic_pe(&PeSpec::example_installer()),
            Path::new("app.exe"),
        )
        .unwrap();
        let mut profile = crate::compat::profile::generic_profile(&inspection, &Config::default());
        profile.id = id.to_string();
        profile.name = name.to_string();
        profile.hashes = if hash.is_empty() {
            vec![]
        } else {
            vec![hash.to_string()]
        };
        serde_json::to_string(&profile).unwrap()
    }

    #[test]
    fn parses_a_bare_array_document() {
        let text = format!(
            "[{}, {}]",
            profile_json("a", "A", "aa"),
            profile_json("b", "B", "bb")
        );
        let index = RegistryIndex::from_json(&text, "https://example.com/r.json").unwrap();
        assert_eq!(index.profiles.len(), 2);
        assert_eq!(index.profiles[0].id, "a");
    }

    #[test]
    fn parses_a_wrapped_document() {
        let text = format!(r#"{{"profiles": [{}]}}"#, profile_json("a", "A", "aa"));
        let index = RegistryIndex::from_json(&text, "https://example.com/r.json").unwrap();
        assert_eq!(index.profiles.len(), 1);
    }

    #[test]
    fn rejects_malformed_documents() {
        assert!(RegistryIndex::from_json("{ not json", "u").is_err());
        // A profile missing required fields must not silently vanish.
        assert!(RegistryIndex::from_json(r#"[{"id":"a"}]"#, "u").is_err());
    }

    #[test]
    fn hash_lookup_prefers_an_exact_digest_match() {
        let text = format!(
            "[{}, {}]",
            profile_json(
                "generic",
                "Generic",
                "1111111111111111111111111111111111111111111111111111111111111111"
            ),
            profile_json("exact", "Exact", "abc123"),
        );
        let index = RegistryIndex::from_json(&text, "https://example.com/r.json").unwrap();
        let hit = index.matching_hash("ABC123", None, None).unwrap();
        assert_eq!(hit.id, "exact");
        assert_eq!(
            hit.source,
            ProfileSource::Remote,
            "provenance must be relabelled"
        );
    }

    #[test]
    fn hash_lookup_without_a_match_returns_nothing() {
        let text = format!("[{}]", profile_json("a", "A", "abc123"));
        let index = RegistryIndex::from_json(&text, "https://example.com/r.json").unwrap();
        assert!(index.matching_hash("ffff", None, None).is_none());
        // A hint that shares nothing with any profile must not match either.
        assert!(index
            .matching_hash("ffff", None, Some("totally-different"))
            .is_none());
        // Hints shorter than three characters are ignored outright.
        assert!(index.matching_hash("ffff", None, Some("ab")).is_none());
    }

    #[test]
    fn a_profile_is_recognised_by_name_when_the_digest_is_unknown() {
        let text = format!("[{}]", profile_json("notepadpp", "Notepad++", ""));
        let index = RegistryIndex::from_json(&text, "https://example.com/r.json").unwrap();

        let hit = index
            .matching_hash("unknown-digest", None, Some("notepad"))
            .unwrap();
        assert_eq!(hit.id, "notepadpp");
        assert_eq!(hit.name, "Notepad++");

        // Arch-agnostic profiles still match when an arch is supplied.
        assert!(index
            .matching_hash("unknown-digest", Some(Arch::X86_64), Some("notepad"))
            .is_some());
    }

    #[test]
    fn generic_installer_names_are_recognised() {
        for generic in [
            "setup",
            "Setup",
            "SETUP",
            "setup64",
            "setup_x64",
            "setup-x86-64",
            "install",
            "installer32",
            "install.exe",
            "update",
            "launcher",
            "",
            "!!!",
        ] {
            assert!(
                is_generic_installer_name(generic),
                "{generic} should be generic"
            );
        }
    }

    #[test]
    fn real_application_names_are_not_generic() {
        for real in [
            "notepad",
            "7-zip",
            "vlc-3.0.20",
            "firefox setup",
            "starter-pack",
            "runway",
            "appsheet",
            "installshield-helper",
        ] {
            assert!(
                !is_generic_installer_name(real),
                "{real} should not be generic"
            );
        }
    }

    #[test]
    fn a_generic_file_name_never_matches_a_profile_by_name() {
        // The single most common installer name in the world must not bind an
        // unrelated recipe to whatever the user happened to drop.
        let text = format!("[{}]", profile_json("setup", "Setup", ""));
        let index = RegistryIndex::from_json(&text, "https://example.com/r.json").unwrap();
        assert!(index
            .matching_hash("unknown", None, Some("setup"))
            .is_none());
    }

    #[test]
    fn name_matching_normalises_case_and_punctuation() {
        let text = format!("[{}]", profile_json("notepadpp", "Notepad++", ""));
        let index = RegistryIndex::from_json(&text, "https://example.com/r.json").unwrap();
        // The same file, named several of the ways a user might have it.
        for hint in [
            "notepad",
            "Notepad",
            "Notepad++",
            "NOTEPAD",
            "Notepad++ 8.6",
        ] {
            assert!(
                index.matching_hash("unknown", None, Some(hint)).is_some(),
                "hint {hint:?} should have matched"
            );
        }
    }

    #[test]
    fn best_match_keeps_the_profiles_own_provenance() {
        let mut profile =
            crate::compat::profile::AppProfile::from_json(&profile_json("seeded", "Notepad++", ""))
                .unwrap();
        profile.source = ProfileSource::Bundled;
        let index = RegistryIndex {
            source_url: "local database".into(),
            fetched_at_unix: 0,
            profiles: vec![profile.clone()],
        };
        let hit = index.best_match("unknown", None, Some("notepad")).unwrap();
        assert_eq!(hit.source, ProfileSource::Bundled);
        // ...while the registry-facing wrapper still relabels.
        assert_eq!(
            index
                .matching_hash("unknown", None, Some("notepad"))
                .unwrap()
                .source,
            ProfileSource::Remote
        );
    }

    #[test]
    fn a_name_match_loses_to_a_digest_match() {
        let text = format!(
            "[{}, {}]",
            profile_json("by-name", "Notepad++", ""),
            profile_json("by-digest", "Something Else", "deadbeef"),
        );
        let index = RegistryIndex::from_json(&text, "https://example.com/r.json").unwrap();
        let hit = index
            .matching_hash("deadbeef", None, Some("notepad"))
            .unwrap();
        assert_eq!(hit.id, "by-digest", "an exact digest must always win");
    }

    #[test]
    fn freshness_window_is_enforced() {
        let text = "[]";
        let mut index = RegistryIndex::from_json(text, "u").unwrap();
        assert!(index.is_fresh(Duration::from_secs(60)));
        index.fetched_at_unix -= 3600;
        assert!(!index.is_fresh(Duration::from_secs(60)));
        assert!(index.is_fresh(Duration::from_secs(7200)));
    }

    #[test]
    fn a_bad_url_is_rejected_up_front() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(RegistryClient::new("ftp://example.com/r.json", tmp.path()).is_err());
        assert!(RegistryClient::new("not a url", tmp.path()).is_err());
        assert!(RegistryClient::new("https://example.com/r.json", tmp.path()).is_ok());
    }

    #[test]
    fn cached_documents_are_read_from_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let client = RegistryClient::new("https://example.com/r.json", tmp.path()).unwrap();
        assert!(client.cached().unwrap().is_none());

        let doc =
            RegistryIndex::from_json(&format!("[{}]", profile_json("a", "A", "aa")), client.url())
                .unwrap();
        std::fs::write(client.cache_path(), serde_json::to_string(&doc).unwrap()).unwrap();

        let cached = client.cached().unwrap().unwrap();
        assert_eq!(cached.profiles.len(), 1);
    }

    #[test]
    fn a_corrupt_cache_is_discarded_rather_than_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let client = RegistryClient::new("https://example.com/r.json", tmp.path()).unwrap();
        std::fs::write(client.cache_path(), "{{{ garbage").unwrap();
        assert!(client.cached().unwrap().is_none());
        assert!(
            !client.cache_path().exists(),
            "the bad cache should be removed"
        );
    }

    #[test]
    fn offline_mode_returns_the_cache_and_never_hits_the_network() {
        let tmp = tempfile::tempdir().unwrap();
        let client = RegistryClient::new("https://example.com/r.json", tmp.path())
            .unwrap()
            .with_ttl(Duration::from_secs(0)); // force staleness
        let doc =
            RegistryIndex::from_json(&format!("[{}]", profile_json("a", "A", "aa")), client.url())
                .unwrap();
        std::fs::write(client.cache_path(), serde_json::to_string(&doc).unwrap()).unwrap();

        let index = client.index(false).unwrap().unwrap();
        assert_eq!(index.profiles.len(), 1);

        // Nothing cached and no network allowed: cleanly empty.
        std::fs::remove_file(client.cache_path()).unwrap();
        assert!(client.index(false).unwrap().is_none());
    }

    #[test]
    fn a_fresh_cache_short_circuits_before_any_request() {
        let tmp = tempfile::tempdir().unwrap();
        // A URL that cannot resolve: if the cache is consulted first, no error.
        let client = RegistryClient::new("https://windrop.invalid/never.json", tmp.path()).unwrap();
        let doc =
            RegistryIndex::from_json(&format!("[{}]", profile_json("a", "A", "aa")), client.url())
                .unwrap();
        std::fs::write(client.cache_path(), serde_json::to_string(&doc).unwrap()).unwrap();
        let index = client.index(true).unwrap().unwrap();
        assert_eq!(index.profiles.len(), 1);
    }

    #[test]
    fn an_unreachable_registry_degrades_to_none_instead_of_erroring() {
        let tmp = tempfile::tempdir().unwrap();
        let client = RegistryClient::new("https://windrop.invalid/never.json", tmp.path())
            .unwrap()
            .with_ttl(Duration::from_secs(0));
        // Must not propagate a network error: the registry is optional.
        assert!(client.index(true).unwrap().is_none());
        assert!(client
            .find_by_hash("abc", None, None, true)
            .unwrap()
            .is_none());
    }
}
