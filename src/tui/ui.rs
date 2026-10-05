//! Workspace-first rendering. Narrow terminals show the current step rather
//! than squeezing three unreadable columns onto the screen.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Padding, Paragraph,
};

use crate::catalog::workspace;
use crate::model::{LabelSource, Liveness};
use crate::output::reader::{
    Emphasis, LineKind, READING_MEASURE, RenderedLine, elide, plural, truncate_cells,
};
use crate::runtime::live::Activity;
use crate::tui::app::{App, Prompt};
use crate::tui::view::{Grouping, Pane, Row};

/// Width of the help panel's key column, wide enough for every key and its
/// aliases beside the description.
const KEY_COLUMN: usize = 20;

/// One action per row: a key, or a key with its aliases, then what it does.
/// An empty key starts a section.
const HELP: &[(&str, &str)] = &[
    ("", "Browse"),
    ("j, ↓", "Move down"),
    ("k, ↑", "Move up"),
    ("PgDn, Ctrl-F", "Next page"),
    ("PgUp, Ctrl-B", "Previous page"),
    ("g, Home", "Top"),
    ("G, End", "Bottom"),
    ("Enter", "Open a session or follow a transcript link"),
    ("Tab", "Next pane"),
    ("Shift-Tab", "Previous pane"),
    ("l, →", "Focus right"),
    ("h, ←, Esc, q", "Focus left"),
    ("", "Filter and search"),
    ("/", "Filter titles, paths and agents"),
    ("s", "Search transcript text; run llm-sessions index first"),
    ("x, Ctrl-L", "Clear filters and search"),
    ("e", "Show only sessions with a process attached"),
    ("a", "Show or hide archived sessions"),
    ("", "Views"),
    ("]", "Next view"),
    ("[", "Previous view"),
    ("1", "Workspace view"),
    ("2", "Agent view"),
    ("3", "Branch view"),
    ("4", "Fleet view"),
    ("5", "Host view"),
    ("6", "Timeline view"),
    ("", "Session folds"),
    ("z a", "Toggle the selected session's descendants"),
    ("z c", "Close the selected session's descendants"),
    ("z o", "Open the selected session's descendants"),
    ("z M", "Close every session fold"),
    ("z R", "Open every session fold"),
    ("", "Read"),
    ("t", "Toggle thinking"),
    ("o", "Toggle tool output"),
    ("c", "Toggle changed files"),
    ("f", "Follow new transcript content"),
    (
        "F11, Ctrl-W o",
        "Fullscreen reader; again or Esc returns to the panes",
    ),
    ("", "Copy"),
    (
        "y y",
        "Copy the assistant reply at the cursor; from the lists, the latest",
    ),
    ("y A", "Copy the whole transcript, folded as shown"),
    ("y r", "Copy the selected session's resume command"),
    ("", "Act on sessions"),
    ("r", "Quit and print the resume command"),
    ("i", "Insert: send one message to the selected session"),
    ("J", "Jump to the selected session's tmux pane"),
    ("Space", "Select or unselect the session under the cursor"),
    ("v", "Select every visible session"),
    ("V", "Select the entire group"),
    ("d", "Archive marked sessions; asks first"),
    ("u", "Restore marked sessions; asks first"),
    ("", "Bookmarks"),
    ("m <letter>", "Bookmark the selected session, view and pane"),
    ("' <letter>", "Jump back to a bookmark"),
    ("` <letter>", "Jump back to a bookmark"),
    ("", "Window"),
    (
        "Ctrl-W",
        "Resize mode: the keys below repeat until Enter or Esc",
    ),
    ("Ctrl-W l, Ctrl-W →", "Widen the focused pane"),
    ("Ctrl-W h, Ctrl-W ←", "Narrow the focused pane"),
    ("Ctrl-W =", "Reset pane widths"),
    ("", "General"),
    ("Ctrl-R", "Rescan now; otherwise refreshes automatically"),
    ("?, F1", "Open or close help; Esc also closes"),
    ("Q, Ctrl-C", "Quit"),
    ("", "Session activity"),
    ("● exact", "A process holds the session file open"),
    ("○ inferred", "A process matches the agent and directory"),
    ("· recent", "Recently written; no process attached"),
];

/// The layout of one frame: pane rectangles plus the measurements the
/// reader wraps against.
///
/// Splitting is pure — it reads App state and touches nothing. The event
/// loop calls [`panes`], hands the result to `App::apply_layout`, then
/// calls [`draw`] with it, so drawing never writes back into App.
pub struct Panes {
    pub area: Rect,
    /// Breadcrumb row; zero-sized in zen mode.
    pub nav: Rect,
    pub status: Rect,
    pub rects: [Rect; 3],
    pub focused: usize,
    /// Body-column width for the reader; `0` when no reader pane is shown.
    pub transcript_width: usize,
    /// Rows the focused pane shows.
    pub viewport: usize,
    /// Zen fullscreen column; `None` in pane mode.
    pub zen: Option<Rect>,
    /// `(viewport, line_count)` for the help popup, when it is open.
    pub help: Option<(usize, usize)>,
    /// The exact rectangle drawn by the transcript reader.
    transcript: Rect,
    /// Prepared once during measurement, then reused when drawing help.
    help_lines: Option<Vec<Line<'static>>>,
}

