//! TUI state. Rendering lives in `tui::ui`; input mapping in
//! `tui::keymap`. Everything here is synchronous and pure
//! enough to test without a terminal.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use time::{OffsetDateTime, UtcOffset};

use crate::catalog::label;
use crate::catalog::query::{Filters, Query};
use crate::catalog::scan::{Scan, scan_with};
use crate::clock;
use crate::model::{Label, Locator, SessionKey, SessionSummary, SourceId, Transcript};
use crate::output::reader::{RenderedLine, fit, plural, render};
use crate::runtime::live::{Activity, Fleet};
use crate::source::Source;
use crate::source::remote;
use crate::storage::index::{Index, UsageCache};
use crate::storage::overlay::Overlay;
use crate::tui::view::{self, Group, Grouping, Pane, Row, owned_key};

mod effects;

/// A full cross-agent scan is 81 ms, so rescanning on a timer costs less than
/// showing a launch-time snapshot that is already wrong.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// Percentage widths of the three panes. Defaults give the transcript the
/// most room while leaving a workspace path readable.
const DEFAULT_WIDTHS: [u16; 3] = [24, 32, 44];

/// No pane may be squeezed below this, or its contents stop being text.
const MIN_WIDTH: u16 = 12;

/// One `Ctrl-W` arrow press.
const RESIZE_STEP: i16 = 2;

/// A modal line at the bottom of the screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Prompt {
    None,
    Filter(String),
    Search(String),
    /// Sticky column-resize mode entered with `Ctrl-W`.
    Resize,
    /// One-key Vim fold command entered with `z`.
    Fold,
    /// One-key Vim yank command entered with `y`.
    Yank,
    /// Sticky one-key bookmark mode: `m<letter>` saves, `'<letter>` jumps.
    /// `jump` says which.
    Bookmark {
        jump: bool,
    },
    /// `archive == false` means un-archive.
    ConfirmArchive {
        count: usize,
        /// How many whole groups the count came from, for the wording.
        groups: usize,
        archive: bool,
    },
    /// Insert mode: one message for `target`'s agent, sent on Enter.
    Insert {
        target: SessionKey,
        text: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Quit,
    Down,
    Up,
    PageDown,
    PageUp,
    Top,
    Bottom,
    FocusNext,
    FocusPrev,
    CycleFocusNext,
    CycleFocusPrev,
    JumpLive,
    CycleGroupingNext,
    CycleGroupingPrev,
    /// Jump straight to one view, rather than cycling to it.
    SetGrouping(Grouping),
    ToggleArchived,
    ToggleLiveOnly,
    ToggleMark,
    ToggleGroupMark,
    MarkAll,
    ArchiveMarked,
    UnarchiveMarked,
    ToggleThinking,
    ToggleTools,
    ToggleFiles,
    ToggleFollow,
    /// `z` — wait for one Vim-style fold command.
    StartFold,
    /// `za` — hide or restore the selected session's descendants.
    ToggleFold,
    /// `zc` / `zo` — close or open the selected session's descendants.
    CloseFold,
    OpenFold,
    /// `zM` / `zR` — close or open every session fold.
    CloseAllFolds,
    OpenAllFolds,
    /// `y` — wait for one Vim-style yank command.
    StartYank,
    /// `yy` — copy the assistant reply at the reader's cursor, or the
    /// latest one when the reader is not focused.
    YankReply,
    /// `yA` — copy the open transcript as the reader shows it.
    YankTranscript,
    /// `yr` — copy the selected session's resume command.
    YankResume,
    /// `yi` — copy the selected session's id.
    YankId,
    /// `yp` — copy the selected session's transcript file or database path.
    YankPath,
    /// Give the reader the whole terminal.
    ToggleZen,
    Refresh,
    StartResize,
    Widen,
    Narrow,
    /// `m` — capture the current view/pane/session under a letter.
    StartBookmark,
    /// `'` — go back to one.
    StartJump,
    Resume,
    /// `i` — type one message for the selected session's agent.
    StartInsert,
    StartFilter,
    StartSearch,
    ClearFilter,
    ToggleHelp,
    Confirm,
    Cancel,
    Char(char),
    Backspace,
}

/// Whether a session's body has changed, without parsing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fingerprint {
    len: u64,
    stamp: i128,
}

pub struct App {
    home: PathBuf,
    sources: Vec<Box<dyn Source>>,
    local_origin: Option<String>,
    local_origin_failure: Option<String>,
    overlay: Option<Overlay>,
    index: Option<Index>,
    fleet: Fleet,
    /// Rows from configured hosts, refreshed off-thread. The panes never
    /// ssh: a two-second timer plus a round trip per host is unusable.
    remote: remote::Cache,
    /// Saved session shortcuts; letters, persisted under the state dir.
    bookmarks: crate::storage::alias::Store,
    bookmark_path: PathBuf,
    /// The clock every displayed timestamp is converted to. Captured once at
    /// startup, so a session does not drift while the process runs.
    offset: UtcOffset,

    sessions: Vec<SessionSummary>,
    /// What failed this scan, each message naming its own origin.
    failures: Vec<String>,
    /// Usage the index computed for agents that only record it in the body.
    cached_usage: UsageCache,
    /// First user prompt per session, from the index; feeds label derivation.
    prompts: HashMap<(SourceId, String), String>,
    /// Display name per session, with the rung of the chain that produced it.
    labels: HashMap<SessionKey, Label>,

    grouping: Grouping,
    show_archived: bool,
    /// Hide everything with no process attached.
    live_only: bool,
    filter: String,
    /// Parsed form of `filter`, shared with the CLI so the two cannot drift.
    query: Query,
    /// Restriction from a full-text search; `None` means unrestricted.
    hits: Option<HashSet<(SourceId, String)>>,

    groups: Vec<Group>,
    group_sel: usize,
    visible: Vec<Row>,
    session_sel: usize,
    marked: HashSet<SessionKey>,
    /// Whole groups selected with `V`; keyed like [`Group::key`].
    marked_groups: HashSet<Option<String>>,
    /// Sessions whose fork tree `F` has hidden. Keyed by identity, not row
    /// position, so a refilter, regroup or rescan cannot move a fold onto a
    /// different session. Session-lifetime only: nothing is written to disk.
    collapsed: HashSet<SessionKey>,

