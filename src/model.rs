use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Which agent wrote a session.
///
/// The registry is one table: adding a source here (and implementing its
/// adapter) updates every enumeration in the crate. Display order, badges,
/// config spellings and index/overlay decoding all derive from it — no other
/// module may hand-list sources.
macro_rules! registry {
    ($($variant:ident = $name:literal, $badge:literal;)*) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "lowercase")]
        pub enum SourceId {
            $($variant,)*
        }

        impl SourceId {
            /// Every source, in display order. The one inventory.
            pub const ALL: &'static [Self] = &[$(Self::$variant),*];

            /// Fixed-width badge for list rows.
            pub const fn badge(self) -> &'static str {
                match self {
                    $(Self::$variant => $badge,)*
                }
            }

            pub const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)*
                }
            }

            /// Inverse of [`SourceId::name`], for config files and remote output.
            pub fn parse(name: &str) -> Option<Self> {
                match name {
                    $($name => Some(Self::$variant),)*
                    _ => None,
                }
            }
        }
    };
}

registry! {
    Omp = "omp", "omp ";
    Claude = "claude", "cldu";
    Codex = "codex", "cdx ";
    OpenCode = "opencode", "ocod";
    Copilot = "copilot", "cplt";
    Kilo = "kilo", "kilo";
    Goose = "goose", "goos";
    Cursor = "cursor", "curs";
    Gemini = "gemini", "gmni";
    Hermes = "hermes", "herm";
    OpenClaw = "openclaw", "claw";
}

impl std::fmt::Display for SourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Where the session body lives, so tier 2 can reopen it without rescanning.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Locator {
    /// One JSONL transcript.
    File { path: PathBuf },
    /// One row in an agent's `SQLite` store.
    Row { db: PathBuf, id: String },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cost: f64,
}

impl Usage {
    pub fn is_zero(&self) -> bool {
        self.input == 0 && self.output == 0 && self.cost == 0.0
    }
}

/// Whether an agent process is attached to this session right now.
///
/// The two live variants are deliberately distinct. `Live` is an exact join —
/// a process holds an open descriptor on this session's file. `Probably` is an
/// inference from agent, working directory and recency, which is ambiguous
/// when two instances share a worktree. A confident lie is worse than an
/// admitted guess, so the renderer must never draw them the same way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum Liveness {
    Live {
        pid: u32,
    },
    Probably {
        pid: u32,
    },
    /// No process found, but written very recently.
    Recent,
    #[default]
    Stale,
}

impl Liveness {
    pub const fn pid(self) -> Option<u32> {
        match self {
            Self::Live { pid } | Self::Probably { pid } => Some(pid),
            _ => None,
        }
    }

    pub const fn is_running(self) -> bool {
        matches!(self, Self::Live { .. } | Self::Probably { .. })
    }

    /// Sort key: running first, then recent, then the rest.
    pub const fn rank(self) -> u8 {
        match self {
            Self::Live { .. } => 0,
            Self::Probably { .. } => 1,
            Self::Recent => 2,
            Self::Stale => 3,
        }
    }

    /// Marker glyph. `Live` and `Probably` differ on purpose.
    pub const fn badge(self) -> &'static str {
        match self {
            Self::Live { .. } => "\u{25cf}",
            Self::Probably { .. } => "\u{25cb}",
            Self::Recent => "\u{00b7}",
            Self::Stale => " ",
        }
    }
}

/// How many lines and files a session changed, when the agent records it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub files: u32,
    pub added: u64,
    pub deleted: u64,
}

impl Change {
    pub fn is_zero(&self) -> bool {
        self.files == 0 && self.added == 0 && self.deleted == 0
    }

