//! The local compatibility database.
//!
//! SQLite holds three kinds of fact:
//!
//! * **profiles** — the recipes, keyed by slug.
//! * **profile_hashes** — which installer digests a profile is known to fit.
//!   This is the index that turns "user dropped a file" into "we already know
//!   how to run this".
//! * **variant_learnings** — which variant of the fallback chain actually
//!   worked. WinDrop moves that variant to the front on the next install of the
//!   same application, so a successful install makes future ones faster.

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};

use crate::compat::profile::{AppProfile, ProfileSource};
use crate::{Error, Result};

/// Bump this and add a migration arm in [`ProfileDb::migrate`] when the schema
/// changes.
const SCHEMA_VERSION: i64 = 1;

#[derive(Debug)]
pub struct ProfileDb {
    conn: Connection,
}

impl ProfileDb {
    /// Open (creating if needed) the database at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        let db = ProfileDb { conn };
        db.configure()?;
        db.migrate()?;
        Ok(db)
    }

    /// An ephemeral database, for tests and for `--dry-run`.
    pub fn open_in_memory() -> Result<Self> {
        let db = ProfileDb {
            conn: Connection::open_in_memory()?,
        };
        db.configure()?;
        db.migrate()?;
        Ok(db)
    }

    fn configure(&self) -> Result<()> {
        // WAL keeps reads from blocking the GUI thread while an install writes.
        let _ = self.conn.pragma_update(None, "journal_mode", "WAL");
        self.conn.pragma_update(None, "foreign_keys", "ON")?;
        self.conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(())
    }

    fn migrate(&self) -> Result<()> {
        let version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;

        if version > SCHEMA_VERSION {
            return Err(Error::Config {
                field: "profiles.db".into(),
                reason: format!(
                    "the database was written by a newer WinDrop (schema {version}, this build \
                     understands {SCHEMA_VERSION}). Update WinDrop to continue."
                ),
            });
        }

        if version < 1 {
            self.conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS profiles (
                    id          TEXT PRIMARY KEY,
                    name        TEXT NOT NULL,
                    version     TEXT NOT NULL DEFAULT '',
                    arch        TEXT,
                    source      TEXT NOT NULL,
                    updated_at  TEXT,
                    json        TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS profile_hashes (
                    hash        TEXT PRIMARY KEY,
                    profile_id  TEXT NOT NULL REFERENCES profiles(id) ON DELETE CASCADE
                );
                CREATE INDEX IF NOT EXISTS idx_profile_hashes_profile
                    ON profile_hashes(profile_id);

                CREATE TABLE IF NOT EXISTS variant_learnings (
                    app_id      TEXT PRIMARY KEY,
                    profile_id  TEXT NOT NULL,
                    signature   TEXT NOT NULL,
                    wins        INTEGER NOT NULL DEFAULT 1,
                    updated_at  TEXT NOT NULL
                );
                "#,
            )?;
            self.conn
                .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(())
    }

    /// Insert or replace a profile, replacing its hash index too.
    pub fn upsert_profile(&self, profile: &AppProfile) -> Result<()> {
        profile.validate()?;
        let json = serde_json::to_string(profile)?;
        let source = source_key(profile.source);
        let arch = profile.arch.map(|a| a.to_string());

        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO profiles (id, name, version, arch, source, updated_at, json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                version = excluded.version,
                arch = excluded.arch,
                source = excluded.source,
                updated_at = excluded.updated_at,
                json = excluded.json",
            params![
                profile.id,
                profile.name,
                profile.version,
                arch,
                source,
                profile.updated_at,
                json
            ],
        )?;
        // Rebuild the hash index for this profile so stale digests disappear.
        tx.execute(
            "DELETE FROM profile_hashes WHERE profile_id = ?1",
            params![profile.id],
        )?;
        for hash in &profile.hashes {
            tx.execute(
                "INSERT OR REPLACE INTO profile_hashes (hash, profile_id) VALUES (?1, ?2)",
                params![hash.to_ascii_lowercase(), profile.id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_profile(&self, id: &str) -> Result<Option<AppProfile>> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT json FROM profiles WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        match json {
            Some(text) => Ok(Some(AppProfile::from_json(&text)?)),
            None => Ok(None),
        }
    }

    /// The profile a specific installer digest maps to.
    pub fn find_profile_by_hash(&self, sha256: &str) -> Result<Option<AppProfile>> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT p.json FROM profiles p
                 JOIN profile_hashes h ON h.profile_id = p.id
                 WHERE h.hash = ?1",
                params![sha256.to_ascii_lowercase()],
                |row| row.get(0),
            )
            .optional()?;
        match json {
            Some(text) => Ok(Some(AppProfile::from_json(&text)?)),
            None => Ok(None),
        }
    }

    /// Every profile, ordered by name for a stable UI.
    pub fn all_profiles(&self) -> Result<Vec<AppProfile>> {
        let mut stmt = self
            .conn
            .prepare("SELECT json FROM profiles ORDER BY name COLLATE NOCASE")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(AppProfile::from_json(&row?)?);
        }
        Ok(out)
    }

    pub fn delete_profile(&self, id: &str) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let hashes = tx.execute(
            "DELETE FROM profile_hashes WHERE profile_id = ?1",
            params![id],
        )?;
        let profiles = tx.execute("DELETE FROM profiles WHERE id = ?1", params![id])?;
        tx.execute(
            "DELETE FROM variant_learnings WHERE profile_id = ?1",
            params![id],
        )?;
        tx.commit()?;
        let _ = hashes;
        Ok(profiles > 0)
    }

    pub fn profile_count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM profiles", [], |row| row.get(0))?;
        Ok(n as usize)
    }

    /// Remember which variant of the chain worked for an application.
    ///
    /// Repeat successes increment a counter so a fluke does not permanently
    /// reorder the chain.
    pub fn record_variant_success(
        &self,
        app_id: &str,
        profile_id: &str,
        signature: &str,
    ) -> Result<()> {
        let now = now_iso8601();
        self.conn.execute(
            "INSERT INTO variant_learnings (app_id, profile_id, signature, wins, updated_at)
             VALUES (?1, ?2, ?3, 1, ?4)
             ON CONFLICT(app_id) DO UPDATE SET
                signature = excluded.signature,
                profile_id = excluded.profile_id,
                wins = CASE WHEN signature = excluded.signature THEN wins + 1 ELSE 1 END,
                updated_at = excluded.updated_at",
            params![app_id, profile_id, signature, now],
        )?;
        Ok(())
    }

    /// The variant signature that previously worked for this application.
    pub fn preferred_variant(&self, app_id: &str) -> Result<Option<String>> {
        let sig: Option<String> = self
            .conn
            .query_row(
                "SELECT signature FROM variant_learnings WHERE app_id = ?1",
                params![app_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(sig)
    }

    /// Number of recorded variant successes, for diagnostics.
    pub fn learning_count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM variant_learnings", [], |row| {
                row.get(0)
            })?;
        Ok(n as usize)
    }

    pub fn forget_learning(&self, app_id: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM variant_learnings WHERE app_id = ?1",
            params![app_id],
        )? > 0)
    }
}

