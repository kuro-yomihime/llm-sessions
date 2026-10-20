use super::*;
use crate::model::{Locator, Transcript};
#[cfg(feature = "http")]
use std::io::{Read, Write};
#[cfg(feature = "http")]
use std::net::TcpListener;
#[cfg(feature = "http")]
use std::sync::mpsc;
use std::time::Instant;

fn session(source: SourceId) -> SessionSummary {
    SessionSummary::new(
        "same-session".into(),
        source,
        time::OffsetDateTime::UNIX_EPOCH,
        Locator::File {
            path: "/unused/session.jsonl".into(),
        },
    )
}

fn wait(delivery: &Delivery) -> std::result::Result<(), String> {
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(result) = delivery.poll() {
            return result;
        }
        assert!(Instant::now() < end, "delivery did not settle");
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct RecordingCli {
    output: PathBuf,
}

impl Source for RecordingCli {
    fn id(&self) -> SourceId {
        SourceId::Hermes
    }
    fn roots(&self) -> Vec<PathBuf> {
        Vec::new()
    }
    fn list(&self) -> Result<Vec<SessionSummary>> {
        Ok(Vec::new())
    }
    fn open(&self, _: &crate::model::Locator) -> Result<Transcript> {
        unreachable!()
    }
    fn insert_command(&self, _: &str, message: &str) -> Option<Vec<String>> {
        Some(vec![
            "sh".into(),
            "-c".into(),
            "printf '%s' \"$1\" > \"$2\"".into(),
            "record".into(),
            message.into(),
            self.output.display().to_string(),
        ])
    }
}

#[test]
fn unconfigured_hermes_retains_native_cli_and_literal_message() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("received");
    let sources: Vec<Box<dyn Source>> = vec![Box::new(RecordingCli {
        output: output.clone(),
    })];
    let row = session(SourceId::Hermes);
    let fleet = Fleet::default();
    let harness = attach_at(&row, &sources, &fleet, &dir.path().join("absent.json")).unwrap();
    let message = "-literal '$(not a command)'\nλ";
    wait(
        &harness
            .write_message(message, Duration::from_secs(2))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(std::fs::read_to_string(output).unwrap(), message);
}

#[test]
fn invalid_explicit_gateway_never_downgrades_to_native_cli() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("must-not-run");
    let sources: Vec<Box<dyn Source>> = vec![Box::new(RecordingCli {
        output: output.clone(),
    })];
    let row = session(SourceId::Hermes);
    let path = dir.path().join("bindings.json");
    std::fs::write(
        &path,
        serde_json::json!({"sessions":[{
            "source":"hermes", "session_id":row.id, "config":{"unknown_transport":"bad"}
        }]})
        .to_string(),
    )
    .unwrap();
    let fleet = Fleet::default();
    assert!(attach_at(&row, &sources, &fleet, &path).is_err());
    assert!(!output.exists());
}

#[cfg(feature = "http")]
fn runs_server() -> (
    String,
    mpsc::Receiver<serde_json::Value>,
    std::thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < end, "no request arrived");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut chunk = [0; 1024];
        let body = loop {
            let n = stream.read(&mut chunk).unwrap();
            assert_ne!(n, 0);
            bytes.extend_from_slice(&chunk[..n]);
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..end]);
                assert!(headers.starts_with("POST /v1/runs HTTP/1.1"), "{headers}");
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap();
                if bytes.len() >= end + 4 + length {
                    break serde_json::from_slice::<serde_json::Value>(
                        &bytes[end + 4..end + 4 + length],
                    )
                    .unwrap();
                }
            }
        };
        let payload = r#"{"run_id":"admitted-run","status":"started"}"#;
        write!(stream, "HTTP/1.1 202 Accepted\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len()).unwrap();
        tx.send(body).unwrap();
    });
    (url, rx, worker)
}

