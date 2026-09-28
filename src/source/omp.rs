//! OMP: `~/.omp/agent/sessions/<bucket>/<timestamp>_<uuid>.jsonl`.
//!
//! The bucket directory encodes the cwd lossily; the `type:"session"` header
//! carries the real one, so only the header is trusted.
//!
//! Each session may own a sibling *directory* named after the session file,
//! holding subagent transcripts. Those children are read as children only:
//! the top-level bucket scan never mistakes them for sessions of their own.
//! A measured artifact is `…/2026-06-02T12-07-49-367Z_019e883b-…/1-FeaturesResearch.jsonl`.
//! Inspected OMP headers record `cwd` but no branch identity, so `branch`
//! intentionally remains `None` rather than shelling out to Git.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::error::{Result, io};
use crate::model::{
    Block, FileChange, Locator, Role, SessionSummary, SourceId, Transcript, Turn, Usage,
};
use crate::source::{
    RemoteLayout, Source, clean_title, file_locator, flatten_text, head_records, mtime,
    parse_rfc3339, read_head, read_records, str_field,
};

/// Enough for the optional 256-byte title slot plus the header line.
const HEAD_BYTES: usize = 8 * 1024;

#[derive(Debug)]
pub struct Omp {
    root: PathBuf,
}

impl Omp {
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join(".omp/agent/sessions"),
        }
    }
}

impl Source for Omp {
    fn id(&self) -> SourceId {
        SourceId::Omp
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn list(&self) -> Result<Vec<SessionSummary>> {
        let Ok(buckets) = std::fs::read_dir(&self.root) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for bucket in buckets.flatten() {
            if !bucket.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let dir = bucket.path();
            let mut artifacts = Vec::new();
            let mut files = HashSet::new();
            for entry in std::fs::read_dir(&dir).map_err(io(&dir))?.flatten() {
                let path = entry.path();
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
                    files.insert(path.clone());
                    if let Some(summary) = read_summary(&path)? {
                        out.push(summary);
                    }
                } else if file_type.is_dir() {
                    artifacts.push(path);
                }
            }
            for artifact in artifacts {
                let children = read_subagents(&artifact)?;
                // A parent that has not been written yet is not a parent that
                // does not exist: OMP flushes the conversation on exit, so a
                // running session can own subagent artifacts while its own
                // file is still only in the agent's memory. Without this row
                // its children would float as top-level sessions and the
                // instance would have no place in the tree.
                if !files.contains(&transcript_path(&artifact))
                    && let Some(parent) = pending_parent(&artifact, &children)
                {
                    out.push(parent);
                }
                out.extend(children);
            }
        }
        Ok(out)
    }

    fn open(&self, locator: &Locator) -> Result<Transcript> {
        let path = file_locator(SourceId::Omp, locator)?;
        if !path.exists() {
            return pending_transcript(path);
        }
        read_transcript(path)
    }

    fn resume_command(&self, id: &str) -> Option<Vec<String>> {
        Some(vec!["omp".into(), "--resume".into(), id.into()])
    }

    /// `-p` answers and exits; `--` keeps a message that starts with `-`
    /// from reading as a flag (checked against omp 18.2).
    fn insert_command(&self, id: &str, message: &str) -> Option<Vec<String>> {
        Some(vec![
            "omp".into(),
            "--print".into(),
            "--resume".into(),
            id.into(),
            "--".into(),
            message.into(),
        ])
    }

    /// The shell transport finds session files in the bucket directories, and
    /// subagent artifacts in the directory that sits beside each session file.
    fn remote_layout(&self) -> RemoteLayout {
        RemoteLayout {
            files: &[
                ".omp/agent/sessions/*/*.jsonl",
                ".omp/agent/sessions/*/*/*.jsonl",
            ],
            sql: None,
        }
    }
}

