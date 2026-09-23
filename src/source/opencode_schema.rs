//! The shared OpenCode/Kilo Drizzle schema reader.
//!
//! The two stores have the same `session`, `project`, `message`, and `part`
//! tables.  The inspected `OpenCode` `message.data` row includes
//! `{"role":"assistant","time":{"created":1771050837835},"modelID":"glm-5"}`;
//! a corresponding inspected tool part begins
//! `{"type":"tool","tool":"apply_patch","state":{"status":"completed",…}}`.
//! Keeping the decoding here prevents the two compatible databases from
//! drifting apart as either client adds part types.
//!
//! `OpenCode` v2 adds a second generation to the same file: `session_v2` and
//! `session_message`, decoded by [`super::opencode_v2`]. Its migration mirrors
//! every v1 `session` row into `session_v2`, and a fresh v2 install creates no
//! v1 tables at all, so this module decides per session which generation
//! answers: the one written last (see `v2_current!`).
//!
//! Neither inspected session schema has a branch column, so the tier-1
//! reader deliberately leaves [`SessionSummary::branch`] empty.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::Value;

use crate::error::{Error, Result, sql};
use crate::model::{
    ArchiveOutcome, Block, Change, FileChange, Locator, Role, SessionSummary, SourceId, Transcript,
    Turn, Usage,
};
use crate::source::{clean_title, from_epoch_guess, opencode_v2};
use crate::storage::sqlite;

/// Fixed column order for [`list`]'s row decoder. `Required` columns exist in
/// every migration of both clients; `Optional` ones do not — Kilo's `session`
/// table has `agent` and `model` but no `cost`/`tokens_*`. Selecting an
/// absent column would fail the whole source with "no such column" instead of
/// costing one field, so missing ones become `NULL`.
const LIST_COLUMNS: &[(&str, bool)] = &[
    ("s.id", true),
    ("s.title", true),
    ("s.directory", true),
    ("p.worktree", true),
    ("parent_id", false),
    ("agent", false),
    ("model", false),
    ("cost", false),
    ("tokens_input", false),
    ("tokens_output", false),
    ("s.time_created", true),
    ("s.time_updated", true),
    ("time_archived", false),
    // OpenCode records this cheap aggregate on the session row. Kilo's
    // historical schema genuinely omits these columns, hence the optional
    // projection rather than an unconditional SELECT.
    ("summary_additions", false),
    ("summary_deletions", false),
    ("summary_files", false),
];

/// A `session_v2` row (aliased `s`) answers for its id unless the v1 row is
/// strictly newer. Whether a v1 client still writes the mirror is not
/// observable, so recency decides: whichever generation was written last
/// holds the session's current state.
macro_rules! v2_current {
    () => {
        "NOT EXISTS (SELECT 1 FROM session o WHERE o.id = s.id AND o.time_updated > s.time_updated)"
    };
}

/// The complement of `v2_current!` for a `session` row aliased `s`. On equal
/// timestamps v2 wins: its mirror copied the v1 timestamp, and the `>=`
/// ensures that a mirrored id lists exactly once.
macro_rules! v1_current {
    () => {
        "NOT EXISTS (SELECT 1 FROM session_v2 o WHERE o.id = s.id AND o.time_updated >= s.time_updated)"
    };
}

/// The remote projection of one session table. Yields exactly the eleven
/// columns [`crate::source::remote`] parses, in order; column spellings mirror
/// [`LIST_COLUMNS`]. Optional local columns are simply `NULL` here: a host
/// without them loses nothing it had.
macro_rules! remote_select {
    ($table:literal) => {
        concat!(
            "SELECT s.id AS id, s.title AS title, ",
            "COALESCE(s.directory, p.worktree) AS cwd, ",
            "s.time_updated / 1000 AS updated_epoch, ",
            "NULL AS model, NULL AS input, NULL AS output, NULL AS cost, ",
            "s.parent_id AS parent, ",
            "CASE WHEN s.time_archived IS NULL THEN 0 ELSE 1 END AS archived, ",
            "NULL AS branch FROM ",
            $table,
            " s LEFT JOIN project p ON p.id = s.project_id"
        )
    };
}

