//! What one scan needs, assembled once, and the lookups every frontend runs
//! against its result.
//!
//! The CLI, the TUI and `serve` all list the same rows from the same inputs:
//! every local adapter, the archive overlay, the configured remote hosts.
//! A [`Loader`] owns those inputs, so a one-shot command builds it, scans
//! once and exits, while a long-running server keeps it and rescans on a
//! timer without rebuilding anything.

use crate::federation::DiscoveryContext;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::thread;

use crate::catalog::query::Query;
use crate::catalog::scan::{Scan, scan_with};
use crate::error::{Error, Result};
use crate::model::{SessionSummary, SourceId, Transcript};
use crate::runtime::live::Fleet;
use crate::source::{self, Source, remote};
use crate::storage::index::Index;
use crate::storage::overlay::Overlay;

/// Whether a scan may fill blank tier-1 usage from the index cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cached {
    /// Presentation: show the best number available.
    Borrow,
    /// Index refresh: the cache is the output, so reading it back in would
    /// make a stale value immortal. Codex's corrected token split survived
    /// three full rebuilds this way before the loop was cut.
    Ignore,
}

/// Whether this scan waits on SSH or lists this machine only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Remote {
    /// CLI: one blocking fetch per host, rows inline. A hung host costs at
    /// most its connect timeout, then becomes a failure line.
    Inline,
    /// Local rows only. The TUI's `remote::Cache` refreshes hosts
    /// off-thread so no frame ever blocks on a socket; `serve` publishes
    /// this machine's sessions and nothing it reaches over the network.
    Skip,
}

/// The inputs of a scan: adapters, archive state, remote hosts.
pub struct Loader {
    home: PathBuf,
    /// Every local adapter, for listing and for opening what a scan listed.
    pub sources: Vec<Box<dyn Source>>,
    /// Local archive state, when the state directory is usable.
    pub overlay: Option<Overlay>,
    /// Every configured remote host, whether or not a scan contacts it.
    pub hosts: Vec<remote::Host>,
    /// Why `hosts.json` could not be read, repeated in every scan's failures.
    config_failure: Option<String>,
}

impl Loader {
    pub fn new(home: &Path) -> Self {
        let (config, config_failure) = remote::Config::load(home);
        Self {
            home: home.to_path_buf(),
            sources: source::all(home),
            // The overlay is optional: a read-only or unwritable state
            // directory must degrade to "no archive state", never to a
            // failed launch.
            overlay: Overlay::open(&Overlay::default_path(home)).ok(),
            hosts: config.hosts.into_iter().map(remote::Host::new).collect(),
            config_failure,
        }
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    /// One tier-1 listing.
    ///
    /// `fleet` attributes running processes to sessions; `None` falls back
    /// to mtime-based liveness, which costs nothing.
    pub fn scan(&self, fleet: Option<&Fleet>, cached: Cached, remote: Remote) -> Scan {
        let hosts: &[remote::Host] = match remote {
            Remote::Inline => &self.hosts,
            Remote::Skip => &[],
        };
        let mut scan = scan_with(&self.sources, self.overlay.as_ref(), fleet, &[]);
        match crate::federation::read_origin(&self.home) {
            Ok(origin) => extend_from_origin(
                &mut scan,
                &self.sources,
                hosts,
                origin,
                Vec::new(),
                crate::federation::DEFAULT_HOPS,
            ),
            Err(error) => scan.failures.push(format!(
                "origin identity unavailable; federation disabled: {error}"
            )),
        }
        scan.sessions = crate::catalog::merge::merge(scan.sessions);
        if let Some(failure) = &self.config_failure {
            scan.failures.push(failure.clone());
        }
        if cached == Cached::Borrow {
            borrow_cached_usage(&self.home, &mut scan);
        }
        scan
    }

    /// Open a session this loader's scan listed.
    pub fn open(&self, session: &SessionSummary) -> Result<Transcript> {
        open(&self.home, &self.sources, session)
    }
}

/// OMP and Claude only record spend inside the transcript, so tier 1 cannot
/// price them. `llm-sessions index` already parsed those bodies; borrow its answer
/// rather than reparsing 280 MB to fill a column.
pub fn borrow_cached_usage(home: &Path, scan: &mut Scan) {
    let Ok(index) = Index::open(&Index::default_path(home)) else {
        return;
    };
    let Ok(cached) = index.usage() else { return };
    cached.fill(&mut scan.sessions);
}

/// Open a session through its local adapter or preferred access route, with
/// the scan's title and workspace.
///
/// Only a [`SessionSummary`] a scan produced can be opened: its locator is
/// the adapter's own, never built from a caller's input.
pub fn open(
    home: &Path,
    sources: &[Box<dyn Source>],
    session: &SessionSummary,
) -> Result<Transcript> {
    let mut transcript = if session.is_local() {
        let reader = source::find(sources, session.source).ok_or(Error::Unsupported {
            agent: session.source,
            what: "reading transcripts",
        })?;
        reader.open(&session.locator)?
    } else {
        let hop = session
            .preferred_route()
            .and_then(|route| route.hops.first())
            .ok_or_else(|| Error::Remote {
                host: session
                    .origin_label
                    .clone()
                    .unwrap_or_else(|| "remote".into()),
                message: "session has no access route".into(),
            })?;
        remote::Host::from_route_hop(hop, home).open(session, home)?
    };
    // Tier 1 already resolved title and workspace authoritatively (Codex
    // titles live in `state_5.sqlite`, not in the rollout; the scan also
    // canonicalized the path and rolled worktrees up). Prefer them.
    transcript.title = transcript.title.take().or_else(|| session.title.clone());
    transcript.workspace = session
        .workspace
        .clone()
        .or_else(|| transcript.workspace.take());
    Ok(transcript)
}

/// What an id, or a prefix of one, names in a listing.
#[derive(Debug)]
pub enum Resolved<'a> {
    One(&'a SessionSummary),
    Missing,
    /// Every candidate, in listing order.
    Ambiguous(Vec<&'a SessionSummary>),
}

/// Resolve an id or unique prefix. An exact id wins over longer ids it
/// prefixes; ids repeated across agents are ambiguous.
pub fn resolve<'a>(sessions: &'a [SessionSummary], needle: &str) -> Resolved<'a> {
    let exact: Vec<&SessionSummary> = sessions.iter().filter(|s| s.id == needle).collect();
    let mut candidates = if exact.is_empty() {
        sessions
            .iter()
            .filter(|s| s.id.starts_with(needle))
            .collect::<Vec<_>>()
    } else {
        exact
    };
    match candidates.len() {
        0 => Resolved::Missing,
        1 => Resolved::One(candidates.remove(0)),
        _ => Resolved::Ambiguous(candidates),
    }
}