/// Split one frame. Pure: no App mutation, no rendering.
pub fn panes(area: Rect, app: &App) -> Panes {
    let help_lines = app.show_help().then(|| {
        help_lines(
            area.width.min(88).saturating_sub(2) as usize,
            app.failures(),
        )
    });
    let help = help_lines
        .as_ref()
        .map(|lines| (area.height.saturating_sub(4) as usize, lines.len()));
    if app.zen() {
        let column = reading_column(area, false);
        let transcript = zen_rows(column)[2];
        return Panes {
            area,
            nav: Rect::default(),
            status: Rect::default(),
            rects: [Rect::default(); 3],
            focused: 2,
            transcript_width: transcript.width as usize,
            viewport: if transcript.width == 0 {
                0
            } else {
                transcript.height as usize
            },
            zen: Some(column),
            help,
            transcript,
            help_lines,
        };
    }
    // One row at the bottom, not two: the keys that act on a pane live on
    // that pane's own border now, so the bar carries state and nothing else.
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);

    let mut split = [Rect::default(); 3];
    let focused = match app.focus() {
        Pane::Groups => 0,
        Pane::Sessions => 1,
        Pane::Transcript => 2,
    };
    if area.width >= 140 {
        let panes = Layout::horizontal(app.widths().map(Constraint::Percentage)).split(rows[1]);
        split.copy_from_slice(&panes);
    } else if area.width >= 100 {
        let reading = app.focus() == Pane::Transcript;
        let panes = Layout::horizontal([
            Constraint::Percentage(if reading { 32 } else { 36 }),
            Constraint::Min(1),
        ])
        .split(rows[1]);
        let first = usize::from(reading);
        split[first] = panes[0];
        split[first + 1] = panes[1];
    } else {
        split[focused] = rows[1];
    }

    let transcript = reading_column(split[2], true);
    let viewport = if focused == 2 {
        if transcript.width == 0 {
            0
        } else {
            transcript.height as usize
        }
    } else {
        split[focused].height.saturating_sub(2) as usize
    };
    Panes {
        area,
        nav: rows[0],
        status: rows[2],
        rects: split,
        focused,
        transcript_width: transcript.width as usize,
        viewport: if focused == 1 { viewport / 2 } else { viewport },
        zen: None,
        help,
        transcript,
        help_lines,
    }
}
pub fn draw(frame: &mut Frame<'_>, app: &App, layout: &Panes) {
    if let Some(column) = layout.zen {
        draw_zen(frame, column, layout.transcript, app);
        // Help is modal and swallows input, so it must be visible wherever it
        // can be opened — otherwise the reader looks frozen.
        if let Some(lines) = &layout.help_lines {
            draw_help(frame, layout.area, app, lines);
        }
        return;
    }
    draw_navigation(frame, layout.nav, app);
    if layout.rects[0].width > 0 {
        draw_groups(frame, layout.rects[0], app);
    }
    if layout.rects[1].width > 0 {
        draw_sessions(frame, layout.rects[1], app);
    }
    if layout.rects[2].width > 0 {
        draw_transcript(frame, layout.rects[2], layout.transcript, app);
    }
    draw_status(frame, layout.status, app);
    if let Some(lines) = &layout.help_lines {
        draw_help(frame, layout.area, app, lines);
    }
}

/// Where the reader's body column sits inside `area`.
///
/// A pane fills the width it was given, minus one cell of air on each side
/// of its border: the flush-right timestamps and the full-width date rules
/// both reach the edge of this column, and neither should touch the frame.
/// Fullscreen is the opposite case — there is no frame, so prose is capped
/// at [`READING_MEASURE`] and centred, and the page reads like a page
/// instead of a billboard.
fn reading_column(area: Rect, bordered: bool) -> Rect {
    let inset: u16 = if bordered { 1 } else { 2 };
    let usable = area.width.saturating_sub(inset * 2 + 2);
    let width = if bordered {
        usable
    } else {
        usable.min(u16::try_from(READING_MEASURE).unwrap_or(u16::MAX))
    };
    let x = if bordered {
        area.x.saturating_add((inset + 1).min(area.width))
    } else {
        area.x.saturating_add(area.width.saturating_sub(width) / 2)
    };
    Rect {
        x,
        y: area.y.saturating_add(inset.min(area.height)),
        width,
        height: area.height.saturating_sub(inset * 2),
    }
}

fn zen_rows(column: Rect) -> [Rect; 4] {
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(Rect {
        y: column.y.saturating_add(1.min(column.height)),
        height: column.height.saturating_sub(2),
        ..column
    });
    [rows[0], rows[1], rows[2], rows[3]]
}

