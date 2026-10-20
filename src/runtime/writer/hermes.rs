//! Instance-bound Hermes API delivery. Admission is not model completion.
//!
//! The runs endpoint is tried anew for each message. Only an explicit unsupported
//! endpoint and advertised session continuity permit synchronous chat fallback;
//! uncertain writes are never retried.

use std::io::{BufRead, BufReader, Read};
use std::time::{Duration, Instant};

use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::{StatusCode, Url};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{SessionHarness, failure, launch};
use crate::error::FailureKind;
use crate::runtime::insert::Delivery;
use crate::{Error, Result};

const HARNESS: &str = "hermes";
const MAX_BODY: u64 = 8 * 1024 * 1024;
const MAX_EVENT: usize = 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HermesConfig {
    /// Gateway root, optionally including a profile prefix or trailing `/v1`.
    pub base_url: String,
    /// Defaults to `API_SERVER_KEY`; resolved for each operation, never persisted.
    pub token_env: Option<String>,
    pub instructions: Option<String>,
}

#[derive(Clone, Debug)]
pub struct HermesHarness {
    pub session_id: String,
    pub config: HermesConfig,
}

impl SessionHarness for HermesHarness {
    fn write_message(&self, message: &str, timeout: Duration) -> Result<Delivery> {
        let harness = self.clone();
        let message = message.to_owned();
        Ok(launch(format!("hermes {}", self.session_id), move || {
            harness.deliver(&message, timeout)
        }))
    }
}

impl HermesHarness {
    /// Submit one turn with the exact selected context. Runs return on admission;
    /// the legacy chat endpoint must finish successfully before returning.
    pub fn deliver(&self, message: &str, timeout: Duration) -> Result<()> {
        let budget = Budget::new(timeout);
        self.deliver_authenticated(message, &self.token()?, &budget)
    }

    /// Observe an already admitted run until its terminal lifecycle event.
    ///
    /// `last_event_id` resumes a previous observation using `Last-Event-ID`.
    /// Token/assistant finalization events and stream closure do not establish
    /// success. A failed observation never submits, retries, or cancels a turn.
    pub fn observe_run(
        &self,
        run_id: &str,
        last_event_id: Option<u64>,
        timeout: Duration,
    ) -> Result<()> {
        let budget = Budget::new(timeout);
        self.observe_authenticated(run_id, last_event_id, &self.token()?, &budget)
    }

