//! Codex: rollout JSONL under `~/.codex/sessions/YYYY/MM/DD/`, indexed by
//! `~/.codex/state_5.sqlite`.
//!
//! Two tiers in one source. `state_5.sqlite:threads` carries cwd, title,
//! model, tokens and git branch for the sessions it knows about — free. It
//! does not cover everything (70 rows against 78 rollout files here, and
//! legacy flat files predate it), so the directory is swept afterwards and
//! anything unindexed is read from its `session_meta` header.
//!
//! `~/.codex/session_index.jsonl` is *not* an index despite the name: four
//! records of `{id, thread_name, updated_at}`, no cwd. It is ignored.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::error::{Result, sql};
use crate::model::{
    ArchiveOutcome, Block, FileChange, Locator, Role, SessionSummary, SourceId, Transcript, Turn,
    Usage,
};
use crate::source::{
    RemoteLayout, Source, clean_title, file_locator, from_unix_secs, mtime, parse_rfc3339,
    read_first_line, read_records, str_field,
};
use crate::storage::sqlite;

/// Codex headers embed the full base instructions, so the first line alone
/// runs tens of kilobytes.
const HEADER_CAP: usize = 1024 * 1024;

const QUERY: &str = "\
SELECT id, rollout_path, title, cwd, model, NULL, created_at, updated_at, archived, git_branch
FROM threads";

#[derive(Debug)]
pub struct Codex {
    sessions: PathBuf,
    state_db: PathBuf,
}

impl Codex {
    pub fn new(home: &Path) -> Self {
        Self {
            sessions: home.join(".codex/sessions"),
            state_db: home.join(".codex/state_5.sqlite"),
        }
    }

    fn indexed(&self) -> Result<Vec<SessionSummary>> {
        if !self.state_db.exists() {
            return Ok(Vec::new());
        }
        let (conn, _stale) = sqlite::open(&self.state_db)?;
        let mut stmt = conn.prepare(QUERY).map_err(sql(&self.state_db))?;
        let rows = stmt
            .query_map([], |row| {
                let rollout: Option<String> = row.get(1)?;
                let archived: i64 = row.get::<_, Option<i64>>(8)?.unwrap_or(0);
                let id: String = row.get(0)?;
                Ok(SessionSummary {
                    recorded_cwd: row.get::<_, Option<String>>(3)?.map(PathBuf::from),
                    title: row
                        .get::<_, Option<String>>(2)?
                        .as_deref()
                        .and_then(clean_title),
                    created: row.get::<_, Option<i64>>(6)?.and_then(from_unix_secs),
                    model: row.get(4)?,
                    archived: archived != 0,
                    branch: row.get(9)?,
                    ..SessionSummary::new(
                        id.clone(),
                        SourceId::Codex,
                        row.get::<_, Option<i64>>(7)?
                            .and_then(from_unix_secs)
                            .unwrap_or(time::OffsetDateTime::UNIX_EPOCH),
                        match rollout {
                            Some(path) => Locator::File {
                                path: PathBuf::from(path),
                            },
                            None => Locator::Row {
                                db: self.state_db.clone(),
                                id,
                            },
                        },
                    )
                })
            })
            .map_err(sql(&self.state_db))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql(&self.state_db))
    }
}

impl Source for Codex {
    fn id(&self) -> SourceId {
        SourceId::Codex
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.sessions.clone(), self.state_db.clone()]
    }

    fn list(&self) -> Result<Vec<SessionSummary>> {
        let mut out = self.indexed().unwrap_or_default();
        let seen: HashSet<String> = out.iter().map(|s| s.id.clone()).collect();

        let mut files = Vec::new();
        collect_rollouts(&self.sessions, &mut files);
        for path in files {
            if id_from_filename(&path).is_some_and(|id| seen.contains(&id)) {
                continue;
            }
            if let Some(summary) = read_summary(&path)? {
                out.push(summary);
            }
        }
        Ok(out)
    }

    fn open(&self, locator: &Locator) -> Result<Transcript> {
        read_transcript(file_locator(SourceId::Codex, locator)?)
    }

    /// Codex models archiving on `threads.archived`, so state belongs in its
    /// own store rather than in our overlay. One bound-parameter statement on
    /// a connection opened read-write only for this call.
    fn archive(&self, locator: &Locator, archived: bool) -> Result<ArchiveOutcome> {
        if !self.state_db.exists() {
            return Ok(ArchiveOutcome::Unsupported);
        }
        let id = match locator {
            Locator::Row { id, .. } => id.clone(),
            Locator::File { path } => match id_from_filename(path) {
                Some(id) => id,
                None => return Ok(ArchiveOutcome::Unsupported),
            },
        };
        let conn = rusqlite::Connection::open(&self.state_db).map_err(sql(&self.state_db))?;
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let changed = conn
            .execute(
                "UPDATE threads SET archived = ?1, archived_at = ?2 WHERE id = ?3",
                rusqlite::params![i64::from(archived), archived.then_some(now), id],
            )
            .map_err(sql(&self.state_db))?;
        Ok(if changed > 0 {
            ArchiveOutcome::Native
        } else {
            ArchiveOutcome::Unsupported
        })
    }

    fn resume_command(&self, id: &str) -> Option<Vec<String>> {
        Some(vec!["codex".into(), "resume".into(), id.into()])
    }

    /// The shell transport finds rollouts in the dated tree plus the legacy flat
    /// files beside it. The titles and archive flags in `state_5.sqlite` need a
    /// remote `llm-sessions`; without one, remote Codex rows carry header data only.
    fn remote_layout(&self) -> RemoteLayout {
        RemoteLayout {
            files: &[
                ".codex/sessions/*/*/*/rollout-*.jsonl",
                ".codex/sessions/rollout-*.jsonl",
            ],
            sql: None,
        }
    }
}

