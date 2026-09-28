pub mod claude;
pub mod codex;
pub mod copilot;
pub mod cursor;
pub mod gemini;
pub mod goose;
pub mod hermes;
pub mod kilo;
pub mod omp;
pub mod openclaw;
pub mod opencode;
pub(crate) mod opencode_schema;
pub(crate) mod opencode_v2;
pub mod remote;

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::error::{Error, Result, io};
use crate::model::{
    ArchiveOutcome, Block, Locator, Role, SessionSummary, SourceId, Transcript, Turn,
};

/// One agent's on-disk store.
pub trait Source: Send + Sync {
    fn id(&self) -> SourceId;

    /// Directories and databases this source reads, for `llm-sessions sources`.
    fn roots(&self) -> Vec<PathBuf>;

    /// Tier 1. MUST NOT parse message bodies: a list row costs a `stat`, a
    /// bounded head read, or one indexed query — never a full transcript.
    fn list(&self) -> Result<Vec<SessionSummary>>;

    /// Tier 2. Parse exactly one session into normalized turns.
    fn open(&self, locator: &Locator) -> Result<Transcript>;

    /// Set archive state in the agent's own store.
    ///
    /// Default: the agent has no archive concept, so the caller falls back to
    /// the overlay rather than inventing a field in someone else's file.
    fn archive(&self, _locator: &Locator, _archived: bool) -> Result<ArchiveOutcome> {
        Ok(ArchiveOutcome::Unsupported)
    }

    /// Command that resumes this session, when the agent supports it.
    fn resume_command(&self, _id: &str) -> Option<Vec<String>> {
        None
    }

    /// Store locations relative to a home directory, for a host this process
    /// cannot read directly. The default is "not listable without a remote
    /// `llm-sessions`", which the remote layer reports rather than guessing.
    fn remote_layout(&self) -> RemoteLayout {
        RemoteLayout::default()
    }

    /// Named SELECTs that read one session's rows on a remote host, for a
    /// [`Locator::Row`] the shell transport listed. Empty: this source cannot
    /// be read remotely without `llm-sessions` on the host.
    fn remote_open_sql(&self, _id: &str) -> Vec<(&'static str, String)> {
        Vec::new()
    }

    /// Rebuild a transcript from the rows [`Source::remote_open_sql`]
    /// fetched, keyed by query name, reusing the local reconstruction.
    fn open_remote_rows(
        &self,
        _db: &Path,
        _id: &str,
        _rows: &std::collections::HashMap<String, Vec<serde_json::Value>>,
    ) -> Result<Transcript> {
        Err(Error::Unsupported {
            agent: self.id(),
            what: "reading remote database rows",
        })
    }
}

