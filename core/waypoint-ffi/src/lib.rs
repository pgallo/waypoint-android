//! Foreign-function boundary. Everything here is a thin, copy-in/copy-out adapter: FFI
//! records are flat and owned, the engine sits behind a mutex, and no domain
//! rule lives in this crate.

use std::sync::{Arc, Mutex, MutexGuard};
use waypoint_core::engine::{self, CheckInState as CoreCheckIn, Engine};
use waypoint_core::{domain, fixtures, geofence, LatLon, SqliteRelay};

uniffi::setup_scaffolding!();

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum WaypointError {
    #[error("not found: {detail}")]
    NotFound { detail: String },
    #[error("invalid input: {detail}")]
    Invalid { detail: String },
    #[error("check-in refused: {reason:?}")]
    CheckInRefused { reason: CheckInState },
    #[error("storage: {detail}")]
    Storage { detail: String },
}

impl From<engine::EngineError> for WaypointError {
    fn from(e: engine::EngineError) -> Self {
        use engine::EngineError as E;
        match e {
            E::NotFound(detail) => Self::NotFound { detail },
            E::Invalid(detail) => Self::Invalid { detail },
            E::CheckInRefused(s) => Self::CheckInRefused { reason: s.into() },
            E::Store(s) => Self::Storage {
                detail: s.to_string(),
            },
        }
    }
}

type Result<T> = std::result::Result<T, WaypointError>;

#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum CheckInState {
    Allowed,
    NoLocationYet,
    OutsideFence,
    SiteHasNoFence,
    AlreadyCheckedIn { by: String },
    Closed,
}