/// Tier-1 SELECTs for the remote shell transport, where no `llm-sessions` runs
/// to answer and the schema cannot be inspected first. The host runs them in
/// order until one succeeds: both generations (a migrated v2 store), v2 alone
/// (a fresh v2 install), then v1 (Kilo and pre-v2 `OpenCode`). Each mirrors
/// [`list_query`] for the same tables, so the two cannot drift.
pub(crate) const REMOTE_LIST_SQL: &[&str] = &[
    concat!(
        remote_select!("session_v2"),
        " WHERE ",
        v2_current!(),
        " UNION ALL ",
        remote_select!("session"),
        " WHERE ",
        v1_current!()
    ),
    remote_select!("session_v2"),
    remote_select!("session"),
];

/// Which session-table generations a database carries.
#[derive(Clone, Copy, Debug)]
struct Tables {
    v1: bool,
    v2: bool,
}

impl Tables {
    fn of(conn: &Connection) -> rusqlite::Result<Self> {
        let mut stmt = conn.prepare(
            "SELECT name FROM sqlite_schema
             WHERE type = 'table' AND name IN ('session', 'session_v2', 'session_message')",
        )?;
        let names = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        Ok(Self {
            v1: names.contains("session"),
            v2: names.contains("session_v2") && names.contains("session_message"),
        })
    }
}

/// The tier-1 SELECT over one session table. `directory` is the session cwd
/// and is therefore preferred over the broader project worktree selected
/// alongside it.
fn table_query(conn: &Connection, table: &str) -> rusqlite::Result<String> {
    let mut present = HashSet::new();
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_info(?1)")?;
    for name in stmt.query_map([table], |row| row.get::<_, String>(0))? {
        present.insert(name?);
    }
    let columns: Vec<String> = LIST_COLUMNS
        .iter()
        .map(|&(name, required)| {
            if required {
                name.to_string()
            } else if present.contains(name) {
                format!("s.{name}")
            } else {
                format!("NULL AS {name}")
            }
        })
        .collect();
    Ok(format!(
        "SELECT {} FROM {table} s LEFT JOIN project p ON p.id = s.project_id",
        columns.join(", ")
    ))
}

/// The one tier-1 query for both Drizzle-backed sources, across whichever
/// generations the database carries.
fn list_query(conn: &Connection, tables: Tables) -> rusqlite::Result<String> {
    Ok(match (tables.v1, tables.v2) {
        (true, true) => format!(
            "{} WHERE {} UNION ALL {} WHERE {}",
            table_query(conn, "session_v2")?,
            v2_current!(),
            table_query(conn, "session")?,
            v1_current!()
        ),
        (false, true) => table_query(conn, "session_v2")?,
        _ => table_query(conn, "session")?,
    })
}

/// List database-backed sessions without parsing a message body.
pub(crate) fn list(db: &Path, source: SourceId) -> Result<Vec<SessionSummary>> {
    if !db.exists() {
        return Ok(Vec::new());
    }

    let (conn, _stale) = sqlite::open(db)?;
    let tables = Tables::of(&conn).map_err(sql(db))?;
    let query = list_query(&conn, tables).map_err(sql(db))?;
    let mut statement = conn.prepare(&query).map_err(sql(db))?;
    let rows = statement
        .query_map([], |row| {
            let cost: Option<f64> = row.get(7)?;
            let input: Option<i64> = row.get(8)?;
            let output: Option<i64> = row.get(9)?;
            let added: Option<i64> = row.get(13)?;
            let deleted: Option<i64> = row.get(14)?;
            let files: Option<i64> = row.get(15)?;
            let updated = row
                .get::<_, Option<i64>>(11)?
                .and_then(from_epoch_guess)
                .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
            Ok(SessionSummary {
                recorded_cwd: row
                    .get::<_, Option<String>>(2)?
                    .or(row.get::<_, Option<String>>(3)?)
                    .map(PathBuf::from),
                title: row
                    .get::<_, Option<String>>(1)?
                    .as_deref()
                    .and_then(clean_title),
                created: row.get::<_, Option<i64>>(10)?.and_then(from_epoch_guess),
                model: model_label(row.get::<_, Option<String>>(6)?.as_deref(), row.get(5)?),
                parent: row.get(4)?,
                archived: row.get::<_, Option<i64>>(12)?.is_some(),
                usage: (cost.is_some() || input.is_some() || output.is_some()).then(|| Usage {
                    input: u64::try_from(input.unwrap_or(0)).unwrap_or(0),
                    output: u64::try_from(output.unwrap_or(0)).unwrap_or(0),
                    cost: cost.filter(|cost| cost.is_finite()).unwrap_or(0.0),
                }),
                change: summary_change(files, added, deleted),
                ..SessionSummary::new(
                    row.get(0)?,
                    source,
                    updated,
                    Locator::Row {
                        db: db.to_path_buf(),
                        id: row.get(0)?,
                    },
                )
            })
        })
        .map_err(sql(db))?;

    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql(db))
}

