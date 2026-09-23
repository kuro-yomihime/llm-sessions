//! `OpenCode`: `~/.local/share/opencode/opencode.db`, v1 and v2 alike.
//!
//! The only source with a real index. One indexed query yields every list
//! column — title, directory, model, tokens, cost, archived state — for all
//! ~1900 sessions in tens of milliseconds, so tier 1 never touches a body.
//!
//! `session.directory` and `project.worktree` store the path verbatim, so
//! there is nothing to decode.
//!
//! The database is 4.9 GB with a live WAL and is written while we browse; the
//! session count moved during a single research pass. Reading goes through
//! `storage::sqlite::open`, which is read-only with an `immutable=1` fallback.
//!
//! `OpenCode` v2 (2.0.x) keeps the same file and binary name but writes new
//! `session_v2`/`session_message` tables beside the v1 ones; one source reads
//! both, so a migrated session is listed once rather than per generation.
//! Kilo ships the v1 Drizzle schema, so the decoding lives in the shared
//! `opencode_schema` module used by both sources.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::model::{ArchiveOutcome, Locator, SessionSummary, SourceId, Transcript};
use crate::source::{RemoteLayout, Source, opencode_schema, row_locator};

#[derive(Debug)]
pub struct OpenCode {
    db: PathBuf,
}

impl OpenCode {
    pub fn new(home: &Path) -> Self {
        Self {
            db: home.join(".local/share/opencode/opencode.db"),
        }
    }
}

impl Source for OpenCode {
    fn id(&self) -> SourceId {
        SourceId::OpenCode
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.db.clone()]
    }

    fn list(&self) -> Result<Vec<SessionSummary>> {
        opencode_schema::list(&self.db, SourceId::OpenCode)
    }

    fn open(&self, locator: &Locator) -> Result<Transcript> {
        let (db, id) = row_locator(SourceId::OpenCode, locator)?;
        opencode_schema::open(db, SourceId::OpenCode, id)
    }

    /// `OpenCode` models archiving on `session.time_archived`, so the state
    /// belongs in its own store rather than in our overlay.
    fn archive(&self, locator: &Locator, archived: bool) -> Result<ArchiveOutcome> {
        let (db, id) = row_locator(SourceId::OpenCode, locator)?;
        opencode_schema::set_archived(db, id, archived)
    }

    fn resume_command(&self, id: &str) -> Option<Vec<String>> {
        Some(vec!["opencode".into(), "--session".into(), id.into()])
    }

    /// One remote `sqlite3` query answers tier 1, so there is nothing to glob.
    fn remote_layout(&self) -> RemoteLayout {
        RemoteLayout {
            files: &[],
            sql: Some((
                ".local/share/opencode/opencode.db",
                opencode_schema::REMOTE_LIST_SQL,
            )),
        }
    }
}