fn draw_navigation(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mut spans = vec![Span::styled(
        " llm-sessions  ",
        Style::default().add_modifier(Modifier::BOLD),
    )];
    for (index, (pane, label)) in [
        (Pane::Groups, app.grouping().label()),
        (Pane::Sessions, "sessions"),
        (Pane::Transcript, "transcript"),
    ]
    .into_iter()
    .enumerate()
    {
        if area.width < 65 && app.focus() != pane {
            continue;
        }
        if index > 0 && area.width >= 65 {
            spans.push(Span::raw(" › "));
        }
        spans.push(Span::styled(
            label,
            if app.focus() == pane {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            },
        ));
    }
    if app.is_read_only() {
        spans.push(Span::styled(
            "  read-only",
            Style::default().fg(Color::Yellow),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn pane_block(title: String, focused: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused {
            Color::Cyan
        } else {
            Color::DarkGray
        }))
        .title(Span::styled(
            title,
            Style::default().fg(if focused { Color::Cyan } else { Color::Gray }),
        ))
}

/// The keys that act on the focused pane, written on its own border.
///
/// Each pane knows its own verbs, so the hint belongs where the thing it
/// operates on is — not in a global cheat sheet that repeats every key for
/// every pane. `reserved` is width already spent on that border.
fn pane_keys(pane: Pane, focused: bool, width: u16, reserved: usize) -> Option<Span<'static>> {
    if !focused {
        return None;
    }
    let chips: &[&str] = match pane {
        Pane::Groups => &["Enter sessions", "/ filter", "] [ view"],
        Pane::Sessions => &["Enter read", "/ filter", "z fold", "r resume"],
        Pane::Transcript => &["C-w o zen", "t thinking", "o tools", "f follow", "y yank"],
    };
    // A border that cannot hold the first key holds none: half a chord is
    // worse than the room it costs.
    // The corners stay legible as corners: the hint never runs the border
    // out to its last cell.
    let budget = (width as usize).saturating_sub(reserved + 8);
    let mut text = String::new();
    for chip in chips {
        let needed = if text.is_empty() {
            chip.len()
        } else {
            chip.len() + 3
        };
        if text.len() + needed > budget {
            break;
        }
        if !text.is_empty() {
            text.push_str(" · ");
        }
        text.push_str(chip);
    }
    if text.is_empty() {
        return None;
    }
    Some(Span::styled(
        format!(" {text} "),
        Style::default().fg(Color::Indexed(245)),
    ))
}

fn draw_groups(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let focused = app.focus() == Pane::Groups;
    let title = format!(" {} · {} ", app.grouping().label(), app.groups().len());
    let mut block = pane_block(title, focused);
    if let Some(keys) = pane_keys(Pane::Groups, focused, area.width, 0) {
        block = block.title_bottom(keys);
    }
    if app.groups().is_empty() {
        frame.render_widget(
            Paragraph::new(empty_groups(app))
                .wrap(ratatui::widgets::Wrap { trim: false })
                .block(block.padding(Padding::new(2, 2, 1, 0))),
            area,
        );
        return;
    }
    let inner = area.width.saturating_sub(4) as usize;
    let items: Vec<_> = app
        .groups()
        .iter()
        .map(|group| {
            let count = group.count.to_string();
            // Mark, liveness, then one cell of air: a live row's dot must not
            // fuse with the path, and a truncated label must not fuse with
            // its count, so the count keeps two cells of its own.
            let field = inner.saturating_sub(count.len() + 5);
            let label = truncate_cells(&group.label, field);
            let padding = field.saturating_sub(Span::raw(&label).width()) + 2;
            ListItem::new(Line::from(vec![
                Span::styled(
                    if app.is_group_marked(group) {
                        "▣"
                    } else {
                        " "
                    },
                    Style::default().fg(Color::Yellow),
                ),
                Span::styled(
                    if group.running > 0 { "● " } else { "  " },
                    Style::default().fg(Color::Green),
                ),
                Span::raw(label),
                Span::raw(" ".repeat(padding)),
                Span::styled(count, Style::default().fg(Color::Gray)),
            ]))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(app.group_selected()));
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .highlight_symbol("› ")
            .highlight_style(highlight(focused)),
        area,
        &mut state,
    );
}

fn draw_sessions(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let focused = app.focus() == Pane::Sessions;
    let title = format!(" sessions · {} ", app.visible_len());
    let mut block = pane_block(title, focused);
    if let Some(keys) = pane_keys(Pane::Sessions, focused, area.width, 0) {
        block = block.title_bottom(keys);
    }
    if app.rows().is_empty() {
        frame.render_widget(
            Paragraph::new(empty_sessions(app))
                .wrap(ratatui::widgets::Wrap { trim: false })
                .block(block.padding(Padding::new(2, 2, 1, 0))),
            area,
        );
        return;
    }
    let width = area.width.saturating_sub(4) as usize;
    // One reading of the clock for the whole list, so no two rows in the
    // same frame can disagree about what "today" is.
    let now = crate::clock::at(time::OffsetDateTime::now_utc(), app.offset());
    let items: Vec<_> = app
        .rows()
        .iter()
        .map(|&row| session_item(row, app, width, now))
        .collect();
    let mut state = ListState::default().with_selected(Some(app.session_selected()));
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .highlight_symbol("› ")
            .highlight_style(highlight(focused)),
        area,
        &mut state,
    );
}

fn session_item(row: Row, app: &App, width: usize, now: time::OffsetDateTime) -> ListItem<'static> {
    let session = app.session(row);
    let label = app.label(session);
    let indent = if row.depth == 0 {
        String::new()
    } else {
        format!("{}↳ ", "  ".repeat((row.depth as usize - 1).min(width / 8)))
    };
    // A parent says whether its branch is showing; a leaf spends the same
    // two cells on nothing, so every label in the column starts together.
    let (disclosure, disclosure_color) = match (row.children, row.folded) {
        (true, true) => ("▸ ", Color::Yellow),
        (true, false) => ("▾ ", Color::DarkGray),
        _ => ("  ", Color::DarkGray),
    };
    // The mark keeps its cell whether or not the row is marked: selecting a
    // session must not shunt every title in the list sideways.
    let mark = if app.is_marked(session) { "▣" } else { " " };
    let gutter = 1 + Span::raw(&indent).width() + 2;
    let title = Line::from(vec![
        Span::styled(mark, Style::default().fg(Color::Yellow)),
        Span::styled(indent, Style::default().fg(Color::Gray)),
        Span::styled(disclosure, Style::default().fg(disclosure_color)),
        Span::styled(
            truncate_cells(&label.text, width.saturating_sub(gutter)),
            if label.source == LabelSource::Title {
                Style::default()
            } else {
                Style::default().add_modifier(Modifier::ITALIC)
            },
        ),
    ]);
    let mut detail = vec![session.source.to_string(), when(session.updated, now)];
    // A remote row names its host; a local row shows nothing, and the
    // absence is the "this machine" signal.
    if let Some(host) = &session.host {
        detail.push(host.clone());
    }
    if session.archived {
        detail.push("archived".to_owned());
    }
    if let Some((_, activity)) = app.attachment(session) {
        match activity {
            Activity::Working => detail.push("working".to_owned()),
            Activity::Waiting => detail.push("waiting".to_owned()),
            Activity::Unknown => {}
        }
    }
    if let Some(branch) = &session.branch {
        detail.push(branch.clone());
    }
    if app.grouping() != Grouping::Workspace
        && let Some(place) = &session.workspace
    {
        detail.push(workspace::display(place, app.home()));
    }
    // The badge takes the disclosure's cells and the tree column is held
    // open with blanks, so the detail text sits under the title it belongs
    // to — including a nested row's — without drawing the branch twice.
    // What does not fit is elided rather than sheared off by the border.
    ListItem::new(vec![
        title,
        Line::from(vec![
            Span::raw(" ".repeat(gutter - 2)),
            Span::styled(
                format!("{} ", session.liveness.badge()),
                Style::default().fg(liveness_color(session.liveness)),
            ),
            Span::styled(
                truncate_cells(&detail.join(" · "), width.saturating_sub(gutter)),
                Style::default().fg(Color::Gray),
            ),
        ]),
    ])
}

