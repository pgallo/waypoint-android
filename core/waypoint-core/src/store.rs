//! Embedded SQL persistence: the op log, the outbox, local-only tables, migrations.
//!
//! The op log is the source of truth. Materialized state is rebuilt from it on
//! open, which keeps the schema small and makes "what is on disk" and "what
//! gets synced" the same thing. Recording a local op and enqueuing it in the
//! outbox happen in one transaction, so a crash can never leave an edit that
//! is visible locally but never sent.

use crate::crdt::Op;
use crate::geofence::{GeofenceEvent, Transition};
use crate::hlc::Hlc;
use crate::track::{Fix, TrackStats};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("corrupt record: {0}")]
    Corrupt(String),
}

pub type StoreResult<T> = Result<T, StoreError>;

/// Ordered, append-only list of migrations. `PRAGMA user_version` stores how
/// many have been applied. Never edit a released entry; add a new one.
const MIGRATIONS: &[&str] = &[
    // 1: op log, outbox, key/value metadata
    "CREATE TABLE ops (
         id          TEXT PRIMARY KEY,
         entity_kind TEXT NOT NULL,
         entity_id   TEXT NOT NULL,
         body        TEXT NOT NULL,
         origin      TEXT NOT NULL CHECK (origin IN ('local', 'remote'))
     ) WITHOUT ROWID;
     CREATE TABLE outbox (
         op_id       TEXT PRIMARY KEY REFERENCES ops(id),
         enqueued_ms INTEGER NOT NULL,
         attempts    INTEGER NOT NULL DEFAULT 0,
         last_error  TEXT
     ) WITHOUT ROWID;
     CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;",
    // 2: device-local history that is never synced
    "CREATE TABLE tracks (
         id            INTEGER PRIMARY KEY AUTOINCREMENT,
         work_order_id TEXT,
         started_ms    INTEGER NOT NULL,
         ended_ms      INTEGER NOT NULL,
         points_json   TEXT NOT NULL,
         stats_json    TEXT NOT NULL
     );
     CREATE TABLE fence_events (
         id         INTEGER PRIMARY KEY AUTOINCREMENT,
         fence_id   TEXT NOT NULL,
         transition TEXT NOT NULL,
         at_ms      INTEGER NOT NULL,
         distance_m REAL NOT NULL
     );",
    // 3: entity lookups for future partial rebuilds
    "CREATE INDEX ops_by_entity ON ops(entity_kind, entity_id);",
];

pub const SCHEMA_VERSION: usize = MIGRATIONS.len();

#[derive(Debug, Clone, PartialEq)]
pub struct OutboxEntry {
    pub op: Op,
    pub attempts: u32,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredTrack {
    pub id: i64,
    pub work_order_id: Option<String>,
    pub started_ms: i64,
    pub ended_ms: i64,
    pub points: Vec<Fix>,
    pub stats: TrackStats,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> StoreResult<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> StoreResult<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> StoreResult<Self> {
        // WAL lets the UI read while a sync writes; NORMAL is durable at
        // transaction granularity under WAL, which is what the outbox needs.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&mut conn)?;
        Ok(Self { conn })
    }

    pub fn schema_version(&self) -> StoreResult<usize> {
        let v: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        Ok(v as usize)
    }

    pub fn meta_get(&self, key: &str) -> StoreResult<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn meta_set(&self, key: &str, value: &str) -> StoreResult<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Persists a locally produced op and queues it for upload, atomically.
    pub fn record_local(&mut self, op: &Op, now_ms: i64) -> StoreResult<()> {
        let tx = self.conn.transaction()?;
        insert_op(&tx, op, "local")?;
        tx.execute(
            "INSERT INTO outbox(op_id, enqueued_ms) VALUES (?1, ?2)",
            params![op.id.to_string(), now_ms],
        )?;
        tx.execute(
            "INSERT INTO meta(key, value) VALUES ('hlc', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [op.id.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Stores remote ops, ignoring ones already present. Returns the ops that
    /// were new, so the caller applies each exactly once to in-memory state
    /// (applying twice would be harmless, but it would be wasted work).
    pub fn ingest_remote(&mut self, ops: &[Op]) -> StoreResult<Vec<Op>> {
        let tx = self.conn.transaction()?;
        let mut fresh = Vec::new();
        for op in ops {
            if insert_op(&tx, op, "remote")? {
                fresh.push(op.clone());
            }
        }
        tx.commit()?;
        Ok(fresh)
    }

    pub fn load_ops(&self) -> StoreResult<Vec<Op>> {
        let mut stmt = self.conn.prepare("SELECT body FROM ops ORDER BY id")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|r| {
            let body = r?;
            Op::from_json(&body).map_err(|e| StoreError::Corrupt(e.to_string()))
        })
        .collect()
    }

    pub fn op_count(&self) -> StoreResult<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM ops", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    pub fn outbox_len(&self) -> StoreResult<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM outbox", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    /// Oldest-first batch of pending ops.
    pub fn outbox_batch(&self, limit: usize) -> StoreResult<Vec<OutboxEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT o.body, x.attempts, x.last_error
             FROM outbox x JOIN ops o ON o.id = x.op_id
             ORDER BY x.op_id LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        rows.map(|r| {
            let (body, attempts, last_error) = r?;
            Ok(OutboxEntry {
                op: Op::from_json(&body).map_err(|e| StoreError::Corrupt(e.to_string()))?,
                attempts: attempts as u32,
                last_error,
            })
        })
        .collect()
    }

    pub fn outbox_ack(&mut self, ids: &[Hlc]) -> StoreResult<usize> {
        let tx = self.conn.transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare("DELETE FROM outbox WHERE op_id = ?1")?;
            for id in ids {
                n += stmt.execute([id.to_string()])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    pub fn outbox_fail(&mut self, ids: &[Hlc], error: &str) -> StoreResult<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "UPDATE outbox SET attempts = attempts + 1, last_error = ?2 WHERE op_id = ?1",
            )?;
            for id in ids {
                stmt.execute(params![id.to_string(), error])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn save_track(
        &self,
        work_order_id: Option<&str>,
        points: &[Fix],
        stats: &TrackStats,
    ) -> StoreResult<i64> {
        let (start, end) = match (points.first(), points.last()) {
            (Some(a), Some(b)) => (a.timestamp_ms, b.timestamp_ms),
            _ => (0, 0),
        };
        self.conn.execute(
            "INSERT INTO tracks(work_order_id, started_ms, ended_ms, points_json, stats_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                work_order_id,
                start,
                end,
                serde_json::to_string(points).map_err(|e| StoreError::Corrupt(e.to_string()))?,
                serde_json::to_string(stats).map_err(|e| StoreError::Corrupt(e.to_string()))?,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn tracks(&self) -> StoreResult<Vec<StoredTrack>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, work_order_id, started_ms, ended_ms, points_json, stats_json
             FROM tracks ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
            ))
        })?;
        rows.map(|r| {
            let (id, wo, s, e, pts, st) = r?;
            let corrupt = |e: serde_json::Error| StoreError::Corrupt(e.to_string());
            Ok(StoredTrack {
                id,
                work_order_id: wo,
                started_ms: s,
                ended_ms: e,
                points: serde_json::from_str(&pts).map_err(corrupt)?,
                stats: serde_json::from_str(&st).map_err(corrupt)?,
            })
        })
        .collect()
    }

