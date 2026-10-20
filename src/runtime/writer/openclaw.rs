//! Instance-bound `OpenClaw` delivery. Only pre-admission unavailability permits
//! trying another transport; a lost acknowledgement must never submit twice.

use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signer as _, SigningKey};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tungstenite::client::IntoClientRequest as _;
use tungstenite::{Message, WebSocket, stream::MaybeTlsStream};

use super::{SessionHarness, failure, launch};
use crate::error::FailureKind;
use crate::{Error, Result};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenClawConfig {
    pub ws_url: Option<String>,
    pub http_url: Option<String>,
    pub token_env: Option<String>,
    pub agent_id: Option<String>,
    pub session_key: Option<String>,
    pub device_seed_env: Option<String>,
    #[serde(default)]
    pub cli: bool,
}

#[derive(Clone, Debug)]
pub struct OpenClawHarness {
    pub session_id: String,
    pub config: OpenClawConfig,
}

impl SessionHarness for OpenClawHarness {
    fn write_message(
        &self,
        message: &str,
        timeout: Duration,
    ) -> Result<crate::runtime::insert::Delivery> {
        let harness = self.clone();
        let message = message.to_owned();
        Ok(launch(format!("openclaw {}", self.session_id), move || {
            harness.deliver(&message, timeout)
        }))
    }
}

impl OpenClawHarness {
    pub fn deliver(&self, message: &str, timeout: Duration) -> Result<()> {
        let token = secret(self.config.token_env.as_deref())?;
        let seed = secret(self.config.device_seed_env.as_deref())?;
        self.deliver_resolved(message, timeout, token.as_deref(), seed.as_deref())
    }

