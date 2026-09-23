//! Fitting reader lines to a column: word wrapping, truncation, elision.

use ratatui::style::Style;
use ratatui::text::Span;

use super::table::lay_out_tables;
use super::{Accent, LineKind, RenderedLine};

/// Fit rendered lines to a column: tables are laid out, prose is wrapped.
///
/// Everything width-dependent happens here, so [`render`](super::render)
/// stays a statement about structure. `usize::MAX` means "no column at all"
/// — tables take their natural width and nothing wraps — which is what a
/// text dump wants.
pub fn fit(lines: Vec<RenderedLine>, width: usize) -> Vec<RenderedLine> {
    wrap_rendered(lay_out_tables(lines, width), width)
}

/// Shorten a subject to `width`, keeping the end of a path.
///
/// `…/cart/CartItem.vue` answers "which file" where a left-truncated
/// `/home/user/Documents/TREE/feat_…` answers nothing.
pub fn elide(subject: &str, width: usize) -> String {
    let count = subject.chars().count();
    if count <= width {
        return subject.to_owned();
    }
    if !subject.contains('/') {
        return truncate(subject, width);
    }
    let tail: String = subject
        .chars()
        .skip(count + 1 - width)
        .skip_while(|&c| c != '/')
        .collect();
    let tail = if tail.is_empty() {
        subject.chars().skip(count + 1 - width).collect()
    } else {
        tail
    };
    format!("…{tail}")
}

/// Wrap before drawing so scrolling and link selection use displayed rows.
///
/// `width` is the whole reader column; the gutter is subtracted per line, so
/// a code rail and a bare spacer wrap against the same right edge. Header
/// lines carrying `meta` are never wrapped: the renderer sets their detail
/// flush right and truncates the label instead.
pub(crate) fn wrap_rendered(lines: Vec<RenderedLine>, width: usize) -> Vec<RenderedLine> {
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let width = width.saturating_sub(line.rail.width()).max(1);
        if line.clip || line.meta.is_some() || Span::raw(line.text.as_str()).width() <= width {
            out.push(line);
            continue;
        }
        let indentation = line.text.bytes().take_while(|&b| b == b' ').count();
        // A wrapped bullet hangs under its own text rather than sliding back
        // under the dot: `• ` is two cells of the first row the rest clears.
        // A record value hangs for the same reason — so a continuation is
        // never mistaken for the next field.
        let hang = if matches!(line.kind, LineKind::Bullet | LineKind::Table) {
            2
        } else {
            0
        };
        let indent = " ".repeat((indentation + hang).min(width.saturating_sub(2)));
        let mut remaining = line.text.as_str();
        let mut first = true;
        while !remaining.is_empty() {
            let prefix = if first { "" } else { indent.as_str() };
            let available = width - prefix.len();
            let span = Span::raw(remaining);
            let mut cells = 0;
            let mut end = 0;
            let mut word_end = 0;
            let mut has_text = false;
            for grapheme in span.styled_graphemes(Style::default()) {
                let next = cells + Span::raw(grapheme.symbol).width();
                if next > available && end > 0 {
                    break;
                }
                cells = next;
                // Ratatui skips control graphemes; keep offsets in the original text.
                end = grapheme.symbol.as_ptr() as usize - remaining.as_ptr() as usize
                    + grapheme.symbol.len();
                if grapheme.symbol.chars().all(char::is_whitespace) {
                    if has_text {
                        word_end = end;
                    }
                } else {
                    has_text = true;
                }
            }
            if end == 0 {
                // Only invisible control characters remain.
                break;
            }
            if end < remaining.len() && word_end > 0 {
                end = word_end;
            }
            // Accents index the unwrapped text; move them onto this row.
            let consumed = remaining.as_ptr() as usize - line.text.as_ptr() as usize;
            // Filtering guarantees both clamped bounds are at least `consumed`,
            // so rebasing onto the row never underflows.
            let accents = line
                .accents
                .iter()
                .filter(|accent| accent.end > consumed && accent.start < consumed + end)
                .map(|accent| Accent {
                    start: accent.start.max(consumed) - consumed + prefix.len(),
                    end: accent.end.min(consumed + end) - consumed + prefix.len(),
                    ..*accent
                })
                .collect();
            let mut text = String::with_capacity(prefix.len() + end);
            text.push_str(prefix);
            text.push_str(&remaining[..end]);
            out.push(RenderedLine {
                kind: line.kind,
                rail: line.rail,
                text,
                accents,
                meta: None,
                link: line.link.clone(),
                clip: false,
                first,
            });
            remaining = remaining[end..].trim_start_matches(char::is_whitespace);
            first = false;
        }
    }
    out
}

