//! Direct SQLite writer for the GRDB `memories` table.
//!
//! Schema source of truth: `desktop/Desktop/Sources/Rewind/Core/RewindDatabase.swift:1243`
//! (migration "createMemoriesTable" + later additive migrations for windowTitle/headline).
//!
//! GRDB column-type mapping:
//!   .boolean  → INTEGER (0/1)
//!   .datetime → TEXT "YYYY-MM-DD HH:MM:SS.SSS"
//!   .text     → TEXT
//!   .double   → REAL
//!   .integer  → INTEGER
//!
//! Columns are camelCase (GRDB preserves Swift field names).
//!
//! `backendId` has a UNIQUE constraint — we insert with `backendId = NULL` and
//! `backendSynced = 0` so these rows look like locally-originated memories awaiting sync.

use rusqlite::{params, Connection, Result as SqlResult, Transaction};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Minimal memory payload extracted from a USB-dumped audio segment.
/// Only the fields we actually have at ingest time — the rest take schema defaults.
#[derive(Debug, Clone)]
pub struct IngestMemory {
    pub content: String,
    pub category: String,         // "system" | "interesting" | "manual"
    pub conversation_id: Option<String>,
    pub confidence: Option<f64>,
    pub reasoning: Option<String>,
    pub headline: Option<String>,
    /// Unix seconds. Converted to GRDB datetime string on write.
    pub created_at: u64,
}

impl IngestMemory {
    pub fn new(content: impl Into<String>) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            content: content.into(),
            category: "system".to_string(),
            conversation_id: None,
            confidence: None,
            reasoning: None,
            headline: None,
            created_at: now,
        }
    }
}

/// Format a unix-seconds timestamp as GRDB's datetime string.
/// GRDB stores `Date` as `YYYY-MM-DD HH:MM:SS.SSS` (UTC).
fn grdb_datetime(unix_secs: u64) -> String {
    // Poor-man's formatter to avoid pulling `chrono` for one call site.
    // Algorithm: days since 1970-01-01 → Gregorian Y/M/D.
    // This is civil-time UTC; GRDB stores UTC.
    let days = unix_secs / 86_400;
    let secs_of_day = unix_secs % 86_400;
    let h = secs_of_day / 3600;
    let m = (secs_of_day % 3600) / 60;
    let s = secs_of_day % 60;

    // Howard Hinnant's civil_from_days, adapted. Returns (y, m, d).
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // 0..146096
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // 0..399
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // 0..365
    let mp = (5 * doy + 2) / 153; // 0..11
    let d = doy - (153 * mp + 2) / 5 + 1; // 1..31
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // 1..12
    let year = if month <= 2 { y + 1 } else { y };

    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.000",
        year, month, d, h, m, s
    )
}

/// Source value written to every row inserted via this path.
pub const SOURCE_USB_DUMP: &str = "usb_dump";

/// INSERT statement. We name every column we populate so ordering is irrelevant
/// and later schema migrations (additive columns with defaults) don't break us.
/// Columns NOT listed here take their schema defaults:
///   backendId=NULL, backendSynced=0, tagsJson=NULL, visibility='private',
///   reviewed=0, userReview=NULL, manuallyAdded=0, scoring=NULL,
///   screenshotId=NULL, sourceApp=NULL, windowTitle=NULL, contextSummary=NULL,
///   currentActivity=NULL, isRead=0, isDismissed=0, deleted=0
const INSERT_SQL: &str = "\
    INSERT INTO memories (\
        content, category, source, conversationId, \
        confidence, reasoning, inputDeviceName, headline, \
        createdAt, updatedAt\
    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)";

/// Insert all memories in a single transaction.
/// Returns number of rows inserted.
pub fn batch_insert_memories(db_path: &Path, memories: &[IngestMemory]) -> SqlResult<usize> {
    let mut conn = Connection::open(db_path)?;
    let tx = conn.transaction()?;
    let n = insert_on_tx(&tx, memories)?;
    tx.commit()?;
    Ok(n)
}

fn insert_on_tx(tx: &Transaction<'_>, memories: &[IngestMemory]) -> SqlResult<usize> {
    let mut stmt = tx.prepare(INSERT_SQL)?;
    let mut n = 0usize;
    for m in memories {
        let ts = grdb_datetime(m.created_at);
        stmt.execute(params![
            m.content,
            m.category,
            SOURCE_USB_DUMP,
            m.conversation_id,
            m.confidence,
            m.reasoning,
            "Omi USB", // inputDeviceName
            m.headline,
            ts,  // createdAt
            ts,  // updatedAt
        ])?;
        n += 1;
    }
    Ok(n)
}

/// Read back memories written by this pipeline.
/// Used by P3 to feed the knowledge-graph NER loop without touching Firestore.
#[derive(Debug)]
pub struct LocalMemoryRow {
    pub id: i64,
    pub content: String,
    pub conversation_id: Option<String>,
    pub created_at: String,
}

