//! Kilo: `~/.local/share/kilo/kilo.db`, with JSON storage mirrors as fallback.
//!
//! Kilo's `SQLite` schema is byte-identical to `OpenCode`'s, so its live database
//! is decoded by the shared reader. The inspected legacy mirror stores session
//! metadata in `storage/session/<project>/<session>.json` and message parts in
//! `storage/part/<message>/<part>.json`; it is used only when the live database
//! is absent, preventing the same sessions from being listed twice.
//!
//! Its inspected session metadata has no branch field; the shared reader
//! therefore preserves `SessionSummary::branch` as `None`.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::{Result, io};
use crate::model::{
    ArchiveOutcome, Block, Locator, Role, SessionSummary, SourceId, Transcript, Turn,
};
use crate::source::opencode_schema;
use crate::source::{
    RemoteLayout, Source, clean_title, file_locator, from_epoch_guess, mtime, read_head,
};

const LEGACY_RECORD_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub struct Kilo {
    db: PathBuf,
    storage: PathBuf,
}

impl Kilo {
    pub fn new(home: &Path) -> Self {
        let root = crate::data_home(home).join("kilo");
        Self {
            db: root.join("kilo.db"),
            storage: root.join("storage"),
        }
    }
}

impl Source for Kilo {
    fn id(&self) -> SourceId {
        SourceId::Kilo
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.db.clone(), self.storage.clone()]
    }

    fn list(&self) -> Result<Vec<SessionSummary>> {
        if self.db.exists() {
            opencode_schema::list(&self.db, self.id())
        } else {
            list_legacy(&self.storage)
        }
    }

    fn open(&self, locator: &Locator) -> Result<Transcript> {
        match locator {
            Locator::Row { db, id } => opencode_schema::open(db, self.id(), id),
            Locator::File { .. } => open_legacy(file_locator(self.id(), locator)?, &self.storage),
        }
    }

    fn archive(&self, locator: &Locator, archived: bool) -> Result<ArchiveOutcome> {
        match locator {
            Locator::Row { db, id } => opencode_schema::set_archived(db, id, archived),
            Locator::File { .. } => Ok(ArchiveOutcome::Unsupported),
        }
    }

    /// Kilo shares `OpenCode`'s schema, so the same remote query serves it. The
    /// legacy JSON `storage/` tree is not listable without a remote `llm-sessions`: one
    /// session's metadata is scattered across session, message and part files.
    fn remote_layout(&self) -> RemoteLayout {
        RemoteLayout {
            files: &[],
            sql: Some((
                ".local/share/kilo/kilo.db",
                opencode_schema::REMOTE_LIST_SQL,
            )),
        }
    }
}

fn list_legacy(storage: &Path) -> Result<Vec<SessionSummary>> {
    let sessions = storage.join("session");
    if !sessions.exists() {
        return Ok(Vec::new());
    }

    let mut summaries = Vec::new();
    for project in fs::read_dir(&sessions).map_err(io(&sessions))? {
        let project = project.map_err(io(&sessions))?;
        if !project.file_type().map_err(io(project.path()))?.is_dir() {
            continue;
        }
        for entry in fs::read_dir(project.path()).map_err(io(project.path()))? {
            let entry = entry.map_err(io(project.path()))?;
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let value = legacy_value(&path)?;
            let id = match value.get("id").and_then(Value::as_str) {
                Some(id) => id.to_owned(),
                None => continue,
            };
            let (fallback_updated, size) = mtime(&path)?;
            let created = timestamp(&value, "/time/created");
            let updated = timestamp(&value, "/time/updated").unwrap_or(fallback_updated);
            summaries.push(SessionSummary {
                recorded_cwd: value
                    .get("directory")
                    .and_then(Value::as_str)
                    .map(PathBuf::from),
                title: value
                    .get("title")
                    .and_then(Value::as_str)
                    .and_then(clean_title),
                created,
                model: value
                    .pointer("/model/modelID")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                parent: value
                    .get("parentID")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                archived: value
                    .pointer("/time/archived")
                    .is_some_and(|archived| !archived.is_null()),
                size_bytes: Some(size),
                ..SessionSummary::new(id.clone(), SourceId::Kilo, updated, Locator::File { path })
            });
        }
    }
    Ok(summaries)
}