    fn token(&self) -> Result<String> {
        let name = self.config.token_env.as_deref().unwrap_or("API_SERVER_KEY");
        std::env::var(name)
            .ok()
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| {
                failure(
                    HARNESS,
                    FailureKind::Authentication,
                    format!("bearer token environment variable {name:?} is missing or empty"),
                )
            })
    }

    fn deliver_authenticated(&self, message: &str, token: &str, budget: &Budget) -> Result<()> {
        if self.session_id.is_empty() || self.session_id.trim() != self.session_id {
            return Err(failure(
                HARNESS,
                FailureKind::Rejected,
                "session ID is empty or has surrounding whitespace",
            ));
        }
        let client = client(token)?;
        let mut payload = json!({"input": message, "session_id": self.session_id});
        if let Some(instructions) = &self.config.instructions {
            payload["instructions"] = json!(instructions);
        }
        let response = client
            .post(endpoint(&self.config.base_url, &["runs"])?)
            .timeout(budget.remaining()?)
            .json(&payload)
            .send()
            .map_err(network_error)?;
        let status = response.status();
        let body = read_body(response, budget)?;
        // These status codes explicitly mean that this protocol is unavailable.
        // A structured session/auth refusal is not endpoint discovery, even if
        // an older server happens to encode it as 404.
        if matches!(status.as_u16(), 404 | 405 | 501) && !context_refusal(&body) {
            self.require_chat_continuity(&client, budget)?;
            return self.chat(&client, message, budget);
        }
        let value = checked_json(status, &body)?;
        check_context(&value, &self.session_id)?;
        check_run_admission(&value)
    }

    fn require_chat_continuity(&self, client: &Client, budget: &Budget) -> Result<()> {
        // Older chat servers can silently ignore the continuity header and start
        // another conversation. Discover support before sending any user input.
        let response = client
            .get(endpoint(&self.config.base_url, &["capabilities"])?)
            .timeout(budget.remaining()?)
            .send()
            .map_err(network_error)?;
        let value = checked_json(response.status(), &read_body(response, budget)?)?;
        let features = &value["features"];
        if features["chat_completions"] != true
            || !features["session_continuity_header"]
                .as_str()
                .is_some_and(|header| header.eq_ignore_ascii_case("X-Hermes-Session-Id"))
        {
            return Err(failure(
                HARNESS,
                FailureKind::Unavailable,
                "chat fallback requires advertised X-Hermes-Session-Id continuity",
            ));
        }
        Ok(())
    }

    fn chat(&self, client: &Client, message: &str, budget: &Budget) -> Result<()> {
        let mut messages = Vec::with_capacity(2);
        if let Some(instructions) = &self.config.instructions {
            messages.push(json!({"role": "system", "content": instructions}));
        }
        messages.push(json!({"role": "user", "content": message}));
        let session = HeaderValue::from_str(&self.session_id).map_err(|_| {
            failure(
                HARNESS,
                FailureKind::Rejected,
                "session ID is not a valid HTTP header",
            )
        })?;
        let response = client
            .post(endpoint(&self.config.base_url, &["chat", "completions"])?)
            .header("X-Hermes-Session-Id", session)
            .timeout(budget.remaining()?)
            .json(&json!({"model": "hermes-agent", "messages": messages, "stream": false}))
            .send()
            .map_err(network_error)?;
        let status = response.status();
        if response
            .headers()
            .get("X-Hermes-Session-Id")
            .is_some_and(|id| id.as_bytes() != self.session_id.as_bytes())
        {
            return Err(failure(
                HARNESS,
                FailureKind::Protocol,
                "chat response changed session context",
            ));
        }
        let value = checked_json(status, &read_body(response, budget)?)?;
        check_context(&value, &self.session_id)?;
        let choices = value["choices"]
            .as_array()
            .filter(|choices| !choices.is_empty())
            .ok_or_else(|| {
                failure(
                    HARNESS,
                    FailureKind::Protocol,
                    "chat response has no choices",
                )
            })?;
        for choice in choices {
            if has_error(choice) {
                return Err(failure(
                    HARNESS,
                    FailureKind::Rejected,
                    "chat choice contains an error",
                ));
            }
            match choice["finish_reason"].as_str() {
                Some("stop") => {}
                Some(reason) => {
                    return Err(failure(
                        HARNESS,
                        FailureKind::Rejected,
                        format!("chat did not complete successfully: {reason}"),
                    ));
                }
                None => {
                    return Err(failure(
                        HARNESS,
                        FailureKind::Protocol,
                        "chat choice has no finish_reason",
                    ));
                }
            }
        }
        Ok(())
    }

    fn observe_authenticated(
        &self,
        run_id: &str,
        last_event_id: Option<u64>,
        token: &str,
        budget: &Budget,
    ) -> Result<()> {
        if run_id.trim().is_empty() {
            return Err(failure(HARNESS, FailureKind::Rejected, "empty run ID"));
        }
        let mut request = client(token)?
            .get(endpoint(
                &self.config.base_url,
                &["runs", run_id, "events"],
            )?)
            .header(ACCEPT, "text/event-stream")
            .timeout(budget.remaining()?);
        if let Some(id) = last_event_id {
            request = request.header("Last-Event-ID", id.to_string());
        }
        let response = request.send().map_err(network_error)?;
        if !response.status().is_success() {
            return checked_json(response.status(), &read_body(response, budget)?).map(|_| ());
        }
        if !response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("text/event-stream"))
            })
        {
            return Err(failure(
                HARNESS,
                FailureKind::Protocol,
                "run events response is not text/event-stream",
            ));
        }
        observe_events(response, run_id, budget)
    }
}

struct Budget {
    started: Instant,
    timeout: Duration,
}

impl Budget {
    fn new(timeout: Duration) -> Self {
        Self {
            started: Instant::now(),
            timeout,
        }
    }

    fn remaining(&self) -> Result<Duration> {
        self.timeout
            .checked_sub(self.started.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| {
                failure(
                    HARNESS,
                    FailureKind::Timeout,
                    "delivery deadline expired; admission may be unknown",
                )
            })
    }
}

fn client(token: &str) -> Result<Client> {
    if token.trim().is_empty() {
        return Err(failure(
            HARNESS,
            FailureKind::Authentication,
            "empty bearer token",
        ));
    }
    let mut authorization = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
        failure(
            HARNESS,
            FailureKind::Authentication,
            "bearer token is not a valid HTTP header",
        )
    })?;
    authorization.set_sensitive(true);
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, authorization);
    Client::builder()
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none())
        // Even a transport-level automatic retry would risk a second admission.
        .retry(reqwest::retry::never())
        .build()
        .map_err(network_error)
}

fn endpoint(base_url: &str, segments: &[&str]) -> Result<Url> {
    let invalid = || {
        failure(
            HARNESS,
            FailureKind::Protocol,
            "base_url must be an HTTP(S) gateway URL without credentials, query, or fragment",
        )
    };
    let mut url = Url::parse(base_url).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    let has_v1 = url.path().trim_end_matches('/').ends_with("/v1");
    {
        let mut path = url.path_segments_mut().map_err(|()| invalid())?;
        path.pop_if_empty();
        if !has_v1 {
            path.push("v1");
        }
        for segment in segments {
            path.push(segment);
        }
    }
    Ok(url)
}

fn network_error(error: reqwest::Error) -> Error {
    let kind = if error.is_timeout() {
        FailureKind::Timeout
    } else if error.is_connect() {
        FailureKind::Unavailable
    } else {
        FailureKind::Transport
    };
    failure(
        HARNESS,
        kind,
        format!(
            "HTTP request failed; admission may be unknown: {}",
            error.without_url()
        ),
    )
}