    pub fn merge(&mut self, other: Change) {
        self.files += other.files;
        self.added += other.added;
        self.deleted += other.deleted;
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: PathBuf,
    pub added: u64,
    pub deleted: u64,
}

/// Which rung of the label precedence chain produced a display name.
///
/// Worth carrying: 51 sessions on this machine share one agent-assigned
/// title, so a derived label is often the *better* one — but it must never
/// masquerade as a name the agent chose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LabelSource {
    /// The agent named the session.
    Title,
    /// First user prompt, from the index cache.
    Prompt,
    /// Nothing usable; agent and timestamp.
    Fallback,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Label {
    pub text: String,
    pub source: LabelSource,
}

/// How confident we are about `workspace`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathTrust {
    /// Read from an authoritative `cwd`/`directory` field.
    Recorded,
    /// Reconstructed from a lossy directory-name encoding. Display only.
    Decoded,
}

/// Tier-1 row. Producing one MUST NOT require parsing message bodies.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub source: SourceId,
    /// Host the session lives on. `None` is this machine; `Some` is the
    /// `name` of a configured remote host, not a DNS name.
    #[serde(default)]
    pub host: Option<String>,
    /// Canonical workspace root, after worktree rollup.
    pub workspace: Option<PathBuf>,
    /// Path exactly as the agent recorded it, before canonicalization.
    pub recorded_cwd: Option<PathBuf>,
    pub trust: PathTrust,
    pub title: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub created: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub updated: OffsetDateTime,
    pub model: Option<String>,
    /// Git branch the session ran on, where the agent records one.
    #[serde(default)]
    pub branch: Option<String>,
    pub parent: Option<String>,
    /// A subagent transcript belonging to `parent`, not a top-level session.
    #[serde(default)]
    pub subagent: bool,
    pub archived: bool,
    /// Present only when the source yields it for free at tier 1.
    pub usage: Option<Usage>,
    /// Files and lines touched, when the source reports it at tier 1.
    #[serde(default)]
    pub change: Option<Change>,
    pub size_bytes: Option<u64>,
    pub locator: Locator,
    /// Filled in by [`crate::catalog::scan`], not by the source: whether a process is
    /// attached to this session right now.
    pub liveness: Liveness,
}

impl SessionSummary {
    /// Row identity plus locator, every other field at its neutral value.
    ///
    /// An adapter then states only what its store actually knows with
    /// struct-update syntax. This replaces the twenty-field literal each
    /// adapter used to retype, where one new field meant thirteen
    /// synchronized edits.
    pub fn new(id: String, source: SourceId, updated: OffsetDateTime, locator: Locator) -> Self {
        Self {
            id,
            source,
            updated,
            locator,
            host: None,
            workspace: None,
            recorded_cwd: None,
            trust: PathTrust::Recorded,
            title: None,
            created: None,
            model: None,
            branch: None,
            parent: None,
            subagent: false,
            archived: false,
            usage: None,
            change: None,
            size_bytes: None,
            liveness: Liveness::default(),
        }
    }

    /// Stable cross-source key. Ids collide across agents, never within one.
    pub fn key(&self) -> (SourceId, &str) {
        (self.source, self.id.as_str())
    }

    pub fn display_title(&self) -> &str {
        self.title.as_deref().unwrap_or("—")
    }

    /// The file holding the session: its transcript, or the database a row
    /// lives in. On the session's host, for remote rows.
    pub fn store_path(&self) -> &Path {
        match &self.locator {
            Locator::File { path } => path,
            Locator::Row { db, .. } => db,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}

impl Role {
    pub const fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "asst",
            Self::System => "sys",
            Self::Tool => "tool",
        }
    }
}

/// One piece of a turn.
///
/// [`Block::Unknown`] is the format-drift firewall. Codex ships legacy and
/// modern rollouts side by side; Claude emits roughly fifteen outer record
/// types. An unrecognized record renders as folded raw JSON — it never fails
/// the session.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "block", rename_all = "lowercase")]
pub enum Block {
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolCall {
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        name: Option<String>,
        ok: bool,
        output: String,
    },
    File {
        path: PathBuf,
    },
    Image {
        label: String,
    },
    Patch {
        summary: String,
        files: Vec<FileChange>,
    },
    Unknown {
        raw: serde_json::Value,
    },
}

