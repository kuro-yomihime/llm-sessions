#![cfg(all(feature = "serve", feature = "http"))]
//! Real HTTP servers and CLI observers, isolated from the user's stores.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TOKEN: &str = "fixture-http-federation-token";
const SECRET_ENV: &str = "LLMS_FIXTURE_PEER_TOKEN";

struct Server {
    child: Child,
    url: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn command(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_llm-sessions"));
    cmd.env("HOME", home)
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env(SECRET_ENV, TOKEN)
        .env_remove("LLMS_TOKEN")
        .env_remove("LLMS_BIND");
    cmd
}

fn home(root: &Path, name: &str, number: u32) -> PathBuf {
    let home = root.join(name);
    let state = home.join(".local/state/llm-sessions");
    fs::create_dir_all(&state).unwrap();
    fs::write(state.join("origin-id"), origin(number)).unwrap();
    home
}

fn origin(number: u32) -> String {
    format!("00000000-0000-4000-8000-{number:012}")
}

fn session(home: &Path, id: &str, title: &str) {
    let store = home.join(".omp/agent/sessions/work");
    fs::create_dir_all(&store).unwrap();
    fs::write(store.join(format!("{id}.jsonl")), format!("{}\n{}\n",
        json!({"type":"session","version":3,"id":id,"timestamp":"2026-01-01T00:00:00Z","cwd":"/workspace","title":title}),
        json!({"type":"message","id":"m1","timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":[{"type":"text","text":title}]}})
    )).unwrap();
}

fn peers(home: &Path, hosts: Value) {
    let config = home.join(".config/llm-sessions");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("hosts.json"),
        Value::Object(serde_json::Map::from_iter([("hosts".into(), hosts)])).to_string(),
    )
    .unwrap();
}

fn peer(name: &str, server: &Server) -> Value {
    json!({"name":name,"url":server.url,"origin_id":get(server, "/v1/federation/capabilities", &[]).1["origin_id"],"token_env":SECRET_ENV,"sources":["omp"]})
}

fn server(home: &Path, gateway: bool) -> Server {
    server_options(home, gateway, "1", &[])
}

fn server_options(home: &Path, gateway: bool, poll_secs: &str, options: &[&str]) -> Server {
    let mut cmd = server_command(home, gateway, poll_secs, options);
    cmd.stdout(Stdio::null())
        .stderr(fs::File::create(home.join("server.log")).unwrap());
    spawn_ready(cmd, &home.join("server.log"))
}

/// A gateway that also makes ssh calls of its own: the fixture's fake
/// `ssh` is first on its PATH, so its ssh peers resolve inside the fixture
/// exactly as an observer's do.
fn ssh_server(home: &Path, root: &Path, bin: &Path) -> Server {
    let mut cmd = server_command(home, true, "1", &[]);
    cmd.env("FIXTURE_ROOT", root)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .stdout(Stdio::null())
        .stderr(fs::File::create(home.join("server.log")).unwrap());
    spawn_ready(cmd, &home.join("server.log"))
}

fn server_command(home: &Path, gateway: bool, poll_secs: &str, options: &[&str]) -> Command {
    let mut cmd = command(home);
    cmd.args([
        "--read-only",
        "serve",
        "--bind",
        "127.0.0.1:0",
        "--token",
        TOKEN,
        "--poll-secs",
        poll_secs,
    ]);
    if gateway {
        cmd.arg("--federate");
    }
    cmd.args(options);
    cmd
}