/// What the shell transport needs: `find`-style globs for file stores,
/// and a database plus its tier-1 SELECT for SQL stores.
#[derive(Clone, Copy, Debug, Default)]
pub struct RemoteLayout {
    /// Globs relative to the remote home, e.g. ".omp/agent/sessions/*/*.jsonl".
    pub files: &'static [&'static str],
    /// Database path relative to home, and SELECTs the host tries in order
    /// until one succeeds — a store whose schema has changed generations
    /// cannot be inspected before querying it.
    pub sql: Option<(&'static str, &'static [&'static str])>,
}

// Shared record normalization. Schema-specific roles and block pairing stay
// with the adapters; unknown-record preservation and text flattening agree.

/// One whole unrecognized record: counted, folded raw, in its own system
/// turn — a session never fails to open because a format drifted.
pub(crate) fn push_unknown(transcript: &mut Transcript, raw: serde_json::Value) {
    transcript.unknown_records += 1;
    let mut turn = Turn::new(Role::System);
    turn.blocks.push(Block::Unknown { raw });
    transcript.push(turn);
}

/// One unrecognized block inside a turn being built: counted, kept folded.
pub(crate) fn count_unknown(transcript: &mut Transcript, turn: &mut Turn, raw: serde_json::Value) {
    transcript.unknown_records += 1;
    turn.blocks.push(Block::Unknown { raw });
}

/// Nested content flattened to displayable text: a plain string is itself,
/// an array joins its `text` fields, any other shape renders as JSON.
pub(crate) fn flatten_text(content: Option<&serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|block| str_field(block, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// Every source, in display order. Sources whose root is absent yield nothing
/// rather than erroring, so an uninstalled agent is silent.
pub fn all(home: &Path) -> Vec<Box<dyn Source>> {
    SourceId::ALL
        .iter()
        .map(|id| -> Box<dyn Source> {
            match id {
                SourceId::Omp => Box::new(omp::Omp::new(home)),
                SourceId::Claude => Box::new(claude::Claude::new(home)),
                SourceId::Codex => Box::new(codex::Codex::new(home)),
                SourceId::OpenCode => Box::new(opencode::OpenCode::new(home)),
                SourceId::Copilot => Box::new(copilot::Copilot::new(home)),
                SourceId::Kilo => Box::new(kilo::Kilo::new(home)),
                SourceId::Goose => Box::new(goose::Goose::new(home)),
                SourceId::Cursor => Box::new(cursor::Cursor::new(home)),
                SourceId::Gemini => Box::new(gemini::Gemini::new(home)),
                SourceId::Hermes => Box::new(hermes::Hermes::new(home)),
                SourceId::OpenClaw => Box::new(openclaw::OpenClaw::new(home)),
            }
        })
        .collect()
}

/// The source that owns a session id, for actions issued from a list row.
pub fn find(sources: &[Box<dyn Source>], id: SourceId) -> Option<&dyn Source> {
    sources.iter().find(|s| s.id() == id).map(AsRef::as_ref)
}

/// First `limit` bytes of a file. Bounded so a 20 MB transcript costs one read.
pub(crate) fn read_head(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; limit];
    let mut file = File::open(path).map_err(io(path))?;
    let mut filled = 0;
    while filled < limit {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(io(path)(e)),
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

/// The first JSONL record, however long the line is.
///
/// Codex session headers embed the full base instructions, so the header line
/// alone can run tens of kilobytes; a fixed head read would truncate it.
/// `cap` bounds a pathological file.
pub(crate) fn read_first_line(path: &Path, cap: usize) -> Result<Vec<u8>> {
    use std::io::BufRead;
    let file = File::open(path).map_err(io(path))?;
    let mut line = Vec::new();
    std::io::BufReader::new(file.take(cap as u64))
        .read_until(b'\n', &mut line)
        .map_err(io(path))?;
    Ok(line)
}

/// Last `limit` bytes of a file, for records an agent appends late (titles,
/// cost summaries). Shorter files are returned whole.
pub(crate) fn read_tail(path: &Path, limit: u64) -> Result<Vec<u8>> {
    use std::io::{Seek, SeekFrom};
    let mut file = File::open(path).map_err(io(path))?;
    let len = file.metadata().map_err(io(path))?.len();
    if len > limit {
        file.seek(SeekFrom::Start(len - limit)).map_err(io(path))?;
    }
    // Capacity is only a hint; a length that does not fit `usize` reads into a
    // growing buffer instead.
    let mut buf = Vec::with_capacity(usize::try_from(limit.min(len)).unwrap_or(0));
    file.read_to_end(&mut buf).map_err(io(path))?;
    Ok(buf)
}

/// Complete JSON values from a possibly truncated head buffer.
///
/// The final line of a bounded read is usually cut mid-object; it is dropped
/// rather than reported, which is why this yields values and not errors.
pub(crate) fn head_records(bytes: &[u8]) -> impl Iterator<Item = serde_json::Value> + '_ {
    bytes
        .split(|&b| b == b'\n')
        .filter_map(|line| serde_json::from_slice(line).ok())
}

/// Every record of a whole JSONL file, in order.
///
/// A line that will not parse is yielded as `Err(line)` so an adapter can turn
/// it into [`crate::model::Block::Unknown`] instead of losing it.
pub(crate) fn read_records(path: &Path) -> Result<Vec<serde_json::Value>> {
    use std::io::BufRead;
    let file = File::open(path).map_err(io(path))?;
    let mut out = Vec::new();
    for line in std::io::BufReader::new(file).lines() {
        let line = line.map_err(io(path))?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(&line) {
            Ok(value) => out.push(value),
            Err(_) => out.push(serde_json::Value::String(line)),
        }
    }
    Ok(out)
}

pub(crate) fn mtime(path: &Path) -> Result<(OffsetDateTime, u64)> {
    let meta = std::fs::metadata(path).map_err(io(path))?;
    let modified = meta.modified().map_err(io(path))?;
    Ok((OffsetDateTime::from(modified), meta.len()))
}

pub(crate) fn parse_rfc3339(s: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(s, &Rfc3339).ok()
}

pub(crate) fn from_unix_millis(ms: i64) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok()
}

pub(crate) fn from_unix_secs(s: i64) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(s).ok()
}

/// Epoch value that may be seconds, milliseconds, or microseconds.
///
/// Agents disagree, and some changed units between versions. Magnitude is the
/// only reliable discriminator.
pub(crate) fn from_epoch_guess(v: i64) -> Option<OffsetDateTime> {
    const YEAR_2001_SECS: i64 = 1_000_000_000;
    match v.abs() {
        0 => None,
        n if n < YEAR_2001_SECS * 10 => from_unix_secs(v),
        n if n < YEAR_2001_SECS * 10_000 => from_unix_millis(v),
        _ => from_unix_millis(v / 1000),
    }
}

/// Non-empty, whitespace-collapsed, single-line string field.
pub(crate) fn clean_title(raw: &str) -> Option<String> {
    let one_line: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    (!one_line.is_empty()).then_some(one_line)
}

pub(crate) fn str_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_owned)
}

/// The `File` path of a locator, or a typed error for adapters that only read
/// transcripts off disk.
pub(crate) fn file_locator(id: SourceId, locator: &Locator) -> Result<&Path> {
    match locator {
        Locator::File { path } => Ok(path),
        Locator::Row { .. } => Err(Error::Unsupported {
            agent: id,
            what: "opening a database row as a file transcript",
        }),
    }
}

/// The `(db, id)` of a locator, or a typed error for database-backed adapters.
pub(crate) fn row_locator(id: SourceId, locator: &Locator) -> Result<(&Path, &str)> {
    match locator {
        Locator::Row { db, id: row } => Ok((db, row)),
        Locator::File { .. } => Err(Error::Unsupported {
            agent: id,
            what: "opening a file as a database row",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_units_are_distinguished_by_magnitude() {
        let secs = from_epoch_guess(1_789_034_098).unwrap();
        let millis = from_epoch_guess(1_789_034_098_458).unwrap();
        let micros = from_epoch_guess(1_789_034_098_458_000).unwrap();
        assert_eq!(secs.date(), millis.date());
        assert_eq!(secs.date(), micros.date());
        assert_eq!(from_epoch_guess(0), None);
    }

    #[test]
    fn truncated_final_record_is_dropped_not_fatal() {
        let bytes = b"{\"a\":1}\n{\"b\":2}\n{\"c\":truncat";
        let records: Vec<_> = head_records(bytes).collect();
        assert_eq!(records.len(), 2);
    }

    #[test]
    fn all_covers_the_registry_in_display_order() {
        let sources = all(Path::new("/nonexistent"));
        let ids: Vec<_> = sources.iter().map(|s| s.id()).collect();
        assert_eq!(ids, SourceId::ALL.to_vec());
    }
}
