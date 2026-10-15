//! `llm-sessions serve`: this machine's sessions over a token-authenticated,
//! read-only HTTP and WebSocket API.
//!
//! ```text
//! GET /v1/healthz                       no auth: {"ok", "version", "host"}
//! GET /v1/sessions?q=<query>&limit=N    {"serverTime", "sessions": [Row]}
//! GET /v1/sessions/{id}                 Row
//! GET /v1/sessions/{id}/transcript      Transcript, the bytes of `show <id> --json`
//! GET /v1/federation/capabilities       {"version", "origin_id"}: the handshake
//! GET /v1/federation/list?path=&hops=   FederationScan, guarded like the SSH route
//! GET /v1/stream                        WebSocket: watched rows as snapshot frames
//! ```
//!
//! `origin`/`source` query parameters qualify the detail routes when
//! federated rows collide with local ids. Federation discovery is opt-in
//! (`--federate`): without it the server lists this machine only, even
//! with peers configured, and never contacts one.
//!
//! A Row is a [`crate::SessionSummary`] exactly as `list --json` prints it,
//! plus `status`: `{"status", "waitingReason", "statusSince"}` from
//! [`crate::runtime::status`] for watched sessions, `null` for the rest.
//! Watched means live, or written within `--status-window`: status needs
//! the transcript tail, and reading every body in a listing would break the
//! tier-1 rule.
//!
//! Read-only by construction, not by a flag: the router registers GET routes
//! and one WebSocket route, nothing else, so there is no request that
//! archives, indexes, edits hosts or reaches a terminal. Ids are resolved
//! against the latest scan and a session is opened through the locator its
//! own adapter produced; nothing a client sends becomes a path. No resume
//! command crosses the wire.
//!
//! Architecture: one poller owns a [`Loader`] and a persistent [`Fleet`],
//! rescans on a timer on a blocking thread, and publishes an immutable
//! snapshot through a `watch` channel. Handlers only read the
//! latest snapshot; the library is never re-entered from async code.
//!
//! Liveness reads `/proc`, so this is Linux-only like the rest of the
//! binary; `main` captures the local offset before this starts threads.

mod auth;
mod cors;
mod poll;
mod router;
mod stream;

use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::catalog::loader::Loader;
use crate::catalog::query::Query;
use crate::error::{Error, Result};
use crate::model::SourceId;
use crate::runtime::live::Fleet;
use crate::source;

pub use self::auth::admit;
pub use self::cors::parse_origin;

/// Everything `llm-sessions serve` is told on its command line. No `Debug`:
/// it holds the token.
#[derive(Clone)]
pub struct Config {
    pub bind: SocketAddr,
    /// The shared secret; `None` only together with `no_auth`.
    pub token: Option<String>,
    /// Serve without a token. Refused unless `bind` is loopback.
    pub no_auth: bool,
    /// Reported by `/v1/healthz`; the system hostname when `None`.
    pub host_name: Option<String>,
    /// Time between rescans.
    pub poll: Duration,
    /// Sessions written this recently get a derived status even when no
    /// process is attached.
    pub status_window: Duration,
    /// Only these agents; empty for all.
    pub sources: Vec<SourceId>,
    /// A base query, in the `list -q` grammar, every published row matches.
    pub query: Option<String>,
    /// Gateway mode: also list the configured peers each scan and answer
    /// federation discovery through them. Default is this machine only.
    pub federate: bool,
    /// Browser origins, from [`parse_origin`], allowed to call the API
    /// cross-origin. Empty for none; refused together with `no_auth`.
    pub cors_origins: Vec<String>,
}

/// Serve until SIGINT or SIGTERM.
pub fn run(home: &Path, mut config: Config) -> Result<()> {
    let token = admit(config.bind, config.token.take(), config.no_auth).map_err(Error::Serve)?;
    if token.is_none() && !config.cors_origins.is_empty() {
        return Err(Error::Serve(
            "--cors-origin needs --token: an allowed page would read every transcript without one"
                .into(),
        ));
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Serve(format!("starting the runtime: {e}")))?;
    let served = runtime.block_on(serve(home, config, token));
    // A scan still running on a blocking thread must not hold the exit.
    runtime.shutdown_timeout(Duration::from_secs(1));
    served
}

