//! Gemini CLI: `~/.gemini/tmp/<opaque-project-hash>/chats/*.json`.
//!
//! The project directory is an opaque hash, not a flattened cwd.  A measured
//! chat begins `{"sessionId":"6e21…","projectHash":"0465…","startTime":"2025-11-19T10:46:12.253Z",…}`.
//! The adjacent `logs.json` has no path, and the inspected config project
//! sidecars do record `folderUri` but share no id or hash reference with `tmp`,
//! so sessions deliberately remain unattributed.  `logs.json` is an aggregate
//! of multiple session ids (for example,
//! `{"sessionId":"0d25…","type":"user","message":"/settings"}`), not an
//! individually addressable transcript.  Google Antigravity stores under
//! `~/.gemini/antigravity*` and `~/.config/Antigravity` are deliberately excluded.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::{Result, io};
use crate::model::{Block, Locator, Role, SessionSummary, SourceId, Transcript, Turn};
use crate::source::{
    RemoteLayout, Source, file_locator, mtime, parse_rfc3339, push_unknown, read_head,
};

const HEAD_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub struct Gemini {
    root: PathBuf,
}

impl Gemini {
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join(".gemini/tmp"),
        }
    }
}

impl Source for Gemini {
    fn id(&self) -> SourceId {
        SourceId::Gemini
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn list(&self) -> Result<Vec<SessionSummary>> {
        let Ok(projects) = std::fs::read_dir(&self.root) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for project in projects.flatten() {
            if !project.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let chats = project.path().join("chats");
            let Ok(files) = std::fs::read_dir(chats) else {
                continue;
            };
            for entry in files.flatten() {
                let path = entry.path();
                if entry.file_type().is_ok_and(|kind| kind.is_file())
                    && path
                        .extension()
                        .is_some_and(|extension| extension == "json")
                    && let Some(summary) = read_summary(&path)?
                {
                    out.push(summary);
                }
            }
        }
        Ok(out)
    }

    fn open(&self, locator: &Locator) -> Result<Transcript> {
        let path = file_locator(self.id(), locator)?;
        let value = read_json(path)?;
        Ok(match value {
            Value::Object(session) => open_chat(path, session),
            // Logs are not emitted by `list`, but retaining this branch makes a
            // saved locator readable if Gemini changes its persistence policy.
            Value::Array(logs) => open_logs(path, logs),
            raw => {
                let mut transcript = Transcript::new(stem(path), SourceId::Gemini);
                push_unknown(&mut transcript, raw);
                transcript
            }
        })
    }

    /// The shell transport finds the chats under each opaque project hash.
    fn remote_layout(&self) -> RemoteLayout {
        RemoteLayout {
            files: &[".gemini/tmp/*/chats/*.json"],
            sql: None,
        }
    }
}

fn read_summary(path: &Path) -> Result<Option<SessionSummary>> {
    let head = read_head(path, HEAD_BYTES)?;
    let (updated_mtime, size) = mtime(path)?;
    let id = head_string_field(&head, "sessionId").unwrap_or_else(|| stem(path));
    let created = head_string_field(&head, "startTime")
        .as_deref()
        .and_then(parse_rfc3339);
    let updated = head_string_field(&head, "lastUpdated")
        .as_deref()
        .and_then(parse_rfc3339)
        .unwrap_or(updated_mtime);

    Ok(Some(SessionSummary {
        created,
        size_bytes: Some(size),
        ..SessionSummary::new(
            id,
            SourceId::Gemini,
            updated,
            Locator::File {
                path: path.to_path_buf(),
            },
        )
    }))
}

/// Pull one top-level scalar from the bounded header without deserializing the
/// `messages` array.  Chat bodies can be megabytes, and list rows never need
/// their text; the measured metadata fields precede that array.
fn head_string_field(bytes: &[u8], field: &str) -> Option<String> {
    let metadata_end = bytes
        .windows(b"\"messages\"".len())
        .position(|window| window == b"\"messages\"")
        .unwrap_or(bytes.len());
    let metadata = &bytes[..metadata_end];
    let needle = format!("\"{field}\"");
    let start = metadata
        .windows(needle.len())
        .position(|window| window == needle.as_bytes())?;
    let mut index = start + needle.len();
    skip_whitespace(metadata, &mut index);
    if metadata.get(index) != Some(&b':') {
        return None;
    }
    index += 1;
    skip_whitespace(metadata, &mut index);
    if metadata.get(index) != Some(&b'"') {
        return None;
    }
    let end = string_end(metadata, index)?;
    serde_json::from_slice::<String>(&metadata[index..=end]).ok()
}

fn skip_whitespace(bytes: &[u8], index: &mut usize) {
    while bytes.get(*index).is_some_and(u8::is_ascii_whitespace) {
        *index += 1;
    }
}

fn string_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut escaped = false;
    for (offset, byte) in bytes.iter().copied().enumerate().skip(start + 1) {
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == b'\"' {
            return Some(offset);
        }
    }
    None
}

fn read_json(path: &Path) -> Result<Value> {
    let bytes = std::fs::read(path).map_err(io(path))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| io(path)(std::io::Error::new(std::io::ErrorKind::InvalidData, error)))
}

fn open_chat(path: &Path, session: serde_json::Map<String, Value>) -> Transcript {
    let id = string_field(&session, "sessionId").unwrap_or_else(|| stem(path));
    let mut transcript = Transcript::new(id, SourceId::Gemini);
    let Some(messages) = session.get("messages").and_then(Value::as_array) else {
        push_unknown(&mut transcript, Value::Object(session));
        return transcript;
    };
    for message in messages {
        append_message(&mut transcript, message.clone());
    }
    transcript
}

