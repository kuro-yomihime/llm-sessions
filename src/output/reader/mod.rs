//! The transcript reader: a pure rendering engine.
//!
//! [`render`] flattens a [`Transcript`] into [`RenderedLine`]s — one column of
//! body text behind a two-cell gutter, folding decided here, colour left to
//! the drawing code via [`LineKind`] and [`Rail`]. [`fit`] wraps those lines
//! to a width. Neither reads UI or App state: the interface is transcript in,
//! laid-out lines out, so every markdown, table and wrapping question has one
//! home and one test suite.

mod table;
mod wrap;

use std::collections::HashMap;

use time::UtcOffset;
// Outside `wrap` and `table`, only tests wrap without laying tables out.
#[cfg(test)]
pub(crate) use wrap::wrap_rendered;
pub use wrap::{elide, fit, truncate, truncate_cells};

use self::table::{table_alignment, table_cells};
use crate::clock;
use crate::model::{Block, FileChange, Role, SourceId, Transcript, Turn};

/// Colour and weight class of a reader line.
///
/// The whole visual system of the transcript reader is this enum plus
/// [`Rail`]: the kind styles the body column, the rail styles the two-cell
/// gutter left of it. Nothing else carries colour, so a new block type
/// cannot invent a fifth shade of grey.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    /// Hairline between conversation turns. The renderer fills its width and
    /// prints `text` — a date — as its label, when it has one.
    Rule,
    SpeakerUser,
    SpeakerAgent,
    SpeakerSystem,
    /// Changed-file summary and per-file rows.
    Files,
    /// A line that points at another session.
    Link,
    Text,
    Heading,
    Bullet,
    /// One row of a markdown table. [`render`] passes the markdown through
    /// verbatim — how wide a column may be is not knowable before the
    /// measure is — and [`fit`] replaces the run with laid-out rows.
    Table,
    Quote,
    Code,
    Thinking,
    Tool,
    ToolOk,
    ToolErr,
    Raw,
    Meta,
}

/// The two-cell gutter left of the body column.
///
/// Every body line starts at the same column, so prose, tool output and
/// changed files form one readable text block and the gutter alone says what
/// kind of thing is speaking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rail {
    /// No gutter: spacers and full-width rules bleed to the edge.
    Bare,
    /// Blank gutter; the body stays on the common text column.
    Pad,
    /// Accent bar on every wrapped row — a user turn.
    Bar,
    /// Dim pipe on every wrapped row — quoted, code and unfolded bodies.
    Pipe,
    /// Two-cell glyph on the first row, blank on its continuations.
    Mark(&'static str),
}

impl Rail {
    /// Cells the gutter occupies. Wrapping subtracts it from the measure.
    pub const fn width(self) -> usize {
        match self {
            Self::Bare => 0,
            _ => 2,
        }
    }

    /// Gutter text for one drawn row.
    pub const fn glyph(self, first: bool) -> &'static str {
        match self {
            Self::Bare => "",
            Self::Pad => "  ",
            Self::Bar => "▌ ",
            Self::Pipe => "│ ",
            Self::Mark(glyph) => {
                if first {
                    glyph
                } else {
                    "  "
                }
            }
        }
    }
}

/// Inline weight inside one line of prose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Emphasis {
    /// `**bold**`
    Strong,
    /// `*italic*`
    Soft,
    /// `` `code` ``
    Code,
}

/// An emphasised byte range of [`RenderedLine::text`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Accent {
    pub start: usize,
    pub end: usize,
    pub emphasis: Emphasis,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedLine {
    pub kind: LineKind,
    pub rail: Rail,
    pub text: String,
    /// Inline emphasis, ascending and non-overlapping. Empty for everything
    /// that is not prose, which is most lines.
    pub accents: Vec<Accent>,
    /// Dim detail set flush right: timestamps, models, line counts. A line
    /// carrying one is a header, so it is truncated rather than wrapped.
    pub meta: Option<String>,
    /// A session this line points at — a subagent transcript behind a `task`
    /// call. `Enter` on the line follows it.
    pub link: Option<(SourceId, String)>,
    /// Index into `Transcript::turns` of the turn this line belongs to;
    /// `None` for preamble, dividers and spacers between turns.
    pub turn: Option<usize>,
    /// One row, always: a chip is truncated at the right edge rather than
    /// reflowed, because three rows of JSON is not a summary.
    pub clip: bool,
    /// First row of a wrapped line; the gutter shows a mark only here.
    pub first: bool,
}