async fn serve(home: &Path, config: Config, token: Option<String>) -> Result<()> {
    // Bind before the first scan: a taken port should fail in milliseconds,
    // not after a quarter-second probe and a full listing.
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(|e| Error::Serve(format!("bind {}: {e}", config.bind)))?;
    let address = listener
        .local_addr()
        .map_err(|e| Error::Serve(format!("bind {}: {e}", config.bind)))?;

    let wanted = |sources: &mut Vec<Box<dyn source::Source>>| {
        if !config.sources.is_empty() {
            sources.retain(|source| config.sources.contains(&source.id()));
        }
    };
    let mut loader = Loader::new(home);
    wanted(&mut loader.sources);
    // Handlers open transcripts with their own adapters: the loader's
    // overlay holds a database connection, so it cannot be shared.
    let mut sources = source::all(home);
    wanted(&mut sources);
    let facts = Arc::new(poll::FactsCache::default());
    let base = Arc::new(Query::parse(config.query.as_deref().unwrap_or_default()));
    let mut poller = poll::Poller::new(
        loader,
        base.clone(),
        config.status_window,
        facts.clone(),
        config.federate,
        config.sources.clone(),
    );
    let (poller, first) = tokio::task::spawn_blocking(move || {
        poller.set_fleet(Fleet::probe());
        let first = poller.tick();
        (poller, first)
    })
    .await
    .map_err(|e| Error::Serve(format!("first scan panicked: {e}")))?;
    let listed = first.sessions.len();
    let (tx, rx) = watch::channel(Arc::new(first));
    let (stop, stopped) = watch::channel(false);
    tokio::spawn(poll_forever(poller, tx, config.poll, stopped.clone()));

    let host = config.host_name.clone().unwrap_or_else(system_hostname);
    eprintln!(
        "llm-sessions: serving {listed} session(s) on http://{address} as {host:?}, auth {}, {}",
        if token.is_some() { "by token" } else { "OFF" },
        if config.federate {
            "federation gateway"
        } else {
            "local-only"
        },
    );
    let app = Arc::new(router::App {
        token,
        host,
        home: home.to_path_buf(),
        adapters: sources,
        snapshot: rx,
        facts,
        transcripts: router::TranscriptCache::default(),
        stopped,
        federate: config.federate,
        base,
        sources: config.sources.clone(),
        cors_origins: config.cors_origins.clone(),
    });
    axum::serve(listener, router::router(app))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            eprintln!("llm-sessions: shutting down");
            // Streams and the poller watch this; open WebSockets would
            // otherwise hold a graceful shutdown forever.
            let _ = stop.send(true);
        })
        .await
        .map_err(|e| Error::Serve(e.to_string()))
}

/// Rescan every `every`, publishing each result. A panicking scan costs one
/// tick, never the server.
async fn poll_forever(
    mut poller: poll::Poller,
    tx: watch::Sender<Arc<poll::Snapshot>>,
    every: Duration,
    mut stopped: watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(every);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await;
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = stopped.changed() => return,
        }
        let joined = tokio::task::spawn_blocking(move || {
            let out = std::panic::catch_unwind(AssertUnwindSafe(|| poller.tick()));
            (poller, out)
        })
        .await;
        let Ok((back, out)) = joined else {
            eprintln!("llm-sessions: the poller was cancelled; serving the last scan");
            return;
        };
        poller = back;
        match out {
            Ok(snapshot) => poll::publish(&tx, snapshot),
            Err(_) => eprintln!("llm-sessions: a scan panicked; retrying next tick"),
        }
    }
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let terminate = async {
        match signal(SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            // No SIGTERM handler: Ctrl-C still stops the server.
            Err(_) => std::future::pending().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = terminate => {}
    }
}

/// This machine's name, lowercased, for `/v1/healthz`.
fn system_hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .ok()
        .map(|name| name.trim().to_lowercase())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "localhost".to_owned())
}
