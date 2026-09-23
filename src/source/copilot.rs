//! GitHub Copilot CLI: `~/.copilot/session-store.db`.
//!
//! The inspected `sessions` table records `cwd`, `summary`, `created_at`, and
//! `updated_at` (for example, the observed row was
//! `{"id":"0688…","cwd":"/tmp","summary":null,…}`). It has no model, usage,
//! parent, or archive columns. A session-state `workspace.yaml` may supply
//! `cwd` when the database row does not; the observed file contains `cwd: /tmp`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, params};
use serde_json::json;

use crate::error::{Error, Result, sql};
use crate::model::{Block, Locator, Role, SessionSummary, SourceId, Transcript, Turn};
use crate::source::{Source, clean_title, parse_rfc3339, read_head, row_locator};
use crate::storage::sqlite;

const WORKSPACE_BYTES: usize = 8 * 1024;
const SESSION_QUERY: &str = "\
SELECT id, cwd, summary, created_at, updated_at
FROM sessions
ORDER BY updated_at DESC";

#[derive(Debug)]
pub struct Copilot {
    db: PathBuf,
    state_root: PathBuf,
}

impl Copilot {
    pub fn new(home: &Path) -> Self {
        let root = home.join(".copilot");
        Self {
            db: root.join("session-store.db"),
            state_root: root.join("session-state"),
        }
    }
}

impl Source for Copilot {
    fn id(&self) -> SourceId {
        SourceId::Copilot
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.db.clone(), self.state_root.clone()]
    }

    fn list(&self) -> Result<Vec<SessionSummary>> {
        if !self.db.exists() {
            return Ok(Vec::new());
        }
        let (connection, _stale) = sqlite::open(&self.db)?;
        let mut statement = connection.prepare(SESSION_QUERY).map_err(sql(&self.db))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })
            .map_err(sql(&self.db))?;

        let mut sessions = Vec::new();
        for row in rows {
            let (id, cwd, summary, created, updated) = row.map_err(sql(&self.db))?;
            let updated = updated
                .as_deref()
                .and_then(parse_rfc3339)
                .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
            sessions.push(SessionSummary {
                recorded_cwd: cwd
                    .or(workspace_cwd(&self.state_root, &id)?)
                    .map(PathBuf::from),
                title: summary.as_deref().and_then(clean_title),
                created: created.as_deref().and_then(parse_rfc3339),
                ..SessionSummary::new(
                    id.clone(),
                    self.id(),
                    updated,
                    Locator::Row {
                        db: self.db.clone(),
                        id,
                    },
                )
            });
        }
        Ok(sessions)
    }

    fn open(&self, locator: &Locator) -> Result<Transcript> {
        let (db, id) = row_locator(self.id(), locator)?;
        open_database(db, self.id(), id)
    }

    fn resume_command(&self, id: &str) -> Option<Vec<String>> {
        // Verified from `copilot --help`: `--resume[=value]` accepts a session id.
        Some(vec!["copilot".into(), "--resume".into(), id.into()])
    }
}

fn workspace_cwd(state_root: &Path, id: &str) -> Result<Option<String>> {
    let path = state_root.join(id).join("workspace.yaml");
    if !path.exists() {
        return Ok(None);
    }
    let bytes = read_head(&path, WORKSPACE_BYTES)?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(text.lines().find_map(|line| {
        let value = line.trim().strip_prefix("cwd:")?.trim();
        let value = value.trim_matches(&['"', '\''][..]);
        (!value.is_empty()).then_some(value.to_owned())
    }))
}