#[test]
#[cfg(feature = "http")]
fn same_id_on_two_origins_uses_only_its_attached_hermes_instance() {
    let dir = tempfile::tempdir().unwrap();
    let (a, requests_a, worker_a) = runs_server();
    let (b, requests_b, worker_b) = runs_server();
    let origin_a = "11111111-1111-4111-8111-111111111111";
    let origin_b = "22222222-2222-4222-8222-222222222222";
    let path = dir.path().join("bindings.json");
    std::fs::write(&path, serde_json::json!({"sessions":[
        {"source":"hermes","session_id":"same-session","origin_id":origin_a,"config":{"base_url":a,"token_env":"PATH"}},
        {"source":"hermes","session_id":"same-session","origin_id":origin_b,"config":{"base_url":b,"token_env":"PATH"}}
    ]}).to_string()).unwrap();
    let mut row = session(SourceId::Hermes);
    row.origin_label = Some("remote API-only origin".into());
    let fleet = Fleet::default();
    let sources = Vec::new();
    for (origin, message) in [(origin_a, "for A"), (origin_b, "for B")] {
        row.origin_id = Some(origin.into());
        let harness = attach_at(&row, &sources, &fleet, &path).unwrap();
        wait(
            &harness
                .write_message(message, Duration::from_secs(3))
                .unwrap(),
        )
        .unwrap();
    }
    let first = requests_a.recv_timeout(Duration::from_secs(2)).unwrap();
    let second = requests_b.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(first["input"], "for A");
    assert_eq!(second["input"], "for B");
    assert_eq!(first["session_id"], "same-session");
    assert_eq!(second["session_id"], "same-session");
    worker_a.join().unwrap();
    worker_b.join().unwrap();
}

#[test]
#[cfg(feature = "http")]
fn local_binding_matches_a_local_row_with_a_canonical_origin() {
    let dir = tempfile::tempdir().unwrap();
    let (url, requests, worker) = runs_server();
    let path = dir.path().join("bindings.json");
    std::fs::write(&path, serde_json::json!({"sessions":[{
        "source":"hermes","session_id":"same-session","config":{"base_url":url,"token_env":"PATH"}
    }]}).to_string()).unwrap();
    let mut row = session(SourceId::Hermes);
    row.origin_id = Some("11111111-1111-4111-8111-111111111111".into());
    let fleet = Fleet::default();
    let sources = Vec::new();
    let harness = attach_at(&row, &sources, &fleet, &path).unwrap();
    wait(
        &harness
            .write_message("local message", Duration::from_secs(3))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        requests.recv_timeout(Duration::from_secs(2)).unwrap()["input"],
        "local message"
    );
    worker.join().unwrap();
}

#[test]
#[cfg(feature = "http")]
fn remote_openclaw_binding_cannot_enable_colocated_cli() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bindings.json");
    let origin = "11111111-1111-4111-8111-111111111111";
    std::fs::write(
        &path,
        serde_json::json!({"sessions":[{
            "source":"openclaw", "session_id":"same-session", "origin_id":origin,
            "config":{"cli":true}
        }]})
        .to_string(),
    )
    .unwrap();
    let mut row = session(SourceId::OpenClaw);
    row.origin_id = Some(origin.into());
    row.origin_label = Some("remote".into());
    let fleet = Fleet::default();
    assert!(matches!(
        attach_at(&row, &[], &fleet, &path),
        Err(Error::Delivery {
            kind: FailureKind::Rejected,
            ..
        })
    ));
}

#[test]
fn duplicate_instance_bindings_refuse_ambiguous_delivery() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bindings.json");
    let binding = serde_json::json!({
        "source":"hermes", "session_id":"same-session",
        "config":{"base_url":"http://127.0.0.1:1"}
    });
    std::fs::write(
        &path,
        serde_json::json!({"sessions":[binding, binding]}).to_string(),
    )
    .unwrap();
    let row = session(SourceId::Hermes);
    let fleet = Fleet::default();
    assert!(matches!(
        attach_at(&row, &[], &fleet, &path),
        Err(Error::Delivery {
            kind: FailureKind::Protocol,
            ..
        })
    ));
}
