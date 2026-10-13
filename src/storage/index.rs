//! Persistent full-text search over session bodies.
//!
//! Tier-1 listing is deliberately cheap, but body search must otherwise parse
//! roughly 280 MB of JSONL and hundreds of thousands of `SQLite` parts on every
//! query. This cache is the sole database `llm-sessions` owns and writes; agent stores
//! remain read-only. [`RefreshStats::elapsed`] records the actual end-to-end
//! cold-build cost, including the other sources and FTS insertion.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::types::Type;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use time::OffsetDateTime;

use crate::error::{Result, io, sql};
use crate::model::{Locator, SessionSummary, SourceId, Transcript, Usage};
use crate::source::Source;

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS docs (
    source       TEXT NOT NULL,
    session_id   TEXT NOT NULL,
    locator_kind TEXT NOT NULL,
    locator_path TEXT NOT NULL,
    locator_id   TEXT,
    freshness    TEXT NOT NULL,
    title        TEXT,
    workspace    TEXT,
    updated      INTEGER NOT NULL,
    tokens_input INTEGER,
    tokens_output INTEGER,
    cost         REAL,
    turns        INTEGER,
    first_prompt TEXT,
    model_usage  TEXT NOT NULL DEFAULT '{}',
    body_truncated INTEGER NOT NULL DEFAULT 0,
    body_limit INTEGER,
    UNIQUE (source, session_id)
);
CREATE VIRTUAL TABLE IF NOT EXISTS body USING fts5(text);";

/// A cached body index kept outside every agent's store.
///
/// `body` is a plain FTS5 table rather than an external-content table. An
/// external-content table would need another copy of every body in `docs` to
/// provide `snippet()` text; storing it directly in FTS avoids that duplicate
/// while `docs.rowid` still gives a stable join key. Refreshes replace only the
/// changed row's FTS document.
pub struct Index {
    path: PathBuf,
    conn: Connection,
}

/// Cached transcript details needed by the fast digest path.
#[derive(Clone, Debug, Default)]
pub(crate) struct CachedDigest {
    /// First user prompt per indexed session that recorded one.
    pub prompts: HashMap<(SourceId, String), String>,
    /// Body-derived usage, for sessions tier 1 cannot price.
    pub usage: UsageCache,
}

/// Body-derived usage per indexed session, for agents that only record spend
/// inside the transcript.
///
/// This is the one place the precedence rule lives: tier-1 usage wins, the
/// index fills the gap, and a session neither side priced stays `None` so
/// missing accounting is never reported as a free session.
#[derive(Clone, Debug, Default)]
pub struct UsageCache {
    /// Keyed by source first so a lookup borrows the session id instead of
    /// cloning it into a tuple key.
    by_source: HashMap<SourceId, HashMap<String, Usage>>,
}

impl UsageCache {
    /// The session's own tier-1 usage, else the local index cache for local rows.
    pub fn resolve(&self, session: &SessionSummary) -> Option<Usage> {
        session.usage.or_else(|| {
            session
                .is_local()
                .then(|| self.get(session.source, &session.id))
                .flatten()
        })
    }

    /// Replace every session's usage with [`UsageCache::resolve`]'s answer.
    pub fn fill(&self, sessions: &mut [SessionSummary]) {
        if self.by_source.is_empty() {
            return;
        }
        for session in sessions {
            session.usage = self.resolve(session);
        }
    }

    fn get(&self, source: SourceId, id: &str) -> Option<Usage> {
        self.by_source.get(&source)?.get(id).copied()
    }

    fn insert(&mut self, source: SourceId, id: String, usage: Usage) {
        self.by_source.entry(source).or_default().insert(id, usage);
    }
}

/// One full-text match together with tier-1 metadata for presentation.
#[derive(Clone, Debug)]
pub struct Hit {
    pub source: SourceId,
    pub session_id: String,
    pub title: Option<String>,
    pub workspace: Option<PathBuf>,
    pub updated: OffsetDateTime,
    pub snippet: String,
}

/// Work performed by one incremental refresh.
#[derive(Clone, Copy, Debug, Default)]
pub struct RefreshStats {
    pub scanned: usize,
    pub reindexed: usize,
    pub removed: usize,
    pub failed: usize,
    pub elapsed: Duration,
}

/// Retention and body-size limits applied while refreshing the index.
#[derive(Clone, Debug, Default)]
pub struct RefreshOptions {
    /// Skip sessions older than this.
    pub since: Option<OffsetDateTime>,
    /// Truncate one indexed body to this many bytes at a UTF-8 boundary.
    pub max_body: Option<usize>,
    /// Drop the oldest indexed documents until indexed body text fits this cap.
    pub max_bytes: Option<u64>,
}

/// A point-in-time description of the local search cache.
#[derive(Clone, Debug)]
pub struct IndexStats {
    pub docs: usize,
    pub oldest: Option<OffsetDateTime>,
    pub newest: Option<OffsetDateTime>,
    pub bytes: u64,
}

#[derive(Clone, Debug)]
struct DocumentState {
    freshness: String,
    derived: bool,
    body_truncated: bool,
    body_limit: Option<usize>,
    /// Which reader wrote the row; see [`READER_VERSION`].
    reader_version: i64,
}

impl Index {
    /// Default location, honouring `$XDG_CACHE_HOME`.
    pub fn default_path(home: &Path) -> PathBuf {
        crate::cache_home(home).join("llm-sessions/index.db")
    }

