//! Sync transport abstraction and an in-memory relay.
//!
//! The relay is deliberately dumb: an append-only, deduplicated op log with a
//! sequence cursor. It never merges anything. All conflict resolution happens
//! on devices through the CRDT, so the relay can be replaced by any store that
//! offers "append" and "read from offset" (an object store, a queue, a table).

use crate::crdt::Op;
use crate::hlc::Hlc;
use std::collections::HashSet;

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum TransportError {
    #[error("network unavailable")]
    Offline,
    #[error("relay rejected request: {0}")]
    Rejected(String),
}

pub trait SyncTransport {
    /// Uploads ops; returns the ids the relay has durably stored.
    fn push(&mut self, ops: &[Op]) -> Result<Vec<Hlc>, TransportError>;
    /// Returns ops after `cursor` (at most `limit`) and the next cursor.
    fn pull(&mut self, cursor: u64, limit: usize) -> Result<(Vec<Op>, u64), TransportError>;
}

#[derive(Debug, Default, Clone)]
pub struct InMemoryRelay {
    log: Vec<Op>,
    ids: HashSet<Hlc>,
}

impl InMemoryRelay {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.log.len()
    }

    pub fn is_empty(&self) -> bool {
        self.log.is_empty()
    }
}

impl SyncTransport for InMemoryRelay {
    fn push(&mut self, ops: &[Op]) -> Result<Vec<Hlc>, TransportError> {
        let mut acked = Vec::with_capacity(ops.len());
        for op in ops {
            if self.ids.insert(op.id.clone()) {
                self.log.push(op.clone());
            }
            // Duplicates are acked too: the relay already has them.
            acked.push(op.id.clone());
        }
        Ok(acked)
    }

    fn pull(&mut self, cursor: u64, limit: usize) -> Result<(Vec<Op>, u64), TransportError> {
        let start = (cursor as usize).min(self.log.len());
        let end = (start + limit).min(self.log.len());
        Ok((self.log[start..end].to_vec(), end as u64))
    }
}

/// The same relay contract persisted in an embedded SQL database. The `seq` rowid is the
/// cursor, and the unique op id makes re-pushes after a lost ack harmless.
/// Used by the Android demo so the simulated peer survives app restarts.
pub struct SqliteRelay {
    conn: rusqlite::Connection,
}

impl SqliteRelay {
    pub fn open(path: impl AsRef<std::path::Path>) -> rusqlite::Result<Self> {
        Self::init(rusqlite::Connection::open(path)?)
    }

    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::init(rusqlite::Connection::open_in_memory()?)
    }

    fn init(conn: rusqlite::Connection) -> rusqlite::Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS relay_log (
                 seq  INTEGER PRIMARY KEY AUTOINCREMENT,
                 id   TEXT NOT NULL UNIQUE,
                 body TEXT NOT NULL
             );",
        )?;
        Ok(Self { conn })
    }

    pub fn len(&self) -> usize {
        self.conn
            .query_row("SELECT COUNT(*) FROM relay_log", [], |r| r.get::<_, i64>(0))
            .map_or(0, |n| n as usize)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn rejected(e: impl std::fmt::Display) -> TransportError {
    TransportError::Rejected(e.to_string())
}

impl SyncTransport for SqliteRelay {
    fn push(&mut self, ops: &[Op]) -> Result<Vec<Hlc>, TransportError> {
        let tx = self.conn.transaction().map_err(rejected)?;
        {
            let mut stmt = tx
                .prepare("INSERT OR IGNORE INTO relay_log(id, body) VALUES (?1, ?2)")
                .map_err(rejected)?;
            for op in ops {
                stmt.execute([op.id.to_string(), op.to_json()])
                    .map_err(rejected)?;
            }
        }
        tx.commit().map_err(rejected)?;
        Ok(ops.iter().map(|o| o.id.clone()).collect())
    }

    fn pull(&mut self, cursor: u64, limit: usize) -> Result<(Vec<Op>, u64), TransportError> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, body FROM relay_log WHERE seq > ?1 ORDER BY seq LIMIT ?2")
            .map_err(rejected)?;
        let rows = stmt
            .query_map([cursor as i64, limit as i64], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(rejected)?;
        let mut next = cursor;
        let mut ops = Vec::new();
        for row in rows {
            let (seq, body) = row.map_err(rejected)?;
            ops.push(Op::from_json(&body).map_err(rejected)?);
            next = seq as u64;
        }
        Ok((ops, next))
    }
}

/// A transport that is always offline, for exercising the retry path.
#[derive(Debug, Default)]
pub struct OfflineTransport;

impl SyncTransport for OfflineTransport {
    fn push(&mut self, _ops: &[Op]) -> Result<Vec<Hlc>, TransportError> {
        Err(TransportError::Offline)
    }
    fn pull(&mut self, _cursor: u64, _limit: usize) -> Result<(Vec<Op>, u64), TransportError> {
        Err(TransportError::Offline)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub pushed: usize,
    pub pulled: usize,
    pub applied: usize,
    pub pending: usize,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crdt::{EntityRef, OpKind};

    fn op(n: i64) -> Op {
        Op {
            id: Hlc::new(n, 0, "a"),
            entity: EntityRef::new("site", "s"),
            kind: OpKind::Set {
                field: "name".into(),
                value: n.to_string(),
            },
        }
    }

    #[test]
    fn relay_dedups_and_pages() {
        let mut r = InMemoryRelay::new();
        assert_eq!(r.push(&[op(1), op(2)]).unwrap().len(), 2);
        assert_eq!(r.push(&[op(2), op(3)]).unwrap().len(), 2);
        assert_eq!(r.len(), 3);
        let (page, c) = r.pull(0, 2).unwrap();
        assert_eq!((page.len(), c), (2, 2));
        let (page, c) = r.pull(c, 2).unwrap();
        assert_eq!((page.len(), c), (1, 3));
        let (page, c) = r.pull(c, 2).unwrap();
        assert_eq!((page.len(), c), (0, 3));
    }

    #[test]
    fn sqlite_relay_persists_and_dedups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay.db");
        {
            let mut r = SqliteRelay::open(&path).unwrap();
            r.push(&[op(1), op(2)]).unwrap();
            r.push(&[op(2), op(3)]).unwrap();
        }
        let mut r = SqliteRelay::open(&path).unwrap();
        assert_eq!(r.len(), 3);
        let (page, c) = r.pull(0, 2).unwrap();
        assert_eq!(page, vec![op(1), op(2)]);
        let (page, c) = r.pull(c, 10).unwrap();
        assert_eq!(page, vec![op(3)]);
        assert_eq!(r.pull(c, 10).unwrap().0.len(), 0);
    }
}