/// Rollouts sit at `YYYY/MM/DD/`, but legacy installs wrote them flat, so the
/// tree is walked rather than assuming a depth.
fn collect_rollouts(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => collect_rollouts(&path, out),
            Ok(_) if is_rollout(&path) => out.push(path),
            _ => {}
        }
    }
}

fn is_rollout(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name.starts_with("rollout-")
        && path.extension().is_some_and(|ext| {
            ext.eq_ignore_ascii_case("jsonl") || ext.eq_ignore_ascii_case("json")
        })
}

/// `rollout-<timestamp>-<uuid>.jsonl` — the id is the trailing UUID.
fn id_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let uuid = stem.get(stem.len().checked_sub(36)?..)?;
    uuid.chars()
        .all(|c| c.is_ascii_hexdigit() || c == '-')
        .then(|| uuid.to_string())
}

fn read_summary(path: &Path) -> Result<Option<SessionSummary>> {
    let line = read_first_line(path, HEADER_CAP)?;
    let Ok(record) = serde_json::from_slice::<serde_json::Value>(&line) else {
        return Ok(None);
    };
    // Modern rollouts wrap the header in `payload`; legacy ones inline it.
    let meta = record.get("payload").unwrap_or(&record);

    let (updated, size) = mtime(path)?;
    let id = str_field(meta, "session_id")
        .or_else(|| str_field(meta, "id"))
        .or_else(|| id_from_filename(path));
    let Some(id) = id else {
        return Ok(None);
    };

    Ok(Some(SessionSummary {
        recorded_cwd: str_field(meta, "cwd").map(PathBuf::from),
        created: str_field(meta, "timestamp")
            .or_else(|| str_field(&record, "timestamp"))
            .as_deref()
            .and_then(parse_rfc3339),
        model: str_field(meta, "model"),
        size_bytes: Some(size),
        ..SessionSummary::new(
            id,
            SourceId::Codex,
            updated,
            Locator::File {
                path: path.to_path_buf(),
            },
        )
    }))
}

/// Rollout record types that are not conversation.
///
/// `event_msg` is deliberately in this list. It mirrors the same turns as
/// `response_item` (measured: 1 061 `agent_message` events beside 1 532
/// `message` items in one corpus), so honouring both would render every
/// assistant turn twice. `response_item` is the canonical stream.
const STATE_RECORDS: &[&str] = &[
    "session_meta",
    "turn_context",
    "world_state",
    "token_usage_record",
    "event_msg",
];

