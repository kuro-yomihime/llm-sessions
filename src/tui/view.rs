//! The view model: how a flat session list becomes panes.
//!
//! Session grouping and tree construction, independent of App and drawing.
//! [`groups`] ranks the left pane by activity and recency; [`members`] picks
//! rows under its selected group, and [`tree`] arranges forks with folds.
//! Shared CLI/TUI visibility rules live in [`crate::catalog::query`].

use std::collections::HashSet;
use std::path::Path;

use time::OffsetDateTime;

use crate::catalog::query::{Filters, eligible};
use crate::catalog::workspace;
use crate::model::{Change, SessionKey, SessionKeyRef, SessionSummary, SourceId, Usage};
use crate::runtime::live::Fleet;

/// Half-life of the workspace recency score, in days. Raw counts let `/tmp`
/// (105 sessions) outrank the project worked on this morning; decay fixes the
/// ordering without hiding anything.
const SCORE_HALF_LIFE_DAYS: f64 = 7.0;

/// Primary axis of the left pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grouping {
    /// Projects, sessions merged across agents. The point of the tool.
    Workspace,
    /// The familiar per-tool view.
    Agent,
    /// Repository broken down by git branch. A stack of worktrees correctly
    /// rolls up into one repository row, which then hides the branch that
    /// distinguishes them; this puts it back.
    Branch,
    /// One row per running agent process: who needs me, right now.
    Fleet,
    /// One row per machine, local sessions under `local`. The fleet-wide
    /// view that answers "where is this running".
    Host,
    /// Flat: every session everywhere, newest first.
    Timeline,
}