    /// Open or create the application's own cache database.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io(parent))?;
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
        .map_err(sql(path))?;
        conn.execute_batch(SCHEMA).map_err(sql(path))?;
        migrate_docs(&conn, path)?;
        Ok(Self {
            path: path.to_path_buf(),
            conn,
        })
    }

    /// Reindex only sessions whose freshness key changed, then drop vanished sessions.
    ///
    /// Files use their current `(size, mtime)` so a stale tier-1 size cannot
    /// hide a change. Database rows have no individual file, so their source's
    /// recorded `updated` timestamp is the freshness key instead.
    pub fn refresh(
        &mut self,
        sessions: &[SessionSummary],
        sources: &[Box<dyn Source>],
    ) -> Result<RefreshStats> {
        self.refresh_with(sessions, sources, &RefreshOptions::default())
    }

    /// Refresh the cache with explicit retention and indexed-body limits.
    ///
    /// A truncated body carries the limit that produced it. This lets an
    /// unchanged session remain incremental at the same cap while a larger cap
    /// (or no cap) reparses it and restores searchable text.
    pub fn refresh_with(
        &mut self,
        sessions: &[SessionSummary],
        sources: &[Box<dyn Source>],
        options: &RefreshOptions,
    ) -> Result<RefreshStats> {
        let started = Instant::now();
        let retained: Vec<&SessionSummary> = sessions
            .iter()
            .filter(|session| session.is_local())
            .filter(|session| options.since.is_none_or(|since| session.updated >= since))
            .collect();
        let mut stats = RefreshStats {
            scanned: retained.len(),
            ..RefreshStats::default()
        };
        let source_by_id: HashMap<SourceId, &dyn Source> = sources
            .iter()
            .map(|source| (source.id(), source.as_ref()))
            .collect();
        let present: HashSet<(String, String)> = retained
            .iter()
            .map(|session| (session.source.name().to_owned(), session.id.clone()))
            .collect();

        for session in retained {
            let Some(source) = source_by_id.get(&session.source) else {
                stats.failed += 1;
                continue;
            };
            let Ok(freshness) = freshness(session) else {
                stats.failed += 1;
                continue;
            };

            let state = self.doc_state(session)?;
            if state.as_ref().is_some_and(|state| {
                state.derived
                    && state.reader_version == READER_VERSION
                    && state.freshness == freshness
                    && !needs_reindex_for_body_limit(state, options.max_body)
            }) {
                // Metadata can change independently of the body. Keeping it current
                // does not violate the no-reparse guarantee for an unchanged key.
                self.update_metadata(session)?;
                continue;
            }

            let Ok(transcript) = source.open(&session.locator) else {
                stats.failed += 1;
                continue;
            };
            self.replace_document(session, &freshness, &transcript, options.max_body)?;
            stats.reindexed += 1;
        }

        let indexed = self.indexed_keys()?;
        for (source, session_id) in indexed {
            if !present.contains(&(source.clone(), session_id.clone())) {
                self.remove_document(&source, &session_id)?;
                stats.removed += 1;
            }
        }
        if let Some(max_bytes) = options.max_bytes {
            stats.removed += self.prune_to(max_bytes)?;
        }

        stats.elapsed = started.elapsed();
        Ok(stats)
    }

    /// Physical size of the `SQLite` database file, excluding a transient WAL.
    pub fn size_bytes(&self) -> Result<u64> {
        std::fs::metadata(&self.path)
            .map(|metadata| metadata.len())
            .map_err(io(&self.path))
    }

    /// Count documents, their age span, and the current file size.
    pub fn stats(&self) -> Result<IndexStats> {
        let (docs, oldest, newest): (i64, Option<i64>, Option<i64>) = self
            .conn
            .query_row(
                "SELECT COUNT(*), MIN(updated), MAX(updated) FROM docs",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                    ))
                },
            )
            .map_err(sql(&self.path))?;
        let date = |timestamp: Option<i64>| -> rusqlite::Result<Option<OffsetDateTime>> {
            timestamp
                .map(|timestamp| {
                    OffsetDateTime::from_unix_timestamp(timestamp)
                        .map_err(|_| invalid_index_value("updated timestamp"))
                })
                .transpose()
        };
        Ok(IndexStats {
            docs: usize::try_from(docs.max(0)).unwrap_or(usize::MAX),
            oldest: date(oldest).map_err(sql(&self.path))?,
            newest: date(newest).map_err(sql(&self.path))?,
            bytes: self.size_bytes()?,
        })
    }

    /// Rebuild `SQLite`'s pages only when a caller explicitly accepts the write
    /// pause. Refresh deletion merely makes pages reusable.
    pub fn vacuum(&mut self) -> Result<u64> {
        let before = self.size_bytes()?;
        self.conn.execute_batch("VACUUM").map_err(sql(&self.path))?;
        Ok(before.saturating_sub(self.size_bytes()?))
    }

    /// Search normalized transcript prose with FTS5 snippets for context.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        let Some(expression) = match_expression(query) else {
            return Ok(Vec::new());
        };
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut stmt = self
            .conn
            .prepare(
                "SELECT docs.source, docs.session_id, docs.title, docs.workspace,
                        docs.updated, snippet(body, 0, '[', ']', '…', 16)
                 FROM body
                 JOIN docs ON docs.rowid = body.rowid
                 WHERE body MATCH ?1
                 ORDER BY rank
                 LIMIT ?2",
            )
            .map_err(sql(&self.path))?;
        let rows = stmt
            .query_map(params![expression, limit], hit_from_row)
            .map_err(sql(&self.path))?;
        let mut hits = Vec::new();
        for row in rows {
            hits.push(row.map_err(sql(&self.path))?);
        }
        Ok(hits)
    }

    /// Cached per-session usage for sessions tier 1 cannot price.
    pub fn usage(&self) -> Result<UsageCache> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT source, session_id, tokens_input, tokens_output, cost
                 FROM docs
                 WHERE tokens_input IS NOT NULL
                    OR tokens_output IS NOT NULL
                    OR cost IS NOT NULL",
            )
            .map_err(sql(&self.path))?;
        let rows = stmt
            .query_map([], |row| {
                let source_name: String = row.get(0)?;
                let source =
                    SourceId::parse(&source_name).ok_or_else(|| invalid_index_value("source"))?;
                let input: Option<i64> = row.get(2)?;
                let output: Option<i64> = row.get(3)?;
                let cost: Option<f64> = row.get(4)?;
                Ok((source, row.get(1)?, cached_usage(input, output, cost)))
            })
            .map_err(sql(&self.path))?;
        let mut usage = UsageCache::default();
        for row in rows {
            let (source, id, value) = row.map_err(sql(&self.path))?;
            usage.insert(source, id, value);
        }
        Ok(usage)
    }

    /// Aggregate the cached per-turn usage by model across all indexed sessions.
    pub fn usage_by_model(&self) -> Result<HashMap<String, Usage>> {
        let mut totals = HashMap::new();
        for usage in self.model_usage_per_session()?.into_values() {
            for (model, usage) in usage {
                merge_usage(totals.entry(model).or_default(), usage);
            }
        }
        Ok(totals)
    }

    /// Cached per-model usage for each indexed session.
    pub fn model_usage_per_session(
        &self,
    ) -> Result<HashMap<(SourceId, String), BTreeMap<String, Usage>>> {
        let mut stmt = self
            .conn
            .prepare("SELECT source, session_id, model_usage FROM docs")
            .map_err(sql(&self.path))?;
        let rows = stmt
            .query_map([], |row| {
                let source_name: String = row.get(0)?;
                let source =
                    SourceId::parse(&source_name).ok_or_else(|| invalid_index_value("source"))?;
                let raw: String = row.get(2)?;
                Ok(((source, row.get(1)?), model_usage_from_json(&raw)?))
            })
            .map_err(sql(&self.path))?;
        let mut usage = HashMap::new();
        for row in rows {
            let (key, value) = row.map_err(sql(&self.path))?;
            usage.insert(key, value);
        }
        Ok(usage)
    }

    /// Total spend and tokens per workspace, for the workspace pane.
    pub fn usage_by_workspace(&self) -> Result<HashMap<PathBuf, Usage>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT workspace, SUM(tokens_input), SUM(tokens_output), SUM(cost)
                 FROM docs
                 WHERE workspace IS NOT NULL
                   AND (tokens_input IS NOT NULL
                        OR tokens_output IS NOT NULL
                        OR cost IS NOT NULL)
                 GROUP BY workspace",
            )
            .map_err(sql(&self.path))?;
        let rows = stmt
            .query_map([], |row| {
                let workspace: String = row.get(0)?;
                let input: Option<i64> = row.get(1)?;
                let output: Option<i64> = row.get(2)?;
                let cost: Option<f64> = row.get(3)?;
                Ok((PathBuf::from(workspace), cached_usage(input, output, cost)))
            })
            .map_err(sql(&self.path))?;
        let mut usage = HashMap::new();
        for row in rows {
            let (workspace, value) = row.map_err(sql(&self.path))?;
            usage.insert(workspace, value);
        }
        Ok(usage)
    }

    /// Read indexed prompt and usage fields in one query. Digest callers use
    /// this rather than parsing transcript bodies, so the command stays fast.
    pub(crate) fn digest_data(&self) -> Result<CachedDigest> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT source, session_id, first_prompt, tokens_input, tokens_output, cost
                 FROM docs",
            )
            .map_err(sql(&self.path))?;
        let rows = stmt
            .query_map([], |row| {
                let source_name: String = row.get(0)?;
                let source =
                    SourceId::parse(&source_name).ok_or_else(|| invalid_index_value("source"))?;
                let input: Option<i64> = row.get(3)?;
                let output: Option<i64> = row.get(4)?;
                let cost: Option<f64> = row.get(5)?;
                let usage = (input.is_some() || output.is_some() || cost.is_some())
                    .then(|| cached_usage(input, output, cost));
                let prompt: Option<String> = row.get(2)?;
                Ok((source, row.get::<_, String>(1)?, prompt, usage))
            })
            .map_err(sql(&self.path))?;
        let mut data = CachedDigest::default();
        for row in rows {
            let (source, id, prompt, usage) = row.map_err(sql(&self.path))?;
            match (prompt, usage) {
                (Some(prompt), Some(usage)) => {
                    data.prompts.insert((source, id.clone()), prompt);
                    data.usage.insert(source, id, usage);
                }
                (Some(prompt), None) => {
                    data.prompts.insert((source, id), prompt);
                }
                (None, Some(usage)) => data.usage.insert(source, id, usage),
                (None, None) => {}
            }
        }
        Ok(data)
    }

    /// Freshness key of an existing row, plus whether its body-derived
    /// columns were ever filled.
    ///
    /// A database migrated from the pre-usage schema has a valid freshness
    /// key and empty derived columns. Treating that as up to date would leave
    /// `turns` and `first_prompt` `NULL` forever, so such a row is reparsed
    /// once — after which the freshness key alone governs again.
    fn doc_state(&self, session: &SessionSummary) -> Result<Option<DocumentState>> {
        self.conn
            .query_row(
                "SELECT freshness, turns IS NOT NULL, model_usage, body_truncated, body_limit,
                        reader_version
                 FROM docs WHERE source = ?1 AND session_id = ?2",
                params![session.source.name(), session.id],
                |row| {
                    let body_limit: Option<i64> = row.get(4)?;
                    Ok(DocumentState {
                        freshness: row.get(0)?,
                        // Empty is the migration sentinel. A newly parsed session
                        // stores `{}` even when no turn named a model.
                        derived: row.get::<_, i64>(1)? != 0 && !row.get::<_, String>(2)?.is_empty(),
                        body_truncated: row.get::<_, i64>(3)? != 0,
                        body_limit: body_limit.and_then(|limit| usize::try_from(limit).ok()),
                        reader_version: row.get::<_, i64>(5)?,
                    })
                },
            )
            .optional()
            .map_err(sql(&self.path))
    }

    fn update_metadata(&self, session: &SessionSummary) -> Result<()> {
        let (kind, path, locator_id) = locator_parts(&session.locator);
        let workspace = session
            .workspace
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        let usage = session.usage;
        self.conn
            .execute(
                "UPDATE docs
                 SET locator_kind = ?3, locator_path = ?4, locator_id = ?5,
                     title = ?6, workspace = ?7, updated = ?8,
                     tokens_input = COALESCE(?9, tokens_input),
                     tokens_output = COALESCE(?10, tokens_output),
                     cost = COALESCE(?11, cost)
                 WHERE source = ?1 AND session_id = ?2",
                params![
                    session.source.name(),
                    session.id,
                    kind,
                    path.to_string_lossy(),
                    locator_id,
                    session.title,
                    workspace,
                    session.updated.unix_timestamp(),
                    usage.map(|usage| i64::try_from(usage.input).unwrap_or(i64::MAX)),
                    usage.map(|usage| i64::try_from(usage.output).unwrap_or(i64::MAX)),
                    stored_cost(session.source, usage),
                ],
            )
            .map_err(sql(&self.path))?;
        Ok(())
    }

    fn replace_document(
        &mut self,
        session: &SessionSummary,
        freshness: &str,
        transcript: &Transcript,
        max_body: Option<usize>,
    ) -> Result<()> {
        let (kind, path, locator_id) = locator_parts(&session.locator);
        let workspace = session
            .workspace
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        let usage = session.usage.or_else(|| transcript.usage());
        let turns = i64::try_from(transcript.turns.len()).unwrap_or(i64::MAX);
        let first_prompt = transcript.first_user_prompt().map(str::to_owned);
        let searchable = transcript.searchable_text();
        let (body, body_truncated) = truncate_body(&searchable, max_body);
        let body_limit = max_body.map(|limit| i64::try_from(limit).unwrap_or(i64::MAX));
        let model_usage = serde_json::to_string(&transcript.usage_by_model())?;
        let tx = self.conn.transaction().map_err(sql(&self.path))?;
        tx.execute(
            "DELETE FROM body WHERE rowid IN (
                 SELECT rowid FROM docs WHERE source = ?1 AND session_id = ?2
             )",
            params![session.source.name(), session.id],
        )
        .map_err(sql(&self.path))?;
        tx.execute(
            "DELETE FROM docs WHERE source = ?1 AND session_id = ?2",
            params![session.source.name(), session.id],
        )
        .map_err(sql(&self.path))?;
        tx.execute(
            "INSERT INTO docs (
                source, session_id, locator_kind, locator_path, locator_id,
                freshness, title, workspace, updated, tokens_input, tokens_output,
                cost, turns, first_prompt, model_usage, body_truncated, body_limit,
                reader_version
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                ?15, ?16, ?17, ?18
             )",
            params![
                session.source.name(),
                session.id,
                kind,
                path.to_string_lossy(),
                locator_id,
                freshness,
                session.title,
                workspace,
                session.updated.unix_timestamp(),
                usage.map(|usage| i64::try_from(usage.input).unwrap_or(i64::MAX)),
                usage.map(|usage| i64::try_from(usage.output).unwrap_or(i64::MAX)),
                stored_cost(session.source, usage),
                turns,
                first_prompt,
                model_usage,
                i64::from(body_truncated),
                body_limit,
                READER_VERSION,
            ],
        )
        .map_err(sql(&self.path))?;
        let rowid = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO body (rowid, text) VALUES (?1, ?2)",
            params![rowid, body],
        )
        .map_err(sql(&self.path))?;
        tx.commit().map_err(sql(&self.path))?;
        Ok(())
    }

    fn prune_to(&mut self, max_bytes: u64) -> Result<usize> {
        let mut removed = 0;
        while self.indexed_body_bytes()? > max_bytes {
            let oldest = self
                .conn
                .query_row(
                    "SELECT docs.source, docs.session_id
                     FROM docs JOIN body ON body.rowid = docs.rowid
                     ORDER BY docs.updated, docs.rowid LIMIT 1",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()
                .map_err(sql(&self.path))?;
            let Some((source, session_id)) = oldest else {
                break;
            };
            self.remove_document(&source, &session_id)?;
            removed += 1;
        }
        Ok(removed)
    }

    fn indexed_body_bytes(&self) -> Result<u64> {
        let bytes: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(SUM(length(CAST(text AS BLOB))), 0) FROM body",
                [],
                |row| row.get(0),
            )
            .map_err(sql(&self.path))?;
        Ok(u64::try_from(bytes).unwrap_or(0))
    }

    fn indexed_keys(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT source, session_id FROM docs")
            .map_err(sql(&self.path))?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(sql(&self.path))?;
        let mut keys = Vec::new();
        for row in rows {
            keys.push(row.map_err(sql(&self.path))?);
        }
        Ok(keys)
    }

    fn remove_document(&mut self, source: &str, session_id: &str) -> Result<()> {
        let tx = self.conn.transaction().map_err(sql(&self.path))?;
        tx.execute(
            "DELETE FROM body WHERE rowid IN (
                 SELECT rowid FROM docs WHERE source = ?1 AND session_id = ?2
             )",
            params![source, session_id],
        )
        .map_err(sql(&self.path))?;
        tx.execute(
            "DELETE FROM docs WHERE source = ?1 AND session_id = ?2",
            params![source, session_id],
        )
        .map_err(sql(&self.path))?;
        tx.commit().map_err(sql(&self.path))?;
        Ok(())
    }
}

