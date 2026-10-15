//! The actual binary against an isolated home: subcommand surfaces whose
//! behavior is the contract.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};

fn run(home: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_llm-sessions"))
        .args(arguments)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        // No ssh on PATH: a configured host is reported, never contacted.
        .env("PATH", "/nonexistent")
        .output()
        .expect("run actual CLI")
}

fn ok(home: &Path, arguments: &[&str]) -> String {
    let output = run(home, arguments);
    assert!(
        output.status.success(),
        "{arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn omp_session(home: &Path, id: &str, text: &str) {
    let directory = home.join(".omp/agent/sessions/workspace");
    fs::create_dir_all(&directory).unwrap();
    let records = [
        json!({"type":"session","version":3,"id":id,"timestamp":"2026-01-01T00:00:00Z","cwd":"/workspace","title":format!("Session {id}")}),
        json!({"type":"message","id":"m1","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":[{"type":"text","text":text}]}}),
    ];
    let body: Vec<String> = records.iter().map(Value::to_string).collect();
    fs::write(
        directory.join(format!("{id}.jsonl")),
        body.join("\n") + "\n",
    )
    .unwrap();
}

fn hit_ids(stdout: &str) -> Vec<String> {
    let hits: Vec<Value> = serde_json::from_str(stdout).unwrap();
    let mut ids: Vec<String> = hits
        .iter()
        .map(|hit| hit["session_id"].as_str().unwrap().to_owned())
        .collect();
    ids.sort_unstable();
    ids
}

#[test]
fn search_applies_list_filters_after_ranking_and_zero_means_no_limit() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    omp_session(home, "first", "needle in the first haystack");
    omp_session(home, "second", "needle in the second haystack");
    ok(home, &["index"]);

    let all = ["first", "second"];
    assert_eq!(
        hit_ids(&ok(home, &["search", "needle", "--json", "--limit", "0"])),
        all
    );
    assert_eq!(
        hit_ids(&ok(home, &["search", "needle", "--json", "--limit", "1"])).len(),
        1
    );
    assert!(
        hit_ids(&ok(
            home,
            &["search", "needle", "--json", "--source", "codex"]
        ))
        .is_empty()
    );

    // Archived sessions leave search exactly as they leave list.
    ok(home, &["archive", "first"]);
    assert_eq!(
        hit_ids(&ok(home, &["search", "needle", "--json"])),
        ["second"]
    );
    assert_eq!(
        hit_ids(&ok(home, &["search", "needle", "--json", "--archived"])),
        all
    );
    // A filter that admits one session still fills the limit from the rest
    // of the ranking rather than from the unfiltered top hit.
    assert_eq!(
        hit_ids(&ok(
            home,
            &[
                "search",
                "needle",
                "--json",
                "--archived",
                "--limit",
                "1",
                "-q",
                "first"
            ]
        )),
        ["first"]
    );
}

#[test]
fn hosts_and_sources_report_separately_with_pins() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let config = home.join(".config/llm-sessions");
    fs::create_dir_all(&config).unwrap();
    let pinned = "00000000-0000-4000-8000-0000000000aa";
    fs::write(
        config.join("hosts.json"),
        json!({"hosts":[
            {"name":"pinned","ssh":"pinned.example","origin_id":pinned},
            {"name":"fresh","ssh":"fresh.example"},
        ]})
        .to_string(),
    )
    .unwrap();

    let hosts: Vec<Value> = serde_json::from_str(&ok(home, &["hosts", "--json"])).unwrap();
    let origins: Vec<(&str, &Value)> = hosts
        .iter()
        .map(|host| (host["name"].as_str().unwrap(), &host["origin_id"]))
        .collect();
    assert_eq!(
        origins,
        [("pinned", &json!(pinned)), ("fresh", &Value::Null)]
    );
    let table = ok(home, &["hosts"]);
    assert!(
        table
            .lines()
            .any(|line| line.starts_with("pinned") && line.contains(pinned))
    );
    assert!(
        table
            .lines()
            .any(|line| line.starts_with("fresh") && line.contains("unpinned"))
    );

    // `sources` is this machine's stores: hosts are `hosts`' business.
    let sources = ok(home, &["sources"]);
    assert!(!sources.contains("pinned.example"), "{sources}");
    let stores: Vec<Value> = serde_json::from_str(&ok(home, &["sources", "--json"])).unwrap();
    assert!(stores.iter().any(|store| store["agent"] == "omp"));
    assert!(
        stores
            .iter()
            .all(|store| store.get("destination").is_none())
    );
}
