use super::*;
use std::net::TcpListener;
use std::thread::{self, JoinHandle};

use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};

const TIMEOUT: Duration = Duration::from_secs(2);
const SESSION: &str = "11111111-2222-4333-8444-555555555555";

fn harness(ws: Option<String>, http: Option<String>) -> OpenClawHarness {
    OpenClawHarness {
        session_id: SESSION.into(),
        config: OpenClawConfig {
            ws_url: ws,
            http_url: http,
            agent_id: Some("research".into()),
            session_key: Some("agent:research:main".into()),
            ..OpenClawConfig::default()
        },
    }
}

fn deliver(harness: &OpenClawHarness) -> Result<()> {
    harness.deliver_resolved("hello\nworld", TIMEOUT, Some("secret-token"), None)
}

fn assert_kind(result: Result<()>, expected: FailureKind) {
    match result {
        Err(Error::Delivery { harness, kind, .. }) => {
            assert_eq!(harness, "openclaw");
            assert_eq!(kind, expected);
        }
        other => panic!("expected {expected:?}, got {other:?}"),
    }
}

fn listener() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").unwrap()
}

fn sentinel() -> (TcpListener, String) {
    let listener = listener();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (listener, url)
}

fn untouched(listener: &TcpListener) {
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
}

fn ws_server(
    handler: impl FnOnce(tungstenite::WebSocket<TcpStream>) + Send + 'static,
) -> (String, JoinHandle<()>) {
    let listener = listener();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let handle = serve_ws(listener, handler);
    (url, handle)
}

fn serve_ws(
    listener: TcpListener,
    handler: impl FnOnce(tungstenite::WebSocket<TcpStream>) + Send + 'static,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        stream.set_write_timeout(Some(TIMEOUT)).unwrap();
        handler(tungstenite::accept(stream).unwrap());
    })
}

fn receive(socket: &mut tungstenite::WebSocket<TcpStream>) -> Value {
    let message = socket.read().unwrap();
    serde_json::from_str(message.to_text().unwrap()).unwrap()
}

fn send(socket: &mut tungstenite::WebSocket<TcpStream>, frame: &Value) {
    socket
        .send(Message::Text(frame.to_string().into()))
        .unwrap();
}

fn challenge(socket: &mut tungstenite::WebSocket<TcpStream>) -> Value {
    send(
        socket,
        &json!({"type":"event","event":"connect.challenge",
    "payload":{"nonce":"challenge-nonce","ts":1_720_000_000_000_u64}}),
    );
    let connect = receive(socket);
    assert_eq!(connect["method"], "connect");
    assert_eq!(connect["params"]["minProtocol"], 4);
    assert_eq!(connect["params"]["auth"]["token"], "secret-token");
    connect
}

fn hello(socket: &mut tungstenite::WebSocket<TcpStream>, connect: &Value, methods: &Value) {
    send(
        socket,
        &json!({"type":"res","id":connect["id"],"ok":true,"payload":{
            "type":"hello-ok","protocol":4,"features":{"methods":methods},
            "auth":{"role":"operator","scopes":["operator.read","operator.write"]},
            "policy":{"maxPayload":26_214_400,"maxBufferedBytes":52_428_800,"tickIntervalMs":15_000}
        }}),
    );
}

fn accepted(socket: &mut tungstenite::WebSocket<TcpStream>) {
    let turn = receive(socket);
    assert_eq!(turn["method"], "agent");
    assert_eq!(turn["params"]["sessionId"], SESSION);
    assert_eq!(turn["params"]["agentId"], "research");
    assert_eq!(turn["params"]["message"], "hello\nworld");
    assert_eq!(turn["params"]["createSessionIfMissing"], false);
    assert_eq!(turn["params"]["deliver"], false);
    assert_eq!(turn["params"]["idempotencyKey"], turn["id"]);
    assert!(turn["params"].get("sessionKey").is_none());
    // Neither unrelated events nor another request's rejection is our result.
    send(socket, &json!({"type":"res","id":"unrelated","ok":false}));
    send(socket, &json!({"type":"event","event":"tick","payload":{}}));
    send(
        socket,
        &json!({"type":"res","id":turn["id"],"ok":true,
    "payload":{"runId":"accepted-run","status":"accepted"}}),
    );
}