/// Add cache fields in place so a large existing FTS index never needs a
/// rebuild merely to gain accounting metadata.
/// Bumped whenever an adapter changes what it produces for the same bytes.
///
/// Freshness alone cannot see this: the file did not change, the reader did.
/// Codex once reported `threads.tokens_used` as `input`, and correcting that
/// left 301 million phantom input tokens in every already-indexed row because
/// nothing asked for a reparse. A version mismatch does.
pub(crate) const READER_VERSION: i64 = 2;

fn migrate_docs(conn: &Connection, path: &Path) -> Result<()> {
    let mut present = HashSet::new();
    let mut statement = conn
        .prepare("SELECT name FROM pragma_table_info('docs')")
        .map_err(sql(path))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(sql(path))?;
    for column in columns {
        present.insert(column.map_err(sql(path))?);
    }
    drop(statement);
    for (name, sql_type) in [
        ("tokens_input", "INTEGER"),
        ("tokens_output", "INTEGER"),
        ("cost", "REAL"),
        ("turns", "INTEGER"),
        ("first_prompt", "TEXT"),
        // An empty value marks a migrated row whose body-derived model cache
        // must be populated once by `doc_state`.
        ("model_usage", "TEXT NOT NULL DEFAULT ''"),
        ("body_truncated", "INTEGER NOT NULL DEFAULT 0"),
        ("body_limit", "INTEGER"),
        // 0 marks a row written before reader versioning; it is reparsed once.
        ("reader_version", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        if !present.contains(name) {
            conn.execute_batch(&format!("ALTER TABLE docs ADD COLUMN {name} {sql_type}"))
                .map_err(sql(path))?;
        }
    }
    Ok(())
}

fn cached_usage(input: Option<i64>, output: Option<i64>, cost: Option<f64>) -> Usage {
    Usage {
        input: u64::try_from(input.unwrap_or(0)).unwrap_or(0),
        output: u64::try_from(output.unwrap_or(0)).unwrap_or(0),
        cost: cost.filter(|cost| cost.is_finite()).unwrap_or(0.0),
    }
}

/// Codex records token totals but no dollar amount, so its cache cost remains
/// `NULL`; a numeric zero remains reserved for an agent that explicitly
/// recorded zero spend.
fn stored_cost(source: SourceId, usage: Option<Usage>) -> Option<f64> {
    (source != SourceId::Codex)
        .then(|| usage.map(|usage| usage.cost))
        .flatten()
}

fn freshness(session: &SessionSummary) -> Result<String> {
    match &session.locator {
        Locator::File { path } => {
            let metadata = std::fs::metadata(path).map_err(io(path))?;
            let modified = metadata.modified().map_err(io(path))?;
            let mtime = OffsetDateTime::from(modified).unix_timestamp_nanos();
            Ok(format!("file:{}:{mtime}", metadata.len()))
        }
        Locator::Row { .. } => Ok(format!("row:{}", session.updated.unix_timestamp_nanos())),
    }
}

fn locator_parts(locator: &Locator) -> (&'static str, &Path, Option<&str>) {
    match locator {
        Locator::File { path } => ("file", path, None),
        Locator::Row { db, id } => ("row", db, Some(id)),
    }
}

fn needs_reindex_for_body_limit(state: &DocumentState, max_body: Option<usize>) -> bool {
    match (max_body, state.body_limit) {
        // An unlimited refresh must restore a previously cut body.
        (None, _) => state.body_truncated,
        // A new lower cap has to be enforced even when the old body was short.
        (Some(_), None) => true,
        (Some(current), Some(previous)) if current < previous => true,
        // A larger cap matters only if the old cap actually cut text.
        (Some(current), Some(previous)) if current > previous => state.body_truncated,
        _ => false,
    }
}

/// Keep FTS input valid UTF-8: byte caps must never split a character.
fn truncate_body(text: &str, max_body: Option<usize>) -> (&str, bool) {
    let Some(limit) = max_body else {
        return (text, false);
    };
    if text.len() <= limit {
        return (text, false);
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

fn merge_usage(total: &mut Usage, usage: Usage) {
    total.input = total.input.saturating_add(usage.input);
    total.output = total.output.saturating_add(usage.output);
    total.cost += usage.cost;
}

fn model_usage_from_json(raw: &str) -> rusqlite::Result<BTreeMap<String, Usage>> {
    if raw.is_empty() {
        return Ok(BTreeMap::new());
    }
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|_| invalid_index_value("model usage"))?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid_index_value("model usage"))?;
    let mut usage_by_model = BTreeMap::new();
    for (model, value) in object {
        let usage = value
            .as_object()
            .ok_or_else(|| invalid_index_value("model usage"))?;
        let input = usage
            .get("input")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| invalid_index_value("model usage"))?;
        let output = usage
            .get("output")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| invalid_index_value("model usage"))?;
        let cost = usage
            .get("cost")
            .and_then(serde_json::Value::as_f64)
            .filter(|cost| cost.is_finite())
            .ok_or_else(|| invalid_index_value("model usage"))?;
        usage_by_model.insert(
            model.clone(),
            Usage {
                input,
                output,
                cost,
            },
        );
    }
    Ok(usage_by_model)
}

