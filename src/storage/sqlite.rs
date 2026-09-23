//! Read-only access to agents' live `SQLite` stores.
//!
//! These databases are written while we browse — `OpenCode`'s is 4.9 GB with an
//! active WAL, and its session count moved during a single research pass. We
//! open read-only and never issue a statement that can mutate or recover the
//! journal.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::error::{Result, sql};

/// Open for reading, tolerating a WAL we may not be able to attach to.
///
/// A read-only connection still needs the `-shm` file to read a WAL database;
/// when that fails (permissions, or a journal needing recovery) we retry with
/// `immutable=1`, which ignores the WAL entirely. That yields a snapshot that
/// may be seconds stale — the right trade for a browser, and the caller is
/// told via the returned flag so staleness can be surfaced rather than hidden.
pub fn open(path: &Path) -> Result<(Connection, bool)> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    let uri = format!("file:{}?mode=ro", encode(path));
    match Connection::open_with_flags(&uri, flags) {
        Ok(conn) if probe(&conn).is_ok() => Ok((conn, false)),
        _ => {
            let uri = format!("file:{}?mode=ro&immutable=1", encode(path));
            let conn = Connection::open_with_flags(&uri, flags).map_err(sql(path))?;
            probe(&conn).map_err(sql(path))?;
            Ok((conn, true))
        }
    }
}

/// Cheapest statement that forces the pager to actually touch the database.
fn probe(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))
}

/// Percent-encode the characters `SQLite`'s URI parser would otherwise consume.
fn encode(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| match c {
            '?' => "%3f".to_string(),
            '#' => "%23".to_string(),
            '%' => "%25".to_string(),
            c => c.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_a_wal_database_without_modifying_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.pragma_update(None, "journal_mode", "wal").unwrap();
            conn.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1);")
                .unwrap();
        }
        let before = std::fs::metadata(&db).unwrap().modified().unwrap();

        let (conn, _stale) = open(&db).unwrap();
        let x: i64 = conn.query_row("SELECT x FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(x, 1);
        drop(conn);

        assert_eq!(std::fs::metadata(&db).unwrap().modified().unwrap(), before);
    }

    #[test]
    fn write_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.db");
        Connection::open(&db)
            .unwrap()
            .execute_batch("CREATE TABLE t(x)")
            .unwrap();

        let (conn, _) = open(&db).unwrap();
        assert!(conn.execute("INSERT INTO t VALUES (1)", []).is_err());
    }
}
