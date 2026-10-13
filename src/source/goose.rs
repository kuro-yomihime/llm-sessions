//! Goose: `~/.local/share/goose/sessions/*.jsonl` plus `projects.json`.
//!
//! `projects.json` is authoritative workspace metadata rather than a derived
//! directory name: it maps each project's real `path` to its `last_session_id`.
//! A measured session starts with (path redacted)
//! `{"working_dir":"/…/goose","description":"Django shipment API implementation",…}`;
//! that header wins when present.  The source deliberately never attempts to
//! reverse any display encoding.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::{Result, io};
use crate::model::{Block, Locator, Role, SessionSummary, SourceId, Transcript, Turn, Usage};
use crate::source::{
    RemoteLayout, Source, clean_title, file_locator, from_unix_secs, head_records, mtime,
    push_unknown, read_head, read_records, str_field,
};

const HEAD_BYTES: usize = 8 * 1024;

#[derive(Debug)]
pub struct Goose {
    root: PathBuf,
}

impl Goose {
    pub fn new(home: &Path) -> Self {
        Self {
            root: crate::data_home(home).join("goose"),
        }
    }

    fn sessions(&self) -> PathBuf {
        self.root.join("sessions")
    }
}

impl Source for Goose {
    fn id(&self) -> SourceId {
        SourceId::Goose
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn list(&self) -> Result<Vec<SessionSummary>> {
        let sessions = self.sessions();
        let Ok(files) = std::fs::read_dir(&sessions) else {
            return Ok(Vec::new());
        };
        let projects = read_projects(&self.root)?;
        let mut out = Vec::new();
        for entry in files.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_file())
                && path
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
                && let Some(summary) = read_summary(&path, &projects)?
            {
                out.push(summary);
            }
        }
        Ok(out)
    }

    fn open(&self, locator: &Locator) -> Result<Transcript> {
        let path = file_locator(self.id(), locator)?;
        let records = read_records(path)?;
        let id = records
            .first()
            .and_then(|record| str_field(record, "id"))
            .unwrap_or_else(|| stem(path));
        let projects = read_projects(&self.root)?;
        let mut transcript = Transcript::new(id.clone(), self.id());

        for record in records {
            if is_header(&record) {
                transcript.title = str_field(&record, "description")
                    .as_deref()
                    .and_then(clean_title);
                transcript.workspace = str_field(&record, "working_dir")
                    .map(PathBuf::from)
                    .or_else(|| projects.get(&id).cloned());
                continue;
            }
            append_record(&mut transcript, record);
        }
        if transcript.workspace.is_none() {
            transcript.workspace = projects.get(&id).cloned();
        }
        Ok(transcript)
    }

    /// The shell transport finds the session files. `projects.json` is not a
    /// transcript and is not fetched, so a remote row's workspace comes from the
    /// `working_dir` in its own header.
    fn remote_layout(&self) -> RemoteLayout {
        RemoteLayout {
            files: &[".local/share/goose/sessions/*.jsonl"],
            sql: None,
        }
    }
}

fn read_projects(root: &Path) -> Result<HashMap<String, PathBuf>> {
    let path = root.join("projects.json");
    let Ok(bytes) = std::fs::read(&path) else {
        return Ok(HashMap::new());
    };
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| io(&path)(std::io::Error::new(std::io::ErrorKind::InvalidData, error)))?;
    let mut sessions = HashMap::new();
    let Some(projects) = value.get("projects").and_then(Value::as_object) else {
        return Ok(sessions);
    };
    for project in projects.values() {
        let Some(id) = str_field(project, "last_session_id") else {
            continue;
        };
        let Some(path) = str_field(project, "path") else {
            continue;
        };
        sessions.insert(id, PathBuf::from(path));
    }
    Ok(sessions)
}