impl From<CoreCheckIn> for CheckInState {
    fn from(s: CoreCheckIn) -> Self {
        match s {
            CoreCheckIn::Allowed => Self::Allowed,
            CoreCheckIn::NoLocationYet => Self::NoLocationYet,
            CoreCheckIn::OutsideFence => Self::OutsideFence,
            CoreCheckIn::SiteHasNoFence => Self::SiteHasNoFence,
            CoreCheckIn::AlreadyCheckedIn { by } => Self::AlreadyCheckedIn { by },
            CoreCheckIn::Closed => Self::Closed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum WorkStatus {
    Open,
    InProgress,
    Paused,
    Done,
    Cancelled,
}

impl From<domain::Status> for WorkStatus {
    fn from(s: domain::Status) -> Self {
        match s {
            domain::Status::Open => Self::Open,
            domain::Status::InProgress => Self::InProgress,
            domain::Status::Paused => Self::Paused,
            domain::Status::Done => Self::Done,
            domain::Status::Cancelled => Self::Cancelled,
        }
    }
}

impl From<WorkStatus> for domain::Status {
    fn from(s: WorkStatus) -> Self {
        match s {
            WorkStatus::Open => Self::Open,
            WorkStatus::InProgress => Self::InProgress,
            WorkStatus::Paused => Self::Paused,
            WorkStatus::Done => Self::Done,
            WorkStatus::Cancelled => Self::Cancelled,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FencePresence {
    Unknown,
    Inside,
    Outside,
}

impl From<geofence::Presence> for FencePresence {
    fn from(p: geofence::Presence) -> Self {
        match p {
            geofence::Presence::Unknown => Self::Unknown,
            geofence::Presence::Inside => Self::Inside,
            geofence::Presence::Outside => Self::Outside,
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct LocationFix {
    pub lat: f64,
    pub lon: f64,
    pub timestamp_ms: i64,
    pub accuracy_m: f64,
}

impl From<&LocationFix> for waypoint_core::Fix {
    fn from(f: &LocationFix) -> Self {
        Self {
            pos: LatLon::new(f.lat, f.lon),
            timestamp_ms: f.timestamp_ms,
            accuracy_m: f.accuracy_m,
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct FenceEvent {
    pub site_id: String,
    pub entered: bool,
    pub timestamp_ms: i64,
    pub distance_m: f64,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct NoteView {
    pub author: String,
    pub at_ms: i64,
    pub text: String,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct PhotoView {
    pub sha256: String,
    pub taken_ms: i64,
    pub width: u32,
    pub height: u32,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub caption: String,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct WorkOrderView {
    pub id: String,
    pub title: String,
    pub site_id: String,
    pub site_name: String,
    pub crew: String,
    pub priority: u8,
    pub status: WorkStatus,
    pub checked_in_by: Option<String>,
    pub checked_in_ms: Option<i64>,
    pub checked_out_ms: Option<i64>,
    pub notes: Vec<NoteView>,
    pub photos: Vec<PhotoView>,
    pub presence: FencePresence,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct TrackSummaryView {
    pub track_id: i64,
    pub raw_points: u32,
    pub kept_points: u32,
    pub simplified_points: u32,
    pub distance_m: f64,
    pub duration_s: f64,
    pub moving_s: f64,
    pub avg_speed_mps: f64,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct SyncStatus {
    pub pushed: u32,
    pub pulled: u32,
    pub applied: u32,
    pub pending: u32,
    pub error: Option<String>,
    pub fingerprint: String,
}

/// On-device relay shared between engines. On a phone this stands in for
/// the network relay; two engines on one relay let the UI show a merge
/// without any server.
#[derive(uniffi::Object)]
pub struct LocalRelay {
    inner: Mutex<SqliteRelay>,
}

#[uniffi::export]
impl LocalRelay {
    /// Relay persisted at `db_path`.
    #[uniffi::constructor]
    pub fn open(db_path: String) -> Result<Arc<Self>> {
        let r = SqliteRelay::open(db_path).map_err(|e| WaypointError::Storage {
            detail: e.to_string(),
        })?;
        Ok(Arc::new(Self {
            inner: Mutex::new(r),
        }))
    }

    #[uniffi::constructor]
    pub fn in_memory() -> Result<Arc<Self>> {
        let r = SqliteRelay::open_in_memory().map_err(|e| WaypointError::Storage {
            detail: e.to_string(),
        })?;
        Ok(Arc::new(Self {
            inner: Mutex::new(r),
        }))
    }

    pub fn op_count(&self) -> u64 {
        lock(&self.inner).len() as u64
    }
}

#[derive(uniffi::Object)]
pub struct WaypointEngine {
    inner: Mutex<Engine>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding the lock leaves state that was valid before the
    // panicking call (writes are transactional), so recovering is safe.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl WaypointEngine {
    fn engine(&self) -> MutexGuard<'_, Engine> {
        lock(&self.inner)
    }
}

#[uniffi::export]
impl WaypointEngine {
    /// Opens (or creates) the database at `db_path` for device `node_id`.
    #[uniffi::constructor]
    pub fn open(db_path: String, node_id: String) -> Result<Arc<Self>> {
        let e = Engine::open(db_path, &node_id, engine::system_clock())?;
        Ok(Arc::new(Self {
            inner: Mutex::new(e),
        }))
    }

    #[uniffi::constructor]
    pub fn in_memory(node_id: String) -> Result<Arc<Self>> {
        let e = Engine::in_memory(&node_id, engine::system_clock())?;
        Ok(Arc::new(Self {
            inner: Mutex::new(e),
        }))
    }

    pub fn node_id(&self) -> String {
        self.engine().node().to_string()
    }

    /// Loads the synthetic demo sites and work orders if the store is empty.
    pub fn seed_demo_data(&self) -> Result<bool> {
        let mut e = self.engine();
        if !e.work_orders().is_empty() {
            return Ok(false);
        }
        fixtures::seed(&mut e)?;
        Ok(true)
    }

    pub fn work_orders(&self) -> Vec<WorkOrderView> {
        let e = self.engine();
        e.work_orders().into_iter().map(|w| view(&e, w)).collect()
    }

    pub fn work_order(&self, id: String) -> Option<WorkOrderView> {
        let e = self.engine();
        e.work_order(&id).map(|w| view(&e, w))
    }

    pub fn check_in_state(&self, work_order_id: String) -> Result<CheckInState> {
        Ok(self.engine().check_in_state(&work_order_id)?.into())
    }

    pub fn check_in(&self, work_order_id: String) -> Result<()> {
        Ok(self.engine().check_in(&work_order_id)?)
    }

    pub fn check_out(&self, work_order_id: String, completed: bool) -> Result<()> {
        Ok(self.engine().check_out(&work_order_id, completed)?)
    }

    pub fn set_status(&self, work_order_id: String, status: WorkStatus) -> Result<()> {
        Ok(self.engine().set_status(&work_order_id, status.into())?)
    }

    pub fn set_title(&self, work_order_id: String, title: String) -> Result<()> {
        Ok(self.engine().set_title(&work_order_id, &title)?)
    }

    pub fn add_note(&self, work_order_id: String, text: String) -> Result<NoteView> {
        let n = self.engine().add_note(&work_order_id, &text)?;
        Ok(NoteView {
            author: n.author,
            at_ms: n.at_ms,
            text: n.text,
        })
    }

    pub fn attach_photo(&self, work_order_id: String, photo: PhotoView) -> Result<()> {
        let meta = domain::PhotoMeta {
            sha256: photo.sha256,
            taken_ms: photo.taken_ms,
            width: photo.width,
            height: photo.height,
            lat: photo.lat,
            lon: photo.lon,
            caption: photo.caption,
        };
        Ok(self.engine().attach_photo(&work_order_id, meta)?)
    }

    pub fn on_location(&self, fix: LocationFix) -> Result<Vec<FenceEvent>> {
        let events = self.engine().on_location((&fix).into())?;
        Ok(events
            .into_iter()
            .map(|ev| FenceEvent {
                site_id: ev.fence_id,
                entered: ev.transition == geofence::Transition::Enter,
                timestamp_ms: ev.timestamp_ms,
                distance_m: ev.distance_m,
            })
            .collect())
    }

    pub fn start_track(&self, work_order_id: Option<String>) -> Result<()> {
        Ok(self.engine().start_track(work_order_id.as_deref())?)
    }

    pub fn is_recording(&self) -> bool {
        self.engine().is_recording()
    }

    pub fn stop_track(&self) -> Result<TrackSummaryView> {
        let s = self.engine().stop_track()?;
        Ok(TrackSummaryView {
            track_id: s.track_id,
            raw_points: s.raw.points_raw as u32,
            kept_points: s.raw.points_kept as u32,
            simplified_points: s.simplified_points as u32,
            distance_m: s.raw.distance_m,
            duration_s: s.raw.duration_s,
            moving_s: s.raw.moving_s,
            avg_speed_mps: s.raw.avg_speed_mps,
        })
    }

    pub fn pending_ops(&self) -> Result<u64> {
        Ok(self.engine().pending_ops()? as u64)
    }

    pub fn sync_with(&self, relay: Arc<LocalRelay>) -> Result<SyncStatus> {
        let mut e = self.engine();
        let mut r = lock(&relay.inner);
        let rep = e.sync(&mut *r)?;
        Ok(SyncStatus {
            pushed: rep.pushed as u32,
            pulled: rep.pulled as u32,
            applied: rep.applied as u32,
            pending: rep.pending as u32,
            error: rep.error,
            fingerprint: format!("{:016x}", e.fingerprint()),
        })
    }

    /// Simulates a failed sync attempt (no network): ops stay in the outbox
    /// with an incremented attempt count.
    pub fn sync_offline(&self) -> Result<SyncStatus> {
        let mut e = self.engine();
        let rep = e.sync(&mut waypoint_core::OfflineTransport)?;
        Ok(SyncStatus {
            pushed: rep.pushed as u32,
            pulled: rep.pulled as u32,
            applied: rep.applied as u32,
            pending: rep.pending as u32,
            error: rep.error,
            fingerprint: format!("{:016x}", e.fingerprint()),
        })
    }
}

/// Synthetic walk from the depot to the demo pump station and away again,
/// for driving the geofence on an emulator without a GPS route file.
#[uniffi::export]
pub fn demo_walk_fixes(start_ms: i64) -> Vec<LocationFix> {
    fixtures::walk_to_pump_station(start_ms)
        .into_iter()
        .map(|f| LocationFix {
            lat: f.pos.lat,
            lon: f.pos.lon,
            timestamp_ms: f.timestamp_ms,
            accuracy_m: f.accuracy_m,
        })
        .collect()
}

#[uniffi::export]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

fn view(e: &Engine, w: domain::WorkOrder) -> WorkOrderView {
    let site_name = domain::site_view(e.state(), &w.site_id)
        .map(|s| s.name)
        .unwrap_or_else(|| w.site_id.clone());
    WorkOrderView {
        presence: e.presence(&w.site_id).into(),
        site_name,
        id: w.id,
        title: w.title,
        site_id: w.site_id,
        crew: w.crew,
        priority: w.priority,
        status: w.status.into(),
        checked_in_by: w.checked_in_by,
        checked_in_ms: w.checked_in_ms,
        checked_out_ms: w.checked_out_ms,
        notes: w
            .notes
            .into_iter()
            .map(|n| NoteView {
                author: n.author,
                at_ms: n.at_ms,
                text: n.text,
            })
            .collect(),
        photos: w
            .photos
            .into_iter()
            .map(|p| PhotoView {
                sha256: p.sha256,
                taken_ms: p.taken_ms,
                width: p.width,
                height: p.height,
                lat: p.lat,
                lon: p.lon,
                caption: p.caption,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_engines_converge_through_local_relay() {
        let a = WaypointEngine::in_memory("tablet-a".into()).unwrap();
        let b = WaypointEngine::in_memory("tablet-b".into()).unwrap();
        assert!(a.seed_demo_data().unwrap());
        assert!(!a.seed_demo_data().unwrap());
        let relay = LocalRelay::in_memory().unwrap();
        a.sync_with(relay.clone()).unwrap();
        let sb = b.sync_with(relay.clone()).unwrap();
        assert_eq!(b.work_orders().len(), 3);
        a.add_note("wo-1001".into(), "valve seized".into()).unwrap();
        b.set_title("wo-1001".into(), "Replace sensor + valve".into())
            .unwrap();
        a.sync_with(relay.clone()).unwrap();
        b.sync_with(relay.clone()).unwrap();
        let sa = a.sync_with(relay.clone()).unwrap();
        let sb2 = b.sync_with(relay.clone()).unwrap();
        assert_ne!(sb.fingerprint, sb2.fingerprint);
        assert_eq!(sa.fingerprint, sb2.fingerprint);
        let wo = a.work_order("wo-1001".into()).unwrap();
        assert_eq!(wo.title, "Replace sensor + valve");
        assert_eq!(wo.notes.len(), 1);
    }

    #[test]
    fn check_in_requires_presence() {
        let e = WaypointEngine::in_memory("t".into()).unwrap();
        e.seed_demo_data().unwrap();
        assert_eq!(
            e.check_in_state("wo-1001".into()).unwrap(),
            CheckInState::NoLocationYet
        );
        let err = e.check_in("wo-1001".into()).unwrap_err();
        assert!(matches!(err, WaypointError::CheckInRefused { .. }));
        for (i, f) in demo_walk_fixes(1_000).into_iter().enumerate() {
            e.on_location(f).unwrap();
            if e.check_in_state("wo-1001".into()).unwrap() == CheckInState::Allowed {
                e.check_in("wo-1001".into()).unwrap();
                assert!(i > 100);
                break;
            }
        }
        let wo = e.work_order("wo-1001".into()).unwrap();
        assert_eq!(wo.status, WorkStatus::InProgress);
        assert_eq!(wo.presence, FencePresence::Inside);
        assert!(e.pending_ops().unwrap() > 0);
        let off = e.sync_offline().unwrap();
        assert!(off.error.is_some());
    }
}
