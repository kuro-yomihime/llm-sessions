//! `OpenClaw`: two generations of store, both read.
//!
//! **`SQLite` (current, `2026.8` migration).** Each agent keeps its sessions in
//! `~/.openclaw/agents/<agentId>/agent/openclaw-agent.sqlite`, in four tables:
//! `session_nodes` (one row per session key, with the old `sessions.json`
//! entry verbatim in `entry_json`), `session_windows` (one row per session id;
//! a key rolls to a new id on reset), `transcript_events` (one JSON event per
//! `(session_id, seq)`; seq 0 is the `session` header whose `cwd` is
//! authoritative) and `session_transcript_active_events` (the live branch —
//! `open` prefers it and falls back to every event in seq order).
//!
//! **JSONL (legacy).** `~/.openclaw/agents/<agentId>/sessions/<id>.jsonl`
//! plus the flat `~/.openclaw/sessions/`, indexed by `sessions.json`. A
//! migrated store renames those files away and does not list them; a session
//! in both generations is listed once, from `SQLite`.
//!
//! Both carry the same event shape (`{"type":"message","message":{…}}` with
//! `text`, `thinking`, `toolCall` blocks and per-message `usage`); the
//! earlier documented shape (`tool_use`, `tool_result`) is still accepted.
//!
//! Credentials (`auth*.json`, `*.lock.sqlite`, `generation-*`) are never
//! opened — only `.jsonl` and `openclaw-agent.sqlite`, the latter read-only
//! through the crate's read-only `SQLite` opener (`storage::sqlite::open`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, params};
use serde_json::Value;

use crate::error::{Error, Result, sql};
use crate::model::{
    Block, Locator, PathTrust, Role, SessionSummary, SourceId, Transcript, Turn, Usage,
};
use crate::source::{
    RemoteLayout, Source, clean_title, from_unix_millis, mtime, parse_rfc3339, push_unknown,
    read_records, str_field,
};
use crate::storage::sqlite;

/// Per-agent database file name, under `agents/<id>/agent/`.
const AGENT_DB: &str = "openclaw-agent.sqlite";

/// Tier-1 statement: one row per session window that has a transcript.
///
/// The node join matches only the key's *current* window: `entry_json`
/// describes that window alone, so an older window must not inherit its
/// token counts.
const LIST_SQL: &str = "SELECT w.session_id, \
COALESCE(w.display_name, cur.label, key.label), \
w.model, w.created_at, w.started_at, \
MAX(w.updated_at, COALESCE(w.transcript_updated_at, 0)), \
json_extract(h.event_json, '$.cwd'), \
json_extract(cur.entry_json, '$.systemPromptReport.workspaceDir'), \
json_extract(cur.entry_json, '$.inputTokens'), \
json_extract(cur.entry_json, '$.outputTokens'), \
json_extract(cur.entry_json, '$.estimatedCostUsd'), \
key.archived_at, w.session_key, \
(SELECT p.current_session_id FROM session_nodes p \
 WHERE p.session_key = COALESCE(w.parent_session_key, key.parent_session_key)) \
FROM session_windows w \
LEFT JOIN session_nodes key ON key.session_key = w.session_key \
LEFT JOIN session_nodes cur ON cur.session_key = w.session_key \
 AND cur.current_session_id = w.session_id \
LEFT JOIN transcript_events h ON h.session_id = w.session_id AND h.seq = 0 \
WHERE EXISTS (SELECT 1 FROM transcript_events e WHERE e.session_id = w.session_id)";

/// Tier-2, live branch: the active path in order.
const ACTIVE_SQL: &str = "SELECT e.event_json FROM session_transcript_active_events a \
JOIN transcript_events e ON e.session_id = a.session_id AND e.seq = a.event_seq \
WHERE a.session_id = ?1 ORDER BY a.active_position";

/// Tier-2, fallback when no active path is indexed: every event in order.
const EVENTS_SQL: &str =
    "SELECT event_json FROM transcript_events WHERE session_id = ?1 ORDER BY seq";

#[derive(Debug)]
pub struct OpenClaw {
    agents: PathBuf,
    legacy: PathBuf,
}

impl OpenClaw {
    pub fn new(home: &Path) -> Self {
        Self {
            agents: home.join(".openclaw/agents"),
            legacy: home.join(".openclaw/sessions"),
        }
    }