/// Turn arbitrary user input into quoted terms, rather than exposing FTS5's
/// query language. This makes operators and unmatched quotes harmless while
/// preserving the useful default that every typed word must match.
fn match_expression(input: &str) -> Option<String> {
    let terms: Vec<String> = input
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{term}\""))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" AND "))
}

fn hit_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Hit> {
    let source_name: String = row.get(0)?;
    let source = SourceId::parse(&source_name).ok_or_else(|| invalid_index_value("source"))?;
    let updated: i64 = row.get(4)?;
    let updated = OffsetDateTime::from_unix_timestamp(updated)
        .map_err(|_| invalid_index_value("updated timestamp"))?;
    Ok(Hit {
        source,
        session_id: row.get(1)?,
        title: row.get(2)?,
        workspace: row.get::<_, Option<String>>(3)?.map(PathBuf::from),
        updated,
        snippet: row.get(5)?,
    })
}

fn invalid_index_value(field: &'static str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid {field} in search index"),
        )),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::error::Error;
    use crate::model::{Block, Role, Transcript, Turn};

    struct FakeSource {
        id: SourceId,
        bodies: BTreeMap<String, String>,
        usage: Option<Usage>,
        model: Option<String>,
        fails: bool,
    }

    impl Source for FakeSource {
        fn id(&self) -> SourceId {
            self.id
        }

        fn roots(&self) -> Vec<PathBuf> {
            Vec::new()
        }
        fn list(&self) -> Result<Vec<SessionSummary>> {
            Ok(Vec::new())
        }

        fn open(&self, locator: &Locator) -> Result<Transcript> {
            if self.fails {
                return Err(Error::Unsupported {
                    agent: self.id,
                    what: "fixture transcript",
                });
            }
            let id = match locator {
                Locator::Row { id, .. } => id,
                Locator::File { .. } => {
                    return Err(Error::Unsupported {
                        agent: self.id,
                        what: "fixture file transcript",
                    });
                }
            };
            let mut transcript = Transcript::new(id, self.id);
            let mut turn = Turn::new(Role::User);
            turn.blocks.push(Block::text(
                self.bodies.get(id).cloned().unwrap_or_default(),
            ));
            transcript.push(turn);
            if let Some(usage) = self.usage {
                let mut assistant = Turn::new(Role::Assistant);
                assistant.usage = Some(usage);
                assistant.model = self.model.clone();
                assistant.blocks.push(Block::text("answer"));
                transcript.push(assistant);
            }
            Ok(transcript)
        }
    }

    fn session(source: SourceId, id: &str, updated: OffsetDateTime) -> SessionSummary {
        SessionSummary {
            title: Some(format!("session {id}")),
            ..SessionSummary::new(
                id.to_owned(),
                source,
                updated,
                Locator::Row {
                    db: PathBuf::from("fixture.db"),
                    id: id.to_owned(),
                },
            )
        }
    }

    fn source(id: SourceId, bodies: &[(&str, &str)]) -> Box<dyn Source> {
        Box::new(FakeSource {
            id,
            bodies: bodies
                .iter()
                .map(|(id, body)| ((*id).to_owned(), (*body).to_owned()))
                .collect(),
            usage: None,
            model: None,
            fails: false,
        })
    }

    #[test]
    fn opening_legacy_docs_table_adds_cache_columns_without_losing_search_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE docs (
                source TEXT NOT NULL, session_id TEXT NOT NULL, locator_kind TEXT NOT NULL,
                locator_path TEXT NOT NULL, locator_id TEXT, freshness TEXT NOT NULL,
                title TEXT, workspace TEXT, updated INTEGER NOT NULL,
                UNIQUE (source, session_id)
             );
             CREATE VIRTUAL TABLE body USING fts5(text);
             INSERT INTO docs VALUES ('omp', 's', 'row', 'fixture.db', 's',
                 'row:0', 'legacy', '/workspace', 0);
             INSERT INTO body(rowid, text) VALUES (1, 'preserved needle');",
        )
        .unwrap();
        drop(conn);

        let index = Index::open(&path).unwrap();
        let columns: HashSet<String> = index
            .conn
            .prepare("SELECT name FROM pragma_table_info('docs')")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(columns.is_superset(&HashSet::from([
            "tokens_input".to_owned(),
            "tokens_output".to_owned(),
            "cost".to_owned(),
            "turns".to_owned(),
            "first_prompt".to_owned(),
            "model_usage".to_owned(),
            "body_truncated".to_owned(),
            "body_limit".to_owned(),
        ])));
        assert_eq!(index.search("preserved", 1).unwrap()[0].session_id, "s");
    }

    #[test]
    fn refresh_caches_authoritative_usage_per_session_and_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(&dir.path().join("index.db")).unwrap();
        let sources = vec![source(SourceId::OpenCode, &[("one", "prompt text")])];
        let mut indexed = session(SourceId::OpenCode, "one", OffsetDateTime::UNIX_EPOCH);
        indexed.workspace = Some(PathBuf::from("/workspace"));
        indexed.usage = Some(Usage {
            input: 8,
            output: 13,
            cost: 1.25,
        });

        index.refresh(&[indexed], &sources).unwrap();

        assert_eq!(
            index.usage().unwrap().get(SourceId::OpenCode, "one"),
            Some(Usage {
                input: 8,
                output: 13,
                cost: 1.25,
            })
        );
        assert_eq!(
            index
                .usage_by_workspace()
                .unwrap()
                .get(&PathBuf::from("/workspace")),
            Some(&Usage {
                input: 8,
                output: 13,
                cost: 1.25,
            })
        );
    }

    #[test]
    fn refresh_caches_transcript_usage_and_first_prompt_when_tier_one_has_none() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(&dir.path().join("index.db")).unwrap();
        let sources: Vec<Box<dyn Source>> = vec![Box::new(FakeSource {
            id: SourceId::Omp,
            bodies: BTreeMap::from([("one".to_owned(), "first prompt".to_owned())]),
            usage: Some(Usage {
                input: 21,
                output: 34,
                cost: 0.5,
            }),
            model: Some("model-a".to_owned()),
            fails: false,
        })];
        let session = session(SourceId::Omp, "one", OffsetDateTime::UNIX_EPOCH);

        index.refresh(&[session], &sources).unwrap();

        assert_eq!(
            index.usage().unwrap().get(SourceId::Omp, "one"),
            Some(Usage {
                input: 21,
                output: 34,
                cost: 0.5,
            })
        );
        assert_eq!(
            index
                .model_usage_per_session()
                .unwrap()
                .get(&(SourceId::Omp, "one".to_owned()))
                .and_then(|models| models.get("model-a")),
            Some(&Usage {
                input: 21,
                output: 34,
                cost: 0.5,
            })
        );
        assert_eq!(
            index
                .digest_data()
                .unwrap()
                .prompts
                .get(&(SourceId::Omp, "one".to_owned()))
                .map(String::as_str),
            Some("first prompt")
        );
    }

    #[test]
    fn unchanged_refresh_does_not_reparse_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(&dir.path().join("index.db")).unwrap();
        let sources = vec![source(SourceId::Omp, &[("one", "alpha"), ("two", "beta")])];
        let sessions = vec![
            session(SourceId::Omp, "one", OffsetDateTime::UNIX_EPOCH),
            session(SourceId::Omp, "two", OffsetDateTime::UNIX_EPOCH),
        ];

        assert_eq!(index.refresh(&sessions, &sources).unwrap().reindexed, 2);
        let again = index.refresh(&sessions, &sources).unwrap();
        assert_eq!(again.reindexed, 0);
    }

    #[test]
    fn changing_one_freshness_key_reindexes_only_that_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(&dir.path().join("index.db")).unwrap();
        let sources = vec![source(SourceId::Omp, &[("one", "alpha"), ("two", "beta")])];
        let initial = vec![
            session(SourceId::Omp, "one", OffsetDateTime::UNIX_EPOCH),
            session(SourceId::Omp, "two", OffsetDateTime::UNIX_EPOCH),
        ];
        index.refresh(&initial, &sources).unwrap();
        let changed = vec![
            session(
                SourceId::Omp,
                "one",
                OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(1),
            ),
            session(SourceId::Omp, "two", OffsetDateTime::UNIX_EPOCH),
        ];

        assert_eq!(index.refresh(&changed, &sources).unwrap().reindexed, 1);
    }

    #[test]
    fn vanished_session_is_removed_from_search() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(&dir.path().join("index.db")).unwrap();
        let sources = vec![source(SourceId::Omp, &[("gone", "vanishing needle")])];
        let sessions = vec![session(SourceId::Omp, "gone", OffsetDateTime::UNIX_EPOCH)];
        index.refresh(&sessions, &sources).unwrap();

        let stats = index.refresh(&[], &sources).unwrap();
        assert_eq!(stats.removed, 1);
        assert!(index.search("vanishing", 10).unwrap().is_empty());
    }

    #[test]
    fn unbalanced_quote_is_sanitized_before_fts() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(&dir.path().join("index.db")).unwrap();
        let sources = vec![source(SourceId::Omp, &[("one", "needle in a haystack")])];
        let sessions = vec![session(SourceId::Omp, "one", OffsetDateTime::UNIX_EPOCH)];
        index.refresh(&sessions, &sources).unwrap();

        assert_eq!(index.search("\"needle", 10).unwrap().len(), 1);
    }

    #[test]
    fn failing_source_is_counted_without_aborting_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(&dir.path().join("index.db")).unwrap();
        let sources: Vec<Box<dyn Source>> = vec![Box::new(FakeSource {
            id: SourceId::Claude,
            bodies: BTreeMap::new(),
            usage: None,
            model: None,
            fails: true,
        })];
        let sessions = vec![session(
            SourceId::Claude,
            "broken",
            OffsetDateTime::UNIX_EPOCH,
        )];

        let stats = index.refresh(&sessions, &sources).unwrap();
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.reindexed, 0);
    }
    #[test]
    fn larger_body_limit_reindexes_an_unchanged_truncated_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(&dir.path().join("index.db")).unwrap();
        let sources = vec![source(
            SourceId::Omp,
            &[("one", "prefix 🦀 searchable-tail")],
        )];
        let sessions = vec![session(SourceId::Omp, "one", OffsetDateTime::UNIX_EPOCH)];

        let small = RefreshOptions {
            max_body: Some(10),
            ..RefreshOptions::default()
        };
        assert_eq!(
            index
                .refresh_with(&sessions, &sources, &small)
                .unwrap()
                .reindexed,
            1
        );
        assert!(index.search("searchable", 10).unwrap().is_empty());
        let indexed: String = index
            .conn
            .query_row("SELECT text FROM body", [], |row| row.get(0))
            .unwrap();
        assert_eq!(indexed, "prefix ");

        let large = RefreshOptions {
            max_body: Some(128),
            ..RefreshOptions::default()
        };
        assert_eq!(
            index
                .refresh_with(&sessions, &sources, &large)
                .unwrap()
                .reindexed,
            1
        );
        assert_eq!(index.search("searchable", 10).unwrap().len(), 1);
    }

    #[test]
    fn byte_cap_prunes_the_oldest_document_and_its_fts_row() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(&dir.path().join("index.db")).unwrap();
        let sources = vec![source(
            SourceId::Omp,
            &[("old", "obsolete needle"), ("new", "lasting marker")],
        )];
        let sessions = vec![
            session(SourceId::Omp, "old", OffsetDateTime::UNIX_EPOCH),
            session(
                SourceId::Omp,
                "new",
                OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(1),
            ),
        ];

        let options = RefreshOptions {
            max_bytes: Some(("lasting marker".len() + 1) as u64),
            ..RefreshOptions::default()
        };
        let stats = index.refresh_with(&sessions, &sources, &options).unwrap();
        assert_eq!(stats.removed, 1);
        assert!(index.search("obsolete", 10).unwrap().is_empty());
        assert_eq!(index.search("lasting", 10).unwrap().len(), 1);
        let docs: i64 = index
            .conn
            .query_row("SELECT COUNT(*) FROM docs", [], |row| row.get(0))
            .unwrap();
        let bodies: i64 = index
            .conn
            .query_row("SELECT COUNT(*) FROM body", [], |row| row.get(0))
            .unwrap();
        assert_eq!((docs, bodies), (1, 1));
    }

    #[test]
    fn vacuum_reclaims_deleted_pages_without_breaking_remaining_search() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(&dir.path().join("index.db")).unwrap();
        let discarded = "discarded ".repeat(30_000);
        let sources = vec![source(
            SourceId::Omp,
            &[("old", &discarded), ("keep", "durable search marker")],
        )];
        let sessions = vec![
            session(SourceId::Omp, "old", OffsetDateTime::UNIX_EPOCH),
            session(
                SourceId::Omp,
                "keep",
                OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(1),
            ),
        ];
        index.refresh(&sessions, &sources).unwrap();
        index.refresh(&sessions[1..], &sources).unwrap();
        let before = index.size_bytes().unwrap();

        let reclaimed = index.vacuum().unwrap();

        assert!(reclaimed > 0);
        assert!(reclaimed < before);
        assert_eq!(index.search("durable", 10).unwrap().len(), 1);
    }

    #[test]
    fn usage_cache_is_local_only_even_with_an_absent_remote_display_label() {
        let cached = Usage {
            input: 12,
            output: 34,
            cost: 5.0,
        };
        let mut cache = UsageCache::default();
        cache.insert(SourceId::Omp, "same".into(), cached);
        let local = session(SourceId::Omp, "same", OffsetDateTime::UNIX_EPOCH);
        let mut remote = local.clone();
        remote.origin_label = Some("beta".into());
        remote.origin_id = Some("remote-origin".into());
        assert_eq!(cache.resolve(&local), Some(cached));
        assert_eq!(cache.resolve(&remote), None);
        let own_usage = Usage {
            input: 1,
            output: 2,
            cost: 3.0,
        };
        remote.usage = Some(own_usage);
        assert_eq!(cache.resolve(&remote), Some(own_usage));
        remote.origin_label = None;
        remote.usage = None;
        remote.routes.push(crate::model::AccessRoute {
            hops: vec![crate::model::RouteHop {
                origin_id: "remote-origin".into(),
                name: "beta".into(),
                ssh: "beta".into(),
                url: None,
                token_env: None,
                home: None,
            }],
        });
        let mut sessions = vec![local, remote];
        cache.fill(&mut sessions);
        assert_eq!(sessions[0].usage, Some(cached));
        assert_eq!(sessions[1].usage, None);
    }

    #[test]
    fn refresh_indexes_only_local_rows_and_ignores_colliding_remote_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(&dir.path().join("index.db")).unwrap();
        let local = session(SourceId::Omp, "same", OffsetDateTime::UNIX_EPOCH);
        let mut remote = local.clone();
        remote.origin_label = Some("beta".into());
        remote.title = Some("remote snapshot".into());
        remote.updated += time::Duration::days(1);
        let sources = vec![source(SourceId::Omp, &[("same", "local secret")])];
        let stats = index.refresh(&[local, remote], &sources).unwrap();
        assert_eq!(stats.scanned, 1);
        assert_eq!(stats.reindexed, 1);
        let hits = index.search("secret", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title.as_deref(), Some("session same"));
        assert_eq!(hits[0].updated, OffsetDateTime::UNIX_EPOCH);
    }

    #[test]
    fn digest_prompts_and_model_usage_never_cross_into_remote_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(&dir.path().join("index.db")).unwrap();
        let cached = Usage {
            input: 12,
            output: 34,
            cost: 5.0,
        };
        let sources: Vec<Box<dyn Source>> = vec![Box::new(FakeSource {
            id: SourceId::Omp,
            bodies: BTreeMap::from([("same".into(), "local secret".into())]),
            usage: Some(cached),
            model: Some("local-model".into()),
            fails: false,
        })];
        let mut local = session(SourceId::Omp, "same", OffsetDateTime::UNIX_EPOCH);
        local.origin_id = Some("local-origin".into());
        local.workspace = Some("/workspace".into());
        index
            .refresh(std::slice::from_ref(&local), &sources)
            .unwrap();
        let mut remote = local.clone();
        remote.origin_id = Some("remote-origin".into());
        remote.title = Some("remote title".into());
        remote.model = Some("remote-model".into());
        remote.routes.push(crate::model::AccessRoute {
            hops: vec![crate::model::RouteHop {
                origin_id: "remote-origin".into(),
                name: "beta".into(),
                ssh: "beta".into(),
                url: None,
                token_env: None,
                home: None,
            }],
        });
        let sessions = vec![local, remote];
        let digest =
            crate::output::digest::build(&index, &sessions, OffsetDateTime::UNIX_EPOCH).unwrap();
        let entries = &digest[0].1;
        let local_entry = entries
            .iter()
            .find(|entry| entry.title.as_deref() != Some("remote title"))
            .unwrap();
        let remote_entry = entries
            .iter()
            .find(|entry| entry.title.as_deref() == Some("remote title"))
            .unwrap();
        assert_eq!(local_entry.prompt.as_deref(), Some("local secret"));
        assert_eq!(local_entry.usage, Some(cached));
        assert_eq!(remote_entry.prompt.as_deref(), Some("remote title"));
        assert_eq!(remote_entry.usage, None);

        let rows = crate::output::usage::report(
            &index,
            &sessions,
            crate::output::usage::Axis::Model,
            None,
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
        let local_row = rows.iter().find(|row| row.key == "local-model").unwrap();
        let remote_row = rows.iter().find(|row| row.key == "remote-model").unwrap();
        assert_eq!(local_row.sessions, 1);
        assert_eq!(local_row.priced, 1);
        assert_eq!(local_row.usage, cached);
        assert_eq!(remote_row.sessions, 1);
        assert_eq!(remote_row.priced, 0);
        assert!(remote_row.usage.is_zero());
    }
}
