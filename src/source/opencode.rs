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
            db: crate::data_home(home).join("opencode/opencode.db"),
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

    /// Nothing is running the session, so `opencode run` may own it for one
    /// message: it answers and exits. `--` ends its flags, so a message
    /// starting with `-` stays a message.
    fn insert_command(&self, id: &str, message: &str) -> Option<Vec<String>> {
        Some(run_command(id, message, None))
    }

    /// A running client is reached through the one server it is connected
    /// to — the shared `opencode serve --service` for a v2 client, the
    /// in-process server for a v1 TUI. The server comes from the client's
    /// sockets, not its flags: flag spellings differ between versions, and a
    /// guessed server would be a second writer. No server, or several to
    /// choose from, is refused.
    fn live_insert_command(&self, id: &str, message: &str, servers: &[u16]) -> Option<Vec<String>> {
        let [port] = servers else {
            return None;
        };
        Some(run_command(
            id,
            message,
            Some(&format!("http://127.0.0.1:{port}")),
        ))
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

/// `opencode run --session <id> [--server <url>] -- <message>`, the
/// spelling of `opencode run --help` in 2.0.18.
fn run_command(id: &str, message: &str, server: Option<&str>) -> Vec<String> {
    let mut command = vec![
        "opencode".to_owned(),
        "run".to_owned(),
        "--session".to_owned(),
        id.to_owned(),
    ];
    if let Some(server) = server {
        command.extend(["--server".to_owned(), server.to_owned()]);
    }
    command.extend(["--".to_owned(), message.to_owned()]);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The message must reach the server the running client is connected
    /// to; any other would be a second writer on the same session.
    #[test]
    fn a_live_client_is_reached_only_through_its_one_server() {
        let opencode = OpenCode::new(Path::new("/home/u"));
        let send = |servers: &[u16]| opencode.live_insert_command("ses_1", "hi", servers);
        assert_eq!(
            send(&[49374]).unwrap(),
            [
                "opencode",
                "run",
                "--session",
                "ses_1",
                "--server",
                "http://127.0.0.1:49374",
                "--",
                "hi"
            ]
        );
        assert_eq!(send(&[]), None, "no known server");
        assert_eq!(send(&[49374, 8080]), None, "ambiguous");
    }
}
