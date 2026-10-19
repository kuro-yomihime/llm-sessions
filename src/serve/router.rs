//! The routes: GET only, plus the one WebSocket upgrade.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::watch;

use super::auth;
use super::cors;
use super::poll::{FactsCache, Key, Snapshot, lock, policy_filter, status_now};
use super::stream;
use crate::catalog::loader::{self, Resolved, body_hits, guarded_scan};
use crate::catalog::query::{self, Filters};
use crate::federation::{Guard, Invalid, MAX_HOPS, valid_origin};
use crate::model::{SessionSummary, SourceId, normalize_origin};
use crate::source::{Source, remote};

/// Rows `/v1/sessions` returns without `limit`, as `list` prints.
const DEFAULT_LIMIT: usize = 50;

/// Serialized transcripts kept for repeat reads. Small: a transcript is
/// opened on request, never in the poll loop, and can run to megabytes.
const CACHED_TRANSCRIPTS: usize = 16;

pub(super) struct App {
    /// `None` only under `--no-auth` on loopback.
    pub token: Option<String>,
    pub host: String,
    pub home: PathBuf,
    /// Local adapters, for opening what the snapshot lists.
    pub adapters: Vec<Box<dyn Source>>,
    pub snapshot: watch::Receiver<Arc<Snapshot>>,
    pub facts: Arc<FactsCache>,
    pub transcripts: TranscriptCache,
    /// Flips once, on shutdown; open streams close on it.
    pub stopped: watch::Receiver<bool>,
    /// Gateway mode: federation discovery may contact the configured
    /// peers. Local-only (the default) answers with this machine's rows
    /// whatever the peer configuration says.
    pub federate: bool,
    /// The base query every published row matches, as on the command line.
    pub base: Arc<query::Query>,
    /// The `--source` allow-list; empty for all.
    pub sources: Vec<SourceId>,
    /// `--cors-origin`: exact origins whose pages may read responses.
    pub cors_origins: Vec<String>,
    /// `--dashboard`: answer `/` with the web page.
    #[cfg(feature = "dashboard")]
    pub dashboard: bool,
}

pub(super) fn router(app: Arc<App>) -> Router {
    let rest = Router::new()
        .route("/v1/sessions", get(sessions))
        .route("/v1/hosts", get(hosts))
        .route("/v1/sessions/{id}", get(session))
        .route("/v1/sessions/{id}/transcript", get(transcript))
        .route("/v1/federation/capabilities", get(federation_capabilities))
        .route("/v1/federation/list", get(federation_list))
        .route_layer(middleware::from_fn_with_state(app.clone(), auth::bearer));
    let stream =
        Router::new()
            .route("/v1/stream", get(stream))
            .route_layer(middleware::from_fn_with_state(
                app.clone(),
                auth::bearer_or_query,
            ));
    let routes = Router::new()
        .route("/v1/healthz", get(healthz))
        .merge(rest)
        .merge(stream);
    // The page is public, like healthz: it carries no data or secret.
    #[cfg(feature = "dashboard")]
    let routes = if app.dashboard {
        routes.route("/", get(super::dashboard::page))
    } else {
        routes
    };
    routes
        .fallback(|| async { ApiError::new(StatusCode::NOT_FOUND, "no such route") })
        .route_layer(middleware::from_fn_with_state(
            app.clone(),
            auth::valid_host,
        ))
        // Outermost: a preflight never reaches auth, and every answer,
        // errors and the fallback included, carries the allow headers.
        .layer(middleware::from_fn_with_state(app.clone(), cors::cors))
        .with_state(app)
}

/// `{"error": "..."}` with a status code.
#[derive(Debug)]
pub(super) struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub(super) fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized")
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

async fn healthz(State(app): State<Arc<App>>) -> Json<serde_json::Value> {
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "host": app.host,
    }))
}

/// The `host:` labels this server's rows can carry: `local` first, then
/// every peer label, sorted. In gateway mode that is the configured peers
/// plus labels further hops contributed; a local-only server lists just
/// `local`, since no peer row is ever published.
fn host_labels(snapshot: &Snapshot) -> Vec<String> {
    let peers: std::collections::BTreeSet<&str> = snapshot
        .sessions
        .iter()
        .filter(|s| !s.is_local())
        .filter_map(|s| s.origin_label.as_deref())
        .chain(snapshot.peers.iter().map(String::as_str))
        .collect();
    std::iter::once("local")
        .chain(peers)
        .map(str::to_owned)
        .collect()
}

/// Which machines `host:` can name, so a client never has to guess one.
async fn hosts(State(app): State<Arc<App>>) -> Json<serde_json::Value> {
    let snapshot = app.snapshot.borrow().clone();
    let hosts: Vec<_> = host_labels(&snapshot)
        .into_iter()
        .map(|label| {
            let local = label == "local";
            let mut entry = json!({ "label": label, "local": local });
            if local {
                entry["hostname"] = json!(app.host);
            }
            entry
        })
        .collect();
    Json(json!({ "hosts": hosts }))
}

#[derive(Deserialize)]
struct ListQuery {
    q: Option<String>,
    /// A string on purpose: `limit=abc` must be a JSON 400 of our own, not
    /// axum's plain-text extractor rejection.
    limit: Option<String>,
    /// The UTC offset `today`/`yesterday` count from; UTC when absent.
    tz: Option<String>,
}