fn draw_transcript(frame: &mut Frame<'_>, area: Rect, body: Rect, app: &App) {
    let focused = app.focus() == Pane::Transcript;
    let title = match app.selected_session() {
        Some(session) => format!(
            " {}{} ",
            truncate_cells(
                &app.label(session).text,
                area.width.saturating_sub(16) as usize
            ),
            if app.following() { " · following" } else { "" }
        ),
        None => " transcript ".to_string(),
    };
    let place = format!(" {} ", position(app));
    let mut bottom = vec![Span::styled(
        place.clone(),
        Style::default().fg(Color::Indexed(244)),
    )];
    bottom.extend(pane_keys(
        Pane::Transcript,
        focused,
        area.width,
        place.len(),
    ));
    let block = pane_block(title, focused).title_bottom(Line::from(bottom));
    if let Some(message) = empty_reader(app) {
        frame.render_widget(
            Paragraph::new(message)
                .wrap(ratatui::widgets::Wrap { trim: false })
                .block(block),
            area,
        );
        return;
    }
    frame.render_widget(block, area);
    draw_reader(frame, body, app, focused);
}

/// The reader with the whole terminal: title, hairline, page, footer.
///
/// No pane borders and no navigation breadcrumb — in fullscreen the only
/// thing on screen is the conversation, which is the point of the mode.
fn draw_zen(frame: &mut Frame<'_>, column: Rect, body: Rect, app: &App) {
    let rows = zen_rows(column);

    let width = column.width as usize;
    let (label, detail) = match app.selected_session() {
        Some(session) => (
            app.label(session).text,
            [
                Some(session.source.to_string()),
                session
                    .workspace
                    .as_ref()
                    .map(|place| workspace::display(place, app.home())),
                app.following().then(|| "following".to_owned()),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · "),
        ),
        None => ("transcript".to_owned(), String::new()),
    };
    frame.render_widget(
        Paragraph::new(flush_right(
            &truncate_cells(&label, width.saturating_sub(detail.chars().count() + 2)),
            Style::default()
                .fg(Color::Indexed(255))
                .add_modifier(Modifier::BOLD),
            &detail,
            Style::default().fg(Color::Indexed(244)),
            width,
        )),
        rows[0],
    );
    frame.render_widget(Paragraph::new(hairline("", width)), rows[1]);

    if let Some(message) = empty_reader(app) {
        frame.render_widget(
            Paragraph::new(message)
                .wrap(ratatui::widgets::Wrap { trim: false })
                .style(Style::default().fg(Color::Indexed(244))),
            body,
        );
    } else {
        draw_reader(frame, body, app, true);
    }

    // Yank is a reading-mode command, so its prompt and its result have to
    // be visible here; the footer is the only line zen keeps.
    let (thinking, tools) = app.folding();
    let (hint, hint_style) = match (app.prompt(), app.status()) {
        (Prompt::Yank, _) => (
            "yank: y reply at cursor · A whole transcript · r resume command · Esc cancel"
                .to_owned(),
            Style::default().fg(Color::Cyan),
        ),
        (_, Some(status)) => (status.to_owned(), Style::default().fg(Color::Yellow)),
        _ => (
            format!(
                "C-w o exit · t {} · o {} · f follow · y yank · ? help",
                if thinking { "thinking" } else { "fold" },
                if tools { "tools" } else { "fold" },
            ),
            Style::default().fg(Color::Indexed(244)),
        ),
    };
    frame.render_widget(
        Paragraph::new(flush_right(
            &position(app),
            Style::default().fg(Color::Indexed(244)),
            &hint,
            hint_style,
            width,
        )),
        rows[3],
    );
}

/// Why a list is empty, and the one key that changes it.
///
/// The three panes answer in the same voice: what is missing, then the fix.
/// A hint is offered only when it would actually do something — telling
/// someone to clear a filter they never set is noise.
fn empty_groups(app: &App) -> &'static str {
    if app.total_sessions() == 0 {
        "No sessions found.\n\nRun a supported coding agent, then press Ctrl-R to rescan.\nllm-sessions sources lists every store that was checked."
    } else if filtering(app) {
        "Nothing matches.\n\nx clears the filter.\na includes archived sessions."
    } else {
        "Nothing to show.\n\na includes archived sessions."
    }
}

fn empty_sessions(app: &App) -> &'static str {
    if filtering(app) {
        "Nothing matches.\n\nx clears the filter."
    } else {
        "This group has no sessions left."
    }
}

fn filtering(app: &App) -> bool {
    !app.filter().is_empty() || app.searching() || app.live_only()
}

