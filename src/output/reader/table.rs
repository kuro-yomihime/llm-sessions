//! Markdown tables: parsing GFM rows and laying them out to a column.

use ratatui::text::Span;

use super::wrap::wrap_rendered;
use super::{Accent, Emphasis, LineKind, Rail, RenderedLine, inline};

/// Cells between two table columns. Two is enough to read as a gap and
/// cheap enough that a third column still fits a split pane.
const COLUMN_GAP: usize = 2;

/// Narrowest a column may be squeezed. Below this a cell is mostly
/// ellipsis, and a table of ellipses answers nothing — the rows become
/// labelled blocks instead.
const MIN_COLUMN: usize = 8;

/// How a table column lines up, as its delimiter row spelled it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Align {
    Left,
    Center,
    Right,
}

/// Cells of one table row, or `None` when the line is not a row.
///
/// The outer pipes are optional — GFM accepts `a | b` — and `\|` is a
/// literal pipe inside a cell, which is how an agent writes alternatives in
/// a column without ending it.
pub(super) fn table_cells(line: &str) -> Option<Vec<String>> {
    let line = line.trim();
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut escaped = false;
    let mut divided = false;
    for character in line.chars() {
        match character {
            '\\' if !escaped => escaped = true,
            '|' if !escaped => {
                divided = true;
                cells.push(std::mem::take(&mut cell));
            }
            '|' => {
                // `\|` is the pipe itself, not the end of a cell.
                escaped = false;
                cell.push('|');
            }
            _ => {
                // A backslash before anything else was never an escape.
                if escaped {
                    cell.push('\\');
                    escaped = false;
                }
                cell.push(character);
            }
        }
    }
    if escaped {
        cell.push('\\');
    }
    cells.push(cell);
    if !divided {
        return None;
    }
    // An outer pipe splits off an empty cell that was never written.
    if line.starts_with('|') {
        cells.remove(0);
    }
    if line.ends_with('|') && !line.ends_with("\\|") {
        cells.pop();
    }
    let cells: Vec<String> = cells.iter().map(|cell| cell.trim().to_owned()).collect();
    (!cells.is_empty()).then_some(cells)
}

/// Alignments a delimiter row declares, or `None` for every other row.
pub(super) fn table_alignment(line: &str) -> Option<Vec<Align>> {
    table_cells(line)?
        .iter()
        .map(|cell| {
            let left = cell.starts_with(':');
            let right = cell.len() > 1 && cell.ends_with(':');
            let dashes = cell.trim_start_matches(':').trim_end_matches(':');
            (!dashes.is_empty() && dashes.bytes().all(|byte| byte == b'-')).then_some(
                match (left, right) {
                    (true, true) => Align::Center,
                    (false, true) => Align::Right,
                    _ => Align::Left,
                },
            )
        })
        .collect()
}

/// Replace each run of markdown table rows with its laid-out form.
pub(super) fn lay_out_tables(lines: Vec<RenderedLine>, width: usize) -> Vec<RenderedLine> {
    let mut out = Vec::with_capacity(lines.len());
    let mut table = Vec::new();
    for line in lines {
        if line.kind == LineKind::Table {
            table.push(line);
            continue;
        }
        flush_table(&mut out, &mut table, width);
        out.push(line);
    }
    flush_table(&mut out, &mut table, width);
    out
}

/// Draw one table: a header, the rule under it, and the rows.
///
/// The delimiter row is punctuation — it said how the columns align, and the
/// underline the reader sees replaces it. Cells go through [`inline`] like
/// any other prose, because `**bold**` in a cell is still emphasis.
fn flush_table(out: &mut Vec<RenderedLine>, table: &mut Vec<RenderedLine>, width: usize) {
    let Some(first) = table.first() else {
        return;
    };
    let rail = first.rail;
    let align = table
        .get(1)
        .and_then(|line| table_alignment(&line.text))
        .unwrap_or_default();
    let rows: Vec<Vec<String>> = table
        .drain(..)
        .enumerate()
        .filter(|(index, _)| *index != 1)
        .map(|(_, line)| table_cells(&line.text).unwrap_or_default())
        .collect();
    let columns = rows.iter().map(Vec::len).max().unwrap_or_default();
    if columns == 0 {
        return;
    }
    let cells: Vec<Vec<(String, Vec<Accent>)>> = rows
        .iter()
        .map(|row| {
            (0..columns)
                .map(|column| inline(row.get(column).map_or("", String::as_str)))
                .collect()
        })
        .collect();
    let natural: Vec<usize> = (0..columns)
        .map(|column| {
            cells
                .iter()
                .map(|row| Span::raw(row[column].0.as_str()).width())
                .max()
                .unwrap_or_default()
        })
        .collect();
    let (header, body) = cells.split_first().expect("a run starts with its header");
    let Some(widths) = fit_columns(&natural, width.saturating_sub(rail.width())) else {
        push_records(out, rail, header, body);
        return;
    };
    let rule = widths
        .iter()
        .map(|width| "─".repeat(*width))
        .collect::<Vec<_>>()
        .join(&" ".repeat(COLUMN_GAP));
    let body: Vec<Vec<RenderedLine>> = body
        .iter()
        .map(|row| table_row(LineKind::Table, rail, row, &widths, &align))
        .collect();
    // A table whose cells wrapped is read row by row, not scanned, so its
    // rows are given air. One that stayed a line each keeps its density.
    let airy = body.iter().any(|row| row.len() > 1);
    out.extend(table_row(LineKind::Heading, rail, header, &widths, &align));
    out.push(RenderedLine {
        clip: true,
        ..RenderedLine::new(LineKind::Meta, rail, rule)
    });
    for (index, row) in body.into_iter().enumerate() {
        if airy && index > 0 {
            out.push(RenderedLine::spacer());
        }
        out.extend(row);
    }
}