#[derive(Debug)]
struct HttpRequest {
    first_line: String,
    headers: String,
    body: Value,
}

fn http_server(responses: Vec<(u16, Value)>) -> (String, JoinHandle<Vec<HttpRequest>>) {
    let listener = listener();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        responses.into_iter().map(|(status, body)| {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(TIMEOUT)).unwrap();
            stream.set_write_timeout(Some(TIMEOUT)).unwrap();
            let request = read_http(&mut stream);
            let body = body.to_string();
            write!(stream, "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            request
        }).collect()
    });
    (url, handle)
}

fn read_http(stream: &mut TcpStream) -> HttpRequest {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
    }
    let headers = String::from_utf8(bytes).unwrap();
    let length = headers
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    HttpRequest {
        first_line: headers.lines().next().unwrap().to_owned(),
        headers: headers.to_ascii_lowercase(),
        body: if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).unwrap()
        },
    }
}

fn inventory() -> (u16, Value) {
    (
        200,
        json!({"object":"list","data":[{"id":"openclaw/research"}]}),
    )
}

fn completion() -> (u16, Value) {
    (
        200,
        json!({"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}),
    )
}

fn assert_http(requests: &[HttpRequest], message: &str) {
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].first_line, "GET /v1/models HTTP/1.1");
    assert_eq!(requests[1].first_line, "POST /v1/chat/completions HTTP/1.1");
    for request in requests {
        assert!(
            request
                .headers
                .contains("authorization: bearer secret-token\r\n")
        );
    }
    assert!(
        requests[1]
            .headers
            .contains("x-openclaw-session-key: agent:research:main\r\n")
    );
    assert_eq!(
        requests[1].body,
        json!({"model":"openclaw/research","stream":false,
        "messages":[{"role":"user","content":message}]})
    );
}

