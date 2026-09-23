//! Deterministic transcript renderers for reports and file sharing.

use std::path::Path;

use crate::model::{Block, FileChange, SessionSummary, Transcript, Usage};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Markdown,
    Html,
}

/// Render a transcript without consulting the clock or the source store.
///
/// `session` contributes tier-1 metadata that a normalized transcript does not
/// retain, such as branch and its cached usage total.
pub fn render(transcript: &Transcript, session: Option<&SessionSummary>, format: Format) -> String {
    match format {
        Format::Markdown => render_markdown(transcript, session),
        Format::Html => render_html(transcript, session),
    }
}

fn render_markdown(transcript: &Transcript, session: Option<&SessionSummary>) -> String {
    let mut out = String::new();
    push_front_matter(&mut out, transcript, session);
    out.push('\n');
    out.push_str("# ");
    out.push_str(
        transcript
            .title
            .as_deref()
            .or_else(|| session.and_then(|s| s.title.as_deref()))
            .unwrap_or(&transcript.id),
    );
    out.push_str("\n\n");

    for turn in &transcript.turns {
        out.push_str("## ");
        out.push_str(turn.role.label());
        if let Some(timestamp) = turn.timestamp {
            out.push_str(" — ");
            out.push_str(&timestamp.to_string());
        }
        out.push_str("\n\n");
        let mut position = 0;
        while position < turn.blocks.len() {
            match &turn.blocks[position] {
                Block::ToolCall { name, input } => {
                    let output = match turn.blocks.get(position + 1) {
                        Some(Block::ToolResult { output, .. }) => Some(output.as_str()),
                        _ => None,
                    };
                    position += if output.is_some() { 2 } else { 1 };
                    render_markdown_tool_call(&mut out, name, input, output);
                }
                block => {
                    position += 1;
                    render_markdown_block(&mut out, block);
                }
            }
        }
    }
    out
}

fn push_front_matter(out: &mut String, transcript: &Transcript, session: Option<&SessionSummary>) {
    let workspace = transcript
        .workspace
        .as_deref()
        .or_else(|| session.and_then(|summary| summary.workspace.as_deref()));
    let model = session
        .and_then(|summary| summary.model.as_deref())
        .or_else(|| {
            transcript
                .turns
                .iter()
                .rev()
                .find_map(|turn| turn.model.as_deref())
        });
    let branch = session.and_then(|summary| summary.branch.as_deref());
    let updated = session
        .map(|summary| summary.updated.to_string())
        .or_else(|| {
            transcript
                .turns
                .iter()
                .filter_map(|turn| turn.timestamp)
                .max()
                .map(|time| time.to_string())
        });
    let usage = transcript
        .usage()
        .or_else(|| session.and_then(|summary| summary.usage));

    out.push_str("---\n");
    push_yaml_field(out, "agent", Some(transcript.source.name()));
    push_yaml_field(
        out,
        "workspace",
        workspace
            .map(|path| path.to_string_lossy().into_owned())
            .as_deref(),
    );
    push_yaml_field(out, "model", model);
    push_yaml_field(out, "branch", branch);
    push_yaml_field(out, "updated", updated.as_deref());
    if let Some(usage) = usage {
        out.push_str("cost: ");
        out.push_str(&usage.cost.to_string());
        out.push('\n');
        out.push_str("tokens:\n");
        out.push_str("  input: ");
        out.push_str(&usage.input.to_string());
        out.push('\n');
        out.push_str("  output: ");
        out.push_str(&usage.output.to_string());
        out.push('\n');
    } else {
        out.push_str("cost: null\n");
        out.push_str("tokens:\n  input: null\n  output: null\n");
    }
    out.push_str("---\n");
}

fn push_yaml_field(out: &mut String, name: &str, value: Option<&str>) {
    out.push_str(name);
    out.push_str(": ");
    match value {
        Some(value) => push_yaml_string(out, value),
        None => out.push_str("null"),
    }
    out.push('\n');
}

// JSON quoted strings are also valid YAML scalars, and avoid a report title or
// workspace accidentally becoming a front-matter key.
fn push_yaml_string(out: &mut String, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if character.is_control() => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                out.push_str("\\u");
                let code = character as u32;
                out.push(HEX[(code >> 12) as usize] as char);
                out.push(HEX[((code >> 8) & 0xf) as usize] as char);
                out.push(HEX[((code >> 4) & 0xf) as usize] as char);
                out.push(HEX[(code & 0xf) as usize] as char);
            }
            character => out.push(character),
        }
    }
    out.push('"');
}