/// Why the page is blank, when it is.
///
/// A body still crossing an ssh connection is not an unreadable one: the
/// waiting case has to answer before the failure case, or a slow host looks
/// like a broken store.
fn empty_reader(app: &App) -> Option<&'static str> {
    if !app.rendered().is_empty() {
        return None;
    }
    Some(if app.selected_session().is_none() {
        "Choose a session to read its transcript."
    } else if app.fetching() {
        "Reading this transcript from its host.\nOne ssh round trip; the status line names the machine."
    } else if app.transcript().is_none() {
        "Transcript unavailable.\nCtrl-R retries the read."
    } else {
        "This session has no transcript content yet.\nf follows new content."
    })
}

fn draw_reader(frame: &mut Frame<'_>, column: Rect, app: &App, focused: bool) {
    let width = column.width as usize;
    let lines: Vec<_> = app
        .rendered()
        .iter()
        .skip(app.scroll())
        .take(column.height as usize)
        .enumerate()
        .map(|(offset, line)| {
            reader_line(
                line,
                width,
                focused && app.scroll() + offset == app.cursor(),
            )
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), column);
}

/// One drawn row: gutter, body, flush-right detail.
fn reader_line(line: &RenderedLine, width: usize, cursor: bool) -> Line<'static> {
    let mut spans = if line.kind == LineKind::Rule {
        hairline(&line.text, width).spans
    } else {
        let body = width.saturating_sub(line.rail.width());
        let mut spans = Vec::with_capacity(4);
        if line.rail.width() > 0 {
            spans.push(Span::styled(
                line.rail.glyph(line.first),
                rail_style(line.kind),
            ));
        }
        match &line.meta {
            // A header keeps its detail flush right and gives up label
            // width to do it: the timestamp is what the eye returns to.
            Some(meta) => {
                let reserved = Span::raw(meta).width() + 2;
                let text = clip(line, body.saturating_sub(reserved));
                spans.push(Span::styled(text.clone(), body_style(line.kind)));
                spans.push(Span::raw(" ".repeat(body.saturating_sub(
                    Span::raw(&text).width() + Span::raw(meta).width(),
                ))));
                spans.push(Span::styled(
                    meta.clone(),
                    Style::default().fg(Color::Indexed(244)),
                ));
            }
            None if line.accents.is_empty() => {
                spans.push(Span::styled(clip(line, body), body_style(line.kind)));
            }
            // Emphasis is weight and colour, never punctuation the reader
            // has to skip: the markers were dropped when the line was built.
            None => {
                let base = body_style(line.kind);
                let mut used = 0;
                for (run, emphasis) in line.runs() {
                    if used >= body {
                        break;
                    }
                    let run = truncate_cells(run, body - used);
                    used += Span::raw(&run).width();
                    spans.push(Span::styled(
                        run,
                        emphasis.map_or(base, |e| accent(base, e)),
                    ));
                }
            }
        }
        spans
    };
    if cursor {
        // A background band, not a reversed row: reading the highlighted
        // line must stay as easy as reading the ones around it.
        let pad = width.saturating_sub(spans.iter().map(Span::width).sum::<usize>());
        spans.push(Span::raw(" ".repeat(pad)));
        for span in &mut spans {
            span.style = span.style.bg(Color::Indexed(236));
        }
    }
    Line::from(spans)
}

/// Fit a line's text into `width`.
///
/// A chip is clipped here, not when it was built, because only the renderer
/// knows how wide the column ended up. `read  …/cart/CartItem.vue` keeps the
/// tool name and the end of the path: both halves answer a question.
fn clip(line: &RenderedLine, width: usize) -> String {
    if Span::raw(line.text.as_str()).width() <= width {
        return line.text.clone();
    }
    let Some((head, subject)) = line.text.split_once("  ").filter(|_| line.clip) else {
        return truncate_cells(&line.text, width);
    };
    let room = width.saturating_sub(Span::raw(head).width() + 2);
    format!("{head}  {}", elide(subject, room))
}

/// `──── label ───────` across `width`.
fn hairline(label: &str, width: usize) -> Line<'static> {
    let style = Style::default().fg(Color::Indexed(238));
    if label.is_empty() {
        return Line::from(Span::styled("─".repeat(width), style));
    }
    let label = truncate_cells(label, width.saturating_sub(8));
    let tail = width.saturating_sub(Span::raw(&label).width() + 6);
    Line::from(vec![
        Span::styled("──── ".to_owned(), style),
        Span::styled(label, Style::default().fg(Color::Indexed(244))),
        Span::styled(format!(" {}", "─".repeat(tail)), style),
    ])
}

/// `left` at the margin, `right` flush against the far edge.
fn flush_right(
    left: &str,
    left_style: Style,
    right: &str,
    right_style: Style,
    width: usize,
) -> Line<'static> {
    let pad = width.saturating_sub(Span::raw(left).width() + Span::raw(right).width());
    Line::from(vec![
        Span::styled(left.to_owned(), left_style),
        Span::raw(" ".repeat(pad)),
        Span::styled(right.to_owned(), right_style),
    ])
}

/// `48 / 512` — the cursor's row, and how many there are. Empty when there
/// is nothing to read.
fn position(app: &App) -> String {
    let total = app.rendered().len();
    if total == 0 {
        return String::new();
    }
    format!("{} / {total}", app.cursor() + 1)
}