fn open_logs(path: &Path, logs: Vec<Value>) -> Transcript {
    let mut transcript = Transcript::new(stem(path), SourceId::Gemini);
    for record in logs {
        let Some(kind) = record.get("type").and_then(Value::as_str) else {
            push_unknown(&mut transcript, record);
            continue;
        };
        let role = match kind {
            "user" => Role::User,
            "gemini" | "model" => Role::Assistant,
            _ => {
                push_unknown(&mut transcript, record);
                continue;
            }
        };
        let Some(message) = record.get("message").and_then(Value::as_str) else {
            push_unknown(&mut transcript, record);
            continue;
        };
        let mut turn = Turn::new(role);
        turn.timestamp = record
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339);
        turn.blocks.push(Block::text(message));
        transcript.push(turn);
    }
    transcript
}

fn append_message(transcript: &mut Transcript, message: Value) {
    let Some(kind) = message.get("type").and_then(Value::as_str) else {
        push_unknown(transcript, message);
        return;
    };
    let role = match kind {
        "user" => Role::User,
        "gemini" | "model" => Role::Assistant,
        "system" => Role::System,
        _ => {
            push_unknown(transcript, message);
            return;
        }
    };
    let timestamp = message
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339);
    let mut turn = Turn::new(role);
    turn.timestamp = timestamp;
    if let Some(content) = message.get("content").and_then(Value::as_str) {
        turn.blocks.push(Block::text(content));
    }
    if let Some(thoughts) = message.get("thoughts").and_then(Value::as_array) {
        for thought in thoughts {
            if let Some(text) = string_field_value(thought, "description") {
                turn.blocks.push(Block::thinking(text));
            } else {
                transcript.unknown_records += 1;
                turn.blocks.push(Block::Unknown {
                    raw: thought.clone(),
                });
            }
        }
    }
    if let Some(calls) = message.get("toolCalls").and_then(Value::as_array) {
        for call in calls {
            if let Some(block) = tool_call(call) {
                turn.blocks.push(block);
            } else {
                transcript.unknown_records += 1;
                turn.blocks.push(Block::Unknown { raw: call.clone() });
            }
        }
    }
    transcript.push(turn);

    if let Some(calls) = message.get("toolCalls").and_then(Value::as_array) {
        for call in calls {
            if let Some(result) = tool_result(call) {
                let mut tool_turn = Turn::new(Role::Tool);
                tool_turn.timestamp = timestamp;
                tool_turn.blocks.push(result);
                transcript.push(tool_turn);
            }
        }
    }
}

fn tool_call(call: &Value) -> Option<Block> {
    Some(Block::ToolCall {
        name: string_field_value(call, "name")?,
        input: call.get("args").cloned().unwrap_or(Value::Null),
    })
}

fn tool_result(call: &Value) -> Option<Block> {
    let result = call.get("result")?;
    let ok = call.get("status").and_then(Value::as_str) != Some("error");
    Some(Block::ToolResult {
        name: string_field_value(call, "name"),
        ok,
        output: json_text(result),
    })
}

fn string_field(object: &serde_json::Map<String, Value>, field: &str) -> Option<String> {
    object.get(field).and_then(Value::as_str).map(str::to_owned)
}

fn string_field_value(value: &Value, field: &str) -> Option<String> {
    value.get(field)?.as_str().map(str::to_owned)
}

fn json_text(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn lists_chat_from_an_opaque_project_without_inventing_a_workspace() {
        let temp = tempdir().expect("temporary store");
        let chat_dir = temp.path().join("hash/chats");
        fs::create_dir_all(&chat_dir).expect("chat directory");
        fs::write(
            chat_dir.join("session-2025-01-01-example.json"),
            r#"{"sessionId":"example","projectHash":"hash","startTime":"2025-01-01T00:00:00Z","lastUpdated":"2025-01-01T00:01:00Z","messages":[{"type":"user","content":"hello"}]}"#,
        )
        .expect("chat session");

        let source = Gemini {
            root: temp.path().to_path_buf(),
        };
        let summaries = source.list().expect("list sessions");

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, "example");
        assert_eq!(summaries[0].recorded_cwd, None);
        assert_eq!(summaries[0].workspace, None);
    }

    #[test]
    fn unrecognized_message_is_retained_when_opening() {
        let temp = tempdir().expect("temporary store");
        let path = temp.path().join("session.json");
        fs::write(
            &path,
            r#"{"sessionId":"example","messages":[{"type":"future","content":"new schema"}]}"#,
        )
        .expect("chat session");

        let source = Gemini {
            root: temp.path().to_path_buf(),
        };
        let transcript = source
            .open(&Locator::File { path })
            .expect("open transcript");

        assert_eq!(transcript.unknown_records, 1);
        assert!(matches!(
            transcript
                .turns
                .first()
                .and_then(|turn| turn.blocks.first()),
            Some(Block::Unknown { .. })
        ));
    }

    #[test]
    fn tool_calls_produce_assistant_calls_and_tool_results() {
        let temp = tempdir().expect("temporary store");
        let path = temp.path().join("session.json");
        fs::write(
            &path,
            r#"{"sessionId":"example","messages":[{"type":"gemini","toolCalls":[{"name":"read_file","args":{"path":"a.txt"},"result":{"output":"contents"},"status":"success"}]}]}"#,
        )
        .expect("chat session");

        let source = Gemini {
            root: temp.path().to_path_buf(),
        };
        let transcript = source
            .open(&Locator::File { path })
            .expect("open transcript");

        assert!(matches!(
            transcript.turns.first().and_then(|turn| turn.blocks.first()),
            Some(Block::ToolCall { name, .. }) if name == "read_file"
        ));
        assert!(matches!(
            transcript.turns.get(1).and_then(|turn| turn.blocks.first()),
            Some(Block::ToolResult { ok: true, .. })
        ));
    }
}
