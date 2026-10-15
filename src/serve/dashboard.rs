//! `serve --dashboard`: one self-contained page at `/`, compiled in.
//!
//! The page carries no session data and no secret, so it is served without
//! the token; every request it makes goes to this origin's `/v1/*` routes
//! and is authenticated there. The user hands it the token once in the URL
//! fragment (`/#token=…`), which never reaches the server or a proxy log.
//! The headers pin it to this origin: no external loads, no framing.

use axum::http::header;
use axum::response::IntoResponse;

/// Scripts, styles and fonts are inline; `connect-src 'self'` covers the
/// same-origin fetches and the `/v1/stream` WebSocket.
const CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
    font-src data:; img-src data:; connect-src 'self'; base-uri 'none'; form-action 'none'; \
    frame-ancestors 'none'";

const PAGE: &str = include_str!("../../assets/dashboard.html");

pub(super) async fn page() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CONTENT_SECURITY_POLICY, CSP),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        PAGE,
    )
}