fn read_summary(path: &Path) -> Result<Option<SessionSummary>> {
    let head = read_head(path, HEAD_BYTES)?;
    // A physical 256-byte `type:"title"` slot may precede the logical header.
    let mut slot_title = None;
    let mut header = None;
    for record in head_records(&head) {
        match record.get("type").and_then(serde_json::Value::as_str) {
            Some("title") => slot_title = str_field(&record, "title"),
            Some("session") => {
                header = Some(record);
                break;
            }
            _ => {}
        }
    }
    let Some(header) = header else {
        return Ok(None);
    };

    let (updated, size) = mtime(path)?;
    let id = str_field(&header, "id").unwrap_or_else(|| stem(path));
    Ok(Some(SessionSummary {
        recorded_cwd: str_field(&header, "cwd").map(PathBuf::from),
        title: str_field(&header, "title")
            .or(slot_title)
            .as_deref()
            .and_then(clean_title),
        created: str_field(&header, "timestamp")
            .as_deref()
            .and_then(parse_rfc3339),
        parent: str_field(&header, "parentSession"),
        size_bytes: Some(size),
        ..SessionSummary::new(
            id,
            SourceId::Omp,
            updated,
            Locator::File {
                path: path.to_path_buf(),
            },
        )
    }))
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn read_subagents(artifact_dir: &Path) -> Result<Vec<SessionSummary>> {
    let Some(parent) = artifact_parent_id(artifact_dir) else {
        return Ok(Vec::new());
    };
    let entries = std::fs::read_dir(artifact_dir).map_err(io(artifact_dir))?;
    let mut children = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|type_| type_.is_file())
            || path.extension().and_then(|extension| extension.to_str()) != Some("jsonl")
        {
            continue;
        }
        if let Some(mut summary) = read_summary(&path)? {
            summary.parent = Some(parent.clone());
            summary.subagent = true;
            summary.title = clean_title(&subagent_title(&path));
            children.push(summary);
        }
    }
    Ok(children)
}

fn artifact_parent_id(artifact_dir: &Path) -> Option<String> {
    let name = artifact_dir.file_name()?.to_str()?;
    let (_, id) = name.rsplit_once('_')?;
    (id.len() == 36
        && id
            .chars()
            .all(|character| character.is_ascii_hexdigit() || character == '-'))
    .then(|| id.to_owned())
}

fn subagent_title(path: &Path) -> String {
    let name = stem(path);
    name.split_once('-')
        .filter(|(number, title)| {
            !number.is_empty()
                && number.chars().all(|character| character.is_ascii_digit())
                && !title.is_empty()
        })
        .map(|(_, title)| title.to_owned())
        .unwrap_or(name)
}

/// The session file an artifact directory belongs to.
///
/// The directory name is the file's stem, so this is an append, never an
/// extension swap: `…_019e883b-…` has no extension to replace.
fn transcript_path(artifact_dir: &Path) -> PathBuf {
    let mut name = artifact_dir.as_os_str().to_owned();
    name.push(".jsonl");
    PathBuf::from(name)
}

/// A session known only by the artifact directory it owns.
///
/// Everything credible comes from the children: their headers record the same
/// `cwd`, and their mtimes say when the conversation last moved. The id comes
/// from the directory name, which is where OMP put it. No children means no
/// evidence, and therefore no row.
fn pending_parent(artifact_dir: &Path, children: &[SessionSummary]) -> Option<SessionSummary> {
    let id = artifact_parent_id(artifact_dir)?;
    let updated = children.iter().map(|child| child.updated).max()?;
    Some(SessionSummary {
        recorded_cwd: children.iter().find_map(|child| child.recorded_cwd.clone()),
        created: artifact_started(artifact_dir),
        size_bytes: Some(children.iter().filter_map(|child| child.size_bytes).sum()),
        ..SessionSummary::new(
            id,
            SourceId::Omp,
            updated,
            Locator::File {
                path: transcript_path(artifact_dir),
            },
        )
    })
}

/// `2026-09-15T10-52-57-998Z_<uuid>` — OMP's filename stamp, which is RFC 3339
/// with the time separators replaced so it can be a filename.
fn artifact_started(artifact_dir: &Path) -> Option<time::OffsetDateTime> {
    let name = artifact_dir.file_name()?.to_str()?;
    let (stamp, _) = name.rsplit_once('_')?;
    let (date, clock) = stamp.split_once('T')?;
    let mut parts = clock.trim_end_matches('Z').splitn(4, '-');
    let (hour, minute, second, millis) = (
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next().unwrap_or("0"),
    );
    parse_rfc3339(&format!("{date}T{hour}:{minute}:{second}.{millis}Z"))
}

/// What a session whose file does not exist yet can honestly show.
///
/// The conversation is in the running agent's memory, not on disk. The one
/// thing this session provably did is spawn the subagents beside it, so they
/// are offered as a `task` call — which the reader already turns into
/// followable links into each child transcript.
fn pending_transcript(path: &Path) -> Result<Transcript> {
    let artifact_dir = path.with_extension("");
    let children = read_subagents(&artifact_dir)?;
    let mut transcript = Transcript::new(
        artifact_parent_id(&artifact_dir).unwrap_or_else(|| stem(path)),
        SourceId::Omp,
    );
    transcript.workspace = children.iter().find_map(|child| child.recorded_cwd.clone());

    let mut turn = Turn::new(Role::System);
    turn.timestamp = artifact_started(&artifact_dir);
    turn.blocks.push(Block::text(
        "This session is still in the running agent's memory: OMP writes its \
         transcript when the session ends. Its subagent transcripts are on \
         disk already.",
    ));
    if !children.is_empty() {
        turn.blocks.push(Block::ToolCall {
            name: "task".into(),
            input: serde_json::json!({
                "tasks": children
                    .iter()
                    .map(|child| serde_json::json!({"name": child.display_title()}))
                    .collect::<Vec<_>>(),
            }),
        });
    }
    transcript.push(turn);
    Ok(transcript)
}

