//! Cursor CLI transcripts under `~/.cursor/projects/*/agent-transcripts/`.
//!
//! Cursor's GUI stores conversations in opaque `SQLite` blobs (`state.vscdb` and
//! per-chat `store.db` files); its searchable database has metadata only, so GUI
//! conversations are deliberately excluded until their body format is understood.
//! `acp-sessions/*/meta.json` likewise records a cwd but no conversation body,
//! and is not listed: a session row must always be openable into a transcript.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::model::{Block, Locator, PathTrust, Role, SessionSummary, SourceId, Transcript, Turn};
use crate::source::{
    RemoteLayout, Source, count_unknown, file_locator, mtime, parse_rfc3339, push_unknown,
    read_head, read_records, str_field,
};

const REPO_METADATA_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub struct Cursor {
    root: PathBuf,
}

impl Cursor {
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join(".cursor/projects"),
        }
    }
}

impl Source for Cursor {
    fn id(&self) -> SourceId {
        SourceId::Cursor
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

            let project_dir = project.path();
            let workspace = project_workspace(&project_dir)?;
            let mut transcripts = Vec::new();
            collect_transcripts(&project_dir.join("agent-transcripts"), &mut transcripts);
            for path in transcripts {
                let Some(id) = transcript_id(&path) else {
                    continue;
                };
                let (updated, size) = mtime(&path)?;
                let (recorded_cwd, trust) = match &workspace {
                    Some(path) => (Some(path.clone()), PathTrust::Recorded),
                    // A present `repo.json` without a usable `folder` is not
                    // permission to guess. The flattened directory is only a
                    // fallback when Cursor did not write repo metadata at all.
                    None if project_dir.join("repo.json").exists() => (None, PathTrust::Recorded),
                    None => (decode_project_dir(&project_dir), PathTrust::Decoded),
                };

                out.push(SessionSummary {
                    recorded_cwd,
                    trust,
                    size_bytes: Some(size),
                    ..SessionSummary::new(id, SourceId::Cursor, updated, Locator::File { path })
                });
            }
        }
        Ok(out)
    }

    fn open(&self, locator: &Locator) -> Result<Transcript> {
        let path = file_locator(self.id(), locator)?;
        let mut transcript = Transcript::new(transcript_id(path).unwrap_or_default(), self.id());
        transcript.workspace =
            project_dir(path).and_then(|dir| project_workspace(dir).ok().flatten());

        for record in read_records(path)? {
            let Some(role) = record
                .get("role")
                .and_then(serde_json::Value::as_str)
                .and_then(parse_role)
            else {
                push_unknown(&mut transcript, record);
                continue;
            };

            let mut turn = Turn::new(role);
            turn.timestamp = str_field(&record, "timestamp")
                .as_deref()
                .and_then(parse_rfc3339);
            let Some(content) = record
                .get("message")
                .and_then(|message| message.get("content"))
                .and_then(serde_json::Value::as_array)
            else {
                count_unknown(&mut transcript, &mut turn, record);
                transcript.push(turn);
                continue;
            };

            for block in content {
                match parse_block(block) {
                    Some(block) => turn.blocks.push(block),
                    None => count_unknown(&mut transcript, &mut turn, block.clone()),
                }
            }
            transcript.push(turn);
        }

        Ok(transcript)
    }

    fn resume_command(&self, id: &str) -> Option<Vec<String>> {
        // Verified from `cursor-agent --help`: `--resume [chatId]`.
        Some(vec!["cursor-agent".into(), "--resume".into(), id.into()])
    }

    /// The shell transport finds transcripts written straight into
    /// `agent-transcripts/` and the per-session directories beside them.
    fn remote_layout(&self) -> RemoteLayout {
        RemoteLayout {
            files: &[
                ".cursor/projects/*/agent-transcripts/*.jsonl",
                ".cursor/projects/*/agent-transcripts/*/*.jsonl",
            ],
            sql: None,
        }
    }
}

/// Walk only Cursor's transcript subtree. Project directories also contain
/// terminal logs, MCP caches, and credentials, none of which are conversations.
fn collect_transcripts(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => collect_transcripts(&path, out),
            Ok(_)
                if path
                    .extension()
                    .is_some_and(|extension| extension == "jsonl") =>
            {
                out.push(path);
            }
            _ => {}
        }
    }
}

fn transcript_id(path: &Path) -> Option<String> {
    path.file_stem()?.to_str().map(str::to_owned)
}

fn project_dir(transcript: &Path) -> Option<&Path> {
    transcript
        .ancestors()
        .find(|ancestor| {
            ancestor
                .file_name()
                .is_some_and(|name| name == "agent-transcripts")
        })?
        .parent()
}