impl RenderedLine {
    pub(crate) fn new(kind: LineKind, rail: Rail, text: impl Into<String>) -> Self {
        Self {
            kind,
            rail,
            text: text.into(),
            accents: Vec::new(),
            meta: None,
            link: None,
            turn: None,
            first: true,
            clip: false,
        }
    }

    /// Prose: markdown emphasis becomes weight, and its markers disappear.
    fn prose(kind: LineKind, rail: Rail, text: &str) -> Self {
        let (text, accents) = inline(text);
        Self {
            accents,
            ..Self::new(kind, rail, text)
        }
    }

    /// A one-row summary: a tool call, its result, a fold, a raw record.
    fn chip(kind: LineKind, rail: Rail, text: impl Into<String>) -> Self {
        Self {
            clip: true,
            ..Self::new(kind, rail, text)
        }
    }

    /// Vertical breathing room. Blank to the edge, so no gutter shows.
    fn spacer() -> Self {
        Self::new(LineKind::Meta, Rail::Bare, String::new())
    }

    /// Turn divider. `label` is drawn inside the hairline.
    fn rule(label: impl Into<String>) -> Self {
        Self::new(LineKind::Rule, Rail::Bare, label)
    }

    fn with_meta(mut self, meta: impl Into<String>) -> Self {
        let meta = meta.into();
        self.meta = (!meta.is_empty()).then_some(meta);
        self
    }

    pub(crate) fn with_link(mut self, link: (SourceId, String)) -> Self {
        self.link = Some(link);
        self
    }

    pub fn is_blank(&self) -> bool {
        self.text.trim().is_empty() && self.meta.is_none() && self.kind != LineKind::Rule
    }

    /// Styled runs of `text`, in order, each with the emphasis to apply.
    ///
    /// Cheap to call per drawn row: with no accents it yields the whole line
    /// as one run and allocates nothing.
    pub fn runs(&self) -> impl Iterator<Item = (&str, Option<Emphasis>)> {
        let mut at = 0;
        let mut next = 0;
        std::iter::from_fn(move || {
            if at >= self.text.len() {
                return None;
            }
            let accent = self.accents.get(next).copied();
            match accent {
                Some(accent) if accent.start <= at => {
                    let end = accent.end.min(self.text.len()).max(at);
                    next += 1;
                    let run = &self.text[at..end];
                    at = end;
                    Some((run, Some(accent.emphasis)))
                }
                Some(accent) => {
                    let end = accent.start.min(self.text.len());
                    let run = &self.text[at..end];
                    at = end;
                    Some((run, None))
                }
                None => {
                    let run = &self.text[at..];
                    at = self.text.len();
                    Some((run, None))
                }
            }
        })
    }
}

/// Widest body column the reader will use.
///
/// Prose stops being readable past roughly ninety cells, so a wide pane or a
/// fullscreen terminal gets a measure, not a longer line.
pub const READING_MEASURE: usize = 92;

/// Longest chip text the renderer keeps. A chip is clipped to the drawn
/// column, which the renderer does not know, so this only bounds the string
/// it hands the drawing code — it must never be the narrower of the two.
const CHIP_BUDGET: usize = 240;