fn render_markdown_block(out: &mut String, block: &Block) {
    match block {
        Block::Text { text } => push_markdown_prose(out, text),
        Block::Thinking { text } => {
            out.push_str("<details><summary>Thinking</summary>\n\n");
            out.push_str(text);
            out.push_str("\n\n</details>\n\n");
        }
        Block::ToolCall { name, input } => render_markdown_tool_call(out, name, input, None),
        Block::ToolResult { name, ok, output } => {
            out.push_str("<details><summary>← ");
            out.push_str(&escape_html(name.as_deref().unwrap_or("tool")));
            out.push_str(if *ok { " (ok)" } else { " (failed)" });
            out.push_str("</summary>\n\n");
            push_fenced(out, output, None);
            out.push_str("</details>\n\n");
        }
        Block::File { path } => {
            out.push_str("File: `");
            out.push_str(&path.to_string_lossy());
            out.push_str("`\n\n");
        }
        Block::Image { label } => {
            out.push_str("Image blob: `");
            out.push_str(label);
            out.push_str("`\n\n");
        }
        Block::Patch { summary, files } => {
            out.push_str("### Changes\n\n");
            push_markdown_prose(out, summary);
            if files.is_empty() {
                out.push_str("- No changed-file list recorded.\n\n");
            } else {
                for file in files {
                    push_markdown_file_change(out, file);
                }
                out.push('\n');
            }
        }
        Block::Unknown { raw } => {
            out.push_str("<details><summary>Unrecognized record</summary>\n\n");
            push_fenced(out, &raw.to_string(), Some("json"));
            out.push_str("</details>\n\n");
        }
    }
}

fn push_markdown_prose(out: &mut String, text: &str) {
    out.push_str(text);
    if !text.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
}

fn render_markdown_tool_call(
    out: &mut String,
    name: &str,
    input: &serde_json::Value,
    output: Option<&str>,
) {
    out.push_str("<details><summary>→ ");
    out.push_str(&escape_html(name));
    out.push('(');
    out.push_str(&escape_html(&input.to_string()));
    out.push_str(")</summary>\n\n");
    match output {
        Some(output) => push_fenced(out, output, None),
        None => push_fenced(out, &input.to_string(), Some("json")),
    }
    out.push_str("</details>\n\n");
}

fn push_markdown_file_change(out: &mut String, file: &FileChange) {
    out.push_str("- `");
    out.push_str(&file.path.to_string_lossy());
    out.push_str("` +");
    out.push_str(&file.added.to_string());
    out.push_str("/−");
    out.push_str(&file.deleted.to_string());
    out.push('\n');
}

fn push_fenced(out: &mut String, text: &str, language: Option<&str>) {
    let fence = fence_for(text);
    out.push_str(&fence);
    if let Some(language) = language {
        out.push_str(language);
    }
    out.push('\n');
    out.push_str(text);
    if !text.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&fence);
    out.push_str("\n\n");
}