    fn deliver_resolved(
        &self,
        message: &str,
        timeout: Duration,
        token: Option<&str>,
        seed: Option<&str>,
    ) -> Result<()> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| fail(FailureKind::Timeout, "delivery deadline is out of range"))?;
        remaining(deadline)?;
        let mut unavailable = Vec::new();
        let mut probe_timeout = None;
        if let Some(url) = &self.config.ws_url {
            match self.websocket(url, message, token, seed, deadline) {
                Ok(()) => return Ok(()),
                Err(Attempt::Stop(error)) => return Err(error),
                Err(Attempt::Unavailable(reason)) => unavailable.push(reason),
                Err(Attempt::ProbeTimeout(error)) => probe_timeout = Some(error),
            }
        }
        if let Some(url) = &self.config.http_url {
            match self.http(url, message, token, deadline) {
                Ok(()) => return Ok(()),
                Err(Attempt::Stop(error)) => return Err(error),
                Err(Attempt::Unavailable(reason)) => unavailable.push(reason),
                Err(Attempt::ProbeTimeout(error)) => probe_timeout = Some(error),
            }
        }
        if self.config.cli {
            return self.cli(message, token, deadline);
        }
        if let Some(error) = probe_timeout {
            return Err(error);
        }
        Err(fail(
            FailureKind::Unavailable,
            if unavailable.is_empty() {
                "no OpenClaw transport is configured".to_owned()
            } else {
                unavailable.join("; ")
            },
        ))
    }

    fn websocket(
        &self,
        url: &str,
        message: &str,
        token: Option<&str>,
        seed: Option<&str>,
        deadline: Instant,
    ) -> AttemptResult {
        let probe_deadline = probe_deadline(deadline);
        let request = url
            .into_client_request()
            .map_err(|_| fail(FailureKind::Protocol, "invalid OpenClaw WebSocket URL"))?;
        let uri = request.uri();
        let scheme = uri.scheme_str().unwrap_or_default();
        if !matches!(scheme, "ws" | "wss")
            || uri.authority().is_some_and(|a| a.as_str().contains('@'))
        {
            return Err(fail(
                FailureKind::Protocol,
                "expected ws:// or wss:// without credentials",
            )
            .into());
        }
        let host = uri
            .host()
            .ok_or_else(|| fail(FailureKind::Protocol, "WebSocket URL has no host"))?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let port = uri
            .port_u16()
            .unwrap_or(if scheme == "wss" { 443 } else { 80 });
        let stream = connect(host, port, probe_deadline).map_err(Attempt::before_dispatch)?;
        // Do not trust hostnames or a forwarded Host header for the device-less
        // exception: both the literal destination and actual peer must be local.
        let direct_loopback = host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
            && stream.peer_addr().is_ok_and(|addr| addr.ip().is_loopback());
        let identity = match seed {
            Some(seed) => Some(device_key(seed)?),
            None if direct_loopback && token.is_some() => None,
            None => {
                return Err(fail(
                    FailureKind::Authentication,
                    "remote WebSocket requires a paired device_seed_env and token_env",
                )
                .into());
            }
        };
        let token = token
            .ok_or_else(|| fail(FailureKind::Authentication, "WebSocket requires token_env"))?;
        let stream = DeadlineStream {
            stream,
            deadline: probe_deadline,
        };
        let (mut socket, _) =
            tungstenite::client_tls(request, stream).map_err(|error| match error {
                tungstenite::HandshakeError::Failure(tungstenite::Error::Http(response)) => {
                    let status = response.status().as_u16();
                    match status {
                        401 | 403 => Attempt::Stop(fail(
                            FailureKind::Authentication,
                            "WebSocket upgrade refused authentication",
                        )),
                        404 | 405 | 501 => Attempt::Unavailable(format!(
                            "WebSocket upgrade unsupported ({status})"
                        )),
                        _ => Attempt::Stop(fail(
                            FailureKind::Rejected,
                            format!("WebSocket upgrade refused ({status})"),
                        )),
                    }
                }
                tungstenite::HandshakeError::Failure(error) => {
                    Attempt::before_dispatch(ws_error(error))
                }
                tungstenite::HandshakeError::Interrupted(_) => {
                    Attempt::Unavailable("WebSocket handshake interrupted before dispatch".into())
                }
            })?;
        let challenge = loop {
            let frame = read_frame(&mut socket).map_err(Attempt::before_dispatch)?;
            if frame["type"] == "event" && frame["event"] == "connect.challenge" {
                break frame;
            }
        };
        let nonce = challenge["payload"]["nonce"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| fail(FailureKind::Protocol, "challenge is missing nonce"))?;
        let signed_at = challenge["payload"]["ts"].as_u64().ok_or_else(|| {
            fail(
                FailureKind::Protocol,
                "challenge timestamp must be a non-negative integer",
            )
        })?;
        let mut params = json!({
            "minProtocol": 4, "maxProtocol": 4,
            "client": {"id": "cli", "mode": "cli", "version": env!("CARGO_PKG_VERSION"), "platform": "linux"},
            "role": "operator", "scopes": ["operator.read", "operator.write"],
            "auth": {"token": token}
        });
        if let Some(key) = identity {
            let public = key.verifying_key().to_bytes();
            let device_id = format!("{:x}", Sha256::digest(public));
            let payload = format!(
                "v3|{device_id}|cli|cli|operator|operator.read,operator.write|{signed_at}|{token}|{nonce}|linux|"
            );
            params["device"] = json!({
                "id": device_id, "publicKey": URL_SAFE_NO_PAD.encode(public),
                "signature": URL_SAFE_NO_PAD.encode(key.sign(payload.as_bytes()).to_bytes()),
                "signedAt": signed_at, "nonce": nonce
            });
        } else {
            params["client"]["id"] = json!("gateway-client");
            params["client"]["mode"] = json!("backend");
        }
        let connect_id = operation_id();
        send_frame(
            &mut socket,
            &json!({"type":"req", "id":connect_id, "method":"connect", "params":params}),
            64 * 1024,
            usize::MAX,
        )
        .map_err(Attempt::before_dispatch)?;
        let hello = response(&mut socket, &connect_id).map_err(Attempt::before_dispatch)?;
        let hello = rpc_payload(&hello)?;
        if hello["type"] != "hello-ok" || hello["protocol"] != 4 {
            return Err(fail(
                FailureKind::Protocol,
                "unsupported WebSocket hello/protocol",
            )
            .into());
        }
        let scopes = hello["auth"]["scopes"]
            .as_array()
            .ok_or_else(|| fail(FailureKind::Protocol, "hello has no granted scopes"))?;
        if hello["auth"]["role"] != "operator"
            || !scopes
                .iter()
                .any(|s| s == "operator.write" || s == "operator.admin")
        {
            return Err(fail(
                FailureKind::Authentication,
                "gateway did not grant operator.write",
            )
            .into());
        }
        let methods = hello["features"]["methods"]
            .as_array()
            .ok_or_else(|| fail(FailureKind::Protocol, "hello has no methods"))?;
        if !methods.iter().any(|m| m == "agent") {
            return Err(Attempt::Unavailable(
                "WebSocket does not advertise agent".into(),
            ));
        }
        let max_payload = policy_limit(hello, "maxPayload")?;
        let max_buffered_bytes = policy_limit(hello, "maxBufferedBytes")?;
        socket.set_config(|config| {
            config.write_buffer_size = 0;
            config.max_write_buffer_size = max_buffered_bytes;
            config.max_message_size = Some(max_payload);
            config.max_frame_size = Some(max_payload);
        });
        let id = operation_id();
        let mut params = json!({
            "sessionId": self.session_id, "message": message,
            "createSessionIfMissing": false, "deliver": false, "idempotencyKey": id
        });
        if let Some(agent) = &self.config.agent_id {
            params["agentId"] = json!(agent);
        }
        match socket.get_mut() {
            MaybeTlsStream::Plain(stream) => stream.deadline = deadline,
            MaybeTlsStream::Rustls(stream) => stream.sock.deadline = deadline,
            _ => return Err(fail(FailureKind::Protocol, "unsupported TLS stream").into()),
        }
        // From the first attempted agent write onward, transport failures have
        // uncertain admission. Never fall back, even if flushing failed.
        send_frame(
            &mut socket,
            &json!({"type":"req", "id":id, "method":"agent", "params":params}),
            max_payload,
            max_buffered_bytes,
        )?;
        let result = response(&mut socket, &id)?;
        let payload = rpc_payload(&result)?;
        check_turn(payload)?;
        Ok(())
    }

    fn http(
        &self,
        base: &str,
        message: &str,
        token: Option<&str>,
        deadline: Instant,
    ) -> AttemptResult {
        let Some(key) = self
            .config
            .session_key
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        else {
            return Err(Attempt::Unavailable(
                "HTTP requires an explicit canonical session_key (not the UUID)".into(),
            ));
        };
        let Some(agent) = self
            .config
            .agent_id
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        else {
            return Err(Attempt::Unavailable(
                "HTTP requires an explicit agent_id".into(),
            ));
        };
        let token = token.ok_or_else(|| {
            fail(
                FailureKind::Authentication,
                "HTTP requires token_env for Bearer authentication",
            )
        })?;
        let mut base = reqwest::Url::parse(base)
            .map_err(|_| fail(FailureKind::Protocol, "invalid OpenClaw HTTP URL"))?;
        if !matches!(base.scheme(), "http" | "https")
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(fail(
                FailureKind::Protocol,
                "expected HTTP gateway base URL without credentials, query or fragment",
            )
            .into());
        }
        let prefix = base
            .path()
            .trim_end_matches('/')
            .trim_end_matches("/v1")
            .to_owned();
        base.set_path(&format!("{prefix}/v1/models"));
        let client = reqwest::blocking::Client::builder()
            .timeout(remaining(deadline)?)
            .connect_timeout(remaining(deadline)?)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|error| fail(FailureKind::Transport, format!("HTTP client: {error}")))?;
        // The authenticated, read-only route proves this instance's HTTP API is
        // enabled without creating a turn. Repeated for every insertion.
        let probe = client
            .get(base.clone())
            .bearer_auth(token)
            .timeout(remaining(probe_deadline(deadline))?)
            .send()
            .map_err(|error| Attempt::before_dispatch(http_error(&error)))?;
        http_status(probe.status().as_u16())?;
        let inventory: Value = probe
            .json()
            .map_err(|error| Attempt::before_dispatch(http_error(&error)))?;
        if !inventory["error"].is_null()
            || !inventory["data"].as_array().is_some_and(|models| {
                let target = format!("openclaw/{agent}");
                models.iter().any(|model| model["id"] == target)
            })
        {
            return Err(fail(
                FailureKind::Protocol,
                "HTTP model probe did not advertise the configured agent",
            )
            .into());
        }
        base.set_path(&format!("{prefix}/v1/chat/completions"));
        let response = client
            .post(base)
            .bearer_auth(token)
            .header("x-openclaw-session-key", key)
            .timeout(remaining(deadline)?)
            .json(&json!({"model":format!("openclaw/{agent}"), "stream":false,
                "messages":[{"role":"user","content":message}]}))
            .send()
            .map_err(|error| {
                if error.is_connect() {
                    Attempt::before_dispatch(http_error(&error))
                } else {
                    Attempt::Stop(http_error(&error))
                }
            })?;
        // Explicitly unsupported endpoint is safe; all other rejected or lost
        // responses stop selection, including HTTP 5xx and read deadlines.
        http_status(response.status().as_u16())?;
        let body: Value = response.json().map_err(|error| http_error(&error))?;
        if !body["error"].is_null() {
            return Err(fail(
                FailureKind::Rejected,
                "HTTP completion returned an error payload",
            )
            .into());
        }
        let choices = body["choices"]
            .as_array()
            .filter(|choices| !choices.is_empty())
            .ok_or_else(|| fail(FailureKind::Protocol, "HTTP completion has no choices"))?;
        for choice in choices {
            match choice["finish_reason"].as_str() {
                Some("stop" | "length" | "tool_calls" | "function_call") => {}
                Some("timeout") => {
                    return Err(fail(FailureKind::Timeout, "HTTP turn timed out").into());
                }
                Some(_) => {
                    return Err(fail(
                        FailureKind::Rejected,
                        "HTTP turn did not complete successfully",
                    )
                    .into());
                }
                None => {
                    return Err(fail(
                        FailureKind::Protocol,
                        "HTTP completion has no finish_reason",
                    )
                    .into());
                }
            }
        }
        Ok(())
    }

    fn cli(&self, message: &str, token: Option<&str>, deadline: Instant) -> Result<()> {
        let endpoint = self.cli_endpoint()?;
        let probe_deadline = probe_deadline(deadline);
        let milliseconds = remaining(probe_deadline)?.as_millis().max(1).to_string();
        let probe = run_cli(
            &["health", "--json", "--timeout", &milliseconds],
            endpoint.as_deref(),
            token,
            probe_deadline,
        )?;
        let health: Value = serde_json::from_str(&probe)
            .map_err(|_| fail(FailureKind::Protocol, "openclaw health did not return JSON"))?;
        if health["ok"] != true {
            return Err(fail(
                FailureKind::Unavailable,
                "OpenClaw CLI gateway is not ready",
            ));
        }
        let message_arg = format!("--message={message}");
        let mut args = vec![
            "agent",
            "--session-id",
            &self.session_id,
            &message_arg,
            "--json",
        ];
        if let Some(agent) = &self.config.agent_id {
            args.extend(["--agent", agent]);
        }
        let output = run_cli(&args, endpoint.as_deref(), token, deadline)?;
        let body: Value = serde_json::from_str(&output)
            .map_err(|_| fail(FailureKind::Protocol, "openclaw agent did not return JSON"))?;
        if body["status"].is_string() {
            check_turn(&body)
        } else if !body["error"].is_null() || !body["meta"]["error"].is_null() {
            Err(fail(
                FailureKind::Rejected,
                "OpenClaw CLI returned an error payload",
            ))
        } else {
            // Older gateway CLI JSON is the completed result itself. Its exit
            // status, checked by run_cli, is the authoritative completion signal.
            Ok(())
        }
    }

    fn cli_endpoint(&self) -> Result<Option<String>> {
        if let Some(url) = &self.config.ws_url {
            return Ok(Some(url.clone()));
        }
        self.config
            .http_url
            .as_deref()
            .map(|base| {
                let mut url = reqwest::Url::parse(base)
                    .map_err(|_| fail(FailureKind::Protocol, "invalid CLI gateway URL"))?;
                let scheme = match url.scheme() {
                    "http" => "ws",
                    "https" => "wss",
                    _ => {
                        return Err(fail(
                            FailureKind::Protocol,
                            "invalid CLI gateway URL scheme",
                        ));
                    }
                };
                url.set_scheme(scheme)
                    .map_err(|()| fail(FailureKind::Protocol, "invalid CLI gateway URL"))?;
                let path = url
                    .path()
                    .trim_end_matches('/')
                    .strip_suffix("/v1")
                    .unwrap_or(url.path().trim_end_matches('/'))
                    .to_owned();
                url.set_path(&path);
                Ok(url.to_string())
            })
            .transpose()
    }
}

