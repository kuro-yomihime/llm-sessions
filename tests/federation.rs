//! Real CLI instances connected by an isolated SSH adapter. No shared env mutation.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::process::{Command, Output};

use llm_sessions::model::{SessionSummary, Transcript};
use serde_json::{Value, json};

struct Graph {
    root: tempfile::TempDir,
}

impl Graph {
    fn new() -> Self {
        let graph = Self {
            root: tempfile::tempdir().expect("graph directory"),
        };
        let bin = graph.root.path().join("bin");
        fs::create_dir_all(&bin).expect("bin directory");
        symlink(env!("CARGO_BIN_EXE_llm-sessions"), bin.join("llm-sessions")).expect("CLI symlink");
        fs::write(
            bin.join("ssh"),
            r#"#!/bin/sh
while [ "$1" = '-o' ]; do shift 2; done
if [ "$1" = '-t' ]; then shift; fi
if [ "$1" = '--' ]; then shift; fi
peer=$1
shift
case "$peer" in
  b-alias) peer=B ;;
  unreachable) printf 'fixture host unreachable\n' >&2; exit 255 ;;
esac
printf '%s -> %s : %s\n' "$FEDERATION_HOST" "$peer" "$*" >> "$FIXTURE_ROOT/queries"
# A regression must fail rather than spawn an unbounded cycle.
if [ "$(wc -l < "$FIXTURE_ROOT/queries")" -gt 40 ]; then
  printf 'fixture discovery exceeded finite query bound\n' >&2
  exit 78
fi
HOME="$FIXTURE_ROOT/$peer"
XDG_CONFIG_HOME="$HOME/.config"
XDG_STATE_HOME="$HOME/.local/state"
XDG_CACHE_HOME="$HOME/.cache"
XDG_DATA_HOME="$HOME/.local/share"
FEDERATION_HOST="$peer"
export HOME XDG_CONFIG_HOME XDG_STATE_HOME XDG_CACHE_HOME XDG_DATA_HOME FEDERATION_HOST
if [ "$peer" = old ]; then
  PATH="$FIXTURE_ROOT/old-bin:$PATH"
  export PATH
fi
exec /bin/sh -c "$*"
"#,
        )
        .expect("SSH adapter");
        fs::set_permissions(bin.join("ssh"), fs::Permissions::from_mode(0o755))
            .expect("SSH executable");
        graph
    }

    fn home(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn origin(name: &str) -> String {
        let number = match name {
            "L" => 1,
            "A" => 2,
            "B" => 3,
            "C" => 4,
            "old" => 5,
            _ => panic!("unknown fixture origin"),
        };
        format!("00000000-0000-4000-8000-{number:012}")
    }

    fn host(&self, name: &str, peers: &[(&str, &str, &str)]) {
        let home = self.home(name);
        let state = home.join(".local/state/llm-sessions");
        let config = home.join(".config/llm-sessions");
        fs::create_dir_all(&state).expect("state directory");
        fs::create_dir_all(&config).expect("config directory");
        fs::write(state.join("origin-id"), format!("{}\n", Self::origin(name)))
            .expect("origin marker");
        let hosts: Vec<_> = peers
            .iter()
            .map(|(label, ssh, mode)| json!({"name":label,"ssh":ssh,"mode":mode,"sources":["omp"]}))
            .collect();
        fs::write(
            config.join("hosts.json"),
            json!({"hosts":hosts}).to_string(),
        )
        .expect("hosts config");
    }

    fn session(&self, host: &str, id: &str) {
        let directory = self.home(host).join(".omp/agent/sessions/workspace");
        fs::create_dir_all(&directory).expect("transcript directory");
        let records = [
            json!({"type":"session","version":3,"id":id,"timestamp":"2026-01-01T00:00:00Z","cwd":"/workspace","title":format!("Session from {host}")}),
            json!({"type":"message","id":"m1","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":[{"type":"text","text":format!("Content from {host}")}]}}),
        ];
        let body = records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(directory.join(format!("{id}.jsonl")), format!("{body}\n"))
            .expect("session transcript");
    }

    fn command(&self, host: &str, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_llm-sessions"))
            .args(arguments)
            .env("HOME", self.home(host))
            .env("XDG_CONFIG_HOME", self.home(host).join(".config"))
            .env("XDG_STATE_HOME", self.home(host).join(".local/state"))
            .env("XDG_CACHE_HOME", self.home(host).join(".cache"))
            .env("XDG_DATA_HOME", self.home(host).join(".local/share"))
            .env("FIXTURE_ROOT", self.root.path())
            .env("FEDERATION_HOST", host)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.path().join("bin").display()),
            )
            .output()
            .expect("run actual CLI")
    }

    fn list(&self, host: &str) -> (Vec<SessionSummary>, String) {
        let output = self.command(
            host,
            &[
                "--read-only",
                "list",
                "--json",
                "--limit",
                "0",
                "--archived",
            ],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        (
            serde_json::from_slice(&output.stdout).expect("session JSON"),
            String::from_utf8(output.stderr).expect("stderr UTF-8"),
        )
    }

    fn queries(&self) -> Vec<String> {
        fs::read_to_string(self.root.path().join("queries"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains("federation") && line.contains("list"))
            .map(str::to_owned)
            .collect()
    }
}

#[test]
fn direct_and_diamond_routes_preserve_origin_and_merge_before_counts() {
    let graph = Graph::new();
    graph.host(
        "L",
        &[
            ("alpha", "A", "auto"),
            ("direct-B", "b-alias", "auto"),
            ("gamma", "C", "auto"),
        ],
    );
    graph.host("A", &[("worker", "B", "auto")]);
    graph.host("C", &[("production", "B", "auto")]);
    graph.host("B", &[]);
    graph.session("B", "shared");
    let (rows, failures) = graph.list("L");
    assert!(failures.is_empty(), "{failures}");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.origin_id.as_deref(), Some(Graph::origin("B").as_str()));
    assert_eq!(row.origin_label.as_deref(), Some("direct-B"));
    assert_eq!(row.preferred_route().expect("route").hops.len(), 1);
    assert_eq!(
        row.routes.len(),
        3,
        "all successful independent routes retained"
    );
    let queries = graph.queries();
    assert!(queries.iter().any(|q| q.starts_with("A -> B")));
    assert!(queries.iter().any(|q| q.starts_with("C -> B")));
    let output = graph.command(
        "L",
        &["--read-only", "workspaces", "--json", "--limit", "0"],
    );
    assert!(output.status.success());
    let workspaces: Value = serde_json::from_slice(&output.stdout).expect("workspace JSON");
    assert_eq!(workspaces[0]["count"], 1);
}

