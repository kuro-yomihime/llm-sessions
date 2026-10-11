//! `GET /v1/stream`: the watched rows, pushed when they change.
//!
//! ```text
//! {"type":"snapshot","serverTime":"…","sessions":[Row…]}   on connect, then on change
//! {"type":"ping","serverTime":"…"}                         every 20 s
//! ```
//!
//! Every snapshot frame is the full watched list; clients diff. Changes are
//! debounced so a burst of ticks costs one frame, and a change that reverted
//! inside the window costs none. Client frames are ignored.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::watch;

use super::poll::Snapshot;

const DEBOUNCE: Duration = Duration::from_millis(500);
const PING_EVERY: Duration = Duration::from_secs(20);

fn server_time() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

/// `watched` is already a serialized JSON array.
fn snapshot_frame(snapshot: &Snapshot) -> String {
    format!(
        r#"{{"type":"snapshot","serverTime":"{}","sessions":{}}}"#,
        server_time(),
        snapshot.watched
    )
}

pub(super) async fn push(
    mut socket: WebSocket,
    mut rx: watch::Receiver<Arc<Snapshot>>,
    mut stopped: watch::Receiver<bool>,
) {
    let mut sent = rx.borrow_and_update().clone();
    if send(&mut socket, snapshot_frame(&sent)).await.is_err() {
        return;
    }
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING_EVERY, PING_EVERY);
    loop {
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() {
                    return;
                }
                // Trailing debounce: changes inside the window coalesce.
                tokio::time::sleep(DEBOUNCE).await;
                let latest = rx.borrow_and_update().clone();
                if latest.watched != sent.watched {
                    if send(&mut socket, snapshot_frame(&latest)).await.is_err() {
                        return;
                    }
                    sent = latest;
                }
            }
            _ = ping.tick() => {
                let frame = format!(r#"{{"type":"ping","serverTime":"{}"}}"#, server_time());
                if send(&mut socket, frame).await.is_err() {
                    return;
                }
            }
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                Some(Ok(_)) => {}
            },
            _ = stopped.changed() => {
                let _ = socket.send(Message::Close(None)).await;
                return;
            }
        }
    }
}

async fn send(socket: &mut WebSocket, frame: String) -> Result<(), axum::Error> {
    socket.send(Message::Text(frame.into())).await
}