fn fail(kind: FailureKind, message: impl Into<String>) -> Error {
    failure("openclaw", kind, message)
}

fn secret(name: Option<&str>) -> Result<Option<String>> {
    name.map(|name| {
        std::env::var(name)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                fail(
                    FailureKind::Authentication,
                    format!("credential environment variable {name} is missing or empty"),
                )
            })
    })
    .transpose()
}

fn device_key(seed: &str) -> Result<SigningKey> {
    let decoded = URL_SAFE_NO_PAD.decode(seed).map_err(|_| {
        fail(
            FailureKind::Authentication,
            "device_seed_env must contain a base64url-without-padding Ed25519 seed",
        )
    })?;
    let seed: [u8; 32] = decoded.try_into().map_err(|_| {
        fail(
            FailureKind::Authentication,
            "device_seed_env must decode to exactly 32 bytes",
        )
    })?;
    Ok(SigningKey::from_bytes(&seed))
}

fn remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| {
            fail(
                FailureKind::Timeout,
                "OpenClaw delivery deadline exceeded; admission may be uncertain",
            )
        })
}

fn probe_deadline(deadline: Instant) -> Instant {
    deadline.min(Instant::now() + Duration::from_secs(3))
}

#[derive(Debug)]
enum Attempt {
    Unavailable(String),
    ProbeTimeout(Error),
    Stop(Error),
}