/// Read a single session from whichever generation holds its body.
pub(crate) fn open(db: &Path, source: SourceId, id: &str) -> Result<Transcript> {
    let (conn, _stale) = sqlite::open(db)?;
    let tables = Tables::of(&conn).map_err(sql(db))?;
    if tables.v2 && (!tables.v1 || v2_holds_body(&conn, id).map_err(sql(db))?) {
        return opencode_v2::open(&conn, db, source, id);
    }
    open_v1(&conn, db, source, id)
}

/// Whether a database with both generations should read `id` from v2. The
/// v2 row must be current and carry a `session_message` projection: 59 of
/// 1951 migrated sessions on the inspected store have v1 messages but no
/// projection, and reading v2 for those would show an empty transcript. An
/// id with no v1 row at all can only be read from v2.
fn v2_holds_body(conn: &Connection, id: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        concat!(
            "SELECT EXISTS (SELECT 1 FROM session_v2 s WHERE s.id = ?1 AND ",
            v2_current!(),
            " AND EXISTS (SELECT 1 FROM session_message m WHERE m.session_id = s.id))",
            " OR NOT EXISTS (SELECT 1 FROM session WHERE id = ?1)"
        ),
        params![id],
        |row| row.get(0),
    )
}

/// Read a v1 session. The `(session_id, time_created)` indexes make the
/// message order cheap; parts are then ordered by their own creation time.
fn open_v1(conn: &Connection, db: &Path, source: SourceId, id: &str) -> Result<Transcript> {
    let (title, directory): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT title, directory FROM session WHERE id = ?1",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(sql(db))?
        .ok_or_else(|| Error::MissingRow {
            db: db.to_path_buf(),
            id: id.to_owned(),
        })?;

    let mut transcript = Transcript::new(id, source);
    transcript.title = title.as_deref().and_then(clean_title);
    transcript.workspace = directory.map(PathBuf::from);

    let mut statement = conn
        .prepare(
            "SELECT m.id, m.data, p.data
             FROM message m
             LEFT JOIN part p ON p.message_id = m.id AND p.session_id = m.session_id
             WHERE m.session_id = ?1
             ORDER BY m.time_created, p.time_created",
        )
        .map_err(sql(db))?;
    let rows = statement
        .query_map(params![id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(sql(db))?;

    let mut current_id = None;
    let mut current_turn = None;
    for row in rows {
        let (message_id, message_data, part_data) = row.map_err(sql(db))?;
        if current_id.as_deref() != Some(message_id.as_str()) {
            if let Some(turn) = current_turn.take() {
                transcript.push(turn);
            }
            current_id = Some(message_id);
            current_turn = Some(turn_from_message(
                &message_data,
                &mut transcript.unknown_records,
            ));
        }
        if let Some(part_data) = part_data
            && let Some(turn) = current_turn.as_mut()
        {
            append_part(turn, &part_data, &mut transcript.unknown_records);
        }
    }
    if let Some(turn) = current_turn {
        transcript.push(turn);
    }

    Ok(transcript)
}

/// Open a short read-write connection only for the reversible archive update.
/// `time_archived` is nullable in the inspected schema, so `NULL` means
/// unarchived. Both generations are updated atomically where the id exists,
/// so the state agrees whichever one a client or the listing reads. Bound
/// parameters keep opaque agent session ids out of SQL text.
pub(crate) fn set_archived(db: &Path, id: &str, archived: bool) -> Result<ArchiveOutcome> {
    let mut connection =
        Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_WRITE).map_err(sql(db))?;
    let tables = Tables::of(&connection).map_err(sql(db))?;
    let archived_at = archived.then(|| time::OffsetDateTime::now_utc().unix_timestamp() * 1000);
    let transaction = connection.transaction().map_err(sql(db))?;
    let mut changed = 0;
    if tables.v2 {
        changed += transaction
            .execute(
                "UPDATE session_v2 SET time_archived = ?1 WHERE id = ?2",
                params![archived_at, id],
            )
            .map_err(sql(db))?;
    }
    if tables.v1 {
        changed += transaction
            .execute(
                "UPDATE session SET time_archived = ?1 WHERE id = ?2",
                params![archived_at, id],
            )
            .map_err(sql(db))?;
    }
    if changed == 0 {
        return Err(Error::MissingRow {
            db: db.to_path_buf(),
            id: id.to_owned(),
        });
    }
    transaction.commit().map_err(sql(db))?;
    Ok(ArchiveOutcome::Native)
}

fn model_label(model: Option<&str>, agent: Option<String>) -> Option<String> {
    model
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|value| value.get("id")?.as_str().map(str::to_owned))
        .or(agent)
}

