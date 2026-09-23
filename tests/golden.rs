//! Regression tests against a redacted corpus of real transcripts.
//!
//! Nine adapters parse nine private formats that change without notice. Unit
//! fixtures written by hand only encode what the author believed a format to
//! be; these files were taken from real stores and redacted, so a format
//! shift shows up as a failing assertion instead of a silent regression.
//!
//! See `tests/golden/README.md` for provenance and redaction rules.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use llm_sessions::model::{Block, Role, SessionSummary, Transcript};
use llm_sessions::source::{
    Source, claude, codex, copilot, cursor, gemini, goose, hermes, kilo, omp, openclaw, opencode,
};

/// Copy the checked-in home into a scratch directory.
///
/// Adapters open `SQLite` read-only but its journal machinery still wants a
/// writable directory, and a test must never mutate the corpus it asserts on.
fn scratch_home() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    copy_tree(&golden().join("home"), dir.path());
    materialise_sql(dir.path());
    dir
}

fn golden() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create");
    for entry in fs::read_dir(from).expect("read golden home").flatten() {
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

/// Build the SQLite-backed stores from their checked-in SQL text.
fn materialise_sql(home: &Path) {
    for (script, relative) in [
        ("opencode.sql", ".local/share/opencode/opencode.db"),
        ("kilo.sql", ".local/share/kilo/kilo.db"),
        ("copilot.sql", ".copilot/session-store.db"),
        ("hermes.sql", ".hermes/state.db"),
    ] {
        let path = home.join(relative);
        fs::create_dir_all(path.parent().expect("parent")).expect("create");
        let sql = fs::read_to_string(golden().join(script)).expect("read sql");
        let conn = rusqlite::Connection::open(&path).expect("create fixture db");
        conn.execute_batch(&sql).expect("apply fixture sql");
    }
}

/// `role:kind,kind|role:kind` — compact enough to paste into a test and
/// specific enough that a changed block mapping breaks it.
fn shape(transcript: &Transcript) -> String {
    transcript
        .turns
        .iter()
        .map(|turn| {
            let kinds: Vec<&str> = turn.blocks.iter().map(Block::kind).collect();
            format!("{}:{}", role(turn.role), kinds.join(","))
        })
        .collect::<Vec<_>>()
        .join("|")
}

fn role(role: Role) -> &'static str {
    match role {
        Role::User => "u",
        Role::Assistant => "a",
        Role::System => "s",
        Role::Tool => "t",
    }
}

fn only(
    sessions: Vec<SessionSummary>,
    predicate: impl Fn(&SessionSummary) -> bool,
) -> SessionSummary {
    let mut matching: Vec<SessionSummary> = sessions.into_iter().filter(|s| predicate(s)).collect();
    assert_eq!(matching.len(), 1, "expected exactly one matching session");
    matching.remove(0)
}

#[test]
fn every_adapter_reads_its_corpus() {
    let home = scratch_home();
    let home = home.path();
    let sources: Vec<Box<dyn Source>> = vec![
        Box::new(omp::Omp::new(home)),
        Box::new(claude::Claude::new(home)),
        Box::new(codex::Codex::new(home)),
        Box::new(opencode::OpenCode::new(home)),
        Box::new(copilot::Copilot::new(home)),
        Box::new(kilo::Kilo::new(home)),
        Box::new(goose::Goose::new(home)),
        Box::new(cursor::Cursor::new(home)),
        Box::new(gemini::Gemini::new(home)),
        Box::new(hermes::Hermes::new(home)),
        Box::new(openclaw::OpenClaw::new(home)),
    ];
    for source in &sources {
        let listed = source
            .list()
            .unwrap_or_else(|e| panic!("{}: {e}", source.id()));
        assert!(
            !listed.is_empty(),
            "{}: corpus produced no sessions",
            source.id()
        );
        for session in &listed {
            // Opening must never fail: an unreadable session is a bug, an
            // unrecognised record is a `Block::Unknown`.
            let transcript = source
                .open(&session.locator)
                .unwrap_or_else(|e| panic!("{} {}: {e}", source.id(), session.id));
            let _ = transcript.searchable_text();
        }
    }
}

#[test]
fn omp_nests_its_subagent_and_ignores_other_artifacts() {
    let home = scratch_home();
    let listed = omp::Omp::new(home.path()).list().expect("omp list");

    let parents: Vec<&SessionSummary> = listed.iter().filter(|s| !s.subagent).collect();
    let children: Vec<&SessionSummary> = listed.iter().filter(|s| s.subagent).collect();
    assert_eq!(parents.len(), 1, "one top-level session");
    assert_eq!(children.len(), 1, "one subagent child, draft.txt ignored");
    assert_eq!(children[0].parent.as_deref(), Some(parents[0].id.as_str()));
    assert_eq!(children[0].title.as_deref(), Some("GoldenTask"));
}