fn read_error(error: &std::io::Error, budget: &Budget) -> Error {
    let kind = if error.kind() == std::io::ErrorKind::TimedOut || budget.remaining().is_err() {
        FailureKind::Timeout
    } else {
        FailureKind::Transport
    };
    failure(
        HARNESS,
        kind,
        format!("response read failed; admission may be unknown: {error}"),
    )
}

fn read_body(response: Response, budget: &Budget) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    response
        .take(MAX_BODY + 1)
        .read_to_end(&mut body)
        .map_err(|error| read_error(&error, budget))?;
    budget.remaining()?;
    if body.len() as u64 > MAX_BODY {
        return Err(failure(
            HARNESS,
            FailureKind::Protocol,
            "HTTP response exceeds 8 MiB",
        ));
    }
    Ok(body)
}

fn context_refusal(body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    // A structured refusal is authoritative unless it explicitly describes a
    // missing route. Never bypass an unfamiliar policy/auth/session error merely
    // because a proxy or older server encodes it as HTTP 404/405/501.
    has_error(&value)
        && !matches!(
            value["error"]["code"].as_str(),
            Some(
                "unsupported_endpoint"
                    | "endpoint_not_found"
                    | "route_not_found"
                    | "method_not_allowed"
                    | "not_implemented"
            )
        )
}

fn checked_json(status: StatusCode, body: &[u8]) -> Result<Value> {
    if !status.is_success() {
        let value = serde_json::from_slice::<Value>(body).ok();
        let code = value
            .as_ref()
            .and_then(|value| value["error"]["code"].as_str());
        let kind = if matches!(status.as_u16(), 401 | 403) {
            FailureKind::Authentication
        } else if matches!(status.as_u16(), 404 | 405 | 501) && !context_refusal(body) {
            FailureKind::Unavailable
        } else {
            FailureKind::Rejected
        };
        return Err(failure(
            HARNESS,
            kind,
            format!(
                "HTTP {status}{}",
                code.map_or_else(String::new, |code| format!(": {code}"))
            ),
        ));
    }
    let value: Value = serde_json::from_slice(body).map_err(|error| {
        failure(
            HARNESS,
            FailureKind::Protocol,
            format!("invalid JSON response: {error}"),
        )
    })?;
    if has_error(&value) {
        return Err(failure(
            HARNESS,
            FailureKind::Rejected,
            "gateway returned an error response",
        ));
    }
    Ok(value)
}

fn has_error(value: &Value) -> bool {
    value
        .get("error")
        .is_some_and(|error| !error.is_null() && error != &Value::Bool(false))
}

fn check_context(value: &Value, session_id: &str) -> Result<()> {
    if value
        .get("session_id")
        .is_some_and(|id| id.as_str() != Some(session_id))
    {
        return Err(failure(
            HARNESS,
            FailureKind::Protocol,
            "response changed session context",
        ));
    }
    Ok(())
}

fn check_run_admission(value: &Value) -> Result<()> {
    if value["run_id"].as_str().is_none_or(str::is_empty) {
        return Err(failure(
            HARNESS,
            FailureKind::Protocol,
            "runs response has no run_id",
        ));
    }
    if value["status"] == "completed"
        && (value["completed"] == false || value["partial"] == true || value["interrupted"] == true)
    {
        return Err(failure(
            HARNESS,
            FailureKind::Rejected,
            "completed admission contains an unsuccessful outcome",
        ));
    }
    match value["status"].as_str() {
        Some(
            "started"
            | "accepted"
            | "queued"
            | "running"
            | "in_progress"
            | "waiting_for_approval"
            | "waiting_for_input"
            | "completed",
        ) => Ok(()),
        Some(
            status @ ("error" | "failed" | "cancelled" | "interrupted" | "timeout" | "unknown"),
        ) => Err(failure(
            HARNESS,
            FailureKind::Rejected,
            format!("run admission returned {status}"),
        )),
        _ => Err(failure(
            HARNESS,
            FailureKind::Protocol,
            "runs response has an unknown or missing status",
        )),
    }
}

fn observe_events(response: Response, run_id: &str, budget: &Budget) -> Result<()> {
    let mut reader = BufReader::new(response);
    let mut line = Vec::new();
    let mut data = String::new();
    let mut event_name = String::new();
    loop {
        budget.remaining()?;
        line.clear();
        let count = reader
            .by_ref()
            .take((MAX_EVENT + 1) as u64)
            .read_until(b'\n', &mut line)
            .map_err(|error| read_error(&error, budget))?;
        budget.remaining()?;
        if count == 0 {
            return Err(failure(
                HARNESS,
                FailureKind::Transport,
                "run event stream closed without a terminal lifecycle event",
            ));
        }
        if count > MAX_EVENT || data.len().saturating_add(count) > MAX_EVENT {
            return Err(failure(
                HARNESS,
                FailureKind::Protocol,
                "SSE event exceeds 1 MiB",
            ));
        }
        let line = std::str::from_utf8(&line)
            .map_err(|_| failure(HARNESS, FailureKind::Protocol, "SSE is not UTF-8"))?;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if !data.is_empty() && terminal_event(&data, &event_name, run_id)? {
                return Ok(());
            }
            data.clear();
            event_name.clear();
            continue;
        }
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "data" => {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(value);
            }
            "event" => {
                event_name.clear();
                event_name.push_str(value);
            }
            _ => {}
        }
    }
}

