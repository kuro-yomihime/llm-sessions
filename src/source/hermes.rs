//! Hermes: `~/.hermes/state.db`.
//!
//! One `SQLite` store holds everything, so tier 1 is a single statement and
//! never touches a message body. The inspected store is `schema_version = 22`
//! and records times as `REAL` unix seconds (`1788418400.879249`), unlike
//! every other database-backed source here, which count milliseconds.
//!
//! `sessions.cwd` is written verbatim, so [`crate::model::PathTrust::Recorded`] is exact and
//! there is no directory-name encoding to reverse.
//!
//! Accounting lives in `session_model_usage`, one row per model and task, and
//! the same totals are denormalised onto `sessions`. Tier 2 therefore attaches
//! the summed total to the last assistant turn rather than to each message:
//! Hermes records no per-message usage, and spreading a total would make
//! [`crate::model::Transcript::usage`] double count.
//!
//! Reading goes through `storage::sqlite::open`, which is read-only with an
//! `immutable=1` fallback, because the store is written while we browse.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::error::{Error, Result, sql};
use crate::model::{
    ArchiveOutcome, Block, Locator, Role, SessionSummary, SourceId, Transcript, Turn, Usage,
};
use crate::source::{RemoteLayout, Source, clean_title, row_locator, str_field};
use crate::storage::sqlite;

/// Tier-1 statement. The correlated `MAX(m.timestamp)` is covered by
/// `idx_messages_session(session_id, timestamp)`, so a row still costs an
/// index seek rather than a scan of the conversation.
const LIST_SQL: &str = "SELECT s.id, s.title, s.cwd, s.git_branch, s.model, s.parent_session_id, \
s.started_at, COALESCE(s.ended_at, s.started_at), \
s.input_tokens, s.output_tokens, \
COALESCE(s.actual_cost_usd, s.estimated_cost_usd, 0.0), \
s.archived, \
(SELECT MAX(m.timestamp) FROM messages m WHERE m.session_id = s.id) \
FROM sessions s";

/// Tier-2 statement. `active = 1 AND compacted = 0` keeps the live
/// conversation: Hermes leaves the pre-compaction messages in place, and
/// replaying them would show the user a history the model no longer has.
const MESSAGES_SQL: &str = "SELECT role, content, tool_calls, tool_name, tool_call_id, timestamp, \
reasoning_content, reasoning, finish_reason \
FROM messages \
WHERE session_id = ?1 AND active = 1 AND compacted = 0 \
ORDER BY id";

/// Per-session totals. `actual_cost_usd` is zero until a provider reports a
/// real price (`cost_status = 'unknown'` on the inspected rows), so the
/// estimate fills in rather than being discarded.
const USAGE_SQL: &str = "SELECT SUM(input_tokens), SUM(output_tokens), \
SUM(CASE WHEN actual_cost_usd != 0 THEN actual_cost_usd ELSE estimated_cost_usd END) \
FROM session_model_usage WHERE session_id = ?1";

/// Tier-1 SELECT for the remote shell transport, yielding exactly the eleven
/// columns [`crate::source::remote`] parses, in order. Column spellings mirror
/// [`LIST_SQL`] so the two cannot drift.
const REMOTE_LIST_SQL: &str = "SELECT id AS id, title AS title, cwd AS cwd, \
CAST(COALESCE(ended_at, started_at) AS INTEGER) AS updated_epoch, \
model AS model, input_tokens AS input, output_tokens AS output, \
COALESCE(actual_cost_usd, estimated_cost_usd) AS cost, \
parent_session_id AS parent, archived AS archived, git_branch AS branch \
FROM sessions";

#[derive(Debug)]
pub struct Hermes {
    db: PathBuf,
}

impl Hermes {
    pub fn new(home: &Path) -> Self {
        Self {
            db: home.join(".hermes/state.db"),
        }
    }
}

