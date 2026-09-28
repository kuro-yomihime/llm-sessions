//! GitHub Copilot CLI: `~/.copilot/session-store.db`.
//!
//! The inspected `sessions` table records `cwd`, `summary`, `created_at`, and
//! `updated_at` (for example, the observed row was
//! `{"id":"0688…","cwd":"/tmp","summary":null,…}`). It has no model, usage,
//! parent, or archive columns. A session-state `workspace.yaml` may supply
//! `cwd` when the database row does not; the observed file contains `cwd: /tmp`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{Error, Result, sql};
use crate::model::{Block, Locator, Role, SessionSummary, SourceId, Transcript, Turn};
use crate::source::{RemoteLayout, Source, clean_title, parse_rfc3339, read_head, row_locator};
use crate::storage::sqlite;

const WORKSPACE_BYTES: usize = 8 * 1024;
const SESSION_QUERY: &str = "\
SELECT id, cwd, summary, created_at, updated_at
FROM sessions
ORDER BY updated_at DESC";
const TURN_COLUMNS: &str = "turn_index, user_message, assistant_response, timestamp";
const EVENT_COLUMNS: &str = "turn_index, event_type, tool_call_id, command, output, \
                             exit_code, event_key, event_value";

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

    /// The listing query runs on the host through `sqlite3 -json`; only its
    /// rows cross the wire, never the database.
    fn remote_layout(&self) -> RemoteLayout {
        RemoteLayout {
            files: &[],
            sql: Some((".copilot/session-store.db", &[SESSION_QUERY])),
        }
    }

    fn remote_open_sql(&self, id: &str) -> Vec<(&'static str, String)> {
        let id = sql_literal(id);
        vec![
            (
                "session",
                format!("SELECT summary FROM sessions WHERE id = {id}"),
            ),
            (
                "turns",
                format!(
                    "SELECT {TURN_COLUMNS} FROM turns WHERE session_id = {id} ORDER BY turn_index"
                ),
            ),
            (
                "events",
                format!(
                    "SELECT {EVENT_COLUMNS} FROM forge_trajectory_events \
                     WHERE session_id = {id} ORDER BY turn_index, id"
                ),
            ),
        ]
    }

    fn open_remote_rows(
        &self,
        db: &Path,
        id: &str,
        rows: &HashMap<String, Vec<Value>>,
    ) -> Result<Transcript> {
        let decode = |name: &str| rows.get(name).cloned().unwrap_or_default();
        let session = decode("session");
        let Some(first) = session.first() else {
            return Err(Error::MissingRow {
                db: db.to_path_buf(),
                id: id.to_owned(),
            });
        };
        let summary = first
            .get("summary")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let turns = decode("turns")
            .into_iter()
            .map(serde_json::from_value::<TurnRow>)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Error::Json)?;
        let mut events: BTreeMap<i64, Vec<Event>> = BTreeMap::new();
        for value in decode("events") {
            let row: EventRow = serde_json::from_value(value).map_err(Error::Json)?;
            events
                .entry(row.turn_index.unwrap_or(-1))
                .or_default()
                .push(row.event);
        }
        Ok(build_transcript(
            id,
            self.id(),
            summary.as_deref(),
            turns,
            events,
        ))
    }
}

/// A SQL string literal. The id is a row key from the listing, but it still
/// travels inside a shell script, so it is escaped rather than trusted.
fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
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
    let events = events_by_turn(&connection, db, id)?;
    let mut statement = connection
        .prepare(&format!(
            "SELECT {TURN_COLUMNS} FROM turns WHERE session_id = ?1 ORDER BY turn_index"
        ))
        .map_err(sql(db))?;
    let turns = statement
        .query_map(params![id], |row| {
            Ok(TurnRow {
                turn_index: row.get(0)?,
                user_message: row.get(1)?,
                assistant_response: row.get(2)?,
                timestamp: row.get(3)?,
            })
        })
        .map_err(sql(db))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sql(db))?;
    Ok(build_transcript(
        id,
        source,
        summary.as_deref(),
        turns,
        events,
    ))
}