fn terminal_event(data: &str, event_name: &str, run_id: &str) -> Result<bool> {
    let value: Value = serde_json::from_str(data).map_err(|error| {
        failure(
            HARNESS,
            FailureKind::Protocol,
            format!("invalid SSE event JSON: {error}"),
        )
    })?;
    if value["run_id"].as_str() != Some(run_id) {
        return Err(failure(
            HARNESS,
            FailureKind::Protocol,
            "SSE event has a different or missing run_id",
        ));
    }
    let event = value["event"].as_str().unwrap_or(event_name);
    match event {
        "run.completed"
            if !has_error(&value)
                && value["completed"] != false
                && value["partial"] != true
                && value["interrupted"] != true =>
        {
            Ok(true)
        }
        "run.completed" | "run.failed" | "run.cancelled" | "run.interrupted" => Err(failure(
            HARNESS,
            FailureKind::Rejected,
            format!("terminal lifecycle event: {event}"),
        )),
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, VecDeque};
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::thread::{self, JoinHandle};

    const TOKEN: &str = "test-hermes-key";
    const ADMITTED: &str = r#"{"run_id":"run_123","status":"started"}"#;
    const CHAT_OK: &str =
        r#"{"choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#;
    const CHAT_CAPABILITIES: &str = r#"{"features":{"chat_completions":true,"session_continuity_header":"X-Hermes-Session-Id"}}"#;

    #[derive(Debug)]
    struct Request {
        method: String,
        path: String,
        headers: BTreeMap<String, String>,
        body: Vec<u8>,
    }

    struct Reply {
        status: u16,
        content_type: &'static str,
        body: String,
        delay: Duration,
        body_delay: Duration,
        disconnect: bool,
        headers: Vec<(&'static str, &'static str)>,
    }

    impl Reply {
        fn json(status: u16, body: &str) -> Self {
            Self {
                status,
                content_type: "application/json",
                body: body.into(),
                delay: Duration::ZERO,
                body_delay: Duration::ZERO,
                disconnect: false,
                headers: Vec::new(),
            }
        }

        fn sse(body: &str) -> Self {
            Self {
                content_type: "text/event-stream; charset=utf-8",
                ..Self::json(200, body)
            }
        }
    }

    struct Server {
        url: String,
        thread: JoinHandle<Vec<Request>>,
    }

    impl Server {
        fn start(replies: Vec<Reply>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            listener.set_nonblocking(true).unwrap();
            let thread = thread::spawn(move || {
                let mut replies: VecDeque<_> = replies.into();
                let mut requests = Vec::new();
                let mut until = Instant::now() + Duration::from_secs(3);
                while Instant::now() < until {
                    let mut stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(error) => panic!("accept mock request: {error}"),
                    };
                    requests.push(read_request(&stream));
                    let reply = replies.pop_front().unwrap_or_else(|| {
                        Reply::json(500, r#"{"error":{"code":"unexpected_dispatch"}}"#)
                    });
                    thread::sleep(reply.delay);
                    if !reply.disconnect {
                        let mut headers = String::new();
                        for (name, value) in &reply.headers {
                            headers.push_str(name);
                            headers.push_str(": ");
                            headers.push_str(value);
                            headers.push_str("\r\n");
                        }
                        let response = format!(
                            "HTTP/1.1 {} Mock\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n",
                            reply.status,
                            reply.content_type,
                            reply.body.len(),
                            headers
                        );
                        let _ = stream.write_all(response.as_bytes());
                        let body = reply.body.as_bytes();
                        let split = body.len() / 2;
                        let _ = stream.write_all(&body[..split]);
                        thread::sleep(reply.body_delay);
                        let _ = stream.write_all(&body[split..]);
                    }
                    if replies.is_empty() {
                        // Capture illicit fallback/re-dispatches as well as the scripted requests.
                        until = Instant::now() + Duration::from_millis(100);
                    }
                }
                assert!(
                    replies.is_empty(),
                    "expected mock HTTP requests were not made"
                );
                requests
            });
            Self { url, thread }
        }

        fn harness(&self, session: &str) -> HermesHarness {
            HermesHarness {
                session_id: session.into(),
                config: HermesConfig {
                    base_url: self.url.clone(),
                    token_env: None,
                    instructions: None,
                },
            }
        }

        fn finish(self) -> Vec<Request> {
            self.thread.join().unwrap()
        }
    }

    fn read_request(stream: &TcpStream) -> Request {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let mut words = line.split_whitespace();
        let method = words.next().unwrap().to_owned();
        let path = words.next().unwrap().to_owned();
        let mut headers = BTreeMap::new();
        loop {
            line.clear();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            let (name, value) = line.trim_end().split_once(':').unwrap();
            headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
        }
        let length = headers
            .get("content-length")
            .map_or(0, |value| value.parse().unwrap());
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        Request {
            method,
            path,
            headers,
            body,
        }
    }

    fn deliver(harness: &HermesHarness, timeout: Duration) -> Result<()> {
        harness.deliver_authenticated("hello \"Hermes\"\nλ", TOKEN, &Budget::new(timeout))
    }

    fn observe(harness: &HermesHarness, last: Option<u64>, timeout: Duration) -> Result<()> {
        harness.observe_authenticated("run_123", last, TOKEN, &Budget::new(timeout))
    }

    fn kind(result: Result<()>) -> FailureKind {
        match result.unwrap_err() {
            Error::Delivery {
                harness: HARNESS,
                kind,
                ..
            } => kind,
            error => panic!("untyped delivery error: {error}"),
        }
    }

    #[test]
    fn runs_payload_authentication_and_admission_do_not_wait_for_completion() {
        let server = Server::start(vec![Reply::json(202, ADMITTED)]);
        let mut harness = server.harness("exact-api-session");
        harness.config.instructions = Some("Keep the selected context".into());
        deliver(&harness, Duration::from_secs(2)).unwrap();
        let requests = server.finish();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v1/runs");
        assert_eq!(request.headers["authorization"], format!("Bearer {TOKEN}"));
        assert_eq!(request.headers["content-type"], "application/json");
        assert_eq!(
            serde_json::from_slice::<Value>(&request.body).unwrap(),
            json!({
                "input": "hello \"Hermes\"\nλ", "session_id": "exact-api-session", "instructions": "Keep the selected context"
            })
        );
    }

    #[test]
    fn unsupported_runs_falls_back_with_exact_chat_context_and_instructions() {
        for unsupported in [404, 405, 501] {
            let server = Server::start(vec![
                Reply::json(unsupported, "route unavailable"),
                Reply::json(200, CHAT_CAPABILITIES),
                Reply::json(200, CHAT_OK),
            ]);
            let mut harness = server.harness("api-context-42");
            harness.config.instructions = Some("Do not fork context".into());
            deliver(&harness, Duration::from_secs(2)).unwrap();
            let requests = server.finish();
            assert_eq!(requests.len(), 3);
            assert_eq!(requests[0].path, "/v1/runs");
            assert_eq!(requests[1].method, "GET");
            assert_eq!(requests[1].path, "/v1/capabilities");
            assert_eq!(
                requests[1].headers["authorization"],
                format!("Bearer {TOKEN}")
            );
            assert!(requests[1].body.is_empty());
            assert_eq!(requests[2].path, "/v1/chat/completions");
            assert_eq!(
                requests[2].headers["authorization"],
                format!("Bearer {TOKEN}")
            );
            assert_eq!(requests[2].headers["x-hermes-session-id"], "api-context-42");
            assert_eq!(
                serde_json::from_slice::<Value>(&requests[2].body).unwrap(),
                json!({
                    "model": "hermes-agent", "stream": false, "messages": [
                        {"role": "system", "content": "Do not fork context"},
                        {"role": "user", "content": "hello \"Hermes\"\nλ"}
                    ]
                })
            );
        }
    }

    #[test]
    fn negotiation_is_instance_bound_and_reprobes_runs_on_every_write() {
        let legacy = Server::start(vec![
            Reply::json(404, "missing"),
            Reply::json(200, CHAT_CAPABILITIES),
            Reply::json(200, CHAT_OK),
            Reply::json(202, ADMITTED),
        ]);
        let modern = Server::start(vec![Reply::json(202, ADMITTED)]);
        let old = legacy.harness("old-instance-context");
        let new = modern.harness("new-instance-context");
        deliver(&old, Duration::from_secs(2)).unwrap();
        deliver(&new, Duration::from_secs(2)).unwrap();
        deliver(&old, Duration::from_secs(2)).unwrap();
        let old_requests = legacy.finish();
        let new_requests = modern.finish();
        assert_eq!(
            old_requests
                .iter()
                .map(|request| request.path.as_str())
                .collect::<Vec<_>>(),
            [
                "/v1/runs",
                "/v1/capabilities",
                "/v1/chat/completions",
                "/v1/runs"
            ]
        );
        assert_eq!(new_requests.len(), 1);
        assert_eq!(
            serde_json::from_slice::<Value>(&new_requests[0].body).unwrap()["session_id"],
            "new-instance-context"
        );
    }

    #[test]
    fn chat_is_not_submitted_without_advertised_session_continuity() {
        for body in [
            r#"{"features":{"chat_completions":true}}"#,
            r#"{"features":{"chat_completions":true,"session_continuity_header":false}}"#,
            r#"{"features":{"chat_completions":true,"session_continuity_header":"X-Hermes-Session-Key"}}"#,
            r#"{"features":{"chat_completions":false,"session_continuity_header":"X-Hermes-Session-Id"}}"#,
        ] {
            let server = Server::start(vec![Reply::json(404, "missing"), Reply::json(200, body)]);
            assert_eq!(
                kind(deliver(
                    &server.harness("selected-context"),
                    Duration::from_secs(2)
                )),
                FailureKind::Unavailable
            );
            let requests = server.finish();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[1].path, "/v1/capabilities");
            assert_eq!(
                requests
                    .iter()
                    .filter(|request| request.method == "POST")
                    .count(),
                1
            );
        }
    }

    #[test]
    fn capability_refusals_and_timeouts_do_not_submit_chat() {
        for (status, body, expected) in [
            (
                401,
                r#"{"error":{"code":"invalid_api_key"}}"#,
                FailureKind::Authentication,
            ),
            (
                403,
                r#"{"error":{"code":"permission_denied"}}"#,
                FailureKind::Authentication,
            ),
            (404, "missing", FailureKind::Unavailable),
            (200, "not JSON", FailureKind::Protocol),
        ] {
            let server =
                Server::start(vec![Reply::json(404, "missing"), Reply::json(status, body)]);
            assert_eq!(
                kind(deliver(&server.harness("context"), Duration::from_secs(2))),
                expected
            );
            assert_eq!(server.finish().len(), 2);
        }
        let server = Server::start(vec![
            Reply::json(404, "missing"),
            Reply {
                delay: Duration::from_secs(2),
                ..Reply::json(200, CHAT_CAPABILITIES)
            },
        ]);
        assert_eq!(
            kind(deliver(&server.harness("context"), Duration::from_secs(1))),
            FailureKind::Timeout
        );
        assert_eq!(server.finish().len(), 2);
    }

    #[test]
    fn chat_capabilities_are_rechecked_each_write_within_the_selected_profile() {
        let server = Server::start(vec![
            Reply::json(404, "missing"),
            Reply::json(200, CHAT_CAPABILITIES),
            Reply::json(200, CHAT_OK),
            Reply::json(404, "missing"),
            Reply::json(200, r#"{"features":{"chat_completions":true}}"#),
        ]);
        let mut harness = server.harness("profile-session");
        harness.config.base_url.push_str("/p/private/v1/");
        deliver(&harness, Duration::from_secs(2)).unwrap();
        assert_eq!(
            kind(deliver(&harness, Duration::from_secs(2))),
            FailureKind::Unavailable
        );
        let requests = server.finish();
        assert_eq!(
            requests
                .iter()
                .map(|request| request.path.as_str())
                .collect::<Vec<_>>(),
            [
                "/p/private/v1/runs",
                "/p/private/v1/capabilities",
                "/p/private/v1/chat/completions",
                "/p/private/v1/runs",
                "/p/private/v1/capabilities",
            ]
        );
        assert_eq!(
            requests[2].headers["x-hermes-session-id"],
            "profile-session"
        );
    }

    #[test]
    fn authenticated_refusals_never_select_another_protocol() {
        for (status, code, expected) in [
            (401, "invalid_api_key", FailureKind::Authentication),
            (403, "permission_denied", FailureKind::Authentication),
            (409, "permission_denied", FailureKind::Rejected),
            (404, "session_not_found", FailureKind::Rejected),
            (404, "policy_denied", FailureKind::Rejected),
            (405, "scope_required", FailureKind::Rejected),
            (501, "execution_failed", FailureKind::Rejected),
            (429, "rate_limit_exceeded", FailureKind::Rejected),
            (503, "runtime_draining", FailureKind::Rejected),
        ] {
            let server = Server::start(vec![Reply::json(
                status,
                &json!({"error": {"code": code}}).to_string(),
            )]);
            assert_eq!(
                kind(deliver(
                    &server.harness("foreign-cli-session"),
                    Duration::from_secs(2)
                )),
                expected
            );
            assert_eq!(server.finish().len(), 1);
        }
    }

    #[test]
    fn malformed_and_terminal_admission_responses_do_not_fallback() {
        for (body, expected) in [
            ("not JSON", FailureKind::Protocol),
            (r#"{"run_id":"run_123"}"#, FailureKind::Protocol),
            (r#"{"status":"started"}"#, FailureKind::Protocol),
            (
                r#"{"run_id":"run_123","status":"failed"}"#,
                FailureKind::Rejected,
            ),
            (
                r#"{"run_id":"run_123","status":"cancelled"}"#,
                FailureKind::Rejected,
            ),
            (
                r#"{"run_id":"run_123","status":"completed","partial":true}"#,
                FailureKind::Rejected,
            ),
            (r#"{"error":{"message":"refused"}}"#, FailureKind::Rejected),
            (
                r#"{"run_id":"run_123","status":"started","session_id":"different"}"#,
                FailureKind::Protocol,
            ),
        ] {
            let server = Server::start(vec![Reply::json(202, body)]);
            assert_eq!(
                kind(deliver(&server.harness("context"), Duration::from_secs(2))),
                expected
            );
            assert_eq!(server.finish().len(), 1);
        }
    }

    #[test]
    fn ambiguous_disconnect_never_re_dispatches() {
        let server = Server::start(vec![Reply {
            disconnect: true,
            ..Reply::json(202, ADMITTED)
        }]);
        assert_eq!(
            kind(deliver(&server.harness("context"), Duration::from_secs(2))),
            FailureKind::Transport
        );
        assert_eq!(server.finish().len(), 1);
    }

    #[test]
    fn admission_timeout_does_not_fallback_or_retry() {
        let server = Server::start(vec![Reply {
            delay: Duration::from_secs(2),
            ..Reply::json(202, ADMITTED)
        }]);
        assert_eq!(
            kind(deliver(&server.harness("context"), Duration::from_secs(1))),
            FailureKind::Timeout
        );
        assert_eq!(server.finish().len(), 1);
    }

    #[test]
    fn fallback_shares_the_original_deadline() {
        let server = Server::start(vec![
            Reply {
                delay: Duration::from_millis(1500),
                ..Reply::json(404, "missing")
            },
            Reply::json(200, CHAT_CAPABILITIES),
            Reply {
                delay: Duration::from_millis(1500),
                ..Reply::json(200, CHAT_OK)
            },
        ]);
        assert_eq!(
            kind(deliver(
                &server.harness("context"),
                Duration::from_millis(2500)
            )),
            FailureKind::Timeout
        );
        assert_eq!(server.finish().len(), 3);
    }

    #[test]
    fn chat_requires_successful_terminal_choices() {
        for reason in ["error", "cancelled", "length", "tool_calls"] {
            let body = json!({"choices": [{"finish_reason": reason}]}).to_string();
            let server = Server::start(vec![
                Reply::json(404, "missing"),
                Reply::json(200, CHAT_CAPABILITIES),
                Reply::json(200, &body),
            ]);
            assert_eq!(
                kind(deliver(&server.harness("context"), Duration::from_secs(2))),
                FailureKind::Rejected
            );
            assert_eq!(server.finish().len(), 3);
        }
        for body in [r#"{"choices":[]}"#, r#"{"choices":[{}]}"#] {
            let server = Server::start(vec![
                Reply::json(404, "missing"),
                Reply::json(200, CHAT_CAPABILITIES),
                Reply::json(200, body),
            ]);
            assert_eq!(
                kind(deliver(&server.harness("context"), Duration::from_secs(2))),
                FailureKind::Protocol
            );
            assert_eq!(server.finish().len(), 3);
        }
    }

    #[test]
    fn chat_refusal_and_context_mismatch_are_not_bypassed() {
        let server = Server::start(vec![
            Reply::json(404, "missing"),
            Reply::json(200, CHAT_CAPABILITIES),
            Reply::json(409, r#"{"error":{"code":"permission_denied"}}"#),
        ]);
        assert_eq!(
            kind(deliver(
                &server.harness("cli-owned"),
                Duration::from_secs(2)
            )),
            FailureKind::Rejected
        );
        assert_eq!(server.finish().len(), 3);
        let server = Server::start(vec![
            Reply::json(404, "missing"),
            Reply::json(200, CHAT_CAPABILITIES),
            Reply {
                headers: vec![("X-Hermes-Session-Id", "another-context")],
                ..Reply::json(200, CHAT_OK)
            },
        ]);
        assert_eq!(
            kind(deliver(&server.harness("context"), Duration::from_secs(2))),
            FailureKind::Protocol
        );
        assert_eq!(server.finish().len(), 3);
    }

    #[test]
    fn redirects_are_not_followed_or_treated_as_admission() {
        let server = Server::start(vec![Reply {
            headers: vec![("Location", "/v1/chat/completions")],
            ..Reply::json(307, "redirect")
        }]);
        assert_eq!(
            kind(deliver(&server.harness("context"), Duration::from_secs(2))),
            FailureKind::Rejected
        );
        assert_eq!(server.finish().len(), 1);
    }

    #[test]
    fn sse_uses_bearer_resumption_and_json_lifecycle_not_assistant_finalization() {
        let server = Server::start(vec![Reply::sse(concat!(
            ": open\r\n\r\n: keepalive\r\n\r\n",
            "id: 8\r\ndata: {\"event\":\"assistant.final\",\"run_id\":\"run_123\"}\r\n\r\n",
            "id: 9\r\ndata: {\"event\":\"run.completed\",\n",
            "data: \"run_id\":\"run_123\",\"completed\":true,\"seq\":9}\r\n\r\n"
        ))]);
        observe(&server.harness("context"), Some(7), Duration::from_secs(2)).unwrap();
        let requests = server.finish();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].path, "/v1/runs/run_123/events");
        assert_eq!(
            requests[0].headers["authorization"],
            format!("Bearer {TOKEN}")
        );
        assert_eq!(requests[0].headers["last-event-id"], "7");
        assert_eq!(requests[0].headers["accept"], "text/event-stream");
    }

    #[test]
    fn sse_terminal_failure_is_not_hidden_by_prior_assistant_final() {
        for event in ["run.failed", "run.cancelled", "run.interrupted"] {
            let server = Server::start(vec![Reply::sse(&format!(
                "data: {{\"event\":\"assistant.final\",\"run_id\":\"run_123\"}}\n\ndata: {{\"event\":\"{event}\",\"run_id\":\"run_123\"}}\n\n"
            ))]);
            assert_eq!(
                kind(observe(
                    &server.harness("context"),
                    None,
                    Duration::from_secs(2)
                )),
                FailureKind::Rejected
            );
            assert_eq!(server.finish().len(), 1);
        }
    }

    #[test]
    fn sse_closure_invalid_json_and_wrong_run_are_not_completion() {
        for (body, expected) in [
            (
                ": keepalive\n\ndata: {\"event\":\"assistant.final\",\"run_id\":\"run_123\"}\n\n",
                FailureKind::Transport,
            ),
            ("data: not-json\n\n", FailureKind::Protocol),
            (
                "data: {\"event\":\"run.completed\",\"run_id\":\"wrong\"}\n\n",
                FailureKind::Protocol,
            ),
            (
                "data: {\"event\":\"run.completed\",\"run_id\":\"run_123\",\"partial\":true}\n\n",
                FailureKind::Rejected,
            ),
        ] {
            let server = Server::start(vec![Reply::sse(body)]);
            assert_eq!(
                kind(observe(
                    &server.harness("context"),
                    None,
                    Duration::from_secs(2)
                )),
                expected
            );
            assert_eq!(server.finish().len(), 1);
        }
    }

    #[test]
    fn sse_authentication_failure_and_timeout_never_dispatch() {
        let server = Server::start(vec![Reply::json(
            401,
            r#"{"error":{"code":"invalid_api_key"}}"#,
        )]);
        assert_eq!(
            kind(observe(
                &server.harness("context"),
                None,
                Duration::from_secs(2)
            )),
            FailureKind::Authentication
        );
        assert_eq!(server.finish().len(), 1);
        let server = Server::start(vec![Reply {
            delay: Duration::from_secs(2),
            ..Reply::sse(": keepalive\n\n")
        }]);
        assert_eq!(
            kind(observe(
                &server.harness("context"),
                None,
                Duration::from_secs(1)
            )),
            FailureKind::Timeout
        );
        assert_eq!(server.finish().len(), 1);
    }

    #[test]
    fn sse_deadline_bounds_an_open_stream_despite_keepalive_comments() {
        let body = format!(
            "{}data: {{\"event\":\"run.completed\",\"run_id\":\"run_123\"}}\n\n",
            ": keepalive\n\n".repeat(20)
        );
        let server = Server::start(vec![Reply {
            body_delay: Duration::from_secs(2),
            ..Reply::sse(&body)
        }]);
        assert_eq!(
            kind(observe(
                &server.harness("context"),
                None,
                Duration::from_secs(1)
            )),
            FailureKind::Timeout
        );
        assert_eq!(server.finish().len(), 1);
    }

    #[test]
    fn profile_prefixes_and_v1_bases_preserve_the_gateway_route() {
        for prefix in ["/p/private", "/p/private/v1", "/p/private/v1/"] {
            let server = Server::start(vec![Reply::json(202, ADMITTED)]);
            let mut harness = server.harness("profile-session");
            harness.config.base_url.push_str(prefix);
            deliver(&harness, Duration::from_secs(2)).unwrap();
            assert_eq!(server.finish()[0].path, "/p/private/v1/runs");
        }
        assert_eq!(
            endpoint("http://localhost:8642", &["runs", "a/b?c", "events"])
                .unwrap()
                .path(),
            "/v1/runs/a%2Fb%3Fc/events"
        );
    }

    #[test]
    fn invalid_configuration_and_expired_budget_fail_before_dispatch() {
        let harness = HermesHarness {
            session_id: "context".into(),
            config: HermesConfig {
                base_url: "http://127.0.0.1:1".into(),
                token_env: None,
                instructions: None,
            },
        };
        assert_eq!(
            kind(harness.deliver_authenticated("hello", "", &Budget::new(Duration::from_secs(1)))),
            FailureKind::Authentication
        );
        assert_eq!(
            kind(harness.deliver_authenticated(
                "hello",
                "bad\nsecret",
                &Budget::new(Duration::from_secs(1))
            )),
            FailureKind::Authentication
        );
        assert_eq!(
            kind(deliver(&harness, Duration::ZERO)),
            FailureKind::Timeout
        );
        for base in [
            "file:///tmp/hermes",
            "http://user:password@localhost:8642",
            "http://localhost:8642?token=secret",
            "http://localhost:8642#fragment",
        ] {
            assert!(endpoint(base, &["runs"]).is_err());
        }
        assert!(
            serde_json::from_value::<HermesConfig>(
                json!({"base_url":"http://localhost:8642", "cli":true})
            )
            .is_err()
        );
        let mut missing_token = harness.clone();
        // '=' cannot occur in an environment variable name; no process-wide mutation.
        missing_token.config.token_env = Some("LLM_SESSIONS=HERMES_TOKEN".into());
        assert_eq!(
            kind(missing_token.deliver("hello", Duration::from_secs(1))),
            FailureKind::Authentication
        );
    }
}