/// A UTC offset: `Z`, `UTC`, `+HH`, or `+HH:MM` (either sign). A leading
/// space is a `+` an unencoded query string turned into one.
fn parse_offset(text: &str) -> Option<time::UtcOffset> {
    if text.eq_ignore_ascii_case("z") || text.eq_ignore_ascii_case("utc") {
        return Some(time::UtcOffset::UTC);
    }
    let (sign, rest) = match text.as_bytes().first()? {
        b'+' | b' ' => (1, &text[1..]),
        b'-' => (-1, &text[1..]),
        _ => return None,
    };
    let (hours, minutes) = rest.split_once(':').unwrap_or((rest, "00"));
    let digits = |part: &str| {
        (part.len() == 2 && part.bytes().all(|b| b.is_ascii_digit()))
            .then(|| part.parse::<i8>().ok())
            .flatten()
    };
    time::UtcOffset::from_hms(sign * digits(hours)?, sign * digits(minutes)?, 0).ok()
}

/// `list --json` over HTTP: the same grammar, visibility and order, plus
/// `warnings` for any filter that cannot have meant what it says.
async fn sessions(
    State(app): State<Arc<App>>,
    Query(params): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let limit = match params.limit.as_deref() {
        None => DEFAULT_LIMIT,
        Some(text) => match text.parse::<usize>() {
            Ok(0) => usize::MAX,
            Ok(n) => n,
            Err(_) => {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!("invalid limit {text:?}: use a non-negative integer"),
                ));
            }
        },
    };
    let offset = match params.tz.as_deref() {
        None => time::UtcOffset::UTC,
        Some(text) => parse_offset(text).ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("invalid tz {text:?}: use a UTC offset like +02:00 (send + as %2B)"),
            )
        })?,
    };
    let text = params.q.unwrap_or_default();
    let query = query::Query::parse_at(&text, OffsetDateTime::now_utc().to_offset(offset));
    let hits = if query.needs_index() {
        let home = app.home.clone();
        let searched =
            tokio::task::spawn_blocking(move || body_hits(&home, &query::Query::parse(&text)))
                .await
                .map_err(|e| ApiError::internal(format!("body search panicked: {e}")))?;
        match searched {
            Some(Err(e)) => {
                // The error names the index file; that is server-side detail.
                eprintln!("llm-sessions: body search failed: {e}");
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "body: needs the index; run `llm-sessions index` on the server",
                ));
            }
            Some(Ok(hits)) => Some(hits),
            None => None,
        }
    } else {
        None
    };
    let filters = Filters {
        query: &query,
        hits: hits.as_ref(),
        show_archived: false,
        live_only: false,
    };
    let snapshot = app.snapshot.borrow().clone();
    let mut warnings = query.warnings().to_vec();
    let mut wanted = query.hosts().peekable();
    if wanted.peek().is_some() {
        let labels = host_labels(&snapshot);
        for value in wanted {
            if labels
                .iter()
                .any(|label| label.to_lowercase().starts_with(value))
            {
                continue;
            }
            let hint = if app.host.starts_with(value) {
                format!("; {value:?} is this server, which host:local names")
            } else {
                String::new()
            };
            warnings.push(format!(
                "host:{value} matches no known machine (known: {}){hint}",
                labels.join(", ")
            ));
        }
    }
    let rows: Vec<_> = snapshot
        .sessions
        .iter()
        // A subagent is a child of a session, never a row of its own; its
        // id still resolves on the detail routes, as with `show`.
        .filter(|s| !s.subagent)
        .filter(|s| query::eligible(s, &filters))
        .take(limit)
        .map(|s| snapshot.row(s))
        .collect();
    Ok(Json(json!({
        "serverTime": now(),
        "tz": format!(
            "{}{:02}:{:02}",
            if offset.is_negative() { '-' } else { '+' },
            offset.whole_hours().unsigned_abs(),
            offset.minutes_past_hour().unsigned_abs()
        ),
        "warnings": warnings,
        "sessions": rows,
    })))
}

/// The one session `id` names in a row set. `origin` and `source`, when a
/// client sends them, narrow the pool first: federated rows can share an
/// agent and id with a local one, and the collision is answered, never
/// guessed.
fn find(
    sessions: &[SessionSummary],
    id: &str,
    origin: Option<&str>,
    source: Option<SourceId>,
) -> Result<SessionSummary, ApiError> {
    let pool: Vec<&SessionSummary> = sessions
        .iter()
        .filter(|s| origin.is_none_or(|want| s.origin_id.as_deref() == Some(want)))
        .filter(|s| source.is_none_or(|want| s.source == want))
        .collect();
    match loader::resolve(pool, id) {
        Resolved::One(session) => Ok(session.clone()),
        Resolved::Missing => Err(ApiError::new(StatusCode::NOT_FOUND, "no such session")),
        Resolved::Ambiguous(candidates) => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!(
                "{id:?} matches {} sessions; send the origin and source, or more of the id",
                candidates.len()
            ),
        )),
    }
}

#[derive(Deserialize)]
struct SessionQuery {
    origin: Option<String>,
    source: Option<String>,
}