/// `OpenCode` and Kilo store assistant accounting with each message. A measured
/// `message.data` value includes `{"role":"assistant","cost":0,
/// "tokens":{"input":10223,"output":73,"reasoning":0,
/// "cache":{"read":448,"write":0}}}`. Cache and reasoning counters have no
/// corresponding [`Usage`] fields.
fn turn_from_message(raw: &str, unknown_records: &mut usize) -> Turn {
    let data = json_or_string(raw);
    let role_name = data.get("role").and_then(Value::as_str);
    #[expect(
        clippy::match_same_arms,
        reason = "an explicit system role and the unrecognised-role fallback are distinct cases; the latter is also recorded as unknown below"
    )]
    let role = match role_name {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        Some("system") => Role::System,
        Some("tool") => Role::Tool,
        _ => Role::System,
    };
    let mut turn = Turn::new(role);
    turn.timestamp = data
        .pointer("/time/created")
        .and_then(Value::as_i64)
        .and_then(from_epoch_guess);
    turn.model = data
        .get("modelID")
        .and_then(Value::as_str)
        .or_else(|| data.pointer("/model/modelID").and_then(Value::as_str))
        .map(str::to_owned);
    if role == Role::Assistant {
        turn.usage = opencode_usage(&data);
    }
    if !matches!(role_name, Some("user" | "assistant" | "system" | "tool")) {
        *unknown_records += 1;
        turn.blocks.push(Block::Unknown { raw: data });
    }
    turn
}

/// Per-message usage from `message.data`.
///
/// A message with no `tokens` object but a real `cost` still counts: the two
/// are recorded independently, and dropping the row would understate spend.
pub(super) fn opencode_usage(value: &Value) -> Option<Usage> {
    let cost = value.get("cost").and_then(Value::as_f64).unwrap_or(0.0);
    let input = value.pointer("/tokens/input").and_then(Value::as_u64);
    let output = value.pointer("/tokens/output").and_then(Value::as_u64);
    if input.is_none() && output.is_none() && value.get("cost").is_none() {
        return None;
    }
    Some(Usage {
        input: input.unwrap_or(0),
        output: output.unwrap_or(0),
        cost: if cost.is_finite() { cost } else { 0.0 },
    })
}

fn append_part(turn: &mut Turn, raw: &str, unknown_records: &mut usize) {
    let data = json_or_string(raw);
    match data.get("type").and_then(Value::as_str) {
        Some("text") => match string_at(&data, "/text") {
            Some(text) => turn.blocks.push(Block::text(text)),
            None => push_unknown(turn, data, unknown_records),
        },
        Some("reasoning") => match string_at(&data, "/text") {
            Some(text) => turn.blocks.push(Block::thinking(text)),
            None => push_unknown(turn, data, unknown_records),
        },
        Some("tool") => {
            let name = string_at(&data, "/tool").unwrap_or_else(|| "tool".to_owned());
            let state = data.get("state").cloned().unwrap_or(Value::Null);
            let input = state.get("input").cloned().unwrap_or(Value::Null);
            turn.blocks.push(Block::ToolCall {
                name: name.clone(),
                input,
            });
            match state.get("status").and_then(Value::as_str) {
                Some("completed") => turn.blocks.push(Block::ToolResult {
                    name: Some(name),
                    ok: true,
                    output: state_output(&state),
                }),
                Some("error") => turn.blocks.push(Block::ToolResult {
                    name: Some(name),
                    ok: false,
                    output: state_error(&state),
                }),
                _ => {}
            }
        }
        Some("file") => match file_path(&data) {
            Some(path) => turn.blocks.push(Block::File { path }),
            None => push_unknown(turn, data, unknown_records),
        },
        Some("patch") => turn.blocks.push(Block::Patch {
            summary: patch_summary(&data),
            files: patch_files(&data),
        }),
        // A delegated subagent run: the prompt is the interesting payload.
        Some("subtask") => turn.blocks.push(Block::ToolCall {
            name: format!(
                "subtask:{}",
                string_at(&data, "/agent").unwrap_or_else(|| "agent".to_owned())
            ),
            input: data.clone(),
        }),
        Some("compaction") => turn.blocks.push(Block::text(format!(
            "context compacted\n\n{}",
            string_at(&data, "/summary").unwrap_or_default()
        ))),
        Some("agent") => turn.blocks.push(Block::text(format!(
            "agent: {}",
            string_at(&data, "/name").unwrap_or_default()
        ))),
        // These measured lifecycle records do not represent transcript content.
        Some("step-start" | "step-finish") => {}
        _ => push_unknown(turn, data, unknown_records),
    }
}

