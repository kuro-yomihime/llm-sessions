//! The shell transport's records, turned back into rows: framing, bounded
//! heads, sqlite3 JSON and the base64 that carries them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;
use time::OffsetDateTime;

use crate::model::{Liveness, Locator, PathTrust, SessionSummary, SourceId, Usage};
use crate::source;

/// A file the shell transport found, before its head arrived.
#[derive(Debug)]
struct FileRow {
    source: SourceId,
    updated: OffsetDateTime,
    size: u64,
    head: Option<Vec<u8>>,
}

/// Turn the shell transport's records into rows.
///
/// Framing is one record per line: `F\t<agent>\t<path>\t<mtime>\t<size>`,
/// `H\t<path>\t<base64 head>`, `S\t<agent>\t<base64 sqlite3 json>`. Anything
/// else is ignored rather than reported: a host may print a warning of its
/// own, and a warning must not cost the whole listing.
pub fn parse_shell_listing(
    raw: &str,
    host: &str,
    dbs: &HashMap<SourceId, PathBuf>,
) -> Vec<SessionSummary> {
    let mut files: HashMap<String, FileRow> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut sessions: Vec<SessionSummary> = Vec::new();

    for line in raw.lines() {
        let mut fields = line.split('\t');
        match fields.next() {
            Some("F") => {
                let (Some(agent), Some(path), Some(stamp), Some(size)) =
                    (fields.next(), fields.next(), fields.next(), fields.next())
                else {
                    continue;
                };
                let Some(source) = SourceId::parse(agent) else {
                    continue;
                };
                let Some(updated) = stamp.parse::<f64>().ok().and_then(from_unix_float) else {
                    continue;
                };
                // `find`'s `*` crosses directory separators, so two globs of
                // one source can match the same deep file; a transcript is
                // one row no matter how many patterns saw it.
                if files.contains_key(path) {
                    continue;
                }
                order.push(path.to_owned());
                files.insert(
                    path.to_owned(),
                    FileRow {
                        source,
                        updated,
                        size: size.parse().unwrap_or(0),
                        head: None,
                    },
                );
            }
            Some("H") => {
                let (Some(path), Some(encoded)) = (fields.next(), fields.next()) else {
                    continue;
                };
                if let Some(row) = files.get_mut(path) {
                    row.head = decode_base64(encoded);
                }
            }
            Some("S") => {
                let (Some(agent), Some(encoded)) = (fields.next(), fields.next()) else {
                    continue;
                };
                let (Some(source), Some(decoded)) =
                    (SourceId::parse(agent), decode_base64(encoded))
                else {
                    continue;
                };
                let Ok(rows) = serde_json::from_slice::<Vec<Value>>(&decoded) else {
                    continue;
                };
                let db = dbs.get(&source);
                for row in &rows {
                    if let Some(summary) = summary_from_sql(source, host, db, row) {
                        sessions.push(summary);
                    }
                }
            }
            _ => {}
        }
    }

    for path in order {
        let Some(row) = files.get(&path) else {
            continue;
        };
        if let Some(summary) = summary_from_head(row, host, &path) {
            sessions.push(summary);
        }
    }
    sessions
}

/// The records of a remote open: `R\t<name>\t<base64 sqlite3 json>`, one
/// per named query. An empty payload (no rows, or a failed query) is an
/// empty list; unframed lines are ignored as in the listing.
pub(super) fn parse_open_rows(raw: &str) -> HashMap<String, Vec<Value>> {
    let mut rows = HashMap::new();
    for line in raw.lines() {
        let mut fields = line.split('\t');
        let (Some("R"), Some(name), Some(encoded)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let parsed = decode_base64(encoded)
            .filter(|decoded| !decoded.is_empty())
            .and_then(|decoded| serde_json::from_slice::<Vec<Value>>(&decoded).ok())
            .unwrap_or_default();
        rows.insert(name.to_owned(), parsed);
    }
    rows
}

/// One row of a remote store's eleven-column listing.
fn summary_from_sql(
    source: SourceId,
    host: &str,
    db: Option<&PathBuf>,
    row: &Value,
) -> Option<SessionSummary> {
    let id = row.get("id")?.as_str()?.to_owned();
    // Most stores report an epoch; Copilot's `sessions` table keeps RFC 3339
    // text in `updated_at`/`created_at` and calls its title `summary`.
    let updated = number(row.get("updated_epoch"))
        .and_then(from_unix_float)
        .or_else(|| rfc3339(row.get("updated_at")))?;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "float-to-int `as` saturates: fractions truncate, negatives and NaN were clamped to 0 above, and huge counts pin to u64::MAX"
    )]
    let (input, output) = (
        number(row.get("input")).unwrap_or(0.0).max(0.0) as u64,
        number(row.get("output")).unwrap_or(0.0).max(0.0) as u64,
    );
    let cost = number(row.get("cost"));
    let priced = row.get("input").is_some_and(|v| !v.is_null())
        || row.get("output").is_some_and(|v| !v.is_null())
        || cost.is_some();

    Some(SessionSummary {
        id: id.clone(),
        source,
        host: Some(host.to_owned()),
        workspace: None,
        recorded_cwd: text(row.get("cwd")).map(PathBuf::from),
        trust: PathTrust::Recorded,
        title: text(row.get("title"))
            .or_else(|| text(row.get("summary")))
            .as_deref()
            .and_then(source::clean_title),
        created: rfc3339(row.get("created_at")),
        updated,
        model: text(row.get("model")),
        branch: text(row.get("branch")),
        parent: text(row.get("parent")),
        subagent: false,
        archived: number(row.get("archived")).is_some_and(|flag| flag != 0.0),
        usage: priced.then_some(Usage {
            input,
            output,
            cost: cost.unwrap_or(0.0),
        }),
        change: None,
        size_bytes: None,
        locator: Locator::Row {
            db: db.cloned().unwrap_or_default(),
            id,
        },
        // The shell transport cannot see the host's `/proc`, and an inferred
        // badge dressed as a fact is worse than no badge.
        liveness: Liveness::Stale,
    })
}

