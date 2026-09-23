//! `OpenCode` v2 transcripts: `session_v2` plus `session_message` in the same
//! `opencode.db` the v1 tables live in.
//!
//! v2 drops the v1 message/part split. Each `session_message` row is one
//! typed record — `type` is a column, `data` its JSON — ordered by a per-session
//! `seq`. Measured on a v2.0.14 store: `user`, `assistant`, `synthetic`,
//! `system`, `compaction`, `idle`, `agent-switched` and `model-switched`.
//! An assistant row carries its parts inline, e.g.
//! `{"agent":"build","model":{"id":"glm-5.3-flash",…},"content":[{"type":"tool",
//! "name":"read","state":{"status":"completed","input":{…},"content":[…]}}],
//! "snapshot":{"start":"…","end":"…","files":["src/app.rs"]},"cost":0,
//! "tokens":{"input":7521,"output":467,…}}`.
//!
//! Which generation answers for a session is decided by
//! [`super::opencode_schema`]; this module only decodes the v2 body.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use crate::error::{Error, Result, sql};
use crate::model::{Block, FileChange, Role, SourceId, Transcript, Turn};
use crate::source::opencode_schema::{json_display, json_or_string, opencode_usage};
use crate::source::{clean_title, count_unknown, flatten_text, from_epoch_guess, push_unknown};

