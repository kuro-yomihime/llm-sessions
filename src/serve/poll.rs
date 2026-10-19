//! The poller: one scan per tick, status for the watched sessions, one
//! immutable [`Snapshot`] published to every reader.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::Serialize;
use time::OffsetDateTime;
use tokio::sync::watch;

use crate::catalog::loader::{Cached, Loader, Remote, body_hits};
use crate::catalog::query::Query;
use crate::error::Result;
use crate::model::{SessionKey, SessionSummary, SourceId, Transcript};
use crate::runtime::live::{Activity, Fleet};
use crate::runtime::status::{
    Facts, Settled, Status, WaitingReason, analyze, derive_status, settle,
};
use crate::source::remote;

/// A session, as the server keys it: full canonical identity, so a local row
/// and a federated row that share an agent and id never share a status, a
/// facts entry or a cached transcript.
pub(super) type Key = SessionKey;

fn key(session: &SessionSummary) -> Key {
    session.identity()
}

pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The `status` object of a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct StatusBlock {
    pub status: Status,
    pub waiting_reason: Option<WaitingReason>,
    #[serde(with = "time::serde::rfc3339")]
    pub status_since: OffsetDateTime,
}

impl From<Settled> for StatusBlock {
    fn from(settled: Settled) -> Self {
        Self {
            status: settled.status,
            waiting_reason: settled.reason,
            status_since: settled.since,
        }
    }
}

/// A [`SessionSummary`] exactly as `list --json` prints it, plus `status`.
///
/// Additive on purpose: serde ignores unknown fields, so a client that
/// parses `list --json` output parses these rows unchanged.
#[derive(Serialize)]
pub(super) struct Row<'a> {
    #[serde(flatten)]
    pub summary: &'a SessionSummary,
    pub status: Option<StatusBlock>,
}

/// One tick's result. Never mutated after publishing.
#[derive(Debug, Default)]
pub(super) struct Snapshot {
    /// Every row this server may publish: the scan after `--source` and the
    /// base query, in scan order (running first, then newest). Subagent and
    /// archived rows stay here for id resolution; listings filter them.
    pub sessions: Vec<SessionSummary>,
    /// Debounced status of each watched session.
    pub statuses: HashMap<Key, StatusBlock>,
    /// The watched rows as a JSON array: the stream's payload, and what
    /// decides whether a stream subscriber is woken at all.
    pub watched: String,
    /// Configured peer names in gateway mode, as of this tick; empty for a
    /// local-only server, which never publishes a peer's rows.
    pub peers: Vec<String>,
}

impl Snapshot {
    pub fn row<'a>(&self, session: &'a SessionSummary) -> Row<'a> {
        Row {
            summary: session,
            status: self.statuses.get(&key(session)).copied(),
        }
    }
}

/// Replace the published snapshot. Stream subscribers are woken only when
/// the watched rows changed; a listing reads the newest scan either way.
pub(super) fn publish(tx: &watch::Sender<Arc<Snapshot>>, snapshot: Snapshot) {
    tx.send_if_modified(|current| {
        let changed = current.watched != snapshot.watched;
        *current = Arc::new(snapshot);
        changed
    });
}

/// Transcript facts per session, valid while its `updated` is unchanged.
///
/// Shared by the poller and the detail route, so a session read once is
/// not reread until it is written again. Bounded by the session count: the
/// poller drops keys its scan no longer lists.
#[derive(Default)]
pub(super) struct FactsCache(Mutex<HashMap<Key, (OffsetDateTime, Facts)>>);

impl FactsCache {
    /// Cached facts, or `open` and analyze. A transcript that cannot be
    /// read has no facts: it reads as done, never as failed.
    pub fn get(
        &self,
        session: &SessionSummary,
        open: impl FnOnce() -> Result<Transcript>,
    ) -> std::result::Result<Facts, String> {
        let key = key(session);
        if let Some(&(updated, facts)) = lock(&self.0).get(&key)
            && updated == session.updated
        {
            return Ok(facts);
        }
        // Opened outside the lock: a large transcript must not stall
        // every other reader.
        let facts = open().map(|transcript| analyze(&transcript));
        let facts = facts.map_err(|e| e.to_string())?;
        lock(&self.0).insert(key, (session.updated, facts));
        Ok(facts)
    }

    fn retain(&self, keep: &HashSet<Key>) {
        lock(&self.0).retain(|key, _| keep.contains(key));
    }
}

