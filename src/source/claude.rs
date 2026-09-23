//! Claude Code: `~/.claude/projects/<flattened-cwd>/<uuid>.jsonl`.
//!
//! Records are heterogeneous — `mode`, `permission-mode`, `file-history-snapshot`,
//! `user`, `assistant`, `ai-title`, `cost-state`, … — and the cwd appears only
//! on conversation records, so the head is scanned for the first one that has
//! it. The directory name is a lossy `/`→`-` flattening and is used only when
//! no record carries a cwd, tagged [`PathTrust::Decoded`].

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::model::{
    Block, Locator, PathTrust, Role, SessionSummary, SourceId, Transcript, Turn, Usage,
};
use crate::source::{
    RemoteLayout, Source, clean_title, file_locator, flatten_text, head_records, mtime,
    parse_rfc3339, push_unknown, read_head, read_records, read_tail, str_field,
};

const HEAD_BYTES: usize = 64 * 1024;
/// `ai-title` is written after the first exchange, not at the end, but the
/// tail is where a *renamed* session's latest title lands.
const TAIL_BYTES: u64 = 128 * 1024;

#[derive(Debug)]
pub struct Claude {
    root: PathBuf,
}

impl Claude {
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join(".claude/projects"),
        }
    }
}

impl Source for Claude {
    fn id(&self) -> SourceId {
        SourceId::Claude
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
            if !project.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let dir = project.path();
            let Ok(files) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in files.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "jsonl")
                    && let Some(summary) = read_summary(&path, &dir)?
                {
                    out.push(summary);
                }
            }
        }
        Ok(out)
    }

    fn open(&self, locator: &Locator) -> Result<Transcript> {
        read_transcript(file_locator(SourceId::Claude, locator)?)
    }

    fn resume_command(&self, id: &str) -> Option<Vec<String>> {
        Some(vec!["claude".into(), "--resume".into(), id.into()])
    }

    /// The shell transport finds one JSONL per session under the flattened
    /// project directories.
    fn remote_layout(&self) -> RemoteLayout {
        RemoteLayout {
            files: &[".claude/projects/*/*.jsonl"],
            sql: None,
        }
    }
}

fn read_summary(path: &Path, project_dir: &Path) -> Result<Option<SessionSummary>> {
    let head = read_head(path, HEAD_BYTES)?;

    let mut id = None;
    let mut cwd = None;
    let mut created = None;
    let mut title = None;
    let mut git_branch = None;
    for record in head_records(&head) {
        id = id.or_else(|| str_field(&record, "sessionId"));
        cwd = cwd.or_else(|| str_field(&record, "cwd"));
        created = created.or_else(|| {
            str_field(&record, "timestamp")
                .as_deref()
                .and_then(parse_rfc3339)
        });
        if record.get("type").and_then(serde_json::Value::as_str) == Some("ai-title") {
            title = str_field(&record, "aiTitle");
        }
        if git_branch.is_none() {
            git_branch = str_field(&record, "gitBranch").filter(|branch| !branch.trim().is_empty());
        }
    }
    if id.is_none() {
        return Ok(None);
    }

    // A later rename supersedes the first title.
    for record in head_records(&read_tail(path, TAIL_BYTES)?) {
        if record.get("type").and_then(serde_json::Value::as_str) == Some("ai-title")
            && let Some(t) = str_field(&record, "aiTitle")
        {
            title = Some(t);
        }
    }

    let (updated, size) = mtime(path)?;
    let (recorded_cwd, trust) = match cwd {
        Some(c) => (Some(PathBuf::from(c)), PathTrust::Recorded),
        None => (decode_project_dir(project_dir), PathTrust::Decoded),
    };

    Ok(Some(SessionSummary {
        recorded_cwd,
        trust,
        title: title.as_deref().and_then(clean_title),
        created,
        size_bytes: Some(size),
        // `gitBranch: "HEAD"` is Claude's detached-worktree marker, not a
        // usable branch name. A measured record also carries real names such
        // as `feat/EREP-357/product-naming-translations`.
        branch: git_branch.filter(|branch| branch != "HEAD"),
        ..SessionSummary::new(
            id.unwrap_or_default(),
            SourceId::Claude,
            updated,
            Locator::File {
                path: path.to_path_buf(),
            },
        )
    }))
}

