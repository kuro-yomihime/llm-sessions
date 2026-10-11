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
use crate::model::{SessionSummary, SourceId, Transcript};
use crate::runtime::live::{Activity, Fleet};
use crate::runtime::status::{
    Facts, Settled, Status, WaitingReason, analyze, derive_status, settle,
};

/// A session, as the server keys it. Local only: ids are unique per agent.
pub(super) type Key = (SourceId, String);

fn key(session: &SessionSummary) -> Key {
    (session.source, session.id.clone())
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
    base: Query,
    window: Duration,
    facts: Arc<FactsCache>,
    /// The published (debounced) status per watched session.
    settled: HashMap<Key, Settled>,
    /// Failure lines already printed; a broken store is reported once, not
    /// once per tick.
    warned: HashSet<String>,
}

impl Poller {
    pub fn new(loader: Loader, base: Query, window: Duration, facts: Arc<FactsCache>) -> Self {
        Self {
            loader,
            fleet: Fleet::default(),
            base,
            window,
            facts,
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
        let scan = self
            .loader
            .scan(Some(&self.fleet), Cached::Borrow, Remote::Skip);
        for failure in scan.failures {
            self.warn(failure);
        }
        let hits = match body_hits(self.loader.home(), &self.base) {
            None => None,
            Some(Ok(hits)) => Some(hits),
            Some(Err(e)) => {
                self.warn(format!("--query body: search failed: {e}"));
                Some(HashSet::new())
            }
        };
        let sessions: Vec<SessionSummary> = scan
            .sessions
            .into_iter()
            .filter(|session| {
                hits.as_ref()
                    .is_none_or(|hits| hits.contains(&key(session)))
            })
            .filter(|session| self.base.matches(session))
            .collect();

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
            let activity = session
                .liveness
                .pid()
                .and_then(|pid| self.fleet.get(pid))
                .map_or(Activity::Unknown, |process| process.activity);
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
        };
        let rows: Vec<Row> = watched.into_iter().map(|s| snapshot.row(s)).collect();
        snapshot.watched = serde_json::to_string(&rows).unwrap_or_else(|_| "[]".to_owned());
        snapshot.sessions = sessions;
        snapshot
    }
}