/// Column widths that fit `available`, or `None` when none sensibly do.
///
/// Width comes off the widest column first: one long note column gives up
/// its slack before three short ones each lose a character. The cap lands on
/// a whole cell, so what it leaves over goes back to the columns still cut.
fn fit_columns(natural: &[usize], available: usize) -> Option<Vec<usize>> {
    let budget = available.checked_sub(COLUMN_GAP * natural.len().saturating_sub(1))?;
    if natural.iter().sum::<usize>() <= budget {
        return Some(natural.to_vec());
    }
    let capped = |cap: usize| natural.iter().map(|width| (*width).min(cap)).sum::<usize>();
    if capped(MIN_COLUMN) > budget {
        return None;
    }
    let cap = (MIN_COLUMN..*natural.iter().max()?)
        .rev()
        .find(|cap| capped(*cap) <= budget)?;
    let mut widths: Vec<usize> = natural.iter().map(|width| (*width).min(cap)).collect();
    let mut spare = budget - capped(cap);
    for (width, natural) in widths.iter_mut().zip(natural) {
        let take = spare.min(natural - *width);
        *width += take;
        spare -= take;
    }
    Some(widths)
}

/// One table row, wrapped inside its columns and aligned.
///
/// A cell too wide for its column costs the row height, never its text: a
/// reader that hides what an agent wrote is worse than a tall table. The
/// wrapping is [`wrap_rendered`]'s, so a cell breaks on words and carries
/// its emphasis across the break exactly like a paragraph does.
fn table_row(
    kind: LineKind,
    rail: Rail,
    cells: &[(String, Vec<Accent>)],
    widths: &[usize],
    align: &[Align],
) -> Vec<RenderedLine> {
    let wrapped: Vec<Vec<RenderedLine>> = cells
        .iter()
        .enumerate()
        .map(|(column, (cell, emphasis))| {
            // A neutral kind: this line is a measuring device, and only its
            // rows and their accents are kept.
            wrap_rendered(
                vec![RenderedLine {
                    accents: emphasis.clone(),
                    ..RenderedLine::new(LineKind::Text, Rail::Bare, cell.as_str())
                }],
                widths.get(column).copied().unwrap_or_default(),
            )
        })
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or_default();
    (0..height)
        .map(|row| {
            let mut text = String::new();
            let mut accents = Vec::new();
            for (column, cell) in wrapped.iter().enumerate() {
                let width = widths.get(column).copied().unwrap_or_default();
                let cell = cell.get(row);
                let body = cell.map_or("", |line| line.text.as_str());
                if column > 0 {
                    text.push_str(&" ".repeat(COLUMN_GAP));
                }
                let pad = width.saturating_sub(Span::raw(body).width());
                let lead = match align.get(column).copied().unwrap_or(Align::Left) {
                    Align::Left => 0,
                    Align::Center => pad / 2,
                    Align::Right => pad,
                };
                text.push_str(&" ".repeat(lead));
                let start = text.len();
                accents.extend(
                    cell.iter()
                        .flat_map(|line| &line.accents)
                        .map(|accent| Accent {
                            start: accent.start + start,
                            end: accent.end + start,
                            ..*accent
                        }),
                );
                text.push_str(body);
                // Padding past the last cell is invisible, and leaving it off
                // keeps the cursor band and the text dump free of blanks.
                if column + 1 < cells.len() {
                    text.push_str(&" ".repeat(pad - lead));
                }
            }
            RenderedLine {
                accents,
                // The row was measured against this very column; reflowing it
                // would only undo the alignment it was just given.
                clip: true,
                // A cell that wrapped on a space leaves it at the line end.
                ..RenderedLine::new(kind, rail, text.trim_end())
            }
        })
        .collect()
}