/// One row from a bounded head.
///
/// Deliberately generic: this is the declared "less precise" mode, and it
/// does not reimplement nine schemas. It reads the few field names every
/// agent happens to agree on and leaves everything else empty.
fn summary_from_head(row: &FileRow, host: &str, path: &str) -> Option<SessionSummary> {
    let head = row.head.as_deref().unwrap_or_default();
    let records: Vec<Value> = source::head_records(head).collect();
    let field = |keys: &[&str]| -> Option<String> {
        for record in &records {
            for key in keys {
                if let Some(found) = record.get(*key).and_then(Value::as_str) {
                    return Some(found.to_owned());
                }
            }
        }
        // A single-document store (Gemini) is one object larger than the
        // head, so nothing parses; scan the bytes for the same keys.
        keys.iter().find_map(|key| scan_string(head, key))
    };

    let id = field(&["sessionId", "session_id", "id"]);
    let title = field(&["title", "aiTitle", "description", "label"]);
    let cwd = field(&["cwd", "working_dir", "directory"]);
    // Not a transcript at all: no complete JSON object and no field we know.
    if records.is_empty() && id.is_none() && title.is_none() && cwd.is_none() {
        return None;
    }

    Some(SessionSummary {
        id: id.unwrap_or_else(|| stem(path)),
        source: row.source,
        host: Some(host.to_owned()),
        workspace: None,
        recorded_cwd: cwd.map(PathBuf::from),
        trust: PathTrust::Recorded,
        title: title.as_deref().and_then(source::clean_title),
        created: None,
        updated: row.updated,
        model: None,
        branch: None,
        parent: None,
        subagent: false,
        archived: false,
        usage: None,
        change: None,
        size_bytes: Some(row.size),
        locator: Locator::File {
            path: PathBuf::from(path),
        },
        liveness: Liveness::Stale,
    })
}

fn stem(path: &str) -> String {
    Path::new(path).file_stem().map_or_else(
        || path.to_owned(),
        |stem| stem.to_string_lossy().into_owned(),
    )
}

/// `"key": "value"` anywhere in a buffer, without requiring valid JSON.
fn scan_string(bytes: &[u8], key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let at = bytes
        .windows(needle.len())
        .position(|window| window == needle.as_bytes())?;
    let mut rest = bytes[at + needle.len()..].iter().copied();
    loop {
        match rest.next()? {
            b' ' | b'\t' | b'\n' | b'\r' | b':' => {}
            b'"' => break,
            _ => return None,
        }
    }
    let mut value = String::new();
    let mut escaped = false;
    for byte in rest {
        match byte {
            b'\\' if !escaped => escaped = true,
            b'"' if !escaped => return Some(value),
            byte => {
                escaped = false;
                value.push(byte as char);
            }
        }
    }
    None
}

fn rfc3339(value: Option<&Value>) -> Option<OffsetDateTime> {
    value?.as_str().and_then(source::parse_rfc3339)
}

fn text(value: Option<&Value>) -> Option<String> {
    value?.as_str().map(str::to_owned)
}

/// sqlite3's JSON emits numbers, but a column may also arrive as a string.
fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

fn from_unix_float(seconds: f64) -> Option<OffsetDateTime> {
    if !seconds.is_finite() {
        return None;
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "float-to-int `as` saturates; out-of-range results are rejected below"
    )]
    let nanos = (seconds * 1_000_000_000.0) as i128;
    OffsetDateTime::from_unix_timestamp_nanos(nanos).ok()
}