#[test]
fn websocket_challenge_authentication_and_exact_correlated_turn() {
    let (sentinel, http) = sentinel();
    let seed = [42_u8; 32];
    let (ws, server) = ws_server(move |mut socket| {
        let connect = challenge(&mut socket);
        assert_eq!(connect["params"]["client"]["id"], "cli");
        assert_eq!(connect["params"]["client"]["mode"], "cli");
        let device = &connect["params"]["device"];
        assert_eq!(device["signedAt"], 1_720_000_000_000_u64);
        assert_eq!(device["nonce"], "challenge-nonce");
        let public: [u8; 32] = URL_SAFE_NO_PAD
            .decode(device["publicKey"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(
            public,
            SigningKey::from_bytes(&seed).verifying_key().to_bytes()
        );
        let device_id = format!("{:x}", Sha256::digest(public));
        assert_eq!(device["id"], device_id);
        let signature = Signature::from_slice(
            &URL_SAFE_NO_PAD
                .decode(device["signature"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        let payload = format!(
            "v3|{device_id}|cli|cli|operator|operator.read,operator.write|1720000000000|secret-token|challenge-nonce|linux|"
        );
        VerifyingKey::from_bytes(&public)
            .unwrap()
            .verify(payload.as_bytes(), &signature)
            .unwrap();
        hello(&mut socket, &connect, &json!(["agent"]));
        accepted(&mut socket);
    });
    harness(Some(ws), Some(http))
        .deliver_resolved(
            "hello\nworld",
            TIMEOUT,
            Some("secret-token"),
            Some(&URL_SAFE_NO_PAD.encode(seed)),
        )
        .unwrap();
    server.join().unwrap();
    untouched(&sentinel);
}

#[test]
fn direct_loopback_shared_token_uses_reserved_backend_identity() {
    let (ws, server) = ws_server(|mut socket| {
        let connect = challenge(&mut socket);
        assert_eq!(connect["params"]["client"]["id"], "gateway-client");
        assert_eq!(connect["params"]["client"]["mode"], "backend");
        assert!(connect["params"].get("device").is_none());
        hello(&mut socket, &connect, &json!(["agent"]));
        accepted(&mut socket);
    });
    deliver(&harness(Some(ws), None)).unwrap();
    server.join().unwrap();
}

#[test]
fn websocket_down_falls_back_to_authenticated_http() {
    let unavailable = listener();
    let ws = format!("ws://{}", unavailable.local_addr().unwrap());
    drop(unavailable);
    let (http, server) = http_server(vec![inventory(), completion()]);
    deliver(&harness(Some(ws), Some(http))).unwrap();
    assert_http(&server.join().unwrap(), "hello\nworld");
}

#[test]
fn http_only_instances_probe_their_own_gateway_each_time() {
    let (a, a_server) = http_server(vec![inventory(), completion(), inventory(), completion()]);
    let (b, b_server) = http_server(vec![inventory(), completion()]);
    let first = harness(None, Some(a));
    let second = harness(None, Some(format!("{b}/v1")));
    deliver(&first).unwrap();
    deliver(&second).unwrap();
    deliver(&first).unwrap();
    let a_requests = a_server.join().unwrap();
    assert_http(&a_requests[..2], "hello\nworld");
    assert_http(&a_requests[2..], "hello\nworld");
    assert_http(&b_server.join().unwrap(), "hello\nworld");
}

#[test]
fn previous_http_success_does_not_stick_when_websocket_recovers() {
    let reserved = listener();
    let address = reserved.local_addr().unwrap();
    drop(reserved);
    let (http, http_server) = http_server(vec![inventory(), completion()]);
    let harness = harness(Some(format!("ws://{address}")), Some(http));
    deliver(&harness).unwrap();
    assert_http(&http_server.join().unwrap(), "hello\nworld");
    let ws_server = serve_ws(TcpListener::bind(address).unwrap(), |mut socket| {
        let connect = challenge(&mut socket);
        hello(&mut socket, &connect, &json!(["agent"]));
        accepted(&mut socket);
    });
    deliver(&harness).unwrap();
    ws_server.join().unwrap();
}

#[test]
fn unavailable_websocket_capability_selects_http() {
    let (ws, ws_server) = ws_server(|mut socket| {
        let connect = challenge(&mut socket);
        hello(&mut socket, &connect, &json!(["health"]));
    });
    let (http, http_server) = http_server(vec![inventory(), completion()]);
    deliver(&harness(Some(ws), Some(http))).unwrap();
    ws_server.join().unwrap();
    assert_http(&http_server.join().unwrap(), "hello\nworld");
}

#[test]
fn unavailable_routes_report_typed_failure() {
    assert_kind(deliver(&harness(None, None)), FailureKind::Unavailable);
    let (url, server) = http_server(vec![(404, json!({"error":"disabled"}))]);
    assert_kind(deliver(&harness(None, Some(url))), FailureKind::Unavailable);
    assert_eq!(server.join().unwrap().len(), 1);
    let (ws, server) = ws_server(|mut socket| {
        let connect = challenge(&mut socket);
        hello(&mut socket, &connect, &json!([]));
    });
    assert_kind(deliver(&harness(Some(ws), None)), FailureKind::Unavailable);
    server.join().unwrap();
}

#[test]
fn http_never_invents_session_key_or_agent() {
    let (sentinel, url) = sentinel();
    let mut harness = harness(None, Some(url));
    harness.config.session_key = None;
    assert_kind(deliver(&harness), FailureKind::Unavailable);
    harness.config.session_key = Some("agent:research:main".into());
    harness.config.agent_id = None;
    assert_kind(deliver(&harness), FailureKind::Unavailable);
    untouched(&sentinel);
}

#[test]
fn http_auth_and_rejection_never_try_another_transport() {
    for status in [401, 403, 409, 429, 500] {
        let (url, server) = http_server(vec![(status, json!({"error":"refused"}))]);
        let expected = if status == 401 || status == 403 {
            FailureKind::Authentication
        } else {
            FailureKind::Rejected
        };
        assert_kind(deliver(&harness(None, Some(url))), expected);
        assert_eq!(server.join().unwrap().len(), 1);
    }
    for body in [
        json!({"error":{"message":"failed"}}),
        json!({"choices":[{"finish_reason":"error"}]}),
        json!({"choices":[{"finish_reason":"cancelled"}]}),
    ] {
        let (url, server) = http_server(vec![inventory(), (200, body)]);
        assert_kind(deliver(&harness(None, Some(url))), FailureKind::Rejected);
        assert_eq!(server.join().unwrap().len(), 2);
    }
}

#[test]
fn websocket_auth_refusal_and_terminal_turn_status_do_not_fallback() {
    for status in ["auth", "error", "timeout"] {
        let (sentinel, http) = sentinel();
        let (ws, server) = ws_server(move |mut socket| {
            let connect = challenge(&mut socket);
            if status == "auth" {
                send(
                    &mut socket,
                    &json!({"type":"res","id":connect["id"],"ok":false,
                "error":{"code":"NOT_PAIRED","message":"device pairing required"}}),
                );
                return;
            }
            hello(&mut socket, &connect, &json!(["agent"]));
            let turn = receive(&mut socket);
            send(
                &mut socket,
                &json!({"type":"res","id":turn["id"],"ok":true,
            "payload":{"runId":"run","status":status}}),
            );
        });
        let expected = match status {
            "auth" => FailureKind::Authentication,
            "timeout" => FailureKind::Timeout,
            _ => FailureKind::Rejected,
        };
        assert_kind(deliver(&harness(Some(ws), Some(http))), expected);
        server.join().unwrap();
        untouched(&sentinel);
    }
}

#[test]
fn websocket_post_dispatch_disconnect_never_falls_back() {
    let (sentinel, http) = sentinel();
    let (ws, server) = ws_server(|mut socket| {
        let connect = challenge(&mut socket);
        hello(&mut socket, &connect, &json!(["agent"]));
        let turn = receive(&mut socket);
        assert_eq!(turn["method"], "agent");
        socket.close(None).unwrap();
    });
    assert_kind(
        deliver(&harness(Some(ws), Some(http))),
        FailureKind::Transport,
    );
    server.join().unwrap();
    untouched(&sentinel);
}

#[test]
fn websocket_post_dispatch_timeout_never_falls_back() {
    let (sentinel, http) = sentinel();
    let (ws, server) = ws_server(|mut socket| {
        let connect = challenge(&mut socket);
        hello(&mut socket, &connect, &json!(["agent"]));
        assert_eq!(receive(&mut socket)["method"], "agent");
        thread::sleep(Duration::from_millis(200));
    });
    assert_kind(
        harness(Some(ws), Some(http)).deliver_resolved(
            "hello\nworld",
            Duration::from_millis(100),
            Some("secret-token"),
            None,
        ),
        FailureKind::Timeout,
    );
    server.join().unwrap();
    untouched(&sentinel);
}

#[test]
fn http_post_dispatch_timeout_does_not_retry_the_turn() {
    let listener = listener();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        assert_eq!(read_http(&mut stream).first_line, "GET /v1/models HTTP/1.1");
        let body = inventory().1.to_string();
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        drop(stream);
        let (mut stream, _) = listener.accept().unwrap();
        assert_eq!(
            read_http(&mut stream).first_line,
            "POST /v1/chat/completions HTTP/1.1"
        );
        thread::sleep(Duration::from_millis(200));
        listener.set_nonblocking(true).unwrap();
        untouched(&listener);
    });
    assert_kind(
        harness(None, Some(url)).deliver_resolved(
            "hello\nworld",
            Duration::from_millis(100),
            Some("secret-token"),
            None,
        ),
        FailureKind::Timeout,
    );
    server.join().unwrap();
}

#[test]
fn malformed_challenge_and_missing_scopes_are_terminal() {
    for malformed in [true, false] {
        let (sentinel, http) = sentinel();
        let (ws, server) = ws_server(move |mut socket| {
            if malformed {
                send(
                    &mut socket,
                    &json!({"type":"event","event":"connect.challenge","payload":{"nonce":"n"}}),
                );
            } else {
                let connect = challenge(&mut socket);
                send(
                    &mut socket,
                    &json!({"type":"res","id":connect["id"],"ok":true,"payload":{
                "type":"hello-ok","protocol":4,"features":{"methods":["agent"]},
                "auth":{"role":"operator","scopes":["operator.read"]}}}),
                );
            }
        });
        assert_kind(
            deliver(&harness(Some(ws), Some(http))),
            if malformed {
                FailureKind::Protocol
            } else {
                FailureKind::Authentication
            },
        );
        server.join().unwrap();
        untouched(&sentinel);
    }
}

#[test]
fn zero_deadline_and_bad_seed_fail_without_dispatch() {
    assert_kind(
        harness(None, None).deliver_resolved("hello", Duration::ZERO, None, None),
        FailureKind::Timeout,
    );
    assert!(device_key("not+base64").is_err());
    assert!(device_key(&URL_SAFE_NO_PAD.encode([0; 64])).is_err());
    assert!(device_key(&URL_SAFE_NO_PAD.encode([0; 32])).is_ok());
    assert!(serde_json::from_value::<OpenClawConfig>(json!({"sessions_send":"invented"})).is_err());
}

#[test]
fn cli_endpoint_is_pinned_to_configured_instance() {
    let harness = harness(
        Some("wss://gateway.example/control".into()),
        Some("https://different.invalid".into()),
    );
    assert_eq!(
        harness.cli_endpoint().unwrap().as_deref(),
        Some("wss://gateway.example/control")
    );
    let harness = self::harness(None, Some("https://gateway.example/prefix/v1/".into()));
    assert_eq!(
        harness.cli_endpoint().unwrap().as_deref(),
        Some("wss://gateway.example/prefix")
    );
}

#[test]
fn colocated_process_deadline_authentication_and_capture() {
    let mut success = Command::new("sh");
    success.args(["-c", "printf '{\"ok\":true}'"]);
    assert_eq!(
        run_process(success, Instant::now() + TIMEOUT).unwrap(),
        "{\"ok\":true}"
    );

    let mut refusal = Command::new("sh");
    refusal.args(["-c", "printf 'pairing required' >&2; exit 1"]);
    assert_kind(
        run_process(refusal, Instant::now() + TIMEOUT).map(|_| ()),
        FailureKind::Authentication,
    );

    let mut stuck = Command::new("sh");
    stuck.args(["-c", "while :; do :; done"]);
    assert_kind(
        run_process(stuck, Instant::now() + Duration::from_millis(40)).map(|_| ()),
        FailureKind::Timeout,
    );
}

#[test]
fn websocket_probe_timeout_has_typed_deadline() {
    let (ws, server) = ws_server(|_socket| {
        thread::sleep(Duration::from_millis(200));
    });
    assert_kind(
        harness(Some(ws), None).deliver_resolved(
            "hello",
            Duration::from_millis(100),
            Some("secret-token"),
            None,
        ),
        FailureKind::Timeout,
    );
    server.join().unwrap();
}

#[test]
fn unsupported_http_post_never_uses_an_invented_route() {
    let (url, server) = http_server(vec![inventory(), (404, json!({"error":"disabled"}))]);
    assert_kind(deliver(&harness(None, Some(url))), FailureKind::Unavailable);
    let requests = server.join().unwrap();
    assert_eq!(requests[0].first_line, "GET /v1/models HTTP/1.1");
    assert_eq!(requests[1].first_line, "POST /v1/chat/completions HTTP/1.1");
}

#[test]
fn missing_credentials_cannot_be_bypassed_with_another_transport() {
    let (sentinel, http) = sentinel();
    let listener = listener();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        let mut byte = [0];
        assert_eq!(stream.read(&mut byte).unwrap(), 0);
    });
    // A hostname resolving to loopback is deliberately not the narrowly
    // reserved literal-loopback backend exception.
    assert_kind(
        deliver(&harness(
            Some(format!("ws://localhost:{port}")),
            Some(http.clone()),
        )),
        FailureKind::Authentication,
    );
    server.join().unwrap();
    untouched(&sentinel);
    assert_kind(
        harness(None, Some(http)).deliver_resolved("hello", TIMEOUT, None, None),
        FailureKind::Authentication,
    );
    untouched(&sentinel);
    assert!(secret(Some("")).is_err());
}

#[test]
fn websocket_payload_policy_rejects_before_dispatch_without_fallback() {
    for limit in ["maxPayload", "maxBufferedBytes"] {
        let (sentinel, http) = sentinel();
        let (ws, server) = ws_server(move |mut socket| {
            let connect = challenge(&mut socket);
            let mut policy = json!({
                "maxPayload":26_214_400,"maxBufferedBytes":52_428_800,"tickIntervalMs":15_000
            });
            policy[limit] = json!(128);
            send(
                &mut socket,
                &json!({"type":"res","id":connect["id"],"ok":true,"payload":{
                    "type":"hello-ok","protocol":4,"features":{"methods":["agent"]},
                    "auth":{"role":"operator","scopes":["operator.write"]},"policy":policy
                }}),
            );
            assert!(socket.read().is_err());
        });
        assert_kind(
            deliver(&harness(Some(ws), Some(http))),
            FailureKind::Rejected,
        );
        server.join().unwrap();
        untouched(&sentinel);
    }
}

#[test]
fn websocket_requires_valid_negotiated_policy_before_dispatch() {
    for policy in [
        Value::Null,
        json!({"maxPayload":0,"maxBufferedBytes":1024}),
        json!({"maxPayload":1024,"maxBufferedBytes":"invalid"}),
    ] {
        let (sentinel, http) = sentinel();
        let (ws, server) = ws_server(move |mut socket| {
            let connect = challenge(&mut socket);
            send(
                &mut socket,
                &json!({"type":"res","id":connect["id"],"ok":true,"payload":{
                    "type":"hello-ok","protocol":4,"features":{"methods":["agent"]},
                    "auth":{"role":"operator","scopes":["operator.write"]},"policy":policy
                }}),
            );
            assert!(socket.read().is_err());
        });
        assert_kind(
            deliver(&harness(Some(ws), Some(http))),
            FailureKind::Protocol,
        );
        server.join().unwrap();
        untouched(&sentinel);
    }
}

#[test]
fn http_probe_requires_selected_agent_without_default_substitution() {
    for body in [
        json!({"data":[{"id":"openclaw"},{"id":"openclaw/default"},{"id":"openclaw/other"}]}),
        json!({"data":[]}),
        json!({"error":"probe failed","data":[{"id":"openclaw/research"}]}),
    ] {
        let (url, server) = http_server(vec![(200, body)]);
        assert_kind(deliver(&harness(None, Some(url))), FailureKind::Protocol);
        assert_eq!(server.join().unwrap().len(), 1);
    }
}

#[test]
fn websocket_structured_auth_failure_is_not_hidden_by_generic_message() {
    let (sentinel, http) = sentinel();
    let (ws, server) = ws_server(|mut socket| {
        let connect = challenge(&mut socket);
        send(
            &mut socket,
            &json!({"type":"res","id":connect["id"],"ok":false,"error":{
                "code":"INVALID_REQUEST","message":"connection refused",
                "details":{"code":"AUTH_SCOPE_MISMATCH"}
            }}),
        );
    });
    assert_kind(
        deliver(&harness(Some(ws), Some(http))),
        FailureKind::Authentication,
    );
    server.join().unwrap();
    untouched(&sentinel);
}

#[test]
fn buffered_websocket_frames_cannot_outlive_delivery_deadline() {
    let listener = listener();
    let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let payload = json!({"type":"res","id":"run","ok":true,"payload":{"status":"ok"}}).to_string();
    assert!(payload.len() < 126);
    let mut bytes = vec![0x81, u8::try_from(payload.len()).unwrap()];
    bytes.extend_from_slice(payload.as_bytes());
    let mut socket = WebSocket::from_partially_read(
        MaybeTlsStream::Plain(DeadlineStream {
            stream,
            deadline: Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
        }),
        bytes,
        tungstenite::protocol::Role::Client,
        None,
    );
    assert_kind(read_frame(&mut socket).map(|_| ()), FailureKind::Timeout);
}