type AttemptResult = std::result::Result<(), Attempt>;

impl From<Error> for Attempt {
    fn from(error: Error) -> Self {
        Self::Stop(error)
    }
}

impl Attempt {
    fn before_dispatch(error: Error) -> Self {
        match error {
            Error::Delivery {
                kind: FailureKind::Transport | FailureKind::Unavailable,
                message,
                ..
            } => Self::Unavailable(message),
            error @ Error::Delivery {
                kind: FailureKind::Timeout,
                ..
            } => Self::ProbeTimeout(error),
            error => Self::Stop(error),
        }
    }
}

fn operation_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "llm-sessions-{}-{stamp}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

// Each underlying read/write resets its socket timeout to the *remaining*
// common deadline, including TLS negotiation and fragmented WebSocket frames.
#[derive(Debug)]
struct DeadlineStream {
    stream: TcpStream,
    deadline: Instant,
}

impl DeadlineStream {
    fn budget(&self) -> std::io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "delivery deadline exceeded")
            })
    }
}

impl Read for DeadlineStream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.stream.set_read_timeout(Some(self.budget()?))?;
        self.stream.read(buffer)
    }
}

impl Write for DeadlineStream {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.stream.set_write_timeout(Some(self.budget()?))?;
        self.stream.write(buffer)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.set_write_timeout(Some(self.budget()?))?;
        self.stream.flush()
    }
}

