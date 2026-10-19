//! Tier-1 fan-out.
//!
//! Measured on the author's machine: a full cross-agent listing costs ~0.5 ms
//! of `stat`, ~8 ms of bounded head reads for 224 OMP transcripts, and ~46 ms
//! for one indexed query over 1901 `OpenCode` sessions — 81 ms end to end across
//! nine agents. That is why there is no persistent index here, and why the TUI
//! can afford to rescan on a timer instead of trusting a launch-time snapshot.
//!
//! Sources run on separate threads so a slow or broken store cannot stall the
//! rest, and a failing source degrades to a reported error rather than an
//! empty list. Peers join the same listing through [`Scan::extend`] in the
//! discovery pipeline, one thread each there, so a machine that has gone
//! away cannot delay a local listing either.

use std::collections::HashSet;
use std::thread;

use time::OffsetDateTime;

use crate::catalog::workspace::Resolver;
use crate::model::{SessionKey, SessionSummary, SourceId};
use crate::runtime::live::Fleet;
use crate::source::Source;
use crate::storage::overlay::Overlay;

#[derive(Debug, Default)]
pub struct Scan {
    pub sessions: Vec<SessionSummary>,
    /// What failed, ready to print: every message names its own origin —
    /// the source id, the store file, or the machine. Never fatal.
    pub failures: Vec<String>,
}

impl Scan {
    /// Attribute this machine's own rows to its origin. `None` is a
    /// statement too — identity unavailable — so a local row is never
    /// left looking unstamped.
    pub fn attribute(&mut self, origin: Option<&str>) {
        for session in &mut self.sessions {
            if session.is_local() {
                session.origin_id = origin.map(str::to_owned);
            }
        }
    }

    /// Fold one independently observed branch into this scan: rows and
    /// failures both, so a failure on one route never suppresses another.
    /// A branch's rows keep the workspace and liveness their own origin
    /// established; nothing here inspects this machine's filesystem for
    /// them.
    pub fn extend(&mut self, mut branch: Scan) {
        self.sessions.append(&mut branch.sessions);
        self.failures.append(&mut branch.failures);
    }

    /// Contribute rows this observation already holds — a cache's
    /// snapshot, rows saved from an earlier poll — with no failures of
    /// their own to add.
    pub fn append(&mut self, mut sessions: Vec<SessionSummary>) {
        self.sessions.append(&mut sessions);
    }

    /// Assemble the whole observation: every contributed branch merged
    /// under canonical identity — routes combine, metadata comes from one
    /// complete, shortest-route observation — then put back in listing
    /// order. Merging sorts by identity; without the re-sort every
    /// frontend's "first N" would be grouped by origin and agent.
    pub fn assemble(&mut self) {
        self.sessions = crate::catalog::merge::merge(std::mem::take(&mut self.sessions));
        self.sessions.sort_unstable_by(listing_order);
    }
}

/// Running first — an attached agent is what you came to look at — then
/// most recently updated, then a total tie-break so the order is stable.
fn listing_order(a: &SessionSummary, b: &SessionSummary) -> std::cmp::Ordering {
    a.liveness
        .rank()
        .cmp(&b.liveness.rank())
        .then_with(|| b.updated.cmp(&a.updated))
        .then_with(|| a.source.cmp(&b.source))
        .then_with(|| a.id.cmp(&b.id))
        .then_with(|| a.origin_id.cmp(&b.origin_id))
        .then_with(|| a.preferred_route().cmp(&b.preferred_route()))
}