    fn agent_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&self.agents)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                    .map(|entry| entry.path())
                    .collect()
            })
            .unwrap_or_default();
        dirs.sort();
        dirs
    }

    /// One `sessions.json` index per directory, read at most once per
    /// `list()` and keyed by `sessionId`; entries without a transcript are
    /// ignored, so a stale entry costs nothing.
    fn index(dir: &Path) -> HashMap<String, (String, Value)> {
        let Ok(raw) = std::fs::read_to_string(dir.join("sessions.json")) else {
            return HashMap::new();
        };
        let Ok(Value::Object(entries)) = serde_json::from_str(&raw) else {
            return HashMap::new();
        };
        let mut by_session_id = HashMap::new();
        for (key, entry) in entries {
            if let Some(id) = entry.get("sessionId").and_then(Value::as_str) {
                by_session_id.insert(id.to_owned(), (key, entry));
            }
        }
        by_session_id
    }

    fn list_db(db: &Path, out: &mut Vec<SessionSummary>) -> Result<()> {
        let (conn, _stale) = sqlite::open(db)?;
        // A pre-migration (or foreign) database has no transcript tables.
        let has_tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name IN \
                 ('session_windows', 'session_nodes', 'transcript_events')",
                [],
                |row| row.get(0),
            )
            .map_err(sql(db))?;
        if has_tables < 3 {
            return Ok(());
        }
        let mut statement = conn.prepare(LIST_SQL).map_err(sql(db))?;
        let rows = statement
            .query_map([], |row| {
                let id: String = row.get(0)?;
                let created: Option<i64> = row.get(3)?;
                let started: Option<i64> = row.get(4)?;
                let updated: Option<i64> = row.get(5)?;
                let cwd: Option<String> = row.get::<_, Option<String>>(6)?;
                let workspace_dir: Option<String> = row.get(7)?;
                let input: Option<i64> = row.get(8)?;
                let output: Option<i64> = row.get(9)?;
                let cost: Option<f64> = row.get(10)?;
                let key: String = row.get(12)?;
                let parent: Option<String> = row.get(13)?;
                let recorded_cwd = cwd.or(workspace_dir).filter(|c| !c.is_empty());
                Ok(SessionSummary {
                    title: row
                        .get::<_, Option<String>>(1)?
                        .as_deref()
                        .and_then(clean_title),
                    model: row.get(2)?,
                    created: started.or(created).and_then(from_unix_millis),
                    trust: PathTrust::Recorded,
                    recorded_cwd: recorded_cwd.map(PathBuf::from),
                    usage: match (input, output) {
                        (Some(input), Some(output)) => Some(Usage {
                            input: u64::try_from(input).unwrap_or(0),
                            output: u64::try_from(output).unwrap_or(0),
                            cost: cost.filter(|cost| cost.is_finite()).unwrap_or(0.0),
                        }),
                        _ => None,
                    },
                    archived: row.get::<_, Option<i64>>(11)?.is_some(),
                    subagent: key.contains(":subagent:"),
                    parent: parent.filter(|parent| parent != &id),
                    ..SessionSummary::new(
                        id.clone(),
                        SourceId::OpenClaw,
                        updated
                            .or(created)
                            .and_then(from_unix_millis)
                            .unwrap_or(time::OffsetDateTime::UNIX_EPOCH),
                        Locator::Row {
                            db: db.to_path_buf(),
                            id,
                        },
                    )
                })
            })
            .map_err(sql(db))?;
        for row in rows {
            let row = row.map_err(sql(db))?;
            if !out.iter().any(|seen| seen.id == row.id) {
                out.push(row);
            }
        }
        Ok(())
    }

    fn list_jsonl(sessions_dir: &Path, out: &mut Vec<SessionSummary>) -> Result<()> {
        let index = Self::index(sessions_dir);
        let Ok(files) = std::fs::read_dir(sessions_dir) else {
            return Ok(());
        };
        for entry in files.flatten() {
            let path = entry.path();
            // `<id>.jsonl` only: `.trajectory` is telemetry, `.deleted.*` /
            // `.reset.*` are retired copies.
            if !entry.file_type().is_ok_and(|kind| kind.is_file())
                || path
                    .extension()
                    .is_none_or(|extension| extension != "jsonl")
            {
                continue;
            }
            let id = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            if id.is_empty() || id.ends_with(".trajectory") || out.iter().any(|row| row.id == id) {
                continue;
            }
            let (updated, size) = mtime(&path)?;
            let indexed = index.get(&id);
            let entry = indexed.map(|(_, entry)| entry);
            // Order by the transcript's own mtime; the index's `updatedAt` is
            // a cross-check only.
            out.push(SessionSummary {
                title: entry
                    .and_then(|entry| entry.get("label"))
                    .and_then(Value::as_str)
                    .and_then(clean_title),
                model: entry
                    .and_then(|entry| entry.get("model"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                usage: entry.and_then(tokens).map(|(input, output)| Usage {
                    input,
                    output,
                    cost: entry
                        .and_then(|entry| entry.get("estimatedCostUsd"))
                        .and_then(Value::as_f64)
                        .filter(|cost| cost.is_finite())
                        .unwrap_or(0.0),
                }),
                recorded_cwd: entry
                    .and_then(|entry| entry.pointer("/systemPromptReport/workspaceDir"))
                    .and_then(Value::as_str)
                    .filter(|dir| !dir.is_empty())
                    .map(PathBuf::from),
                subagent: indexed.is_some_and(|(key, _)| key.contains(":subagent:")),
                size_bytes: Some(size),
                ..SessionSummary::new(
                    id.clone(),
                    SourceId::OpenClaw,
                    updated,
                    Locator::File { path },
                )
            });
        }
        Ok(())
    }

    fn open_db(db: &Path, id: &str) -> Result<Transcript> {
        let (conn, _stale) = sqlite::open(db)?;
        let (title, header): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT COALESCE(w.display_name, n.label), \
                 (SELECT event_json FROM transcript_events WHERE session_id = w.session_id AND seq = 0) \
                 FROM session_windows w LEFT JOIN session_nodes n ON n.session_key = w.session_key \
                 WHERE w.session_id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sql(db))?
            .ok_or_else(|| Error::MissingRow {
                db: db.to_path_buf(),
                id: id.to_owned(),
            })?;
        let mut transcript = Transcript::new(id, SourceId::OpenClaw);
        transcript.title = title.as_deref().and_then(clean_title);
        transcript.workspace = header
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .and_then(|header| str_field(&header, "cwd"))
            .map(PathBuf::from);

        let mut events = Vec::new();
        for query in [ACTIVE_SQL, EVENTS_SQL] {
            let mut statement = conn.prepare(query).map_err(sql(db))?;
            events = statement
                .query_map(params![id], |row| row.get::<_, String>(0))
                .map_err(sql(db))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(sql(db))?;
            if !events.is_empty() {
                break;
            }
        }
        for raw in events {
            let record = serde_json::from_str::<Value>(&raw).unwrap_or(Value::String(raw));
            push_record(&mut transcript, record);
        }
        Ok(transcript)
    }
}