    focus: Pane,
    transcript: Option<Transcript>,
    transcript_key: Option<SessionKey>,
    transcript_print: Option<Fingerprint>,
    rendered: Vec<RenderedLine>,
    scroll: usize,
    /// Highlighted transcript line; `Enter` follows its link, if it has one.
    cursor: usize,
    viewport: usize,
    transcript_width: usize,
    /// Files the open session changed, folded by default.
    show_files: bool,
    /// The reader owns the whole terminal; panes and chrome step aside.
    zen: bool,

    fold_thinking: bool,
    fold_tools: bool,
    follow: bool,
    widths: [u16; 3],

    last_refresh: Instant,
    prompt: Prompt,
    status: Option<String>,
    help: bool,
    help_scroll: usize,
    help_viewport: usize,
    help_max_scroll: usize,
    read_only: bool,
    quit: bool,
    resume: Option<Vec<String>>,
    /// Insert-mode messages whose agent has not exited yet.
    deliveries: Vec<crate::runtime::insert::Delivery>,
    /// Where `y` sends text; tests substitute one that records it.
    clipboard: fn(String) -> crate::Result<crate::runtime::clipboard::PendingCopy>,
    clipboard_pending: Option<(crate::runtime::clipboard::PendingCopy, String)>,
}

impl App {
    pub fn new(
        home: &Path,
        sources: Vec<Box<dyn Source>>,
        sessions: Vec<SessionSummary>,
        failures: Vec<String>,
        overlay: Option<Overlay>,
        index: Option<Index>,
        fleet: Fleet,
    ) -> Self {
        Self::new_with_mode(
            home, sources, sessions, failures, overlay, index, fleet, false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_mode(
        home: &Path,
        sources: Vec<Box<dyn Source>>,
        mut sessions: Vec<SessionSummary>,
        mut failures: Vec<String>,
        overlay: Option<Overlay>,
        index: Option<Index>,
        fleet: Fleet,
        read_only: bool,
    ) -> Self {
        #[cfg(not(test))]
        let origin = if read_only {
            crate::federation::read_origin(home)
        } else {
            crate::federation::ensure_origin(home)
        };
        // Synthetic test homes must not create state on the workstation.
        #[cfg(test)]
        let origin: crate::Result<String> = Ok("9c66d120-48bf-44c5-a129-6d8d45c3e2a8".into());
        let (local_origin, local_origin_failure) = match origin {
            Ok(origin) => (Some(origin), None),
            Err(error) => (None, Some(format!("local origin: {error}"))),
        };
        failures.extend(local_origin_failure.iter().cloned());
        for session in sessions.iter_mut().filter(|session| session.is_local()) {
            session.origin_id.clone_from(&local_origin);
        }
        // The index already caches every session's first prompt for the
        // digest; labelling reuses it rather than opening a single body.
        let cached = index
            .as_ref()
            .and_then(|index| index.digest_data().ok())
            .unwrap_or_default();
        #[cfg(test)]
        let (mut cache, problems) = remote::Cache::empty();
        #[cfg(not(test))]
        let (mut cache, problems) = remote::Cache::new(home);
        // Test homes must not read (or write) the developer's real aliases.
        #[cfg(test)]
        let (bookmarks, bookmark_path, alias_failure) = (
            crate::storage::alias::Store::default(),
            PathBuf::new(),
            None,
        );
        #[cfg(not(test))]
        let (bookmarks, bookmark_path, alias_failure) = {
            let (store, failure) = crate::storage::alias::Store::load(home);
            (
                store,
                crate::storage::alias::Store::default_path(home),
                failure,
            )
        };
        failures.extend(problems);
        failures.extend(alias_failure);
        cache.poll(true);
        sessions.extend(cache.sessions().iter().cloned());
        sessions = crate::catalog::merge::merge(sessions);
        let mut app = Self {
            home: home.to_path_buf(),
            sources,
            local_origin,
            local_origin_failure,
            overlay,
            index,
            fleet,
            remote: cache,
            bookmarks,
            bookmark_path,
            offset: clock::offset(),
            sessions,
            failures,
            cached_usage: cached.usage,
            prompts: cached.prompts,
            labels: HashMap::new(),
            grouping: Grouping::Workspace,
            show_archived: false,
            live_only: false,
            filter: String::new(),
            query: Query::parse(""),
            hits: None,
            groups: Vec::new(),
            group_sel: 0,
            visible: Vec::new(),
            session_sel: 0,
            marked: HashSet::new(),
            marked_groups: HashSet::new(),
            collapsed: HashSet::new(),
            focus: Pane::Groups,
            transcript: None,
            transcript_key: None,
            transcript_print: None,
            rendered: Vec::new(),
            scroll: 0,
            cursor: 0,
            viewport: 20,
            transcript_width: 80,
            show_files: false,
            zen: false,
            fold_thinking: true,
            fold_tools: true,
            follow: false,
            widths: DEFAULT_WIDTHS,
            last_refresh: Instant::now(),
            prompt: Prompt::None,
            status: None,
            help: false,
            help_scroll: 0,
            help_viewport: 20,
            help_max_scroll: 0,
            read_only,
            quit: false,
            resume: None,
            deliveries: Vec::new(),
            // Tests must never reach the developer's real clipboard.
            #[cfg(not(test))]
            clipboard: crate::runtime::clipboard::start,
            #[cfg(test)]
            clipboard: |_| {
                Err(crate::error::Error::Clipboard {
                    message: "no clipboard in tests".into(),
                })
            },
            clipboard_pending: None,
        };
        app.apply_cached_usage();
        app.regroup();
        app
    }

    // ---- read-only accessors used by the renderer ----

    pub fn home(&self) -> &Path {
        &self.home
    }
    pub fn grouping(&self) -> Grouping {
        self.grouping
    }
    pub fn groups(&self) -> &[Group] {
        &self.groups
    }
    pub fn group_selected(&self) -> usize {
        self.group_sel
    }
    pub fn session_selected(&self) -> usize {
        self.session_sel
    }
    pub fn focus(&self) -> Pane {
        self.focus
    }
    pub fn prompt(&self) -> &Prompt {
        &self.prompt
    }
    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }
    pub fn show_help(&self) -> bool {
        self.help
    }
    pub fn help_scroll(&self) -> usize {
        self.help_scroll
    }
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// The one measure entry point: everything the renderer learned about
    /// the frame it is about to draw, applied before drawing so `ui::draw`
    /// stays a pure function of App state.
    pub fn apply_layout(&mut self, layout: &crate::tui::ui::Panes) {
        if layout.transcript_width > 0 {
            self.set_transcript_width(layout.transcript_width);
        }
        self.set_viewport(layout.viewport);
        if let Some((height, lines)) = layout.help {
            self.set_help_viewport(height, lines);
        }
    }

    pub(crate) fn set_help_viewport(&mut self, height: usize, lines: usize) {
        self.help_viewport = height.max(1);
        self.help_max_scroll = lines.saturating_sub(self.help_viewport);
        self.help_scroll = self.help_scroll.min(self.help_max_scroll);
    }

    pub(crate) fn set_transcript_width(&mut self, width: usize) {
        let width = width.max(1);
        if self.transcript_width != width {
            self.transcript_width = width;
            self.scroll = 0;
            self.cursor = 0;
            self.rerender();
            if self.follow {
                self.scroll_to_end();
            }
        }
    }

    pub fn should_quit(&self) -> bool {
        self.quit
    }
    pub fn resume_command(&self) -> Option<&[String]> {
        self.resume.as_deref()
    }
    pub fn failures(&self) -> &[String] {
        &self.failures
    }
    pub fn offset(&self) -> UtcOffset {
        self.offset
    }
    /// Letters that already hold a bookmark, in order — the prompt offers
    /// them instead of making the user guess which slots are taken.
    pub fn bookmark_letters(&self) -> Vec<char> {
        self.bookmarks.marks.keys().copied().collect()
    }
    /// Whether the open row's body is still coming over ssh.
    pub fn fetching(&self) -> bool {
        self.remote.fetching()
    }
    pub fn showing_archived(&self) -> bool {
        self.show_archived
    }
    pub fn live_only(&self) -> bool {
        self.live_only
    }
    pub fn filter(&self) -> &str {
        &self.filter
    }
    pub fn searching(&self) -> bool {
        self.hits.is_some()
    }
    pub fn total_sessions(&self) -> usize {
        self.sessions.len()
    }
    pub fn rendered(&self) -> &[RenderedLine] {
        &self.rendered
    }
    pub fn scroll(&self) -> usize {
        self.scroll
    }
    pub fn transcript(&self) -> Option<&Transcript> {
        self.transcript.as_ref()
    }
    pub fn folding(&self) -> (bool, bool) {
        (self.fold_thinking, self.fold_tools)
    }
    pub fn following(&self) -> bool {
        self.follow
    }
    pub fn zen(&self) -> bool {
        self.zen
    }
    pub fn fleet(&self) -> &Fleet {
        &self.fleet
    }
    pub fn rows(&self) -> &[Row] {
        &self.visible
    }
    pub fn session(&self, row: Row) -> &SessionSummary {
        &self.sessions[row.index]
    }

    /// Display name and the rung of the chain that produced it. The renderer
    /// dims anything that is not [`crate::model::LabelSource::Title`] so a
    /// derived name never passes as one the agent chose.
    pub fn label(&self, session: &SessionSummary) -> Label {
        self.labels
            .get(&session.identity())
            .cloned()
            .unwrap_or_else(|| label::derive(session, None))
    }

    pub fn show_files(&self) -> bool {
        self.show_files
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Live process detail for a row, when one is attached.
    pub fn attachment(
        &self,
        session: &SessionSummary,
    ) -> Option<(&crate::runtime::live::Process, Activity)> {
        if !session.is_local() {
            return None;
        }
        let pid = session.liveness.pid()?;
        let process = self.fleet.get(pid)?;
        Some((process, process.activity))
    }

    pub fn visible_len(&self) -> usize {
        self.visible.len()
    }

    pub fn is_marked(&self, session: &SessionSummary) -> bool {
        self.marked.contains(&owned_key(session))
    }

    pub fn marked_count(&self) -> usize {
        self.marked.len()
    }

    pub fn selected_session(&self) -> Option<&SessionSummary> {
        self.visible
            .get(self.session_sel)
            .map(|row| &self.sessions[row.index])
    }

    /// The renderer knows the real transcript height; scrolling needs it.
    pub(crate) fn set_viewport(&mut self, rows: usize) {
        if self.viewport != rows {
            self.viewport = rows;
            if rows > 0 && self.focus == Pane::Transcript {
                if self.follow {
                    self.scroll_to_end();
                } else {
                    self.keep_cursor_visible();
                }
            }
        }
    }

    /// Refuse every mutation. Set from `--read-only`.
    pub fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
    }

    // ---- the clock ----

    /// Called by the event loop whenever it has nothing else to do.
    ///
    /// Rescanning on a timer is what keeps liveness, titles and ordering
    /// honest; follow mode is the same idea applied to one open transcript.
    pub fn tick(&mut self) {
        self.collect_clipboard();
        // The remote cache is polled every wake-up, not on the local timer:
        // collecting it is free, and a finished refresh — or transcript —
        // should show up immediately rather than up to `REFRESH_INTERVAL`
        // late.
        self.collect_deliveries();
        self.collect_remote_transcript();
        if self.remote.poll(false) {
            self.rescan();
        }
        if self.last_refresh.elapsed() < REFRESH_INTERVAL {
            return;
        }
        self.last_refresh = Instant::now();
        self.rescan();
        if self.follow {
            self.reload_if_changed();
        }
    }

    fn rescan(&mut self) {
        self.fleet.refresh();
        // No hosts here: the cache owns remote rows precisely so this
        // two-second cadence never waits on a socket.
        let Scan { sessions, failures } =
            scan_with(&self.sources, self.overlay.as_ref(), Some(&self.fleet), &[]);
        // Selection is by identity, not position: a rescan reorders rows when
        // a session goes live, and the cursor must not jump to a stranger.
        let group = self.groups.get(self.group_sel).and_then(|g| g.key.clone());
        let selected = self.selected_session().map(owned_key);

        self.sessions = sessions;
        for session in self
            .sessions
            .iter_mut()
            .filter(|session| session.is_local())
        {
            session.origin_id.clone_from(&self.local_origin);
        }
        self.sessions.extend(self.remote.sessions().iter().cloned());
        self.sessions = crate::catalog::merge::merge(std::mem::take(&mut self.sessions));
        self.failures = failures;
        self.failures
            .extend(self.local_origin_failure.iter().cloned());
        self.failures.extend(self.remote.failures().iter().cloned());
        self.apply_cached_usage();
        self.regroup();

        if let Some(key) = group
            && let Some(position) = self
                .groups
                .iter()
                .position(|g| g.key.as_deref() == Some(key.as_str()))
        {
            self.group_sel = position;
            self.refilter();
        }
        if let Some(key) = selected
            && let Some(position) = self
                .visible
                .iter()
                .position(|row| owned_key(&self.sessions[row.index]) == key)
        {
            self.session_sel = position;
            self.load_transcript();
        }
    }

    /// Fill in usage the sources cannot price at tier 1 from the index cache.
    fn apply_cached_usage(&mut self) {
        self.cached_usage.fill(&mut self.sessions);
    }

    // ---- input ----

    pub fn handle(&mut self, action: Action) {
        self.status = None;
        // A held key starves `tick`; a transcript that has already arrived
        // must not wait for the keyboard to go quiet.
        self.collect_remote_transcript();
        self.collect_deliveries();
        self.collect_clipboard();
        // Quit is global, including unfinished edits and destructive prompts.
        if action == Action::Quit {
            self.quit = true;
            return;
        }
        // Help owns navigation while open; no action reaches the panes.
        if self.help {
            match action {
                Action::ToggleHelp | Action::Cancel => self.help = false,
                Action::Down => {
                    self.help_scroll = self.help_scroll.saturating_add(1).min(self.help_max_scroll);
                }
                Action::Up => self.help_scroll = self.help_scroll.saturating_sub(1),
                Action::PageDown => {
                    self.help_scroll = self
                        .help_scroll
                        .saturating_add(self.help_viewport)
                        .min(self.help_max_scroll);
                }
                Action::PageUp => {
                    self.help_scroll = self.help_scroll.saturating_sub(self.help_viewport);
                }
                Action::Top => self.help_scroll = 0,
                Action::Bottom => self.help_scroll = self.help_max_scroll,
                _ => {}
            }
            return;
        }
        match self.prompt.clone() {
            Prompt::Filter(text) => return self.editing(action, text, false),
            Prompt::Search(text) => return self.editing(action, text, true),
            Prompt::Resize => return self.resizing(action),
            Prompt::Fold => return self.fold_command(action),
            Prompt::Yank => {
                self.prompt = Prompt::None;
                return match action {
                    Action::YankReply => self.yank(false),
                    Action::YankTranscript => self.yank(true),
                    Action::YankResume => self.yank_resume(),
                    Action::YankId => self.yank_session_field(false),
                    Action::YankPath => self.yank_session_field(true),
                    _ => {}
                };
            }
            Prompt::Bookmark { jump } => return self.bookmarking(action, jump),
            Prompt::ConfirmArchive { archive, .. } => return self.confirming(action, archive),
            Prompt::Insert { target, text } => return self.composing(action, target, text),
            Prompt::None => {}
        }

        // Fullscreen is a reading mode, not a pane: pane navigation leaves it
        // instead of moving focus behind an invisible layout.
        if self.zen {
            if action == Action::StartResize {
                // `Ctrl-W` is a prefix here too. Keep the reader visible
                // until its second key decides whether to leave Zen.
                self.prompt = Prompt::Resize;
                return;
            }
            match action {
                Action::ToggleZen
                | Action::Cancel
                | Action::FocusPrev
                | Action::FocusNext
                | Action::CycleFocusNext
                | Action::CycleFocusPrev => {
                    self.zen = false;
                    return;
                }
                // These open a prompt or act on the lists, and fullscreen
                // draws neither. Falling through would leave an invisible
                // prompt swallowing every key.
                Action::StartFilter
                | Action::StartSearch
                | Action::ClearFilter
                | Action::CycleGroupingNext
                | Action::CycleGroupingPrev
                | Action::SetGrouping(_)
                | Action::ToggleArchived
                | Action::ToggleLiveOnly
                | Action::ToggleMark
                | Action::ToggleGroupMark
                | Action::MarkAll
                | Action::ArchiveMarked
                | Action::UnarchiveMarked
                | Action::StartInsert => self.zen = false,
                _ => {}
            }
        }

        match action {
            Action::Quit => unreachable!("handled before modal input"),
            Action::ToggleHelp => {
                self.help = true;
                self.help_scroll = 0;
            }
            Action::Down => self.step(1),
            Action::Up => self.step(-1),
            Action::PageDown => self.step(page(self.viewport)),
            Action::PageUp => self.step(-page(self.viewport)),
            Action::Top => self.jump(true),
            Action::Bottom => self.jump(false),
            Action::FocusNext => self.focus_next(),
            Action::FocusPrev => self.focus_prev(),
            Action::CycleFocusNext => {
                if self.focus == Pane::Transcript {
                    self.focus = Pane::Groups;
                } else {
                    self.focus_next();
                }
            }
            Action::CycleFocusPrev => {
                if self.focus == Pane::Groups {
                    self.focus = Pane::Transcript;
                    self.load_transcript();
                } else {
                    self.focus_prev();
                }
            }
            Action::JumpLive => self.jump_live(),
            Action::CycleGroupingNext => self.show(self.grouping.next()),
            Action::CycleGroupingPrev => self.show(self.grouping.prev()),
            Action::SetGrouping(grouping) => self.show(grouping),
            Action::ToggleArchived => {
                self.show_archived = !self.show_archived;
                self.regroup();
            }
            Action::ToggleLiveOnly => {
                self.live_only = !self.live_only;
                // Groups vanish along with their sessions, so the left pane
                // must be rebuilt, not merely refiltered.
                self.group_sel = 0;
                self.regroup();
            }
            Action::ToggleMark => self.toggle_mark(),
            Action::ToggleGroupMark => self.toggle_group_mark(),
            Action::MarkAll => self.mark_all(),
            Action::ArchiveMarked => self.request_archive(true),
            Action::UnarchiveMarked => self.request_archive(false),
            Action::ToggleThinking => {
                self.fold_thinking = !self.fold_thinking;
                self.rerender();
            }
            Action::ToggleTools => {
                self.fold_tools = !self.fold_tools;
                self.rerender();
            }
            Action::ToggleFiles => {
                self.show_files = !self.show_files;
                self.rerender();
            }
            Action::ToggleFollow => {
                self.follow = !self.follow;
                if self.follow {
                    self.reload_if_changed();
                    self.scroll_to_end();
                }
            }
            Action::ToggleFold => self.toggle_fold(),
            Action::CloseFold => self.set_selected_fold(true),
            Action::OpenFold => self.set_selected_fold(false),
            Action::CloseAllFolds => self.close_all_folds(),
            Action::OpenAllFolds => {
                self.collapsed.clear();
                self.refilter();
            }
            // Entering from a list pane is the common case: fullscreen should
            // read the highlighted session, not an empty page.
            Action::ToggleZen => {
                self.zen = true;
                self.focus = Pane::Transcript;
                self.load_transcript();
            }
            Action::Refresh => {
                self.remote.poll(true);
                self.last_refresh = Instant::now();
                self.rescan();
                self.reload_if_changed();
                self.status = Some("rescanned".into());
            }
            Action::StartResize => self.prompt = Prompt::Resize,
            Action::StartFold => {
                if self.focus == Pane::Sessions {
                    self.prompt = Prompt::Fold;
                }
            }
            Action::StartYank => self.prompt = Prompt::Yank,
            Action::YankReply => self.yank(false),
            Action::YankTranscript => self.yank(true),
            Action::YankResume => self.yank_resume(),
            Action::YankId => self.yank_session_field(false),
            Action::YankPath => self.yank_session_field(true),
            Action::StartBookmark => self.prompt = Prompt::Bookmark { jump: false },
            Action::StartJump => self.prompt = Prompt::Bookmark { jump: true },
            Action::Resume => self.request_resume(),
            Action::StartInsert => self.start_insert(),
            Action::StartFilter => self.prompt = Prompt::Filter(self.filter.clone()),
            Action::StartSearch => self.prompt = Prompt::Search(String::new()),
            Action::ClearFilter => {
                self.filter.clear();
                self.query = Query::parse("");
                self.hits = None;
                self.live_only = false;
                self.group_sel = 0;
                self.regroup();
            }
            // Nothing left to back out of: say how to leave rather than
            // leaving. `q` is pressed by reflex; quitting must be asked for.
            Action::Cancel => {
                if self.focus == Pane::Groups {
                    self.status = Some("Q quits · Ctrl-C quits".into());
                } else {
                    self.focus_prev();
                }
            }
            Action::Confirm => self.advance(),
            // Only meaningful inside the resize and text prompts.
            Action::Widen | Action::Narrow | Action::Char(_) | Action::Backspace => {}
        }
    }

    /// Switch the left pane's axis. Selection restarts: row 3 of "agent"
    /// means nothing in "branch".
    fn show(&mut self, grouping: Grouping) {
        self.grouping = grouping;
        self.group_sel = 0;
        self.regroup();
    }

    /// Column widths as percentages, summing to 100.
    pub fn widths(&self) -> [u16; 3] {
        self.widths
    }

    /// `Ctrl-W` is sticky: arrows keep resizing until Esc or Enter, because
    /// finding the right split takes several presses, not one.
    fn resizing(&mut self, action: Action) {
        match action {
            Action::Widen | Action::FocusNext => {
                self.zen = false;
                self.resize(RESIZE_STEP);
            }
            Action::Narrow | Action::FocusPrev => {
                self.zen = false;
                self.resize(-RESIZE_STEP);
            }
            Action::ToggleZen => {
                self.prompt = Prompt::None;
                self.zen = !self.zen;
                if self.zen {
                    self.focus = Pane::Transcript;
                    self.load_transcript();
                }
            }
            Action::Confirm | Action::Cancel | Action::StartResize => self.prompt = Prompt::None,
            // `=` restores the default split, the way a tiling WM does.
            Action::Char('=') => self.widths = DEFAULT_WIDTHS,
            _ => {}
        }
    }

    /// One key decides the bookmark, then the mode is gone: `m` then `x`
    /// saves under `x`; `'` then `x` goes back to it.
    fn bookmarking(&mut self, action: Action, jump: bool) {
        match action {
            Action::Cancel | Action::Confirm => self.prompt = Prompt::None,
            Action::Char(c) => {
                self.prompt = Prompt::None;
                if !crate::storage::alias::valid_letter(c) {
                    self.status = Some(format!("bookmarks are 'a' to 'z'; {c:?} is not one"));
                    return;
                }
                if jump {
                    self.jump_to_bookmark(c);
                } else {
                    self.save_bookmark(c);
                }
            }
            _ => {}
        }
    }

    /// Capture the current view, focus and row under a letter.
    fn save_bookmark(&mut self, letter: char) {
        let Some(session) = self.selected_session() else {
            self.status = Some("nothing selected to bookmark".into());
            return;
        };
        if self.read_only {
            self.status = Some("read-only refuses saving bookmarks".into());
            return;
        }
        if session.origin_id.is_none() {
            self.status = Some("cannot bookmark: canonical origin identity is unavailable".into());
            return;
        }
        let bookmark = crate::storage::alias::Bookmark {
            origin_id: session.origin_id.clone(),
            host: session.origin_label.clone(),
            source: session.source,
            id: session.id.clone(),
            view: self.grouping.label().to_owned(),
            focus: self.focus.letter(),
        };
        let replaced = self.bookmarks.marks.insert(letter, bookmark).is_some();
        // Write-through: a bookmark is worthless if it dies with the process,
        // and the file is one pretty-printed map.
        let what = if replaced { "updated" } else { "saved" };
        match self.bookmarks.save_at(&self.bookmark_path) {
            Ok(()) => self.status = Some(format!("bookmark '{letter} {what}")),
            Err(e) => self.status = Some(format!("cannot save bookmark: {e}")),
        }
    }

    /// Put the view, the group, the row and the focus back as they were.
    fn jump_to_bookmark(&mut self, letter: char) {
        let Some(bookmark) = self.bookmarks.get(letter).cloned() else {
            self.status = Some(format!("no bookmark '{letter}"));
            return;
        };
        // The grouping is restored first so the group lookup below sees the
        // same axis the bookmark was saved under.
        self.show(Grouping::from_label(&bookmark.view));
        let Some(index) = self.sessions.iter().position(|session| {
            bookmark.origin_id.is_some()
                && session.origin_id == bookmark.origin_id
                && session.source == bookmark.source
                && session.id == bookmark.id
        }) else {
            self.status = Some(format!(
                "bookmark '{letter}: that session is gone (archived away, or its host is down)"
            ));
            return;
        };
        let key = view::group_key(&self.sessions[index], self.grouping, &self.sessions);
        if let Some(position) = self.groups.iter().position(|group| group.key == key) {
            self.group_sel = position;
        }
        self.refilter();
        match self.visible.iter().position(|row| row.index == index) {
            Some(position) => {
                self.session_sel = position;
                self.focus = Pane::from_letter(bookmark.focus);
                if self.focus == Pane::Transcript {
                    self.load_transcript();
                } else {
                    self.transcript_key = None;
                    self.transcript_print = None;
                    self.load_transcript();
                }
                self.status = Some(format!("bookmark '{letter}"));
            }
            // The session exists but the current filter or live-only toggle
            // hides it; saying so beats silently selecting its neighbour.
            None => {
                self.status = Some(format!(
                    "bookmark '{letter} is hidden by the current filter (x clears)"
                ));
            }
        }
    }

    /// Grow or shrink the focused pane, taking the difference from its
    /// neighbour so the three always sum to 100.
    fn resize(&mut self, delta: i16) {
        let focused = match self.focus {
            Pane::Groups => 0usize,
            Pane::Sessions => 1,
            Pane::Transcript => 2,
        };
        // The last pane has no pane to its right, so it trades with its left.
        let neighbour = if focused == 2 { 1 } else { focused + 1 };
        let grown = self.widths[focused]
            .saturating_add_signed(delta)
            .max(MIN_WIDTH);
        let Some(shed) = (self.widths[focused] + self.widths[neighbour])
            .checked_sub(grown)
            .filter(|&shed| shed >= MIN_WIDTH)
        else {
            return;
        };
        self.widths[focused] = grown;
        self.widths[neighbour] = shed;
    }

    fn editing(&mut self, action: Action, mut text: String, search: bool) {
        match action {
            Action::Char(c) => {
                text.push(c);
                self.prompt = prompt_of(text, search);
            }
            Action::Backspace => {
                text.pop();
                self.prompt = prompt_of(text, search);
            }
            Action::Cancel => self.prompt = Prompt::None,
            Action::Confirm => {
                self.prompt = Prompt::None;
                if search {
                    self.run_search(&text);
                } else {
                    self.query = Query::parse(&text);
                    self.filter = text;
                    self.regroup();
                }
            }
            _ => {}
        }
    }

    fn composing(&mut self, action: Action, target: SessionKey, mut text: String) {
        match action {
            Action::Char(c) => {
                text.push(c);
                self.prompt = Prompt::Insert { target, text };
            }
            Action::Backspace => {
                text.pop();
                self.prompt = Prompt::Insert { target, text };
            }
            Action::Cancel => self.prompt = Prompt::None,
            Action::Confirm => {
                self.prompt = Prompt::None;
                self.send_insert(&target, &text);
            }
            _ => {}
        }
    }

    fn confirming(&mut self, action: Action, archive: bool) {
        match action {
            Action::Confirm | Action::Char('y' | 'Y') => {
                self.prompt = Prompt::None;
                self.apply_archive(archive);
            }
            Action::Cancel | Action::Char('n' | 'N') => {
                self.prompt = Prompt::None;
            }
            _ => {}
        }
    }

    // ---- navigation ----

    /// Confirm opens the reader or follows its highlighted subagent link.
    fn advance(&mut self) {
        if self.focus == Pane::Transcript && self.follow_link() {
            return;
        }
        self.focus_next();
    }

    fn step(&mut self, delta: isize) {
        match self.focus {
            Pane::Groups => {
                self.group_sel = shift(self.group_sel, delta, self.groups.len());
                self.refilter();
            }
            Pane::Sessions => {
                self.session_sel = shift(self.session_sel, delta, self.visible.len());
                self.load_transcript();
            }
            Pane::Transcript => {
                let max = self.rendered.len().saturating_sub(1);
                self.cursor = shift(self.cursor, delta, max + 1);
                self.keep_cursor_visible();
            }
        }
    }

    fn jump(&mut self, top: bool) {
        let last = |n: usize| n.saturating_sub(1);
        match self.focus {
            Pane::Groups => {
                self.group_sel = if top { 0 } else { last(self.groups.len()) };
                self.refilter();
            }
            Pane::Sessions => {
                self.session_sel = if top { 0 } else { last(self.visible.len()) };
                self.load_transcript();
            }
            Pane::Transcript => {
                self.cursor = if top { 0 } else { last(self.rendered.len()) };
                self.keep_cursor_visible();
            }
        }
    }

    fn focus_next(&mut self) {
        self.focus = match self.focus {
            Pane::Groups => Pane::Sessions,
            Pane::Sessions | Pane::Transcript => Pane::Transcript,
        };
        if self.focus == Pane::Transcript {
            self.load_transcript();
        }
    }

    fn focus_prev(&mut self) {
        self.focus = match self.focus {
            Pane::Transcript => Pane::Sessions,
            Pane::Sessions | Pane::Groups => Pane::Groups,
        };
    }

    // ---- grouping and filtering ----

    fn filters(&self) -> Filters<'_> {
        Filters {
            query: &self.query,
            hits: self.hits.as_ref(),
            show_archived: self.show_archived,
            live_only: self.live_only,
        }
    }

