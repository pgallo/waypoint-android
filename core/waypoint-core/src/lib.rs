//! Waypoint core: offline-first domain logic for mobile field crews.
//!
//! Layering, bottom up: `geo` (math) -> `track`, `geofence` (sensors) ->
//! `hlc`, `crdt` (replication) -> `store` (embedded SQL) -> `domain` (projections)
//! -> `engine` (the API a UI calls). `sync` defines the transport seam.

pub mod crdt;
pub mod domain;
pub mod engine;
pub mod fixtures;
pub mod geo;
pub mod geofence;
pub mod hlc;
pub mod store;
pub mod sync;
pub mod track;

pub use crdt::{EntityRef, Op, OpKind, ReplicaState};
pub use domain::{Note, PhotoMeta, Site, Status, WorkOrder};
pub use engine::{CheckInState, Engine, EngineError, TrackSummary};
pub use geo::LatLon;
pub use geofence::{GeofenceEvent, Presence, Shape, Transition};
pub use hlc::Hlc;
pub use sync::{InMemoryRelay, OfflineTransport, SqliteRelay, SyncReport, SyncTransport};
pub use track::Fix;