/// The one bottom row: what is true right now, and the two keys that are
/// true everywhere.
///
/// Colour carries one meaning each. Cyan is a prompt waiting for input,
/// yellow is something the user should know about, grey is state. Failed
/// sources collapse to a count — a wall of concatenated error text is
/// unreadable and never stops being displayed — and `?` opens the list.
fn draw_status(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let width = area.width as usize;
    let cyan = Style::default().fg(Color::Cyan);
    let (left, right, band) = match app.prompt() {
        Prompt::Filter(query) => (
            Span::styled(format!("Filter: {query}_"), cyan),
            "Enter apply · Esc cancel".to_owned(),
            None,
        ),
        Prompt::Search(query) => (
            Span::styled(format!("Search text: {query}_"), cyan),
            "Enter search · Esc cancel".to_owned(),
            None,
        ),
        // The end of the message is where typing happens, so a long one
        // loses its head, not the cursor.
        Prompt::Insert { target, text } => {
            let head = format!("Message to {}: ", target.0);
            let room = width.saturating_sub(Span::raw(head.as_str()).width() + 1);
            (
                Span::styled(format!("{head}{}_", tail_cells(text, room)), cyan),
                "Enter send · Esc cancel".to_owned(),
                None,
            )
        }
        Prompt::Bookmark { jump } => (
            Span::styled(
                format!(
                    "{}: _",
                    if *jump {
                        "Jump to bookmark"
                    } else {
                        "Save bookmark under"
                    }
                ),
                cyan,
            ),
            bookmark_hint(app, *jump),
            None,
        ),
        Prompt::Fold => (
            Span::styled("Fold command: z", cyan),
            "a toggle · c close · o open · M close all · R open all · Esc cancel".to_owned(),
            None,
        ),
        Prompt::Yank => (
            Span::styled("Yank: y", cyan),
            "y reply · A whole transcript · r resume command · Esc cancel".to_owned(),
            None,
        ),
        Prompt::Resize => {
            let [groups, sessions, transcript] = app.widths();
            (
                Span::styled(
                    format!("Wide layout: {groups} / {sessions} / {transcript}%"),
                    cyan,
                ),
                "←/→ resize · o only pane · = reset · Esc done".to_owned(),
                None,
            )
        }
        // The one destructive question in the app: it takes the whole row.
        Prompt::ConfirmArchive {
            count,
            groups,
            archive,
        } => {
            let band = Style::default().fg(Color::Black).bg(Color::Yellow);
            (
                Span::styled(
                    format!(
                        "{} {}{}?",
                        if *archive { "Archive" } else { "Restore" },
                        plural(*count, "session"),
                        if *groups == 0 {
                            String::new()
                        } else {
                            format!(" in {} marked", plural(*groups, "group"))
                        }
                    ),
                    band,
                ),
                "y confirm · n / Esc cancel".to_owned(),
                Some(band),
            )
        }
        Prompt::None => {
            frame.render_widget(Paragraph::new(idle_line(app, width)), area);
            return;
        }
    };
    let right_style = band.unwrap_or_else(|| Style::default().fg(Color::Gray));
    let left = Span::styled(truncate_cells(&left.content, width), left.style);
    let used = left.width();
    let mut spans = vec![left];
    // The answer is what matters; its key legend is dropped rather than
    // sheared when the terminal cannot hold both.
    let gap = width.saturating_sub(used + right.chars().count());
    if gap >= 2 {
        spans.push(Span::styled(" ".repeat(gap), right_style));
        spans.push(Span::styled(right, right_style));
    } else if band.is_some() {
        // A confirmation band spans the row whether or not its keys fit.
        spans.push(Span::styled(
            " ".repeat(width.saturating_sub(used)),
            right_style,
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The last `width` cells of `s`, with `…` standing in for what was cut.
fn tail_cells(s: &str, width: usize) -> String {
    let span = Span::raw(s);
    if span.width() <= width {
        return s.to_owned();
    }
    let graphemes: Vec<_> = span.styled_graphemes(Style::default()).collect();
    let mut used = 1;
    let mut start = graphemes.len();
    while start > 0 {
        let cells = Span::raw(graphemes[start - 1].symbol).width();
        if used + cells > width {
            break;
        }
        used += cells;
        start -= 1;
    }
    std::iter::once("…")
        .chain(graphemes[start..].iter().map(|g| g.symbol))
        .collect()
}

/// The letters that already answer, so nobody has to guess which of the
/// twenty-six slots are taken.
fn bookmark_hint(app: &App, jump: bool) -> String {
    let letters = app.bookmark_letters();
    if letters.is_empty() {
        return if jump {
            "no bookmarks yet — m then a letter saves one  Esc cancel".to_owned()
        } else {
            "a-z choose  Esc cancel".to_owned()
        };
    }
    let list = letters
        .iter()
        .map(char::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    if jump {
        format!("{list}  Esc cancel")
    } else {
        format!("a-z choose  in use: {list}  Esc cancel")
    }
}

/// The idle row: state or selection context on the left, and on the right
/// the two keys that always work, behind a count of anything wrong.
///
/// A failed source is a standing condition, not an event: spelling it out
/// here would occupy the row for the rest of the session and, with two
/// hosts down, run past the edge of the terminal. The count says how many,
/// the colour says it matters, and `?` says the rest.
fn idle_line(app: &App, width: usize) -> Line<'static> {
    let mut right = Vec::new();
    if !app.failures().is_empty() {
        right.push(Span::styled(
            format!("! {}", plural(app.failures().len(), "warning")),
            Style::default().fg(Color::Yellow),
        ));
        right.push(Span::raw("  "));
    }
    right.push(Span::styled(
        "? help · Q quit",
        Style::default().fg(Color::Indexed(245)),
    ));
    let reserved: usize = right.iter().map(Span::width).sum();

    let (text, style) = match app.status() {
        Some(status) => (status.to_owned(), Style::default().fg(Color::Yellow)),
        None => (
            idle_state(app).join(" · "),
            Style::default().fg(Color::Gray),
        ),
    };
    let left = Span::styled(
        truncate_cells(&text, width.saturating_sub(reserved + 2)),
        style,
    );
    let gap = width.saturating_sub(left.width() + reserved);
    let mut spans = vec![left];
    if gap >= 2 {
        spans.push(Span::raw(" ".repeat(gap)));
        spans.extend(right);
    }
    Line::from(spans)
}

fn idle_state(app: &App) -> Vec<String> {
    let mut parts = Vec::new();
    if !app.filter().is_empty() {
        parts.push(format!("Filter: {}", app.filter()));
    }
    if app.searching() {
        parts.push("Text search active".into());
    }
    if app.live_only() {
        parts.push("Live only".into());
    }
    if app.showing_archived() {
        parts.push("Including archived".into());
    }
    if app.marked_count() > 0 {
        parts.push(format!("{} marked", app.marked_count()));
    }
    if app.marked_group_count() > 0 {
        parts.push(format!(
            "{} marked",
            plural(app.marked_group_count(), "group")
        ));
    }
    if app.following() {
        parts.push("Following".into());
    }
    if parts.is_empty() {
        if let Some(session) = app.selected_session() {
            if let Some(place) = &session.workspace {
                parts.push(workspace::display(place, app.home()));
            }
            if let Some(change) = session.change.filter(|change| !change.is_zero()) {
                parts.push(format!(
                    "{} +{} −{}",
                    plural(change.files as usize, "file"),
                    change.added,
                    change.deleted
                ));
            }
            if app
                .attachment(session)
                .is_some_and(|(process, _)| process.pane.is_some())
            {
                parts.push("J jump to tmux".into());
            }
        }
    } else if filtering(app) {
        parts.push("x clear".into());
    }
    parts
}

fn draw_help(frame: &mut Frame<'_>, area: Rect, app: &App, lines: &[Line<'static>]) {
    // Full width when the terminal is narrower than the panel: a popup two
    // cells inside a bordered pane draws a second frame around the first.
    let width = area.width.min(88);
    let height = area.height.saturating_sub(2);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let end = (app.help_scroll() + height.saturating_sub(2) as usize).min(lines.len());
    let title = format!(" Help · {}–{end}/{} ", app.help_scroll() + 1, lines.len());
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((u16::try_from(app.help_scroll()).unwrap_or(u16::MAX), 0))
            .block(pane_block(title, true).title_bottom(" ↑↓ scroll · Esc close ")),
        popup,
    );
}

/// The help popup's rows: failures first, then the key table wrapped to the
/// popup's inner width.
fn help_lines(inner_width: usize, failures: &[String]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    // The status bar's warning count points here, so the detail leads: a
    // reader who pressed `?` because of it should not have to scroll.
    if !failures.is_empty() {
        lines.push(Line::styled(
            "Warnings",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        for message in failures {
            for row in wrap_words(message, inner_width.saturating_sub(2)) {
                lines.push(Line::styled(
                    format!("  {row}"),
                    Style::default().fg(Color::Yellow),
                ));
            }
        }
    }
    for &(key, description) in HELP {
        if key.is_empty() {
            if !lines.is_empty() {
                lines.push(Line::raw(""));
            }
            lines.push(Line::styled(
                description,
                Style::default().add_modifier(Modifier::BOLD),
            ));
            continue;
        }
        // A chord too long for the column takes its own line rather than
        // running into the description it is supposed to explain.
        let columns = inner_width >= 70;
        let inline = columns && Span::raw(key).width() < KEY_COLUMN - 1;
        let indent = if columns { KEY_COLUMN } else { 2 };
        let available = inner_width.saturating_sub(indent).max(1);
        if !inline {
            lines.push(Line::styled(key, Style::default().fg(Color::Cyan)));
        }
        for (row, text) in wrap_words(description, available).into_iter().enumerate() {
            lines.push(help_line(key, &text, inline && row == 0, indent));
        }
    }
    lines
}

fn help_line(key: &str, description: &str, show_key: bool, indent: usize) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            if show_key {
                format!("{key:<indent$}")
            } else {
                " ".repeat(indent)
            },
            Style::default().fg(Color::Cyan),
        ),
        Span::raw(description.to_owned()),
    ])
}

