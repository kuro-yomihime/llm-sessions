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
//! empty list. Configured remote hosts join the same fan-out, one thread
//! each, so a machine that has gone away cannot delay the local listing
//! either.

use std::collections::HashSet;
use std::thread;

use time::OffsetDateTime;

use crate::catalog::workspace::Resolver;
use crate::model::{SessionSummary, SourceId};
use crate::runtime::live::Fleet;
use crate::source::Source;
use crate::source::remote;
use crate::storage::overlay::Overlay;

#[derive(Debug, Default)]
pub struct Scan {
    pub sessions: Vec<SessionSummary>,
    /// What failed, ready to print: every message names its own origin —
    /// the source id, the store file, or the machine. Never fatal.
    pub failures: Vec<String>,
}

/// `overlay` supplies archive state for agents that have no such concept of
/// their own; without it those sessions simply never look archived.
///
/// `fleet` attributes running processes to sessions. Without it every session
/// falls back to mtime-based liveness, which is still correct, just coarser.
///
/// `hosts` are machines reached over SSH. Their rows arrive already keyed to
/// the host that produced them, and the TUI passes none: it reads a
/// background cache instead of paying an `ssh` per two-second tick.
pub fn scan_with(
    sources: &[Box<dyn Source>],
    overlay: Option<&Overlay>,
    fleet: Option<&Fleet>,
    hosts: &[remote::Host],
) -> Scan {
    let (results, remotes) = thread::scope(|scope| {
        let handles: Vec<_> = sources
            .iter()
            .map(|src| (src.id(), scope.spawn(move || src.list())))
            .collect();
        let reaches: Vec<_> = hosts
            .iter()
            .map(|host| (host.name(), scope.spawn(move || host.list(sources))))
            .collect();
        // A panicking adapter is a failed source, not an absent one: the
        // contract is a warning line, never a silently shorter list.
        let results: Vec<_> = handles
            .into_iter()
            .map(|(id, handle)| (id, handle.join().map_err(|_| "reader panicked".to_owned())))
            .collect();
        let remotes: Vec<_> = reaches
            .into_iter()
            .map(|(name, handle)| {
                handle
                    .join()
                    .map_err(|_| format!("{name}: remote listing panicked"))
            })
            .collect();
        (results, remotes)
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
    // A host-wide failure (probe, ssh) has no source of its own; its message
    // names the machine instead.
    for result in remotes {
        match result {
            Ok(Ok(mut scan)) => {
                sessions.append(&mut scan.sessions);
                failures.append(&mut scan.failures);
            }
            Ok(Err(e)) => failures.push(e.to_string()),
            Err(panic) => failures.push(panic),
        }
    }

    if let Some(overlay) = overlay {
        match overlay.archived() {
            Ok(rows) => {
                let archived: HashSet<(SourceId, String)> = rows.into_iter().collect();
                for session in &mut sessions {
                    if archived.contains(&(session.source, session.id.clone())) {
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
        // unrelated local directory of the same name. A host that ran its own
        // `llm-sessions` already sent a resolved workspace; the shell transport can
        // only offer what the agent recorded.
        if session.host.is_some() {
            if session.workspace.is_none() {
                session.workspace = session.recorded_cwd.clone();
            }
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
    let present: HashSet<(SourceId, String)> = sessions
        .iter()
        .filter(|session| !session.subagent)
        .map(|session| (session.source, session.id.clone()))
        .collect();
    for session in &mut sessions {
        let orphan = session.subagent
            && session
                .parent
                .as_ref()
                .is_none_or(|id| !present.contains(&(session.source, id.clone())));
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
            if sessions[index].host.is_none() {
                sessions.swap(local, index);
                local += 1;
            }
        }
        fleet.attribute(&mut sessions[..local], OffsetDateTime::now_utc());
    }

    // Running first: an attached agent is what you came to look at.
    sessions.sort_unstable_by(|a, b| {
        a.liveness
            .rank()
            .cmp(&b.liveness.rank())
            .then_with(|| b.updated.cmp(&a.updated))
            .then_with(|| a.source.cmp(&b.source))
            .then_with(|| a.id.cmp(&b.id))
    });
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
            host: None,
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
        let scan = scan_with(&sources, None, None, &[]);
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
        let scan = scan_with(&sources, None, None, &[]);
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
        far.host = Some("nx-1".into());
        far.recorded_cwd = Some(PathBuf::from("/var/lib/hermes/work"));
        far.liveness = Liveness::Live { pid: 9 };

        let sources: Vec<Box<dyn Source>> = vec![Box::new(Fake(vec![local]))];
        let hosts = vec![crate::source::remote::Host::fake("nx-1", vec![far])];
        let scan = scan_with(&sources, None, Some(&Fleet::default()), &hosts);

        let of = |id: &str| {
            scan.sessions
                .iter()
                .find(|session| session.id == id)
                .expect(id)
        };
        assert_eq!(
            of("far").workspace.as_deref(),
            Some(Path::new("/var/lib/hermes/work")),
            "a remote row keeps what its agent recorded"
        );
        assert_eq!(of("far").liveness, Liveness::Live { pid: 9 });
        assert_eq!(of("far").host.as_deref(), Some("nx-1"));
        assert_eq!(
            of("local").workspace,
            Some(Resolver::new().resolve(dir.path())),
            "and a local row in the same scan is still resolved"
        );
    }
}