    /// Rebuild the left pane, then the session list under it.
    pub fn regroup(&mut self) {
        self.labels = label::assign(&self.sessions, &self.prompts);
        self.groups = view::groups(
            &self.sessions,
            self.grouping,
            &self.home,
            &self.fleet,
            &self.filters(),
        );
        if self.group_sel >= self.groups.len() {
            self.group_sel = self.groups.len().saturating_sub(1);
        }
        self.refilter();
    }

    fn refilter(&mut self) {
        let selected = self
            .groups
            .get(self.group_sel)
            .and_then(|group| group.key.as_deref());
        let members = view::members(&self.sessions, self.grouping, &self.filters(), selected);
        self.visible = view::tree(&self.sessions, &members, &self.collapsed);
        if self.session_sel >= self.visible.len() {
            self.session_sel = self.visible.len().saturating_sub(1);
        }
        self.load_transcript();
    }

    // ---- transcript ----

    fn load_transcript(&mut self) {
        let Some(session) = self.selected_session() else {
            self.transcript = None;
            self.transcript_key = None;
            self.transcript_print = None;
            self.rendered.clear();
            self.scroll = 0;
            self.cursor = 0;
            return;
        };
        let key = owned_key(session);
        if self.transcript_key.as_ref() == Some(&key) {
            return;
        }
        self.scroll = 0;
        self.cursor = 0;
        self.open_selected(key);
    }

