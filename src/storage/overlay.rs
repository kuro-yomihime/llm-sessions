//! State `llm-sessions` keeps about sessions whose agent has no place to put it.
//!
//! OMP, Claude, Codex rollouts, Cursor, Goose and Gemini have no archive
//! concept. Rather than inventing a field inside another tool's transcript,
//! that state lives here, under `$XDG_STATE_HOME/llm-sessions/overlay.db`.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::error::{Result, io, sql};
use crate::model::SourceId;

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS archived (
    source TEXT NOT NULL,
    id     TEXT NOT NULL,
    at     INTEGER NOT NULL,
    PRIMARY KEY (source, id)
) WITHOUT ROWID";

#[derive(Debug)]
pub struct Overlay {
    path: PathBuf,
    conn: Connection,
}

impl Overlay {
    /// Default location, honouring `$XDG_STATE_HOME`.
    pub fn default_path(home: &Path) -> PathBuf {
        std::env::var_os("XDG_STATE_HOME")
            .map_or_else(|| home.join(".local/state"), PathBuf::from)
            .join("llm-sessions/overlay.db")
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io(parent))?;
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
        .map_err(sql(path))?;
        conn.execute_batch(SCHEMA).map_err(sql(path))?;
        Ok(Self {
            path: path.to_path_buf(),
            conn,
        })
    }

    /// Every archived `(source, id)` pair, for folding into a scan.
    pub fn archived(&self) -> Result<Vec<(SourceId, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT source, id FROM archived")
            .map_err(sql(&self.path))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(sql(&self.path))?;
        let mut out = Vec::new();
        for row in rows {
            let (source, id) = row.map_err(sql(&self.path))?;
            if let Some(source) = SourceId::parse(&source) {
                out.push((source, id));
            }
        }
        Ok(out)
    }

    pub fn set_archived(&self, source: SourceId, id: &str, archived: bool) -> Result<()> {
        if archived {
            let now = time::OffsetDateTime::now_utc().unix_timestamp();
            self.conn
                .execute(
                    "INSERT INTO archived (source, id, at) VALUES (?1, ?2, ?3)
                     ON CONFLICT (source, id) DO UPDATE SET at = excluded.at",
                    rusqlite::params![source.name(), id, now],
                )
                .map_err(sql(&self.path))?;
        } else {
            self.conn
                .execute(
                    "DELETE FROM archived WHERE source = ?1 AND id = ?2",
                    rusqlite::params![source.name(), id],
                )
                .map_err(sql(&self.path))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_state_round_trips_and_clears() {
        let dir = tempfile::tempdir().unwrap();
        let overlay = Overlay::open(&dir.path().join("o.db")).unwrap();

        overlay.set_archived(SourceId::Omp, "abc", true).unwrap();
        // Repeating must not violate the primary key.
        overlay.set_archived(SourceId::Omp, "abc", true).unwrap();
        assert_eq!(
            overlay.archived().unwrap(),
            vec![(SourceId::Omp, "abc".into())]
        );

        overlay.set_archived(SourceId::Omp, "abc", false).unwrap();
        assert!(overlay.archived().unwrap().is_empty());
    }

    #[test]
    fn ids_are_namespaced_by_source() {
        let dir = tempfile::tempdir().unwrap();
        let overlay = Overlay::open(&dir.path().join("o.db")).unwrap();
        overlay.set_archived(SourceId::Omp, "same", true).unwrap();
        overlay
            .set_archived(SourceId::Claude, "same", true)
            .unwrap();
        assert_eq!(overlay.archived().unwrap().len(), 2);
    }

    #[test]
    fn every_registry_source_survives_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let overlay = Overlay::open(&dir.path().join("o.db")).unwrap();
        for source in SourceId::ALL {
            overlay.set_archived(*source, "s", true).unwrap();
        }
        // A source the overlay cannot name is silently dropped on read; the
        // registry is what keeps that set equal to the real inventory.
        assert_eq!(overlay.archived().unwrap().len(), SourceId::ALL.len());
    }
}