#[test]
fn omp_survives_a_record_type_it_has_never_seen() {
    let home = scratch_home();
    let source = omp::Omp::new(home.path());
    let session = only(source.list().expect("list"), |s| !s.subagent);
    let transcript = source.open(&session.locator).expect("open");

    // The corpus ends with `{"type":"from_the_future"}`.
    assert_eq!(
        transcript.unknown_records, 1,
        "the unknown record must be counted, not dropped"
    );
    assert!(
        transcript
            .turns
            .iter()
            .flat_map(|turn| &turn.blocks)
            .any(|block| matches!(block, Block::Unknown { .. })),
        "and kept as raw JSON"
    );
}

#[test]
fn a_truncated_final_record_does_not_lose_the_rest() {
    let home = scratch_home();
    let path = home.path().join(
        ".omp/agent/sessions/-workspace/2026-01-01T00-00-00-000Z_11111111-1111-4111-8111-111111111111.jsonl",
    );
    let whole = fs::read_to_string(&path).expect("read");
    let source = omp::Omp::new(home.path());
    let before = source
        .open(&llm_sessions::Locator::File { path: path.clone() })
        .expect("open whole")
        .turns
        .len();

    // A session being written right now ends mid-object.
    fs::write(&path, format!("{whole}{{\"type\":\"message\",\"mess")).expect("write");
    let after = source
        .open(&llm_sessions::Locator::File { path })
        .expect("open truncated");
    assert!(
        after.turns.len() >= before,
        "a half-written tail must not discard the completed turns"
    );
}

#[test]
fn codex_reads_both_its_generations() {
    let home = scratch_home();
    let source = codex::Codex::new(home.path());
    let listed = source.list().expect("codex list");
    assert_eq!(
        listed.len(),
        2,
        "one modern rollout and one legacy flat file"
    );

    for session in &listed {
        let transcript = source.open(&session.locator).expect("open");
        assert!(
            !transcript.turns.is_empty(),
            "{}: no turns parsed",
            session.id
        );
    }
}

#[test]
fn codex_does_not_multiply_its_running_token_total() {
    let home = scratch_home();
    let source = codex::Codex::new(home.path());
    let modern = only(source.list().expect("list"), |s| {
        matches!(&s.locator, llm_sessions::Locator::File { path }
            if path.to_string_lossy().contains("2026/01/01"))
    });
    let transcript = source.open(&modern.locator).expect("open");

    // `token_count` events are cumulative. Summing them would scale the real
    // figure by the number of events; only the last may be attributed.
    let counted = transcript
        .turns
        .iter()
        .filter(|turn| turn.usage.is_some())
        .count();
    assert!(counted <= 1, "{counted} turns carry a running total");
}

#[test]
fn opencode_reports_the_files_a_session_changed() {
    let home = scratch_home();
    let source = opencode::OpenCode::new(home.path());
    // One v1 session with `patch` parts, one v2 session whose snapshot
    // records paths relative to its project root.
    for session in source.list().expect("list") {
        let transcript = source.open(&session.locator).expect("open");
        let files = transcript.changed_files();
        assert!(
            !files.is_empty(),
            "{}: the corpus was chosen for its changed files",
            session.id
        );
        assert!(
            files.iter().all(|file| file.path.starts_with("/workspace")),
            "{}: changed files resolve under the workspace",
            session.id
        );
    }
}

/// v2's migration mirrors every v1 row into `session_v2`. The mirror must
/// list once, and a mirror without a `session_message` projection must still
/// open the v1 body rather than an empty one.
#[test]
fn opencode_lists_a_migrated_session_once_and_reads_v2_sessions() {
    let home = scratch_home();
    let source = opencode::OpenCode::new(home.path());
    let sessions = source.list().expect("list");
    let mut ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, ["ses_fixture_v1", "ses_fixture_v2"]);

    let migrated = only(sessions.clone(), |s| s.id == "ses_fixture_v1");
    assert!(
        !source
            .open(&migrated.locator)
            .expect("open")
            .turns
            .is_empty()
    );

    let v2 = only(sessions, |s| s.id == "ses_fixture_v2");
    assert_eq!(v2.model.as_deref(), Some("glm-5.3-flash"));
    assert_eq!(v2.usage.map(|u| u.input), Some(146_904));
    let transcript = source.open(&v2.locator).expect("open");
    assert!(transcript.first_user_prompt().is_some());
    assert!(transcript.usage().is_some_and(|u| u.output > 0));
}