    /// Re-read the open transcript when its body has actually changed.
    ///
    /// Cheap enough to run every tick: a `stat` for file-backed sources, and
    /// the already-scanned `updated` for database rows.
    fn reload_if_changed(&mut self) {
        let Some(session) = self.selected_session() else {
            return;
        };
        let key = owned_key(session);
        let print = fingerprint(session);
        if self.transcript_key.as_ref() == Some(&key) && self.transcript_print == print {
            return;
        }
        let at_end = self.scroll + self.viewport >= self.rendered.len();
        self.open_selected(key);
        if at_end || self.follow {
            self.scroll_to_end();
        }
    }

    fn scroll_to_end(&mut self) {
        self.scroll = self.rendered.len().saturating_sub(self.viewport);
        self.cursor = self.rendered.len().saturating_sub(1);
    }

    /// Scroll the minimum needed to keep the highlighted line on screen.
    fn keep_cursor_visible(&mut self) {
        if self.viewport == 0 {
            return;
        }
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + self.viewport {
            self.scroll = self.cursor + 1 - self.viewport;
        }
    }

    fn rerender(&mut self) {
        let subagents = self.subagents_of_selected();
        let lines = match &self.transcript {
            Some(transcript) => render(
                transcript,
                self.fold_thinking,
                self.fold_tools,
                self.show_files,
                &subagents,
                self.offset,
            ),
            None => Vec::new(),
        };
        self.rendered = fit(lines, self.transcript_width);
        if self.scroll >= self.rendered.len() {
            self.scroll = self.rendered.len().saturating_sub(1);
        }
        if self.cursor >= self.rendered.len() {
            self.cursor = self.rendered.len().saturating_sub(1);
        }
    }