impl Source for Hermes {
    fn id(&self) -> SourceId {
        SourceId::Hermes
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.db.clone()]
    }

    fn list(&self) -> Result<Vec<SessionSummary>> {
        if !self.db.exists() {
            return Ok(Vec::new());
        }

        let (conn, _stale) = sqlite::open(&self.db)?;
        let mut statement = conn.prepare(LIST_SQL).map_err(sql(&self.db))?;
        let rows = statement
            .query_map([], |row| {
                let started: Option<f64> = row.get(6)?;
                let ended: Option<f64> = row.get(7)?;
                let last_message: Option<f64> = row.get(12)?;
                let input = u64::try_from(row.get::<_, Option<i64>>(8)?.unwrap_or(0)).unwrap_or(0);
                let output = u64::try_from(row.get::<_, Option<i64>>(9)?.unwrap_or(0)).unwrap_or(0);
                let cost = row
                    .get::<_, Option<f64>>(10)?
                    .filter(|cost| cost.is_finite())
                    .unwrap_or(0.0);
                let id: String = row.get(0)?;
                Ok(SessionSummary {
                    recorded_cwd: row.get::<_, Option<String>>(2)?.map(PathBuf::from),
                    title: row
                        .get::<_, Option<String>>(1)?
                        .as_deref()
                        .and_then(clean_title),
                    created: started.map(seconds),
                    model: row.get(4)?,
                    branch: row.get(3)?,
                    parent: row.get(5)?,
                    archived: row.get::<_, Option<i64>>(11)?.unwrap_or(0) != 0,
                    // A zero cost beside real tokens is a fact, not a gap:
                    // Hermes prices unpriced providers at zero.
                    usage: (input != 0 || output != 0 || cost != 0.0).then_some(Usage {
                        input,
                        output,
                        cost,
                    }),
                    ..SessionSummary::new(
                        id.clone(),
                        SourceId::Hermes,
                        latest(last_message, ended),
                        Locator::Row {
                            db: self.db.clone(),
                            id,
                        },
                    )
                })
            })
            .map_err(sql(&self.db))?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql(&self.db))
    }

    fn open(&self, locator: &Locator) -> Result<Transcript> {
        let (db, id) = row_locator(SourceId::Hermes, locator)?;
        let (conn, _stale) = sqlite::open(db)?;
        let (title, cwd, model): (Option<String>, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT title, cwd, model FROM sessions WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(sql(db))?
            .ok_or_else(|| Error::MissingRow {
                db: db.to_path_buf(),
                id: id.to_owned(),
            })?;

        let mut transcript = Transcript::new(id, SourceId::Hermes);
        transcript.title = title.as_deref().and_then(clean_title);
        transcript.workspace = cwd.map(PathBuf::from);

        let mut statement = conn.prepare(MESSAGES_SQL).map_err(sql(db))?;
        let rows = statement
            .query_map(params![id], |row| {
                Ok(Message {
                    role: row.get(0)?,
                    content: row.get(1)?,
                    tool_calls: row.get(2)?,
                    tool_name: row.get(3)?,
                    tool_call_id: row.get(4)?,
                    timestamp: row.get(5)?,
                    reasoning_content: row.get(6)?,
                    reasoning: row.get(7)?,
                    finish_reason: row.get(8)?,
                })
            })
            .map_err(sql(db))?;

        let mut unknown = 0;
        for row in rows {
            let turn = turn_from_message(&row.map_err(sql(db))?, model.as_deref(), &mut unknown);
            transcript.push(turn);
        }
        transcript.unknown_records = unknown;

        if let Some(usage) = session_usage(&conn, db, id)?
            && let Some(turn) = transcript
                .turns
                .iter_mut()
                .rev()
                .find(|turn| turn.role == Role::Assistant)
        {
            turn.usage = Some(usage);
        }

        Ok(transcript)
    }

    /// Hermes models archiving on `sessions.archived`, so the state belongs in
    /// its own store rather than in our overlay. A store that has gone away,
    /// or an id it does not hold, falls back to the overlay instead of failing
    /// the command.
    fn archive(&self, locator: &Locator, archived: bool) -> Result<ArchiveOutcome> {
        let (db, id) = row_locator(SourceId::Hermes, locator)?;
        if !db.exists() {
            return Ok(ArchiveOutcome::Unsupported);
        }
        let connection =
            Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_WRITE).map_err(sql(db))?;
        let changed = connection
            .execute(
                "UPDATE sessions SET archived = ?1 WHERE id = ?2",
                params![i64::from(archived), id],
            )
            .map_err(sql(db))?;
        drop(connection);
        if changed == 0 {
            return Ok(ArchiveOutcome::Unsupported);
        }
        Ok(ArchiveOutcome::Native)
    }

    /// Checked against `hermes --help` of Hermes Agent v0.21.1 (2026.9.7),
    /// installed at `~/.hermes/hermes-agent`: `--resume SESSION`.
    fn resume_command(&self, id: &str) -> Option<Vec<String>> {
        Some(vec!["hermes".into(), "--resume".into(), id.into()])
    }

    /// `chat --quiet --resume <id> --query=<msg>`, checked against the
    /// v0.21.1 source. `--quiet` (`-Q`) answers and exits instead of seeding
    /// an interactive session. Resuming loads the stored transcript as the
    /// conversation history (`cli_agent_setup_mixin.py`, following the
    /// compression chain to the session that holds the messages), and the
    /// quiet path runs `run_conversation(user_message, conversation_history)`
    /// (`cli.py`, `_run_quiet_single_query`): the message is appended to that
    /// conversation, not forked. A new row appears only if Hermes compresses
    /// context mid-turn, which continues the session in a child just as an
    /// interactive turn would. The `--query=` spelling keeps argparse from
    /// reading a message that starts with `-` as a flag.
    fn insert_command(&self, id: &str, message: &str) -> Option<Vec<String>> {
        Some(vec![
            "hermes".into(),
            "chat".into(),
            "--quiet".into(),
            "--resume".into(),
            id.into(),
            format!("--query={message}"),
        ])
    }

    fn remote_layout(&self) -> RemoteLayout {
        RemoteLayout {
            files: &[],
            sql: Some((".hermes/state.db", &[REMOTE_LIST_SQL])),
        }
    }
}