impl Source for OpenClaw {
    fn id(&self) -> SourceId {
        SourceId::OpenClaw
    }

    fn roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.agents.clone(), self.legacy.clone()];
        roots.extend(
            self.agent_dirs()
                .into_iter()
                .map(|dir| dir.join("agent").join(AGENT_DB))
                .filter(|db| db.is_file()),
        );
        roots
    }

    fn list(&self) -> Result<Vec<SessionSummary>> {
        let mut out: Vec<SessionSummary> = Vec::new();
        let agent_dirs = self.agent_dirs();
        // SQLite first: a migrated session's id wins over any JSONL copy.
        for dir in &agent_dirs {
            let db = dir.join("agent").join(AGENT_DB);
            if db.is_file() {
                Self::list_db(&db, &mut out)?;
            }
        }
        // JSONL next: per-agent ids win over a legacy copy of the same chat.
        for sessions_dir in agent_dirs
            .iter()
            .map(|dir| dir.join("sessions"))
            .chain(std::iter::once(self.legacy.clone()))
        {
            Self::list_jsonl(&sessions_dir, &mut out)?;
        }
        Ok(out)
    }

    fn open(&self, locator: &Locator) -> Result<Transcript> {
        let path = match locator {
            Locator::Row { db, id } => return Self::open_db(db, id),
            Locator::File { path } => path,
        };
        let id = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut transcript = Transcript::new(id, SourceId::OpenClaw);
        for record in read_records(path)? {
            push_record(&mut transcript, record);
        }
        Ok(transcript)
    }

    /// unverified — confirm first: `OpenClaw` sessions are chat-bound and the
    /// documentation names no CLI flag that resumes one from a terminal.
    fn resume_command(&self, _id: &str) -> Option<Vec<String>> {
        None
    }

    /// JSONL only: the shell transport takes one database path, and
    /// `OpenClaw` keeps one per agent.
    fn remote_layout(&self) -> RemoteLayout {
        RemoteLayout {
            files: &[
                ".openclaw/agents/*/sessions/*.jsonl",
                ".openclaw/sessions/*.jsonl",
            ],
            sql: None,
        }
    }
}

/// Session-state events (`model-snapshot` bookkeeping, tree cursor, …) that
/// are skipped rather than counted unknown.
const STATE_RECORDS: &[&str] = &[
    "session",
    "model_change",
    "thinking_level_change",
    "custom",
    "leaf",
    "label",
];

/// One transcript event, from either generation, into the transcript.
fn push_record(transcript: &mut Transcript, record: Value) {
    let kind = record
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match kind.as_deref() {
        Some("session") => {
            if transcript.workspace.is_none() {
                transcript.workspace = str_field(&record, "cwd").map(PathBuf::from);
            }
        }
        Some(state) if STATE_RECORDS.contains(&state) => {}
        // A `/new` or `/reset` boundary inside the window: shown, not raw.
        Some("reset") => {
            let mut turn = Turn::new(Role::System);
            turn.timestamp = timestamp(&record, None);
            let reason = str_field(&record, "reason").unwrap_or_default();
            turn.blocks
                .push(Block::text(format!("session reset ({reason})")));
            transcript.push(turn);
        }
        Some("custom_message") => {
            if record.get("display").and_then(Value::as_bool) == Some(true) {
                let mut turn = Turn::new(Role::System);
                turn.timestamp = timestamp(&record, None);
                if let Some(text) = str_field(&record, "content") {
                    turn.blocks.push(Block::text(text));
                }
                transcript.push(turn);
            }
        }
        Some("message") | None if record.get("message").is_some() => {
            message_turn(transcript, record);
        }
        // Kept raw and counted: a format shift must not fail the session.
        _ => push_unknown(transcript, record),
    }
}

