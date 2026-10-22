//! Replicated state as a join-semilattice built from an operation log.
//!
//! Two primitives cover the domain:
//! * a last-writer-wins register per (entity, field), ordered by HLC stamp;
//! * an observed-remove set per (entity, set name), where every add carries a
//!   unique tag (its HLC stamp) and a remove tombstones only the tags it saw.
//!
//! Every component merges with a join (max or union), so applying ops in any
//! order, any number of times, gives the same state. The property tests in
//! `tests/merge_laws.rs` check that claim rather than trusting this comment.

use crate::hlc::Hlc;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EntityRef {
    pub kind: String,
    pub id: String,
}

impl EntityRef {
    pub fn new(kind: &str, id: &str) -> Self {
        Self {
            kind: kind.to_string(),
            id: id.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum OpKind {
    /// Write a field. Values are opaque strings; the domain layer owns encoding.
    Set { field: String, value: String },
    /// Add an element to a named OR-set. The op's own stamp is the add tag.
    Add { set: String, element: String },
    /// Remove the add tags observed locally for this element.
    Remove {
        set: String,
        element: String,
        observed: Vec<Hlc>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Op {
    pub id: Hlc,
    pub entity: EntityRef,
    #[serde(flatten)]
    pub kind: OpKind,
}

impl Op {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("op serialization cannot fail")
    }

    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

type FieldKey = (EntityRef, String);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplicaState {
    /// (entity, field) -> (stamp, value). The pair compares stamp first, then
    /// value, so even a duplicated stamp with different payloads resolves the
    /// same way on every replica.
    registers: BTreeMap<FieldKey, (Hlc, String)>,
    /// (entity, set) -> element -> add tags.
    adds: BTreeMap<FieldKey, BTreeMap<String, BTreeSet<Hlc>>>,
    /// Removed add tags. Kept as a set of stamps (not per element) because a
    /// tag is globally unique; a remove that arrives before its add still wins.
    tombstones: BTreeSet<Hlc>,
}

impl ReplicaState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, op: &Op) {
        match &op.kind {
            OpKind::Set { field, value } => {
                let key = (op.entity.clone(), field.clone());
                let candidate = (op.id.clone(), value.clone());
                match self.registers.get(&key) {
                    Some(cur) if *cur >= candidate => {}
                    _ => {
                        self.registers.insert(key, candidate);
                    }
                }
            }
            OpKind::Add { set, element } => {
                self.adds
                    .entry((op.entity.clone(), set.clone()))
                    .or_default()
                    .entry(element.clone())
                    .or_default()
                    .insert(op.id.clone());
            }
            OpKind::Remove { observed, .. } => {
                self.tombstones.extend(observed.iter().cloned());
            }
        }
    }

    /// State-based join: the least upper bound of `self` and `other`.
    pub fn merge(&mut self, other: &ReplicaState) {
        for (k, v) in &other.registers {
            match self.registers.get(k) {
                Some(cur) if cur >= v => {}
                _ => {
                    self.registers.insert(k.clone(), v.clone());
                }
            }
        }
        for (k, elems) in &other.adds {
            let mine = self.adds.entry(k.clone()).or_default();
            for (e, tags) in elems {
                mine.entry(e.clone())
                    .or_default()
                    .extend(tags.iter().cloned());
            }
        }
        self.tombstones.extend(other.tombstones.iter().cloned());
    }

    pub fn get(&self, entity: &EntityRef, field: &str) -> Option<&str> {
        self.registers
            .get(&(entity.clone(), field.to_string()))
            .map(|(_, v)| v.as_str())
    }

    pub fn get_stamped(&self, entity: &EntityRef, field: &str) -> Option<(&Hlc, &str)> {
        self.registers
            .get(&(entity.clone(), field.to_string()))
            .map(|(h, v)| (h, v.as_str()))
    }

    /// Live elements of an OR-set: those with at least one non-tombstoned tag.
    pub fn elements(&self, entity: &EntityRef, set: &str) -> Vec<String> {
        self.adds
            .get(&(entity.clone(), set.to_string()))
            .map(|m| {
                m.iter()
                    .filter(|(_, tags)| tags.iter().any(|t| !self.tombstones.contains(t)))
                    .map(|(e, _)| e.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Tags a local remove must cite: exactly the live tags this replica has seen.
    pub fn live_tags(&self, entity: &EntityRef, set: &str, element: &str) -> Vec<Hlc> {
        self.adds
            .get(&(entity.clone(), set.to_string()))
            .and_then(|m| m.get(element))
            .map(|tags| {
                tags.iter()
                    .filter(|t| !self.tombstones.contains(*t))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Entity ids of a kind that have at least one register written.
    pub fn entity_ids(&self, kind: &str) -> Vec<String> {
        let mut ids: BTreeSet<&str> = BTreeSet::new();
        for (e, _) in self.registers.keys() {
            if e.kind == kind {
                ids.insert(&e.id);
            }
        }
        ids.into_iter().map(str::to_string).collect()
    }

    /// Order-independent digest for quick "did we converge" checks in demos.
    pub fn fingerprint(&self) -> u64 {
        // FNV-1a over a canonical rendering; BTreeMaps iterate in key order.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut feed = |s: &str| {
            for b in s.bytes().chain(std::iter::once(0xff)) {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
        };
        for ((e, f), (stamp, v)) in &self.registers {
            feed(&e.kind);
            feed(&e.id);
            feed(f);
            feed(&stamp.to_string());
            feed(v);
        }
        for ((e, s), elems) in &self.adds {
            for (el, tags) in elems {
                for t in tags {
                    feed(&e.kind);
                    feed(&e.id);
                    feed(s);
                    feed(el);
                    feed(&t.to_string());
                }
            }
        }
        for t in &self.tombstones {
            feed(&t.to_string());
        }
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site() -> EntityRef {
        EntityRef::new("site", "s1")
    }

    fn set(stamp: Hlc, field: &str, value: &str) -> Op {
        Op {
            id: stamp,
            entity: site(),
            kind: OpKind::Set {
                field: field.into(),
                value: value.into(),
            },
        }
    }

    #[test]
    fn lww_keeps_highest_stamp_regardless_of_arrival() {
        let older = set(Hlc::new(10, 0, "a"), "name", "old");
        let newer = set(Hlc::new(20, 0, "b"), "name", "new");
        let mut x = ReplicaState::new();
        x.apply(&newer);
        x.apply(&older);
        assert_eq!(x.get(&site(), "name"), Some("new"));
    }

    #[test]
    fn lww_tie_breaks_on_node_id() {
        let a = set(Hlc::new(10, 0, "a"), "name", "from-a");
        let b = set(Hlc::new(10, 0, "b"), "name", "from-b");
        let mut x = ReplicaState::new();
        x.apply(&b);
        x.apply(&a);
        assert_eq!(x.get(&site(), "name"), Some("from-b"));
    }

    #[test]
    fn concurrent_add_survives_remove() {
        // a adds "gate-code", b sees it and removes it, a re-adds concurrently.
        let add1 = Op {
            id: Hlc::new(1, 0, "a"),
            entity: site(),
            kind: OpKind::Add {
                set: "tags".into(),
                element: "gate-code".into(),
            },
        };
        let mut b = ReplicaState::new();
        b.apply(&add1);
        let rm = Op {
            id: Hlc::new(2, 0, "b"),
            entity: site(),
            kind: OpKind::Remove {
                set: "tags".into(),
                element: "gate-code".into(),
                observed: b.live_tags(&site(), "tags", "gate-code"),
            },
        };
        let add2 = Op {
            id: Hlc::new(2, 0, "a"),
            entity: site(),
            kind: OpKind::Add {
                set: "tags".into(),
                element: "gate-code".into(),
            },
        };
        let mut x = ReplicaState::new();
        for op in [&rm, &add2, &add1] {
            x.apply(op);
        }
        assert_eq!(x.elements(&site(), "tags"), vec!["gate-code".to_string()]);
        // Without the concurrent add the remove wins.
        let mut y = ReplicaState::new();
        y.apply(&rm);
        y.apply(&add1);
        assert!(y.elements(&site(), "tags").is_empty());
    }

    #[test]
    fn op_json_round_trip() {
        let op = Op {
            id: Hlc::new(1, 2, "n"),
            entity: site(),
            kind: OpKind::Remove {
                set: "notes".into(),
                element: "x".into(),
                observed: vec![Hlc::new(1, 0, "m")],
            },
        };
        assert_eq!(Op::from_json(&op.to_json()).unwrap(), op);
    }
}
