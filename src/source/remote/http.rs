//! The HTTP federation client: how a `url` peer in hosts.json is reached.
//!
//! One shared client, one request per question — capabilities (which doubles
//! as the probe), guarded `federation list`, and origin-qualified transcript
//! reads. Requests are authenticated with a bearer token resolved from the
//! environment variable the *local* configuration names; the token value
//! never appears in a route, an error, or the configuration file.
//!
//! The client follows no redirects: a peer that points elsewhere never gets
//! this observer's credentials forwarded to the new origin. TLS is verified
//! by rustls; `http://` remains available for loopback peers.
//!
//! This transport is fetch-only. Nothing here attaches a terminal, so rows
//! discovered over HTTP offer no resume command.

use std::io::Read;
use std::sync::LazyLock;
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::error::Result;
use crate::federation::{DiscoveryContext, FederationScan, valid_origin};
use crate::model::{SessionSummary, Transcript};
use crate::source::remote::{Host, REMOTE_BODY_MAX};

/// Connection budget, matching the ssh transport's `ConnectTimeout`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Whole-request budget, finite on purpose: a gateway chains one request
/// per hop, and every hop must give up before the observer's patience does.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// One client for every peer: one connection pool, one background runtime,
/// and one place that refuses redirects.
fn client() -> std::result::Result<&'static reqwest::blocking::Client, String> {
    static CLIENT: LazyLock<std::result::Result<reqwest::blocking::Client, String>> =
        LazyLock::new(|| {
            reqwest::blocking::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|error| error.to_string())
        });
    CLIENT.as_ref().map_err(String::clone)
}

/// A transport or protocol failure message, capped like the ssh transport
/// caps stderr: one line, not a page.
fn brief(message: impl std::fmt::Display) -> String {
    message.to_string().chars().take(200).collect()
}

/// Percent-encode a session id as one URL path segment. Ids are agent
/// strings, never trusted URL parts: `/`, `?`, `#` and spaces must stay
/// inside the segment.
fn encode_path_segment(value: &str) -> String {
    use std::fmt::Write as _;
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                // Writing into a String cannot fail.
                let _ = write!(encoded, "%{byte:02X}");
            }
        }
    }
    encoded
}

#[derive(Deserialize)]
struct Capabilities {
    version: u32,
    origin_id: String,
}

impl Host {
    /// This peer's origin identity, learned from its capabilities once per
    /// process. The probe doubles as the reachability check `status` and
    /// `hosts list` report, exactly as the ssh probe does.
    pub(super) fn http_probed(&self) -> std::result::Result<&str, String> {
        self.http_origin
            .get_or_init(|| {
                let capabilities: Capabilities = self
                    .http_get_json("/v1/federation/capabilities", &[])
                    .map_err(|error| error.to_string())?;
                if capabilities.version != crate::federation::PROTOCOL_VERSION {
                    return Err(format!(
                        "federation protocol version {} is not supported",
                        capabilities.version
                    ));
                }
                if !valid_origin(&capabilities.origin_id) {
                    return Err("capabilities carry no valid origin identity".to_string());
                }
                Ok(capabilities.origin_id.to_ascii_lowercase())
            })
            .as_deref()
            .map_err(String::clone)
    }

    /// One guarded listing, mirroring the ssh `federation list` call: the
    /// visited path and remaining hops decrement exactly as they do over
    /// ssh, and credential references inside received routes are dropped —
    /// only local hosts.json may bind a token to an endpoint.
    pub(super) fn http_list(&self, context: &DiscoveryContext) -> Result<FederationScan> {
        let mut scan: FederationScan = self.http_get_json(
            "/v1/federation/list",
            &[
                ("path", serde_json::to_string(&context.visited)?),
                ("hops", (context.remaining_hops - 1).to_string()),
            ],
        )?;
        for session in &mut scan.sessions {
            for route in &mut session.routes {
                route.strip_credentials();
            }
        }
        Ok(scan)
    }