/// Derive a status for a session the poller does not watch, on request.
/// Nothing to debounce against: `since` is its last write.
pub(super) fn status_now(live: bool, facts: Facts, session: &SessionSummary) -> StatusBlock {
    let (status, reason) = derive_status(
        live,
        Activity::Unknown,
        &facts,
        session.updated,
        OffsetDateTime::now_utc(),
    );
    settle(None, status, reason, session.updated, session.updated).into()
}

pub(super) struct Poller {
    loader: Loader,
    fleet: Fleet,
    base: Arc<Query>,
    window: Duration,
    facts: Arc<FactsCache>,
    /// Gateway mode: each tick also lists the configured peers, under the
    /// server's own discovery context. Local-only (the default) never
    /// contacts a peer, even with peers configured.
    federate: bool,
    /// The `--source` allow-list, applied to local and federated rows alike
    /// before any status, cache entry or watched row exists.
    sources: Vec<SourceId>,
    /// The published (debounced) status per watched session.
    settled: HashMap<Key, Settled>,
    /// Failure lines already printed; a broken store is reported once, not
    /// once per tick.
    warned: HashSet<String>,
}

impl Poller {
    pub fn new(
        loader: Loader,
        base: Arc<Query>,
        window: Duration,
        facts: Arc<FactsCache>,
        federate: bool,
        sources: Vec<SourceId>,
    ) -> Self {
        Self {
            loader,
            fleet: Fleet::default(),
            base,
            window,
            facts,
            federate,
            sources,
            settled: HashMap::new(),
            warned: HashSet::new(),
        }
    }

    /// Start from an already-sampled fleet, so the first tick can tell
    /// working from waiting.
    pub fn set_fleet(&mut self, fleet: Fleet) {
        self.fleet = fleet;
    }

    fn warn(&mut self, message: String) {
        if !self.warned.contains(&message) {
            eprintln!("llm-sessions: {message}");
            self.warned.insert(message);
        }
    }

    pub fn tick(&mut self) -> Snapshot {
        let now = OffsetDateTime::now_utc();
        self.fleet.refresh();
        // Gateway mode rereads hosts.json every tick: a peer added after
        // startup is discovered without a restart, like the request-local
        // federation route.
        let (remote, peers) = if self.federate {
            let (config, failure) = remote::Config::load(self.loader.home());
            let peers = config.hosts.iter().map(|host| host.name.clone()).collect();
            self.loader.hosts = config.hosts.into_iter().map(remote::Host::new).collect();
            if let Some(failure) = failure {
                self.warn(failure);
            }
            (Remote::Inline, peers)
        } else {
            (Remote::Skip, Vec::new())
        };
        let mut scan = self.loader.scan(Some(&self.fleet), Cached::Borrow, remote);
        for failure in scan.failures.drain(..) {
            self.warn(failure);
        }
        let sessions = policy_filter(self.loader.home(), &self.base, &self.sources, &mut scan);

        let window = time::Duration::try_from(self.window).unwrap_or(time::Duration::MAX);
        let mut statuses = HashMap::new();
        let mut watched = Vec::new();
        for session in &sessions {
            let live = session.liveness.is_running();
            if session.subagent || session.archived || !(live || now - session.updated <= window) {
                continue;
            }
            let facts = match self.facts.get(session, || self.loader.open(session)) {
                Ok(facts) => facts,
                Err(e) => {
                    self.warn(format!(
                        "{} {}: transcript unreadable: {e}",
                        session.source, session.id
                    ));
                    Facts::default()
                }
            };
            // The local fleet says nothing about another machine: a remote
            // row keeps whatever liveness its own origin reported, and a
            // coincidentally equal pid is never attributed to it.
            let activity = if session.is_local() {
                session
                    .liveness
                    .pid()
                    .and_then(|pid| self.fleet.get(pid))
                    .map_or(Activity::Unknown, |process| process.activity)
            } else {
                Activity::Unknown
            };
            let (status, reason) = derive_status(live, activity, &facts, session.updated, now);
            let key = key(session);
            let settled = settle(
                self.settled.get(&key).copied(),
                status,
                reason,
                session.updated,
                now,
            );
            self.settled.insert(key.clone(), settled);
            statuses.insert(key, StatusBlock::from(settled));
            watched.push(session);
        }

        let listed: HashSet<Key> = sessions.iter().map(key).collect();
        self.facts.retain(&listed);
        self.settled.retain(|key, _| statuses.contains_key(key));

        let mut snapshot = Snapshot {
            sessions: Vec::new(),
            statuses,
            watched: String::new(),
            peers,
        };
        let rows: Vec<Row> = watched.into_iter().map(|s| snapshot.row(s)).collect();
        snapshot.watched = serde_json::to_string(&rows).unwrap_or_else(|_| "[]".to_owned());
        snapshot.sessions = sessions;
        snapshot
    }
}