/// The `origin`/`source` query parameters, validated once: a malformed
/// UUID or agent name is the client's mistake, not a missing session.
fn qualifiers(params: &SessionQuery) -> Result<(Option<String>, Option<SourceId>), ApiError> {
    let origin = match &params.origin {
        Some(text) => {
            let id = normalize_origin(text);
            if valid_origin(&id) {
                Some(id)
            } else {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid origin: expected the peer's UUID origin identity",
                ));
            }
        }
        None => None,
    };
    let source = match &params.source {
        Some(name) => Some(SourceId::parse(&name.to_lowercase()).ok_or_else(|| {
            ApiError::new(StatusCode::BAD_REQUEST, format!("unknown source {name:?}"))
        })?),
        None => None,
    };
    Ok((origin, source))
}

/// A detail read's id resolution. The poll snapshot answers first; a miss
/// carrying both qualifiers — exactly what a discovery row hands a client —
/// is retried against one fresh, policy-filtered discovery, so a row first
/// seen through `/v1/federation/list` is readable before the next poll
/// tick. A less-qualified miss stays a miss: only a read that already
/// names its row pays for discovery, and the refreshed rows passed the
/// same publication policy, so nothing hidden becomes readable.
async fn resolve(
    app: &Arc<App>,
    id: &str,
    origin: Option<&str>,
    source: Option<SourceId>,
) -> Result<SessionSummary, ApiError> {
    let snapshot = app.snapshot.borrow().clone();
    match find(&snapshot.sessions, id, origin, source) {
        Ok(session) => Ok(session),
        Err(miss) => {
            if miss.status != StatusCode::NOT_FOUND || origin.is_none() || source.is_none() {
                return Err(miss);
            }
            let worker = app.clone();
            let id = id.to_owned();
            let origin = origin.unwrap().to_owned();
            // A discovery that itself failed is the answer, not a detail
            // to smooth over: its 503 or 500 reaches the client instead
            // of a miss that pretends the row is absent. A completed scan
            // answers on its own terms — its misses read exactly like the
            // snapshot's, and an ambiguity it introduced is an ambiguity
            // the client must see.
            let scan = tokio::task::spawn_blocking(move || discover(&worker, &Guard::default()))
                .await
                .map_err(|e| ApiError::internal(format!("federation discovery panicked: {e}")))
                .and_then(|scan| scan)?;
            find(&scan.sessions, &id, Some(&origin), source)
        }
    }
}

/// One row, with a status even outside the watched set.
async fn session(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(params): Query<SessionQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (origin, source) = qualifiers(&params)?;
    let snapshot = app.snapshot.borrow().clone();
    let session = resolve(&app, &id, origin.as_deref(), source).await?;
    let mut row = snapshot.row(&session);
    if row.status.is_none() {
        let worker = app.clone();
        let summary = session.clone();
        let facts = tokio::task::spawn_blocking(move || {
            worker.facts.get(&summary, || {
                loader::open(&worker.home, &worker.adapters, &summary)
            })
        })
        .await
        .map_err(|e| ApiError::internal(format!("reading the transcript panicked: {e}")))?
        .map_err(|e| {
            // The error carries the session's path on this machine.
            eprintln!("llm-sessions: opening {} failed: {e}", session.id);
            ApiError::internal("transcript unreadable")
        })?;
        row.status = Some(status_now(session.liveness.is_running(), facts, &session));
    }
    Ok(Json(
        serde_json::to_value(&row).map_err(|e| ApiError::internal(e.to_string()))?,
    ))
}

/// The normalized transcript, byte for byte what `show <id> --json` prints.
async fn transcript(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(params): Query<SessionQuery>,
) -> Result<Response, ApiError> {
    let (origin, source) = qualifiers(&params)?;
    let session = resolve(&app, &id, origin.as_deref(), source).await?;
    let config_revision = (!session.is_local())
        .then(|| remote::Config::revision(&app.home))
        .flatten();
    let body = if let Some(body) = app.transcripts.get(&session, config_revision) {
        body
    } else {
        let worker = app.clone();
        let summary = session.clone();
        let body = tokio::task::spawn_blocking(move || {
            let transcript =
                loader::open(&worker.home, &worker.adapters, &summary).map_err(|e| {
                    // The error carries the session's path on this machine.
                    eprintln!("llm-sessions: opening {} failed: {e}", summary.id);
                    "transcript unreadable".to_owned()
                })?;
            let mut json = serde_json::to_string_pretty(&transcript).map_err(|e| e.to_string())?;
            json.push('\n');
            Ok::<_, String>(Bytes::from(json))
        })
        .await
        .map_err(|e| ApiError::internal(format!("reading the transcript panicked: {e}")))?
        .map_err(ApiError::internal)?;
        app.transcripts.put(&session, config_revision, body.clone());
        body
    };
    Ok(([(header::CONTENT_TYPE, "application/json")], body).into_response())
}

/// This server's federation handshake: the protocol version and the stable
/// origin identity. A missing or unreadable marker is an explicit error —
/// never an invented identity.
async fn federation_capabilities(
    State(app): State<Arc<App>>,
) -> Result<Json<crate::federation::Capabilities>, ApiError> {
    let home = app.home.clone();
    let capabilities =
        tokio::task::spawn_blocking(move || crate::federation::Capabilities::own(&home))
            .await
            .map_err(|e| ApiError::internal(format!("reading the origin identity panicked: {e}")))?
            .map_err(|e| {
                // The error names this machine's state directory; that stays here.
                eprintln!("llm-sessions: federation identity unavailable: {e}");
                ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "federation identity unavailable on this server",
                )
            })?;
    Ok(Json(capabilities))
}