    /// One origin-qualified transcript read. The peer resolves the identity
    /// from its own permitted catalog and follows its own routes; no route
    /// is sent for the server to execute.
    pub(super) fn http_transcript(&self, session: &SessionSummary) -> Result<Transcript> {
        let origin = session
            .origin_id
            .as_deref()
            .filter(|id| valid_origin(id))
            .ok_or_else(|| self.fail("canonical origin identity unavailable"))?;
        self.http_get_json(
            &format!(
                "/v1/sessions/{}/transcript",
                encode_path_segment(&session.id)
            ),
            &[
                ("origin", origin.to_owned()),
                ("source", session.source.name().to_owned()),
            ],
        )
    }

    // ---- one request ----

    /// One authenticated GET parsed as JSON, with the body capped at
    /// [`REMOTE_BODY_MAX`] so a peer cannot stream this observer's memory
    /// full.
    fn http_get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T> {
        let body = self.http_get(path, query)?;
        serde_json::from_str(&body)
            .map_err(|error| self.fail(format!("unreadable federation response: {error}")))
    }

    fn http_get(&self, path: &str, query: &[(&str, String)]) -> Result<String> {
        let base = self
            .config
            .url
            .as_deref()
            .unwrap_or_default()
            .trim_end_matches('/');
        let token = self.bearer()?;
        let client = client().map_err(|message| {
            self.fail(brief(format!("cannot build the HTTP client: {message}")))
        })?;
        let mut request = client.get(format!("{base}{path}")).query(query);
        if let Some(token) = &token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .map_err(|error| self.fail(brief(Self::redact(error.to_string(), token.as_deref()))))?;
        let status = response.status();
        if status.is_redirection() {
            return Err(self.fail(
                "peer redirected the request; refusing to follow it or forward credentials to another origin",
            ));
        }
        let mut body = Vec::new();
        response
            .take(REMOTE_BODY_MAX + 1)
            .read_to_end(&mut body)
            .map_err(|error| self.fail(brief(format!("truncated response: {error}"))))?;
        if body.len() as u64 > REMOTE_BODY_MAX {
            return Err(self.fail(format!(
                "peer response exceeds the {REMOTE_BODY_MAX} byte bound"
            )));
        }
        if !status.is_success() {
            return Err(self.fail(Self::http_error(status, &body, token.as_deref())));
        }
        String::from_utf8(body)
            .map_err(|error| self.fail(format!("response is not UTF-8: {error}")))
    }

    /// A status line a person can act on. Authentication failures say
    /// which side is missing what and quote nothing the peer sent — a
    /// hostile peer can echo the very Authorization header it received
    /// back in its body. Other statuses keep the peer's reason, with the
    /// bearer value redacted first for the same reason. A token never
    /// appears in a message either way.
    fn http_error(status: reqwest::StatusCode, body: &[u8], token: Option<&str>) -> String {
        let reason = match (status.as_u16(), token.is_some()) {
            (401 | 403, false) => {
                format!(
                    "peer requires a token this observer has none configured for (HTTP {status})"
                )
            }
            (401 | 403, true) => format!("peer rejected this observer's token (HTTP {status})"),
            _ => format!("peer answered HTTP {status}"),
        };
        if matches!(status.as_u16(), 401 | 403) {
            return reason;
        }
        let detail = String::from_utf8_lossy(body);
        let detail = detail.trim();
        if detail.is_empty() {
            return reason;
        }
        // Redact the whole text before capping it: a bearer reflected into
        // the body could straddle the cut, and a truncated secret is still
        // a leak.
        let detail: String = Self::redact(detail.to_string(), token)
            .chars()
            .take(120)
            .collect();
        format!("{reason}: {detail}")
    }

    /// Replace the locally resolved bearer value wherever it appears. A
    /// peer that echoes what it received — in a body, an error line, or a
    /// URL — must never see its gift returned in this observer's
    /// diagnostics.
    fn redact(message: String, token: Option<&str>) -> String {
        match token.filter(|token| !token.is_empty()) {
            Some(token) => message.replace(token, "<redacted>"),
            None => message,
        }
    }