fn read_summary(
    path: &Path,
    projects: &HashMap<String, PathBuf>,
) -> Result<Option<SessionSummary>> {
    let head = read_head(path, HEAD_BYTES)?;
    let Some(header) = head_records(&head).find(is_header) else {
        return Ok(None);
    };
    let (updated, size) = mtime(path)?;
    let id = str_field(&header, "id").unwrap_or_else(|| stem(path));
    let recorded_cwd = str_field(&header, "working_dir")
        .map(PathBuf::from)
        .or_else(|| projects.get(&id).cloned());
    Ok(Some(SessionSummary {
        recorded_cwd,
        title: str_field(&header, "description")
            .as_deref()
            .and_then(clean_title),
        created: str_field(&header, "created")
            .and_then(|created| created.parse::<i64>().ok())
            .and_then(from_unix_secs),
        usage: usage(&header),
        size_bytes: Some(size),
        ..SessionSummary::new(
            id,
            SourceId::Goose,
            updated,
            Locator::File {
                path: path.to_path_buf(),
            },
        )
    }))
}

fn usage(header: &Value) -> Option<Usage> {
    let input = unsigned_field(header, "input_tokens").unwrap_or(0);
    let output = unsigned_field(header, "output_tokens").unwrap_or(0);
    (input != 0 || output != 0).then_some(Usage {
        input,
        output,
        cost: 0.0,
    })
}

fn unsigned_field(value: &Value, field: &str) -> Option<u64> {
    value.get(field)?.as_u64().or_else(|| {
        value
            .get(field)?
            .as_i64()
            .and_then(|number| u64::try_from(number).ok())
    })
}

fn is_header(record: &Value) -> bool {
    record.get("working_dir").is_some()
        || record.get("description").is_some()
        || record.get("message_count").is_some()
}

fn append_record(transcript: &mut Transcript, record: Value) {
    let Some(role) = record.get("role").and_then(Value::as_str) else {
        push_unknown(transcript, record);
        return;
    };
    let (role, timestamp) = match role {
        "user" => (
            Role::User,
            record
                .get("created")
                .and_then(Value::as_i64)
                .and_then(from_unix_secs),
        ),
        "assistant" => (
            Role::Assistant,
            record
                .get("created")
                .and_then(Value::as_i64)
                .and_then(from_unix_secs),
        ),
        "system" => (
            Role::System,
            record
                .get("created")
                .and_then(Value::as_i64)
                .and_then(from_unix_secs),
        ),
        "tool" => (
            Role::Tool,
            record
                .get("created")
                .and_then(Value::as_i64)
                .and_then(from_unix_secs),
        ),
        _ => {
            push_unknown(transcript, record);
            return;
        }
    };
    let Some(content) = record.get("content") else {
        push_unknown(transcript, record);
        return;
    };

    match content {
        Value::String(text) => {
            let mut turn = Turn::new(role);
            turn.timestamp = timestamp;
            turn.blocks.push(Block::text(text));
            transcript.push(turn);
        }
        Value::Array(items) => append_content(transcript, role, timestamp, items),
        _ => push_unknown(transcript, record.clone()),
    }
}

fn append_content(
    transcript: &mut Transcript,
    role: Role,
    timestamp: Option<time::OffsetDateTime>,
    items: &[Value],
) {
    let mut turn = Turn::new(role);
    turn.timestamp = timestamp;
    for item in items {
        if let Some(text) = str_field(item, "text") {
            turn.blocks.push(Block::text(text));
            continue;
        }
        if item.get("toolCall").is_some() {
            if let Some(block) = tool_call(item) {
                turn.blocks.push(block);
            } else {
                transcript.unknown_records += 1;
                turn.blocks.push(Block::Unknown { raw: item.clone() });
            }
            continue;
        }
        // A measured record is
        // `{"type":"toolResponse","id":"…","toolResult":{"status":"success",
        //   "value":[{"type":"text","text":"…"}]}}` — the result hangs off the
        // item itself. An older nesting put it under `toolResponse`, so both
        // are accepted; the golden corpus caught the missing outer form.
        if let Some(response) = item
            .get("toolResponse")
            .filter(|value| value.get("toolResult").is_some())
            .or_else(|| {
                (item.get("type").and_then(Value::as_str) == Some("toolResponse")
                    || item.get("toolResult").is_some())
                .then_some(item)
            })
        {
            transcript.push(turn);
            let mut result = Turn::new(Role::Tool);
            result.timestamp = timestamp;
            if let Some(block) = tool_result(response) {
                result.blocks.push(block);
            } else {
                transcript.unknown_records += 1;
                result.blocks.push(Block::Unknown { raw: item.clone() });
            }
            transcript.push(result);
            turn = Turn::new(role);
            turn.timestamp = timestamp;
            continue;
        }
        transcript.unknown_records += 1;
        turn.blocks.push(Block::Unknown { raw: item.clone() });
    }
    transcript.push(turn);
}