fn connect(host: &str, port: u16, deadline: Instant) -> Result<TcpStream> {
    let addresses = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![std::net::SocketAddr::new(ip, port)]
    } else {
        let (tx, rx) = mpsc::sync_channel(1);
        let host = host.to_owned();
        std::thread::spawn(move || {
            let _ = tx.send(
                (host.as_str(), port)
                    .to_socket_addrs()
                    .map(Iterator::collect::<Vec<_>>),
            );
        });
        rx.recv_timeout(remaining(deadline)?)
            .map_err(|_| {
                fail(
                    FailureKind::Timeout,
                    "WebSocket DNS resolution deadline exceeded",
                )
            })?
            .map_err(|error| fail(FailureKind::Transport, format!("WebSocket DNS: {error}")))?
    };
    let mut last = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, remaining(deadline)?) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
    }
    Err(last.map_or_else(
        || {
            fail(
                FailureKind::Unavailable,
                "WebSocket hostname resolved to no addresses",
            )
        },
        |error| io_error(&error),
    ))
}

type Socket = WebSocket<MaybeTlsStream<DeadlineStream>>;

fn send_frame(
    socket: &mut Socket,
    frame: &Value,
    max_payload: usize,
    max_buffered_bytes: usize,
) -> Result<()> {
    let text = frame.to_string();
    // Include the largest masked client-frame header in the local buffer bound.
    if text.len() > max_payload || text.len().saturating_add(14) > max_buffered_bytes {
        return Err(fail(
            FailureKind::Rejected,
            "WebSocket request exceeds the gateway's negotiated payload/buffer limit",
        ));
    }
    socket.send(Message::Text(text.into())).map_err(ws_error)
}