#[test]
fn same_session_id_on_distinct_origins_remains_distinct_and_show_is_qualified() {
    let graph = Graph::new();
    graph.host("L", &[("A", "A", "auto"), ("B", "B", "auto")]);
    graph.host("A", &[]);
    graph.host("B", &[]);
    graph.session("A", "shared");
    graph.session("B", "shared");
    let (rows, _) = graph.list("L");
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0].origin_id, rows[1].origin_id);
    let ambiguous = graph.command("L", &["--read-only", "show", "shared", "--json"]);
    assert!(!ambiguous.status.success());
    let origin = Graph::origin("B");
    let output = graph.command(
        "L",
        &[
            "--read-only",
            "show",
            "shared",
            "--origin",
            &origin,
            "--source",
            "omp",
            "--json",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let transcript: Transcript = serde_json::from_slice(&output.stdout).expect("transcript JSON");
    assert!(
        serde_json::to_string(&transcript)
            .expect("serialize")
            .contains("Content from B")
    );
}

#[test]
fn indirectly_reachable_origin_supports_transcript_and_resume_route() {
    let graph = Graph::new();
    graph.host("L", &[("gateway", "A", "auto")]);
    graph.host("A", &[("worker", "B", "auto")]);
    graph.host("B", &[]);
    graph.session("B", "shared");
    let (rows, _) = graph.list("L");
    assert_eq!(rows[0].origin_label.as_deref(), Some("worker"));
    let output = graph.command("L", &["--read-only", "show", "shared", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let transcript: Transcript = serde_json::from_slice(&output.stdout).expect("transcript JSON");
    assert!(
        serde_json::to_string(&transcript)
            .expect("serialize")
            .contains("Content from B")
    );
    let sources = llm_sessions::source::all(&graph.home("L"));
    let resume =
        llm_sessions::source::remote::resume_command(&rows[0], &sources).expect("resume command");
    let bin = graph.root.path().join("bin");
    fs::write(
        bin.join("omp"),
        "#!/bin/sh\nprintf '%s:%s\\n' \"$FEDERATION_HOST\" \"$*\"\n",
    )
    .expect("agent adapter");
    fs::set_permissions(bin.join("omp"), fs::Permissions::from_mode(0o755))
        .expect("agent executable");
    let output = Command::new(&resume[0])
        .args(&resume[1..])
        .env("FIXTURE_ROOT", graph.root.path())
        .env("FEDERATION_HOST", "L")
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .output()
        .expect("exercise actual resume route");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "B:--resume shared"
    );
}

#[test]
fn callers_skip_self_two_node_and_longer_cycles_before_session_query() {
    for length in [1, 2, 3] {
        let graph = Graph::new();
        graph.host("L", &[("A", "A", "auto")]);
        match length {
            1 => graph.host("A", &[("self-alias", "A", "auto")]),
            2 => {
                graph.host("A", &[("B", "B", "auto")]);
                graph.host("B", &[("A", "A", "auto")]);
            }
            _ => {
                graph.host("A", &[("B", "B", "auto")]);
                graph.host("B", &[("C", "C", "auto")]);
                graph.host("C", &[("A", "A", "auto"), ("requester", "L", "auto")]);
            }
        }
        graph.session("A", "a-session");
        let (rows, failures) = graph.list("L");
        assert!(failures.is_empty(), "{failures}");
        assert_eq!(rows.len(), 1);
        let queries = graph.queries();
        assert_eq!(queries.len(), length, "queries: {queries:?}");
        assert!(!queries.iter().any(|q| q.starts_with("A -> A")
            || q.starts_with("B -> A")
            || q.starts_with("C -> A")
            || q.starts_with("C -> L")));
    }
}

#[test]
fn defensive_guard_and_depth_limit_do_not_hide_endpoint_local_sessions() {
    let graph = Graph::new();
    graph.host("A", &[("B", "B", "auto")]);
    graph.host("B", &[]);
    graph.session("A", "a-session");
    let path = json!([Graph::origin("A")]).to_string();
    let stopped = graph.command("A", &["federation", "list", "--path", &path, "--hops", "8"]);
    assert!(stopped.status.success());
    let payload: Value = serde_json::from_slice(&stopped.stdout).expect("guard JSON");
    assert_eq!(payload["sessions"], json!([]));
    assert!(graph.queries().is_empty());
    let limited = graph.command("A", &["federation", "list", "--path", "[]", "--hops", "0"]);
    assert!(limited.status.success());
    let payload: Value = serde_json::from_slice(&limited.stdout).expect("limited JSON");
    assert_eq!(payload["sessions"][0]["id"], "a-session");
    assert!(
        payload["failures"]
            .as_array()
            .expect("failures")
            .iter()
            .any(|f| f.as_str().expect("message").contains("hop limit"))
    );
    assert!(graph.queries().is_empty());
}

#[test]
fn shell_observers_share_origin_and_missing_marker_is_not_initialized() {
    let graph = Graph::new();
    graph.host("A", &[("one-label", "B", "shell")]);
    graph.host("C", &[("another-label", "b-alias", "shell")]);
    graph.host("B", &[]);
    graph.session("B", "shared");
    let (from_a, _) = graph.list("A");
    let (from_c, _) = graph.list("C");
    assert_eq!(from_a[0].identity(), from_c[0].identity());
    let (local, _) = graph.list("B");
    assert_eq!(from_a[0].identity(), local[0].identity());
    let marker = graph.home("B").join(".local/state/llm-sessions/origin-id");
    fs::remove_file(&marker).expect("remove fixture marker");
    let (rows, failures) = graph.list("A");
    assert!(rows.is_empty());
    assert!(failures.contains("origin"), "{failures}");
    assert!(
        !marker.exists(),
        "remote listing must not initialize identity"
    );
}

#[test]
fn older_peer_is_a_shell_leaf_and_failed_route_does_not_suppress_working_route() {
    let graph = Graph::new();
    graph.host(
        "L",
        &[
            ("old", "old", "auto"),
            ("broken", "unreachable", "auto"),
            ("B", "B", "auto"),
        ],
    );
    graph.host("old", &[("must-not-be-followed", "A", "auto")]);
    graph.host("B", &[]);
    graph.session("old", "old-session");
    graph.session("B", "b-session");
    let bin = graph.root.path().join("old-bin");
    fs::create_dir_all(&bin).expect("old bin");
    fs::write(
        bin.join("llm-sessions"),
        "#!/bin/sh\ncase \"$1\" in list|show|top) printf 'legacy-unguarded-query\\n' >> \"$FIXTURE_ROOT/queries\";; esac\nprintf 'old binary lacks guarded federation\\n' >&2\nexit 2\n",
    ).expect("old binary");
    fs::set_permissions(bin.join("llm-sessions"), fs::Permissions::from_mode(0o755))
        .expect("old executable");
    let (rows, failures) = graph.list("L");
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|r| r.id == "old-session"));
    assert!(rows.iter().any(|r| r.id == "b-session"));
    assert!(failures.contains("unreachable"), "{failures}");
    assert!(failures.contains("shell"), "{failures}");
    let log = fs::read_to_string(graph.root.path().join("queries")).expect("query log");
    assert!(
        !log.lines().any(|line| line.starts_with("old ->")),
        "old peer must remain a leaf"
    );
    assert!(
        !log.contains("legacy-unguarded-query"),
        "no unguarded legacy query"
    );
}