#[derive(Deserialize)]
struct FederationQuery {
    /// A JSON array of the UUIDs already visiting on this request's branch.
    path: Option<String>,
    /// The remaining hop budget, already decremented by the caller.
    hops: Option<String>,
}

/// Guarded discovery over HTTP, the same contract as `federation list`
/// over SSH: the caller's own ancestors and depth, never a cached gateway
/// snapshot. Local-only servers answer with this machine's rows alone,
/// whatever the peer configuration says.
async fn federation_list(
    State(app): State<Arc<App>>,
    Query(params): Query<FederationQuery>,
) -> Result<Json<crate::federation::FederationScan>, ApiError> {
    let bad = |what: &str| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            format!(
                "invalid {what}: use a JSON array of origin ids and hops of at most {MAX_HOPS}"
            ),
        )
    };
    let guard = Guard::parse(
        params.path.as_deref().unwrap_or("[]"),
        params.hops.as_deref(),
    )
    .map_err(|invalid| match invalid {
        Invalid::Path => bad("federation path"),
        Invalid::Hops => bad("hops"),
    })?;
    let worker = app.clone();
    let scan = tokio::task::spawn_blocking(move || discover(&worker, &guard))
        .await
        .map_err(|e| ApiError::internal(format!("federation discovery panicked: {e}")))?;
    Ok(Json(scan?))
}

/// One request-local discovery: a fresh scan under the caller's own branch
/// context, assembled by the same guarded pipeline as the SSH endpoint,
/// then narrowed to what this server's publication policy allows.
fn discover(
    app: &Arc<App>,
    guard: &Guard,
) -> std::result::Result<crate::federation::FederationScan, ApiError> {
    let origin = crate::federation::read_origin(&app.home).map_err(|e| {
        // The error names this machine's state directory; that stays here.
        eprintln!("llm-sessions: federation identity unavailable: {e}");
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "federation identity unavailable on this server",
        )
    })?;
    if guard.visited.contains(&origin) {
        return Ok(crate::federation::FederationScan::default());
    }
    let (hosts, failure) = if app.federate {
        configured_hosts(app)
    } else {
        (Vec::new(), None)
    };
    let mut scan = guarded_scan(&app.home, &app.adapters, &hosts, &origin, guard);
    if let Some(failure) = failure {
        // A broken hosts.json is part of the answer, not only the log:
        // the caller learns why this branch contributed no peers.
        scan.failures.push(failure);
    }
    let sessions = policy_filter(&app.home, &app.base, &app.sources, &mut scan);
    Ok(crate::federation::FederationScan {
        sessions,
        failures: scan.failures,
    })
}

/// The peers configured right now, not at startup: a host added after the
/// server started is discoverable without a restart. A malformed file is
/// reported, never swallowed.
fn configured_hosts(app: &Arc<App>) -> (Vec<remote::Host>, Option<String>) {
    let (config, failure) = remote::Config::load(&app.home);
    (
        config.hosts.into_iter().map(remote::Host::new).collect(),
        failure,
    )
}

async fn stream(State(app): State<Arc<App>>, ws: WebSocketUpgrade) -> Response {
    let snapshot = app.snapshot.clone();
    let stopped = app.stopped.clone();
    ws.on_upgrade(move |socket| stream::push(socket, snapshot, stopped))
}

/// Serialized transcripts remain valid only within one peer configuration.
struct CachedTranscript {
    key: Key,
    updated: OffsetDateTime,
    config_revision: Option<(std::time::SystemTime, u64)>,
    body: Bytes,
}

#[derive(Default)]
pub(super) struct TranscriptCache(Mutex<VecDeque<CachedTranscript>>);

impl TranscriptCache {
    fn get(
        &self,
        session: &SessionSummary,
        revision: Option<(std::time::SystemTime, u64)>,
    ) -> Option<Bytes> {
        lock(&self.0)
            .iter()
            .find(|entry| {
                entry.key == session.key()
                    && entry.updated == session.updated
                    && entry.config_revision == revision
            })
            .map(|entry| entry.body.clone())
    }

