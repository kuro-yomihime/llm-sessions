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
use super::poll::{FactsCache, Key, Snapshot, lock, status_now};
use super::stream;
use crate::catalog::loader::{self, Resolved, body_hits, resolve};
use crate::catalog::query::{self, Filters};
use crate::model::SessionSummary;
use crate::source::Source;

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
    pub sources: Vec<Box<dyn Source>>,
    pub snapshot: watch::Receiver<Arc<Snapshot>>,
    pub facts: Arc<FactsCache>,
    pub transcripts: TranscriptCache,
    /// Flips once, on shutdown; open streams close on it.
    pub stopped: watch::Receiver<bool>,
}

pub(super) fn router(app: Arc<App>) -> Router {
    let rest = Router::new()
        .route("/v1/sessions", get(sessions))
        .route("/v1/sessions/{id}", get(session))
        .route("/v1/sessions/{id}/transcript", get(transcript))
        .route_layer(middleware::from_fn_with_state(app.clone(), auth::bearer));
    let stream =
        Router::new()
            .route("/v1/stream", get(stream))
            .route_layer(middleware::from_fn_with_state(
                app.clone(),
                auth::bearer_or_query,
            ));
    Router::new()
        .route("/v1/healthz", get(healthz))
        .merge(rest)
        .merge(stream)
        .fallback(|| async { ApiError::new(StatusCode::NOT_FOUND, "no such route") })
        .route_layer(middleware::from_fn_with_state(
            app.clone(),
            auth::valid_host,
        ))
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

#[derive(Deserialize)]
struct ListQuery {
    q: Option<String>,
    /// A string on purpose: `limit=abc` must be a JSON 400 of our own, not
    /// axum's plain-text extractor rejection.
    limit: Option<String>,
}

/// `list --json` over HTTP: the same grammar, visibility and order.
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
    let text = params.q.unwrap_or_default();
    let query = query::Query::parse(&text);
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
    Ok(Json(json!({ "serverTime": now(), "sessions": rows })))
}

/// The one session `id` names in the latest snapshot.
fn find(snapshot: &Snapshot, id: &str) -> Result<SessionSummary, ApiError> {
    match resolve(&snapshot.sessions, id) {
        Resolved::One(session) => Ok(session.clone()),
        Resolved::Missing => Err(ApiError::new(StatusCode::NOT_FOUND, "no such session")),
        Resolved::Ambiguous(candidates) => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!(
                "{id:?} matches {} sessions; send more of the id",
                candidates.len()
            ),
        )),
    }
}

/// One row, with a status even outside the watched set.
async fn session(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let snapshot = app.snapshot.borrow().clone();
    let session = find(&snapshot, &id)?;
    let mut row = snapshot.row(&session);
    if row.status.is_none() {
        let worker = app.clone();
        let summary = session.clone();
        let facts = tokio::task::spawn_blocking(move || {
            worker.facts.get(&summary, || {
                loader::open(&worker.home, &worker.sources, &[], &summary)
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
) -> Result<Response, ApiError> {
    let session = find(&app.snapshot.borrow().clone(), &id)?;
    let body = if let Some(body) = app.transcripts.get(&session) {
        body
    } else {
        let worker = app.clone();
        let summary = session.clone();
        let body = tokio::task::spawn_blocking(move || {
            let transcript =
                loader::open(&worker.home, &worker.sources, &[], &summary).map_err(|e| {
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
        app.transcripts.put(&session, body.clone());
        body
    };
    Ok(([(header::CONTENT_TYPE, "application/json")], body).into_response())
}

async fn stream(State(app): State<Arc<App>>, ws: WebSocketUpgrade) -> Response {
    let snapshot = app.snapshot.clone();
    let stopped = app.stopped.clone();
    ws.on_upgrade(move |socket| stream::push(socket, snapshot, stopped))
}

/// Serialized transcripts by session, valid while `updated` is unchanged;
/// the oldest entry goes first once full.
#[derive(Default)]
pub(super) struct TranscriptCache(Mutex<VecDeque<(Key, OffsetDateTime, Bytes)>>);

impl TranscriptCache {
    fn get(&self, session: &SessionSummary) -> Option<Bytes> {
        lock(&self.0)
            .iter()
            .find(|(key, updated, _)| {
                key.0 == session.source && key.1 == session.id && *updated == session.updated
            })
            .map(|(_, _, body)| body.clone())
    }

    fn put(&self, session: &SessionSummary, body: Bytes) {
        let mut cache = lock(&self.0);
        cache.retain(|(key, _, _)| !(key.0 == session.source && key.1 == session.id));
        if cache.len() >= CACHED_TRANSCRIPTS {
            cache.pop_front();
        }
        cache.push_back(((session.source, session.id.clone()), session.updated, body));
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
            statuses: HashMap::from([((SourceId::Codex, "live-1".to_owned()), block)]),
            watched: watched.to_owned(),
        }
    }

    struct Harness {
        app: Arc<App>,
        tx: watch::Sender<Arc<Snapshot>>,
        opened: Arc<Mutex<Vec<PathBuf>>>,
        _stop: watch::Sender<bool>,
    }

    fn harness() -> Harness {
        harness_with(Some(TOKEN.into()))
    }

    /// `--no-auth` mode: the branch where a wrong `Host` is the whole attack.
    fn no_auth_harness() -> Harness {
        harness_with(None)
    }

    fn harness_with(token: Option<String>) -> Harness {
        let (tx, rx) = watch::channel(Arc::new(snapshot("[1]")));
        let (stop, stopped) = watch::channel(false);
        let opened = Arc::new(Mutex::new(Vec::new()));
        let app = Arc::new(App {
            token,
            host: "golden".into(),
            home: FsPath::new("/nonexistent-home").into(),
            sources: vec![Box::new(Fake {
                opened: opened.clone(),
            })],
            snapshot: rx,
            facts: Arc::default(),
            transcripts: TranscriptCache::default(),
            stopped,
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
            "/v1/sessions/live-1",
            "/v1/sessions/live-1/transcript",
            "/v1/stream",
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
            sources: Vec::new(),
            snapshot: rx,
            facts: Arc::default(),
            transcripts: TranscriptCache::default(),
            stopped,
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