fn open_database(db: &Path, source: SourceId, id: &str) -> Result<Transcript> {
    let (connection, _stale) = sqlite::open(db)?;
    let summary = connection
        .query_row(
            "SELECT summary FROM sessions WHERE id = ?1",
            params![id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(sql(db))?
        .ok_or_else(|| Error::MissingRow {
            db: db.to_path_buf(),
            id: id.to_owned(),
        })?;
    let mut transcript = Transcript::new(id, source);
    transcript.title = summary.as_deref().and_then(clean_title);

    let events = events_by_turn(&connection, db, id)?;
    let mut statement = connection
        .prepare(
            "SELECT turn_index, user_message, assistant_response, timestamp
             FROM turns WHERE session_id = ?1 ORDER BY turn_index",
        )
        .map_err(sql(db))?;
    let turns = statement
        .query_map(params![id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(sql(db))?;

    let mut events = events;
    for turn in turns {
        let (index, user_message, assistant_response, timestamp) = turn.map_err(sql(db))?;
        let timestamp = timestamp.as_deref().and_then(parse_rfc3339);
        if let Some(message) = user_message.filter(|message| !message.trim().is_empty()) {
            let mut turn = Turn::new(Role::User);
            turn.timestamp = timestamp;
            turn.blocks.push(Block::text(message));
            transcript.push(turn);
        }

        let mut assistant = Turn::new(Role::Assistant);
        assistant.timestamp = timestamp;
        if let Some(response) = assistant_response.filter(|response| !response.trim().is_empty()) {
            assistant.blocks.push(Block::text(response));
        }
        if let Some(turn_events) = events.remove(&index) {
            for event in turn_events {
                append_event(&mut assistant, event);
            }
        }
        transcript.push(assistant);
    }

    // A database migration or interrupted write can leave a trajectory event
    // without a turn row. Preserve it rather than silently discarding it.
    for (_index, orphaned) in events {
        let mut turn = Turn::new(Role::Tool);
        for event in orphaned {
            append_event(&mut turn, event);
        }
        transcript.push(turn);
    }
    Ok(transcript)
}

#[derive(Debug)]
struct Event {
    kind: String,
    tool_call_id: Option<String>,
    command: Option<String>,
    output: Option<String>,
    exit_code: Option<i64>,
    key: Option<String>,
    value: Option<String>,
}

fn events_by_turn(
    connection: &rusqlite::Connection,
    db: &Path,
    id: &str,
) -> Result<BTreeMap<i64, Vec<Event>>> {
    let mut statement = connection
        .prepare(
            "SELECT turn_index, event_type, tool_call_id, command, output,
                    exit_code, event_key, event_value
             FROM forge_trajectory_events
             WHERE session_id = ?1
             ORDER BY turn_index, id",
        )
        .map_err(sql(db))?;
    let rows = statement
        .query_map(params![id], |row| {
            Ok((
                row.get::<_, Option<i64>>(0)?.unwrap_or(-1),
                Event {
                    kind: row.get(1)?,
                    tool_call_id: row.get(2)?,
                    command: row.get(3)?,
                    output: row.get(4)?,
                    exit_code: row.get(5)?,
                    key: row.get(6)?,
                    value: row.get(7)?,
                },
            ))
        })
        .map_err(sql(db))?;
    let mut events = BTreeMap::new();
    for row in rows {
        let (index, event) = row.map_err(sql(db))?;
        events.entry(index).or_insert_with(Vec::new).push(event);
    }
    Ok(events)
}

fn append_event(turn: &mut Turn, event: Event) {
    turn.blocks.push(Block::ToolCall {
        name: event.kind,
        input: json!({
            "tool_call_id": event.tool_call_id,
            "command": event.command,
            "event_key": event.key,
            "event_value": event.value,
        }),
    });
    if event.output.is_some() || event.exit_code.is_some() {
        turn.blocks.push(Block::ToolResult {
            name: None,
            ok: event.exit_code.unwrap_or_default() == 0,
            output: event.output.unwrap_or_default(),
        });
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::Connection;

    use super::*;

    fn fixture() -> (tempfile::TempDir, Copilot) {
        let home = tempfile::tempdir().expect("temp home");
        let copilot_root = home.path().join(".copilot");
        fs::create_dir_all(copilot_root.join("session-state/s/workspace")).expect("state root");
        fs::write(
            copilot_root.join("session-state/s/workspace.yaml"),
            "cwd: /from-state\n",
        )
        .expect("workspace state");
        let db = copilot_root.join("session-store.db");
        let connection = Connection::open(&db).expect("fixture database");
        connection
            .execute_batch(
                "CREATE TABLE sessions(
                    id TEXT PRIMARY KEY, cwd TEXT, repository TEXT, host_type TEXT,
                    branch TEXT, summary TEXT, created_at TEXT, updated_at TEXT
                 );
                 CREATE TABLE turns(
                    id INTEGER PRIMARY KEY, session_id TEXT, turn_index INTEGER,
                    user_message TEXT, assistant_response TEXT, timestamp TEXT
                 );
                 CREATE TABLE forge_trajectory_events(
                    id INTEGER PRIMARY KEY, session_id TEXT, tool_call_id TEXT,
                    turn_index INTEGER, event_type TEXT, command TEXT, output TEXT,
                    exit_code INTEGER, event_key TEXT, event_value TEXT, created_at TEXT
                 );
                 INSERT INTO sessions VALUES (
                    's', NULL, NULL, NULL, NULL, ' Title ',
                    '2026-01-02T03:04:05Z', '2026-01-02T03:04:06Z'
                 );
                 INSERT INTO turns VALUES (
                    1, 's', 0, 'question', 'answer', '2026-01-02T03:04:07Z'
                 );
                 INSERT INTO forge_trajectory_events VALUES (
                    1, 's', 'call-1', 0, 'shell', 'pwd', '/from-state', 0,
                    NULL, NULL, '2026-01-02T03:04:08Z'
                 );",
            )
            .expect("fixture schema");
        let copilot = Copilot::new(home.path());
        (home, copilot)
    }

    #[test]
    fn uses_workspace_state_when_database_cwd_is_missing() {
        let (_home, source) = fixture();
        let sessions = source.list().expect("list fixture");
        assert_eq!(sessions[0].recorded_cwd, Some(PathBuf::from("/from-state")));
        assert_eq!(sessions[0].title.as_deref(), Some("Title"));
    }

    #[test]
    fn opens_turns_and_trajectory_events() {
        let (_home, source) = fixture();
        let transcript = source
            .open(&Locator::Row {
                db: source.db.clone(),
                id: "s".into(),
            })
            .expect("open fixture");
        assert_eq!(transcript.turns.len(), 2);
        assert!(matches!(
            transcript.turns[1].blocks[1],
            Block::ToolCall { ref name, .. } if name == "shell"
        ));
        assert!(matches!(
            transcript.turns[1].blocks[2],
            Block::ToolResult { ok: true, ref output, .. } if output == "/from-state"
        ));
    }
}