impl Grouping {
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Workspace => Self::Agent,
            Self::Agent => Self::Branch,
            Self::Branch => Self::Fleet,
            Self::Fleet => Self::Host,
            Self::Host => Self::Timeline,
            Self::Timeline => Self::Workspace,
        }
    }

    #[must_use]
    pub const fn prev(self) -> Self {
        match self {
            Self::Workspace => Self::Timeline,
            Self::Agent => Self::Workspace,
            Self::Branch => Self::Agent,
            Self::Fleet => Self::Branch,
            Self::Host => Self::Fleet,
            Self::Timeline => Self::Host,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Agent => "agent",
            Self::Branch => "branch",
            Self::Fleet => "fleet",
            Self::Host => "host",
            Self::Timeline => "timeline",
        }
    }

    /// Inverse of [`Grouping::label`], for the bookmark file. Unknown
    /// spellings degrade to the default view rather than failing the jump.
    pub fn from_label(label: &str) -> Self {
        match label {
            "agent" => Self::Agent,
            "branch" => Self::Branch,
            "fleet" => Self::Fleet,
            "host" => Self::Host,
            "timeline" => Self::Timeline,
            _ => Self::Workspace,
        }
    }

    /// The view a digit selects. `]`/`[` still cycle; these jump.
    pub const fn for_key(key: char) -> Option<Self> {
        match key {
            '1' => Some(Self::Workspace),
            '2' => Some(Self::Agent),
            '3' => Some(Self::Branch),
            '4' => Some(Self::Fleet),
            '5' => Some(Self::Host),
            '6' => Some(Self::Timeline),
            _ => None,
        }
    }

    /// Digit that selects this view, for the help and the breadcrumb.
    pub const fn key(self) -> char {
        match self {
            Self::Workspace => '1',
            Self::Agent => '2',
            Self::Branch => '3',
            Self::Fleet => '4',
            Self::Host => '5',
            Self::Timeline => '6',
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Groups,
    Sessions,
    Transcript,
}

impl Pane {
    /// Pane as one storable letter; [`Pane::from_letter`] reads it back.
    pub const fn letter(self) -> char {
        match self {
            Self::Groups => 'g',
            Self::Sessions => 's',
            Self::Transcript => 't',
        }
    }

    /// Inverse of [`Pane::letter`]. Unknown letters degrade to the groups
    /// pane rather than failing the jump.
    pub const fn from_letter(letter: char) -> Self {
        match letter {
            't' => Self::Transcript,
            's' => Self::Sessions,
            _ => Self::Groups,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Group {
    pub label: String,
    /// `None` is the synthetic "everything" group used by timeline mode.
    pub key: Option<String>,
    pub count: usize,
    pub running: usize,
    pub agents: Vec<SourceId>,
    pub updated: OffsetDateTime,
    /// Recency-weighted activity; the left pane's sort key.
    pub score: f64,
    pub usage: Option<Usage>,
    pub change: Option<Change>,
}

/// One row of the session pane, already placed in its fork tree.
///
/// The two tree flags are decided while the tree is built, so drawing a
/// frame never walks the parent map or allocates a key to ask "does this
/// have children, and are they showing?".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    /// Index into the full session list.
    pub index: usize,
    /// Nesting under a parent session; 0 is a root.
    pub depth: u8,
    /// At least one session in this view hangs off this one.
    pub children: bool,
    /// Has children, and `F` is currently hiding them.
    pub folded: bool,
}

/// Origin-qualified row identity shared with discovery and transcript caches.
pub fn owned_key(session: &SessionSummary) -> SessionKey {
    session.identity()
}

/// Sessions eligible for the current view.
///
/// A subagent transcript is only ever a child row. If its parent is not
/// itself eligible, the child is dropped rather than promoted — listing
/// one as a session was a real bug, and a filter must not resurrect it.
pub fn candidates<'a>(
    sessions: &'a [SessionSummary],
    filters: &Filters<'_>,
) -> impl Iterator<Item = (usize, &'a SessionSummary)> {
    let visible_parents: HashSet<SessionKeyRef<'_>> = sessions
        .iter()
        .filter(|s| !s.subagent && eligible(s, filters))
        .map(SessionSummary::key)
        .collect();
    sessions.iter().enumerate().filter(move |(_, s)| {
        if s.subagent {
            return s.parent.as_deref().is_some_and(|id| {
                visible_parents.contains(&SessionKeyRef {
                    origin_id: s.origin_id.as_deref(),
                    source: s.source,
                    id,
                })
            });
        }
        eligible(s, filters)
    })
}

/// The process a row belongs to in fleet mode: its own, else the one
/// running the ancestor it was spawned from.
///
/// Bounded: a corrupt store could record a parent cycle, and a view must
/// not hang on one.
fn fleet_pid(session: &SessionSummary, sessions: &[SessionSummary]) -> Option<u32> {
    let mut current = session;
    for _ in 0..8 {
        if let Some(pid) = current.liveness.pid() {
            return Some(pid);
        }
        // Only a spawned subagent belongs to its parent's process. A fork
        // is a separate conversation that merely shares an ancestor, and
        // pulling old forks into a live bucket would answer a question
        // nobody asked.
        if !current.subagent {
            return None;
        }
        let parent = current.parent.as_deref()?;
        let parent_key = SessionKeyRef {
            origin_id: current.origin_id.as_deref(),
            source: current.source,
            id: parent,
        };
        current = sessions.iter().find(|s| s.key() == parent_key)?;
    }
    None
}

pub fn group_key(
    session: &SessionSummary,
    grouping: Grouping,
    sessions: &[SessionSummary],
) -> Option<String> {
    // Canonical origins qualify groups; display labels are not identities.
    let origin = session.origin_id.clone().unwrap_or_else(|| {
        if session.is_local() {
            "local".into()
        } else {
            format!("unknown:{:?}", session.routes)
        }
    });
    match grouping {
        Grouping::Timeline => None,
        Grouping::Agent => Some(session.source.name().to_string()),
        Grouping::Host => Some(origin),
        // A repository plus its branch: the worktree rollup is right, but
        // it hides the only thing that distinguishes a stack of them.
        Grouping::Branch => Some(format!(
            "{origin}\u{1f}{}\u{1f}{}",
            session
                .workspace
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            session.branch.as_deref().unwrap_or("")
        )),
        // One bucket per attached process. A subagent belongs to the
        // process running its parent even though no fd points at it: the
        // whole point of the view is "what is this instance doing", and
        // that includes the children it already finished. A remote pid is
        // prefixed with its host, because two machines each have a pid 42.
        Grouping::Fleet => Some(format!("{origin}:{}", fleet_pid(session, sessions)?)),
        Grouping::Workspace => Some(format!(
            "{origin}\u{1f}{}",
            session
                .workspace
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        )),
    }
}

fn group_label(
    session: &SessionSummary,
    grouping: Grouping,
    key: Option<&str>,
    home: &Path,
    fleet: &Fleet,
) -> String {
    match (grouping, key) {
        (Grouping::Timeline, _) | (_, None) => "all sessions".to_string(),
        (Grouping::Agent, Some(name)) => name.to_string(),
        (Grouping::Host, Some(_)) => session
            .origin_label
            .clone()
            .unwrap_or_else(|| "local".into()),
        (Grouping::Fleet, Some(pid)) => {
            let pid = pid.rsplit_once(':').map_or(pid, |(_, pid)| pid);
            let origin = session.origin_label.as_deref().unwrap_or("");
            let separator = if session.origin_label.is_some() {
                ":"
            } else {
                ""
            };
            let place = session
                .workspace
                .as_deref()
                .map_or_else(|| "?".into(), |p| workspace::display(p, home));
            // A pane address is this machine's tmux state; a remote row
            // must not claim an unrelated local pane that shares the pid.
            let pane = if session.is_local() {
                session
                    .liveness
                    .pid()
                    .and_then(|pid| fleet.get(pid))
                    .and_then(|process| process.pane.as_ref())
                    .map(|pane| format!(" [{}]", pane.address))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            // The pid is what distinguishes two instances of the same
            // agent in the same project, so it leads: this is the
            // narrowest pane, and the tail is what truncation eats.
            format!(
                "{} pid {origin}{separator}{pid} · {place}{pane}",
                session.source.badge().trim_end()
            )
        }
        (Grouping::Branch, Some(_)) => {
            let place = session
                .workspace
                .as_deref()
                .map_or_else(|| "(unattributed)".into(), |p| workspace::display(p, home));
            let place = match &session.origin_label {
                Some(host) => format!("{place} · {host}"),
                None => place,
            };
            match session.branch.as_deref() {
                Some(branch) => format!("{place} @ {branch}"),
                None => place,
            }
        }
        // Identity keys stay separate from the human-readable origin label.
        (Grouping::Workspace, Some(_)) => {
            let place = session.workspace.as_deref().map_or_else(
                || "(unattributed)".to_string(),
                |path| workspace::display(path, home),
            );
            match session.origin_label.as_deref() {
                None => place,
                Some(host) => format!("{place} · {host}"),
            }
        }
    }
}

/// The left pane: one group per key, sorted live-first then by recency.
pub fn groups(
    sessions: &[SessionSummary],
    grouping: Grouping,
    home: &Path,
    fleet: &Fleet,
    filters: &Filters<'_>,
) -> Vec<Group> {
    let now = OffsetDateTime::now_utc();
    let mut buckets: std::collections::BTreeMap<Option<String>, Group> =
        std::collections::BTreeMap::new();
    for (_, session) in candidates(sessions, filters) {
        let key = group_key(session, grouping, sessions);
        // Fleet mode is about processes; a session with none has no row.
        if grouping == Grouping::Fleet && key.is_none() {
            continue;
        }
        let label = group_label(session, grouping, key.as_deref(), home, fleet);
        let entry = buckets.entry(key.clone()).or_insert_with(|| Group {
            label,
            key,
            count: 0,
            running: 0,
            agents: Vec::new(),
            updated: OffsetDateTime::UNIX_EPOCH,
            score: 0.0,
            usage: None,
            change: None,
        });
        entry.count += 1;
        if session.liveness.is_running() {
            entry.running += 1;
        }
        entry.updated = entry.updated.max(session.updated);
        entry.score += recency_weight(session.updated, now);
        if let Some(usage) = session.usage {
            let total = entry.usage.get_or_insert(Usage::default());
            total.input += usage.input;
            total.output += usage.output;
            total.cost += usage.cost;
        }
        if let Some(change) = session.change {
            entry.change.get_or_insert(Change::default()).merge(change);
        }
        if !entry.agents.contains(&session.source) {
            entry.agents.push(session.source);
        }
    }

    let mut groups: Vec<Group> = buckets.into_values().collect();
    for group in &mut groups {
        group.agents.sort_unstable();
    }
    // A workspace with something running outranks any amount of history.
    groups.sort_by(|a, b| {
        (b.running > 0)
            .cmp(&(a.running > 0))
            .then_with(|| b.score.total_cmp(&a.score))
            .then_with(|| b.updated.cmp(&a.updated))
    });
    groups
}

/// Rows of the selected group (timeline: everything), as session indexes.
pub fn members(
    sessions: &[SessionSummary],
    grouping: Grouping,
    filters: &Filters<'_>,
    selected: Option<&str>,
) -> Vec<usize> {
    candidates(sessions, filters)
        .filter(|(_, s)| match (selected, grouping) {
            (_, Grouping::Timeline) | (None, _) => true,
            (Some(key), _) => group_key(s, grouping, sessions).as_deref() == Some(key),
        })
        .map(|(i, _)| i)
        .collect()
}

/// Order rows as a fork tree: a child sits under its parent, indented.
///
/// Forks and subagent branches are already recorded (`parent_id`,
/// `parentSession`, `parentUuid`) and were previously thrown away, which
/// is why one project could show forty flat rows for three real
/// conversations. A parent outside the current filter is treated as
/// absent, so filtering never hides a row behind an invisible ancestor —
/// a promoted child is a depth-0 root and no fold applies to it, because
/// the fold that could hide it belongs to a session this view does not
/// have.
pub fn tree(
    sessions: &[SessionSummary],
    members: &[usize],
    collapsed: &HashSet<SessionKey>,
) -> Vec<Row> {
    let present: std::collections::HashMap<SessionKeyRef<'_>, usize> =
        members.iter().map(|&i| (sessions[i].key(), i)).collect();

    let mut children: std::collections::HashMap<usize, Vec<usize>> =
        std::collections::HashMap::new();
    let mut roots = Vec::new();
    for &index in members {
        let session = &sessions[index];
        let parent = session
            .parent
            .as_deref()
            .and_then(|id| {
                present
                    .get(&SessionKeyRef {
                        origin_id: session.origin_id.as_deref(),
                        source: session.source,
                        id,
                    })
                    .copied()
            })
            .filter(|&parent| parent != index);
        match parent {
            Some(parent) => children.entry(parent).or_default().push(index),
            None => roots.push(index),
        }
    }

    let mut rows = Vec::with_capacity(members.len());
    let mut seen = HashSet::new();
    for root in roots {
        push_subtree(
            sessions, root, 0, &children, &mut rows, &mut seen, collapsed,
        );
    }
    // A parent cycle would strand its members; list them flat rather than
    // dropping rows the user can see in the count.
    for &index in members {
        if seen.insert(index) {
            rows.push(Row {
                index,
                depth: 0,
                children: false,
                folded: false,
            });
        }
    }
    rows
}

fn push_subtree(
    sessions: &[SessionSummary],
    index: usize,
    depth: u8,
    children: &std::collections::HashMap<usize, Vec<usize>>,
    out: &mut Vec<Row>,
    seen: &mut HashSet<usize>,
    collapsed: &HashSet<SessionKey>,
) {
    if !seen.insert(index) {
        return;
    }
    let Some(kids) = children.get(&index) else {
        out.push(Row {
            index,
            depth,
            children: false,
            folded: false,
        });
        return;
    };
    // Only a parent can be folded, so only a parent pays for the key.
    let folded = collapsed.contains(&owned_key(&sessions[index]));
    out.push(Row {
        index,
        depth,
        children: true,
        folded,
    });
    // A folded branch is hidden, not forgotten: its members still count
    // as placed, or the cycle sweep below would re-list every one of
    // them flat at depth 0.
    if folded {
        for &kid in kids {
            mark_placed(kid, children, seen);
        }
        return;
    }
    let mut kids = kids.clone();
    kids.sort_by_key(|&i| std::cmp::Reverse(sessions[i].updated));
    for kid in kids {
        push_subtree(
            sessions,
            kid,
            depth.saturating_add(1),
            children,
            out,
            seen,
            collapsed,
        );
    }
}

/// Walk a hidden branch, marking it placed without emitting a row.
fn mark_placed(
    index: usize,
    children: &std::collections::HashMap<usize, Vec<usize>>,
    seen: &mut HashSet<usize>,
) {
    if !seen.insert(index) {
        return;
    }
    let Some(kids) = children.get(&index) else {
        return;
    };
    for &kid in kids {
        mark_placed(kid, children, seen);
    }
}

/// Exponential decay with a [`SCORE_HALF_LIFE_DAYS`] half-life.
fn recency_weight(updated: OffsetDateTime, now: OffsetDateTime) -> f64 {
    let days = (now - updated).as_seconds_f64() / 86_400.0;
    if days <= 0.0 {
        return 1.0;
    }
    0.5f64.powf(days / SCORE_HALF_LIFE_DAYS)
}