fn open_legacy(path: &Path, storage: &Path) -> Result<Transcript> {
    let session = legacy_value(path)?;
    let id = session
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let mut transcript = Transcript::new(id.clone(), SourceId::Kilo);
    transcript.title = session
        .get("title")
        .and_then(Value::as_str)
        .and_then(clean_title);

    let messages = sorted_values(&storage.join("message").join(&id))?;
    for message in messages {
        let role_name = message.get("role").and_then(Value::as_str);
        #[expect(
            clippy::match_same_arms,
            reason = "an explicit system role and the unrecognised-role fallback are distinct cases; the latter is also recorded as unknown below"
        )]
        let role = match role_name {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            Some("system") => Role::System,
            Some("tool") => Role::Tool,
            _ => Role::System,
        };
        let mut turn = Turn::new(role);
        turn.timestamp = timestamp(&message, "/time/created");
        turn.model = message
            .get("modelID")
            .and_then(Value::as_str)
            .or_else(|| message.pointer("/model/modelID").and_then(Value::as_str))
            .map(str::to_owned);
        if !matches!(role_name, Some("user" | "assistant" | "system" | "tool")) {
            transcript.unknown_records += 1;
            turn.blocks.push(Block::Unknown {
                raw: message.clone(),
            });
        }

        let Some(message_id) = message.get("id").and_then(Value::as_str) else {
            transcript.unknown_records += 1;
            turn.blocks.push(Block::Unknown { raw: message });
            transcript.push(turn);
            continue;
        };
        for part in sorted_values(&storage.join("part").join(message_id))? {
            append_legacy_part(&mut turn, part, &mut transcript.unknown_records);
        }
        transcript.push(turn);
    }
    Ok(transcript)
}

fn legacy_value(path: &Path) -> Result<Value> {
    let bytes = read_head(path, LEGACY_RECORD_BYTES)?;
    Ok(serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned())))
}

fn sorted_values(directory: &Path) -> Result<Vec<Value>> {
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut values = Vec::new();
    for entry in fs::read_dir(directory).map_err(io(directory))? {
        let entry = entry.map_err(io(directory))?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("json") {
            values.push(legacy_value(&path)?);
        }
    }
    values.sort_by_key(|value| {
        value
            .pointer("/time/created")
            .and_then(Value::as_i64)
            .unwrap_or_default()
    });
    Ok(values)
}

fn append_legacy_part(turn: &mut Turn, part: Value, unknown_records: &mut usize) {
    match part.get("type").and_then(Value::as_str) {
        Some("text") => match part.get("text").and_then(Value::as_str) {
            Some(text) => turn.blocks.push(Block::text(text)),
            None => unknown(turn, part, unknown_records),
        },
        Some("reasoning") => match part.get("text").and_then(Value::as_str) {
            Some(text) => turn.blocks.push(Block::thinking(text)),
            None => unknown(turn, part, unknown_records),
        },
        Some("tool") => {
            let name = part
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_owned();
            let state = part.get("state").cloned().unwrap_or(Value::Null);
            turn.blocks.push(Block::ToolCall {
                name: name.clone(),
                input: state.get("input").cloned().unwrap_or(Value::Null),
            });
            if let Some(status) = state.get("status").and_then(Value::as_str)
                && (status == "completed" || status == "error")
            {
                turn.blocks.push(Block::ToolResult {
                    name: Some(name),
                    ok: status == "completed",
                    output: state
                        .get(if status == "completed" {
                            "output"
                        } else {
                            "error"
                        })
                        .or_else(|| state.get("output"))
                        .map(json_display)
                        .unwrap_or_default(),
                });
            }
        }
        Some("file") => match part
            .get("path")
            .and_then(Value::as_str)
            .or_else(|| part.pointer("/source/path").and_then(Value::as_str))
            .or_else(|| part.get("filename").and_then(Value::as_str))
        {
            Some(path) => turn.blocks.push(Block::File {
                path: PathBuf::from(path),
            }),
            None => unknown(turn, part, unknown_records),
        },
        Some("patch") => turn.blocks.push(Block::Patch {
            summary: part
                .get("hash")
                .and_then(Value::as_str)
                .unwrap_or("patch")
                .to_owned(),
            files: opencode_schema::patch_files(&part),
        }),
        Some("step-start" | "step-finish") => {}
        _ => unknown(turn, part, unknown_records),
    }
}

fn unknown(turn: &mut Turn, raw: Value, unknown_records: &mut usize) {
    *unknown_records += 1;
    turn.blocks.push(Block::Unknown { raw });
}

fn timestamp(value: &Value, pointer: &str) -> Option<time::OffsetDateTime> {
    value.pointer(pointer)?.as_i64().and_then(from_epoch_guess)
}

fn json_display(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}