fn fence_for(text: &str) -> String {
    let mut longest = 0;
    let mut current = 0;
    for character in text.chars() {
        if character == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    "`".repeat(longest.max(3) + 1)
}

fn render_html(transcript: &Transcript, session: Option<&SessionSummary>) -> String {
    let title = transcript
        .title
        .as_deref()
        .or_else(|| session.and_then(|summary| summary.title.as_deref()))
        .unwrap_or(&transcript.id);
    let workspace = transcript
        .workspace
        .as_deref()
        .or_else(|| session.and_then(|summary| summary.workspace.as_deref()));
    let model = session
        .and_then(|summary| summary.model.as_deref())
        .or_else(|| {
            transcript
                .turns
                .iter()
                .rev()
                .find_map(|turn| turn.model.as_deref())
        });
    let branch = session.and_then(|summary| summary.branch.as_deref());
    let updated = session
        .map(|summary| summary.updated.to_string())
        .or_else(|| {
            transcript
                .turns
                .iter()
                .filter_map(|turn| turn.timestamp)
                .max()
                .map(|time| time.to_string())
        });
    let usage = transcript
        .usage()
        .or_else(|| session.and_then(|summary| summary.usage));

    let mut out = String::from(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>",
    );
    out.push_str(&escape_html(title));
    out.push_str("</title><style>body{max-width:76rem;margin:2rem auto;padding:0 1rem;font:16px/1.5 system-ui,sans-serif;color:#1f2328}h1,h2{line-height:1.2}dl{display:grid;grid-template-columns:max-content 1fr;gap:.25rem 1rem;background:#f6f8fa;padding:1rem}dt{font-weight:600}dd{margin:0}section{border-top:1px solid #d0d7de;padding:1rem 0}pre{overflow:auto;padding:1rem;background:#f6f8fa;white-space:pre-wrap}details{margin:.75rem 0}summary{cursor:pointer}ul{padding-left:1.5rem}.status{color:#57606a}</style></head><body>\n<h1>");
    out.push_str(&escape_html(title));
    out.push_str("</h1>\n<dl>");
    push_html_field(&mut out, "Agent", transcript.source.name());
    push_html_field(
        &mut out,
        "Workspace",
        workspace.map(path_text).as_deref().unwrap_or("—"),
    );
    push_html_field(&mut out, "Model", model.unwrap_or("—"));
    push_html_field(&mut out, "Branch", branch.unwrap_or("—"));
    push_html_field(&mut out, "Updated", updated.as_deref().unwrap_or("—"));
    push_html_usage(&mut out, usage);
    out.push_str("</dl>\n");

    for turn in &transcript.turns {
        out.push_str("<section><h2>");
        out.push_str(turn.role.label());
        if let Some(timestamp) = turn.timestamp {
            out.push_str(" <span class=\"status\">— ");
            out.push_str(&escape_html(&timestamp.to_string()));
            out.push_str("</span>");
        }
        out.push_str("</h2>\n");
        for block in &turn.blocks {
            render_html_block(&mut out, block);
        }
        out.push_str("</section>\n");
    }
    out.push_str("</body></html>\n");
    out
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn push_html_field(out: &mut String, name: &str, value: &str) {
    out.push_str("<dt>");
    out.push_str(&escape_html(name));
    out.push_str("</dt><dd>");
    out.push_str(&escape_html(value));
    out.push_str("</dd>");
}

fn push_html_usage(out: &mut String, usage: Option<Usage>) {
    if let Some(usage) = usage {
        push_html_field(out, "Cost", &usage.cost.to_string());
        push_html_field(out, "Input tokens", &usage.input.to_string());
        push_html_field(out, "Output tokens", &usage.output.to_string());
    } else {
        push_html_field(out, "Cost", "—");
        push_html_field(out, "Input tokens", "—");
        push_html_field(out, "Output tokens", "—");
    }
}

fn render_html_block(out: &mut String, block: &Block) {
    match block {
        Block::Text { text } => {
            out.push_str("<p>");
            out.push_str(&escape_html(text).replace('\n', "<br>\n"));
            out.push_str("</p>\n");
        }
        Block::Thinking { text } => {
            out.push_str("<details><summary>Thinking</summary><pre>");
            out.push_str(&escape_html(text));
            out.push_str("</pre></details>\n");
        }
        Block::ToolCall { name, input } => {
            out.push_str("<details><summary>→ ");
            out.push_str(&escape_html(name));
            out.push('(');
            out.push_str(&escape_html(&input.to_string()));
            out.push_str(")</summary><pre><code class=\"language-json\">");
            out.push_str(&escape_html(&input.to_string()));
            out.push_str("</code></pre></details>\n");
        }
        Block::ToolResult { name, ok, output } => {
            out.push_str("<details><summary>← ");
            out.push_str(&escape_html(name.as_deref().unwrap_or("tool")));
            out.push_str(if *ok { " (ok)" } else { " (failed)" });
            out.push_str("</summary><pre><code>");
            out.push_str(&escape_html(output));
            out.push_str("</code></pre></details>\n");
        }
        Block::File { path } => {
            out.push_str("<p>File: <code>");
            out.push_str(&escape_html(&path.to_string_lossy()));
            out.push_str("</code></p>\n");
        }
        Block::Image { label } => {
            out.push_str("<p>Image blob: <code>");
            out.push_str(&escape_html(label));
            out.push_str("</code></p>\n");
        }
        Block::Patch { summary, files } => {
            out.push_str("<h3>Changes</h3><p>");
            out.push_str(&escape_html(summary));
            out.push_str("</p>");
            if files.is_empty() {
                out.push_str("<p class=\"status\">No changed-file list recorded.</p>\n");
            } else {
                out.push_str("<ul>");
                for file in files {
                    out.push_str("<li><code>");
                    out.push_str(&escape_html(&file.path.to_string_lossy()));
                    out.push_str("</code> +");
                    out.push_str(&file.added.to_string());
                    out.push_str("/−");
                    out.push_str(&file.deleted.to_string());
                    out.push_str("</li>");
                }
                out.push_str("</ul>\n");
            }
        }
        Block::Unknown { raw } => {
            out.push_str("<details><summary>Unrecognized record</summary><pre><code class=\"language-json\">");
            out.push_str(&escape_html(&raw.to_string()));
            out.push_str("</code></pre></details>\n");
        }
    }
}

fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            character => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Turn;
    use crate::model::{Role, SourceId};

    #[test]
    fn html_escapes_transcript_text() {
        let mut transcript = Transcript::new("session", SourceId::Omp);
        let mut turn = Turn::new(Role::Assistant);
        turn.blocks
            .push(Block::text("<script>alert(1)</script> & \"quoted\""));
        transcript.push(turn);

        let html = render(&transcript, None, Format::Html);
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(html.contains("&amp;"));
        assert!(html.contains("&quot;quoted&quot;"));
        assert!(!html.contains("<script>alert(1)</script>"));
    }

    #[test]
    fn markdown_uses_a_fence_longer_than_tool_output() {
        let mut transcript = Transcript::new("session", SourceId::Omp);
        let mut turn = Turn::new(Role::Tool);
        turn.blocks.push(Block::ToolResult {
            name: Some("read".into()),
            ok: true,
            output: "before\n```\nafter".into(),
        });
        transcript.push(turn);

        let markdown = render(&transcript, None, Format::Markdown);
        assert!(markdown.contains("````\nbefore\n```\nafter\n````"));
    }
}