fn spawn_ready(mut cmd: Command, log: &Path) -> Server {
    let mut child = cmd.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let text = fs::read_to_string(log).unwrap();
        if let Some(address) = text
            .split("on http://")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
        {
            return Server {
                child,
                url: format!("http://{address}"),
            };
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("server exited {status}: {text}");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("server readiness timeout: {text}");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn get(server: &Server, path: &str, query: &[(&str, &str)]) -> (u16, Value) {
    let response = reqwest::blocking::Client::new()
        .get(format!("{}{path}", server.url))
        .bearer_auth(TOKEN)
        .query(query)
        .timeout(Duration::from_secs(15))
        .send()
        .unwrap();
    let status = response.status().as_u16();
    (status, response.json().unwrap())
}

fn list(home: &Path) -> Value {
    let output = command(home)
        .args([
            "--read-only",
            "list",
            "--json",
            "--limit",
            "0",
            "--archived",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn http_leaf_keeps_local_scope_and_qualified_reads_resolve_agent_collisions() {
    let root = tempfile::tempdir().unwrap();
    let b = home(root.path(), "B", 2);
    session(&b, "shared", "OMP session");
    let claude = b.join(".claude/projects/work");
    fs::create_dir_all(&claude).unwrap();
    fs::write(claude.join("shared.jsonl"), format!("{}\n", json!({"type":"user","sessionId":"shared","cwd":"/workspace","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"Claude session"}}))).unwrap();
    peers(
        &b,
        json!([{"name":"unreachable","ssh":"invalid.example.invalid"}]),
    );
    let server = server(&b, false);
    let capabilities = get(&server, "/v1/federation/capabilities", &[]);
    assert_eq!(capabilities.0, 200);
    assert_eq!(capabilities.1["version"], json!(2));
    assert_eq!(capabilities.1["origin_id"], origin(2));
    let scan = get(
        &server,
        "/v1/federation/list",
        &[("path", "[]"), ("hops", "8")],
    );
    assert_eq!(scan.0, 200);
    assert_eq!(scan.1["sessions"].as_array().unwrap().len(), 2);
    assert_eq!(scan.1["failures"], json!([]));
    assert_eq!(get(&server, "/v1/sessions/shared/transcript", &[]).0, 400);
    let transcript = get(
        &server,
        "/v1/sessions/shared/transcript",
        &[("origin", &origin(2)), ("source", "omp")],
    );
    assert_eq!(transcript.0, 200);
    assert_eq!(transcript.1["title"], "OMP session");
    assert_eq!(
        get(
            &server,
            "/v1/sessions/shared/transcript",
            &[("origin", &origin(99)), ("source", "omp")]
        )
        .0,
        404
    );
    let l = home(root.path(), "L", 1);
    peers(&l, json!([peer("leaf", &server)]));
    let rows = list(&l);
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["origin_id"], origin(2));
    let output = command(&l)
        .args([
            "--read-only",
            "show",
            "shared",
            "--origin",
            &origin(2),
            "--source",
            "omp",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["title"],
        "OMP session"
    );
    assert!(!rows.to_string().contains(TOKEN));
}

#[test]
fn http_gateway_deduplicates_routes_and_keeps_colliding_origins_separate() {
    let root = tempfile::tempdir().unwrap();
    let b = home(root.path(), "B", 2);
    session(&b, "shared", "B transcript");
    let bs = server(&b, false);
    let a = home(root.path(), "A", 3);
    session(&a, "shared", "A transcript");
    peers(&a, json!([peer("worker", &bs)]));
    let gateway = server(&a, true);
    let rows = get(&gateway, "/v1/sessions", &[("limit", "0")]);
    assert_eq!(rows.0, 200);
    assert_eq!(rows.1["sessions"].as_array().unwrap().len(), 2);
    for (number, title) in [(2, "B transcript"), (3, "A transcript")] {
        let transcript = get(
            &gateway,
            "/v1/sessions/shared/transcript",
            &[("origin", &origin(number)), ("source", "omp")],
        );
        assert_eq!(transcript.0, 200);
        assert_eq!(transcript.1["title"], title);
    }
    let l = home(root.path(), "L", 1);
    peers(&l, json!([peer("gateway", &gateway), peer("direct", &bs)]));
    let rows = list(&l);
    assert_eq!(rows.as_array().unwrap().len(), 2);
    let b = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["origin_id"] == origin(2))
        .unwrap();
    assert_eq!(b["routes"].as_array().unwrap().len(), 2);
    assert_eq!(b["host"], "direct");
    assert!(!rows.to_string().contains(TOKEN));
}

#[test]
fn guarded_http_discovery_stops_cycles_and_preserves_depth_endpoint_rows() {
    let root = tempfile::tempdir().unwrap();
    let a = home(root.path(), "A", 2);
    session(&a, "a", "A endpoint");
    let server = server(&a, true);
    let repeated = serde_json::to_string(&vec![origin(2)]).unwrap();
    assert_eq!(
        get(
            &server,
            "/v1/federation/list",
            &[("path", &repeated), ("hops", "8")]
        )
        .1,
        json!({"sessions":[],"failures":[]})
    );
    assert_eq!(
        get(
            &server,
            "/v1/federation/list",
            &[("path", "[\"invalid\"]"), ("hops", "8")]
        )
        .0,
        400
    );
    assert_eq!(
        get(
            &server,
            "/v1/federation/list",
            &[("path", "[]"), ("hops", "9")]
        )
        .0,
        400
    );
    peers(&a, json!([peer("self", &server)]));
    let scan = get(
        &server,
        "/v1/federation/list",
        &[("path", "[]"), ("hops", "0")],
    );
    assert_eq!(scan.0, 200);
    assert_eq!(scan.1["sessions"][0]["id"], "a");
    // A self-edge is cyclic, not unfinished discovery, even at zero hops.
    assert_eq!(scan.1["failures"], json!([]));
    let cycle = get(
        &server,
        "/v1/federation/list",
        &[("path", "[]"), ("hops", "8")],
    );
    assert_eq!(cycle.0, 200);
    assert_eq!(cycle.1["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(cycle.1["failures"], json!([]));
}

#[test]
fn ssh_forwarder_can_discover_and_open_an_http_only_origin() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = tempfile::tempdir().unwrap();
    let b = home(root.path(), "B", 2);
    session(&b, "mixed", "HTTP origin through SSH");
    let bs = server(&b, false);
    let a = home(root.path(), "A", 3);
    peers(&a, json!([peer("http-worker", &bs)]));
    let l = home(root.path(), "L", 1);
    peers(
        &l,
        json!([{"name":"ssh-gateway","ssh":"A","origin_id":origin(3),"sources":["omp"]}]),
    );
    let bin = root.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    symlink(env!("CARGO_BIN_EXE_llm-sessions"), bin.join("llm-sessions")).unwrap();
    let ssh = bin.join("ssh");
    fs::write(
        &ssh,
        r#"#!/bin/sh
while [ "$1" = '-o' ]; do shift 2; done
if [ "$1" = '-t' ]; then shift; fi
if [ "$1" = '--' ]; then shift; fi
peer=$1
shift
HOME="$FIXTURE_ROOT/$peer"
XDG_CONFIG_HOME="$HOME/.config"
XDG_STATE_HOME="$HOME/.local/state"
XDG_CACHE_HOME="$HOME/.cache"
XDG_DATA_HOME="$HOME/.local/share"
export HOME XDG_CONFIG_HOME XDG_STATE_HOME XDG_CACHE_HOME XDG_DATA_HOME
exec /bin/sh -c "$*"
"#,
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    let execute = |args: &[&str]| {
        let output = command(&l)
            .env("FIXTURE_ROOT", root.path())
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let rows = execute(&["--read-only", "list", "--json", "--limit", "0"]);
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["origin_id"], origin(2));
    assert_eq!(rows[0]["routes"][0]["hops"].as_array().unwrap().len(), 2);
    let transcript = execute(&[
        "--read-only",
        "show",
        "mixed",
        "--origin",
        &origin(2),
        "--source",
        "omp",
        "--json",
    ]);
    assert_eq!(transcript["title"], "HTTP origin through SSH");
    peers(
        &l,
        json!([{"name":"ssh-gateway","ssh":"A","origin_id":origin(3),"sources":["omp"]}, peer("direct-http", &bs)]),
    );
    let rows = execute(&["--read-only", "list", "--json", "--limit", "0"]);
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["routes"].as_array().unwrap().len(), 2);
    assert_eq!(rows[0]["host"], "direct-http");
}

#[test]
fn gateway_source_policy_bounds_regular_and_federation_reads() {
    let root = tempfile::tempdir().unwrap();
    let b = home(root.path(), "B", 2);
    session(&b, "hidden", "Forbidden OMP session");
    let claude = b.join(".claude/projects/work");
    fs::create_dir_all(&claude).unwrap();
    fs::write(claude.join("allowed.jsonl"), format!("{}\n", json!({"type":"user","sessionId":"allowed","cwd":"/workspace","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"Allowed Claude session"}}))).unwrap();
    let bs = server(&b, false);
    let a = home(root.path(), "A", 3);
    peers(
        &a,
        json!([{"name":"worker","url":bs.url,"origin_id":origin(2),"token_env":SECRET_ENV}]),
    );
    let gateway = server_options(&a, true, "1", &["--source", "claude"]);
    let regular = get(&gateway, "/v1/sessions", &[("limit", "0")]);
    assert_eq!(regular.0, 200);
    assert_eq!(regular.1["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(regular.1["sessions"][0]["source"], "claude");
    let scan = get(
        &gateway,
        "/v1/federation/list",
        &[("path", "[]"), ("hops", "8")],
    );
    assert_eq!(scan.0, 200);
    assert_eq!(scan.1["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(scan.1["sessions"][0]["id"], "allowed");
    assert_eq!(
        get(
            &gateway,
            "/v1/sessions/hidden/transcript",
            &[("origin", &origin(2)), ("source", "omp")]
        )
        .0,
        404
    );
    assert_eq!(
        get(
            &gateway,
            "/v1/sessions/allowed/transcript",
            &[("origin", &origin(2)), ("source", "claude")]
        )
        .0,
        200
    );
}

#[test]
fn delegated_http_routes_use_only_locally_configured_credentials() {
    let root = tempfile::tempdir().unwrap();
    let b = home(root.path(), "B", 2);
    session(&b, "read", "Authorized transcript");
    let bs = server(&b, false);
    let l = home(root.path(), "L", 1);
    peers(&l, json!([peer("trusted", &bs)]));
    let wire =
        json!([{"origin_id":origin(2),"name":"forged-name","url":bs.url,"token_env":"HOME"}])
            .to_string();
    let read = || {
        command(&l)
            .args([
                "--read-only",
                "federation",
                "show",
                "--origin",
                &origin(2),
                "--source",
                "omp",
                "--id",
                "read",
                "--route",
                &wire,
            ])
            .output()
            .unwrap()
    };
    let authorized = read();
    assert!(
        authorized.status.success(),
        "{}",
        String::from_utf8_lossy(&authorized.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&authorized.stdout).unwrap()["title"],
        "Authorized transcript"
    );
    peers(&l, json!([]));
    let unauthorized = read();
    assert!(!unauthorized.status.success());
    let error = String::from_utf8_lossy(&unauthorized.stderr);
    assert!(error.contains("requires a token"), "{error}");
    assert!(!error.contains(TOKEN), "{error}");
    assert!(
        !error.contains(l.to_str().unwrap()),
        "a forged credential name must not select HOME: {error}"
    );
}

#[test]
fn malformed_gateway_configuration_is_reported_to_the_observer() {
    let root = tempfile::tempdir().unwrap();
    let a = home(root.path(), "A", 2);
    session(&a, "local", "Local session survives config failure");
    let config = a.join(".config/llm-sessions");
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("hosts.json"), "not json").unwrap();
    let gateway = server(&a, true);
    let (status, scan) = get(
        &gateway,
        "/v1/federation/list",
        &[("path", "[]"), ("hops", "8")],
    );
    assert_eq!(status, 200);
    assert_eq!(scan["sessions"][0]["id"], "local");
    assert!(
        scan["failures"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f.as_str().unwrap().contains("hosts.json"))
    );
}

#[test]
fn missing_identity_is_explicit_and_federation_routes_require_authentication() {
    let root = tempfile::tempdir().unwrap();
    let a = home(root.path(), "A", 2);
    fs::remove_file(a.join(".local/state/llm-sessions/origin-id")).unwrap();
    session(&a, "local", "Local row without canonical identity");
    let leaf = server(&a, false);
    let client = reqwest::blocking::Client::new();
    for route in ["/v1/federation/capabilities", "/v1/federation/list"] {
        assert_eq!(
            client
                .get(format!("{}{route}", leaf.url))
                .send()
                .unwrap()
                .status()
                .as_u16(),
            401
        );
        assert_eq!(get(&leaf, route, &[]).0, 503);
    }
    assert!(
        !a.join(".local/state/llm-sessions/origin-id").exists(),
        "read-only server must not initialize identity"
    );
    let listing = get(&leaf, "/v1/sessions", &[("limit", "0")]);
    assert_eq!(listing.0, 200);
    assert_eq!(listing.1["sessions"][0]["origin_id"], Value::Null);
}

#[test]
fn gateway_base_query_bounds_every_public_read() {
    let root = tempfile::tempdir().unwrap();
    let b = home(root.path(), "B", 2);
    session(&b, "allowed", "Allowed transcript");
    session(&b, "hidden", "Hidden transcript");
    let bs = server(&b, false);
    let a = home(root.path(), "A", 3);
    peers(&a, json!([peer("worker", &bs)]));
    let gateway = server_options(&a, true, "1", &["-q", "allowed"]);
    let listing = get(&gateway, "/v1/sessions", &[("limit", "0")]);
    assert_eq!(listing.1["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(listing.1["sessions"][0]["id"], "allowed");
    let scan = get(
        &gateway,
        "/v1/federation/list",
        &[("path", "[]"), ("hops", "8")],
    );
    assert_eq!(scan.1["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(scan.1["sessions"][0]["id"], "allowed");
    assert_eq!(
        get(
            &gateway,
            "/v1/sessions/hidden",
            &[("origin", &origin(2)), ("source", "omp")]
        )
        .0,
        404
    );
    assert_eq!(
        get(
            &gateway,
            "/v1/sessions/hidden/transcript",
            &[("origin", &origin(2)), ("source", "omp")]
        )
        .0,
        404
    );
}

/// The fake `ssh` the mixed-transport tests install: a peer name maps to
/// that peer's fixture home, and the command runs there with this repo's
/// binary on PATH exactly as a real remote needs it.
fn ssh_shim(root: &Path) -> PathBuf {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    symlink(env!("CARGO_BIN_EXE_llm-sessions"), bin.join("llm-sessions")).unwrap();
    let ssh = bin.join("ssh");
    fs::write(
        &ssh,
        r#"#!/bin/sh
while [ "$1" = '-o' ]; do shift 2; done
if [ "$1" = '-t' ]; then shift; fi
if [ "$1" = '--' ]; then shift; fi
peer=$1
shift
HOME="$FIXTURE_ROOT/$peer"
XDG_CONFIG_HOME="$HOME/.config"
XDG_STATE_HOME="$HOME/.local/state"
XDG_CACHE_HOME="$HOME/.cache"
XDG_DATA_HOME="$HOME/.local/share"
export HOME XDG_CONFIG_HOME XDG_STATE_HOME XDG_CACHE_HOME XDG_DATA_HOME
exec /bin/sh -c "$*"
"#,
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// One CLI observer call with the fixture's fake ssh first on PATH.
fn observer(home: &Path, root: &Path, bin: &Path, args: &[&str]) -> Value {
    let output = command(home)
        .env("FIXTURE_ROOT", root)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// The gap this pins: a gateway's poll snapshot refreshes on its own
/// interval while `/v1/federation/list` discovers on request, so a peer
/// added after startup was listable but not readable — the qualified
/// detail read 404ed against the stale snapshot until the next tick. Both
/// reads share the discovery engine now, and the publication policy bounds
/// the refresh exactly as it bounds the poll: the hidden row stays 404.
#[test]
fn a_peer_added_after_startup_is_readable_before_the_next_poll() {
    let root = tempfile::tempdir().unwrap();
    let b = home(root.path(), "B", 2);
    session(&b, "fresh", "Fresh transcript");
    session(&b, "hidden", "Mystery transcript");
    session(&b, "dup", "Fresh duplicate one");
    session(&b, "dup-two", "Fresh duplicate two");
    let bs = server(&b, false);
    let a = home(root.path(), "A", 3);
    session(&a, "a-local", "Fresh local row");
    // One minute of poll: no tick rescans anything during the test.
    let gateway = server_options(&a, true, "60", &["-q", "fresh"]);
    let startup = get(&gateway, "/v1/sessions", &[("limit", "0")]);
    assert_eq!(startup.1["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(startup.1["sessions"][0]["id"], "a-local");
    // The peer appears only now, after the startup snapshot was built.
    peers(&a, json!([peer("late", &bs)]));
    let scan = get(
        &gateway,
        "/v1/federation/list",
        &[("path", "[]"), ("hops", "8")],
    );
    assert_eq!(scan.0, 200);
    assert_eq!(scan.1["sessions"].as_array().unwrap().len(), 4);
    // The immediate qualified reads work before any poll tick...
    let detail = get(
        &gateway,
        "/v1/sessions/fresh",
        &[("origin", &origin(2)), ("source", "omp")],
    );
    assert_eq!(detail.0, 200);
    assert_eq!(detail.1["id"], "fresh");
    let transcript = get(
        &gateway,
        "/v1/sessions/fresh/transcript",
        &[("origin", &origin(2)), ("source", "omp")],
    );
    assert_eq!(transcript.0, 200);
    assert_eq!(transcript.1["title"], "Fresh transcript");
    // ...while the poll snapshot is still the startup one: the reads were
    // served by request-local discovery, not by a lucky tick.
    let stale = get(&gateway, "/v1/sessions", &[("limit", "0")]);
    assert_eq!(stale.1["sessions"].as_array().unwrap().len(), 1);
    // The base query hides `hidden` from the listing and from the refresh
    // alike: a qualified read cannot learn a row the policy filters.
    for path in ["/v1/sessions/hidden", "/v1/sessions/hidden/transcript"] {
        assert_eq!(
            get(&gateway, path, &[("origin", &origin(2)), ("source", "omp")]).0,
            404,
            "{path}"
        );
    }
    // No fall-through: a miss without both qualifiers, a malformed
    // qualifier, and an ambiguity the refresh itself found each keep
    // their own answer instead of guessing a row.
    assert_eq!(get(&gateway, "/v1/sessions/fresh", &[]).0, 404);
    assert_eq!(
        get(
            &gateway,
            "/v1/sessions/fresh",
            &[("origin", "not-a-uuid"), ("source", "omp")]
        )
        .0,
        400
    );
    assert_eq!(
        get(
            &gateway,
            "/v1/sessions/du",
            &[("origin", &origin(2)), ("source", "omp")]
        )
        .0,
        400,
        "two id-prefix matches behind one origin and source"
    );
}

/// An HTTP first hop with an ssh tail: the observer reads through the
/// gateway's HTTP endpoint, and the gateway follows its own ssh peer to
/// the origin. Two CLI observer calls, one server, and the ssh shim stand
/// in for the whole chain.
#[test]
fn an_http_hop_delegates_the_ssh_leg_to_the_gateway() {
    let root = tempfile::tempdir().unwrap();
    let bin = ssh_shim(root.path());
    let c = home(root.path(), "C", 2);
    session(&c, "mixed", "SSH leaf transcript behind an HTTP gateway");
    let a = home(root.path(), "A", 3);
    peers(
        &a,
        json!([{"name": "ssh-leaf", "ssh": "C", "origin_id":origin(2), "sources": ["omp"]}]),
    );
    let gateway = ssh_server(&a, root.path(), &bin);
    let l = home(root.path(), "L", 1);
    peers(&l, json!([peer("gateway", &gateway)]));
    let rows = observer(
        &l,
        root.path(),
        &bin,
        &["--read-only", "list", "--json", "--limit", "0"],
    );
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["origin_id"], origin(2));
    let hops = rows[0]["routes"][0]["hops"].as_array().unwrap();
    assert_eq!(hops.len(), 2);
    assert_eq!(hops[0]["url"], json!(gateway.url), "the first leg is HTTP");
    assert_eq!(hops[1]["ssh"], json!("C"), "the origin leg is ssh");
    let transcript = observer(
        &l,
        root.path(),
        &bin,
        &[
            "--read-only",
            "show",
            "mixed",
            "--origin",
            &origin(2),
            "--source",
            "omp",
            "--json",
        ],
    );
    assert_eq!(
        transcript["title"],
        "SSH leaf transcript behind an HTTP gateway"
    );
}

/// Three origins in a ring that mixes transports — HTTP both ways between
/// the gateway and one leaf, ssh one way and HTTP back the other — and an
/// observer holding a direct alias to each. Discovery stops at the cycle,
/// every alias collapses into one identity per session, the observer's
/// query bounds the federated rows, and the guarded endpoints keep the
/// caller's path and hop budget.
#[test]
fn mixed_transport_cycles_terminate_and_keep_alias_identity() {
    let root = tempfile::tempdir().unwrap();
    let bin = ssh_shim(root.path());
    let b = home(root.path(), "B", 2);
    session(&b, "b-row", "Beta cycle row");
    // A gateway already; its peer file is rewritten below to close the ring.
    let bs = server(&b, true);
    let c = home(root.path(), "C", 4);
    session(&c, "c-row", "Gamma cycle row");
    let a = home(root.path(), "A", 3);
    session(&a, "a-row", "Alpha cycle row");
    peers(
        &a,
        json!([peer("b-http", &bs), {"name": "c-ssh", "ssh": "C", "origin_id":origin(4), "sources": ["omp"]}]),
    );
    let gateway = ssh_server(&a, root.path(), &bin);
    // Close the ring: B answers A over HTTP, C answers A over HTTP while
    // A reaches C over ssh.
    peers(&b, json!([peer("cycle-a", &gateway)]));
    peers(&c, json!([peer("cycle-a", &gateway)]));
    let l = home(root.path(), "L", 1);
    peers(
        &l,
        json!([
            peer("a-gateway", &gateway),
            peer("direct-b", &bs),
            {"name": "direct-c", "ssh": "C", "origin_id":origin(4), "sources": ["omp"]}
        ]),
    );
    let run = |args: &[&str]| observer(&l, root.path(), &bin, args);
    let rows = run(&["--read-only", "list", "--json", "--limit", "0"]);
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 3, "one identity per session, not per alias");
    let by = |id: &str| rows.iter().find(|row| row["id"] == id).unwrap();
    // Each session is reachable through all three first hops; the routes
    // merge onto one row, shortest first, named by the direct alias.
    for (id, host) in [
        ("a-row", "a-gateway"),
        ("b-row", "direct-b"),
        ("c-row", "direct-c"),
    ] {
        let row = by(id);
        assert_eq!(row["routes"].as_array().unwrap().len(), 3, "{id}");
        assert_eq!(
            row["routes"][0]["hops"].as_array().unwrap().len(),
            1,
            "{id}: the direct route is preferred"
        );
        assert_eq!(row["host"], json!(host), "{id}");
    }
    // The observer's own query bounds which federated rows it lists.
    let filtered = run(&[
        "--read-only",
        "list",
        "--json",
        "--limit",
        "0",
        "-q",
        "gamma",
    ]);
    assert_eq!(filtered.as_array().unwrap().len(), 1);
    assert_eq!(filtered[0]["id"], "c-row");
    // Qualified reads resolve through the ring over both transports.
    for (id, number, title) in [
        ("b-row", 2, "Beta cycle row"),
        ("c-row", 4, "Gamma cycle row"),
    ] {
        let transcript = run(&[
            "--read-only",
            "show",
            id,
            "--origin",
            &origin(number),
            "--source",
            "omp",
            "--json",
        ]);
        assert_eq!(transcript["title"], json!(title));
    }
    // The guarded endpoint honors the caller's ancestors: with B and C
    // already visiting, A answers with its own row alone, and a zero hop
    // budget reports truncation instead of recursing through either peer.
    let visited = serde_json::to_string(&vec![origin(2), origin(4)]).unwrap();
    let scoped = get(
        &gateway,
        "/v1/federation/list",
        &[("path", &visited), ("hops", "8")],
    );
    assert_eq!(scoped.0, 200);
    assert_eq!(scoped.1["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(scoped.1["sessions"][0]["id"], "a-row");
    let truncated = get(
        &gateway,
        "/v1/federation/list",
        &[("path", "[]"), ("hops", "0")],
    );
    assert_eq!(truncated.0, 200);
    assert_eq!(truncated.1["sessions"].as_array().unwrap().len(), 1);
    let failures = truncated.1["failures"].as_array().unwrap();
    assert!(
        failures
            .iter()
            .any(|f| f.as_str().unwrap().contains("truncat")),
        "{failures:?}"
    );
}

#[test]
fn resume_keeps_the_ssh_alternate_after_http_dedup_prefers_the_http_route() {
    use llm_sessions::model::SessionSummary;
    use std::os::unix::fs::PermissionsExt;
    // One origin, both transports: an HTTP server and a fixture ssh alias
    // resolving to the same home. After the merge deduplicates the two
    // observations, the read-preferred route is the http hop; a terminal
    // still has to travel the ssh one, with quoting intact.
    let root = tempfile::tempdir().unwrap();
    let b = home(root.path(), "B", 2);
    session(&b, "mixed id 'quoted'", "Both transports, one origin");
    let bs = server(&b, false);
    let l = home(root.path(), "L", 1);
    let bin = ssh_shim(root.path());
    peers(
        &l,
        json!([
            {"name":"api","url":bs.url,"origin_id":origin(2),"token_env":SECRET_ENV,"sources":["omp"]},
            {"name":"ssh-direct","ssh":"B","origin_id":origin(2),"sources":["omp"]}
        ]),
    );
    let rows: Vec<SessionSummary> = serde_json::from_value(observer(
        &l,
        root.path(),
        &bin,
        &["--read-only", "list", "--json", "--limit", "0"],
    ))
    .unwrap();
    assert_eq!(rows.len(), 1, "one origin, one canonical session");
    assert_eq!(rows[0].routes.len(), 2, "both transports survive the merge");
    assert!(
        rows[0]
            .preferred_route()
            .unwrap()
            .hops
            .iter()
            .any(|hop| hop.url.is_some()),
        "the read-preferred route is the http hop"
    );
    let sources = llm_sessions::source::all(&l);
    let resume = llm_sessions::source::remote::resume_command(&rows[0], &sources)
        .expect("the ssh alternative keeps resume available");
    assert_eq!(&resume[..4], &["ssh", "-t", "--", "B"]);
    fs::write(
        bin.join("omp"),
        "#!/bin/sh\nprintf '%s\\n%s\\n' \"$HOME\" \"$*\"\n",
    )
    .unwrap();
    fs::set_permissions(bin.join("omp"), fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(&resume[0])
        .args(&resume[1..])
        .env("FIXTURE_ROOT", root.path())
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("{}\n--resume mixed id 'quoted'", b.display()),
        "the quoted session id survives every ssh shell layer and lands on B"
    );
}

#[test]
fn http_origin_refresh_rejects_mismatch_then_recovers_without_serving_cached_old_identity() {
    let root = tempfile::tempdir().unwrap();
    let b = home(root.path(), "B", 2);
    session(&b, "pinned", "Pinned transcript");
    let bs = server(&b, false);
    let l = home(root.path(), "L", 1);
    peers(&l, json!([peer("worker", &bs)]));
    let gateway = server_options(&l, true, "3600", &[]);
    let old_query = [("origin", origin(2)), ("source", "omp".to_owned())];
    let read = |query: &[(&str, String)]| {
        let query: Vec<_> = query
            .iter()
            .map(|(key, value)| (*key, value.as_str()))
            .collect();
        get(&gateway, "/v1/sessions/pinned/transcript", &query)
    };
    assert_eq!(read(&old_query).0, 200);
    fs::write(b.join(".local/state/llm-sessions/origin-id"), origin(3)).unwrap();
    let mismatch = command(&l)
        .args(["--read-only", "list", "--json", "--limit", "0"])
        .output()
        .unwrap();
    assert!(mismatch.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&mismatch.stdout).unwrap(),
        json!([])
    );
    assert!(String::from_utf8_lossy(&mismatch.stderr).contains("origin mismatch"));
    let refreshed = command(&l)
        .args(["hosts", "refresh", "worker"])
        .output()
        .unwrap();
    assert!(
        refreshed.status.success(),
        "{}",
        String::from_utf8_lossy(&refreshed.stderr)
    );
    let saved: Value =
        serde_json::from_slice(&fs::read(l.join(".config/llm-sessions/hosts.json")).unwrap())
            .unwrap();
    assert_eq!(saved["hosts"][0]["origin_id"], origin(3));
    assert_ne!(
        read(&old_query).0,
        200,
        "a pre-refresh cached transcript must not bypass the new pin"
    );
    let current = read(&[("origin", origin(3)), ("source", "omp".to_owned())]);
    assert_eq!(current.0, 200);
    assert_eq!(current.1["id"], "pinned");
}