#[test]
fn claude_keeps_a_named_branch_and_drops_a_detached_head() {
    let home = scratch_home();
    let listed = claude::Claude::new(home.path()).list().expect("list");
    let session = &listed[0];
    assert_eq!(session.branch.as_deref(), Some("feat/golden"));
}

#[test]
fn cursor_resolves_its_workspace_from_repo_json() {
    let home = scratch_home();
    let listed = cursor::Cursor::new(home.path()).list().expect("list");
    let session = &listed[0];
    assert_eq!(
        session.recorded_cwd.as_deref(),
        Some(Path::new("/workspace")),
        "repo.json is authoritative; the directory name is not"
    );
}

/// Compact snapshots of the whole corpus.
///
/// `role:kind,kind|role:kind` per turn. The point is not the string itself
/// but that a change to any adapter's block mapping shows up here as a diff
/// naming the agent, instead of quietly altering what every other feature
/// sees.
#[test]
fn normalized_shapes_are_stable() {
    let home = scratch_home();
    let home = home.path();
    let sources: Vec<Box<dyn Source>> = vec![
        Box::new(omp::Omp::new(home)),
        Box::new(claude::Claude::new(home)),
        Box::new(codex::Codex::new(home)),
        Box::new(opencode::OpenCode::new(home)),
        Box::new(goose::Goose::new(home)),
        Box::new(cursor::Cursor::new(home)),
        Box::new(gemini::Gemini::new(home)),
        Box::new(kilo::Kilo::new(home)),
        Box::new(copilot::Copilot::new(home)),
        Box::new(hermes::Hermes::new(home)),
        Box::new(openclaw::OpenClaw::new(home)),
    ];

    let mut seen: Vec<(String, usize, usize, String)> = Vec::new();
    for source in &sources {
        for session in source.list().expect("list") {
            let transcript = source.open(&session.locator).expect("open");
            seen.push((
                format!("{}/{}", source.id(), session.id),
                transcript.unknown_records,
                transcript.changed_files().len(),
                shape(&transcript),
            ));
        }
    }
    seen.sort();

    let expected: &[(&str, usize, usize, &str)] = &[
        (
            "claude/33333333-3333-4333-8333-333333333333",
            0,
            0,
            "s:text|s:text|u:text|u:text|u:text|u:text|a:thinking|a:text|a:tool|a:tool|t:result|u:text",
        ),
        ("codex/00000000-0000-4000-8000-000000000001", 0, 0, "u:text"),
        (
            "codex/44444444-4444-4444-8444-444444444444",
            0,
            0,
            "s:text,text,text|s:text|s:text|u:text|u:text|a:text|a:tool|t:result|a:tool|t:result|a:tool|t:result",
        ),
        ("copilot/0688c579-6aae-4941-a1e9-a48d30fe5c82", 0, 0, ""),
        (
            "cursor/66666666-6666-4666-8666-666666666666",
            0,
            0,
            "u:text|a:text|a:text|a:text|a:text|a:text|a:text|a:text|a:text|a:text",
        ),
        (
            "gemini/golden",
            0,
            0,
            "u:text|a:text,thinking|a:text,thinking,thinking,thinking|a:text,thinking,thinking,thinking,thinking|a:text,thinking|a:text,thinking|a:text,thinking|a:text,thinking|a:text,thinking|a:text,thinking|a:text,thinking|a:text,thinking|a:text,thinking|a:text,thinking|a:text,thinking,thinking|a:text,thinking,thinking|a:text,thinking,thinking|a:text,thinking,thinking|a:text,thinking,thinking|a:text,thinking|a:text,thinking,thinking|a:text,thinking|a:text,thinking,thinking|a:text,thinking|a:text,thinking|a:text,thinking|a:text,thinking|a:text,thinking,thinking|a:text,thinking|a:text,thinking|a:text,thinking|a:text,thinking,thinking|u:text|a:text,thinking|a:text|a:text|a:text|a:text|a:text|a:text|a:text|a:text|a:text|a:text|u:text|a:text,thinking,thinking|a:text|a:text|a:text|a:text|a:text|a:text|a:text|a:text|a:text|a:text|a:text|u:text|a:text,thinking,thinking,thinking|a:text",
        ),
        (
            "goose/20260101_000000",
            0,
            0,
            "u:text|a:text|u:text|a:text|u:text|a:tool|t:result|a:tool|t:result|a:tool|t:result|a:tool|t:result|a:tool|t:result|a:tool|t:result|a:tool|t:result",
        ),
        (
            "hermes/20260101_000000_plain",
            0,
            0,
            "u:text|a:thinking,text|a:tool|t:result|a:text",
        ),
        ("hermes/20260101_000001_child", 0, 0, "u:text|a:text"),
        (
            "kilo/ses_2ffe21e3affeR8H1RfNW2u75G1",
            0,
            0,
            "u:file,text,text,text",
        ),
        (
            "omp/11111111-1111-4111-8111-111111111111",
            1,
            0,
            "s:text|a:thinking,tool,tool,thinking,tool,tool,thinking,tool,tool,tool,thinking,tool,tool,thinking,text,thinking,tool,thinking,tool,tool,thinking,text|t:result|t:result|t:result|t:result|t:result|s:raw",
        ),
        (
            "omp/22222222-2222-4222-8222-222222222222",
            0,
            0,
            "u:text|a:thinking,tool,tool,tool,tool,tool,tool|t:result|t:result|t:result|t:result|t:result|t:result",
        ),
        (
            "openclaw/77777777-7777-4777-8777-777777777777",
            0,
            0,
            "u:text|a:thinking,text|a:tool|u:result",
        ),
        (
            "opencode/ses_fixture_v1",
            0,
            1,
            "a:patch,thinking,tool,result|a:thinking,tool,result|a:thinking,thinking,thinking,thinking,tool,result|a:thinking,tool,result|a:thinking,tool,result|a:thinking,thinking,text",
        ),
        (
            "opencode/ses_fixture_v2",
            0,
            37,
            "u:text|a:tool,result|a:thinking,tool,result|a:tool,result|a:tool,result|a:thinking,text,tool,result,tool,result|a:tool,result,tool,result|a:thinking,tool,result|a:thinking,text|s:text|s:text|s:text|s:text|u:text|a:thinking,tool,result|a:tool,result|a:thinking,text,tool,result|a:thinking,text,tool,result|a:thinking,text|u:text|a:thinking,text|s:text|s:text|s:text|u:text|a:thinking,tool,result|a:thinking,tool,result|a:tool,result,patch|s:text|a:thinking,text,tool,result|a:tool,result|a:thinking,text,tool,result,tool,result|a:thinking,tool,result,tool,result|a:thinking,text,tool,result|a:tool,result|a:thinking,text",
        ),
    ];

    assert_eq!(seen.len(), expected.len(), "corpus size changed");
    for (actual, want) in seen.iter().zip(expected) {
        assert_eq!(actual.0, want.0, "session order");
        assert_eq!(actual.1, want.1, "{}: unknown record count", want.0);
        assert_eq!(actual.2, want.2, "{}: changed files", want.0);
        assert_eq!(actual.3, want.3, "{}: normalized shape", want.0);
    }
}