/// Display-only reversal of `/`→`-` flattening. Irreversible: `/tmp/a-b` and
/// `/tmp/a/b` both encode to `-tmp-a-b`, so the result is a guess.
fn decode_project_dir(dir: &Path) -> Option<PathBuf> {
    let name = dir.file_name()?.to_str()?;
    let body = name.strip_prefix('-')?;
    Some(PathBuf::from(format!("/{}", body.replace('-', "/"))))
}

/// Outer record types that are session bookkeeping, not conversation.
/// Measured across this machine's transcripts: `mode`, `permission-mode`,
/// `file-history-snapshot`, `cost-state`, `last-prompt`, `queue-operation`,
/// `ai-title`, `atis-latch`.
const STATE_RECORDS: &[&str] = &[
    "mode",
    "permission-mode",
    "file-history-snapshot",
    "cost-state",
    "last-prompt",
    "queue-operation",
    "ai-title",
    "atis-latch",
    "turn_duration",
    "attachment",
];

/// Claude's `cost-state.totalCostUSD` is a running session total. A measured
/// terminal record is `{"type":"cost-state","totalCostUSD":0.844114,
/// "modelUsage":{"claude-opus-5[1m]":…}}`, so only its final value is attached
/// to the final assistant turn; summing cost-state records would double count.
fn read_transcript(path: &Path) -> Result<Transcript> {
    let mut transcript = Transcript::new(String::new(), SourceId::Claude);
    let mut final_cost = None;
    for record in read_records(path)? {
        if transcript.id.is_empty()
            && let Some(id) = str_field(&record, "sessionId")
        {
            transcript.id = id;
        }
        if transcript.workspace.is_none()
            && let Some(cwd) = str_field(&record, "cwd")
        {
            transcript.workspace = Some(PathBuf::from(cwd));
        }

        match record.get("type").and_then(serde_json::Value::as_str) {
            Some("ai-title") => {
                transcript.title = str_field(&record, "aiTitle")
                    .as_deref()
                    .and_then(clean_title)
                    .or(transcript.title.take());
            }
            Some(role @ ("user" | "assistant" | "system")) => {
                transcript.push(message_turn(&record, role));
            }
            Some("cost-state") => {
                final_cost = record
                    .get("totalCostUSD")
                    .and_then(serde_json::Value::as_f64)
                    .filter(|cost| cost.is_finite());
            }
            Some(other) if STATE_RECORDS.contains(&other) => {}
            _ => push_unknown(&mut transcript, record),
        }
    }
    if let Some(cost) = final_cost
        && let Some(turn) = transcript
            .turns
            .iter_mut()
            .rfind(|turn| turn.role == Role::Assistant)
    {
        turn.usage.get_or_insert_default().cost = cost;
    }
    if transcript.id.is_empty() {
        transcript.id = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
    }
    Ok(transcript)
}

fn message_turn(record: &serde_json::Value, outer_role: &str) -> Turn {
    let message = record.get("message").unwrap_or(record);
    // A `tool_result` arrives inside a record typed `user`; the blocks decide
    // the real role, so the envelope is only a default.
    let mut turn = Turn::new(match outer_role {
        "assistant" => Role::Assistant,
        "system" => Role::System,
        _ => Role::User,
    });
    turn.timestamp = str_field(record, "timestamp")
        .as_deref()
        .and_then(parse_rfc3339);
    turn.model = str_field(message, "model");
    if turn.role == Role::Assistant {
        turn.usage = message.get("usage").and_then(claude_usage);
    }

    match message.get("content") {
        Some(serde_json::Value::String(text)) => turn.blocks.push(Block::text(text.clone())),
        Some(serde_json::Value::Array(blocks)) => {
            for block in blocks {
                turn.blocks.push(content_block(block));
            }
        }
        _ => {
            if let Some(text) = str_field(record, "content") {
                turn.blocks.push(Block::text(text));
            }
        }
    }
    if turn
        .blocks
        .iter()
        .all(|b| matches!(b, Block::ToolResult { .. }))
        && !turn.blocks.is_empty()
    {
        turn.role = Role::Tool;
        turn.usage = None;
    }
    turn
}