/// One `messages` row, exactly the columns [`MESSAGES_SQL`] selects.
struct Message {
    role: String,
    content: Option<String>,
    tool_calls: Option<String>,
    tool_name: Option<String>,
    tool_call_id: Option<String>,
    timestamp: Option<f64>,
    reasoning_content: Option<String>,
    reasoning: Option<String>,
    finish_reason: Option<String>,
}

impl Message {
    /// The row as JSON, for a role this adapter does not recognize. Every
    /// selected column is kept so a folded raw block loses nothing.
    fn raw(&self) -> Value {
        json!({
            "role": self.role,
            "content": self.content,
            "tool_calls": self.tool_calls,
            "tool_name": self.tool_name,
            "tool_call_id": self.tool_call_id,
            "timestamp": self.timestamp,
            "reasoning_content": self.reasoning_content,
            "reasoning": self.reasoning,
            "finish_reason": self.finish_reason,
        })
    }
}

fn turn_from_message(message: &Message, session_model: Option<&str>, unknown: &mut usize) -> Turn {
    let role = match message.role.as_str() {
        "user" => Some(Role::User),
        "assistant" => Some(Role::Assistant),
        "system" => Some(Role::System),
        "tool" => Some(Role::Tool),
        _ => None,
    };
    let mut turn = Turn::new(role.unwrap_or(Role::System));
    turn.timestamp = message.timestamp.map(seconds);
    if role == Some(Role::Assistant) {
        // Hermes records no per-message model; the session's is what the
        // reader's speaker line shows.
        turn.model = session_model.map(str::to_owned);
    }

    if role.is_none() {
        *unknown += 1;
        turn.blocks.push(Block::Unknown { raw: message.raw() });
        return turn;
    }

    if let Some(text) = message
        .reasoning_content
        .as_deref()
        .or(message.reasoning.as_deref())
        .and_then(non_empty)
    {
        turn.blocks.push(Block::thinking(text));
    }

    if role == Some(Role::Tool) {
        turn.blocks.push(Block::ToolResult {
            name: message.tool_name.clone(),
            ok: message.finish_reason.as_deref() != Some("error"),
            output: message.content.clone().unwrap_or_default(),
        });
    } else if let Some(text) = message.content.as_deref().and_then(non_empty) {
        turn.blocks.push(Block::text(text));
    }

    if let Some(raw) = message.tool_calls.as_deref() {
        append_tool_calls(&mut turn, raw, unknown);
    }

    turn
}

