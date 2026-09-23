//! Compatibility profiles that ship with WinDrop.
//!
//! A handful of applications are installed by so many people that a good recipe
//! is worth writing down once and reusing. The recipes live in
//! `registry/profiles.json` at the root of the repository — the same file the
//! community registry serves — and are compiled into the binary, so they work
//! with no network access at all.
//!
//! Seeding is opt-in rather than automatic. Writing profiles into a user's
//! database that they did not ask for would be a surprise, and a seeded profile
//! that later turns out to be wrong is worse than no profile at all: it takes
//! the blame for an install that would otherwise have been generated from the
//! executable itself.
//!
//! Every bundled profile is validated when it is read, and the test-suite
//! validates the whole file, so a malformed recipe cannot ship.

use crate::compat::profile::{AppProfile, ProfileSource};
use crate::db::ProfileDb;
use crate::registry::RegistryIndex;
use crate::Result;

/// The bundled registry document, compiled into the binary.
pub const BUNDLED_PROFILES_JSON: &str = include_str!("../../../../registry/profiles.json");

/// Parse the bundled document.
///
/// Returns every profile it contains, with provenance rewritten to
/// [`ProfileSource::Bundled`] so a user can always tell where a recipe came
/// from.
pub fn bundled_profiles() -> Result<Vec<AppProfile>> {
    let index = RegistryIndex::from_json(BUNDLED_PROFILES_JSON, "bundled:profiles.json")?;
    let mut profiles = index.profiles;
    for profile in &mut profiles {
        profile.validate()?;
        profile.source = ProfileSource::Bundled;
    }
    Ok(profiles)
}

/// How many recipes ship with WinDrop.
pub fn bundled_count() -> usize {
    bundled_profiles().map(|p| p.len()).unwrap_or(0)
}

/// A one-line description of each bundled recipe, for `windrop profiles bundled`.
pub fn bundled_summary() -> Result<Vec<String>> {
    Ok(bundled_profiles()?
        .into_iter()
        .map(|p| {
            format!(
                "{:<22} {:<34} {} environment{}",
                p.id,
                p.name,
                p.variants.len(),
                if p.variants.len() == 1 { "" } else { "s" }
            )
        })
        .collect())
}

/// Write the bundled recipes into a database.
///
/// Existing profiles are overwritten, because the point of seeding is to get the
/// recipe WinDrop ships. Profiles the user created or that were learned on this
/// machine are left alone: seeding must never destroy local knowledge.
pub fn seed(db: &ProfileDb) -> Result<SeedReport> {
    let bundled = bundled_profiles()?;
    let mut report = SeedReport::default();

    for profile in bundled {
        let existing = db.get_profile(&profile.id)?;
        match existing {
            Some(current) if current.source == ProfileSource::Local => {
                // Learned on this machine, from a machine that has actually run
                // the thing. That beats a recipe written by hand.
                report.kept_local.push(profile.id);
                continue;
            }
            Some(current) if current.source == ProfileSource::Bundled => {
                if current.variants.len() == profile.variants.len()
                    && current.updated_at == profile.updated_at
                {
                    report.already_current.push(profile.id);
                    continue;
                }
                db.upsert_profile(&profile)?;
                report.refreshed.push(profile.id);
            }
            Some(_) => {
                // A remote profile the user already has: overwriting it with a
                // bundled one would be a downgrade in the middle of an update.
                report.kept_existing.push(profile.id);
            }
            None => {
                db.upsert_profile(&profile)?;
                report.added.push(profile.id);
            }
        }
    }

    tracing::info!(
        added = report.added.len(),
        refreshed = report.refreshed.len(),
        "seeded bundled compatibility profiles"
    );
    Ok(report)
}

/// What [`seed`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SeedReport {
    /// Recipes written into an empty slot.
    pub added: Vec<String>,
    /// Bundled recipes the database already had, with newer content.
    pub refreshed: Vec<String>,
    /// Bundled recipes the database already had, unchanged.
    pub already_current: Vec<String>,
    /// Recipes learned on this machine, which were left untouched.
    pub kept_local: Vec<String>,
    /// Recipes from the community registry, which were left untouched.
    pub kept_existing: Vec<String>,
}