fn policy_limit(hello: &Value, name: &str) -> Result<usize> {
    hello["policy"][name]
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            fail(
                FailureKind::Protocol,
                format!("hello has no valid policy.{name}"),
            )
        })
}

fn socket_deadline(socket: &Socket) -> Result<Instant> {
    match socket.get_ref() {
        MaybeTlsStream::Plain(stream) => Ok(stream.deadline),
        MaybeTlsStream::Rustls(stream) => Ok(stream.sock.deadline),
        _ => Err(fail(FailureKind::Protocol, "unsupported TLS stream")),
    }
}

fn read_frame(socket: &mut Socket) -> Result<Value> {
    loop {
        // Buffered frames and ping traffic must not evade the total deadline.
        remaining(socket_deadline(socket)?)?;
        match socket.read().map_err(ws_error)? {
            Message::Text(text) => {
                return serde_json::from_str(text.as_str())
                    .map_err(|_| fail(FailureKind::Protocol, "invalid WebSocket JSON frame"));
            }
            Message::Ping(_) => socket.flush().map_err(ws_error)?,
            Message::Pong(_) => {}
            Message::Close(_) => {
                return Err(fail(
                    FailureKind::Transport,
                    "WebSocket closed; admission may be uncertain",
                ));
            }
            _ => {
                return Err(fail(
                    FailureKind::Protocol,
                    "unexpected non-text WebSocket frame",
                ));
            }
        }
    }
}

fn response(socket: &mut Socket, id: &str) -> Result<Value> {
    loop {
        let frame = read_frame(socket)?;
        if frame["type"] == "res" && frame["id"] == id {
            return Ok(frame);
        }
    }
}

fn rpc_payload(frame: &Value) -> Result<&Value> {
    if frame["ok"] != true {
        let code = frame["error"]["code"].as_str().unwrap_or("UNKNOWN");
        let detail = frame["error"]["message"]
            .as_str()
            .unwrap_or("request refused");
        let detail_code = frame["error"]["details"]["code"].as_str();
        let text = match detail_code {
            Some(detail_code) => format!("{code}/{detail_code}: {detail}"),
            None => format!("{code}: {detail}"),
        };
        let upper = match detail_code {
            Some(detail_code) => format!("{code}/{detail_code}").to_ascii_uppercase(),
            None => text.to_ascii_uppercase(),
        };
        let kind = if [
            "AUTH",
            "TOKEN",
            "PAIR",
            "SCOPE",
            "FORBIDDEN",
            "UNAUTHORIZED",
            "SIGNATURE",
            "DEVICE",
        ]
        .iter()
        .any(|s| upper.contains(s))
        {
            FailureKind::Authentication
        } else if upper.contains("TIMEOUT") {
            FailureKind::Timeout
        } else {
            FailureKind::Rejected
        };
        return Err(fail(kind, text));
    }
    frame
        .get("payload")
        .ok_or_else(|| fail(FailureKind::Protocol, "RPC response has no payload"))
}

fn check_turn(payload: &Value) -> Result<()> {
    if !payload["error"].is_null() {
        return Err(fail(
            FailureKind::Rejected,
            "agent returned an error payload",
        ));
    }
    match payload["status"].as_str() {
        Some("accepted" | "started" | "in_flight")
            if payload["runId"].as_str().is_some_and(|id| !id.is_empty()) =>
        {
            Ok(())
        }
        Some("ok" | "completed") => Ok(()),
        Some("timeout") => Err(fail(FailureKind::Timeout, "agent turn timed out")),
        Some("error" | "failed" | "cancelled" | "canceled" | "rejected") => Err(fail(
            FailureKind::Rejected,
            "agent turn was rejected or failed",
        )),
        _ => Err(fail(
            FailureKind::Protocol,
            "agent response has no recognized run/status admission",
        )),
    }
}

