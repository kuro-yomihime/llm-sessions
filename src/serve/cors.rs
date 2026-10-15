//! Opt-in CORS for browser clients served from another origin.
//!
//! Off unless `--cors-origin` names origins, and then only those, exactly:
//! no wildcard, no `null`. A preflight carries no credentials by design, so
//! it is answered here, before auth; it only says which method and header a
//! listed origin may use. The request that follows still needs the token.
//! `--no-auth` refuses CORS outright: an allowed page would read every
//! transcript without a secret.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::router::App;

/// How long a browser may reuse a preflight answer.
const PREFLIGHT_MAX_AGE_SECS: &str = "600";

/// One `--cors-origin` value, as a browser writes it in `Origin`:
/// `scheme://host[:port]`, lowercased. Anything with a path, query,
/// credentials or another scheme is a mistake worth refusing at startup,
/// because a browser never sends it and the origin would silently never
/// match.
pub fn parse_origin(value: &str) -> Result<String, String> {
    // clap already prefixes "invalid value '…' for '--cors-origin'".
    let shape = || "expected scheme://host[:port], e.g. https://dash.example.net".to_owned();
    let value = value.trim();
    if value == "*" || value.eq_ignore_ascii_case("null") {
        return Err(
            "a wildcard admits every page; list each origin that may read transcripts".into(),
        );
    }
    let lower = value.to_ascii_lowercase();
    let (scheme, rest) = lower.split_once("://").ok_or_else(shape)?;
    if scheme != "http" && scheme != "https" {
        return Err(shape());
    }
    let (host, port) = match rest.rsplit_once(':') {
        // `[::1]:8080` splits at the port; a bare `[::1]` ends in `]`.
        Some((host, port)) if !port.ends_with(']') => (host, Some(port)),
        _ => (rest, None),
    };
    let host_ok = !host.is_empty()
        && if let Some(inner) = host.strip_prefix('[') {
            inner
                .strip_suffix(']')
                .is_some_and(|ip| ip.parse::<std::net::Ipv6Addr>().is_ok())
        } else {
            host.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        };
    let port_ok = port.is_none_or(|port| port.parse::<u16>().is_ok_and(|port| port != 0));
    if !host_ok || !port_ok {
        return Err(shape());
    }
    Ok(lower)
}

/// The request's `Origin` when it is listed, to echo back.
fn allowed(app: &App, headers: &HeaderMap) -> Option<HeaderValue> {
    let origin = headers.get(header::ORIGIN)?;
    let text = origin.to_str().ok()?;
    app.cors_origins
        .iter()
        .any(|listed| listed.eq_ignore_ascii_case(text))
        .then(|| origin.clone())
}

pub(super) async fn cors(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    if app.cors_origins.is_empty() {
        return next.run(request).await;
    }
    let origin = allowed(&app, request.headers());
    let preflight = request.method() == Method::OPTIONS
        && request
            .headers()
            .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD);
    let mut response = match (&origin, preflight) {
        (Some(_), true) => {
            let mut response = StatusCode::NO_CONTENT.into_response();
            let headers = response.headers_mut();
            headers.insert(
                header::ACCESS_CONTROL_ALLOW_METHODS,
                HeaderValue::from_static("GET"),
            );
            headers.insert(
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                HeaderValue::from_static("authorization"),
            );
            headers.insert(
                header::ACCESS_CONTROL_MAX_AGE,
                HeaderValue::from_static(PREFLIGHT_MAX_AGE_SECS),
            );
            response
        }
        // An unlisted origin's preflight gets what any OPTIONS gets; with
        // no allow headers the browser stops there.
        _ => next.run(request).await,
    };
    let headers = response.headers_mut();
    // Answers differ by `Origin`; a shared cache must not cross them.
    headers.append(header::VARY, HeaderValue::from_static("origin"));
    if let Some(origin) = origin {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_origin_is_scheme_host_and_optional_port_lowercased() {
        for (given, kept) in [
            ("https://dash.example.net", "https://dash.example.net"),
            ("HTTP://Localhost:5173", "http://localhost:5173"),
            ("http://127.0.0.1:8790", "http://127.0.0.1:8790"),
            ("http://[::1]:8080", "http://[::1]:8080"),
            ("https://[::1]", "https://[::1]"),
        ] {
            assert_eq!(parse_origin(given).as_deref(), Ok(kept), "{given}");
        }
    }

    /// A browser never sends these as `Origin`, or sending them would admit
    /// every page; either way the flag would not mean what it says.
    #[test]
    fn wildcards_paths_and_malformed_origins_are_refused() {
        for bad in [
            "*",
            "null",
            "NULL",
            "dash.example.net",
            "ftp://dash.example.net",
            "file://",
            "https://",
            "https://dash.example.net/",
            "https://dash.example.net/app",
            "https://dash.example.net?x=1",
            "https://user@dash.example.net",
            "https://dash.example.net:0",
            "https://dash.example.net:99999",
            "https://dash.example.net:",
            "http://[not-ip]:80",
        ] {
            assert!(parse_origin(bad).is_err(), "{bad}");
        }
    }
}