    pub fn log_fence_event(&self, ev: &GeofenceEvent) -> StoreResult<()> {
        let t = match ev.transition {
            Transition::Enter => "enter",
            Transition::Exit => "exit",
        };
        self.conn.execute(
            "INSERT INTO fence_events(fence_id, transition, at_ms, distance_m)
             VALUES (?1, ?2, ?3, ?4)",
            params![ev.fence_id, t, ev.timestamp_ms, ev.distance_m],
        )?;
        Ok(())
    }

    pub fn fence_event_count(&self) -> StoreResult<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM fence_events", [], |r| r.get(0))?;
        Ok(n as usize)
    }
}

fn insert_op(tx: &Transaction<'_>, op: &Op, origin: &str) -> StoreResult<bool> {
    let n = tx.execute(
        "INSERT OR IGNORE INTO ops(id, entity_kind, entity_id, body, origin)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            op.id.to_string(),
            op.entity.kind,
            op.entity.id,
            op.to_json(),
            origin
        ],
    )?;
    Ok(n == 1)
}

fn migrate(conn: &mut Connection) -> StoreResult<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let current = current as usize;
    if current > MIGRATIONS.len() {
        return Err(StoreError::Corrupt(format!(
            "database schema v{current} is newer than this build (v{})",
            MIGRATIONS.len()
        )));
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        // PRAGMA does not accept bound parameters; the value is our own integer.
        tx.execute_batch(&format!("PRAGMA user_version = {}", i + 1))?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crdt::{EntityRef, OpKind};

    fn op(wall: i64, node: &str) -> Op {
        Op {
            id: Hlc::new(wall, 0, node),
            entity: EntityRef::new("work_order", "wo-1"),
            kind: OpKind::Set {
                field: "status".into(),
                value: "open".into(),
            },
        }
    }

    #[test]
    fn fresh_db_is_fully_migrated() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn upgrades_from_v1() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.db");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(MIGRATIONS[0]).unwrap();
            c.execute_batch("PRAGMA user_version = 1").unwrap();
            c.execute("INSERT INTO meta(key, value) VALUES ('node', 'legacy')", [])
                .unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(s.meta_get("node").unwrap().as_deref(), Some("legacy"));
        assert_eq!(s.tracks().unwrap().len(), 0);
    }

    #[test]
    fn refuses_newer_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.db");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch("PRAGMA user_version = 99").unwrap();
        }
        assert!(matches!(Store::open(&path), Err(StoreError::Corrupt(_))));
    }

    #[test]
    fn local_ops_land_in_outbox_and_ack_clears() {
        let mut s = Store::open_in_memory().unwrap();
        s.record_local(&op(1, "a"), 1).unwrap();
        s.record_local(&op(2, "a"), 2).unwrap();
        assert_eq!(s.outbox_len().unwrap(), 2);
        let batch = s.outbox_batch(10).unwrap();
        assert_eq!(batch[0].op.id.wall_ms, 1);
        s.outbox_fail(&[batch[0].op.id.clone()], "offline").unwrap();
        let again = s.outbox_batch(10).unwrap();
        assert_eq!(again[0].attempts, 1);
        assert_eq!(again[0].last_error.as_deref(), Some("offline"));
        assert_eq!(s.outbox_ack(&[batch[0].op.id.clone()]).unwrap(), 1);
        assert_eq!(s.outbox_len().unwrap(), 1);
        assert_eq!(s.op_count().unwrap(), 2);
    }

    #[test]
    fn remote_ingest_is_idempotent() {
        let mut s = Store::open_in_memory().unwrap();
        let ops = vec![op(1, "b"), op(2, "b")];
        assert_eq!(s.ingest_remote(&ops).unwrap().len(), 2);
        assert_eq!(s.ingest_remote(&ops).unwrap().len(), 0);
        assert_eq!(s.outbox_len().unwrap(), 0);
        assert_eq!(s.load_ops().unwrap(), ops);
    }
}
