//! Field-work domain: sites, work orders, notes, photo metadata.
//!
//! Domain objects are never stored as rows. They are projections over the
//! replicated state: each attribute is an LWW register, each collection an
//! OR-set. This module owns the field names and the encoding of values.

use crate::crdt::{EntityRef, ReplicaState};
use crate::geofence::Shape;
use serde::{Deserialize, Serialize};

pub const SITE: &str = "site";
pub const WORK_ORDER: &str = "work_order";

pub mod field {
    pub const NAME: &str = "name";
    pub const SHAPE: &str = "shape";
    pub const TITLE: &str = "title";
    pub const SITE_ID: &str = "site_id";
    pub const CREW: &str = "crew";
    pub const PRIORITY: &str = "priority";
    pub const STATUS: &str = "status";
    pub const CHECKED_IN_BY: &str = "checked_in_by";
    pub const CHECKED_IN_MS: &str = "checked_in_ms";
    pub const CHECKED_OUT_MS: &str = "checked_out_ms";
}

pub mod set {
    pub const TAGS: &str = "tags";
    pub const NOTES: &str = "notes";
    pub const PHOTOS: &str = "photos";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Open,
    InProgress,
    Paused,
    Done,
    Cancelled,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Open => "open",
            Status::InProgress => "in_progress",
            Status::Paused => "paused",
            Status::Done => "done",
            Status::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "open" => Status::Open,
            "in_progress" => Status::InProgress,
            "paused" => Status::Paused,
            "done" => Status::Done,
            "cancelled" => Status::Cancelled,
            _ => return None,
        })
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Status::Done | Status::Cancelled)
    }
}

/// A note is an immutable OR-set element. Editing a note is remove + add,
/// which keeps concurrent edits from silently overwriting each other: both
/// versions survive and the crew sees the conflict.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Note {
    pub author: String,
    pub at_ms: i64,
    pub text: String,
}

/// Metadata only. Binaries travel through a separate blob channel keyed by
/// content hash, so the op log stays small enough to sync over poor links.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhotoMeta {
    pub sha256: String,
    pub taken_ms: i64,
    pub width: u32,
    pub height: u32,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub caption: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Site {
    pub id: String,
    pub name: String,
    pub shape: Option<Shape>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkOrder {
    pub id: String,
    pub title: String,
    pub site_id: String,
    pub crew: String,
    pub priority: u8,
    pub status: Status,
    pub checked_in_by: Option<String>,
    pub checked_in_ms: Option<i64>,
    pub checked_out_ms: Option<i64>,
    pub notes: Vec<Note>,
    pub photos: Vec<PhotoMeta>,
}

/// Encodes an element for an OR-set. The JSON encoder writes struct fields in
/// declaration order is deterministic, so equal values give equal strings.
pub fn encode<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).expect("domain values always serialize")
}

pub fn site_view(state: &ReplicaState, id: &str) -> Option<Site> {
    let e = EntityRef::new(SITE, id);
    let name = state.get(&e, field::NAME)?.to_string();
    Some(Site {
        id: id.to_string(),
        name,
        shape: state
            .get(&e, field::SHAPE)
            .and_then(|s| serde_json::from_str(s).ok()),
        tags: state.elements(&e, set::TAGS),
    })
}

pub fn work_order_view(state: &ReplicaState, id: &str) -> Option<WorkOrder> {
    let e = EntityRef::new(WORK_ORDER, id);
    let get = |f: &str| state.get(&e, f).map(str::to_string);
    let title = get(field::TITLE)?;
    let mut notes: Vec<Note> = state
        .elements(&e, set::NOTES)
        .iter()
        .filter_map(|s| serde_json::from_str(s).ok())
        .collect();
    notes.sort_by(|a, b| (a.at_ms, &a.author).cmp(&(b.at_ms, &b.author)));
    let mut photos: Vec<PhotoMeta> = state
        .elements(&e, set::PHOTOS)
        .iter()
        .filter_map(|s| serde_json::from_str(s).ok())
        .collect();
    photos.sort_by_key(|p| p.taken_ms);
    Some(WorkOrder {
        id: id.to_string(),
        title,
        site_id: get(field::SITE_ID).unwrap_or_default(),
        crew: get(field::CREW).unwrap_or_default(),
        priority: get(field::PRIORITY)
            .and_then(|p| p.parse().ok())
            .unwrap_or(3),
        status: get(field::STATUS)
            .as_deref()
            .and_then(Status::parse)
            .unwrap_or(Status::Open),
        checked_in_by: get(field::CHECKED_IN_BY).filter(|s| !s.is_empty()),
        checked_in_ms: get(field::CHECKED_IN_MS).and_then(|s| s.parse().ok()),
        checked_out_ms: get(field::CHECKED_OUT_MS).and_then(|s| s.parse().ok()),
        notes,
        photos,
    })
}

pub fn all_sites(state: &ReplicaState) -> Vec<Site> {
    state
        .entity_ids(SITE)
        .iter()
        .filter_map(|id| site_view(state, id))
        .collect()
}

/// Work orders sorted for a crew's list screen: active first, then by
/// priority (1 = most urgent), then id for stability.
pub fn all_work_orders(state: &ReplicaState) -> Vec<WorkOrder> {
    let mut v: Vec<WorkOrder> = state
        .entity_ids(WORK_ORDER)
        .iter()
        .filter_map(|id| work_order_view(state, id))
        .collect();
    v.sort_by(|a, b| {
        (a.status.is_terminal(), a.priority, &a.id).cmp(&(
            b.status.is_terminal(),
            b.priority,
            &b.id,
        ))
    });
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_round_trip() {
        for s in [
            Status::Open,
            Status::InProgress,
            Status::Paused,
            Status::Done,
            Status::Cancelled,
        ] {
            assert_eq!(Status::parse(s.as_str()), Some(s));
        }
        assert_eq!(Status::parse("bogus"), None);
    }

    #[test]
    fn note_encoding_is_deterministic() {
        let n = Note {
            author: "dev-a".into(),
            at_ms: 5,
            text: "hi".into(),
        };
        assert_eq!(encode(&n), encode(&n.clone()));
        assert_eq!(encode(&n), r#"{"author":"dev-a","at_ms":5,"text":"hi"}"#);
    }
}