fn io_error(error: &std::io::Error) -> Error {
    let kind = if matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ) {
        FailureKind::Timeout
    } else {
        FailureKind::Transport
    };
    fail(kind, format!("OpenClaw socket: {error}"))
}

fn ws_error(error: tungstenite::Error) -> Error {
    match error {
        tungstenite::Error::Io(error) => io_error(&error),
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => fail(
            FailureKind::Transport,
            "WebSocket disconnected; admission may be uncertain",
        ),
        error => fail(FailureKind::Protocol, format!("WebSocket: {error}")),
    }
}

fn http_error(error: &reqwest::Error) -> Error {
    let kind = if error.is_timeout() {
        FailureKind::Timeout
    } else if error.is_decode() {
        FailureKind::Protocol
    } else {
        FailureKind::Transport
    };
    fail(kind, format!("OpenClaw HTTP: {error}"))
}

fn http_status(status: u16) -> AttemptResult {
    match status {
        200..=299 => Ok(()),
        404 | 405 | 501 => Err(Attempt::Unavailable(format!(
            "HTTP endpoint unsupported ({status})"
        ))),
        401 | 403 => Err(fail(
            FailureKind::Authentication,
            format!("HTTP authentication/scope refused ({status})"),
        )
        .into()),
        408 | 504 => Err(fail(
            FailureKind::Timeout,
            format!("HTTP deadline failure ({status})"),
        )
        .into()),
        _ => Err(fail(
            FailureKind::Rejected,
            format!("HTTP request refused ({status})"),
        )
        .into()),
    }
}

fn run_cli(
    args: &[&str],
    endpoint: Option<&str>,
    token: Option<&str>,
    deadline: Instant,
) -> Result<String> {
    let mut command = Command::new("openclaw");
    command.args(args);
    if let Some(endpoint) = endpoint {
        command.env("OPENCLAW_GATEWAY_URL", endpoint);
    }
    if let Some(token) = token {
        command.env("OPENCLAW_GATEWAY_TOKEN", token);
    }
    run_process(command, deadline)
}

fn run_process(mut command: Command, deadline: Instant) -> Result<String> {
    remaining(deadline)?;
    let mut log = tempfile::tempfile()
        .map_err(|error| fail(FailureKind::Process, format!("capture CLI output: {error}")))?;
    let stdout = log
        .try_clone()
        .map_err(|error| fail(FailureKind::Process, error.to_string()))?;
    let stderr =
        tempfile::tempfile().map_err(|error| fail(FailureKind::Process, error.to_string()))?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(
            stderr
                .try_clone()
                .map_err(|error| fail(FailureKind::Process, error.to_string()))?,
        )
        .spawn()
        .map_err(|error| {
            fail(
                if error.kind() == std::io::ErrorKind::NotFound {
                    FailureKind::Unavailable
                } else {
                    FailureKind::Process
                },
                format!("cannot start openclaw: {error}"),
            )
        })?;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => match remaining(deadline) {
                Ok(left) => std::thread::sleep(left.min(Duration::from_millis(20))),
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
            },
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(fail(
                    FailureKind::Process,
                    format!("cannot wait for openclaw: {error}"),
                ));
            }
        }
    };
    if !status.success() {
        let mut stderr = stderr;
        let mut output = String::new();
        let _ = stderr
            .seek(SeekFrom::Start(0))
            .and_then(|_| stderr.read_to_string(&mut output));
        let upper = output.to_ascii_uppercase();
        let kind = if [
            "AUTH",
            "TOKEN",
            "PAIR",
            "FORBIDDEN",
            "UNAUTHORIZED",
            "SCOPE",
        ]
        .iter()
        .any(|s| upper.contains(s))
        {
            FailureKind::Authentication
        } else if upper.contains("TIMEOUT") || upper.contains("TIMED OUT") {
            FailureKind::Timeout
        } else {
            FailureKind::Process
        };
        return Err(fail(
            kind,
            format!("openclaw exited {status}: {}", output.trim()),
        ));
    }
    let mut output = String::new();
    log.seek(SeekFrom::Start(0))
        .and_then(|_| log.read_to_string(&mut output))
        .map_err(|error| fail(FailureKind::Process, format!("read CLI output: {error}")))?;
    Ok(output)
}

#[cfg(test)]
#[path = "openclaw/tests.rs"]
mod tests;