/// The publication policy every outgoing listing enforces: the `--source`
/// allow-list over everything including rows a federated peer contributed,
/// and the base query over the local rows.
///
/// `body:` terms are answered by this machine's index, which knows nothing
/// about other origins: a remote row simply does not pass a body query,
/// rather than being matched against some local session's hits.
pub(super) fn policy_filter(
    home: &std::path::Path,
    base: &Query,
    sources: &[SourceId],
    scan: &mut crate::catalog::scan::Scan,
) -> Vec<SessionSummary> {
    let hits = match body_hits(home, base) {
        None => None,
        Some(Ok(hits)) => Some(hits),
        Some(Err(_)) => Some(HashSet::new()),
    };
    let indexed = hits.is_some();
    scan.sessions
        .drain(..)
        .filter(|session| sources.is_empty() || sources.contains(&session.source))
        .filter(|session| {
            (!indexed || session.is_local())
                && hits
                    .as_ref()
                    .is_none_or(|hits| hits.contains(&(session.source, session.id.clone())))
                && base.matches(session)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::model::{Locator, SourceId};

    const ORIGIN_A: &str = "00000000-0000-4000-8000-00000000000a";

    fn summary(origin: Option<&str>, id: &str) -> SessionSummary {
        let mut session = SessionSummary::new(
            id.to_owned(),
            SourceId::Codex,
            OffsetDateTime::from_unix_timestamp(1).unwrap(),
            Locator::File {
                path: PathBuf::from("/store/x.jsonl"),
            },
        );
        session.origin_id = origin.map(str::to_owned);
        session
    }

    /// A local row and a federated row that share an agent and id are two
    /// sessions: neither facts nor a status may cross that boundary.
    #[test]
    fn caches_key_on_canonical_identity_not_agent_and_id() {
        let facts = FactsCache::default();
        let opens = Arc::new(AtomicUsize::new(0));
        let local = summary(None, "shared");
        let remote = summary(Some(ORIGIN_A), "shared");
        for session in [&local, &local, &remote, &local] {
            let counter = Arc::clone(&opens);
            let opened = facts.get(session, move || {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(crate::model::Transcript::new("x", SourceId::Codex))
            });
            assert!(opened.is_ok());
        }
        assert_eq!(
            opens.load(Ordering::SeqCst),
            2,
            "one open per origin; the repeat read is served from the cache"
        );
    }

    #[test]
    fn statuses_never_cross_origins() {
        let waiting = StatusBlock {
            status: Status::Waiting,
            waiting_reason: None,
            status_since: OffsetDateTime::from_unix_timestamp(1).unwrap(),
        };
        let mut snapshot = Snapshot::default();
        snapshot.statuses.insert(
            SessionKey {
                origin_id: Some(ORIGIN_A.to_owned()),
                source: SourceId::Codex,
                id: "shared".into(),
            },
            waiting,
        );
        let federated = summary(Some(ORIGIN_A), "shared");
        let local_row = summary(None, "shared");
        let remote = snapshot.row(&federated);
        let local = snapshot.row(&local_row);
        assert_eq!(remote.status, Some(waiting));
        assert_eq!(
            local.status, None,
            "a local row never wears a federated status"
        );
    }

    /// The `--source` allow-list bounds publication: a row a federated
    /// peer contributed is dropped exactly like a local one, before any
    /// status or cache entry could exist for it.
    #[test]
    fn policy_filter_applies_the_source_allow_list_to_remote_rows_too() {
        let mut scan = crate::catalog::scan::Scan::default();
        let mut remote_omp = summary(Some(ORIGIN_A), "remote-omp");
        remote_omp.source = SourceId::Omp;
        scan.sessions = vec![
            summary(None, "local-codex"),
            summary(Some(ORIGIN_A), "remote-codex"),
            remote_omp,
        ];
        let kept = policy_filter(
            std::path::Path::new("/nonexistent-home"),
            &Query::parse(""),
            &[SourceId::Omp],
            &mut scan,
        );
        let ids: Vec<&str> = kept.iter().map(|session| session.id.as_str()).collect();
        assert_eq!(ids, ["remote-omp"]);
    }
}