/// Sessions matching the query's `body:` terms, or `None` when it has none.
///
/// The parser is shared by every frontend, so `body:` has to mean the same
/// thing everywhere; it is answered by the index and fails without one.
pub fn body_hits(home: &Path, query: &Query) -> Option<Result<HashSet<(SourceId, String)>>> {
    if !query.needs_index() {
        return None;
    }
    let path = Index::default_path(home);
    // A read must not build the index: `Index::open` would happily create an
    // empty one and the caller could not tell "no matches" from "no index".
    if !path.is_file() {
        return Some(Err(Error::Io {
            path,
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no index"),
        }));
    }
    let expression = query.body_query()?;
    Some(
        Index::open(&path)
            .and_then(|index| index.search(&expression, 5000))
            .map(|hits| hits.into_iter().map(|h| (h.source, h.session_id)).collect()),
    )
}

/// Merge independent branches. A failure on one route never suppresses another.
fn extend_scan(
    scan: &mut Scan,
    sources: &[Box<dyn Source>],
    hosts: &[remote::Host],
    context: &DiscoveryContext,
) {
    if context.remaining_hops == 0 {
        if !hosts.is_empty() {
            scan.failures
                .push("federation discovery truncated at hop limit".into());
        }
        return;
    }
    let results = thread::scope(|scope| {
        let handles: Vec<_> = hosts
            .iter()
            .map(|host| {
                (
                    host.name(),
                    scope.spawn(move || host.list_with(sources, context)),
                )
            })
            .collect();
        handles
            .into_iter()
            .map(|(name, handle)| {
                handle
                    .join()
                    .map_err(|_| format!("{name}: remote listing panicked"))
            })
            .collect::<Vec<_>>()
    });
    for result in results {
        match result {
            Ok(Ok(mut remote)) => {
                scan.sessions.append(&mut remote.sessions);
                scan.failures.append(&mut remote.failures);
            }
            Ok(Err(error)) => scan.failures.push(error.to_string()),
            Err(error) => scan.failures.push(error),
        }
    }
}

/// The shared discovery step of every guarded entry point: stamp the local
/// scan with this instance's origin, then extend it through the given peers
/// under the request's own ancestors and remaining depth. Both callers merge
/// the result themselves, so a federation request can borrow cached usage
/// between the two steps.
pub fn extend_from_origin(
    scan: &mut Scan,
    sources: &[Box<dyn Source>],
    hosts: &[remote::Host],
    origin: String,
    ancestors: Vec<String>,
    remaining_hops: u32,
) {
    for session in &mut scan.sessions {
        session.origin_id = Some(origin.clone());
    }
    let mut context = DiscoveryContext {
        visited: ancestors,
        remaining_hops,
    };
    context.visited.push(origin);
    extend_scan(scan, sources, hosts, &context);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use time::OffsetDateTime;

    use super::*;
    use crate::model::Locator;

    fn row(source: SourceId, id: &str) -> SessionSummary {
        SessionSummary::new(
            id.into(),
            source,
            OffsetDateTime::UNIX_EPOCH,
            Locator::File {
                path: PathBuf::from(format!("/s/{id}.jsonl")),
            },
        )
    }

    #[test]
    fn an_exact_id_wins_over_the_longer_ids_it_prefixes() {
        let rows = [
            row(SourceId::Omp, "abc"),
            row(SourceId::Omp, "abcdef"),
            row(SourceId::Claude, "abd"),
        ];
        assert!(matches!(resolve(&rows, "abc"), Resolved::One(s) if s.id == "abc"));
        assert!(matches!(resolve(&rows, "abcd"), Resolved::One(s) if s.id == "abcdef"));
        assert!(matches!(resolve(&rows, "ab"), Resolved::Ambiguous(c) if c.len() == 3));
        assert!(matches!(resolve(&rows, "x"), Resolved::Missing));
    }

    /// Ids are unique within an agent, not across agents.
    #[test]
    fn an_id_two_agents_share_is_ambiguous() {
        let rows = [row(SourceId::Omp, "same"), row(SourceId::Codex, "same")];
        assert!(matches!(resolve(&rows, "same"), Resolved::Ambiguous(c) if c.len() == 2));
    }
}