#[test]
fn delegated_show_rejects_wrong_origin_and_cyclic_route_before_querying() {
    let graph = Graph::new();
    graph.host("A", &[]);
    graph.session("A", "shared");
    let target = Graph::origin("B");
    let wrong = graph.command(
        "A",
        &[
            "federation",
            "show",
            "--origin",
            &target,
            "--source",
            "omp",
            "--id",
            "shared",
            "--route",
            "[]",
        ],
    );
    assert!(!wrong.status.success());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("federation route"));
    let route = json!([
        {"origin_id":Graph::origin("A"),"name":"self","ssh":"A"},
        {"origin_id":target,"name":"target","ssh":"B"}
    ])
    .to_string();
    let cyclic = graph.command(
        "A",
        &[
            "federation",
            "show",
            "--origin",
            &target,
            "--source",
            "omp",
            "--id",
            "shared",
            "--route",
            &route,
        ],
    );
    assert!(!cyclic.status.success());
    assert!(String::from_utf8_lossy(&cyclic.stderr).contains("federation route"));
    assert!(graph.queries().is_empty());
}

#[test]
fn configured_home_routes_keep_the_alternate_namespace_end_to_end() {
    let graph = Graph::new();
    let alternate = "alternate namespace";
    let alternate_home = graph.root.path().join("alternate home");
    let state = alternate_home.join(".local/state/llm-sessions");
    let directory = alternate_home.join(".omp/agent/sessions/workspace");
    fs::create_dir_all(&state).expect("alternate state directory");
    fs::create_dir_all(&directory).expect("alternate transcript directory");
    fs::write(state.join("origin-id"), format!("{}\n", Graph::origin("C")))
        .expect("alternate origin marker");
    fs::write(
        directory.join("shared.jsonl"),
        format!(
            "{{\"type\":\"session\",\"version\":3,\"id\":\"shared\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/workspace\",\"title\":\"Alternate\"}}\n{{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01Z\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"{alternate}\"}}]}}}}\n"
        ),
    )
    .expect("alternate transcript");
    // The login account and configured namespace have different identities.
    graph.host("B", &[]);
    graph.host("L", &[]);
    fs::write(
        graph.home("L").join(".config/llm-sessions/hosts.json"),
        json!({"hosts":[{
            "name":"alt", "ssh":"B", "home":alternate_home,
            "mode":"auto", "sources":["omp"]
        }]})
        .to_string(),
    )
    .expect("configured-home host");
    let (rows, failures) = graph.list("L");
    assert!(failures.is_empty(), "{failures}");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].origin_id.as_deref(),
        Some(Graph::origin("C").as_str())
    );
    assert_eq!(
        rows[0].preferred_route().expect("route").hops[0]
            .home
            .as_deref(),
        Some(alternate_home.as_path())
    );
    let output = graph.command("L", &["--read-only", "show", "shared", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let transcript: Transcript = serde_json::from_slice(&output.stdout).expect("transcript JSON");
    assert!(
        serde_json::to_string(&transcript)
            .expect("serialize")
            .contains(alternate),
        "delegated show must read the configured-home namespace, not the login account"
    );
    let bin = graph.root.path().join("bin");
    fs::write(
        bin.join("omp"),
        "#!/bin/sh\nprintf '%s\\n' \"$HOME\" \"$XDG_CONFIG_HOME\"\n",
    )
    .expect("agent adapter");
    fs::set_permissions(bin.join("omp"), fs::Permissions::from_mode(0o755))
        .expect("agent executable");
    let sources = llm_sessions::source::all(&graph.home("L"));
    let resume =
        llm_sessions::source::remote::resume_command(&rows[0], &sources).expect("resume command");
    let output = Command::new(&resume[0])
        .args(&resume[1..])
        .env("FIXTURE_ROOT", graph.root.path())
        .env("FEDERATION_HOST", "L")
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .output()
        .expect("resume configured namespace");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!(
            "{}\n{}\n",
            alternate_home.display(),
            alternate_home.join(".config").display()
        )
    );
}

#[test]
fn explicit_origin_initialization_survives_restart_and_read_only_refuses_creation() {
    let graph = Graph::new();
    fs::create_dir_all(graph.home("L")).expect("home");
    let refused = graph.command("L", &["--read-only", "origin", "--init"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--read-only refuses"));
    let marker = graph.home("L").join(".local/state/llm-sessions/origin-id");
    assert!(!marker.exists());
    let created = graph.command("L", &["origin", "--init"]);
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let reopened = graph.command("L", &["--read-only", "origin"]);
    assert!(reopened.status.success());
    assert_eq!(created.stdout, reopened.stdout);
    assert_eq!(fs::read(&marker).expect("marker"), created.stdout);
}
