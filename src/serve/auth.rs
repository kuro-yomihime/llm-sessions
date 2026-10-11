//! Who may connect: the bind/token rules checked before listening, and the
//! per-request token check.
//!
//! Default closed. A loopback bind still needs `--token` unless `--no-auth`
//! says otherwise, and `--no-auth` is refused outright off loopback: every
//! transcript holds code, paths and often secrets.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::router::{ApiError, App};

/// Auth attempts are not rate-limited, so anything shorter is brute force
/// at line speed. A floor, not a strength claim: the README's advice is
/// high entropy, `openssl rand -hex 32`.
const MIN_TOKEN_CHARS: usize = 16;

/// The token to require, or `None` for an unauthenticated loopback server.
pub fn admit(
    bind: SocketAddr,
    token: Option<String>,
    no_auth: bool,
) -> Result<Option<String>, String> {
    let loopback = bind.ip().to_canonical().is_loopback();
    match (token, no_auth) {
        (Some(_), true) => Err("--token and --no-auth contradict each other".into()),
        (Some(token), false) if token.trim().is_empty() => Err("--token must not be empty".into()),
        (Some(token), false) if token.chars().count() < MIN_TOKEN_CHARS => Err(format!(
            "--token is shorter than {MIN_TOKEN_CHARS} characters; \
             auth attempts are not rate-limited, so generate one with \
             `openssl rand -hex 32`"
        )),
        (Some(token), false) => Ok(Some(token)),
        (None, true) if loopback => Ok(None),
        (None, true) => Err(format!(
            "--no-auth is refused for {bind}: only a loopback address may serve without a token"
        )),
        (None, false) => Err(format!(
            "serving {bind} needs --token (or LLMS_TOKEN){}",
            if loopback {
                "; --no-auth serves loopback without one"
            } else {
                ""
            }
        )),
    }
}

/// Length is not secret; the bytes are compared without early exit.
pub(super) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `Authorization: Bearer <token>` only.
pub(super) async fn bearer(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    if authorized(&app, &request, false) {
        next.run(request).await
    } else {
        ApiError::unauthorized().into_response()
    }
}

/// The bearer header, or `?token=` for WebSocket clients that cannot set
/// headers. A query-string token lands in proxy logs, so no other route
/// accepts one.
pub(super) async fn bearer_or_query(
    State(app): State<Arc<App>>,
    request: Request,
    next: Next,
) -> Response {
    if authorized(&app, &request, true) {
        next.run(request).await
    } else {
        ApiError::unauthorized().into_response()
    }
}

fn authorized(app: &App, request: &Request, query: bool) -> bool {
    let Some(expected) = app.token.as_deref() else {
        return true;
    };
    let matches = |offered: &str| constant_time_eq(offered.as_bytes(), expected.as_bytes());
    let from_header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let (scheme, rest) = value.split_once(' ')?;
            // RFC 7235: the auth scheme is case-insensitive.
            scheme.eq_ignore_ascii_case("Bearer").then_some(rest)
        })
        .map(str::trim);
    if from_header.is_some_and(matches) {
        return true;
    }
    query
        && Query::<TokenQuery>::try_from_uri(request.uri())
            .ok()
            .and_then(|Query(query)| query.token)
            .is_some_and(|offered| matches(&offered))
}

/// Guards `--no-auth`: answers only requests whose `Host` names the
/// machine's loopback, 403 for the rest.
///
/// Loopback binds are still reachable from the browser: a page can point a
/// name it controls at 127.0.0.1 (DNS rebinding) and read transcripts
/// same-origin, and a WebSocket needs no CORS at all. Both require the
/// attacker's hostname in `Host`, so only loopback names pass. With a token
/// the check is skipped: a reverse proxy legitimately rewrites `Host`.
pub(super) async fn valid_host(
    State(app): State<Arc<App>>,
    request: Request,
    next: Next,
) -> Response {
    if app.token.is_none() {
        let host = request
            .headers()
            .get(header::HOST)
            .and_then(|value| value.to_str().ok());
        if !host.is_some_and(loopback_host) {
            return ApiError::new(StatusCode::FORBIDDEN, "untrusted Host header").into_response();
        }
    }
    next.run(request).await
}

/// `Host` is `name` or `name:port`, IPv6 names bracketed.
fn loopback_host(value: &str) -> bool {
    let name = match value.rsplit_once(':') {
        // `127.0.0.1:8787`, `[::1]:8787`; a bare IPv6 address has several
        // colons and no port, so only split when the left side is not itself
        // an address.
        Some((name, _)) if name.starts_with('[') || !name.contains(':') => name,
        _ => value,
    };
    let name = name.trim_matches(['[', ']']);
    name == "localhost"
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.to_canonical().is_loopback())
}

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(address: &str) -> SocketAddr {
        address.parse().unwrap()
    }

    #[test]
    fn a_token_is_required_unless_no_auth_is_explicit_on_loopback() {
        let token = || Some("s3cret-0123456789ab".to_owned());
        assert_eq!(
            admit(at("0.0.0.0:8787"), token(), false),
            Ok(Some("s3cret-0123456789ab".into()))
        );
        assert_eq!(admit(at("127.0.0.1:8787"), None, true), Ok(None));
        assert_eq!(admit(at("[::1]:8787"), None, true), Ok(None));
        assert_eq!(admit(at("[::ffff:127.0.0.1]:8787"), None, true), Ok(None));

        assert!(admit(at("127.0.0.1:8787"), None, false).is_err());
        assert!(admit(at("0.0.0.0:8787"), None, false).is_err());
        assert!(admit(at("0.0.0.0:8787"), None, true).is_err());
        assert!(admit(at("192.168.1.4:8787"), None, true).is_err());
        assert!(admit(at("[::]:8787"), None, true).is_err());
        assert!(admit(at("127.0.0.1:8787"), Some("  ".into()), false).is_err());
        assert!(admit(at("127.0.0.1:8787"), token(), true).is_err());
    }

    /// Auth attempts are unrate-limited, so a short token falls to brute
    /// force; the floor is characters, not bytes.
    #[test]
    fn a_token_must_reach_the_minimum_length() {
        let bind = at("127.0.0.1:8787");
        let at_floor = "a".repeat(MIN_TOKEN_CHARS);
        assert_eq!(
            admit(bind, Some(at_floor.clone()), false),
            Ok(Some(at_floor))
        );
        assert!(admit(bind, Some("a".repeat(MIN_TOKEN_CHARS - 1)), false).is_err());
        // Counted in characters: 15 two-byte characters are 30 bytes and
        // still refused.
        let multibyte = "ä".repeat(MIN_TOKEN_CHARS - 1);
        assert_eq!(multibyte.chars().count(), MIN_TOKEN_CHARS - 1);
        assert!(multibyte.len() >= MIN_TOKEN_CHARS);
        assert!(admit(bind, Some(multibyte), false).is_err());
    }

    #[test]
    fn constant_time_eq_needs_every_byte() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}