pub fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    s.chars()
        .take(width.saturating_sub(1))
        .chain(['…'])
        .collect()
}

/// Ellipsize by terminal cells without splitting a grapheme cluster.
///
/// Unlike [`truncate`], which counts characters, this is for text that will
/// occupy a known number of screen cells: a CJK character is two of them.
pub fn truncate_cells(s: &str, width: usize) -> String {
    let span = Span::raw(s);
    if span.width() <= width {
        return s.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut used = 0;
    for grapheme in span.styled_graphemes(Style::default()) {
        let cells = Span::raw(grapheme.symbol).width();
        if used + cells >= width {
            break;
        }
        result.push_str(grapheme.symbol);
        used += cells;
    }
    result.push('…');
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::reader::Rail;

    /// The gutter is part of the line, so it has to come out of the measure —
    /// otherwise every railed line overflows its pane by two cells.
    #[test]
    fn the_gutter_is_subtracted_from_the_measure() {
        let text = "alpha beta gamma delta";
        let railed = wrap_rendered(
            vec![RenderedLine::new(LineKind::Code, Rail::Pipe, text)],
            12,
        );
        assert!(
            railed
                .iter()
                .all(|line| Span::raw(line.text.as_str()).width() + line.rail.width() <= 12)
        );
        assert!(railed.len() > 1, "22 cells cannot fit in 10");
        assert_eq!(railed[0].rail.glyph(railed[0].first), "│ ");
        assert_eq!(
            railed[1].rail.glyph(railed[1].first),
            "│ ",
            "a repeating rail continues down the block"
        );
        let marked = wrap_rendered(
            vec![RenderedLine::new(LineKind::Tool, Rail::Mark("▸ "), text)],
            12,
        );
        assert_eq!(marked[0].rail.glyph(marked[0].first), "▸ ");
        assert_eq!(
            marked[1].rail.glyph(marked[1].first),
            "  ",
            "a mark belongs to the first row only"
        );
    }

    #[test]
    fn a_line_carrying_detail_is_never_wrapped() {
        let lines = wrap_rendered(
            vec![
                RenderedLine::new(
                    LineKind::SpeakerAgent,
                    Rail::Pad,
                    "a model with a long name",
                )
                .with_meta("12:00"),
            ],
            10,
        );
        assert_eq!(lines.len(), 1);
    }

    /// "Which file?" must survive truncation, so a path loses its head.
    #[test]
    fn a_long_path_keeps_its_tail() {
        let elided = elide(
            "/home/dev/src/checkouts/feat_long-branch-name-here/app/cart/CartItem.vue",
            40,
        );
        assert!(elided.starts_with('…'));
        assert!(elided.ends_with("cart/CartItem.vue"), "{elided}");
        assert!(elided.chars().count() <= 40);
        assert_eq!(elide("short/path.rs", 40), "short/path.rs");
        assert_eq!(
            elide("cargo test --lib reader and more words here", 12),
            "cargo test …",
            "prose is cut at the end, not the start"
        );
    }

    #[test]
    fn wrapping_preserves_words_graphemes_and_indentation() {
        let text = "  alpha beta 界界 e\u{301}e\u{301} 👩‍💻👩‍💻 abcdefghijklmnop tail";
        let lines = wrap_rendered(vec![RenderedLine::new(LineKind::Text, Rail::Bare, text)], 8);
        assert!(
            lines
                .iter()
                .all(|line| Span::raw(line.text.as_str()).width() <= 8)
        );
        assert!(lines.iter().all(|line| line.text.starts_with("  ")));
        assert!(lines.iter().any(|line| line.text.trim() == "alpha"));
        assert!(lines.iter().any(|line| line.text.trim() == "beta"));
        let joined: String = lines.iter().map(|line| line.text.as_str()).collect();
        let without_spaces = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        assert_eq!(without_spaces(&joined), without_spaces(text));
        assert!(lines.iter().any(|line| line.text.contains("👩‍💻👩‍💻")));
        assert!(
            lines
                .iter()
                .any(|line| line.text.contains("e\u{301}e\u{301}"))
        );
        // Control graphemes are skipped by Ratatui, but byte offsets must stay valid.
        let controlled = wrap_rendered(
            vec![RenderedLine::new(
                LineKind::Text,
                Rail::Bare,
                "a\t界界界 tail",
            )],
            4,
        );
        assert!(controlled.iter().any(|line| line.text.contains("tail")));
    }

    #[test]
    fn cell_truncation_counts_cells_not_characters() {
        // A CJK character occupies two terminal cells.
        assert_eq!(truncate_cells("界界界界", 5), "界界…");
        assert_eq!(truncate("界界界界", 5), "界界界界");
        assert_eq!(truncate_cells("", 3), "");
    }
}