fn tool_call(request: &Value) -> Option<Block> {
    let call = request.get("toolCall")?;
    let payload = call.get("value").unwrap_or(call);
    let name = str_field(payload, "name")?;
    Some(Block::ToolCall {
        name,
        input: payload.get("arguments").cloned().unwrap_or(Value::Null),
    })
}

fn tool_result(response: &Value) -> Option<Block> {
    let result = response.get("toolResult")?;
    let ok = result.get("status").and_then(Value::as_str) != Some("error");
    let output = result
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| result.get("value").map(json_text))
        .unwrap_or_default();
    Some(Block::ToolResult {
        name: str_field(result, "name"),
        ok,
        output,
    })
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
    use crate::model::PathTrust;

    #[test]
    fn project_metadata_recovers_a_missing_header_workspace() {
        let temp = tempdir().expect("temporary store");
        let root = temp.path().join("goose");
        fs::create_dir_all(root.join("sessions")).expect("session directory");
        fs::write(
            root.join("projects.json"),
            r#"{"projects":{"demo":{"path":"/work/demo","last_session_id":"alpha"}}}"#,
        )
        .expect("projects metadata");
        fs::write(
            root.join("sessions/alpha.jsonl"),
            "{\"description\":\"Session title\",\"message_count\":1}\n",
        )
        .expect("session header");

        let source = Goose { root };
        let summaries = source.list().expect("list sessions");

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].recorded_cwd, Some(PathBuf::from("/work/demo")));
        assert_eq!(summaries[0].trust, PathTrust::Recorded);
    }

    #[test]
    fn unknown_record_is_retained_when_opening() {
        let temp = tempdir().expect("temporary store");
        let path = temp.path().join("session.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"working_dir\":\"/work/demo\",\"description\":\"Demo\"}\n",
                "{\"role\":\"user\",\"created\":1,\"content\":[{\"text\":\"hello\"}]}\n",
                "{\"future_record\":true}\n"
            ),
        )
        .expect("session records");

        let source = Goose {
            root: temp.path().join("goose"),
        };
        let transcript = source
            .open(&Locator::File { path })
            .expect("open transcript");

        assert_eq!(transcript.unknown_records, 1);
        assert!(matches!(
            transcript.turns.last().and_then(|turn| turn.blocks.first()),
            Some(Block::Unknown { .. })
        ));
    }

    #[test]
    fn tool_requests_and_responses_keep_their_distinct_roles() {
        let temp = tempdir().expect("temporary store");
        let path = temp.path().join("session.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"working_dir\":\"/work/demo\"}\n",
                "{\"role\":\"assistant\",\"content\":[{\"type\":\"toolRequest\",\"toolCall\":{\"value\":{\"name\":\"read_file\",\"arguments\":{\"path\":\"a.txt\"}}}}]}\n",
                "{\"role\":\"user\",\"content\":[{\"type\":\"toolResponse\",\"id\":\"t1\",\"toolResult\":{\"status\":\"success\",\"value\":[{\"type\":\"text\",\"text\":\"contents\"}]}}]}\n"
            ),
        )
        .expect("session records");

        let source = Goose {
            root: temp.path().join("goose"),
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