/// `event_msg.token_count.info.total_token_usage` is a session running total.
/// A measured update contains `{"input_tokens":41450,"cached_input_tokens":28672,
/// "output_tokens":248,"reasoning_output_tokens":0,"total_tokens":41698}`.
/// Content remains in `response_item`; only the last counter is attached to its
/// final assistant turn so repeated running totals cannot multiply usage.
fn read_transcript(path: &Path) -> Result<Transcript> {
    let mut transcript = Transcript::new(String::new(), SourceId::Codex);
    let mut final_usage = None;
    for record in read_records(path)? {
        let kind = record.get("type").and_then(serde_json::Value::as_str);
        // Legacy rollouts inline the payload; modern ones nest it.
        let payload = record.get("payload").unwrap_or(&record);
        let stamp = str_field(&record, "timestamp")
            .as_deref()
            .and_then(parse_rfc3339);

        match kind {
            // The oldest rollouts have no envelope at all: the header is a
            // bare `{id, timestamp, instructions}` and every turn is a naked
            // response item. The golden corpus caught this reading as two
            // unknown records and nothing else.
            None if payload.get("id").is_some() && payload.get("instructions").is_some() => {
                transcript.id = str_field(payload, "id").unwrap_or_default();
                transcript.workspace = str_field(payload, "cwd").map(PathBuf::from);
            }
            Some(
                "message"
                | "function_call"
                | "function_call_output"
                | "reasoning"
                | "custom_tool_call"
                | "custom_tool_call_output"
                | "web_search_call",
            ) => {
                if let Some(turn) = response_turn(&record, stamp) {
                    transcript.push(turn);
                }
            }
            Some("session_meta") => {
                transcript.id = str_field(payload, "session_id")
                    .or_else(|| str_field(payload, "id"))
                    .unwrap_or_default();
                transcript.workspace = str_field(payload, "cwd").map(PathBuf::from);
            }
            Some("response_item") => {
                if let Some(turn) = response_turn(payload, stamp) {
                    transcript.push(turn);
                } else {
                    transcript.unknown_records += 1;
                    let mut turn = Turn::new(Role::System);
                    turn.timestamp = stamp;
                    turn.blocks.push(Block::Unknown {
                        raw: payload.clone(),
                    });
                    transcript.push(turn);
                }
            }
            Some("compacted") => {
                let mut turn = Turn::new(Role::System);
                turn.timestamp = stamp;
                turn.blocks.push(Block::text(format!(
                    "context compacted\n\n{}",
                    str_field(payload, "message").unwrap_or_default()
                )));
                transcript.push(turn);
            }
            Some("event_msg")
                if payload.get("type").and_then(serde_json::Value::as_str)
                    == Some("token_count") =>
            {
                final_usage = payload
                    .pointer("/info/total_token_usage")
                    .and_then(codex_usage);
            }
            Some(other) if STATE_RECORDS.contains(&other) => {}
            _ => {
                transcript.unknown_records += 1;
                let mut turn = Turn::new(Role::System);
                turn.timestamp = stamp;
                turn.blocks.push(Block::Unknown { raw: record });
                transcript.push(turn);
            }
        }
    }
    if let Some(usage) = final_usage
        && let Some(turn) = transcript
            .turns
            .iter_mut()
            .rfind(|turn| turn.role == Role::Assistant)
    {
        turn.usage = Some(usage);
    }
    if transcript.id.is_empty() {
        transcript.id = id_from_filename(path).unwrap_or_default();
    }
    Ok(transcript)
}

fn codex_usage(value: &serde_json::Value) -> Option<Usage> {
    Some(Usage {
        input: value.get("input_tokens")?.as_u64()?,
        output: value.get("output_tokens")?.as_u64()?,
        cost: 0.0,
    })
}

fn response_turn(payload: &serde_json::Value, stamp: Option<time::OffsetDateTime>) -> Option<Turn> {
    let kind = payload.get("type").and_then(serde_json::Value::as_str)?;
    let mut turn = Turn::new(Role::Assistant);
    turn.timestamp = stamp;

    match kind {
        "message" => {
            turn.role = match str_field(payload, "role").as_deref() {
                Some("user") => Role::User,
                Some("developer" | "system") => Role::System,
                _ => Role::Assistant,
            };
            match payload.get("content") {
                Some(serde_json::Value::Array(blocks)) => {
                    for block in blocks {
                        // `input_text` on the way in, `output_text` on the way
                        // out; both are plain prose.
                        match str_field(block, "text") {
                            Some(text) => turn.blocks.push(Block::text(text)),
                            None => turn.blocks.push(Block::Unknown { raw: block.clone() }),
                        }
                    }
                }
                Some(serde_json::Value::String(text)) => {
                    turn.blocks.push(Block::text(text.clone()));
                }
                _ => {}
            }
        }
        "reasoning" => {
            let summary = payload
                .get("summary")
                .and_then(serde_json::Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|i| str_field(i, "text"))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            // Most reasoning is stored `encrypted_content` only. Saying so is
            // honest; rendering an empty block is not.
            turn.blocks.push(Block::thinking(if summary.is_empty() {
                "[encrypted reasoning]".to_string()
            } else {
                summary
            }));
        }
        "function_call" | "custom_tool_call" => {
            let name = str_field(payload, "name").unwrap_or_default();
            let input = tool_input(payload);
            let patch = patch_block(&name, &input);
            turn.blocks.push(Block::ToolCall { name, input });
            if let Some(patch) = patch {
                turn.blocks.push(patch);
            }
        }
        "function_call_output" | "custom_tool_call_output" => {
            turn.role = Role::Tool;
            turn.blocks.push(Block::ToolResult {
                name: str_field(payload, "name"),
                ok: payload.get("status").and_then(serde_json::Value::as_str) != Some("failed"),
                output: str_field(payload, "output").unwrap_or_default(),
            });
        }
        "web_search_call" => {
            turn.blocks.push(Block::ToolCall {
                name: "web_search".to_string(),
                input: payload.get("action").cloned().unwrap_or_default(),
            });
        }
        _ => return None,
    }
    Some(turn)
}