/// Read one v2 session. `seq` is unique per session and indexed, so the
/// order is the one the agent recorded.
pub(crate) fn open(conn: &Connection, db: &Path, source: SourceId, id: &str) -> Result<Transcript> {
    let (title, directory, path): (Option<String>, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT title, directory, path FROM session_v2 WHERE id = ?1",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(sql(db))?
        .ok_or_else(|| Error::MissingRow {
            db: db.to_path_buf(),
            id: id.to_owned(),
        })?;

    let mut transcript = Transcript::new(id, source);
    transcript.title = title.as_deref().and_then(clean_title);
    let root = directory
        .as_deref()
        .map(|directory| snapshot_root(Path::new(directory), path.as_deref()));
    transcript.workspace = directory.map(PathBuf::from);

    let mut statement = conn
        .prepare("SELECT type, data FROM session_message WHERE session_id = ?1 ORDER BY seq")
        .map_err(sql(db))?;
    let rows = statement
        .query_map(params![id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(sql(db))?;
    for row in rows {
        let (kind, data) = row.map_err(sql(db))?;
        record(
            &mut transcript,
            &kind,
            &json_or_string(&data),
            root.as_deref(),
        );
    }
    Ok(transcript)
}

/// Snapshot paths are relative to the project root the session ran in, not
/// to its cwd. `path` is that cwd relative to the root — `''` on every
/// measured git session, `tmp` for a `/tmp` session of the global `/`
/// project — so stripping it off `directory` recovers the root.
fn snapshot_root(directory: &Path, path: Option<&str>) -> PathBuf {
    let Some(relative) = path.map(Path::new).filter(|p| !p.as_os_str().is_empty()) else {
        return directory.to_path_buf();
    };
    if !directory.ends_with(relative) {
        return directory.to_path_buf();
    }
    directory
        .ancestors()
        .nth(relative.components().count())
        .unwrap_or(directory)
        .to_path_buf()
}

fn record(transcript: &mut Transcript, kind: &str, data: &Value, root: Option<&Path>) {
    let mut turn = match kind {
        "user" => user_turn(transcript, data),
        "assistant" => assistant_turn(transcript, data, root),
        // Injected text the user did not type: plan-mode reminders,
        // continuation nudges, background shell results, date updates.
        "synthetic" | "system" => {
            let mut turn = Turn::new(Role::System);
            if let Some(text) = nonempty_text_at(data, "/text") {
                turn.blocks.push(Block::text(text));
            }
            turn
        }
        "compaction" => {
            let mut turn = Turn::new(Role::System);
            turn.usage = opencode_usage(data);
            turn.blocks.push(Block::text(format!(
                "context compacted\n\n{}",
                nonempty_text_at(data, "/summary").unwrap_or_default()
            )));
            turn
        }
        "agent-switched" => system_note(format!(
            "agent: {}",
            nonempty_text_at(data, "/agent").unwrap_or_default()
        )),
        "model-switched" => system_note(format!(
            "model: {}",
            nonempty_text_at(data, "/model/id").unwrap_or_default()
        )),
        // The run-loop settling; it carries only an outcome, no content.
        "idle" => return,
        _ => {
            push_unknown(
                transcript,
                serde_json::json!({ "type": kind, "data": data }),
            );
            return;
        }
    };
    turn.timestamp = data
        .pointer("/time/created")
        .and_then(Value::as_i64)
        .and_then(from_epoch_guess);
    transcript.push(turn);
}

fn system_note(text: String) -> Turn {
    let mut turn = Turn::new(Role::System);
    turn.blocks.push(Block::text(text));
    turn
}

/// `{"text":"…","files":[…],"agents":[…]}`. Agent mentions are already in
/// the text as `@name`, so only the attachments add anything.
fn user_turn(transcript: &mut Transcript, data: &Value) -> Turn {
    let mut turn = Turn::new(Role::User);
    for file in data
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match attachment(file) {
            Some(block) => turn.blocks.push(block),
            None => count_unknown(transcript, &mut turn, file.clone()),
        }
    }
    if let Some(text) = nonempty_text_at(data, "/text") {
        turn.blocks.push(Block::text(text));
    }
    turn
}

/// A measured `@idea.md` mention is `{"mime":"text/plain","name":"idea.md",
/// "source":{"type":"uri","uri":"file:///home/…/idea.md"},"data":"…"}`; a
/// pasted image is `{"mime":"image/png","name":"pagination.png",
/// "source":{"type":"inline"},"data":"iVBOR…"}`. The inline payload is never
/// rendered, only named.
fn attachment(file: &Value) -> Option<Block> {
    if let Some(path) = file
        .pointer("/source/uri")
        .and_then(Value::as_str)
        .and_then(|uri| uri.strip_prefix("file://"))
    {
        return Some(Block::File {
            path: PathBuf::from(path),
        });
    }
    let name = file.get("name").and_then(Value::as_str);
    let mime = file.get("mime").and_then(Value::as_str);
    if mime.is_some_and(|mime| mime.starts_with("image/")) {
        return Some(Block::Image {
            label: name.or(mime).unwrap_or("image").to_owned(),
        });
    }
    name.map(|name| Block::File {
        path: PathBuf::from(name),
    })
}

fn assistant_turn(transcript: &mut Transcript, data: &Value, root: Option<&Path>) -> Turn {
    let mut turn = Turn::new(Role::Assistant);
    turn.model = nonempty_text_at(data, "/model/id");
    turn.usage = opencode_usage(data);
    for part in data
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        content_part(transcript, &mut turn, part);
    }
    // A failed request records no content, only why: without this the turn
    // is empty and the session reads as if the agent never answered.
    if let Some(message) = nonempty_text_at(data, "/error/message") {
        turn.blocks.push(Block::text(format!("error: {message}")));
    }
    if let Some(block) = snapshot_patch(data, root) {
        turn.blocks.push(block);
    }
    turn
}

fn content_part(transcript: &mut Transcript, turn: &mut Turn, part: &Value) {
    // Encrypted reasoning records `"text":""`; the v1 reader keeps those as
    // (empty) thinking blocks. V1 also keeps empty text parts, so retain both
    // across generations rather than silently changing transcript shape.
    let text = part.get("text").and_then(Value::as_str);
    match (part.get("type").and_then(Value::as_str), text) {
        (Some("text"), Some(text)) => turn.blocks.push(Block::text(text)),
        (Some("reasoning"), Some(text)) => turn.blocks.push(Block::thinking(text)),
        (Some("tool"), _) => {
            let name = nonempty_text_at(part, "/name").unwrap_or_else(|| "tool".to_owned());
            let state = part.get("state").unwrap_or(&Value::Null);
            turn.blocks.push(Block::ToolCall {
                name: name.clone(),
                input: state.get("input").cloned().unwrap_or(Value::Null),
            });
            match state.get("status").and_then(Value::as_str) {
                Some("completed") => turn.blocks.push(Block::ToolResult {
                    name: Some(name),
                    ok: true,
                    output: flatten_text(state.get("content")),
                }),
                // `{"type":"tool.execution","message":"File not found: build.py"}`
                Some("error") => turn.blocks.push(Block::ToolResult {
                    name: Some(name),
                    ok: false,
                    output: nonempty_text_at(state, "/error/message")
                        .unwrap_or_else(|| json_display(state.get("error"))),
                }),
                _ => {}
            }
        }
        _ => count_unknown(transcript, turn, part.clone()),
    }
}

/// v2's replacement for the v1 `patch` part: the files the step's snapshot
/// saw change. v2-written paths are relative to the project root; rows the
/// migration copied from v1 keep their absolute paths. Line totals are not
/// recorded, so they stay zero, as for v1.
fn snapshot_patch(data: &Value, root: Option<&Path>) -> Option<Block> {
    let files: Vec<FileChange> = data
        .pointer("/snapshot/files")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(Value::as_str)
        .map(|file| FileChange {
            path: match root {
                Some(root) if Path::new(file).is_relative() => root.join(file),
                _ => PathBuf::from(file),
            },
            added: 0,
            deleted: 0,
        })
        .collect();
    if files.is_empty() {
        return None;
    }
    let summary = files
        .iter()
        .map(|file| file.path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Some(Block::Patch { summary, files })
}

/// Unlike v1's `string_at`, omit empty metadata and notes. Content parts
/// explicitly retain empty text/reasoning for parity with v1.
fn nonempty_text_at(value: &Value, pointer: &str) -> Option<String> {
    value
        .pointer(pointer)?
        .as_str()
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_root_strips_the_cwd_suffix() {
        assert_eq!(
            snapshot_root(Path::new("/tmp"), Some("tmp")),
            PathBuf::from("/")
        );
        assert_eq!(
            snapshot_root(Path::new("/w/repo"), Some("")),
            PathBuf::from("/w/repo")
        );
        assert_eq!(
            snapshot_root(Path::new("/w/repo"), Some("elsewhere")),
            PathBuf::from("/w/repo")
        );
        assert_eq!(
            snapshot_root(Path::new("/w/repo/src/server"), Some("src/server")),
            PathBuf::from("/w/repo")
        );
    }

    #[test]
    fn a_failed_tool_and_a_failed_request_both_surface_their_message() {
        let mut transcript = Transcript::new("s", SourceId::OpenCode);
        record(
            &mut transcript,
            "assistant",
            &serde_json::json!({
                "time": {"created": 1_790_062_960_845_i64},
                "model": {"id": "m"},
                "content": [{"type": "tool", "name": "read", "state": {
                    "status": "error", "input": {"path": "build.py"},
                    "error": {"type": "tool.execution", "message": "File not found: build.py"}
                }}],
                "finish": "error",
                "error": {"type": "provider.quota", "message": "Add credits", "status": 402}
            }),
            None,
        );
        let blocks = &transcript.turns[0].blocks;
        assert!(matches!(
            &blocks[1],
            Block::ToolResult { ok: false, output, .. } if output == "File not found: build.py"
        ));
        assert!(matches!(&blocks[2], Block::Text { text } if text == "error: Add credits"));
    }

    #[test]
    fn empty_text_and_reasoning_parts_match_v1() {
        let mut transcript = Transcript::new("s", SourceId::OpenCode);
        record(
            &mut transcript,
            "assistant",
            &serde_json::json!({
                "content": [{"type": "text", "text": ""}, {"type": "reasoning", "text": ""}]
            }),
            None,
        );
        assert!(matches!(&transcript.turns[0].blocks[0], Block::Text { text } if text.is_empty()));
        assert!(
            matches!(&transcript.turns[0].blocks[1], Block::Thinking { text } if text.is_empty())
        );
    }

    #[test]
    fn an_unknown_record_type_is_kept_not_dropped() {
        let mut transcript = Transcript::new("s", SourceId::OpenCode);
        record(
            &mut transcript,
            "future-kind",
            &serde_json::json!({"x": 1}),
            None,
        );
        record(
            &mut transcript,
            "idle",
            &serde_json::json!({"outcome": "succeeded"}),
            None,
        );
        assert_eq!(transcript.unknown_records, 1);
        assert_eq!(transcript.turns.len(), 1);
    }
}