/// Flatten a transcript into display lines.
///
/// Folding is a property of the render, not of the data: toggling it never
/// re-reads the session.
///
/// The shape is one column of body text behind a two-cell gutter: a user turn
/// carries an accent bar, an agent turn carries nothing, and machinery (tool
/// calls, results, thinking, raw records) carries a single glyph. Turn
/// dividers and dates are the only horizontal furniture.
pub fn render(
    transcript: &Transcript,
    fold_thinking: bool,
    fold_tools: bool,
    show_files: bool,
    subagents: &HashMap<String, (SourceId, String)>,
    // Clock for every timestamp and date divider the reader draws.
    offset: UtcOffset,
) -> Vec<RenderedLine> {
    let mut out = Vec::new();

    // The most useful question about a finished session is which files it
    // touched, so the answer goes at the top rather than being reconstructed
    // by scrolling through patch blocks.
    let files = transcript.changed_files();
    if !files.is_empty() {
        let added: u64 = files.iter().map(|f| f.added).sum();
        let deleted: u64 = files.iter().map(|f| f.deleted).sum();
        out.push(
            RenderedLine::chip(
                LineKind::Files,
                Rail::Mark(if show_files { "▾ " } else { "▸ " }),
                format!("{} changed", plural(files.len(), "file")),
            )
            .with_meta(if added + deleted > 0 {
                format!("+{added} −{deleted}")
            } else {
                String::new()
            }),
        );
        if show_files {
            for file in &files {
                out.push(file_row(file));
            }
        }
        out.push(RenderedLine::spacer());
    }

    let mut day = None;
    let mut first_turn = true;
    for (index, turn) in transcript.turns.iter().enumerate() {
        // An envelope with no content is bookkeeping, not conversation.
        if turn.blocks.is_empty() {
            continue;
        }
        // A result envelope is not a speaker. Its glyph already says what it
        // is, so the header and the blank line around it are pure noise.
        let speaks = !(turn.blocks.iter().all(is_machinery) && turn.role == Role::Tool);
        let day_changed = turn
            .timestamp
            .map(|stamp| clock::at(stamp, offset).date())
            .filter(|date| Some(*date) != day);
        // One step of work — what was said, the calls it made and their
        // results — reads as a single block. The air goes before whoever
        // speaks next, never between a call and its result.
        if speaks && !first_turn && !out.last().is_some_and(RenderedLine::is_blank) {
            out.push(RenderedLine::spacer());
        }
        if let Some(date) = day_changed {
            out.push(RenderedLine::rule(date.to_string()));
            day = Some(date);
        } else if turn.role == Role::User && !first_turn {
            // Each new question is where a reader resumes scanning.
            out.push(RenderedLine::rule(String::new()));
        }
        first_turn = false;

        let rail = if turn.role == Role::User {
            Rail::Bar
        } else {
            Rail::Pad
        };
        let start = out.len();
        if speaks {
            out.push(turn_header(turn, rail, offset));
        }
        for block in &turn.blocks {
            push_block(&mut out, block, rail, fold_thinking, fold_tools, subagents);
        }
        for line in &mut out[start..] {
            line.turn = Some(index);
        }
    }
    if transcript.unknown_records > 0 {
        out.push(RenderedLine::chip(
            LineKind::Meta,
            Rail::Mark("? "),
            format!(
                "{} this reader does not understand, shown raw",
                plural(transcript.unknown_records, "record")
            ),
        ));
    }
    trim_trailing_blanks(&mut out);
    out
}

fn trim_trailing_blanks(out: &mut Vec<RenderedLine>) {
    while out.last().is_some_and(RenderedLine::is_blank) {
        out.pop();
    }
}

/// `+12 −3  src/ui.rs`, counts in a fixed column so paths line up.
fn file_row(file: &FileChange) -> RenderedLine {
    let counts = if file.added == 0 && file.deleted == 0 {
        String::new()
    } else {
        format!("+{} −{}", file.added, file.deleted)
    };
    RenderedLine::new(
        LineKind::Files,
        Rail::Pipe,
        format!("{}", file.path.display()),
    )
    .with_meta(counts)
}

/// Who is speaking, with the detail nobody scans for pushed into `meta`.
fn turn_header(turn: &Turn, rail: Rail, offset: UtcOffset) -> RenderedLine {
    let mut meta = Vec::new();
    if let Some(stamp) = turn.timestamp {
        let (h, m, _) = clock::at(stamp, offset).time().as_hms();
        meta.push(format!("{h:02}:{m:02}"));
    }
    if let Some(usage) = turn.usage.filter(|usage| !usage.is_zero()) {
        if usage.input > 0 || usage.output > 0 {
            meta.push(format!(
                "{}↑ {}↓",
                tokens(usage.input),
                tokens(usage.output)
            ));
        }
        if usage.cost > 0.0 {
            meta.push(format!("${:.2}", usage.cost));
        }
    }
    let (kind, name) = match turn.role {
        Role::User => (LineKind::SpeakerUser, "you".to_owned()),
        Role::Assistant => (
            LineKind::SpeakerAgent,
            turn.model.clone().unwrap_or_else(|| "assistant".to_owned()),
        ),
        Role::System => (LineKind::SpeakerSystem, "system".to_owned()),
        Role::Tool => (LineKind::SpeakerSystem, "tool".to_owned()),
    };
    RenderedLine::new(kind, rail, name).with_meta(meta.join(" · "))
}

/// `1 line`, `4 lines`, `2 matches` — a count that reads like English.
pub fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        return format!("1 {noun}");
    }
    let hiss = noun.ends_with(['s', 'x', 'z']) || noun.ends_with("ch") || noun.ends_with("sh");
    format!("{count} {noun}{}", if hiss { "es" } else { "s" })
}