/// `overlay` supplies archive state for agents that have no such concept of
/// their own; without it those sessions simply never look archived.
///
/// `fleet` attributes running processes to sessions. Without it every session
/// falls back to mtime-based liveness, which is still correct, just coarser.
///
/// This is the local tier only: every row a source lists is this machine's
/// own. Peers join through the catalog's assembly, never here, so a remote
/// path is never resolved against a filesystem it did not come from.
pub fn scan_with(
    sources: &[Box<dyn Source>],
    overlay: Option<&Overlay>,
    fleet: Option<&Fleet>,
) -> Scan {
    // A panicking adapter is a failed source, not an absent one: the
    // contract is a warning line, never a silently shorter list.
    let results = thread::scope(|scope| {
        let handles: Vec<_> = sources
            .iter()
            .map(|src| (src.id(), scope.spawn(move || src.list())))
            .collect();
        handles
            .into_iter()
            .map(|(id, handle)| (id, handle.join().map_err(|_| "reader panicked".to_owned())))
            .collect::<Vec<_>>()
    });

    let mut sessions = Vec::new();
    let mut failures = Vec::new();
    for (id, result) in results {
        match result {
            Ok(Ok(mut found)) => sessions.append(&mut found),
            Ok(Err(e)) => failures.push(format!("{id}: {e}")),
            Err(panic) => failures.push(format!("{id}: {panic}")),
        }
    }

    if let Some(overlay) = overlay {
        match overlay.archived() {
            Ok(rows) => {
                let archived: HashSet<(SourceId, String)> = rows.into_iter().collect();
                for session in &mut sessions {
                    if session.is_local()
                        && archived.contains(&(session.source, session.id.clone()))
                    {
                        session.archived = true;
                    }
                }
            }
            Err(e) => failures.push(format!("overlay: {e}")),
        }
    }

    // Sequential: `Resolver` caches repo-root lookups, and every session in a
    // project hits the same ancestor chain.
    let mut resolver = Resolver::new();
    for session in &mut sessions {
        // A remote path must never be resolved here: `Resolver` stats the
        // local filesystem, so it would either fail or — worse — match an
        // unrelated local directory of the same name. Rows that arrive
        // already attributed keep whatever their own origin established.
        if !session.is_local() {
            continue;
        }
        session.workspace = session
            .recorded_cwd
            .as_deref()
            .map(|cwd| resolver.resolve(cwd));
    }

    // An orphan is a top-level row, not a hidden one.
    //
    // A subagent is normally a child of a listed session, and listing one as a
    // session of its own was a real bug. But newer OMP keeps some parent
    // conversations only in `agent.db` while still writing its subagent
    // artifacts to disk, so the child's parent file may not exist at all.
    // Measured here: a running `omp` held three `<Task>.jsonl` children whose
    // parent had no file, and the whole instance was therefore invisible in
    // every list — including the fleet view, which is about processes. A row
    // with no parent in the scan has nothing to nest under.
    let present: HashSet<SessionKey> = sessions
        .iter()
        .filter(|session| !session.subagent && (session.origin_id.is_some() || session.is_local()))
        .map(SessionSummary::identity)
        .collect();
    for session in &mut sessions {
        let orphan = session.subagent
            && (session.origin_id.is_none() && !session.is_local()
                || session.parent.as_ref().is_none_or(|id| {
                    !present.contains(&SessionKey {
                        origin_id: session.origin_id.clone(),
                        source: session.source,
                        id: id.clone(),
                    })
                }));
        if orphan {
            session.subagent = false;
            session.parent = None;
        }
    }

    // Attribution needs canonical workspaces, so it runs after resolution.
    //
    // Only local rows: `Fleet` reads this machine's `/proc`, and a remote row
    // already carries whatever liveness its own host established. Locals are
    // swapped to the front for the call; the final sort reorders everything
    // anyway.
    if let Some(fleet) = fleet {
        let mut local = 0;
        for index in 0..sessions.len() {
            if sessions[index].is_local() {
                sessions.swap(local, index);
                local += 1;
            }
        }
        fleet.attribute(&mut sessions[..local], OffsetDateTime::now_utc());
    }

    sessions.sort_unstable_by(listing_order);
    Scan { sessions, failures }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::error::Result;
    use crate::model::{Liveness, Locator, PathTrust, Transcript};

    struct Fake(Vec<SessionSummary>);

    impl Source for Fake {
        fn id(&self) -> SourceId {
            SourceId::Omp
        }
        fn roots(&self) -> Vec<PathBuf> {
            Vec::new()
        }
        fn list(&self) -> Result<Vec<SessionSummary>> {
            Ok(self.0.clone())
        }
        fn open(&self, _: &Locator) -> Result<Transcript> {
            Ok(Transcript::new("t", SourceId::Omp))
        }
    }

    fn row(id: &str, parent: Option<&str>, subagent: bool) -> SessionSummary {
        SessionSummary {
            id: id.into(),
            source: SourceId::Omp,
            workspace: None,
            recorded_cwd: None,
            trust: PathTrust::Recorded,
            title: Some(id.into()),
            created: None,
            updated: OffsetDateTime::from_unix_timestamp(1_000).unwrap(),
            model: None,
            parent: parent.map(str::to_owned),
            archived: false,
            usage: None,
            size_bytes: None,
            locator: Locator::File {
                path: PathBuf::from(format!("/s/{id}.jsonl")),
            },
            branch: None,
            subagent,
            change: None,
            liveness: Liveness::Stale,
            origin_label: None,
            origin_id: None,
            routes: Vec::new(),
        }
    }

    struct Panics;

    impl Source for Panics {
        fn id(&self) -> SourceId {
            SourceId::Codex
        }
        fn roots(&self) -> Vec<PathBuf> {
            Vec::new()
        }
        fn list(&self) -> Result<Vec<SessionSummary>> {
            panic!("format drifted")
        }
        fn open(&self, _: &Locator) -> Result<Transcript> {
            unreachable!()
        }
    }

    /// A reader bug must surface as a failure line naming its source while
    /// every other source still lists; it used to vanish without a trace.
    #[test]
    fn a_panicking_source_is_reported_and_the_rest_still_list() {
        let sources: Vec<Box<dyn Source>> = vec![
            Box::new(Panics),
            Box::new(Fake(vec![row("kept", None, false)])),
        ];
        let scan = scan_with(&sources, None, None);
        assert_eq!(scan.sessions.len(), 1);
        assert_eq!(scan.failures, ["codex: reader panicked"]);
    }

    /// Measured: newer OMP keeps a parent conversation only in `agent.db`
    /// while still writing its subagent artifacts to disk. The running
    /// instance was invisible everywhere, because a subagent is shown only
    /// under its parent and that parent had no row.
    #[test]
    fn a_subagent_with_no_parent_row_becomes_a_session_of_its_own() {
        let sources: Vec<Box<dyn Source>> = vec![Box::new(Fake(vec![
            row("parent", None, false),
            row("kept-child", Some("parent"), true),
            row("orphan", Some("vanished"), true),
        ]))];
        let scan = scan_with(&sources, None, None);
        let of = |id: &str| {
            scan.sessions
                .iter()
                .find(|session| session.id == id)
                .expect(id)
        };

        assert!(!of("orphan").subagent, "nothing to nest under");
        assert_eq!(of("orphan").parent, None);
        assert!(
            of("kept-child").subagent,
            "a child whose parent is listed stays a child"
        );
        assert_eq!(of("kept-child").parent.as_deref(), Some("parent"));
    }

    /// A remote path has no meaning on this machine: resolving it would
    /// either fail or match an unrelated local directory, and the liveness
    /// its own host established is the only trustworthy one.
    #[test]
    fn a_remote_row_keeps_its_recorded_path_and_liveness() {
        let dir = tempfile::tempdir().unwrap();
        let mut local = row("local", None, false);
        local.recorded_cwd = Some(dir.path().to_path_buf());

        let mut far = row("far", None, false);
        far.origin_id = Some("00000000-0000-4000-8000-0000000000aa".into());
        far.origin_label = Some("nx-1".into());
        far.recorded_cwd = Some(PathBuf::from("/var/lib/hermes/work"));
        far.liveness = Liveness::Live { pid: 9 };

        let sources: Vec<Box<dyn Source>> = vec![Box::new(Fake(vec![local]))];
        let mut scan = scan_with(&sources, None, Some(&Fleet::default()));
        scan.extend(Scan {
            sessions: vec![far],
            failures: Vec::new(),
        });
        scan.assemble();

        let of = |id: &str| {
            scan.sessions
                .iter()
                .find(|session| session.id == id)
                .expect(id)
        };
        assert_eq!(
            of("far").workspace,
            None,
            "a remote row is never resolved against this machine"
        );
        assert_eq!(
            of("far").recorded_cwd.as_deref(),
            Some(Path::new("/var/lib/hermes/work")),
            "a remote row keeps what its agent recorded"
        );
        assert_eq!(of("far").liveness, Liveness::Live { pid: 9 });
        assert_eq!(of("far").origin_label.as_deref(), Some("nx-1"));
        assert_eq!(
            of("local").workspace,
            Some(Resolver::new().resolve(dir.path())),
            "and a local row in the same scan is still resolved"
        );
    }

    #[test]
    fn assembly_lists_running_first_then_newest_across_agents_and_origins() {
        let at = |seconds| OffsetDateTime::from_unix_timestamp(seconds).unwrap();
        // Identity order (origin, source, id) is the opposite of listing
        // order here, so a merge that forgot to re-sort fails loudly.
        let mut old = row("a-old", None, false);
        old.source = SourceId::Codex;
        old.updated = at(1_000);
        let mut new = row("b-new", None, false);
        new.updated = at(3_000);
        let mut running = row("c-running", None, false);
        running.updated = at(500);
        running.liveness = Liveness::Live { pid: 1 };
        let mut far = row("d-far", None, false);
        far.origin_id = Some("00000000-0000-4000-8000-0000000000aa".into());
        far.updated = at(2_000);

        let mut scan = Scan {
            sessions: vec![old, new, running, far],
            failures: Vec::new(),
        };
        scan.assemble();
        let ids: Vec<_> = scan.sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["c-running", "b-new", "d-far", "a-old"]);
    }

    #[test]
    fn parent_relationships_require_the_same_origin_and_source() {
        let mut parent = row("parent", None, false);
        parent.origin_id = Some("origin-a".into());
        let mut wrong_origin = row("wrong-origin", Some("parent"), true);
        wrong_origin.origin_id = Some("origin-b".into());
        wrong_origin.origin_label = Some("beta".into());
        let mut wrong_source = row("wrong-source", Some("parent"), true);
        wrong_source.origin_id = parent.origin_id.clone();
        wrong_source.source = SourceId::Claude;
        let mut child = row("child", Some("parent"), true);
        child.origin_id = parent.origin_id.clone();
        child.origin_label = Some("forwarder".into());
        let sources: Vec<Box<dyn Source>> = vec![Box::new(Fake(vec![
            parent,
            wrong_origin,
            wrong_source,
            child,
        ]))];
        let scan = scan_with(&sources, None, None);
        let of = |id: &str| {
            scan.sessions
                .iter()
                .find(|session| session.id == id)
                .unwrap()
        };
        assert!(!of("wrong-origin").subagent);
        assert_eq!(of("wrong-origin").parent, None);
        assert!(!of("wrong-source").subagent);
        assert_eq!(of("wrong-source").parent, None);
        assert!(
            of("child").subagent,
            "route/display label is not parent identity"
        );
        assert_eq!(of("child").parent.as_deref(), Some("parent"));
    }

    #[test]
    fn unknown_remote_parents_cannot_capture_local_children_or_each_other() {
        let mut remote_parent = row("parent", None, false);
        remote_parent.origin_label = Some("beta".into());
        let local_child = row("local-child", Some("parent"), true);
        let mut remote_child = row("remote-child", Some("parent"), true);
        remote_child.origin_label = Some("beta".into());
        let sources: Vec<Box<dyn Source>> = vec![Box::new(Fake(vec![
            remote_parent,
            local_child,
            remote_child,
        ]))];
        let scan = scan_with(&sources, None, None);
        assert!(scan.sessions.iter().all(|session| !session.subagent));
        assert!(scan.sessions.iter().all(|session| session.parent.is_none()));
    }

    #[test]
    fn local_archive_overlay_does_not_leak_to_remote_identity_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let overlay = Overlay::open(&dir.path().join("overlay.db")).unwrap();
        overlay.set_archived(SourceId::Omp, "same", true).unwrap();
        let mut local = row("same", None, false);
        local.origin_id = Some("origin-a".into());
        let mut remote = local.clone();
        remote.origin_id = Some("origin-b".into());
        remote.origin_label = Some("beta".into());
        let mut route_only = local.clone();
        route_only.origin_id = Some("origin-c".into());
        route_only.routes.push(crate::model::AccessRoute {
            hops: vec![crate::model::RouteHop {
                origin_id: "origin-c".into(),
                name: "gamma".into(),
                ssh: "gamma".into(),
                url: None,
                token_env: None,
                home: None,
            }],
        });
        let sources: Vec<Box<dyn Source>> = vec![Box::new(Fake(vec![local, remote, route_only]))];
        let scan = scan_with(&sources, Some(&overlay), None);
        assert_eq!(scan.sessions.len(), 3);
        for session in scan.sessions {
            assert_eq!(
                session.archived,
                session.origin_id.as_deref() == Some("origin-a")
            );
        }
    }
}