    /// The bearer token for this peer, resolved from the environment
    /// variable the local configuration names. Only the variable's name can
    /// appear in a message; its value goes into the header and nowhere else.
    fn bearer(&self) -> Result<Option<String>> {
        let Some(name) = &self.config.token_env else {
            return Ok(None);
        };
        match std::env::var(name) {
            Ok(token) if token.is_empty() => Err(self.fail(format!(
                "token environment variable {name} is set but empty"
            ))),
            Ok(token) => Ok(Some(token)),
            Err(std::env::VarError::NotPresent) => {
                Err(self.fail(format!("token environment variable {name} is not set")))
            }
            Err(_) => Err(self.fail(format!(
                "token environment variable {name} does not hold valid Unicode"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::federation::DiscoveryContext;
    use crate::model::{AccessRoute, Locator, RouteHop, SourceId};
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    const A: &str = "00000000-0000-4000-8000-000000000002";
    const LOCAL: &str = "00000000-0000-4000-8000-000000000001";
    const B: &str = "00000000-0000-4000-8000-000000000003";
    const TOKEN: &str = "fixture-http-token-value";

    /// Lock a shared fixture without treating a panicked sibling as fatal,
    /// the same bargain the scripted ssh transport makes.
    fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// One scripted reply: an HTTP status and a body.
    struct Reply(u16, String);

    /// What the peer observed per request: request line and bearer header.
    type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

    /// A loopback peer speaking just enough HTTP/1.1 for this client: each
    /// reply closes the connection, so every request is one accept. Dropping
    /// the peer stops the accept loop.
    struct Peer {
        base: String,
        seen: Seen,
        stop: Arc<AtomicBool>,
    }

    impl Drop for Peer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    fn serve(replies: Vec<Reply>) -> Peer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        listener.set_nonblocking(true).unwrap();
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let mut replies = replies.into_iter().map(Some).collect::<Vec<_>>();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let seen = Arc::clone(&seen);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let Ok((mut stream, _)) = listener.accept() else {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    };
                    stream.set_nonblocking(false).unwrap();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        continue;
                    }
                    let mut length = 0usize;
                    let mut auth = None;
                    for header in reader.by_ref().lines() {
                        let header = header.unwrap();
                        if header.is_empty() {
                            break;
                        }
                        if let Some(value) = header
                            .to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(str::trim)
                            .and_then(|value| value.parse::<usize>().ok())
                        {
                            length = value;
                        }
                        if let Some(value) = header.strip_prefix("Authorization: Bearer ") {
                            auth = Some(value.to_owned());
                        }
                    }
                    if length > 0 {
                        let mut body = vec![0u8; length];
                        std::io::Read::read_exact(&mut reader, &mut body).ok();
                    }
                    locked(&seen).push((line.trim().to_owned(), auth));
                    let Some(reply) = replies.iter_mut().find_map(Option::take) else {
                        continue;
                    };
                    let Reply(status, body) = reply;
                    let head = match status {
                        302 => format!(
                            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/elsewhere\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        ),
                        status => format!(
                            "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            status,
                            body.len()
                        ),
                    };
                    stream.write_all(head.as_bytes()).unwrap();
                    stream.write_all(body.as_bytes()).unwrap();
                }
            })
        };
        let _ = thread; // exits on its own once `stop` is set
        Peer { base, seen, stop }
    }

    fn host(url: &str, token_env: Option<&str>) -> Host {
        Host::new(crate::source::remote::HostConfig {
            name: "gateway".into(),
            ssh: String::new(),
            url: Some(url.into()),
            token_env: token_env.map(str::to_owned),
            home: None,
            sources: Vec::new(),
            mode: crate::source::remote::Mode::Auto,
        })
    }

    fn capabilities(origin: &str) -> Reply {
        Reply(
            200,
            serde_json::json!({
                "version": crate::federation::PROTOCOL_VERSION,
                "origin_id": origin
            })
            .to_string(),
        )
    }

    fn guarded(remaining: u32) -> DiscoveryContext {
        DiscoveryContext {
            visited: vec![LOCAL.into()],
            remaining_hops: remaining,
        }
    }

    /// A peer answer whose forwarded route tries to name a local secret.
    fn row_with_forwarded_route() -> String {
        let mut row = SessionSummary::new(
            "shared".into(),
            SourceId::Omp,
            time::OffsetDateTime::UNIX_EPOCH,
            Locator::File {
                path: "/remote/session.jsonl".into(),
            },
        );
        row.origin_id = Some(B.into());
        // The forwarding peer attributed the row to its own configured
        // worker; that display label is the row's own, not the next
        // forwarder's to overwrite.
        row.origin_label = Some("worker".into());
        row.routes = vec![AccessRoute {
            hops: vec![RouteHop {
                origin_id: B.into(),
                name: "worker".into(),
                ssh: String::new(),
                url: Some("http://127.0.0.1:8001".into()),
                token_env: Some("HOME".into()),
                home: None,
            }],
        }];
        serde_json::json!({"sessions": [row], "failures": []}).to_string()
    }

    #[test]
    fn path_segments_keep_special_characters_encoded() {
        assert_eq!(encode_path_segment("abc123-._~"), "abc123-._~");
        assert_eq!(encode_path_segment("a/b?c#d e"), "a%2Fb%3Fc%23d%20e");
        assert_eq!(encode_path_segment("ü"), "%C3%BC");
    }

    #[test]
    fn list_probes_identity_decrements_hops_and_strips_forwarded_credentials() {
        let peer = serve(vec![
            capabilities(A),
            Reply(200, row_with_forwarded_route()),
        ]);
        let host = host(&peer.base, None);
        assert_eq!(host.http_probed().unwrap(), A);

        let scan = host.list_with(&[], &guarded(3)).unwrap();
        assert_eq!(scan.sessions.len(), 1);
        let route = &scan.sessions[0].routes[0];
        assert_eq!(route.hops.len(), 2, "the peer hop is prepended");
        assert_eq!(route.hops[0].url.as_deref(), Some(peer.base.as_str()));
        assert_eq!(route.hops[0].ssh, "");
        assert_eq!(
            route.hops[1].token_env, None,
            "a credential name received over the wire is dropped"
        );
        assert_eq!(
            scan.sessions[0].origin_label.as_deref(),
            Some("worker"),
            "an indirect row keeps its true origin attribution; only the route grows"
        );

        let seen = locked(&peer.seen);
        let (request, auth) = &seen[1];
        assert!(request.contains("/v1/federation/list"), "{request}");
        assert!(
            request.contains("hops=2"),
            "{request}: hops decrement by one"
        );
        assert!(auth.is_none(), "a peer without token_env sends no token");
    }

    #[test]
    fn transcript_reads_are_origin_qualified_and_never_send_a_route() {
        let served = Transcript {
            title: Some("served".into()),
            ..Transcript::new("shared", SourceId::Omp)
        };
        let peer = serve(vec![
            capabilities(A),
            Reply(200, serde_json::to_string(&served).unwrap()),
        ]);
        let mut session = SessionSummary::new(
            "shared id/with?odd".into(),
            SourceId::Omp,
            time::OffsetDateTime::UNIX_EPOCH,
            Locator::File {
                path: "/remote/session.jsonl".into(),
            },
        );
        session.origin_id = Some(A.into());
        session.routes = vec![AccessRoute {
            hops: vec![RouteHop {
                origin_id: A.into(),
                name: "gateway".into(),
                ssh: String::new(),
                url: Some(peer.base.clone()),
                token_env: None,
                home: None,
            }],
        }];
        let transcript = host(&peer.base, None)
            .open(&session, Path::new("/local"))
            .unwrap();
        assert_eq!(transcript.title.as_deref(), Some("served"));

        let seen = locked(&peer.seen);
        let (request, _) = &seen[1];
        assert!(
            request.contains("/v1/sessions/shared%20id%2Fwith%3Fodd/transcript"),
            "{request}: the id is one encoded segment"
        );
        assert!(request.contains("origin=00000000-0000"), "{request}");
        assert!(request.contains("source=omp"), "{request}");
        assert!(!request.contains("route"), "{request}: no route is sent");
    }

    #[test]
    fn auth_failures_name_the_missing_side_without_leaking_the_token() {
        // A variable no test sets: the environment is process-global, so
        // every name in this module belongs to exactly one test.
        let peer = serve(Vec::new());
        let error = host(&peer.base, Some("LLMS_UNSET_PEER_TOKEN"))
            .list_with(&[], &guarded(2))
            .unwrap_err();
        assert!(
            error.to_string().contains("LLMS_UNSET_PEER_TOKEN"),
            "{error}"
        );
        assert!(error.to_string().contains("is not set"), "{error}");

        // No token configured: the rejection says which side lacks what.
        let peer = serve(vec![
            capabilities(A),
            Reply(401, r#"{"error":"denied"}"#.to_owned()),
        ]);
        let unauthenticated = host(&peer.base, None);
        let printed = unauthenticated
            .list_with(&[], &guarded(2))
            .unwrap_err()
            .to_string();
        assert!(
            printed.contains("requires a token this observer has none"),
            "{printed}"
        );
        assert!(
            printed.contains("HTTP 401"),
            "{printed}: the failure is still explicit"
        );
        assert!(
            !printed.contains("denied"),
            "{printed}: an authentication failure quotes nothing the peer sent"
        );
        assert_eq!(
            locked(&peer.seen)[1].1,
            None,
            "nothing was sent to authenticate with"
        );

        // A configured token the peer refuses: the message names the
        // rejection, never the value, and an authentication failure never
        // quotes the peer's body — a hostile peer can echo the very header
        // it received. The full header round trip with a real environment
        // variable is the integration harness's job; this crate forbids
        // unsafe code, so tests never mutate the process environment.
        let message = Host::http_error(reqwest::StatusCode::FORBIDDEN, b"echo", Some(TOKEN));
        assert!(
            message.contains("rejected this observer's token"),
            "{message}"
        );
        assert!(!message.contains(TOKEN), "{message}: never the value");
        assert!(
            !message.contains("echo"),
            "{message}: no peer text on auth failures"
        );

        // Other statuses do quote the peer's reason, with the bearer value
        // redacted even if the peer maliciously echoes it back.
        let message = Host::http_error(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            format!("boom {TOKEN} boom").as_bytes(),
            Some(TOKEN),
        );
        assert!(message.contains("HTTP 500"), "{message}");
        assert!(message.contains("<redacted>"), "{message}");
        assert!(!message.contains(TOKEN), "{message}");

        // The boundary case: a reflected bearer straddling the detail cap
        // must be redacted whole, before truncation — a partial secret is
        // still a leak. Padding of 100 starts the 24-character token inside
        // the 120-character window with room to spare, so the old
        // truncate-then-redact order would have leaked its first 20
        // characters.
        let straddling = format!("{}{TOKEN}", "x".repeat(100));
        let message = Host::http_error(
            reqwest::StatusCode::BAD_GATEWAY,
            straddling.as_bytes(),
            Some(TOKEN),
        );
        assert!(message.contains("<redacted>"), "{message}");
        assert!(
            !message.contains(&TOKEN[..20]),
            "{message}: no prefix of the token survives the cut"
        );
        assert!(!message.contains(TOKEN), "{message}");
    }

    #[test]
    fn redirects_are_refused_without_a_second_request() {
        let peer = serve(vec![capabilities(A), Reply(302, String::new())]);
        let error = host(&peer.base, None)
            .list_with(&[], &guarded(2))
            .unwrap_err();
        assert!(error.to_string().contains("redirect"), "{error}");
        assert_eq!(locked(&peer.seen).len(), 2, "no follow-up request");
    }

    #[test]
    fn capabilities_version_and_identity_are_checked() {
        // Version 1 never existed over HTTP: the endpoint arrives with the
        // mixed-route contract, so anything but this observer's protocol
        // version is refused — including a legacy SSH-only peer that
        // accidentally exposes one.
        for version in [1, 3] {
            let peer = serve(vec![Reply(
                200,
                serde_json::json!({"version": version, "origin_id": A}).to_string(),
            )]);
            let error = host(&peer.base, None).http_probed().unwrap_err();
            assert!(error.contains(&format!("version {version}")), "{error}");
        }

        let peer = serve(vec![Reply(
            200,
            serde_json::json!({
                "version": crate::federation::PROTOCOL_VERSION,
                "origin_id": "not-a-uuid"
            })
            .to_string(),
        )]);
        let error = host(&peer.base, None).http_probed().unwrap_err();
        assert!(error.contains("origin identity"), "{error}");
    }
}