/// `812`, `12.3k`, `1.2M` — a token count is scanned, not audited.
fn tokens(count: u64) -> String {
    match count {
        0..1_000 => count.to_string(),
        1_000..1_000_000 => format!("{:.1}k", count as f64 / 1_000.0),
        _ => format!("{:.1}M", count as f64 / 1_000_000.0),
    }
}

fn push_block(
    out: &mut Vec<RenderedLine>,
    block: &Block,
    rail: Rail,
    fold_thinking: bool,
    fold_tools: bool,
    subagents: &HashMap<String, (SourceId, String)>,
) {
    match block {
        Block::Text { text } => push_prose(out, rail, text),
        Block::Thinking { text } => {
            if fold_thinking {
                out.push(
                    RenderedLine::chip(LineKind::Thinking, Rail::Mark("◦ "), "thinking")
                        .with_meta(plural(text.lines().count().max(1), "line")),
                );
            } else {
                for line in text.lines() {
                    out.push(RenderedLine::new(LineKind::Thinking, Rail::Pipe, line));
                }
            }
        }
        Block::ToolCall { name, input } => {
            // Name and subject read as one phrase — `read src/ui.rs` — so the
            // subject stays beside the name instead of drifting right.
            out.push(RenderedLine::chip(
                LineKind::Tool,
                Rail::Mark("▸ "),
                match call_summary(input) {
                    subject if subject.is_empty() => name.clone(),
                    subject => format!("{name}  {subject}"),
                },
            ));
            // A delegating call has real transcripts behind it. They are
            // never top-level rows, so this is the only way to reach them.
            for (task, key) in subagent_targets(name, input, subagents) {
                out.push(
                    RenderedLine::chip(LineKind::Link, Rail::Mark("↳ "), task)
                        .with_meta("Enter opens")
                        .with_link(key),
                );
            }
        }
        Block::ToolResult { name, ok, output } => {
            let kind = if *ok {
                LineKind::ToolOk
            } else {
                LineKind::ToolErr
            };
            let mark = Rail::Mark(if *ok { "✓ " } else { "✗ " });
            let label = name.as_deref().unwrap_or("result");
            let lines = output.lines().count();
            let size = if lines > 1 {
                plural(lines, "line")
            } else {
                String::new()
            };
            if fold_tools {
                out.push(
                    RenderedLine::chip(
                        kind,
                        mark,
                        match headline(output) {
                            Some(headline) => format!("{label}  {headline}"),
                            None => label.to_owned(),
                        },
                    )
                    .with_meta(size),
                );
            } else {
                out.push(RenderedLine::chip(kind, mark, label.to_owned()).with_meta(size));
                for line in output.lines() {
                    out.push(RenderedLine::new(kind, Rail::Pipe, line));
                }
            }
        }
        Block::File { path } => out.push(RenderedLine::chip(
            LineKind::Tool,
            Rail::Mark("◆ "),
            format!("{}", path.display()),
        )),
        Block::Image { label } => out.push(RenderedLine::chip(
            LineKind::Tool,
            Rail::Mark("◆ "),
            format!("image {label}"),
        )),
        Block::Patch { summary, files } => {
            out.push(
                RenderedLine::chip(LineKind::Files, Rail::Mark("± "), summary.clone())
                    .with_meta(plural(files.len(), "file")),
            );
            for file in files {
                out.push(file_row(file));
            }
        }
        Block::Unknown { raw } => out.push(RenderedLine::chip(
            LineKind::Raw,
            Rail::Mark("? "),
            truncate(&compact_json(raw), 200),
        )),
    }
}

/// Blocks that are the agent operating a tool, not the agent talking.
fn is_machinery(block: &Block) -> bool {
    matches!(
        block,
        Block::ToolCall { .. }
            | Block::ToolResult { .. }
            | Block::File { .. }
            | Block::Image { .. }
            | Block::Unknown { .. }
    )
}

/// First line of tool output worth putting on a folded row.
///
/// Real output opens with rule-offs, prompts and single punctuation; those
/// say nothing next to the tool's own name, so the first line carrying two
/// or more word characters wins instead.
fn headline(output: &str) -> Option<String> {
    output
        .lines()
        .take(8)
        .map(str::trim)
        .find(|line| line.chars().filter(|c| c.is_alphanumeric()).count() >= 2)
        .map(|line| truncate(line, CHIP_BUDGET))
}