impl Block {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn thinking(text: impl Into<String>) -> Self {
        Self::Thinking { text: text.into() }
    }

    /// Content contributing to full-text search and previews.
    pub fn searchable(&self) -> Option<&str> {
        match self {
            Self::Text { text } | Self::Thinking { text } => Some(text),
            Self::ToolResult { output, .. } => Some(output),
            _ => None,
        }
    }

    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Text { .. } => "text",
            Self::Thinking { .. } => "thinking",
            Self::ToolCall { .. } => "tool",
            Self::ToolResult { .. } => "result",
            Self::File { .. } => "file",
            Self::Image { .. } => "image",
            Self::Patch { .. } => "patch",
            Self::Unknown { .. } => "raw",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Turn {
    pub role: Role,
    #[serde(with = "time::serde::rfc3339::option")]
    pub timestamp: Option<OffsetDateTime>,
    pub model: Option<String>,
    /// What this turn cost, when the agent records it per message.
    ///
    /// OMP and Claude only report usage inside the transcript, so a
    /// per-workspace spend column is impossible from tier 1 alone. Summing
    /// here lets `llm-sessions index` cache the total while it is already parsing the
    /// body (improvement 8).
    pub usage: Option<Usage>,
    pub blocks: Vec<Block>,
}

impl Turn {
    pub fn new(role: Role) -> Self {
        Self {
            role,
            timestamp: None,
            model: None,
            usage: None,
            blocks: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// First line of prose, for one-line previews.
    pub fn preview(&self) -> Option<&str> {
        self.blocks
            .iter()
            .find_map(|b| match b {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .map(|t| t.lines().find(|l| !l.trim().is_empty()).unwrap_or(t))
    }
}

/// Tier 2: one session's body, normalized.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Transcript {
    pub id: String,
    pub source: SourceId,
    pub title: Option<String>,
    pub workspace: Option<PathBuf>,
    pub turns: Vec<Turn>,
    /// Records the adapter could not classify, already surfaced as
    /// [`Block::Unknown`]; counted so the UI can admit what it skipped.
    pub unknown_records: usize,
}

impl Transcript {
    pub fn new(id: impl Into<String>, source: SourceId) -> Self {
        Self {
            id: id.into(),
            source,
            title: None,
            workspace: None,
            turns: Vec::new(),
            unknown_records: 0,
        }
    }

    pub fn push(&mut self, turn: Turn) {
        if !turn.is_empty() {
            self.turns.push(turn);
        }
    }

    /// Concatenated prose, used to build the search index.
    pub fn searchable_text(&self) -> String {
        let mut out = String::new();
        for turn in &self.turns {
            for block in &turn.blocks {
                if let Some(text) = block.searchable() {
                    out.push_str(text);
                    out.push('\n');
                }
            }
        }
        out
    }

    /// Text blocks of the nearest assistant turn at or before `turn` that has
    /// any, joined by blank lines: the reply a person would copy, without
    /// tool calls or thinking. Reading a tool result or the next question
    /// still means the reply just above it.
    pub fn reply_at(&self, turn: usize) -> Option<String> {
        self.turns
            .iter()
            .take(turn.saturating_add(1))
            .rev()
            .filter(|candidate| candidate.role == Role::Assistant)
            .find_map(|candidate| {
                let parts: Vec<&str> = candidate
                    .blocks
                    .iter()
                    .filter_map(|block| match block {
                        Block::Text { text } if !text.trim().is_empty() => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                (!parts.is_empty()).then(|| parts.join("\n\n"))
            })
    }

    pub fn last_reply(&self) -> Option<String> {
        self.reply_at(usize::MAX)
    }

    /// Total recorded usage, or `None` when no turn carried any.
    ///
    /// `None` and `Some(zero)` mean different things: the first is "this agent
    /// does not record it", the second is "it recorded nothing spent". A spend
    /// column must not show 0.00 for the former.
    pub fn usage(&self) -> Option<Usage> {
        let mut total = Usage::default();
        let mut seen = false;
        for turn in &self.turns {
            if let Some(usage) = turn.usage {
                seen = true;
                total.input += usage.input;
                total.output += usage.output;
                total.cost += usage.cost;
            }
        }
        seen.then_some(total)
    }

    /// Every file this session touched, merged across patch blocks.
    ///
    /// Patch parts are parsed today and rendered as one grey line; the most
    /// useful question about a finished session is which files it changed.
    pub fn changed_files(&self) -> Vec<FileChange> {
        let mut merged: Vec<FileChange> = Vec::new();
        for turn in &self.turns {
            for block in &turn.blocks {
                let Block::Patch { files, .. } = block else {
                    continue;
                };
                for file in files {
                    match merged.iter_mut().find(|f| f.path == file.path) {
                        Some(existing) => {
                            existing.added += file.added;
                            existing.deleted += file.deleted;
                        }
                        None => merged.push(file.clone()),
                    }
                }
            }
        }
        merged.sort_by_key(|f| std::cmp::Reverse(f.added + f.deleted));
        merged
    }

    /// Roll [`Transcript::changed_files`] up into a tier-1 shaped summary.
    pub fn change(&self) -> Option<Change> {
        let files = self.changed_files();
        if files.is_empty() {
            return None;
        }
        Some(Change {
            // Saturate: a count past `u32::MAX` files is not representable,
            // and the largest value is the honest answer.
            files: u32::try_from(files.len()).unwrap_or(u32::MAX),
            added: files.iter().map(|f| f.added).sum(),
            deleted: files.iter().map(|f| f.deleted).sum(),
        })
    }

    /// Name of the first tool the session called, for labelling sessions that
    /// contain no prose at all.
    pub fn first_tool(&self) -> Option<&str> {
        self.turns
            .iter()
            .flat_map(|turn| &turn.blocks)
            .find_map(|b| match b {
                Block::ToolCall { name, .. } => Some(name.as_str()),
                _ => None,
            })
    }

    /// Recorded spend split by the model that incurred it.
    pub fn usage_by_model(&self) -> std::collections::BTreeMap<String, Usage> {
        let mut out = std::collections::BTreeMap::<String, Usage>::new();
        for turn in &self.turns {
            let (Some(usage), Some(model)) = (turn.usage, turn.model.as_deref()) else {
                continue;
            };
            let entry = out.entry(model.to_owned()).or_default();
            entry.input += usage.input;
            entry.output += usage.output;
            entry.cost += usage.cost;
        }
        out
    }

    /// The prompt that started the session, for digests.
    pub fn first_user_prompt(&self) -> Option<&str> {
        self.turns
            .iter()
            .find(|turn| turn.role == Role::User)
            .and_then(Turn::preview)
    }
}

/// Result of asking a source to change archive state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveOutcome {
    /// The agent models archiving; its own store was updated.
    Native,
    /// The agent has no archive concept; the caller must use the overlay.
    Unsupported,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_preserves_indentation_and_markdown_breaks() {
        let mut transcript = Transcript::new("a", SourceId::Omp);
        let mut turn = Turn::new(Role::Assistant);
        turn.blocks.push(Block::text("    first\n    second"));
        turn.blocks.push(Block::text(" \n\t "));
        turn.blocks.push(Block::text("line with a break  \nnext  "));
        transcript.push(turn);
        assert_eq!(
            transcript.last_reply().as_deref(),
            Some("    first\n    second\n\nline with a break  \nnext  ")
        );
    }

    #[test]
    fn registry_round_trips() {
        for source in SourceId::ALL {
            assert_eq!(SourceId::parse(source.name()), Some(*source));
            assert_eq!(source.to_string(), source.name());
            assert_eq!(source.badge().chars().count(), 4, "badges stay fixed-width");
        }
        assert_eq!(SourceId::parse("not-a-source"), None);
    }
}