fn claude_usage(value: &serde_json::Value) -> Option<Usage> {
    let input = value.get("input_tokens")?.as_u64()?;
    let output = value.get("output_tokens")?.as_u64()?;
    Some(Usage {
        input,
        output,
        cost: 0.0,
    })
}

fn content_block(block: &serde_json::Value) -> Block {
    match block.get("type").and_then(serde_json::Value::as_str) {
        Some("text") => Block::text(str_field(block, "text").unwrap_or_default()),
        // Redacted thinking arrives with an empty body and only a signature;
        // showing the signature would be noise.
        Some("thinking") => Block::thinking(str_field(block, "thinking").unwrap_or_default()),
        Some("tool_use") => Block::ToolCall {
            name: str_field(block, "name").unwrap_or_default(),
            input: block.get("input").cloned().unwrap_or_default(),
        },
        Some("tool_result") => Block::ToolResult {
            name: None,
            ok: !block
                .get("is_error")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            output: flatten_text(block.get("content")),
        },
        Some("image") => Block::Image {
            label: block
                .pointer("/source/media_type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("image")
                .to_string(),
        },
        _ => Block::Unknown { raw: block.clone() },
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn transcript_of(lines: &[&str]) -> Transcript {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("11111111-2222-3333-4444-555555555555.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        drop(file);
        read_transcript(&path).unwrap()
    }

    fn summary_with_branch(git_branch: &str) -> SessionSummary {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("-workspace");
        std::fs::create_dir(&project).unwrap();
        let path = project.join("session.jsonl");
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"user\",\"sessionId\":\"session\",\"cwd\":\"/workspace\",\"gitBranch\":\"{git_branch}\"}}\n"
            ),
        )
        .unwrap();
        read_summary(&path, &project).unwrap().unwrap()
    }

    #[test]
    fn cost_state_uses_only_the_final_running_total() {
        let t = transcript_of(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":"first","usage":{"input_tokens":2,"output_tokens":3}}}"#,
            r#"{"type":"cost-state","totalCostUSD":0.25}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":"second","usage":{"input_tokens":5,"output_tokens":7}}}"#,
            r#"{"type":"cost-state","totalCostUSD":0.9}"#,
        ]);

        assert_eq!(
            t.usage(),
            Some(Usage {
                input: 7,
                output: 10,
                cost: 0.9
            })
        );
        assert!(t.turns[0].usage.unwrap().cost.abs() < 1e-9);
        assert!((t.turns[1].usage.unwrap().cost - 0.9).abs() < 1e-9);
    }

    #[test]
    fn detached_head_is_not_a_branch_but_named_branch_survives() {
        assert_eq!(summary_with_branch("HEAD").branch, None);
        assert_eq!(
            summary_with_branch("feat/EREP-357/product-naming-translations").branch,
            Some("feat/EREP-357/product-naming-translations".to_owned())
        );
    }

    #[test]
    fn tool_result_records_are_attributed_to_the_tool_not_the_user() {
        let t = transcript_of(&[
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
        ]);
        assert_eq!(t.turns[0].role, Role::Tool);
    }

    #[test]
    fn a_later_rename_wins_over_the_first_title() {
        let t = transcript_of(&[
            r#"{"type":"ai-title","aiTitle":"first","sessionId":"s"}"#,
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"ai-title","aiTitle":"second","sessionId":"s"}"#,
        ]);
        assert_eq!(t.title.as_deref(), Some("second"));
        assert_eq!(t.id, "s");
    }

    #[test]
    fn bookkeeping_records_are_not_raw_noise() {
        let t = transcript_of(&[
            r#"{"type":"mode","mode":"normal"}"#,
            r#"{"type":"file-history-snapshot","snapshot":{}}"#,
            r#"{"type":"cost-state","totalCostUSD":1.0}"#,
        ]);
        assert_eq!(t.unknown_records, 0);
        assert!(t.turns.is_empty());
    }

    #[test]
    fn decoded_project_dir_is_a_guess_not_a_fact() {
        // Both `/tmp/a-b` and `/tmp/a/b` flatten to `-tmp-a-b`.
        assert_eq!(
            decode_project_dir(Path::new("/x/-tmp-a-b")),
            Some(PathBuf::from("/tmp/a/b"))
        );
    }
}