pub(super) fn decode_base64(encoded: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(encoded.len() / 4 * 3);
    let mut accumulator: u32 = 0;
    let mut bits = 0;
    for byte in encoded.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' => continue,
            _ => return None,
        };
        accumulator = (accumulator << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            #[expect(
                clippy::cast_possible_truncation,
                reason = "the decoded byte is the low eight bits; older bits are spent"
            )]
            out.push((accumulator >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPOCH: i64 = 1_700_000_000;

    fn listing(records: &[String]) -> Vec<SessionSummary> {
        let dbs: HashMap<SourceId, PathBuf> = [(
            SourceId::OpenCode,
            PathBuf::from("/home/x/.local/share/opencode/opencode.db"),
        )]
        .into_iter()
        .collect();
        parse_shell_listing(&records.join("\n"), "nx-1", &dbs)
    }

    fn head_records_for(head: &str, path: &str) -> Vec<String> {
        let encoded = encode_base64(head.as_bytes());
        vec![
            format!("F\tomp\t{path}\t{EPOCH}.0000000\t{}", head.len()),
            format!("H\t{path}\t{encoded}"),
        ]
    }

    /// Mirror of the host-side `base64`, for fixtures only.
    fn encode_base64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let mut buffer = [0u8; 3];
            buffer[..chunk.len()].copy_from_slice(chunk);
            let triple =
                (u32::from(buffer[0]) << 16) | (u32::from(buffer[1]) << 8) | u32::from(buffer[2]);
            for index in 0..4 {
                if index <= chunk.len() {
                    out.push(ALPHABET[((triple >> (18 - 6 * index)) & 0x3f) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    #[test]
    fn a_head_without_json_is_not_a_session() {
        let rows = listing(&head_records_for("not json at all", "/home/x/a.jsonl"));
        assert!(rows.is_empty(), "{rows:?}");
    }

    #[test]
    fn a_bounded_head_yields_id_title_and_cwd() {
        let head = r#"{"type":"session","id":"01a0","title":"Fix the thing","cwd":"/work/thing"}"#;
        let rows = listing(&head_records_for(head, "/home/x/.omp/a.jsonl"));

        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.id, "01a0");
        assert_eq!(row.title.as_deref(), Some("Fix the thing"));
        assert_eq!(row.recorded_cwd.as_deref(), Some(Path::new("/work/thing")));
        assert_eq!(row.host.as_deref(), Some("nx-1"));
        assert_eq!(row.updated.unix_timestamp(), EPOCH);
        assert_eq!(row.source, SourceId::Omp);
    }

    #[test]
    fn a_sqlite_json_row_becomes_a_summary() {
        let json = r#"[{"id":"ses_1","title":"t","cwd":"/w","updated_epoch":1700000000,
            "model":null,"input":null,"output":null,"cost":null,"parent":null,
            "archived":1,"branch":null}]"#;
        let rows = listing(&[format!("S\topencode\t{}", encode_base64(json.as_bytes()))]);

        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert!(row.archived);
        assert_eq!(
            row.locator,
            Locator::Row {
                db: PathBuf::from("/home/x/.local/share/opencode/opencode.db"),
                id: "ses_1".into(),
            }
        );
        assert_eq!(row.usage, None, "no counted tokens is not zero tokens");
        assert_eq!(row.updated.unix_timestamp(), EPOCH);
    }

    #[test]
    fn a_copilot_json_row_uses_rfc3339_and_summary() {
        let json = r#"[{"id":"0688","cwd":"/tmp","summary":" Fix it ",
            "created_at":"2023-11-14T22:13:10Z","updated_at":"2023-11-14T22:13:20Z"}]"#;
        let dbs: HashMap<SourceId, PathBuf> = [(
            SourceId::Copilot,
            PathBuf::from("/home/x/.copilot/session-store.db"),
        )]
        .into_iter()
        .collect();
        let raw = format!("S\tcopilot\t{}", encode_base64(json.as_bytes()));
        let rows = parse_shell_listing(&raw, "ar-2", &dbs);

        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.source, SourceId::Copilot);
        assert_eq!(row.title.as_deref(), Some("Fix it"));
        assert_eq!(row.recorded_cwd.as_deref(), Some(Path::new("/tmp")));
        assert_eq!(row.updated.unix_timestamp(), EPOCH);
        assert_eq!(
            row.created.map(OffsetDateTime::unix_timestamp),
            Some(EPOCH - 10)
        );
        assert_eq!(
            row.locator,
            Locator::Row {
                db: PathBuf::from("/home/x/.copilot/session-store.db"),
                id: "0688".into(),
            }
        );
    }
}