    /// Child transcripts of the open session, keyed by the task name their
    /// filename encodes.
    fn subagents_of_selected(&self) -> HashMap<String, (SourceId, String)> {
        let Some(session) = self.selected_session() else {
            return HashMap::new();
        };
        let (source, id) = (session.source, session.id.clone());
        self.sessions
            .iter()
            .filter(|child| {
                child.subagent
                    && child.origin_id == session.origin_id
                    && child.source == source
                    && child.parent.as_deref() == Some(&id)
            })
            .filter_map(|child| {
                child
                    .title
                    .clone()
                    .map(|task| (task, (child.source, child.id.clone())))
            })
            .collect()
    }

    /// Jump to the session a transcript line links to.
    fn follow_link(&mut self) -> bool {
        let Some(target) = self
            .rendered
            .get(self.cursor)
            .and_then(|line| line.link.clone())
        else {
            return false;
        };
        let origin = self.transcript_key.as_ref().and_then(|key| key.0.clone());
        self.jump_to(&(origin, target.0, target.1));
        true
    }

    /// Select a session by key.
    ///
    /// False when it is filtered out of the current view: widening the view
    /// silently would be worse than saying so.
    fn jump_to(&mut self, target: &SessionKey) -> bool {
        let Some(position) = self
            .visible
            .iter()
            .position(|row| owned_key(&self.sessions[row.index]) == *target)
        else {
            self.status = Some(format!("{} {} is not in this view", target.1, target.2));
            return false;
        };
        self.session_sel = position;
        self.focus = Pane::Transcript;
        self.load_transcript();
        true
    }