fn project_workspace(project_dir: &Path) -> Result<Option<PathBuf>> {
    let repo = project_dir.join("repo.json");
    if !repo.exists() {
        return Ok(None);
    }
    let bytes = read_head(&repo, REPO_METADATA_BYTES)?;
    let Ok(metadata) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Ok(None);
    };
    Ok(metadata
        .get("folder")
        .and_then(serde_json::Value::as_str)
        .and_then(file_uri_path))
}

/// Parse Cursor's `repo.json` URI without treating `+` as a space as form
/// decoders do. Cursor records local `file://` URIs, not remote URI authorities.
fn file_uri_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let encoded_path = if rest.starts_with('/') {
        rest.to_owned()
    } else {
        format!("/{}", rest.strip_prefix("localhost/")?)
    };
    let encoded_path = encoded_path.split(['?', '#']).next()?;
    let mut bytes = Vec::with_capacity(encoded_path.len());
    let raw = encoded_path.as_bytes();
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == b'%' {
            let high = *raw.get(index + 1)?;
            let low = *raw.get(index + 2)?;
            bytes.push((hex_digit(high)? << 4) | hex_digit(low)?);
            index += 3;
        } else {
            bytes.push(raw[index]);
            index += 1;
        }
    }
    String::from_utf8(bytes).ok().map(PathBuf::from)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Display-only reversal of Cursor's `/`→`-` project naming. It is inherently
/// ambiguous (`/tmp/a-b` and `/tmp/a/b` collide), so callers mark it Decoded.
fn decode_project_dir(dir: &Path) -> Option<PathBuf> {
    let name = dir.file_name()?.to_str()?;
    let body = name.strip_prefix('-').unwrap_or(name);
    Some(PathBuf::from(format!("/{}", body.replace('-', "/"))))
}

fn parse_role(role: &str) -> Option<Role> {
    match role {
        "user" => Some(Role::User),
        "assistant" => Some(Role::Assistant),
        "system" => Some(Role::System),
        "tool" => Some(Role::Tool),
        _ => None,
    }
}

fn parse_block(block: &serde_json::Value) -> Option<Block> {
    match block.get("type")?.as_str()? {
        "text" => str_field(block, "text").map(Block::text),
        "thinking" => str_field(block, "text").map(Block::thinking),
        "tool_use" => Some(Block::ToolCall {
            name: str_field(block, "name")?,
            input: block.get("input")?.clone(),
        }),
        "tool_result" => Some(Block::ToolResult {
            name: str_field(block, "name"),
            ok: !block
                .get("is_error")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            output: str_field(block, "content").or_else(|| str_field(block, "output"))?,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn write_transcript(root: &Path, project: &str, session: &str, contents: &str) -> PathBuf {
        let path = root
            .join(project)
            .join("agent-transcripts")
            .join(session)
            .join(format!("{session}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn file_uri_decodes_percent_encoded_path() {
        assert_eq!(
            file_uri_path("file:///work/with%20space/%E2%9C%93"),
            Some(PathBuf::from("/work/with space/✓"))
        );
        assert_eq!(file_uri_path("file:///bad%2"), None);
    }

    #[test]
    fn discovers_nested_transcript_and_uses_repo_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("projects");
        let session = "c4ad4dc8-a1af-4c93-ac9c-02ec5d3e6a21";
        let project = "lossy-project-name";
        let transcript = write_transcript(
            &root,
            project,
            session,
            r#"{"role":"user","message":{"content":[{"type":"text","text":"hello"}]}}"#,
        );
        fs::write(
            root.join(project).join("repo.json"),
            r#"{"folder":"file:///work/with%20space"}"#,
        )
        .unwrap();

        let summaries = Cursor { root }.list().unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, session);
        assert_eq!(
            summaries[0].recorded_cwd,
            Some(PathBuf::from("/work/with space"))
        );
        assert_eq!(summaries[0].trust, PathTrust::Recorded);
        assert_eq!(summaries[0].locator, Locator::File { path: transcript });
    }

    #[test]
    fn missing_repo_metadata_uses_decoded_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("projects");
        write_transcript(
            &root,
            "home-alice-work-project",
            "12345678-1234-1234-1234-123456789abc",
            r#"{"role":"user","message":{"content":[]}}"#,
        );

        let summaries = Cursor { root }.list().unwrap();
        assert_eq!(
            summaries[0].recorded_cwd,
            Some(PathBuf::from("/home/alice/work/project"))
        );
        assert_eq!(summaries[0].trust, PathTrust::Decoded);
    }

    #[test]
    fn unknown_record_is_preserved_and_counted() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("projects");
        let path = write_transcript(
            &root,
            "project",
            "12345678-1234-1234-1234-123456789abc",
            r#"{"role":"future","message":{"content":[]}}"#,
        );
        let transcript = Cursor { root }.open(&Locator::File { path }).unwrap();

        assert_eq!(transcript.unknown_records, 1);
        assert!(matches!(
            &transcript.turns[0].blocks[0],
            Block::Unknown { .. }
        ));
    }
}