/// The composed pipeline, on the real corpus: scan → refresh → read back.
///
/// The adapter tests stop at the `Source` interface; this one proves the
/// index decodes every source it stores. A source the reader cannot name
/// fails the whole read side — Hermes and `OpenClaw` once did, silently,
/// because a second hand-written inventory had drifted.
#[test]
fn indexed_corpus_reads_back_for_every_source() {
    let home = scratch_home();
    let home = home.path();
    let scan =
        llm_sessions::catalog::scan::scan_with(&llm_sessions::source::all(home), None, None, &[]);
    let sources = llm_sessions::source::all(home);
    let dir = tempfile::tempdir().expect("tempdir");
    let mut index = llm_sessions::storage::index::Index::open(&dir.path().join("index.db"))
        .expect("open index");
    let stats = index.refresh(&scan.sessions, &sources).expect("refresh");
    assert_eq!(stats.failed, 0, "every corpus session must index");

    index.usage().expect("usage read side decodes every row");
    let hits = index.search("directory", 50).expect("search read side");
    assert!(
        hits.iter()
            .any(|hit| hit.source == llm_sessions::model::SourceId::OpenClaw),
        "the corpus's OpenClaw body must be searchable"
    );
}

#[test]
fn opencode_remote_sql_falls_back_for_single_generation_stores() {
    for (schema, drop_tables, expected_query) in [
        (
            "v1 only",
            "DROP TABLE session_message; DROP TABLE session_v2;",
            2,
        ),
        (
            "v2 only",
            "DELETE FROM session_v2 WHERE id = 'ses_fixture_v1';
             DROP TABLE part; DROP TABLE message; DROP TABLE session;",
            1,
        ),
    ] {
        let home = scratch_home();
        let source = opencode::OpenCode::new(home.path());
        let (db_rel, queries) = source.remote_layout().sql.expect("OpenCode remote SQL");
        let db = home.path().join(db_rel);
        let conn = rusqlite::Connection::open(&db).expect("open fixture db");
        conn.execute_batch(drop_tables).expect("restrict schema");
        let (chosen, mut statement) = queries
            .iter()
            .enumerate()
            .find_map(|(index, query)| conn.prepare(query).ok().map(|stmt| (index, stmt)))
            .expect("a remote query matches the schema");
        assert_eq!(chosen, expected_query, "{schema}: fallback query");
        let remote_ids: Vec<String> = statement
            .query_map([], |row| row.get(0))
            .expect("remote query runs")
            .collect::<rusqlite::Result<_>>()
            .expect("remote ids");
        assert_eq!(remote_ids.len(), 1, "{schema}: one session");
        let sessions = source.list().expect("local list");
        assert_eq!(sessions.len(), 1, "{schema}: one local session");
        assert_eq!(remote_ids[0], sessions[0].id, "{schema}: same session");
        assert!(
            !source
                .open(&sessions[0].locator)
                .expect("open single-generation session")
                .turns
                .is_empty(),
            "{schema}: transcript is readable"
        );
    }
}