fn source_key(source: ProfileSource) -> &'static str {
    match source {
        ProfileSource::Bundled => "bundled",
        ProfileSource::Local => "local",
        ProfileSource::Remote => "remote",
        ProfileSource::Generated => "generated",
    }
}

/// A UTC timestamp in `YYYY-MM-DDTHH:MM:SSZ`, without pulling in a date crate.
pub fn now_iso8601() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_iso8601(secs as i64)
}

/// Convert Unix seconds to an ISO-8601 UTC timestamp.
pub fn format_iso8601(unix_seconds: i64) -> String {
    let days = unix_seconds.div_euclid(86_400);
    let rem = unix_seconds.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Howard Hinnant's `civil_from_days`, adapted. Days are counted from
/// 1970-01-01.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compat::pe::PeInspection;
    use crate::compat::profile::generic_profile;
    use crate::config::Config;
    use crate::fixtures::{self, PeSpec};
    use std::path::Path as StdPath;

    fn sample_profile() -> AppProfile {
        let bytes = fixtures::synthetic_pe(&PeSpec::example_installer());
        let inspection: PeInspection =
            crate::compat::pe::inspect_bytes(&bytes, StdPath::new("app.exe")).unwrap();
        generic_profile(&inspection, &Config::default())
    }

    #[test]
    fn opening_creates_the_file_and_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/profiles.db");
        let db = ProfileDb::open(&path).unwrap();
        assert!(path.is_file());
        assert_eq!(db.profile_count().unwrap(), 0);
    }

    #[test]
    fn reopening_an_existing_database_keeps_its_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("profiles.db");
        let profile = sample_profile();
        {
            let db = ProfileDb::open(&path).unwrap();
            db.upsert_profile(&profile).unwrap();
        }
        let db = ProfileDb::open(&path).unwrap();
        assert_eq!(db.get_profile(&profile.id).unwrap().unwrap(), profile);
    }

    #[test]
    fn profiles_round_trip_with_all_their_fields() {
        let db = ProfileDb::open_in_memory().unwrap();
        let profile = sample_profile();
        db.upsert_profile(&profile).unwrap();
        let got = db.get_profile(&profile.id).unwrap().unwrap();
        assert_eq!(got, profile);
        assert_eq!(got.variants.len(), profile.variants.len());
        assert_eq!(
            got.variants[0].dependencies,
            profile.variants[0].dependencies
        );
    }

    #[test]
    fn upsert_updates_in_place_rather_than_duplicating() {
        let db = ProfileDb::open_in_memory().unwrap();
        let mut profile = sample_profile();
        db.upsert_profile(&profile).unwrap();
        profile.name = "Renamed".into();
        db.upsert_profile(&profile).unwrap();
        assert_eq!(db.profile_count().unwrap(), 1);
        assert_eq!(
            db.get_profile(&profile.id).unwrap().unwrap().name,
            "Renamed"
        );
    }

    #[test]
    fn hash_index_finds_the_profile_for_a_dropped_file() {
        let db = ProfileDb::open_in_memory().unwrap();
        let profile = sample_profile();
        db.upsert_profile(&profile).unwrap();
        let hash = &profile.hashes[0];

        let found = db.find_profile_by_hash(hash).unwrap().unwrap();
        assert_eq!(found.id, profile.id);

        // Case-insensitive, because digests travel through user-facing tools.
        assert!(db
            .find_profile_by_hash(&hash.to_uppercase())
            .unwrap()
            .is_some());
        assert!(db.find_profile_by_hash("deadbeef").unwrap().is_none());
    }

    #[test]
    fn reindexing_drops_stale_hashes() {
        let db = ProfileDb::open_in_memory().unwrap();
        let mut profile = sample_profile();
        let old_hash = profile.hashes[0].clone();
        db.upsert_profile(&profile).unwrap();

        profile.hashes =
            vec!["1111111111111111111111111111111111111111111111111111111111111111".into()];
        db.upsert_profile(&profile).unwrap();

        assert!(db.find_profile_by_hash(&old_hash).unwrap().is_none());
        assert!(db
            .find_profile_by_hash(&profile.hashes[0])
            .unwrap()
            .is_some());
    }

    #[test]
    fn profiles_are_listed_alphabetically() {
        let db = ProfileDb::open_in_memory().unwrap();
        let mut a = sample_profile();
        a.id = "alpha".into();
        a.name = "zebra".into();
        let mut b = sample_profile();
        b.id = "beta".into();
        b.name = "Apple".into();
        db.upsert_profile(&a).unwrap();
        db.upsert_profile(&b).unwrap();
        let names: Vec<String> = db
            .all_profiles()
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, vec!["Apple".to_string(), "zebra".to_string()]);
    }

    #[test]
    fn deleting_a_profile_cascades_to_its_hashes() {
        let db = ProfileDb::open_in_memory().unwrap();
        let profile = sample_profile();
        let hash = profile.hashes[0].clone();
        db.upsert_profile(&profile).unwrap();

        assert!(db.delete_profile(&profile.id).unwrap());
        assert!(db.get_profile(&profile.id).unwrap().is_none());
        assert!(db.find_profile_by_hash(&hash).unwrap().is_none());
        assert!(
            !db.delete_profile(&profile.id).unwrap(),
            "second delete is a no-op"
        );
    }

    #[test]
    fn invalid_profiles_are_refused_before_hitting_the_database() {
        let db = ProfileDb::open_in_memory().unwrap();
        let mut profile = sample_profile();
        profile.variants.clear();
        assert!(db.upsert_profile(&profile).is_err());
        assert_eq!(db.profile_count().unwrap(), 0);
    }

    #[test]
    fn variant_learning_moves_the_winning_variant_to_the_front() {
        let db = ProfileDb::open_in_memory().unwrap();
        assert!(db.preferred_variant("notepad").unwrap().is_none());

        db.record_variant_success("notepad", "notepad-64", "sig-a")
            .unwrap();
        assert_eq!(db.preferred_variant("notepad").unwrap().unwrap(), "sig-a");

        db.record_variant_success("notepad", "notepad-64", "sig-b")
            .unwrap();
        assert_eq!(db.preferred_variant("notepad").unwrap().unwrap(), "sig-b");

        // Repeating the same winner increments the counter instead of resetting.
        db.record_variant_success("notepad", "notepad-64", "sig-b")
            .unwrap();
        let wins: i64 = db
            .conn
            .query_row(
                "SELECT wins FROM variant_learnings WHERE app_id = 'notepad'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(wins, 2);

        // A different variant resets the streak.
        db.record_variant_success("notepad", "notepad-64", "sig-c")
            .unwrap();
        let wins: i64 = db
            .conn
            .query_row(
                "SELECT wins FROM variant_learnings WHERE app_id = 'notepad'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(wins, 1);
    }

    #[test]
    fn learnings_are_per_application() {
        let db = ProfileDb::open_in_memory().unwrap();
        db.record_variant_success("app-a", "p", "sig-a").unwrap();
        db.record_variant_success("app-b", "p", "sig-b").unwrap();
        assert_eq!(db.preferred_variant("app-a").unwrap().unwrap(), "sig-a");
        assert_eq!(db.preferred_variant("app-b").unwrap().unwrap(), "sig-b");
        assert_eq!(db.learning_count().unwrap(), 2);

        assert!(db.forget_learning("app-a").unwrap());
        assert!(db.preferred_variant("app-a").unwrap().is_none());
        assert!(!db.forget_learning("app-a").unwrap());
    }

    #[test]
    fn a_newer_schema_is_refused_with_a_clear_message() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("profiles.db");
        {
            let db = ProfileDb::open(&path).unwrap();
            db.conn.pragma_update(None, "user_version", 999).unwrap();
        }
        match ProfileDb::open(&path) {
            Err(Error::Config { reason, .. }) => assert!(reason.contains("newer WinDrop")),
            other => panic!("expected a schema error, got {other:?}"),
        }
    }

    #[test]
    fn iso8601_formatting_matches_known_instants() {
        assert_eq!(format_iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_iso8601(1_000_000_000), "2001-09-09T01:46:40Z");
        assert_eq!(format_iso8601(1_700_000_000), "2023-11-14T22:13:20Z");
        // Leap-year day, to exercise the civil-date maths.
        assert_eq!(format_iso8601(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn now_is_a_plausible_iso8601_timestamp() {
        let now = now_iso8601();
        assert_eq!(now.len(), 20);
        assert!(now.ends_with('Z'));
        assert!(now.starts_with("20"));
    }
}