fn highlight(focused: bool) -> Style {
    if focused {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    }
}

fn liveness_color(liveness: Liveness) -> Color {
    match liveness {
        Liveness::Live { .. } => Color::Green,
        Liveness::Probably { .. } => Color::Yellow,
        Liveness::Recent => Color::Gray,
        Liveness::Stale => Color::Reset,
    }
}

/// The reader's palette, in one place.
///
/// Prose is the terminal's own foreground; everything else is a step away
/// from it. Machinery (tools, thinking, raw records) is grey so it never
/// competes with what was actually said, and only three hues carry meaning:
/// the user's bar, a failed result, and a followable link.
fn body_style(kind: LineKind) -> Style {
    match kind {
        LineKind::Rule => Style::default().fg(Color::Indexed(238)),
        LineKind::SpeakerUser => Style::default()
            .fg(Color::Indexed(111))
            .add_modifier(Modifier::BOLD),
        LineKind::SpeakerAgent => Style::default()
            .fg(Color::Indexed(180))
            .add_modifier(Modifier::BOLD),
        LineKind::SpeakerSystem => Style::default()
            .fg(Color::Indexed(244))
            .add_modifier(Modifier::BOLD),
        LineKind::Text | LineKind::Bullet | LineKind::Table => Style::default(),
        LineKind::Heading => Style::default()
            .fg(Color::Indexed(255))
            .add_modifier(Modifier::BOLD),
        LineKind::Code => Style::default().fg(Color::Indexed(109)),
        LineKind::Quote | LineKind::Thinking => Style::default()
            .fg(Color::Indexed(245))
            .add_modifier(Modifier::ITALIC),
        LineKind::Tool | LineKind::ToolOk => Style::default().fg(Color::Indexed(245)),
        LineKind::ToolErr => Style::default().fg(Color::Indexed(174)),
        LineKind::Files => Style::default().fg(Color::Indexed(250)),
        LineKind::Raw | LineKind::Meta => Style::default().fg(Color::Indexed(244)),
        LineKind::Link => Style::default()
            .fg(Color::Indexed(117))
            .add_modifier(Modifier::UNDERLINED),
    }
}