/// `function_call.arguments` is a JSON *string*; `custom_tool_call.input` is
/// raw text (an `apply_patch` body, for instance). Parse when it parses.
fn tool_input(payload: &serde_json::Value) -> serde_json::Value {
    let raw = payload
        .get("arguments")
        .or_else(|| payload.get("input"))
        .cloned()
        .unwrap_or_default();
    match &raw {
        serde_json::Value::String(s) => {
            serde_json::from_str(s).unwrap_or_else(|_| serde_json::Value::String(s.clone()))
        }
        _ => raw,
    }
}

/// A measured Codex custom call is `{"name":"apply_patch","input":"*** Update
/// File: internal/modules/cart/service.go… "}`.  The patch headers are the
/// only file identities trusted here; arbitrary tool text is never guessed.
fn patch_block(name: &str, input: &serde_json::Value) -> Option<Block> {
    let files = match name {
        "apply_patch" => input.as_str().map(patch_paths).unwrap_or_default(),
        "write" => input
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(|path| {
                vec![FileChange {
                    path: PathBuf::from(path),
                    added: 0,
                    deleted: 0,
                }]
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    (!files.is_empty()).then(|| Block::Patch {
        summary: name.to_owned(),
        files,
    })
}

fn patch_paths(input: &str) -> Vec<FileChange> {
    input
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            ["*** Update File: ", "*** Add File: ", "*** Delete File: "]
                .iter()
                .find_map(|&prefix| line.strip_prefix(prefix))
                .filter(|path| !path.is_empty())
        })
        .map(|path| FileChange {
            path: PathBuf::from(path),
            added: 0,
            deleted: 0,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn transcript_of(lines: &[&str]) -> Transcript {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("rollout-2026-01-01T00-00-00-11111111-2222-3333-4444-555555555555.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        drop(file);
        read_transcript(&path).unwrap()
    }

    #[test]
    fn token_count_uses_only_the_final_running_total() {
        let t = transcript_of(&[
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}}"#,
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":2,"output_tokens":3}}}}"#,
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":20,"output_tokens":30}}}}"#,
        ]);

        assert_eq!(
            t.usage(),
            Some(Usage {
                input: 20,
                output: 30,
                cost: 0.0,
            })
        );
        assert_eq!(t.turns.len(), 1);
    }

    #[test]
    fn event_msg_does_not_duplicate_response_item() {
        let t = transcript_of(&[
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}}"#,
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"hello"}}"#,
        ]);
        assert_eq!(t.turns.len(), 1);
        assert_eq!(t.unknown_records, 0);
    }

    #[test]
    fn encrypted_reasoning_is_labelled_not_blank() {
        let t = transcript_of(&[
            r#"{"type":"response_item","payload":{"type":"reasoning","summary":[],"encrypted_content":"gAAAA"}}"#,
        ]);
        match &t.turns[0].blocks[0] {
            Block::Thinking { text } => assert_eq!(text, "[encrypted reasoning]"),
            other => panic!("expected thinking, got {other:?}"),
        }
    }

    #[test]
    fn function_call_arguments_are_parsed_out_of_their_json_string() {
        let t = transcript_of(&[
            r#"{"type":"response_item","payload":{"type":"function_call","name":"read","arguments":"{\"path\":\"/tmp/x\"}"}}"#,
        ]);
        match &t.turns[0].blocks[0] {
            Block::ToolCall { name, input } => {
                assert_eq!(name, "read");
                assert_eq!(input["path"], "/tmp/x");
            }
            other => panic!("expected tool call, got {other:?}"),
        }
    }

    #[test]
    fn apply_patch_call_exposes_only_header_file_paths() {
        let t = transcript_of(&[
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch","input":"*** Begin Patch\n*** Update File: src/lib.rs\n*** Add File: tests/new.rs\n*** End Patch"}}"#,
        ]);

        match &t.turns[0].blocks[1] {
            Block::Patch { files, .. } => {
                assert_eq!(files.len(), 2);
                assert_eq!(files[0].path, PathBuf::from("src/lib.rs"));
                assert_eq!(files[1].path, PathBuf::from("tests/new.rs"));
            }
            other => panic!("expected patch, got {other:?}"),
        }
    }

    #[test]
    fn id_comes_from_the_filename_when_the_header_is_missing() {
        let t = transcript_of(&[r#"{"type":"turn_context","payload":{}}"#]);
        assert_eq!(t.id, "11111111-2222-3333-4444-555555555555");
    }

    #[test]
    fn unknown_response_item_survives_as_raw() {
        let t = transcript_of(&[r#"{"type":"response_item","payload":{"type":"telepathy_call"}}"#]);
        assert_eq!(t.unknown_records, 1);
    }
}