    fn put(
        &self,
        session: &SessionSummary,
        config_revision: Option<(std::time::SystemTime, u64)>,
        body: Bytes,
    ) {
        let mut cache = lock(&self.0);
        let key = session.identity();
        cache.retain(|entry| entry.key != key);
        if cache.len() >= CACHED_TRANSCRIPTS {
            cache.pop_front();
        }
        cache.push_back(CachedTranscript {
            key,
            updated: session.updated,
            config_revision,
            body,
        });
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path as FsPath;

    use axum::body::Body;
    use axum::http::{Method, Request};
    use futures_util::StreamExt;
    use tower::ServiceExt;

    use super::*;
    use crate::error::Result;
    use crate::model::{Block, Liveness, Locator, Role, SourceId, Transcript, Turn};
    use crate::runtime::status::{Status, WaitingReason};
    use crate::serve::poll::{StatusBlock, publish};

    const TOKEN: &str = "s3cret";

    /// Opens whatever it is asked to and remembers the locator: a test of
    /// what the server may open, not of any agent format.
    struct Fake {
        opened: Arc<Mutex<Vec<PathBuf>>>,
    }

    impl Source for Fake {
        fn id(&self) -> SourceId {
            SourceId::Codex
        }
        fn roots(&self) -> Vec<PathBuf> {
            Vec::new()
        }
        fn list(&self) -> Result<Vec<SessionSummary>> {
            Ok(Vec::new())
        }
        fn open(&self, locator: &Locator) -> Result<Transcript> {
            let Locator::File { path } = locator else {
                unreachable!()
            };
            self.opened.lock().unwrap().push(path.clone());
            let stem = path.file_stem().unwrap().to_string_lossy();
            let mut transcript = Transcript::new(stem.as_ref(), SourceId::Codex);
            let mut call = Turn::new(Role::Assistant);
            call.blocks.push(Block::ToolCall {
                name: "bash".into(),
                input: serde_json::json!({"command": "cargo test"}),
            });
            transcript.push(call);
            let mut result = Turn::new(Role::Tool);
            result.blocks.push(Block::ToolResult {
                name: None,
                ok: false,
                output: "1 failed".into(),
            });
            transcript.push(result);
            Ok(transcript)
        }
    }

    fn row(id: &str) -> SessionSummary {
        SessionSummary {
            title: Some(format!("title {id}")),
            ..SessionSummary::new(
                id.into(),
                SourceId::Codex,
                OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(),
                Locator::File {
                    path: PathBuf::from(format!("/store/{id}.jsonl")),
                },
            )
        }
    }

    fn snapshot(watched: &str) -> Snapshot {
        let mut live = row("live-1");
        live.liveness = Liveness::Live { pid: 7 };
        let mut archived = row("old-archived");
        archived.archived = true;
        let mut child = row("live-1-child");
        child.subagent = true;
        child.parent = Some("live-1".into());
        let block = StatusBlock {
            status: Status::Waiting,
            waiting_reason: Some(WaitingReason::Approval),
            status_since: OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(),
        };
        Snapshot {
            sessions: vec![live, row("dead-2"), archived, child],
            statuses: HashMap::from([(
                crate::model::SessionKey {
                    origin_id: None,
                    source: SourceId::Codex,
                    id: "live-1".to_owned(),
                },
                block,
            )]),
            watched: watched.to_owned(),
            peers: Vec::new(),
        }
    }

    struct Harness {
        app: Arc<App>,
        tx: watch::Sender<Arc<Snapshot>>,
        opened: Arc<Mutex<Vec<PathBuf>>>,
        _stop: watch::Sender<bool>,
    }

    fn harness() -> Harness {
        harness_with(Some(TOKEN.into()), &[])
    }

    /// `--no-auth` mode: the branch where a wrong `Host` is the whole attack.
    fn no_auth_harness() -> Harness {
        harness_with(None, &[])
    }

    fn harness_with(token: Option<String>, cors_origins: &[&str]) -> Harness {
        let (tx, rx) = watch::channel(Arc::new(snapshot("[1]")));
        let (stop, stopped) = watch::channel(false);
        let opened = Arc::new(Mutex::new(Vec::new()));
        let app = Arc::new(App {
            token,
            host: "golden".into(),
            home: FsPath::new("/nonexistent-home").into(),
            adapters: vec![Box::new(Fake {
                opened: opened.clone(),
            })],
            snapshot: rx,
            facts: Arc::default(),
            transcripts: TranscriptCache::default(),
            stopped,
            federate: false,
            base: Arc::new(query::Query::parse("")),
            sources: Vec::new(),
            cors_origins: cors_origins.iter().map(|&o| o.to_owned()).collect(),
            #[cfg(feature = "dashboard")]
            dashboard: true,
        });
        Harness {
            app,
            tx,
            opened,
            _stop: stop,
        }
    }

    async fn call(app: &Arc<App>, method: Method, uri: &str, auth: Option<&str>) -> Response {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(auth) = auth {
            request = request.header(header::AUTHORIZATION, auth);
        }
        router(app.clone())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn get(app: &Arc<App>, uri: &str) -> (StatusCode, serde_json::Value) {
        let response = call(app, Method::GET, uri, Some("Bearer s3cret")).await;
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap_or_default())
    }

    async fn status_of(app: &Arc<App>, uri: &str, auth: Option<&str>) -> StatusCode {
        call(app, Method::GET, uri, auth).await.status()
    }

    async fn status_with_host(app: &Arc<App>, uri: &str, host: Option<&str>) -> StatusCode {
        let mut request = Request::builder().method(Method::GET).uri(uri);
        if let Some(host) = host {
            request = request.header(header::HOST, host);
        }
        router(app.clone())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn every_route_but_healthz_needs_the_token() {
        let Harness { app, .. } = harness();
        for uri in [
            "/v1/sessions",
            "/v1/hosts",
            "/v1/sessions/live-1",
            "/v1/sessions/live-1/transcript",
            "/v1/federation/capabilities",
            "/v1/federation/list",
        ] {
            assert_eq!(
                status_of(&app, uri, None).await,
                StatusCode::UNAUTHORIZED,
                "{uri}"
            );
            assert_eq!(
                status_of(&app, uri, Some("Bearer s3cre")).await,
                StatusCode::UNAUTHORIZED,
                "{uri}"
            );
            assert_eq!(
                status_of(&app, uri, Some(TOKEN)).await,
                StatusCode::UNAUTHORIZED,
                "{uri}: the scheme is part of the header"
            );
        }
        assert_eq!(status_of(&app, "/v1/healthz", None).await, StatusCode::OK);
        assert_eq!(
            status_of(&app, "/v1/sessions", Some("Bearer s3cret")).await,
            StatusCode::OK
        );
    }

    /// Query-string tokens end up in proxy logs; only the WebSocket route,
    /// whose browser clients cannot set headers, takes one.
    #[tokio::test]
    async fn only_the_stream_accepts_a_query_string_token() {
        let Harness { app, .. } = harness();
        assert_eq!(
            status_of(&app, "/v1/sessions?token=s3cret", None).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status_of(&app, "/v1/stream?token=wrong", None).await,
            StatusCode::UNAUTHORIZED
        );
        // Past auth, a plain GET is refused by the upgrade, not by auth.
        let past = status_of(&app, "/v1/stream?token=s3cret", None).await;
        assert!(past.is_client_error() && past != StatusCode::UNAUTHORIZED);
    }

    /// `--no-auth` on loopback is still reachable from any web page through
    /// DNS rebinding or cross-site WebSocket connects; both need the
    /// attacker's name in `Host`. Only loopback names get answers.
    #[tokio::test]
    async fn no_auth_answers_only_loopback_host_headers() {
        let Harness { app, .. } = no_auth_harness();
        for host in [
            None,
            Some("evil.com:8787"),
            Some("rebind.attacker.example:8787"),
            Some("localhost.evil.com"),
            Some("2130706433:8787"),
        ] {
            assert_eq!(
                status_with_host(&app, "/v1/healthz", host).await,
                StatusCode::FORBIDDEN,
                "{host:?}"
            );
            assert_eq!(
                status_with_host(&app, "/v1/sessions", host).await,
                StatusCode::FORBIDDEN,
                "{host:?}"
            );
            assert_eq!(
                status_with_host(&app, "/v1/stream", host).await,
                StatusCode::FORBIDDEN,
                "{host:?}"
            );
        }
        for host in ["localhost:8787", "127.0.0.1:8787", "[::1]:8787"] {
            assert_eq!(
                status_with_host(&app, "/v1/healthz", Some(host)).await,
                StatusCode::OK,
                "{host}"
            );
            assert_eq!(
                status_with_host(&app, "/v1/sessions", Some(host)).await,
                StatusCode::OK,
                "{host}"
            );
        }
    }

    fn cors_request(method: &Method, uri: &str, origin: &str, auth: Option<&str>) -> Request<Body> {
        let mut request = Request::builder()
            .method(method.clone())
            .uri(uri)
            .header(header::ORIGIN, origin);
        if *method == Method::OPTIONS {
            request = request
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "authorization");
        }
        if let Some(auth) = auth {
            request = request.header(header::AUTHORIZATION, auth);
        }
        request.body(Body::empty()).unwrap()
    }

    async fn send(app: &Arc<App>, request: Request<Body>) -> Response {
        router(app.clone()).oneshot(request).await.unwrap()
    }

    fn allow_origin(response: &Response) -> Option<&str> {
        response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .map(|value| value.to_str().unwrap())
    }

    /// A browser's preflight carries no token, so a listed origin's is
    /// answered before auth; the GET that follows still needs the token.
    #[tokio::test]
    async fn a_listed_origin_passes_preflight_but_still_needs_the_token() {
        let Harness { app, .. } = harness_with(Some(TOKEN.into()), &["https://dash.example"]);
        let origin = "https://dash.example";

        let preflight = send(
            &app,
            cors_request(&Method::OPTIONS, "/v1/sessions", origin, None),
        )
        .await;
        assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
        assert_eq!(allow_origin(&preflight), Some(origin));
        assert_eq!(
            preflight.headers()[header::ACCESS_CONTROL_ALLOW_HEADERS],
            "authorization"
        );
        assert_eq!(
            preflight.headers()[header::ACCESS_CONTROL_ALLOW_METHODS],
            "GET"
        );

        let anonymous = send(
            &app,
            cors_request(&Method::GET, "/v1/sessions", origin, None),
        )
        .await;
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            allow_origin(&anonymous),
            Some(origin),
            "the page must be able to read why it was refused"
        );

        let authed = send(
            &app,
            cors_request(&Method::GET, "/v1/sessions", origin, Some("Bearer s3cret")),
        )
        .await;
        assert_eq!(authed.status(), StatusCode::OK);
        assert_eq!(allow_origin(&authed), Some(origin));
        assert_eq!(authed.headers()[header::VARY], "origin");
    }