/// unverified — confirm first: the inspected store contains no tool rows, so
/// both spellings a Hermes provider could write are accepted — `OpenAI`'s
/// `function.name`/`function.arguments` and a flat `name`/`input` — and
/// anything else survives as a folded raw block rather than failing the
/// session.
fn append_tool_calls(turn: &mut Turn, raw: &str, unknown: &mut usize) {
    let calls = serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|value| match value {
            Value::Array(items) => Some(items),
            _ => None,
        });
    let Some(calls) = calls else {
        *unknown += 1;
        turn.blocks.push(Block::Unknown {
            raw: Value::String(raw.to_owned()),
        });
        return;
    };
    for call in calls {
        if let Some(block) = tool_call(&call) {
            turn.blocks.push(block);
        } else {
            *unknown += 1;
            turn.blocks.push(Block::Unknown { raw: call });
        }
    }
}

fn tool_call(call: &Value) -> Option<Block> {
    if let Some(function) = call.get("function") {
        return Some(Block::ToolCall {
            name: str_field(function, "name")?,
            input: tool_input(function.get("arguments")),
        });
    }
    let name = str_field(call, "name")?;
    let arguments = call.get("input").or_else(|| call.get("arguments"))?;
    Some(Block::ToolCall {
        name,
        input: tool_input(Some(arguments)),
    })
}

/// An OpenAI-style `arguments` is itself a JSON document inside a string. A
/// payload that will not parse is kept verbatim rather than dropped.
fn tool_input(value: Option<&Value>) -> Value {
    match value {
        Some(Value::String(raw)) => {
            serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.clone()))
        }
        Some(other) => other.clone(),
        None => Value::Null,
    }
}

/// Summed per-model accounting, or `None` when the session has no rows —
/// which is "not recorded", not "nothing spent".
fn session_usage(conn: &Connection, db: &Path, id: &str) -> Result<Option<Usage>> {
    let row: (Option<i64>, Option<i64>, Option<f64>) = conn
        .query_row(USAGE_SQL, params![id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .optional()
        .map_err(sql(db))?
        .unwrap_or((None, None, None));
    let (Some(input), output, cost) = row else {
        return Ok(None);
    };
    Ok(Some(Usage {
        input: u64::try_from(input).unwrap_or(0),
        output: u64::try_from(output.unwrap_or(0)).unwrap_or(0),
        cost: cost.filter(|cost| cost.is_finite()).unwrap_or(0.0),
    }))
}

/// Hermes writes `REAL` unix seconds. An out-of-range value would otherwise
/// fail a whole session for one bad row.
fn seconds(value: f64) -> OffsetDateTime {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "float-to-int `as` saturates (NaN -> 0); out-of-range results are rejected below"
    )]
    let nanos = (value * 1_000_000_000.0) as i128;
    OffsetDateTime::from_unix_timestamp_nanos(nanos).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// A session is as recent as its newest message, which outlives `ended_at` on
/// a session that is still running.
fn latest(last_message: Option<f64>, ended: Option<f64>) -> OffsetDateTime {
    let newest = match (last_message, ended) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (value, None) | (None, value) => value,
    };
    newest.map_or(OffsetDateTime::UNIX_EPOCH, seconds)
}