fn timestamp(record: &Value, message: Option<&Value>) -> Option<time::OffsetDateTime> {
    record
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339)
        .or_else(|| {
            message
                .and_then(|message| message.get("timestamp"))
                .and_then(Value::as_i64)
                .and_then(from_unix_millis)
        })
}

fn message_turn(transcript: &mut Transcript, record: Value) {
    let Some(message) = record.get("message") else {
        return push_unknown(transcript, record);
    };
    let role_name = message.get("role").and_then(Value::as_str).unwrap_or("");
    let mut turn = Turn::new(match role_name {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "system" => Role::System,
        "tool" | "toolResult" => Role::Tool,
        _ => return push_unknown(transcript, record),
    });
    turn.timestamp = timestamp(&record, Some(message));
    turn.model = str_field(message, "model");
    if turn.role == Role::Assistant {
        turn.usage = message.get("usage").and_then(message_usage);
    }
    // A tool result is one whole message: outcome on the envelope, payload in `content`.
    if role_name == "toolResult" {
        turn.blocks.push(Block::ToolResult {
            name: str_field(message, "toolName"),
            ok: message.get("isError").and_then(Value::as_bool) != Some(true),
            output: flatten_text(message.get("content")),
        });
        transcript.push(turn);
        return;
    }
    match message.get("content") {
        Some(Value::String(text)) => turn.blocks.push(Block::text(text)),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                append_block(&mut turn, &mut transcript.unknown_records, block);
            }
        }
        _ => {}
    }
    transcript.push(turn);
}

/// Per-message usage on assistant envelopes: `input`/`output` plus
/// `cost.total`. Cache fields have no slot in [`Usage`].
fn message_usage(value: &Value) -> Option<Usage> {
    let count = |name: &str| value.get(name).and_then(Value::as_u64);
    let cost = value
        .pointer("/cost/total")
        .and_then(Value::as_f64)
        .filter(|cost| cost.is_finite());
    if count("input").is_none() && count("output").is_none() && cost.is_none() {
        return None;
    }
    Some(Usage {
        input: count("input").unwrap_or(0),
        output: count("output").unwrap_or(0),
        cost: cost.unwrap_or(0.0),
    })
}

fn tokens(entry: &Value) -> Option<(u64, u64)> {
    let field = |name: &str| {
        let value = entry.get(name)?;
        // A negative count is corrupt accounting and clamps to zero.
        value.as_u64().or_else(|| value.as_i64().map(|_| 0))
    };
    match (field("inputTokens"), field("outputTokens")) {
        (Some(input), Some(output)) => Some((input, output)),
        _ => None,
    }
}

fn append_block(turn: &mut Turn, unknown_records: &mut usize, block: &Value) {
    let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
    let text = block.get("text").and_then(Value::as_str).unwrap_or("");
    match kind {
        "text" => turn.blocks.push(Block::text(text)),
        // The live store names the field `thinking`; the documented one `text`.
        "thinking" | "reasoning" => turn.blocks.push(Block::thinking(
            block
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or(text),
        )),
        "tool_use" | "toolCall" => turn.blocks.push(Block::ToolCall {
            name: block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_owned(),
            input: block
                .get("arguments")
                .or_else(|| block.get("input"))
                .cloned()
                .unwrap_or(Value::Null),
        }),
        "tool_result" => turn.blocks.push(Block::ToolResult {
            name: block.get("name").and_then(Value::as_str).map(str::to_owned),
            ok: !block
                .get("is_error")
                .is_some_and(|flag| flag == &Value::Bool(true)),
            output: flatten_text(block.get("content")),
        }),
        "image" => turn.blocks.push(Block::Image {
            label: block
                .get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or(text)
                .to_owned(),
        }),
        _ => {
            *unknown_records += 1;
            turn.blocks.push(Block::Unknown { raw: block.clone() });
        }
    }
}

