//! Hybrid logical clock (physical milliseconds + logical counter + node id).
//!
//! Stamps are totally ordered, stay close to wall time, and respect causality:
//! anything a device does after receiving an op is stamped after that op,
//! even if the device's own clock runs behind the sender's.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Hlc {
    pub wall_ms: i64,
    pub counter: u32,
    pub node: String,
}

impl Hlc {
    pub fn new(wall_ms: i64, counter: u32, node: impl Into<String>) -> Self {
        Self {
            wall_ms,
            counter,
            node: node.into(),
        }
    }
}

/// Fixed-width encoding so lexicographic order of the string equals the
/// order of the stamp. Used as the primary key of the op log.
impl fmt::Display for Hlc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:015}-{:08}-{}", self.wall_ms, self.counter, self.node)
    }
}

impl FromStr for Hlc {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.splitn(3, '-');
        let wall = parts.next().ok_or("missing wall")?;
        let counter = parts.next().ok_or("missing counter")?;
        let node = parts.next().ok_or("missing node")?;
        Ok(Self {
            wall_ms: wall.parse().map_err(|e| format!("wall: {e}"))?,
            counter: counter.parse().map_err(|e| format!("counter: {e}"))?,
            node: node.to_string(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct HlcClock {
    node: String,
    wall_ms: i64,
    counter: u32,
}

impl HlcClock {
    pub fn new(node: impl Into<String>) -> Self {
        Self {
            node: node.into(),
            wall_ms: 0,
            counter: 0,
        }
    }

    /// Restores the clock from the last persisted stamp, so a restart with a
    /// wall clock that jumped backwards still produces increasing stamps.
    pub fn restore(node: impl Into<String>, last: Option<&Hlc>) -> Self {
        let mut c = Self::new(node);
        if let Some(h) = last {
            c.wall_ms = h.wall_ms;
            c.counter = h.counter;
        }
        c
    }

    pub fn node(&self) -> &str {
        &self.node
    }

    pub fn last(&self) -> Hlc {
        Hlc::new(self.wall_ms, self.counter, self.node.clone())
    }

    /// Stamp for a local event at physical time `now_ms`.
    pub fn tick(&mut self, now_ms: i64) -> Hlc {
        if now_ms > self.wall_ms {
            self.wall_ms = now_ms;
            self.counter = 0;
        } else {
            self.counter += 1;
        }
        self.last()
    }

    /// Merge a remote stamp observed at physical time `now_ms`.
    pub fn observe(&mut self, remote: &Hlc, now_ms: i64) -> Hlc {
        let wall = self.wall_ms.max(remote.wall_ms).max(now_ms);
        self.counter = if wall == self.wall_ms && wall == remote.wall_ms {
            self.counter.max(remote.counter) + 1
        } else if wall == self.wall_ms {
            self.counter + 1
        } else if wall == remote.wall_ms {
            remote.counter + 1
        } else {
            0
        };
        self.wall_ms = wall;
        self.last()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_round_trips_and_sorts() {
        let a = Hlc::new(1_700_000_000_000, 3, "dev-a");
        let b = Hlc::new(1_700_000_000_000, 12, "dev-a");
        assert_eq!(a.to_string().parse::<Hlc>().unwrap(), a);
        assert!(a < b);
        assert!(a.to_string() < b.to_string());
        let dashed = Hlc::new(5, 0, "crew-7-tablet");
        assert_eq!(dashed.to_string().parse::<Hlc>().unwrap(), dashed);
    }

    #[test]
    fn tick_is_monotonic_when_wall_clock_stalls_or_regresses() {
        let mut c = HlcClock::new("a");
        let s1 = c.tick(1000);
        let s2 = c.tick(1000);
        let s3 = c.tick(900);
        assert!(s1 < s2 && s2 < s3);
        assert_eq!(s3.wall_ms, 1000);
        assert_eq!(s3.counter, 2);
    }

    #[test]
    fn observe_moves_past_remote_ahead_of_local_clock() {
        let mut c = HlcClock::new("a");
        c.tick(1000);
        let remote = Hlc::new(5000, 7, "b");
        let s = c.observe(&remote, 1200);
        assert!(s > remote);
        assert_eq!(s.wall_ms, 5000);
        assert_eq!(s.counter, 8);
        assert!(c.tick(1300) > s);
    }

    #[test]
    fn restore_continues_after_restart() {
        let last = Hlc::new(10_000, 4, "a");
        let mut c = HlcClock::restore("a", Some(&last));
        assert!(c.tick(500) > last);
    }
}