    // ---- actions ----

    fn toggle_mark(&mut self) {
        let Some(session) = self.selected_session() else {
            return;
        };
        let key = owned_key(session);
        if !self.marked.remove(&key) {
            self.marked.insert(key);
        }
        self.step(1);
    }

    /// `za` — hide or restore everything nested under the selected session.
    fn toggle_fold(&mut self) {
        let Some(row) = self.selected_parent_row() else {
            return;
        };
        let key = owned_key(&self.sessions[row.index]);
        let fold = !self.collapsed.contains(&key);
        self.set_fold(&key, fold);
    }

    /// `zc` / `zo` — set the selected session's fold explicitly.
    fn set_selected_fold(&mut self, fold: bool) {
        let Some(row) = self.selected_parent_row() else {
            return;
        };
        self.set_fold(&owned_key(&self.sessions[row.index]), fold);
    }

    fn selected_parent_row(&mut self) -> Option<Row> {
        if self.focus != Pane::Sessions {
            return None;
        }
        let row = self.visible.get(self.session_sel).copied()?;
        if !row.children {
            self.status = Some("nothing is nested under this session".into());
            return None;
        }
        Some(row)
    }

    fn set_fold(&mut self, key: &SessionKey, fold: bool) {
        if fold {
            self.collapsed.insert(key.clone());
        } else {
            self.collapsed.remove(key);
        }
        self.refilter();
        // A fold only adds or removes rows below its owner. Re-finding the
        // owner makes selection stability explicit instead of positional.
        if let Some(position) = self
            .visible
            .iter()
            .position(|row| owned_key(&self.sessions[row.index]) == *key)
            && position != self.session_sel
        {
            self.session_sel = position;
            self.load_transcript();
        }
    }