/// Record types that carry session state rather than conversation. Skipped
/// silently: surfacing them as `Unknown` would bury the actual dialogue under
/// hundreds of `model_change` and `mode_change` rows (measured: 209 and 436
/// across this machine's 46 sessions).
const STATE_RECORDS: &[&str] = &[
    "session",
    "title",
    "title_change",
    "model_change",
    "thinking_level_change",
    "mode_change",
    "service_tier_change",
    "credential_pin",
    "model_usage",
    "ttsr_injection",
    "session_init",
    "reset_boundary",
    "label",
    "custom",
];

fn read_transcript(path: &Path) -> Result<Transcript> {
    let mut transcript = Transcript::new(String::new(), SourceId::Omp);
    for record in read_records(path)? {
        match record.get("type").and_then(serde_json::Value::as_str) {
            Some("session") => {
                transcript.id = str_field(&record, "id").unwrap_or_default();
                transcript.title = str_field(&record, "title").as_deref().and_then(clean_title);
                transcript.workspace = str_field(&record, "cwd").map(PathBuf::from);
            }
            Some("message") => transcript.push(message_turn(&record)),
            Some("compaction") => {
                let mut turn = Turn::new(Role::System);
                turn.timestamp = str_field(&record, "timestamp")
                    .as_deref()
                    .and_then(parse_rfc3339);
                let summary = str_field(&record, "summary").unwrap_or_default();
                turn.blocks
                    .push(Block::text(format!("context compacted\n\n{summary}")));
                transcript.push(turn);
            }
            // Harness notices the user actually saw; `display: false` ones are
            // bookkeeping.
            Some("custom_message")
                if record
                    .get("display")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false) =>
            {
                let mut turn = Turn::new(Role::System);
                turn.timestamp = str_field(&record, "timestamp")
                    .as_deref()
                    .and_then(parse_rfc3339);
                if let Some(text) = str_field(&record, "content") {
                    turn.blocks.push(Block::text(text));
                }
                transcript.push(turn);
            }
            Some("custom_message") => {}
            Some(other) if STATE_RECORDS.contains(&other) => {}
            _ => {
                transcript.unknown_records += 1;
                let mut turn = Turn::new(Role::System);
                turn.blocks.push(Block::Unknown { raw: record });
                transcript.push(turn);
            }
        }
    }
    // A header-less file still has a usable identity: its filename.
    if transcript.id.is_empty() {
        transcript.id = stem(path);
    }
    Ok(transcript)
}

/// OMP stores per-message accounting on assistant envelopes. A measured record
/// is `{"message":{"role":"assistant","usage":{"input":4,"output":354,
/// "cost":{"total":0.195195}}}}`; cache fields intentionally have no slot in
/// [`Usage`].
fn message_turn(record: &serde_json::Value) -> Turn {
    let message = record.get("message").unwrap_or(record);
    let role_name = str_field(message, "role").unwrap_or_default();
    let mut turn = Turn::new(role_of(&role_name));
    turn.timestamp = str_field(message, "timestamp")
        .or_else(|| str_field(record, "timestamp"))
        .as_deref()
        .and_then(parse_rfc3339);
    turn.model = str_field(message, "model");
    if turn.role == Role::Assistant {
        turn.usage = message.get("usage").and_then(omp_usage);
    }

    // A tool result is a whole message, not a content block: the outcome lives
    // on the envelope (`toolName`, `isError`) and the payload in `content`.
    if role_name == "toolResult" {
        turn.blocks.push(Block::ToolResult {
            name: str_field(message, "toolName"),
            ok: !message
                .get("isError")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            output: flatten_text(message.get("content")),
        });
        return turn;
    }

    match message.get("content") {
        Some(serde_json::Value::String(text)) => turn.blocks.push(Block::text(text.clone())),
        Some(serde_json::Value::Array(blocks)) => {
            for block in blocks {
                let rendered = content_block(block);
                let patch = match &rendered {
                    Block::ToolCall { name, input } => omp_patch_block(name, input),
                    _ => None,
                };
                turn.blocks.push(rendered);
                if let Some(patch) = patch {
                    turn.blocks.push(patch);
                }
            }
        }
        _ => {}
    }
    turn
}