impl SeedReport {
    /// True when nothing was written.
    pub fn is_noop(&self) -> bool {
        self.added.is_empty() && self.refreshed.is_empty()
    }

    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.added.is_empty() {
            parts.push(format!("added {}", self.added.len()));
        }
        if !self.refreshed.is_empty() {
            parts.push(format!("updated {}", self.refreshed.len()));
        }
        if !self.already_current.is_empty() {
            parts.push(format!("{} already present", self.already_current.len()));
        }
        if !self.kept_local.is_empty() {
            parts.push(format!("kept {} learned locally", self.kept_local.len()));
        }
        if !self.kept_existing.is_empty() {
            parts.push(format!(
                "kept {} from the registry",
                self.kept_existing.len()
            ));
        }
        if parts.is_empty() {
            "nothing to do".to_string()
        } else {
            parts.join(", ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compat::profile::ProfileSource;

    #[test]
    fn every_bundled_profile_is_valid() {
        let profiles = bundled_profiles().expect("the bundled document must parse");
        assert!(!profiles.is_empty(), "shipping no recipes would be useless");
        for profile in &profiles {
            // `bundled_profiles` validates, but assert the properties the rest
            // of the pipeline relies on explicitly.
            assert!(
                !profile.variants.is_empty(),
                "{} has no variants",
                profile.id
            );
            assert!(
                !profile
                    .variants
                    .iter()
                    .any(|v| v.wine_build.trim().is_empty()),
                "{} has a variant with no Wine build",
                profile.id
            );
        }
    }

    #[test]
    fn every_bundled_profile_is_relabelled_as_bundled() {
        for profile in bundled_profiles().unwrap() {
            assert_eq!(profile.source, ProfileSource::Bundled, "{}", profile.id);
        }
    }

    #[test]
    fn bundled_ids_are_unique_and_slug_shaped() {
        let profiles = bundled_profiles().unwrap();
        let mut seen = std::collections::HashSet::new();
        for profile in &profiles {
            assert!(
                seen.insert(profile.id.clone()),
                "duplicate profile id {}",
                profile.id
            );
            assert_eq!(
                crate::compat::profile::slugify(&profile.id),
                profile.id,
                "{} is not a valid profile id",
                profile.id
            );
        }
    }

    #[test]
    fn bundled_profiles_carry_no_hashes_until_someone_verifies_one() {
        // A digest in this file would have to be a real installer's digest.
        // Shipping a guess would silently bind a recipe to the wrong file.
        for profile in bundled_profiles().unwrap() {
            assert!(
                profile.hashes.is_empty(),
                "{} ships a digest that cannot have been verified",
                profile.id
            );
        }
    }

    #[test]
    fn every_bundled_profile_is_reachable_by_its_own_name() {
        // The whole point of a bundled recipe is that dropping the installer
        // finds it, so each one must win a name lookup against its own name.
        let index = RegistryIndex {
            source_url: "bundled".into(),
            fetched_at_unix: 0,
            profiles: bundled_profiles().unwrap(),
        };
        for profile in &index.profiles {
            let hit = index
                .best_match("no-such-digest", None, Some(&profile.name))
                .unwrap_or_else(|| panic!("{} is unreachable by name", profile.id));
            assert_eq!(
                hit.id, profile.id,
                "{} matched the wrong profile",
                profile.id
            );
        }
    }

    #[test]
    fn bundled_installer_arguments_are_free_of_shell_metacharacters() {
        // They end up on a command line built by the CLI and the GUI.
        for profile in bundled_profiles().unwrap() {
            for arg in &profile.installer_args {
                assert!(
                    !arg.contains([';', '|', '&', '$', '`', '\n', '"']),
                    "{} has a suspicious installer argument: {arg}",
                    profile.id
                );
            }
        }
    }

    #[test]
    fn seeding_an_empty_database_adds_every_recipe() {
        let db = ProfileDb::open_in_memory().unwrap();
        let report = seed(&db).unwrap();
        assert_eq!(report.added.len(), bundled_count());
        assert!(report.kept_local.is_empty());
        assert!(!report.is_noop());
        assert_eq!(db.profile_count().unwrap(), bundled_count());
        assert!(report.summary().contains("added"));
    }

    #[test]
    fn seeding_twice_is_idempotent() {
        let db = ProfileDb::open_in_memory().unwrap();
        seed(&db).unwrap();
        let second = seed(&db).unwrap();
        assert!(second.is_noop(), "{second:?}");
        assert_eq!(second.already_current.len(), bundled_count());
        assert_eq!(db.profile_count().unwrap(), bundled_count());
    }

    #[test]
    fn seeding_never_overwrites_what_was_learned_on_this_machine() {
        let db = ProfileDb::open_in_memory().unwrap();
        let mut profile = bundled_profiles().unwrap()[0].clone();
        profile.source = ProfileSource::Local;
        profile.variants.truncate(1);
        db.upsert_profile(&profile).unwrap();

        let report = seed(&db).unwrap();
        assert!(report.kept_local.contains(&profile.id));
        let after = db.get_profile(&profile.id).unwrap().unwrap();
        assert_eq!(
            after.source,
            ProfileSource::Local,
            "local knowledge must survive"
        );
        assert_eq!(after.variants.len(), 1);
    }

    #[test]
    fn seeding_does_not_replace_a_newer_profile_from_the_registry() {
        let db = ProfileDb::open_in_memory().unwrap();
        let mut profile = bundled_profiles().unwrap()[0].clone();
        profile.source = ProfileSource::Remote;
        profile.updated_at = Some("2099-01-01T00:00:00Z".into());
        db.upsert_profile(&profile).unwrap();

        let report = seed(&db).unwrap();
        assert!(report.kept_existing.contains(&profile.id));
        assert_eq!(
            db.get_profile(&profile.id).unwrap().unwrap().updated_at,
            Some("2099-01-01T00:00:00Z".into())
        );
    }

    #[test]
    fn the_summary_lists_what_it_did() {
        let db = ProfileDb::open_in_memory().unwrap();
        let report = seed(&db).unwrap();
        let text = report.summary();
        assert!(text.contains("added"), "{text}");
        assert!(
            text.contains(&report.added.len().to_string()),
            "the summary must say how many were added: {text}"
        );

        // ...and when there is nothing to do, it says that instead of nothing.
        let empty = SeedReport::default();
        assert_eq!(empty.summary(), "nothing to do");
        assert!(empty.is_noop());
    }
}