/// Inline emphasis, applied on top of the line's own style.
fn accent(base: Style, emphasis: Emphasis) -> Style {
    match emphasis {
        Emphasis::Strong => base.add_modifier(Modifier::BOLD).fg(Color::Indexed(255)),
        Emphasis::Soft => base.add_modifier(Modifier::ITALIC),
        Emphasis::Code => base.fg(Color::Indexed(109)),
    }
}

/// Gutter colour. The glyph carries the status so the body can stay grey.
fn rail_style(kind: LineKind) -> Style {
    match kind {
        LineKind::SpeakerUser
        | LineKind::Text
        | LineKind::Bullet
        | LineKind::Table
        | LineKind::Heading => Style::default().fg(Color::Indexed(111)),
        LineKind::ToolOk => Style::default().fg(Color::Indexed(107)),
        LineKind::ToolErr => Style::default().fg(Color::Indexed(203)),
        LineKind::Tool => Style::default().fg(Color::Indexed(110)),
        LineKind::Files => Style::default().fg(Color::Indexed(179)),
        LineKind::Link => Style::default().fg(Color::Indexed(117)),
        _ => Style::default().fg(Color::Indexed(238)),
    }
}

/// Break `text` on word boundaries into rows of at most `width` cells.
///
/// The help panel and the warning list are the only places the TUI wraps
/// its own copy rather than a transcript, and they wrap it the same way.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut row = String::new();
    for word in text.split_whitespace() {
        let needed = Span::raw(word).width() + usize::from(!row.is_empty());
        if !row.is_empty() && Span::raw(&row).width() + needed > width {
            rows.push(std::mem::take(&mut row));
        }
        if !row.is_empty() {
            row.push(' ');
        }
        row.push_str(word);
    }
    rows.push(row);
    rows
}

/// When a session was last written, on the reader's own clock.
///
/// The list is ordered by recency, so its top rows all share today's date
/// and the column says nothing; the clock time does. A weekday carries the
/// week, and anything older is a plain date — the same convention a mail
/// client or `ls -l` uses, for the same reason.
fn when(t: time::OffsetDateTime, now: time::OffsetDateTime) -> String {
    let at = crate::clock::at(t, now.offset());
    let (h, m, _) = at.time().as_hms();
    match (now.date() - at.date()).whole_days() {
        0 => format!("{h:02}:{m:02}"),
        1..=6 => format!("{} {h:02}:{m:02}", &at.weekday().to_string()[..3]),
        _ => at.date().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    /// The column answers "how recent" first: a date nobody can compare at a
    /// glance is kept for rows old enough to need one.
    #[test]
    fn a_row_stamp_gets_shorter_the_more_recent_it_is() {
        let now = datetime!(2026 - 09 - 21 16:05 UTC);
        assert_eq!(when(datetime!(2026 - 09 - 21 09:07 UTC), now), "09:07");
        // Yesterday evening is a weekday, not "0 days ago": the boundary is
        // the date, not 24 hours of elapsed time.
        assert_eq!(when(datetime!(2026 - 09 - 20 23:50 UTC), now), "Sun 23:50");
        assert_eq!(when(datetime!(2026 - 09 - 15 08:00 UTC), now), "Tue 08:00");
        assert_eq!(when(datetime!(2026 - 09 - 14 08:00 UTC), now), "2026-09-14");
    }

    /// A clipped cell must still read as one: the ellipsis replaces a
    /// character rather than being appended past the budget.
    #[test]
    fn truncation_never_exceeds_its_width() {
        assert_eq!(truncate_cells("session", 7), "session");
        assert_eq!(truncate_cells("session", 4), "ses…");
        assert_eq!(truncate_cells("session", 1), "…");
        assert_eq!(truncate_cells("session", 0), "");
    }

    /// One action per row: at the popup's normal width every key sits
    /// beside its description on a single line, never stacked above it.
    #[test]
    fn every_help_row_is_one_line_at_popup_width() {
        let lines = help_lines(86, &[]);
        for &(key, description) in HELP.iter().filter(|(key, _)| !key.is_empty()) {
            assert!(
                Span::raw(key).width() < KEY_COLUMN - 1,
                "key requires its own row: {key}"
            );
            assert!(
                KEY_COLUMN + Span::raw(description).width() <= 86,
                "description wraps: {key}"
            );
            let row = lines
                .iter()
                .find(|line| {
                    line.spans
                        .first()
                        .is_some_and(|span| span.content.trim_end() == key)
                })
                .unwrap_or_else(|| panic!("missing key row: {key}"));
            assert_eq!(
                row.spans[1].content, description,
                "description is not beside {key}"
            );
            assert!(row.width() <= 86, "rendered row exceeds popup width: {key}");
        }
        let rows = HELP.iter().filter(|(key, _)| !key.is_empty()).count();
        let sections = HELP.len() - rows;
        // Each section is a heading plus a blank separator, except the first.
        assert_eq!(lines.len(), rows + sections * 2 - 1);
    }
}