    /// `zM` — close every branch, including branches outside the current
    /// group. The stable keys keep the command true after changing views.
    fn close_all_folds(&mut self) {
        self.collapsed
            .extend(self.sessions.iter().filter_map(|session| {
                session
                    .parent
                    .as_ref()
                    .map(|parent| (session.origin_id.clone(), session.source, parent.clone()))
            }));
        self.refilter();
    }

    /// A `z` command consumes exactly one following key.
    fn fold_command(&mut self, action: Action) {
        self.prompt = Prompt::None;
        match action {
            Action::ToggleFold => self.toggle_fold(),
            Action::CloseFold => self.set_selected_fold(true),
            Action::OpenFold => self.set_selected_fold(false),
            Action::CloseAllFolds => self.close_all_folds(),
            Action::OpenAllFolds => {
                self.collapsed.clear();
                self.refilter();
            }
            _ => {}
        }
    }

    /// Mark every session currently listed. Already-marked rows stay marked,
    /// so a second press is not a toggle-off of the whole list.
    fn mark_all(&mut self) {
        let keys: Vec<_> = self
            .visible
            .iter()
            .map(|row| owned_key(&self.sessions[row.index]))
            .collect();
        self.marked.extend(keys);
    }

    /// Mark or unmark the highlighted group, so a whole project can be
    /// archived in one action.
    ///
    /// Marking a group is not the same as marking each of its rows: the group
    /// keeps its meaning while the session list is filtered, and it covers
    /// sessions the current filter is hiding — which is the point when you
    /// are retiring a finished project rather than curating a list.
    fn toggle_group_mark(&mut self) {
        let Some(group) = self.groups.get(self.group_sel) else {
            return;
        };
        let key = group.key.clone();
        if !self.marked_groups.remove(&key) {
            self.marked_groups.insert(key);
        }
    }