pub fn fetch_memories_by_source(
    db_path: &Path,
    source: &str,
    limit: usize,
) -> SqlResult<Vec<LocalMemoryRow>> {
    let conn = Connection::open(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT id, content, conversationId, createdAt \
         FROM memories WHERE source = ?1 AND deleted = 0 \
         ORDER BY createdAt DESC LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![source, limit as i64], |r| {
            Ok(LocalMemoryRow {
                id: r.get(0)?,
                content: r.get(1)?,
                conversation_id: r.get(2)?,
                created_at: r.get(3)?,
            })
        })?
        .collect::<SqlResult<Vec<_>>>()?;
    Ok(rows)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::path::PathBuf;

    /// Mirror the GRDB `createMemoriesTable` migration closely enough that our
    /// INSERT hits the same constraints a real DB would.
    fn create_schema(conn: &Connection) {
        conn.execute_batch(
            "
            CREATE TABLE memories (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                backendId       TEXT UNIQUE,
                backendSynced   INTEGER NOT NULL DEFAULT 0,
                content         TEXT NOT NULL,
                category        TEXT NOT NULL,
                tagsJson        TEXT,
                visibility      TEXT NOT NULL DEFAULT 'private',
                reviewed        INTEGER NOT NULL DEFAULT 0,
                userReview      INTEGER,
                manuallyAdded   INTEGER NOT NULL DEFAULT 0,
                scoring         TEXT,
                source          TEXT,
                conversationId  TEXT,
                screenshotId    INTEGER,
                confidence      REAL,
                reasoning       TEXT,
                sourceApp       TEXT,
                windowTitle     TEXT,
                contextSummary  TEXT,
                currentActivity TEXT,
                inputDeviceName TEXT,
                headline        TEXT,
                isRead          INTEGER NOT NULL DEFAULT 0,
                isDismissed     INTEGER NOT NULL DEFAULT 0,
                deleted         INTEGER NOT NULL DEFAULT 0,
                createdAt       TEXT NOT NULL,
                updatedAt       TEXT NOT NULL
            );
            CREATE UNIQUE INDEX idx_memories_backend_id ON memories(backendId);
            CREATE INDEX idx_memories_created ON memories(createdAt);
            CREATE INDEX idx_memories_source ON memories(source);
            ",
        )
        .expect("schema create");
    }

    fn temp_db() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "omi_test_{}_{}.sqlite",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let conn = Connection::open(&p).unwrap();
        create_schema(&conn);
        p
    }

    #[test]
    fn grdb_datetime_format() {
        // 2021-01-01 00:00:00 UTC
        assert_eq!(grdb_datetime(1_609_459_200), "2021-01-01 00:00:00.000");
        // 2024-06-15 12:30:45 UTC
        assert_eq!(grdb_datetime(1_718_454_645), "2024-06-15 12:30:45.000");
        // Unix epoch
        assert_eq!(grdb_datetime(0), "1970-01-01 00:00:00.000");
    }

    #[test]
    fn inserts_single_memory() {
        let db = temp_db();
        let mems = vec![IngestMemory::new("test content")];
        let n = batch_insert_memories(&db, &mems).unwrap();
        assert_eq!(n, 1);

        let conn = Connection::open(&db).unwrap();
        let (content, source, device): (String, String, String) = conn
            .query_row(
                "SELECT content, source, inputDeviceName FROM memories WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(content, "test content");
        assert_eq!(source, SOURCE_USB_DUMP);
        assert_eq!(device, "Omi USB");
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn batch_insert_1k_is_transactional() {
        let db = temp_db();
        let mems: Vec<_> = (0..1000)
            .map(|i| {
                let mut m = IngestMemory::new(format!("memory #{i}"));
                m.conversation_id = Some(format!("conv-{}", i / 10));
                m.confidence = Some(0.5 + (i as f64) / 2000.0);
                m.created_at = 1_700_000_000 + i as u64 * 60;
                m
            })
            .collect();

        let n = batch_insert_memories(&db, &mems).unwrap();
        assert_eq!(n, 1000);

        let conn = Connection::open(&db).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE source = ?1",
                [SOURCE_USB_DUMP],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1000);

        // Spot-check the last row's timestamp formatting
        let last_ts: String = conn
            .query_row(
                "SELECT createdAt FROM memories ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // 1_700_000_000 + 999*60 = 1700059940 → 2023-11-15 14:52:20
        assert_eq!(last_ts, "2023-11-15 14:52:20.000");

        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn defaults_are_applied() {
        let db = temp_db();
        let mems = vec![IngestMemory::new("hello")];
        batch_insert_memories(&db, &mems).unwrap();

        let conn = Connection::open(&db).unwrap();
        let (synced, vis, deleted, backend_id): (i64, String, i64, Option<String>) = conn
            .query_row(
                "SELECT backendSynced, visibility, deleted, backendId FROM memories LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(synced, 0);
        assert_eq!(vis, "private");
        assert_eq!(deleted, 0);
        assert!(backend_id.is_none()); // NULL, so sync service will pick it up
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn fetch_by_source_roundtrip() {
        let db = temp_db();
        let mems: Vec<_> = (0..50)
            .map(|i| {
                let mut m = IngestMemory::new(format!("m{i}"));
                m.created_at = 1_700_000_000 + i as u64;
                m
            })
            .collect();
        batch_insert_memories(&db, &mems).unwrap();

        // Insert a decoy with a different source
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO memories (content, category, source, createdAt, updatedAt) \
             VALUES ('decoy', 'system', 'desktop', '2023-01-01 00:00:00.000', '2023-01-01 00:00:00.000')",
            [],
        )
        .unwrap();

        let rows = fetch_memories_by_source(&db, SOURCE_USB_DUMP, 100).unwrap();
        assert_eq!(rows.len(), 50);
        // Ordered DESC by createdAt → last inserted comes first
        assert_eq!(rows[0].content, "m49");
        assert_eq!(rows[49].content, "m0");
        // Decoy excluded
        assert!(!rows.iter().any(|r| r.content == "decoy"));

        std::fs::remove_file(&db).ok();
    }
}