/// A table the column cannot hold, as one labelled block per row.
///
/// Five columns in a thirty-cell pane is not a table, it is confetti. The
/// same rows read fine as `header: value` pairs, wrap like prose, and lose
/// nothing to truncation.
fn push_records(
    out: &mut Vec<RenderedLine>,
    rail: Rail,
    header: &[(String, Vec<Accent>)],
    body: &[Vec<(String, Vec<Accent>)>],
) {
    for (index, row) in body.iter().enumerate() {
        if index > 0 {
            out.push(RenderedLine::spacer());
        }
        for (column, (cell, emphasis)) in row.iter().enumerate() {
            if cell.is_empty() {
                continue;
            }
            let label = header.get(column).map_or("", |(label, _)| label.as_str());
            let mut text = String::new();
            let mut accents = Vec::new();
            if !label.is_empty() {
                text.push_str(label);
                accents.push(Accent {
                    start: 0,
                    end: text.len(),
                    emphasis: Emphasis::Strong,
                });
                text.push_str(": ");
            }
            let start = text.len();
            accents.extend(emphasis.iter().map(|accent| Accent {
                start: accent.start + start,
                end: accent.end + start,
                ..*accent
            }));
            text.push_str(cell);
            out.push(RenderedLine {
                accents,
                ..RenderedLine::new(LineKind::Table, rail, text)
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use time::UtcOffset;

    use super::*;
    use crate::model::{Block, Role, SourceId, Transcript, Turn};
    use crate::output::reader::{fit, render};

    /// One assistant message, rendered and fitted to a `width`-cell column.
    fn table_lines(markdown: &str, width: usize) -> Vec<RenderedLine> {
        let mut transcript = Transcript::new("t", SourceId::Omp);
        let mut turn = Turn::new(Role::Assistant);
        turn.blocks.push(Block::text(markdown));
        transcript.push(turn);
        fit(
            render(
                &transcript,
                true,
                true,
                false,
                &HashMap::new(),
                UtcOffset::UTC,
            ),
            width,
        )
    }

    /// The columns are the content; the pipes that drew them are not.
    #[test]
    fn a_markdown_table_becomes_columns() {
        let lines = table_lines(
            "| Tool | Calls | Note |\n|:-----|------:|:-----|\n| `read` | 12 | reads **files** |\n| write | 3 | writes |",
            60,
        );
        assert!(
            !lines.iter().any(|line| line.text.contains('|')),
            "markers never reach the page"
        );
        let drawn: Vec<&str> = lines
            .iter()
            .filter(|line| !line.is_blank() && line.kind != LineKind::SpeakerAgent)
            .map(|line| line.text.as_str())
            .collect();
        assert_eq!(
            drawn,
            [
                "Tool   Calls  Note",
                "─────  ─────  ───────────",
                "read      12  reads files",
                "write      3  writes",
            ],
            "columns line up and the delimiter row became the rule under them"
        );
        let row = lines
            .iter()
            .find(|line| line.text.starts_with("read"))
            .expect("the first body row");
        assert_eq!(
            row.runs().collect::<Vec<_>>(),
            [
                ("read", Some(Emphasis::Code)),
                ("      12  reads ", None),
                ("files", Some(Emphasis::Strong)),
            ],
            "emphasis inside a cell survives being laid out"
        );
    }

    /// A narrow pane costs a table its height, never a word of its text.
    #[test]
    fn a_table_too_wide_for_the_pane_keeps_every_cell() {
        let markdown = "| Tool | Note |\n|:-----|:-----|\n| read | reads files and directories |";
        let words = |lines: &[RenderedLine]| {
            let text = lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            ["read", "reads", "files", "and", "directories"]
                .into_iter()
                .all(|word| text.contains(word))
        };

        let wrapped = table_lines(markdown, 26);
        assert!(
            wrapped.iter().any(|line| line.text == "Tool  Note"),
            "still a table: {:?}",
            wrapped.iter().map(|l| &l.text).collect::<Vec<_>>()
        );
        assert!(words(&wrapped), "a squeezed cell wraps rather than cutting");

        // Below two readable columns the shape stops being a table at all.
        let records = table_lines(markdown, 15);
        assert!(
            records.iter().any(|line| line.text == "Tool: read"),
            "each row became a labelled block: {:?}",
            records.iter().map(|l| &l.text).collect::<Vec<_>>()
        );
        assert!(words(&records), "nothing is lost on the way to records");
    }

    #[test]
    fn an_escaped_pipe_stays_inside_its_cell() {
        let lines = table_lines("| Tool | Note |\n|---|---|\n| read | a \\| b |", 40);
        assert!(
            lines.iter().any(|line| line.text.ends_with("a | b")),
            "`\\|` is a pipe in a cell, not the end of one"
        );
    }
}