/// One `turns` row, read locally through rusqlite or remotely from
/// `sqlite3 -json`; both paths meet in [`build_transcript`].
#[derive(Debug, Deserialize)]
struct TurnRow {
    turn_index: i64,
    user_message: Option<String>,
    assistant_response: Option<String>,
    timestamp: Option<String>,
}

/// One `forge_trajectory_events` row as `sqlite3 -json` names its columns.
#[derive(Debug, Deserialize)]
struct EventRow {
    turn_index: Option<i64>,
    #[serde(flatten)]
    event: Event,
}

fn build_transcript(
    id: &str,
    source: SourceId,
    summary: Option<&str>,
    turns: Vec<TurnRow>,
    mut events: BTreeMap<i64, Vec<Event>>,
) -> Transcript {
    let mut transcript = Transcript::new(id, source);
    transcript.title = summary.and_then(clean_title);

    for turn in turns {
        let TurnRow {
            turn_index: index,
            user_message,
            assistant_response,
            timestamp,
        } = turn;
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
    transcript
}

#[derive(Debug, Deserialize)]
struct Event {
    #[serde(rename = "event_type")]
    kind: String,
    tool_call_id: Option<String>,
    command: Option<String>,
    output: Option<String>,
    exit_code: Option<i64>,
    #[serde(rename = "event_key")]
    key: Option<String>,
    #[serde(rename = "event_value")]
    value: Option<String>,
}

fn events_by_turn(
    connection: &rusqlite::Connection,
    db: &Path,
    id: &str,
) -> Result<BTreeMap<i64, Vec<Event>>> {
    let mut statement = connection
        .prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM forge_trajectory_events
             WHERE session_id = ?1 ORDER BY turn_index, id"
        ))
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

    #[test]
    fn declares_the_sql_lane_for_remote_hosts() {
        let (_home, source) = fixture();
        let layout = source.remote_layout();
        assert!(layout.files.is_empty());
        let (db, queries) = layout.sql.expect("copilot lists over sqlite3");
        assert_eq!(db, ".copilot/session-store.db");
        assert_eq!(queries, &[SESSION_QUERY]);
    }

    #[test]
    fn remote_open_sql_escapes_the_id() {
        let (_home, source) = fixture();
        let queries = source.remote_open_sql("a'b");
        let names: Vec<_> = queries.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ["session", "turns", "events"]);
        assert!(queries.iter().all(|(_, query)| query.contains("'a''b'")));
    }

    /// Rows as `sqlite3 -json` would send them rebuild the same transcript
    /// the local rusqlite path produces.
    #[test]
    fn remote_rows_match_the_local_transcript() {
        let (_home, source) = fixture();
        let locator = Locator::Row {
            db: source.db.clone(),
            id: "s".into(),
        };
        let local = source.open(&locator).expect("open fixture");
        let rows: HashMap<String, Vec<Value>> = [
            ("session", json!([{"summary": " Title "}])),
            (
                "turns",
                json!([{"turn_index": 0, "user_message": "question",
                        "assistant_response": "answer",
                        "timestamp": "2026-01-02T03:04:07Z"}]),
            ),
            (
                "events",
                json!([{"turn_index": 0, "event_type": "shell", "tool_call_id": "call-1",
                        "command": "pwd", "output": "/from-state", "exit_code": 0,
                        "event_key": null, "event_value": null}]),
            ),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), serde_json::from_value(value).unwrap()))
        .collect();
        let remote = source
            .open_remote_rows(&source.db, "s", &rows)
            .expect("remote rows");
        assert_eq!(
            serde_json::to_value(&remote).unwrap(),
            serde_json::to_value(&local).unwrap()
        );
    }

    #[test]
    fn remote_rows_without_a_session_are_a_missing_row() {
        let (_home, source) = fixture();
        let result = source.open_remote_rows(&source.db, "gone", &HashMap::new());
        assert!(matches!(result, Err(Error::MissingRow { .. })));
    }
}