fn omp_usage(value: &serde_json::Value) -> Option<Usage> {
    let cost = value.pointer("/cost/total")?.as_f64()?;
    cost.is_finite().then(|| Usage {
        input: value
            .get("input")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        output: value
            .get("output")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        cost,
    })
}

fn content_block(block: &serde_json::Value) -> Block {
    match block.get("type").and_then(serde_json::Value::as_str) {
        Some("text") => Block::text(str_field(block, "text").unwrap_or_default()),
        Some("thinking") => Block::thinking(str_field(block, "thinking").unwrap_or_default()),
        Some("toolCall") => Block::ToolCall {
            name: str_field(block, "name").unwrap_or_default(),
            input: block.get("arguments").cloned().unwrap_or_default(),
        },
        // Image payloads are `blob:sha256:…` references into
        // `~/.omp/agent/blobs`; the label is all a terminal can show.
        Some("image") => Block::Image {
            label: str_field(block, "mimeType").unwrap_or_else(|| "image".to_string()),
        },
        _ => Block::Unknown { raw: block.clone() },
    }
}

/// OMP's measured `write` call uses `{"path":"/home/…/oauth.ts"}`, while
/// `edit` carries `{"input":"*** Begin Patch\n[/home/…/types.ts#E644]…"}`.
/// Only these explicit path positions count as changes.
fn omp_patch_block(name: &str, input: &serde_json::Value) -> Option<Block> {
    let files = match name {
        "write" => input
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(file_change)
            .into_iter()
            .collect(),
        "edit" => input
            .get("input")
            .and_then(serde_json::Value::as_str)
            .map(omp_edit_paths)
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    (!files.is_empty()).then(|| Block::Patch {
        summary: name.to_owned(),
        files,
    })
}

fn omp_edit_paths(input: &str) -> Vec<FileChange> {
    input
        .lines()
        .filter_map(|line| {
            let header = line.trim_start().strip_prefix('[')?;
            let path = header.split_once('#')?.0;
            (!path.is_empty()).then_some(path)
        })
        .map(file_change)
        .collect()
}

fn file_change(path: &str) -> FileChange {
    FileChange {
        path: PathBuf::from(path),
        added: 0,
        deleted: 0,
    }
}

fn role_of(name: &str) -> Role {
    match name {
        "assistant" => Role::Assistant,
        "toolResult" => Role::Tool,
        "developer" => Role::System,
        // `fileMention`, `bashExecution`, `pythonExecution` are user-side
        // injections into the conversation.
        _ => Role::User,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn transcript_of(lines: &[&str]) -> Transcript {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2026-01-01T00-00-00-000Z_x.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        drop(file);
        read_transcript(&path).unwrap()
    }

    #[test]
    fn artifact_directory_yields_child_without_listing_drafts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        let bucket = root.join("bucket");
        std::fs::create_dir_all(&bucket).unwrap();
        let parent = "019e883b-9f77-7000-9e36-27694c4ef2a9";
        let stem = format!("2026-06-02T12-07-49-367Z_{parent}");
        std::fs::write(
            bucket.join(format!("{stem}.jsonl")),
            format!(r#"{{"type":"session","id":"{parent}","cwd":"/workspace"}}"#),
        )
        .unwrap();
        let artifact = bucket.join(&stem);
        std::fs::create_dir(&artifact).unwrap();
        std::fs::write(
            artifact.join("1-FeaturesResearch.jsonl"),
            r#"{"type":"session","id":"child","cwd":"/workspace"}"#,
        )
        .unwrap();
        std::fs::write(artifact.join("draft.txt"), "not a transcript").unwrap();

        let sessions = Omp { root }.list().unwrap();
        assert_eq!(sessions.len(), 2);
        let child = sessions
            .iter()
            .find(|summary| summary.id == "child")
            .unwrap();
        assert!(child.subagent);
        assert_eq!(child.parent.as_deref(), Some(parent));
        assert_eq!(child.title.as_deref(), Some("FeaturesResearch"));
    }

    /// Measured: a running instance had written `CstmBranchScout.jsonl` but
    /// not its own transcript — OMP flushes that on exit. The session exists,
    /// so it gets a row and its children hang under it, instead of three
    /// orphans floating at the top level.
    #[test]
    fn a_session_whose_file_is_not_written_yet_still_gets_a_row() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        let bucket = root.join("-Documents-TREE-thing");
        let parent = "01a0a4b2-d30e-77da-923c-c1ec4cbdeb61";
        let artifact = bucket.join(format!("2026-09-15T10-52-57-998Z_{parent}"));
        std::fs::create_dir_all(&artifact).unwrap();
        for task in ["CstmBranchScout", "CstmStateScout"] {
            std::fs::write(
                artifact.join(format!("{task}.jsonl")),
                format!(r#"{{"type":"session","id":"{task}","cwd":"/work/thing"}}"#),
            )
            .unwrap();
        }

        let sessions = Omp { root: root.clone() }.list().unwrap();
        let row = sessions
            .iter()
            .find(|summary| summary.id == parent)
            .expect("the parent exists, so it is listed");
        assert!(!row.subagent);
        // The children record the cwd; the bucket name is never decoded.
        assert_eq!(row.recorded_cwd.as_deref(), Some(Path::new("/work/thing")));
        assert_eq!(
            row.created.map(|at| at.to_string()),
            Some("2026-09-15 10:52:57.998 +00:00:00".to_owned())
        );
        assert_eq!(
            sessions
                .iter()
                .filter(|s| s.subagent && s.parent.as_deref() == Some(parent))
                .count(),
            2,
            "both children nest under it"
        );

        // Reading it says why there is no conversation, and offers the
        // children the reader can actually open.
        let transcript = Omp { root }.open(&row.locator).unwrap();
        assert_eq!(transcript.id, parent);
        let text = transcript.searchable_text();
        assert!(
            text.contains("still in the running agent's memory"),
            "{text}"
        );
        assert!(matches!(
            transcript.turns[0].blocks[1],
            Block::ToolCall { ref name, .. } if name == "task"
        ));
    }

    #[test]
    fn edit_tool_emits_patch_for_explicit_patch_header_path() {
        let t = transcript_of(&[
            r#"{"type":"message","message":{"role":"assistant","content":[{"type":"toolCall","name":"edit","arguments":{"input":"*** Begin Patch\n[/workspace/src/types.ts#E644]\n*** End Patch"}}]}}"#,
        ]);

        match &t.turns[0].blocks[1] {
            Block::Patch { files, .. } => {
                assert_eq!(files.len(), 1);
                assert_eq!(files[0].path, PathBuf::from("/workspace/src/types.ts"));
                assert_eq!(files[0].added, 0);
                assert_eq!(files[0].deleted, 0);
            }
            other => panic!("expected patch, got {other:?}"),
        }
    }

    #[test]
    fn assistant_usage_is_summed_from_per_message_records() {
        let t = transcript_of(&[
            r#"{"type":"message","message":{"role":"assistant","content":"one","usage":{"input":2,"output":3,"cost":{"total":0.1}}}}"#,
            r#"{"type":"message","message":{"role":"assistant","content":"two","usage":{"input":5,"output":7,"cost":{"total":0.2}}}}"#,
        ]);

        assert_eq!(
            t.usage(),
            Some(Usage {
                input: 7,
                output: 10,
                cost: 0.1 + 0.2,
            })
        );
    }

    #[test]
    fn tool_result_envelope_becomes_a_tool_block() {
        let t = transcript_of(&[
            r#"{"type":"message","message":{"role":"toolResult","toolName":"read","isError":true,"content":[{"type":"text","text":"boom"}]}}"#,
        ]);
        assert!(matches!(
            t.turns[0].blocks[0],
            Block::ToolResult { ok: false, .. }
        ));
        assert_eq!(t.turns[0].role, Role::Tool);
    }

    #[test]
    fn state_records_do_not_pollute_the_conversation() {
        let t = transcript_of(&[
            r#"{"type":"model_change","model":"x"}"#,
            r#"{"type":"mode_change","mode":"y"}"#,
            r#"{"type":"custom","customType":"tool_execution_start"}"#,
            r#"{"type":"message","message":{"role":"user","content":"hi"}}"#,
        ]);
        assert_eq!(t.turns.len(), 1);
        assert_eq!(t.unknown_records, 0);
    }

    #[test]
    fn unrecognized_record_is_kept_as_raw_and_counted() {
        let t = transcript_of(&[r#"{"type":"from_the_future","payload":1}"#]);
        assert_eq!(t.unknown_records, 1);
        assert!(matches!(t.turns[0].blocks[0], Block::Unknown { .. }));
    }

    #[test]
    fn hidden_harness_notices_are_not_shown() {
        let t = transcript_of(&[
            r#"{"type":"custom_message","display":false,"content":"internal"}"#,
            r#"{"type":"custom_message","display":true,"content":"visible"}"#,
        ]);
        assert_eq!(t.turns.len(), 1);
        assert_eq!(t.turns[0].preview(), Some("visible"));
    }
}