    pub fn is_group_marked(&self, group: &Group) -> bool {
        self.marked_groups.contains(&group.key)
    }

    pub fn marked_group_count(&self) -> usize {
        self.marked_groups.len()
    }

    /// Every session a marked group owns, whatever the current filter shows.
    fn group_members(&self) -> Vec<SessionKey> {
        if self.marked_groups.is_empty() {
            return Vec::new();
        }
        self.sessions
            .iter()
            .filter(|session| {
                self.marked_groups.contains(&view::group_key(
                    session,
                    self.grouping,
                    &self.sessions,
                ))
            })
            .map(owned_key)
            .collect()
    }

    /// What an archive action would touch: marked groups, then marked rows,
    /// and only if neither exists, the row under the cursor.
    fn targets(&self) -> Vec<SessionKey> {
        let mut keys: HashSet<SessionKey> = self.group_members().into_iter().collect();
        keys.extend(self.marked.iter().cloned());
        if keys.is_empty() {
            keys.extend(self.selected_session().map(owned_key));
        }
        keys.retain(|key| {
            self.sessions
                .iter()
                .any(|session| session.is_local() && session.identity() == *key)
        });
        let mut keys: Vec<_> = keys.into_iter().collect();
        keys.sort();
        keys
    }

    fn request_archive(&mut self, archive: bool) {
        if self.read_only {
            self.status = Some("--read-only: refusing to change archive state".into());
            return;
        }
        // Only count sessions the action would actually change; confirming
        // "archive 102" when 98 are already archived is a lie.
        let count = self
            .targets()
            .iter()
            .filter(|key| {
                self.sessions
                    .iter()
                    .any(|s| s.is_local() && s.identity() == **key && s.archived != archive)
            })
            .count();
        if count == 0 {
            self.status = Some(format!(
                "nothing to {}",
                if archive { "archive" } else { "restore" }
            ));
            return;
        }
        self.prompt = Prompt::ConfirmArchive {
            count,
            groups: self.marked_groups.len(),
            archive,
        };
    }

    fn run_search(&mut self, query: &str) {
        if query.trim().is_empty() {
            self.hits = None;
            self.regroup();
            return;
        }
        let Some(index) = &self.index else {
            self.status = Some("no search index; run `llm-sessions index` first".into());
            return;
        };
        match index.search(query, 500) {
            Ok(hits) => {
                let found: HashSet<(SourceId, String)> =
                    hits.into_iter().map(|h| (h.source, h.session_id)).collect();
                self.status = Some(format!("{} for {query:?}", plural(found.len(), "match")));
                self.hits = Some(found);
                self.group_sel = 0;
                self.regroup();
            }
            Err(e) => self.status = Some(e.to_string()),
        }
    }
}

fn prompt_of(text: String, search: bool) -> Prompt {
    if search {
        Prompt::Search(text)
    } else {
        Prompt::Filter(text)
    }
}

/// A page step as a signed cursor delta. A viewport cannot approach
/// `isize::MAX` rows, so saturating is never observable.
fn page(viewport: usize) -> isize {
    isize::try_from(viewport).unwrap_or(isize::MAX)
}

fn shift(current: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    current.saturating_add_signed(delta).min(len - 1)
}

/// Has the body changed? A `stat` for files; the scanned timestamp otherwise.
fn fingerprint(session: &SessionSummary) -> Option<Fingerprint> {
    if !session.is_local() {
        return Some(Fingerprint {
            len: session.size_bytes.unwrap_or(0),
            stamp: session.updated.unix_timestamp_nanos(),
        });
    }
    match &session.locator {
        Locator::File { path } => {
            let meta = std::fs::metadata(path).ok()?;
            let stamp = meta
                .modified()
                .ok()
                .map(|t| OffsetDateTime::from(t).unix_timestamp_nanos())
                .unwrap_or_default();
            Some(Fingerprint {
                len: meta.len(),
                stamp,
            })
        }
        Locator::Row { .. } => Some(Fingerprint {
            len: 0,
            stamp: i128::from(session.updated.unix_timestamp()),
        }),
    }
}

#[cfg(test)]
mod tests;