pub(super) fn json_or_string(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned()))
}

fn push_unknown(turn: &mut Turn, raw: Value, unknown_records: &mut usize) {
    *unknown_records += 1;
    turn.blocks.push(Block::Unknown { raw });
}

/// Preserve empty text, including encrypted reasoning placeholders. Unlike
/// v2's `nonempty_text_at`, v1 text parts also retain empty strings.
fn string_at(value: &Value, pointer: &str) -> Option<String> {
    value.pointer(pointer)?.as_str().map(str::to_owned)
}

pub(super) fn json_display(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

fn state_output(state: &Value) -> String {
    json_display(state.get("output").or_else(|| state.get("error")))
}

fn state_error(state: &Value) -> String {
    json_display(state.get("error").or_else(|| state.get("output")))
}

fn file_path(data: &Value) -> Option<PathBuf> {
    let path = data
        .get("path")
        .and_then(Value::as_str)
        .or_else(|| data.pointer("/source/path").and_then(Value::as_str))
        .or_else(|| data.get("filename").and_then(Value::as_str))
        .or_else(|| {
            data.get("url")
                .and_then(Value::as_str)
                .and_then(|url| url.strip_prefix("file://"))
        })?;
    Some(PathBuf::from(path))
}

fn patch_summary(data: &Value) -> String {
    if let Some(files) = data.get("files").and_then(Value::as_array) {
        let names = files.iter().filter_map(Value::as_str).collect::<Vec<_>>();
        if !names.is_empty() {
            return names.join(", ");
        }
    }
    string_at(data, "/hash").unwrap_or_else(|| "patch".to_owned())
}

fn summary_change(files: Option<i64>, added: Option<i64>, deleted: Option<i64>) -> Option<Change> {
    (files.is_some() || added.is_some() || deleted.is_some()).then(|| Change {
        files: files.unwrap_or(0).max(0).try_into().unwrap_or(u32::MAX),
        added: u64::try_from(added.unwrap_or(0)).unwrap_or(0),
        deleted: u64::try_from(deleted.unwrap_or(0)).unwrap_or(0),
    })
}

/// A measured part is `{"type":"patch","hash":"794c…","files":["/home/…/gallery_watty.py"]}`.
/// The format records paths, not line totals, so those counters intentionally stay zero.
pub(crate) fn patch_files(data: &Value) -> Vec<FileChange> {
    let Some(files) = data.get("files").and_then(Value::as_array) else {
        return Vec::new();
    };
    files
        .iter()
        .filter_map(Value::as_str)
        .map(|path| FileChange {
            path: PathBuf::from(path),
            added: 0,
            deleted: 0,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("store.db");
        let conn = Connection::open(&db).expect("fixture database");
        conn.execute_batch(
            "CREATE TABLE project(id TEXT PRIMARY KEY, worktree TEXT);
             CREATE TABLE session(
                id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, directory TEXT,
                title TEXT, agent TEXT, model TEXT, cost REAL, tokens_input INTEGER,
                tokens_output INTEGER, time_created INTEGER, time_updated INTEGER,
                time_archived INTEGER
             );
             CREATE TABLE message(
                id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER,
                time_updated INTEGER, data TEXT
             );
             CREATE TABLE part(
                id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER,
                time_updated INTEGER, data TEXT
             );
             INSERT INTO project VALUES ('p', '/workspace');
             INSERT INTO session VALUES ('s', 'p', NULL, '/recorded', ' A title ', 'build',
                NULL, 1.5, 3, 5, 1700000000000, 1700000001000, 1700000002000);
             INSERT INTO message VALUES ('m', 's', 1700000000000, 1700000000000,
                '{\"role\":\"assistant\",\"time\":{\"created\":1700000000000},\"modelID\":\"m\",\"cost\":1.25,\"tokens\":{\"input\":8,\"output\":13,\"reasoning\":0,\"cache\":{\"read\":2,\"write\":0}}}');
             INSERT INTO part VALUES ('p1', 'm', 's', 1700000000001, 1700000000001,
                '{\"type\":\"tool\",\"tool\":\"shell\",\"state\":{\"status\":\"error\",\"input\":{\"command\":\"false\"},\"error\":\"failed\"}}');
             INSERT INTO part VALUES ('p2', 'm', 's', 1700000000002, 1700000000002,
                '{\"type\":\"future-part\"}');",
        )
        .expect("fixture schema");
        (dir, db)
    }

    #[test]
    fn assistant_usage_comes_from_its_message_data() {
        let (_dir, db) = fixture();
        let transcript = open(&db, SourceId::OpenCode, "s").expect("open fixture");

        assert_eq!(
            transcript.usage(),
            Some(Usage {
                input: 8,
                output: 13,
                cost: 1.25,
            })
        );
    }

    #[test]
    fn kilo_schema_without_summary_columns_lists() {
        let (_dir, db) = fixture();
        let sessions = list(&db, SourceId::Kilo).expect("list fixture");
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].archived);
        assert_eq!(sessions[0].recorded_cwd, Some(PathBuf::from("/recorded")));
        assert_eq!(sessions[0].change, None);
    }

    #[test]
    fn session_summary_change_comes_from_summary_columns() {
        let (_dir, db) = fixture();
        let connection = Connection::open(&db).expect("fixture database");
        connection
            .execute_batch(
                "ALTER TABLE session ADD COLUMN summary_additions INTEGER;
                 ALTER TABLE session ADD COLUMN summary_deletions INTEGER;
                 ALTER TABLE session ADD COLUMN summary_files INTEGER;
                 UPDATE session
                 SET summary_additions = 12, summary_deletions = 4, summary_files = 3;",
            )
            .expect("summary columns");

        let sessions = list(&db, SourceId::OpenCode).expect("list fixture");
        assert_eq!(
            sessions[0].change,
            Some(Change {
                files: 3,
                added: 12,
                deleted: 4,
            })
        );
    }

    #[test]
    fn archive_update_is_reversible() {
        let (_dir, db) = fixture();
        set_archived(&db, "s", false).expect("unarchive fixture");
        let connection = Connection::open(&db).expect("fixture database");
        let unarchived: Option<i64> = connection
            .query_row(
                "SELECT time_archived FROM session WHERE id = 's'",
                [],
                |row| row.get(0),
            )
            .expect("archive value");
        assert_eq!(unarchived, None);
        drop(connection);

        set_archived(&db, "s", true).expect("archive fixture");
        let connection = Connection::open(&db).expect("fixture database");
        let archived: Option<i64> = connection
            .query_row(
                "SELECT time_archived FROM session WHERE id = 's'",
                [],
                |row| row.get(0),
            )
            .expect("archive value");
        assert!(archived.is_some());
    }

    #[test]
    fn mirrored_archive_updates_both_rows_or_neither() {
        let (_dir, db) = fixture();
        let connection = Connection::open(&db).expect("fixture database");
        connection
            .execute_batch(
                "CREATE TABLE session_v2(id TEXT PRIMARY KEY, time_archived INTEGER);
                 CREATE TABLE session_message(session_id TEXT);
                 INSERT INTO session_v2 VALUES ('s', NULL);",
            )
            .expect("v2 mirror");
        drop(connection);

        set_archived(&db, "s", true).expect("archive both generations");
        let connection = Connection::open(&db).expect("fixture database");
        let archived: (Option<i64>, Option<i64>) = connection
            .query_row(
                "SELECT v.time_archived, s.time_archived
                 FROM session_v2 v JOIN session s ON s.id = v.id WHERE v.id = 's'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("archive values");
        assert!(archived.0.is_some());
        assert_eq!(archived.0, archived.1);

        connection
            .execute_batch(
                "CREATE TRIGGER reject_v1_update BEFORE UPDATE ON session
                 BEGIN SELECT RAISE(ABORT, 'v1 update rejected'); END;",
            )
            .expect("failing second update");
        drop(connection);
        assert!(set_archived(&db, "s", false).is_err());
        let connection = Connection::open(&db).expect("fixture database");
        let after: (Option<i64>, Option<i64>) = connection
            .query_row(
                "SELECT v.time_archived, s.time_archived
                 FROM session_v2 v JOIN session s ON s.id = v.id WHERE v.id = 's'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("archive values after failure");
        assert_eq!(after, archived);
    }

    #[test]
    fn mirrored_listing_chooses_v2_on_a_tie_and_v1_when_newer() {
        let (_dir, db) = fixture();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session_v2 AS SELECT *, CAST(NULL AS TEXT) AS path FROM session WHERE 0;
             CREATE TABLE session_message(id TEXT, session_id TEXT, type TEXT, seq INTEGER, data TEXT);
             INSERT INTO session_v2 SELECT *, NULL FROM session;
             UPDATE session_v2 SET title = 'v2' WHERE id = 's';
             INSERT INTO session_message VALUES ('v2m', 's', 'user', 1, '{\"text\":\"v2 body\"}');",
        ).unwrap();
        let sessions = list(&db, SourceId::OpenCode).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title.as_deref(), Some("v2"));
        assert_eq!(
            open(&db, SourceId::OpenCode, "s").unwrap().title.as_deref(),
            Some("v2")
        );

        conn.execute(
            "UPDATE session SET title = 'v1 newer', time_updated = time_updated + 1 WHERE id = 's'",
            [],
        )
        .unwrap();
        let sessions = list(&db, SourceId::OpenCode).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title.as_deref(), Some("v1 newer"));
        let transcript = open(&db, SourceId::OpenCode, "s").unwrap();
        assert_eq!(transcript.title.as_deref(), Some("v1 newer"));
        assert!(matches!(
            transcript.turns[0].blocks[0],
            Block::ToolCall { .. }
        ));
    }

    #[test]
    fn archive_v2_only_and_empty_schema() {
        let (_dir, db) = fixture();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "DROP TABLE part; DROP TABLE message; DROP TABLE session;
             CREATE TABLE session_v2(id TEXT PRIMARY KEY, time_archived INTEGER);
             CREATE TABLE session_message(session_id TEXT);
             INSERT INTO session_v2 VALUES ('s', NULL);",
        )
        .unwrap();
        drop(conn);
        set_archived(&db, "s", true).unwrap();
        let conn = Connection::open(&db).unwrap();
        let value: Option<i64> = conn
            .query_row(
                "SELECT time_archived FROM session_v2 WHERE id = 's'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(value.is_some());
        conn.execute_batch("DROP TABLE session_message; DROP TABLE session_v2;")
            .unwrap();
        drop(conn);
        assert!(matches!(
            set_archived(&db, "s", true),
            Err(Error::MissingRow { .. })
        ));
    }

    #[test]
    fn error_tool_and_unknown_part_are_preserved() {
        let (_dir, db) = fixture();
        let transcript = open(&db, SourceId::Kilo, "s").expect("open fixture");
        assert_eq!(transcript.unknown_records, 1);
        assert!(matches!(
            transcript.turns[0].blocks[1],
            Block::ToolResult { ok: false, ref output, .. } if output == "failed"
        ));
        assert!(matches!(
            transcript.turns[0].blocks[2],
            Block::Unknown { .. }
        ));
    }
    #[test]
    fn patch_part_preserves_each_recorded_file_path() {
        let files = patch_files(&serde_json::json!({
            "type": "patch",
            "hash": "794c2874",
            "files": ["/workspace/src/gallery_watty.py", "/workspace/tests/test_gallery.py"],
        }));

        assert_eq!(
            files,
            vec![
                FileChange {
                    path: PathBuf::from("/workspace/src/gallery_watty.py"),
                    added: 0,
                    deleted: 0,
                },
                FileChange {
                    path: PathBuf::from("/workspace/tests/test_gallery.py"),
                    added: 0,
                    deleted: 0,
                },
            ]
        );
    }
}