/// What a tool call actually did, in one phrase.
///
/// Agents write large argument objects; the reader wants the subject — the
/// path, command or pattern — not a JSON dump. The whole object is the
/// fallback, never the default.
fn call_summary(input: &serde_json::Value) -> String {
    const SUBJECTS: &[&str] = &[
        "path",
        "file",
        "file_path",
        "filePath",
        "command",
        "cmd",
        "pattern",
        "query",
        "url",
        "name",
        "title",
        "i",
    ];
    // A delegating call's subject is who it delegated to.
    if let Some(tasks) = input.get("tasks").and_then(|value| value.as_array()) {
        let names: Vec<_> = tasks
            .iter()
            .filter_map(|task| {
                task.get("name")
                    .or_else(|| task.get("id"))
                    .or_else(|| task.get("agent"))
                    .and_then(serde_json::Value::as_str)
            })
            .collect();
        if !names.is_empty() {
            return truncate(&names.join(", "), CHIP_BUDGET);
        }
    }
    let subject = SUBJECTS.iter().find_map(|key| {
        input
            .get(*key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    });
    match subject {
        Some(subject) => elide(subject.lines().next().unwrap_or_default(), CHIP_BUDGET),
        None => truncate(&compact_json(input), CHIP_BUDGET),
    }
}

/// Markdown structure agents actually emit, rendered as structure.
///
/// Headings, bullets, quotes and fenced code are the difference between a
/// wall of text and a readable answer. Markers themselves never reach the
/// page: the rail already says "this is code", emphasis becomes weight, and
/// backticks were never content.
fn push_prose(out: &mut Vec<RenderedLine>, rail: Rail, text: &str) {
    let mut fence: Option<char> = None;
    // Agents hard-wrap prose at their own width. Markdown says a single
    // newline is not a break, so a paragraph is collected and rewrapped to
    // this reader's measure instead of inheriting someone else's raggedness.
    let mut paragraph = String::new();
    let flush_paragraph = |out: &mut Vec<RenderedLine>, paragraph: &mut String| {
        if !paragraph.is_empty() {
            out.push(RenderedLine::prose(LineKind::Text, rail, paragraph));
            paragraph.clear();
        }
    };

    // Lookahead by one row: a table is only a table because the row under
    // its header is a delimiter, so the scan needs to see the next line.
    let rows: Vec<&str> = text.lines().collect();
    let mut at = 0;
    while at < rows.len() {
        let raw = rows[at];
        at += 1;
        let trimmed = raw.trim_end();
        let flush = trimmed.trim_start();

        if let Some(marker) = fence_marker(flush) {
            flush_paragraph(out, &mut paragraph);
            match fence {
                Some(open) if open == marker => fence = None,
                // A nested fence of the other spelling is code, not a fence.
                Some(_) => out.push(RenderedLine::new(LineKind::Code, Rail::Pipe, trimmed)),
                None => fence = Some(marker),
            }
            continue;
        }
        if fence.is_some() {
            out.push(RenderedLine::new(LineKind::Code, Rail::Pipe, trimmed));
            continue;
        }
        if flush.is_empty() {
            flush_paragraph(out, &mut paragraph);
            // One blank line is rhythm; three are a hole in the page.
            if !out.last().is_some_and(RenderedLine::is_blank) {
                out.push(RenderedLine::spacer());
            }
            continue;
        }
        if let Some(level) = heading_level(flush) {
            flush_paragraph(out, &mut paragraph);
            if !out.last().is_some_and(RenderedLine::is_blank) {
                out.push(RenderedLine::spacer());
            }
            let title = flush[level..].trim_start_matches('#').trim();
            out.push(RenderedLine::prose(LineKind::Heading, rail, title));
            continue;
        }
        if is_horizontal_rule(flush) {
            flush_paragraph(out, &mut paragraph);
            out.push(RenderedLine::rule(String::new()));
            continue;
        }
        if let Some(rest) = flush
            .strip_prefix("> ")
            .or_else(|| flush.strip_prefix('>').filter(|rest| rest.is_empty()))
        {
            flush_paragraph(out, &mut paragraph);
            out.push(RenderedLine::prose(LineKind::Quote, Rail::Pipe, rest));
            continue;
        }
        if let Some(item) = bullet_body(flush) {
            flush_paragraph(out, &mut paragraph);
            let indent = " ".repeat(raw.len() - raw.trim_start().len());
            out.push(RenderedLine::prose(
                LineKind::Bullet,
                rail,
                &format!("{indent}• {item}"),
            ));
            continue;
        }
        // A GFM table: the delimiter row under the header is what makes it
        // one. The rows go through as markdown — only [`fit`] knows how wide
        // a column may be — and every row of the run stays contiguous.
        if table_cells(flush).is_some_and(|cells| !cells.is_empty())
            && rows
                .get(at)
                .is_some_and(|next| table_alignment(next).is_some())
        {
            flush_paragraph(out, &mut paragraph);
            if !out.last().is_some_and(RenderedLine::is_blank) {
                out.push(RenderedLine::spacer());
            }
            out.push(RenderedLine::new(LineKind::Table, rail, flush));
            while let Some(row) = rows.get(at).map(|row| row.trim()) {
                if table_cells(row).is_none() {
                    break;
                }
                out.push(RenderedLine::new(LineKind::Table, rail, row));
                at += 1;
            }
            out.push(RenderedLine::spacer());
            continue;
        }
        // A pipe drawing with no delimiter row, or a tree: it is a picture,
        // and rewrapping destroys it.
        if flush.starts_with('|') || flush.contains('\t') {
            flush_paragraph(out, &mut paragraph);
            out.push(RenderedLine::new(LineKind::Text, rail, trimmed));
            continue;
        }
        if !paragraph.is_empty() {
            paragraph.push(' ');
        }
        paragraph.push_str(flush);
    }
    flush_paragraph(out, &mut paragraph);
}

/// Strip inline markdown, returning clean text and what it emphasised.
///
/// Only closed, non-empty pairs count; an unmatched `*` or a `snake_case`
/// underscore is literal text, because agents write both constantly.
fn inline(text: &str) -> (String, Vec<Accent>) {
    const MARKERS: [(&str, Emphasis); 5] = [
        ("**", Emphasis::Strong),
        ("__", Emphasis::Strong),
        ("`", Emphasis::Code),
        ("*", Emphasis::Soft),
        ("_", Emphasis::Soft),
    ];
    let mut clean = String::with_capacity(text.len());
    let mut accents = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let opening = MARKERS.into_iter().find(|(marker, _)| {
            if !rest.starts_with(marker) {
                return false;
            }
            // `_` binds words together far more often than it emphasises.
            if marker.starts_with('_')
                && clean
                    .chars()
                    .last()
                    .is_some_and(|previous| previous.is_alphanumeric() || previous == '_')
            {
                return false;
            }
            let body = &rest[marker.len()..];
            !body.starts_with(char::is_whitespace) && body.find(marker).is_some_and(|end| end > 0)
        });
        if let Some((marker, emphasis)) = opening {
            let body = &rest[marker.len()..];
            let end = body.find(marker).expect("checked above");
            let start = clean.len();
            // Code is literal; anything else may nest.
            if emphasis == Emphasis::Code {
                clean.push_str(&body[..end]);
            } else {
                let (nested_text, nested) = inline(&body[..end]);
                clean.push_str(&nested_text);
                accents.extend(nested.into_iter().map(|accent| Accent {
                    start: accent.start + start,
                    end: accent.end + start,
                    ..accent
                }));
            }
            accents.push(Accent {
                start,
                end: clean.len(),
                emphasis,
            });
            rest = &body[end + marker.len()..];
            continue;
        }
        let character = rest.chars().next().expect("non-empty");
        clean.push(character);
        rest = &rest[character.len_utf8()..];
    }
    // Drawing walks runs in order and takes the outermost accent first.
    accents.sort_by_key(|accent| (accent.start, std::cmp::Reverse(accent.end)));
    accents.dedup_by(|later, earlier| later.start < earlier.end);
    (clean, accents)
}