    /// An unlisted origin, or any origin with CORS off, gets no allow
    /// header, so the browser withholds even a successful response.
    #[tokio::test]
    async fn unlisted_origins_and_the_default_get_no_allow_headers() {
        let Harness { app: listed, .. } =
            harness_with(Some(TOKEN.into()), &["https://dash.example"]);
        let Harness { app: off, .. } = harness();
        for (app, origin) in [
            (&listed, "https://evil.example"),
            (&listed, "https://dash.example.evil.example"),
            (&listed, "http://dash.example"),
            (&off, "https://dash.example"),
        ] {
            let preflight = send(
                app,
                cors_request(&Method::OPTIONS, "/v1/sessions", origin, None),
            )
            .await;
            assert_ne!(preflight.status(), StatusCode::NO_CONTENT, "{origin}");
            assert_eq!(allow_origin(&preflight), None, "{origin}");
            let authed = send(
                app,
                cors_request(&Method::GET, "/v1/sessions", origin, Some("Bearer s3cret")),
            )
            .await;
            assert_eq!(authed.status(), StatusCode::OK, "{origin}");
            assert_eq!(allow_origin(&authed), None, "{origin}");
        }
    }

    /// `q=body:` needs the index, and the server must not build it: creating
    /// an empty one answers an empty list the client cannot tell from "no
    /// matches", on a host that never asked for an index.
    #[tokio::test]
    async fn a_body_query_without_an_index_is_rejected_and_creates_nothing() {
        let home = std::env::temp_dir().join(format!("llms-serve-no-index-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let (tx, rx) = watch::channel(Arc::new(snapshot("[1]")));
        let (stop, stopped) = watch::channel(false);
        let app = Arc::new(App {
            token: Some(TOKEN.into()),
            host: "golden".into(),
            home: home.clone(),
            adapters: Vec::new(),
            snapshot: rx,
            facts: Arc::default(),
            transcripts: TranscriptCache::default(),
            stopped,
            federate: false,
            base: Arc::new(query::Query::parse("")),
            sources: Vec::new(),
            cors_origins: Vec::new(),
            #[cfg(feature = "dashboard")]
            dashboard: false,
        });
        let (status, body) = get(&app, "/v1/sessions?q=body:needle").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let printed = body["error"].as_str().unwrap();
        assert!(printed.contains("index"), "{printed}");
        assert!(
            !printed.contains(home.to_string_lossy().as_ref()),
            "error leaks the index location: {printed}"
        );
        assert!(
            !home.join(".cache").exists(),
            "the request created the index"
        );
        let _ = std::fs::remove_dir_all(&home);
        drop((tx, stop));
    }

    /// A limit the query string cannot deserialize used to answer axum's
    /// plain-text extractor rejection, breaking the error shape; the
    /// handler parses it now.
    #[tokio::test]
    async fn a_bad_limit_is_a_json_error_not_an_extractor_rejection() {
        let Harness { app, .. } = harness();
        for limit in ["abc", "-1", "99999999999999999999999"] {
            let response = call(
                &app,
                Method::GET,
                &format!("/v1/sessions?limit={limit}"),
                Some("Bearer s3cret"),
            )
            .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{limit}");
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes)
                .expect("every error is a JSON object with an error field");
            assert!(body["error"].as_str().unwrap().contains("limit"), "{body}");
        }
    }

    /// RFC 7235 makes the auth scheme case-insensitive; clients in the
    /// wild send `bearer`, `BEARER`.
    #[tokio::test]
    async fn the_bearer_scheme_matches_case_insensitively() {
        let Harness { app, .. } = harness();
        for value in ["bearer s3cret", "BEARER s3cret", "BeArEr  s3cret"] {
            assert_eq!(
                status_of(&app, "/v1/sessions", Some(value)).await,
                StatusCode::OK,
                "{value}"
            );
        }
        // Still no scheme, no entry.
        assert_eq!(
            status_of(&app, "/v1/sessions", Some("s3cret")).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn no_method_but_get_reaches_any_route() {
        let Harness { app, opened, .. } = harness();
        for uri in [
            "/v1/healthz",
            "/v1/sessions",
            "/v1/sessions/live-1",
            "/v1/sessions/live-1/transcript",
            "/v1/sessions/live-1/input",
            "/v1/sessions/live-1/approve",
            "/v1/stream",
        ] {
            for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
                let status = call(&app, method.clone(), uri, Some("Bearer s3cret"))
                    .await
                    .status();
                assert!(
                    matches!(
                        status,
                        StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_FOUND
                    ),
                    "{method} {uri}: {status}"
                );
            }
        }
        assert!(opened.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn listing_applies_the_query_grammar_and_hides_children_and_archived_rows() {
        let Harness { app, .. } = harness();
        let ids = |body: &serde_json::Value| -> Vec<String> {
            body["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["id"].as_str().unwrap().to_owned())
                .collect()
        };
        let (status, body) = get(&app, "/v1/sessions").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ids(&body), ["live-1", "dead-2"]);
        assert_eq!(body["sessions"][0]["status"]["status"], "waiting");
        assert_eq!(body["sessions"][0]["status"]["waitingReason"], "approval");
        assert!(body["sessions"][1]["status"].is_null(), "not watched");
        // `list --json` fields are all there, unchanged.
        let parsed: SessionSummary =
            serde_json::from_value(body["sessions"][0].clone()).expect("a list row");
        assert_eq!(parsed.liveness, Liveness::Live { pid: 7 });

        assert_eq!(
            ids(&get(&app, "/v1/sessions?q=live:yes").await.1),
            ["live-1"]
        );
        assert_eq!(
            ids(&get(&app, "/v1/sessions?q=archived:yes").await.1),
            ["old-archived"]
        );
        assert_eq!(ids(&get(&app, "/v1/sessions?limit=1").await.1), ["live-1"]);
        assert_eq!(ids(&get(&app, "/v1/sessions?limit=0").await.1).len(), 2);
    }

    #[tokio::test]
    async fn hosts_are_listed_and_a_host_filter_matching_none_is_explained() {
        let Harness { app, tx, .. } = harness();
        tx.send(Arc::new(Snapshot {
            peers: vec!["t16".into(), "fe-1".into()],
            ..snapshot("[1]")
        }))
        .unwrap();

        let (status, body) = get(&app, "/v1/hosts").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["hosts"],
            json!([
                {"label": "local", "local": true, "hostname": "golden"},
                {"label": "fe-1", "local": false},
                {"label": "t16", "local": false},
            ])
        );

        let (_, body) = get(&app, "/v1/sessions?q=host:fe").await;
        assert_eq!(body["warnings"], json!([]), "a prefix of fe-1 is known");
        let (_, body) = get(&app, "/v1/sessions?q=host:AR3%20hots:x").await;
        assert_eq!(
            body["warnings"],
            json!([
                r#"unknown filter key "hots" in "hots:x"; searched as free text"#,
                "host:ar3 matches no known machine (known: local, fe-1, t16)",
            ])
        );
        let (_, body) = get(&app, "/v1/sessions?q=host:golden").await;
        let warning = body["warnings"][0].as_str().unwrap();
        assert!(warning.contains("host:local"), "{warning}");
    }

    #[tokio::test]
    async fn the_calendar_offset_is_validated_and_echoed() {
        let Harness { app, .. } = harness();
        assert_eq!(get(&app, "/v1/sessions").await.1["tz"], "+00:00");
        for (tz, echoed) in [
            ("%2B02:00", "+02:00"),
            ("+05:30", "+05:30"),
            ("-03", "-03:00"),
        ] {
            let (status, body) = get(&app, &format!("/v1/sessions?tz={tz}")).await;
            assert_eq!(status, StatusCode::OK, "{tz}");
            assert_eq!(body["tz"], echoed, "{tz}");
        }
        for tz in ["Europe%2FWarsaw", "2", "%2B2:00", "%2B26:00"] {
            let (status, body) = get(&app, &format!("/v1/sessions?tz={tz}")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{tz}");
            assert!(body["error"].as_str().unwrap().contains("tz"), "{tz}");
        }
    }

    #[tokio::test]
    async fn ids_resolve_like_show_and_unwatched_rows_get_a_status_on_request() {
        let Harness { app, .. } = harness();
        let (status, body) = get(&app, "/v1/sessions/dead").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], "dead-2");
        // Not live, last event a failed tool result: derived on request.
        assert_eq!(body["status"]["status"], "failed");

        let (status, body) = get(&app, "/v1/sessions/live-1-child").await;
        assert_eq!(status, StatusCode::OK, "a subagent resolves by its own id");
        assert_eq!(body["parent"], "live-1");

        assert_eq!(get(&app, "/v1/sessions/live-1").await.1["id"], "live-1");
        assert_eq!(
            get(&app, "/v1/sessions/live").await.0,
            StatusCode::BAD_REQUEST,
            "a prefix of two ids"
        );
        assert_eq!(
            get(&app, "/v1/sessions/nope").await.0,
            StatusCode::NOT_FOUND
        );
    }

    /// The server opens only locators its own scan produced; an id is looked
    /// up, never turned into a path.
    #[tokio::test]
    async fn a_transcript_is_opened_only_through_a_listed_locator() {
        let Harness { app, opened, .. } = harness();
        for uri in [
            "/v1/sessions/..%2F..%2Fetc%2Fpasswd/transcript",
            "/v1/sessions/%2Fetc%2Fpasswd/transcript",
            "/v1/sessions/nope/transcript",
        ] {
            assert_eq!(get(&app, uri).await.0, StatusCode::NOT_FOUND, "{uri}");
        }
        assert!(opened.lock().unwrap().is_empty());

        let response = call(
            &app,
            Method::GET,
            "/v1/sessions/dead-2/transcript",
            Some("Bearer s3cret"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let transcript: Transcript = serde_json::from_slice(&body).unwrap();
        assert_eq!(transcript.title.as_deref(), Some("title dead-2"));
        assert_eq!(
            *opened.lock().unwrap(),
            [PathBuf::from("/store/dead-2.jsonl")]
        );

        // Unchanged since: served again without reopening.
        assert_eq!(
            get(&app, "/v1/sessions/dead-2/transcript").await.0,
            StatusCode::OK
        );
        assert_eq!(opened.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_stream_sends_a_snapshot_then_only_changes() {
        // Held: a dropped stop sender reads as shutdown.
        let Harness { app, tx, _stop, .. } = harness();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(axum::serve(listener, router(app)).into_future());

        let (mut socket, _) =
            tokio_tungstenite::connect_async(format!("ws://{address}/v1/stream?token=s3cret"))
                .await
                .unwrap();
        let mut next_frame = async || -> serde_json::Value {
            let message = socket.next().await.unwrap().unwrap();
            serde_json::from_str(message.to_text().unwrap()).unwrap()
        };
        let first = next_frame().await;
        assert_eq!(first["type"], "snapshot");
        assert_eq!(first["sessions"], serde_json::json!([1]));

        // A rescan whose watched rows are unchanged wakes no one.
        publish(&tx, snapshot("[1]"));
        publish(&tx, snapshot("[2]"));
        let frame = tokio::time::timeout(Duration::from_secs(5), next_frame())
            .await
            .expect("a changed watch set is pushed");
        assert_eq!(frame["sessions"], serde_json::json!([2]));

        // A change that reverts inside the debounce window is no change.
        publish(&tx, snapshot("[3]"));
        publish(&tx, snapshot("[2]"));
        assert!(
            tokio::time::timeout(Duration::from_millis(1500), next_frame())
                .await
                .is_err()
        );
    }
}
