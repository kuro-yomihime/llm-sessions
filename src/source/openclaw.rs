//! `OpenClaw`: `~/.openclaw/agents/<agentId>/sessions/<sessionId>.jsonl`.
//!
//! No store exists on the machine this was written on — only `skills/` — so
//! the adapter follows the documented layout and is proved by the golden
//! corpus. Two transcript locations exist: the per-agent tree and the legacy
//! flat `~/.openclaw/sessions/`; when one session id appears in both the
//! per-agent copy wins, because the legacy key is a chat handle rather than
//! the session's own id.
//!
//! Sessions are per-chat, not per-directory: no documented field records a
//! cwd, so [`SessionSummary::recorded_cwd`] stays empty and rows group under
//! `(unattributed)` rather than inheriting a guess. Credentials in the same
//! tree (`auth.json`, lock files) are never opened — only `.jsonl` is read.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::Result;
use crate::model::{Locator, Role, SessionSummary, SourceId, Transcript, Turn, Usage};
use crate::source::{
    RemoteLayout, Source, clean_title, file_locator, mtime, parse_rfc3339, push_unknown,
    read_records,
};

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

    /// One metadata index, read at most once per `list()`.
    ///
    /// The index maps `sessionKey -> { sessionId, label, model, inputTokens,
    /// outputTokens, … }`; only entries whose `sessionId` matches a transcript
    /// stem are used, so a stale entry costs nothing.
    fn index(dir: &Path) -> HashMap<String, Value> {
        let Ok(raw) = std::fs::read_to_string(dir.join("sessions.json")) else {
            return HashMap::new();
        };
        let Ok(Value::Object(entries)) = serde_json::from_str(&raw) else {
            return HashMap::new();
        };
        let mut by_session_id = HashMap::new();
        for entry in entries.into_values() {
            if let Some(id) = entry.get("sessionId").and_then(Value::as_str) {
                by_session_id.insert(id.to_owned(), entry);
            }
        }
        by_session_id
    }
}

impl Source for OpenClaw {
    fn id(&self) -> SourceId {
        SourceId::OpenClaw
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.agents.clone(), self.legacy.clone()]
    }

    fn list(&self) -> Result<Vec<SessionSummary>> {
        let mut out: Vec<SessionSummary> = Vec::new();
        // Per-agent first: its ids win over a legacy copy of the same chat.
        let agent_dirs: Vec<PathBuf> = std::fs::read_dir(&self.agents)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                    .map(|entry| entry.path().join("sessions"))
                    .collect()
            })
            .unwrap_or_default();
        for sessions_dir in agent_dirs.iter().chain(std::iter::once(&self.legacy)) {
            let index = Self::index(sessions_dir);
            let Ok(files) = std::fs::read_dir(sessions_dir) else {
                continue;
            };
            for entry in files.flatten() {
                let path = entry.path();
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
                if id.is_empty() || out.iter().any(|row| row.id == id) {
                    continue;
                }
                let (updated, size) = mtime(&path)?;
                let entry = index.get(&id);
                // The index's `updatedAt` is a cross-check only: ordering by
                // the transcript's own mtime cannot disagree with what was
                // actually written last.
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
                        // OpenClaw records token counts, not money.
                        cost: 0.0,
                    }),
                    size_bytes: Some(size),
                    ..SessionSummary::new(
                        id.clone(),
                        SourceId::OpenClaw,
                        updated,
                        Locator::File { path },
                    )
                });
            }
        }
        Ok(out)
    }

    fn open(&self, locator: &Locator) -> Result<Transcript> {
        let path = file_locator(SourceId::OpenClaw, locator)?;
        let id = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut transcript = Transcript::new(id, SourceId::OpenClaw);

        for record in read_records(path)? {
            let Some(message) = record.get("message") else {
                // A line the adapter does not recognise is kept raw and
                // counted, so a format shift never fails the session.
                push_unknown(&mut transcript, record);
                continue;
            };
            let mut turn = Turn::new(match message.get("role").and_then(Value::as_str) {
                Some("user") => Role::User,
                Some("assistant") => Role::Assistant,
                Some("system") => Role::System,
                Some("tool") => Role::Tool,
                _ => {
                    push_unknown(&mut transcript, record);
                    continue;
                }
            });
            turn.timestamp = record
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_rfc3339);
            match message.get("content") {
                Some(Value::String(text)) => turn.blocks.push(crate::model::Block::text(text)),
                Some(Value::Array(blocks)) => {
                    for block in blocks {
                        append_block(&mut turn, &mut transcript.unknown_records, block);
                    }
                }
                _ => {}
            }
            transcript.push(turn);
        }
        Ok(transcript)
    }

    /// unverified — confirm first: `OpenClaw` sessions are chat-bound and the
    /// documentation names no CLI flag that resumes one from a terminal.
    fn resume_command(&self, _id: &str) -> Option<Vec<String>> {
        None
    }

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
        "text" => turn.blocks.push(crate::model::Block::text(text)),
        "thinking" | "reasoning" => {
            turn.blocks.push(crate::model::Block::thinking(text));
        }
        "tool_use" => turn.blocks.push(crate::model::Block::ToolCall {
            name: block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_owned(),
            input: block.get("input").cloned().unwrap_or(Value::Null),
        }),
        "tool_result" => turn.blocks.push(crate::model::Block::ToolResult {
            name: block.get("name").and_then(Value::as_str).map(str::to_owned),
            ok: !block
                .get("is_error")
                .is_some_and(|flag| flag == &Value::Bool(true)),
            output: flatten_text(block.get("content")),
        }),
        "image" => turn.blocks.push(crate::model::Block::Image {
            label: text.to_owned(),
        }),
        _ => {
            *unknown_records += 1;
            turn.blocks
                .push(crate::model::Block::Unknown { raw: block.clone() });
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
}