/// Nested content arrays flattened to displayable text, the way OMP's reader
/// does it: every string in the tree, in order.
fn flatten_text(content: Option<&Value>) -> String {
    fn walk(value: &Value, out: &mut String) {
        match value {
            Value::String(text) => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
            Value::Array(items) => items.iter().for_each(|item| walk(item, out)),
            Value::Object(fields) => {
                if let Some(text) = fields.get("text").and_then(Value::as_str) {
                    walk(&Value::String(text.to_owned()), out);
                }
            }
            _ => {}
        }
    }
    let mut out = String::new();
    walk(content.unwrap_or(&Value::Null), &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Block;

    fn store(dir: &Path) -> OpenClaw {
        OpenClaw::new(dir)
    }

    fn transcript_dir(dir: &Path) -> PathBuf {
        let sessions = dir.join(".openclaw/agents/main/sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        sessions
    }

    fn write(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn a_label_from_the_index_titles_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = transcript_dir(dir.path());
        write(
            &sessions.join("11111111-1111-4111-8111-111111111111.jsonl"),
            "{\"timestamp\":\"2026-02-19T10:00:00.000Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
        );
        write(
            &sessions.join("sessions.json"),
            "{\"chat:main\":{\"sessionId\":\"11111111-1111-4111-8111-111111111111\",\"label\":\"Fix the login flow\",\"model\":\"glm-5\",\"inputTokens\":12,\"outputTokens\":34,\"updatedAt\":1770000000000}}",
        );

        let listed = store(dir.path()).list().unwrap();
        assert_eq!(listed.len(), 1);
        let row = &listed[0];
        assert_eq!(row.id, "11111111-1111-4111-8111-111111111111");
        assert_eq!(row.title.as_deref(), Some("Fix the login flow"));
        assert_eq!(row.model.as_deref(), Some("glm-5"));
        assert_eq!(
            row.usage,
            Some(Usage {
                input: 12,
                output: 34,
                cost: 0.0
            })
        );
        assert_eq!(row.recorded_cwd, None, "per-chat sessions claim no cwd");
    }

    #[test]
    fn string_and_block_content_both_become_text() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = transcript_dir(dir.path());
        let path = sessions.join("22222222-2222-4222-8222-222222222222.jsonl");
        write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-02-19T10:00:00.000Z\",\"message\":{\"role\":\"user\",\"content\":\"plain string\"}}\n",
                "{\"timestamp\":\"2026-02-19T10:00:01.000Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"blocked\"}]}}\n",
            ),
        );

        let transcript = store(dir.path()).open(&Locator::File { path }).unwrap();
        assert_eq!(transcript.turns.len(), 2);
        assert!(matches!(
            &transcript.turns[0].blocks[0],
            Block::Text { text } if text == "plain string"
        ));
        assert!(matches!(
            &transcript.turns[1].blocks[0],
            Block::Text { text } if text == "blocked"
        ));
    }

    #[test]
    fn an_unparseable_line_is_kept_raw_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = transcript_dir(dir.path());
        let path = sessions.join("33333333-3333-4333-8333-333333333333.jsonl");
        write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-02-19T10:00:00.000Z\",\"message\":{\"role\":\"user\",\"content\":\"real\"}}\n",
                "{\"timestamp\":\"2026-02-19T10:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":\"line with a stray control character \u{7}\"}}\n",
            ),
        );

        let transcript = store(dir.path()).open(&Locator::File { path }).unwrap();
        assert!(
            transcript.unknown_records >= 1,
            "a format shift must be visible, not silent"
        );
        assert!(
            transcript
                .turns
                .iter()
                .flat_map(|turn| &turn.blocks)
                .any(|block| matches!(block, Block::Unknown { .. })),
            "and kept as raw JSON"
        );
    }

    #[test]
    fn the_per_agent_copy_wins_over_a_legacy_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = transcript_dir(dir.path());
        let legacy = dir.path().join(".openclaw/sessions");
        std::fs::create_dir_all(&legacy).unwrap();
        let id = "44444444-4444-4444-8444-444444444444";
        write(
            &sessions.join(format!("{id}.jsonl")),
            "{\"timestamp\":\"2026-02-19T10:00:00.000Z\",\"message\":{\"role\":\"user\",\"content\":\"new\"}}\n",
        );
        write(
            &legacy.join(format!("{id}.jsonl")),
            "{\"timestamp\":\"2025-01-01T00:00:00.000Z\",\"message\":{\"role\":\"user\",\"content\":\"old\"}}\n",
        );

        let listed = store(dir.path()).list().unwrap();
        assert_eq!(listed.len(), 1, "one chat, one row");
    }

    #[test]
    fn an_absent_store_is_silent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(store(dir.path()).list().unwrap().is_empty());
    }

    // Real columns and event shapes from `2026.8.1`, content redacted.
    const LIVE_SCHEMA: &str = "
        CREATE TABLE session_nodes (session_key TEXT PRIMARY KEY, current_session_id TEXT NOT NULL,
            entry_json TEXT NOT NULL, updated_at INTEGER NOT NULL, parent_session_key TEXT,
            label TEXT, archived_at INTEGER);
        CREATE TABLE session_windows (session_id TEXT PRIMARY KEY, session_key TEXT NOT NULL,
            created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
            transcript_updated_at INTEGER, started_at INTEGER, model_provider TEXT, model TEXT,
            parent_session_key TEXT, display_name TEXT);
        CREATE TABLE transcript_events (session_id TEXT NOT NULL, seq INTEGER NOT NULL,
            event_json TEXT NOT NULL, created_at INTEGER NOT NULL, PRIMARY KEY (session_id, seq));
        CREATE TABLE session_transcript_active_events (session_id TEXT NOT NULL,
            active_position INTEGER NOT NULL, event_seq INTEGER NOT NULL,
            message_position INTEGER, context_eligible INTEGER,
            PRIMARY KEY (session_id, active_position));";

    const MAIN: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const CHILD: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";

    fn live_db(home: &Path) -> PathBuf {
        let dir = home.join(".openclaw/agents/main/agent");
        std::fs::create_dir_all(&dir).unwrap();
        // Credentials and locks beside the database must stay unopened.
        write(
            &dir.join("auth-profiles.json.sqlite-import.1.bak"),
            "not json",
        );
        write(
            &dir.join("openclaw-agent.sqlite.generation-lock.sqlite"),
            "",
        );
        let db = dir.join(AGENT_DB);
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(LIVE_SCHEMA).unwrap();
        let entry = r#"{"label":"redacted label","sessionId":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","model":"glm-5.3-flash","modelProvider":"zai","systemPromptReport":{"workspaceDir":"/workspace/redacted"},"inputTokens":75572,"outputTokens":7,"totalTokens":75707,"cacheRead":128,"cacheWrite":0,"estimatedCostUsd":0.25}"#;
        conn.execute(
            "INSERT INTO session_nodes VALUES ('agent:main:telegram:direct:1', ?1, ?2, 1790289929346, NULL, 'redacted label', NULL)",
            params![MAIN, entry],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_nodes VALUES ('agent:main:subagent:c', ?1, '{}', 1790289929346, 'agent:main:telegram:direct:1', NULL, 1790289929000)",
            params![CHILD],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_windows VALUES (?1, 'agent:main:telegram:direct:1', 1788239932804, 1790289929346, 1790289928724, 1790289581816, 'zai', 'glm-5.3-flash', NULL, NULL)",
            params![MAIN],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_windows VALUES (?1, 'agent:main:subagent:c', 1788239932804, 1788239932900, NULL, NULL, 'zai', 'glm-5.3-flash', NULL, 'redacted child')",
            params![CHILD],
        )
        .unwrap();
        // A window with no transcript yet is not a session to list.
        conn.execute(
            "INSERT INTO session_windows VALUES ('cccccccc-cccc-4ccc-8ccc-cccccccccccc', 'agent:main:subagent:c', 1, 1, NULL, NULL, NULL, NULL, NULL, NULL)",
            [],
        )
        .unwrap();
        let events: &[(&str, &str)] = &[
            (
                MAIN,
                r#"{"type":"session","version":3,"id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","timestamp":"2026-09-01T06:57:12.834Z","cwd":"/workspace/redacted"}"#,
            ),
            (
                MAIN,
                r#"{"type":"model_change","id":"1","parentId":null,"timestamp":"2026-09-01T06:57:12.835Z","provider":"zai","modelId":"glm-5.3-flash"}"#,
            ),
            (
                MAIN,
                r#"{"type":"thinking_level_change","id":"2","parentId":"1","timestamp":"2026-09-01T06:57:12.836Z","thinkingLevel":"low"}"#,
            ),
            (
                MAIN,
                r#"{"type":"custom","customType":"model-snapshot","data":{"timestamp":1,"provider":"zai","modelApi":"x","modelId":"glm-5.3-flash"},"id":"3","parentId":"2","timestamp":"2026-09-01T06:57:12.837Z"}"#,
            ),
            (
                MAIN,
                r#"{"type":"message","id":"4","parentId":"3","timestamp":"2026-09-01T06:57:13.000Z","message":{"role":"user","content":"redacted question","timestamp":1788245833000,"__openclaw":{"senderIsOwner":true}}}"#,
            ),
            (
                MAIN,
                r#"{"type":"message","id":"5","parentId":"4","timestamp":"2026-09-01T06:57:14.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"redacted thought","thinkingSignature":"sig"},{"type":"toolCall","id":"call_1","name":"exec","arguments":{"command":"true","timeout":5},"partialArgs":"{}"}],"api":"x","provider":"zai","model":"glm-5.3-flash","usage":{"input":10,"output":2,"cacheRead":0,"cacheWrite":0,"reasoningTokens":0,"totalTokens":12,"cost":{"input":0.1,"output":0.01,"cacheRead":0,"cacheWrite":0,"total":0.11}},"stopReason":"toolUse","timestamp":1788245834000,"responseId":"r"}}"#,
            ),
            (
                MAIN,
                r#"{"type":"message","id":"6","parentId":"5","timestamp":"2026-09-01T06:57:15.000Z","message":{"role":"toolResult","toolCallId":"call_1","toolName":"exec","content":[{"type":"text","text":"redacted output"}],"details":{},"isError":true,"timestamp":1788245835000}}"#,
            ),
            // A rewound branch: kept in the table, off the active path.
            (
                MAIN,
                r#"{"type":"message","id":"7","parentId":"6","timestamp":"2026-09-01T06:57:16.000Z","message":{"role":"assistant","content":[{"type":"text","text":"abandoned branch"}],"usage":{"input":1,"output":1,"cost":{"total":0.0}}}}"#,
            ),
            (
                MAIN,
                r#"{"type":"leaf","id":"8","parentId":"6","timestamp":"2026-09-01T06:57:17.000Z","targetId":"6"}"#,
            ),
            (
                MAIN,
                r#"{"type":"message","id":"9","parentId":"6","timestamp":"2026-09-01T06:57:18.000Z","message":{"role":"assistant","content":[{"type":"text","text":"redacted answer"}],"usage":{"input":20,"output":3,"cost":{"total":0.2}}}}"#,
            ),
            (
                MAIN,
                r#"{"type":"reset","id":"10","parentId":"9","timestamp":"2026-09-01T06:57:19.000Z","reason":"new"}"#,
            ),
            (
                MAIN,
                r#"{"type":"custom_message","customType":"openclaw:turn-aborted","content":"redacted notice","display":true,"id":"11","parentId":"10","timestamp":"2026-09-01T06:57:20.000Z"}"#,
            ),
            (
                CHILD,
                r#"{"type":"session","version":3,"id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","timestamp":"2026-09-01T06:57:12.834Z","cwd":"/workspace/redacted"}"#,
            ),
            (
                CHILD,
                r#"{"type":"message","id":"1","parentId":null,"timestamp":"2026-09-01T06:57:13.000Z","message":{"role":"user","content":"redacted task"}}"#,
            ),
        ];
        let mut seq: HashMap<&str, i64> = HashMap::new();
        for (session, event) in events {
            let next = seq.entry(session).or_insert(0);
            conn.execute(
                "INSERT INTO transcript_events VALUES (?1, ?2, ?3, 0)",
                params![session, *next, event],
            )
            .unwrap();
            *next += 1;
        }
        // MAIN skips seq 7 (rewound); CHILD has no active path and falls back to seq order.
        for (position, event_seq) in [0, 1, 2, 3, 4, 5, 6, 8, 9, 10, 11].iter().enumerate() {
            conn.execute(
                "INSERT INTO session_transcript_active_events VALUES (?1, ?2, ?3, NULL, NULL)",
                params![MAIN, i64::try_from(position).unwrap(), event_seq],
            )
            .unwrap();
        }
        db
    }

    #[test]
    fn a_migrated_sqlite_store_lists_with_workspace_label_usage_and_lineage() {
        let dir = tempfile::tempdir().unwrap();
        let db = live_db(dir.path());
        // The retired JSONL copy left by the migration must not add a row.
        let sessions = transcript_dir(dir.path());
        write(
            &sessions.join(format!(
                "{MAIN}.jsonl.deleted.2026-09-02T03-00-31.217Z.x.zst"
            )),
            "",
        );
        write(&sessions.join(format!("{MAIN}.trajectory.jsonl")), "{}\n");

        let mut listed = store(dir.path()).list().unwrap();
        listed.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(listed.len(), 2, "windows without transcripts are skipped");
        let main = &listed[0];
        assert_eq!(main.id, MAIN);
        assert_eq!(main.title.as_deref(), Some("redacted label"));
        assert_eq!(main.model.as_deref(), Some("glm-5.3-flash"));
        assert_eq!(
            main.recorded_cwd.as_deref(),
            Some(Path::new("/workspace/redacted"))
        );
        assert_eq!(main.trust, PathTrust::Recorded);
        assert_eq!(
            main.usage,
            Some(Usage {
                input: 75572,
                output: 7,
                cost: 0.25
            })
        );
        assert_eq!(main.updated, from_unix_millis(1_790_289_929_346).unwrap());
        assert_eq!(main.created, from_unix_millis(1_790_289_581_816));
        assert!(!main.subagent && !main.archived);
        assert_eq!(
            main.locator,
            Locator::Row {
                db: db.clone(),
                id: MAIN.to_owned()
            }
        );

        let child = &listed[1];
        assert_eq!(child.title.as_deref(), Some("redacted child"));
        assert!(child.subagent && child.archived);
        assert_eq!(child.parent.as_deref(), Some(MAIN));
        assert_eq!(child.usage, None, "an empty entry claims no usage");
    }

    #[test]
    fn a_sqlite_session_opens_along_its_active_branch() {
        let dir = tempfile::tempdir().unwrap();
        let db = live_db(dir.path());
        let transcript = store(dir.path())
            .open(&Locator::Row {
                db,
                id: MAIN.to_owned(),
            })
            .unwrap();
        assert_eq!(
            transcript.unknown_records, 0,
            "every live event type is known"
        );
        assert_eq!(
            transcript.workspace.as_deref(),
            Some(Path::new("/workspace/redacted"))
        );
        let roles: Vec<Role> = transcript.turns.iter().map(|turn| turn.role).collect();
        assert_eq!(
            roles,
            [
                Role::User,
                Role::Assistant,
                Role::Tool,
                Role::Assistant,
                Role::System,
                Role::System
            ]
        );
        assert!(
            matches!(&transcript.turns[1].blocks[0], Block::Thinking { text } if text == "redacted thought")
        );
        assert!(
            matches!(&transcript.turns[1].blocks[1], Block::ToolCall { name, input } if name == "exec" && input["timeout"] == 5)
        );
        assert!(
            matches!(&transcript.turns[2].blocks[0], Block::ToolResult { name: Some(name), ok: false, output } if name == "exec" && output == "redacted output")
        );
        assert!(
            transcript
                .turns
                .iter()
                .flat_map(|turn| &turn.blocks)
                .all(|block| !matches!(block, Block::Text { text } if text == "abandoned branch")),
            "a rewound branch is not replayed"
        );
        assert_eq!(transcript.turns[1].model.as_deref(), Some("glm-5.3-flash"));
        let usage = transcript.usage().unwrap();
        assert_eq!((usage.input, usage.output), (30, 5));
        assert!((usage.cost - 0.31).abs() < 1e-9);
    }

    #[test]
    fn a_sqlite_session_without_an_active_path_replays_every_event() {
        let dir = tempfile::tempdir().unwrap();
        let db = live_db(dir.path());
        let transcript = store(dir.path())
            .open(&Locator::Row {
                db,
                id: CHILD.to_owned(),
            })
            .unwrap();
        assert_eq!(transcript.title.as_deref(), Some("redacted child"));
        assert_eq!(transcript.turns.len(), 1);
    }

    #[test]
    fn an_unknown_sqlite_row_is_a_missing_row_error() {
        let dir = tempfile::tempdir().unwrap();
        let db = live_db(dir.path());
        assert!(matches!(
            store(dir.path()).open(&Locator::Row {
                db,
                id: "nope".to_owned()
            }),
            Err(Error::MissingRow { .. })
        ));
    }

    #[test]
    fn a_live_shaped_jsonl_transcript_parses_without_unknowns() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = transcript_dir(dir.path());
        let path = sessions.join(format!("{MAIN}.jsonl"));
        write(
            &path,
            concat!(
                r#"{"type":"session","version":3,"id":"x","timestamp":"2026-09-01T06:57:12.834Z","cwd":"/workspace/redacted"}"#,
                "\n",
                r#"{"type":"model_change","id":"1","parentId":null,"timestamp":"2026-09-01T06:57:12.835Z","provider":"zai","modelId":"m"}"#,
                "\n",
                r#"{"type":"message","id":"2","parentId":"1","timestamp":"2026-09-01T06:57:13.000Z","message":{"role":"user","content":"q"}}"#,
                "\n",
                r#"{"type":"message","id":"3","parentId":"2","timestamp":"2026-09-01T06:57:14.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"t"},{"type":"toolCall","name":"read","arguments":{"path":"/a"}}]}}"#,
                "\n",
                r#"{"type":"message","id":"4","parentId":"3","timestamp":"2026-09-01T06:57:15.000Z","message":{"role":"toolResult","toolName":"read","content":[{"type":"text","text":"ok"}],"isError":false}}"#,
                "\n",
            ),
        );
        write(
            &sessions.join("sessions.json"),
            &format!(
                r#"{{"agent:main:subagent:x":{{"sessionId":"{MAIN}","label":"l","systemPromptReport":{{"workspaceDir":"/workspace/redacted"}},"inputTokens":1,"outputTokens":2,"estimatedCostUsd":0.5}}}}"#
            ),
        );
        let listed = store(dir.path()).list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].recorded_cwd.as_deref(),
            Some(Path::new("/workspace/redacted"))
        );
        assert!(listed[0].subagent);
        assert_eq!(listed[0].usage.map(|usage| usage.cost), Some(0.5));

        let transcript = store(dir.path()).open(&Locator::File { path }).unwrap();
        assert_eq!(transcript.unknown_records, 0);
        assert_eq!(
            transcript.workspace.as_deref(),
            Some(Path::new("/workspace/redacted"))
        );
        assert_eq!(transcript.turns.len(), 3);
        assert!(matches!(&transcript.turns[1].blocks[0], Block::Thinking { text } if text == "t"));
        assert!(matches!(
            &transcript.turns[2].blocks[0],
            Block::ToolResult { ok: true, .. }
        ));
    }
}