fn non_empty(text: &str) -> Option<&str> {
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PathTrust;

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().to_path_buf();
        let db = home.join(".hermes/state.db");
        std::fs::create_dir_all(db.parent().expect("parent")).expect("create");
        let conn = Connection::open(&db).expect("fixture database");
        conn.execute_batch(
            "CREATE TABLE sessions(
                id TEXT PRIMARY KEY, source TEXT, model TEXT, parent_session_id TEXT,
                started_at REAL, ended_at REAL, input_tokens INTEGER, output_tokens INTEGER,
                estimated_cost_usd REAL, actual_cost_usd REAL, title TEXT, cwd TEXT,
                git_branch TEXT, git_repo_root TEXT, message_count INTEGER,
                archived INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE messages(
                id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT, role TEXT,
                content TEXT, tool_call_id TEXT, tool_calls TEXT, tool_name TEXT,
                timestamp REAL, finish_reason TEXT, reasoning TEXT, reasoning_content TEXT,
                active INTEGER NOT NULL DEFAULT 1, compacted INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE session_model_usage(
                session_id TEXT, model TEXT, task TEXT, input_tokens INTEGER,
                output_tokens INTEGER, reasoning_tokens INTEGER,
                estimated_cost_usd REAL, actual_cost_usd REAL,
                first_seen REAL, last_seen REAL
             );
             INSERT INTO sessions(id, source, model, started_at, ended_at, input_tokens,
                output_tokens, estimated_cost_usd, actual_cost_usd, title, cwd, git_branch,
                archived)
             VALUES ('s1', 'cli', 'glm-5', 1700000000.5, 1700000100.25, 40, 9,
                0.5, NULL, ' Fix  the thing ', '/work/thing', 'main', 0);
             INSERT INTO messages(session_id, role, content, timestamp)
             VALUES ('s1', 'user', 'hello', 1700000001.0);
             INSERT INTO messages(session_id, role, content, reasoning_content, timestamp)
             VALUES ('s1', 'assistant', 'answer', 'because', 1700000002.0);
             INSERT INTO messages(session_id, role, content, timestamp, compacted)
             VALUES ('s1', 'user', 'superseded by a compaction', 1700000003.0, 1);
             INSERT INTO session_model_usage(session_id, model, task, input_tokens,
                output_tokens, estimated_cost_usd, actual_cost_usd)
             VALUES ('s1', 'glm-5', '', 40, 9, 0.5, 0.0);",
        )
        .expect("fixture schema");
        drop(conn);
        (dir, home)
    }

    #[test]
    fn a_session_reports_its_cwd_branch_and_usage() {
        let (_dir, home) = fixture();
        let sessions = Hermes::new(&home).list().expect("list");

        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.recorded_cwd, Some(PathBuf::from("/work/thing")));
        assert_eq!(session.trust, PathTrust::Recorded);
        assert_eq!(session.branch.as_deref(), Some("main"));
        assert_eq!(session.title.as_deref(), Some("Fix the thing"));
        assert_eq!(
            session.usage,
            Some(Usage {
                input: 40,
                output: 9,
                cost: 0.5,
            })
        );
        // The last message is newer than `ended_at`, and is what ordering uses.
        assert_eq!(session.updated, seconds(1_700_000_100.25));
        assert_eq!(session.created, Some(seconds(1_700_000_000.5)));
        assert!(!session.archived);
        assert_eq!(session.origin_label, None);
    }

    #[test]
    fn a_missing_store_lists_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(Hermes::new(dir.path()).list().expect("list").is_empty());
    }

    #[test]
    fn a_compacted_message_is_not_replayed() {
        let (_dir, home) = fixture();
        let source = Hermes::new(&home);
        let sessions = source.list().expect("list");
        let transcript = source.open(&sessions[0].locator).expect("open");

        assert_eq!(transcript.turns.len(), 2, "the compacted user turn is gone");
        assert!(
            !transcript.searchable_text().contains("superseded"),
            "a compacted message must not reappear"
        );
        assert_eq!(transcript.turns[1].role, Role::Assistant);
        assert_eq!(transcript.turns[1].model.as_deref(), Some("glm-5"));
        assert!(matches!(
            transcript.turns[1].blocks.as_slice(),
            [Block::Thinking { .. }, Block::Text { .. }]
        ));
        // The per-model totals land once, so the body agrees with tier 1.
        assert_eq!(
            transcript.usage(),
            Some(Usage {
                input: 40,
                output: 9,
                cost: 0.5,
            })
        );
    }

    #[test]
    fn archiving_flips_the_native_column() {
        let (_dir, home) = fixture();
        let source = Hermes::new(&home);
        let locator = source.list().expect("list").remove(0).locator;

        assert_eq!(
            source.archive(&locator, true).expect("archive"),
            ArchiveOutcome::Native
        );
        assert!(source.list().expect("list")[0].archived);

        assert_eq!(
            source.archive(&locator, false).expect("unarchive"),
            ArchiveOutcome::Native
        );
        assert!(!source.list().expect("list")[0].archived);
    }

    #[test]
    fn an_unrecognised_role_is_kept_raw_and_counted() {
        let (_dir, home) = fixture();
        let db = home.join(".hermes/state.db");
        Connection::open(&db)
            .expect("fixture database")
            .execute_batch(
                "INSERT INTO messages(session_id, role, content, timestamp)
                 VALUES ('s1', 'from_the_future', 'payload', 1700000004.0);",
            )
            .expect("extra row");

        let source = Hermes::new(&home);
        let sessions = source.list().expect("list");
        let transcript = source.open(&sessions[0].locator).expect("open");

        assert_eq!(transcript.unknown_records, 1);
        let last = transcript.turns.last().expect("turn");
        assert_eq!(last.role, Role::System);
        assert!(matches!(last.blocks[0], Block::Unknown { .. }));
    }

    #[test]
    fn both_tool_call_shapes_become_tool_blocks() {
        let mut turn = Turn::new(Role::Assistant);
        let mut unknown = 0;
        append_tool_calls(
            &mut turn,
            r#"[{"function":{"name":"shell","arguments":"{\"command\":\"ls\"}"}},
                {"name":"read","input":{"path":"/tmp/x"}},
                {"shape":"unheard of"}]"#,
            &mut unknown,
        );

        assert_eq!(unknown, 1, "only the third element is unrecognised");
        assert!(matches!(
            &turn.blocks[0],
            Block::ToolCall { name, input }
                if name == "shell" && input["command"] == "ls"
        ));
        assert!(matches!(
            &turn.blocks[1],
            Block::ToolCall { name, input }
                if name == "read" && input["path"] == "/tmp/x"
        ));
        assert!(matches!(turn.blocks[2], Block::Unknown { .. }));
    }

    #[test]
    fn a_tool_row_carries_its_outcome() {
        let mut unknown = 0;
        let failed = turn_from_message(
            &Message {
                role: "tool".into(),
                content: Some("boom".into()),
                tool_calls: None,
                tool_name: Some("shell".into()),
                tool_call_id: Some("call_1".into()),
                timestamp: Some(1_700_000_005.0),
                reasoning_content: None,
                reasoning: None,
                finish_reason: Some("error".into()),
            },
            Some("glm-5"),
            &mut unknown,
        );

        assert_eq!(unknown, 0);
        assert_eq!(failed.role, Role::Tool);
        assert_eq!(failed.model, None, "a tool row is not a model's turn");
        assert!(matches!(
            &failed.blocks[0],
            Block::ToolResult { name, ok: false, output }
                if name.as_deref() == Some("shell") && output == "boom"
        ));
    }
}