/// The shell transport's SQL contract, executed rather than trusted.
///
/// Each SQL-backed adapter mirrors its listing query in `remote_layout()`;
/// until now that mirror was kept in sync with the local decoder by
/// comment. This runs the mirrored SELECT against the golden fixture
/// database and feeds the rows through the real shell-listing parser, so a
/// schema change that breaks one side breaks this test instead of a remote
/// listing on somebody else's machine.
#[test]
fn remote_listing_sql_round_trips_every_sql_source() {
    let home = scratch_home();
    let home = home.path();
    let mut dbs = std::collections::HashMap::new();
    let mut checked = std::collections::HashSet::new();
    for source in llm_sessions::source::all(home) {
        let Some((db_rel, queries)) = source.remote_layout().sql else {
            continue;
        };
        let db = home.join(db_rel);
        let remote_db = Path::new("/srv/agent").join(db_rel);
        dbs.insert(source.id(), remote_db.clone());
        let conn =
            rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .expect("open fixture db");
        // The host runs the alternatives in order until one succeeds.
        let mut statement = queries
            .iter()
            .find_map(|query| conn.prepare(query).ok())
            .expect("a REMOTE_LIST_SQL alternative parses");
        let columns: Vec<String> = statement
            .column_names()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let mut rows = statement.query([]).expect("REMOTE_LIST_SQL runs");
        let mut records = String::new();
        while let Some(row) = rows.next().expect("read remote listing row") {
            let mut object = serde_json::Map::new();
            for (index, column) in columns.iter().enumerate() {
                let value = match row.get_ref(index).expect("cell") {
                    rusqlite::types::ValueRef::Null => serde_json::Value::Null,
                    rusqlite::types::ValueRef::Integer(i) => serde_json::Value::from(i),
                    rusqlite::types::ValueRef::Real(f) => serde_json::Value::from(f),
                    rusqlite::types::ValueRef::Text(t) => {
                        serde_json::Value::from(String::from_utf8_lossy(t).into_owned())
                    }
                    rusqlite::types::ValueRef::Blob(b) => {
                        serde_json::Value::from(String::from_utf8_lossy(b).into_owned())
                    }
                };
                object.insert(column.clone(), value);
            }
            let payload =
                serde_json::to_vec(&serde_json::Value::Array(vec![serde_json::Value::Object(
                    object,
                )]))
                .unwrap();
            writeln!(records, "S\t{}\t{}", source.id(), base64(&payload)).unwrap();
        }
        let sessions = llm_sessions::source::remote::parse_shell_listing(&records, "nx-1", &dbs);
        let expected: std::collections::HashSet<_> =
            source.list().unwrap().into_iter().map(|s| s.id).collect();
        let actual: std::collections::HashSet<_> = sessions.iter().map(|s| s.id.clone()).collect();
        assert_eq!(
            actual,
            expected,
            "{}: remote/local session IDs",
            source.id()
        );
        for session in sessions {
            assert_eq!(session.host.as_deref(), Some("nx-1"));
            assert_eq!(session.source, source.id());
            assert_eq!(
                session.locator,
                llm_sessions::Locator::Row {
                    db: remote_db.clone(),
                    id: session.id,
                },
                "remote bodies must reopen the configured database"
            );
        }
        checked.insert(source.id());
    }
    assert_eq!(
        checked,
        [
            llm_sessions::SourceId::OpenCode,
            llm_sessions::SourceId::Kilo,
            llm_sessions::SourceId::Hermes,
        ]
        .into_iter()
        .collect()
    );
}

/// Standard base64, as `base64` on the host emits.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}