fn fence_marker(line: &str) -> Option<char> {
    ["```", "~~~"]
        .into_iter()
        .find(|marker| line.starts_with(marker))
        .and_then(|marker| marker.chars().next())
}

fn heading_level(line: &str) -> Option<usize> {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    (1..=6).contains(&hashes).then_some(hashes)
}

fn is_horizontal_rule(line: &str) -> bool {
    line.len() >= 3
        && ['-', '*', '_']
            .into_iter()
            .any(|glyph| line.chars().all(|c| c == glyph))
}

/// Body of a list item, for either bullet spelling or an ordered marker.
fn bullet_body(line: &str) -> Option<&str> {
    if let Some(rest) = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("+ "))
    {
        return Some(rest.trim_start());
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 && line[digits..].starts_with(". ") {
        return Some(line[digits + 2..].trim_start());
    }
    None
}

/// Subagent transcripts a delegating tool call spawned.
///
/// OMP writes each child as `<N>-<TaskId>.jsonl` beside the parent, and the
/// parent's `task` call names the same ids in `tasks[].id` — that shared name
/// is the whole join. A call whose name is unknown, or whose task has no
/// transcript, contributes nothing rather than a dead link.
fn subagent_targets(
    name: &str,
    input: &serde_json::Value,
    subagents: &HashMap<String, (SourceId, String)>,
) -> Vec<(String, (SourceId, String))> {
    if !matches!(name, "task" | "agent" | "subtask") {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Some(tasks) = input.get("tasks").and_then(|v| v.as_array()) {
        for task in tasks {
            // Both spellings occur in real OMP calls: `tasks[].id` in some
            // versions, `tasks[].name` in others. Either one is the filename
            // the child transcript was written under.
            let named = task
                .get("id")
                .or_else(|| task.get("name"))
                .and_then(|v| v.as_str());
            if let Some(name) = named
                && let Some(key) = subagents.get(name)
            {
                out.push((name.to_owned(), key.clone()));
            }
        }
    }
    // Single-task forms name the agent instead of a task list.
    if out.is_empty()
        && let Some(id) = input.get("agent").and_then(|v| v.as_str())
        && let Some(key) = subagents.get(id)
    {
        out.push((id.to_owned(), key.clone()));
    }
    out
}

fn compact_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, v)| format!("{k}={}", truncate(&scalar(v), 60)))
            .collect::<Vec<_>>()
            .join(", "),
        other => scalar(other),
    }
}

fn scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.replace('\n', "⏎"),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_records_are_admitted_to_the_reader() {
        let mut transcript = Transcript::new("t", SourceId::Codex);
        transcript.unknown_records = 2;
        let mut turn = Turn::new(Role::System);
        turn.blocks.push(Block::Unknown {
            raw: serde_json::json!({"type": "from_the_future"}),
        });
        transcript.push(turn);
        let lines = render(
            &transcript,
            true,
            true,
            false,
            &HashMap::new(),
            UtcOffset::UTC,
        );
        assert!(lines.iter().any(|l| l.kind == LineKind::Raw));
        assert!(lines.iter().any(|l| l.text.contains("does not understand")));
    }

    /// A folded call or result is a summary. Reflowing 400 bytes of JSON
    /// across four rows would make the fold pointless.
    #[test]
    fn a_folded_tool_row_stays_one_row_at_any_width() {
        let mut transcript = Transcript::new("t", SourceId::Omp);
        let mut turn = Turn::new(Role::Assistant);
        turn.blocks.push(Block::ToolCall {
            name: "github_pull_request_read".into(),
            input: serde_json::json!({"method": "get_review_comments", "owner": "tylkocom", "perPage": 100}),
        });
        turn.blocks.push(Block::ToolResult {
            name: Some("github_pull_request_read".into()),
            ok: true,
            output: format!(
                "{{\"review_threads\":[{}]}}",
                "{\"id\":\"PRRT\"},".repeat(20)
            ),
        });
        transcript.push(turn);
        let lines = render(
            &transcript,
            true,
            true,
            false,
            &HashMap::new(),
            UtcOffset::UTC,
        );
        let chips = lines.iter().filter(|line| line.clip).count();
        assert_eq!(chips, 2, "the call and its result");
        for width in [24, 48, 200] {
            let rows = wrap_rendered(lines.clone(), width)
                .into_iter()
                .filter(|line| line.clip)
                .count();
            assert_eq!(rows, chips, "chips never reflow at {width} cells");
        }
    }

    #[test]
    fn markdown_becomes_structure_not_punctuation() {
        let mut transcript = Transcript::new("t", SourceId::Omp);
        let mut turn = Turn::new(Role::Assistant);
        turn.blocks.push(Block::text(
            "## Findings\n\n- first with `code`\n- second\n\n> quoted\n\n```rust\nfn main() {}\n```\n\n**plain** text with snake_case intact",
        ));
        transcript.push(turn);
        let lines = render(
            &transcript,
            true,
            true,
            false,
            &HashMap::new(),
            UtcOffset::UTC,
        );
        let of = |kind: LineKind| {
            lines
                .iter()
                .filter(move |l| l.kind == kind)
                .collect::<Vec<_>>()
        };

        assert_eq!(of(LineKind::Heading)[0].text, "Findings");
        assert_eq!(
            of(LineKind::Bullet)
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>(),
            ["• first with code", "• second"]
        );
        assert_eq!(of(LineKind::Quote)[0].text, "quoted");
        let code = of(LineKind::Code);
        assert_eq!(code.len(), 1, "fence markers are not content");
        assert_eq!(code[0].text, "fn main() {}");
        assert_eq!(code[0].rail, Rail::Pipe);
        assert!(
            !lines.iter().any(|l| l.text.contains("```")),
            "backticks never reach the page"
        );

        let text = of(LineKind::Text)[0];
        assert_eq!(
            text.text, "plain text with snake_case intact",
            "markers leave, an identifier stays whole"
        );
        assert_eq!(
            text.runs().collect::<Vec<_>>(),
            [
                ("plain", Some(Emphasis::Strong)),
                (" text with snake_case intact", None)
            ]
        );
        let bullet = of(LineKind::Bullet)[0];
        assert_eq!(
            bullet.runs().collect::<Vec<_>>(),
            [("• first with ", None), ("code", Some(Emphasis::Code))]
        );
    }

    /// Status text is copy, and copy that says "1 matches" is a typo the
    /// user sees. `-es` after a sibilant is the only irregular case here.
    #[test]
    fn counts_read_like_english() {
        assert_eq!(plural(1, "session"), "1 session");
        assert_eq!(plural(0, "session"), "0 sessions");
        assert_eq!(plural(2, "match"), "2 matches");
    }

    #[test]
    fn a_tool_call_shows_its_subject_not_its_json() {
        let mut transcript = Transcript::new("t", SourceId::Omp);
        let mut turn = Turn::new(Role::Assistant);
        turn.blocks.push(Block::ToolCall {
            name: "read".into(),
            input: serde_json::json!({"i": "Reading the reader", "path": "src/ui.rs:1-40"}),
        });
        turn.blocks.push(Block::ToolCall {
            name: "mystery".into(),
            input: serde_json::json!({"depth": 3}),
        });
        transcript.push(turn);
        let lines = render(
            &transcript,
            true,
            true,
            false,
            &HashMap::new(),
            UtcOffset::UTC,
        );
        let calls: Vec<_> = lines.iter().filter(|l| l.kind == LineKind::Tool).collect();
        assert_eq!(calls[0].text, "read  src/ui.rs:1-40");
        assert_eq!(
            calls[1].text, "mystery  depth=3",
            "an unknown shape still says something"
        );
    }

    /// Turns are separated by a hairline, and a new day is named once.
    #[test]
    fn turns_are_separated_and_days_are_named_once() {
        let mut transcript = Transcript::new("t", SourceId::Omp);
        let day = time::macros::datetime!(2026 - 09 - 15 09:30 UTC);
        for (role, stamp) in [
            (Role::User, day),
            (Role::Assistant, day + time::Duration::minutes(1)),
            (Role::User, day + time::Duration::days(1)),
        ] {
            let mut turn = Turn::new(role);
            turn.timestamp = Some(stamp);
            turn.blocks.push(Block::text("something"));
            transcript.push(turn);
        }
        let rules: Vec<_> = lines_of(&transcript)
            .into_iter()
            .filter(|l| l.kind == LineKind::Rule)
            .map(|l| l.text)
            .collect();
        assert_eq!(rules, ["2026-09-15", "2026-09-16"]);
    }

    fn lines_of(transcript: &Transcript) -> Vec<RenderedLine> {
        render(
            transcript,
            true,
            true,
            false,
            &HashMap::new(),
            UtcOffset::UTC,
        )
    }

    #[test]
    fn a_task_call_links_to_the_subagent_it_spawned() {
        let mut subagents = HashMap::new();
        subagents.insert(
            "SpecReview".to_string(),
            (SourceId::Omp, "child-1".to_string()),
        );
        // Real calls name the task under `id` in some OMP versions and
        // `name` in others.
        for key in ["id", "name"] {
            let input = serde_json::json!({ "tasks": [{ key: "SpecReview" }] });
            let found = subagent_targets("task", &input, &subagents);
            assert_eq!(found.len(), 1, "{key} spelling");
            assert_eq!(found[0].1.1, "child-1");
        }
        // A task with no transcript is not a link.
        let missing = serde_json::json!({ "tasks": [{ "id": "Nope" }] });
        assert!(subagent_targets("task", &missing, &subagents).is_empty());
        // Neither is an unrelated tool.
        let input = serde_json::json!({ "tasks": [{ "id": "SpecReview" }] });
        assert!(subagent_targets("bash", &input, &subagents).is_empty());
    }
}
