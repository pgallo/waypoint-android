//! The engine ties the pieces together behind one API that the UI layer calls.
//!
//! Every user action becomes one or more ops: stamped by the HLC, written to
//! the local database together with an outbox entry, then applied to in-memory state. The
//! read side (`work_orders`, `check_in_state`) is a pure function of that state
//! plus the live geofence presence.

use crate::crdt::{EntityRef, Op, OpKind, ReplicaState};
use crate::domain::{self, field, set, Note, PhotoMeta, Site, Status, WorkOrder};
use crate::geofence::{GeofenceEvent, GeofenceMonitor, HysteresisConfig, Presence, Shape};
use crate::hlc::{Hlc, HlcClock};
use crate::store::{Store, StoreError};
use crate::sync::{SyncReport, SyncTransport};
use crate::track::{Fix, RecorderConfig, TrackRecorder, TrackStats};
use std::path::Path;

/// Source of wall-clock milliseconds. Injected so tests and the demo can run
/// on simulated time.
pub type Clock = Box<dyn Fn() -> i64 + Send>;

pub fn system_clock() -> Clock {
    Box::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    })
}

/// Simplification tolerance for stored tracks. Consumer GPS is good to a few
/// metres, so detail below 5 m is mostly noise.
pub const TRACK_EPSILON_M: f64 = 5.0;
const SYNC_BATCH: usize = 256;
const META_CURSOR: &str = "relay_cursor";
const META_HLC: &str = "hlc";

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("check-in refused: {0:?}")]
    CheckInRefused(CheckInState),
}

pub type EngineResult<T> = Result<T, EngineError>;

/// Why the check-in button is enabled or not. The UI renders this directly.
#[derive(Debug, Clone, PartialEq)]
pub enum CheckInState {
    Allowed,
    NoLocationYet,
    OutsideFence,
    SiteHasNoFence,
    AlreadyCheckedIn { by: String },
    Closed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackSummary {
    pub track_id: i64,
    pub raw: TrackStats,
    pub simplified_points: usize,
}

pub struct Engine {
    node: String,
    store: Store,
    state: ReplicaState,
    clock: HlcClock,
    now: Clock,
    fences: GeofenceMonitor,
    recorder: Option<(Option<String>, TrackRecorder)>,
    last_fix: Option<Fix>,
}

impl Engine {
    pub fn open(path: impl AsRef<Path>, node: &str, now: Clock) -> EngineResult<Self> {
        Self::with_store(Store::open(path)?, node, now)
    }

    pub fn in_memory(node: &str, now: Clock) -> EngineResult<Self> {
        Self::with_store(Store::open_in_memory()?, node, now)
    }

    fn with_store(store: Store, node: &str, now: Clock) -> EngineResult<Self> {
        if node.is_empty() || node.contains(char::is_whitespace) {
            return Err(EngineError::Invalid(format!("bad node id {node:?}")));
        }
        let ops = store.load_ops()?;
        let mut state = ReplicaState::new();
        for op in &ops {
            state.apply(op);
        }
        // Resume the clock from whichever is later: our persisted stamp or
        // anything we have seen from peers.
        let persisted = store
            .meta_get(META_HLC)?
            .and_then(|s| s.parse::<Hlc>().ok());
        let seen = ops.iter().map(|o| &o.id).max();
        let resume = match (persisted.as_ref(), seen) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        let mut engine = Self {
            node: node.to_string(),
            clock: HlcClock::restore(node, resume),
            store,
            state,
            now,
            fences: GeofenceMonitor::new(HysteresisConfig::default()),
            recorder: None,
            last_fix: None,
        };
        engine.refresh_fences();
        Ok(engine)
    }

    pub fn node(&self) -> &str {
        &self.node
    }

    pub fn state(&self) -> &ReplicaState {
        &self.state
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    // ----- writes -------------------------------------------------------

    fn emit(&mut self, entity: EntityRef, kind: OpKind) -> EngineResult<Op> {
        let now = (self.now)();
        let op = Op {
            id: self.clock.tick(now),
            entity,
            kind,
        };
        self.store.record_local(&op, now)?;
        self.state.apply(&op);
        Ok(op)
    }

    fn set_field(
        &mut self,
        entity: &EntityRef,
        f: &str,
        value: impl Into<String>,
    ) -> EngineResult<()> {
        self.emit(
            entity.clone(),
            OpKind::Set {
                field: f.to_string(),
                value: value.into(),
            },
        )?;
        Ok(())
    }

    pub fn upsert_site(&mut self, id: &str, name: &str, shape: Option<Shape>) -> EngineResult<()> {
        if let Some(s) = &shape {
            if !s.is_valid() {
                return Err(EngineError::Invalid(format!("site {id}: invalid geofence")));
            }
        }
        let e = EntityRef::new(domain::SITE, id);
        self.set_field(&e, field::NAME, name)?;
        if let Some(s) = shape {
            self.set_field(&e, field::SHAPE, domain::encode(&s))?;
        }
        self.refresh_fences();
        Ok(())
    }

    pub fn create_work_order(
        &mut self,
        id: &str,
        title: &str,
        site_id: &str,
        crew: &str,
        priority: u8,
    ) -> EngineResult<()> {
        if domain::site_view(&self.state, site_id).is_none() {
            return Err(EngineError::NotFound(format!("site {site_id}")));
        }
        if !(1..=5).contains(&priority) {
            return Err(EngineError::Invalid("priority must be 1..=5".into()));
        }
        let e = EntityRef::new(domain::WORK_ORDER, id);
        self.set_field(&e, field::TITLE, title)?;
        self.set_field(&e, field::SITE_ID, site_id)?;
        self.set_field(&e, field::CREW, crew)?;
        self.set_field(&e, field::PRIORITY, priority.to_string())?;
        self.set_field(&e, field::STATUS, Status::Open.as_str())?;
        Ok(())
    }

    fn require_work_order(&self, id: &str) -> EngineResult<WorkOrder> {
        domain::work_order_view(&self.state, id)
            .ok_or_else(|| EngineError::NotFound(format!("work order {id}")))
    }

    pub fn set_status(&mut self, wo_id: &str, status: Status) -> EngineResult<()> {
        self.require_work_order(wo_id)?;
        let e = EntityRef::new(domain::WORK_ORDER, wo_id);
        self.set_field(&e, field::STATUS, status.as_str())
    }

    pub fn set_title(&mut self, wo_id: &str, title: &str) -> EngineResult<()> {
        self.require_work_order(wo_id)?;
        let e = EntityRef::new(domain::WORK_ORDER, wo_id);
        self.set_field(&e, field::TITLE, title)
    }

    pub fn add_note(&mut self, wo_id: &str, text: &str) -> EngineResult<Note> {
        self.require_work_order(wo_id)?;
        if text.trim().is_empty() {
            return Err(EngineError::Invalid("empty note".into()));
        }
        let note = Note {
            author: self.node.clone(),
            at_ms: (self.now)(),
            text: text.trim().to_string(),
        };
        self.add_element(domain::WORK_ORDER, wo_id, set::NOTES, domain::encode(&note))?;
        Ok(note)
    }

    pub fn remove_note(&mut self, wo_id: &str, note: &Note) -> EngineResult<()> {
        self.remove_element(domain::WORK_ORDER, wo_id, set::NOTES, &domain::encode(note))
    }

    pub fn attach_photo(&mut self, wo_id: &str, photo: PhotoMeta) -> EngineResult<()> {
        self.require_work_order(wo_id)?;
        let hex = photo.sha256.len() == 64 && photo.sha256.bytes().all(|b| b.is_ascii_hexdigit());
        if !hex {
            return Err(EngineError::Invalid("sha256 must be 64 hex chars".into()));
        }
        self.add_element(
            domain::WORK_ORDER,
            wo_id,
            set::PHOTOS,
            domain::encode(&photo),
        )
    }

    pub fn add_site_tag(&mut self, site_id: &str, tag: &str) -> EngineResult<()> {
        self.add_element(domain::SITE, site_id, set::TAGS, tag.to_string())
    }

    pub fn remove_site_tag(&mut self, site_id: &str, tag: &str) -> EngineResult<()> {
        self.remove_element(domain::SITE, site_id, set::TAGS, tag)
    }

    fn add_element(&mut self, kind: &str, id: &str, s: &str, element: String) -> EngineResult<()> {
        self.emit(
            EntityRef::new(kind, id),
            OpKind::Add {
                set: s.to_string(),
                element,
            },
        )?;
        Ok(())
    }

    fn remove_element(&mut self, kind: &str, id: &str, s: &str, element: &str) -> EngineResult<()> {
        let e = EntityRef::new(kind, id);
        let observed = self.state.live_tags(&e, s, element);
        if observed.is_empty() {
            return Err(EngineError::NotFound(format!("{s} element in {kind} {id}")));
        }
        self.emit(
            e,
            OpKind::Remove {
                set: s.to_string(),
                element: element.to_string(),
                observed,
            },
        )?;
        Ok(())
    }

    // ----- check-in / check-out ----------------------------------------

    pub fn check_in_state(&self, wo_id: &str) -> EngineResult<CheckInState> {
        let wo = self.require_work_order(wo_id)?;
        if wo.status.is_terminal() {
            return Ok(CheckInState::Closed);
        }
        if let (Some(by), None) = (&wo.checked_in_by, wo.checked_out_ms) {
            return Ok(CheckInState::AlreadyCheckedIn { by: by.clone() });
        }
        let has_fence =
            domain::site_view(&self.state, &wo.site_id).is_some_and(|s| s.shape.is_some());
        if !has_fence {
            return Ok(CheckInState::SiteHasNoFence);
        }
        Ok(match self.fences.presence(&wo.site_id) {
            Presence::Inside => CheckInState::Allowed,
            Presence::Outside => CheckInState::OutsideFence,
            Presence::Unknown => CheckInState::NoLocationYet,
        })
    }

    /// Check-in is validated locally against the geofence. The server cannot
    /// re-validate offline edits, so the check-in fix position is not trusted
    /// beyond this device; the recorded track is the audit trail.
    pub fn check_in(&mut self, wo_id: &str) -> EngineResult<()> {
        match self.check_in_state(wo_id)? {
            CheckInState::Allowed => {}
            other => return Err(EngineError::CheckInRefused(other)),
        }
        let now = (self.now)();
        let e = EntityRef::new(domain::WORK_ORDER, wo_id);
        self.set_field(&e, field::CHECKED_IN_BY, self.node.clone())?;
        self.set_field(&e, field::CHECKED_IN_MS, now.to_string())?;
        self.set_field(&e, field::CHECKED_OUT_MS, "")?;
        self.set_field(&e, field::STATUS, Status::InProgress.as_str())?;
        Ok(())
    }

    /// Check-out is allowed anywhere (people leave sites in a hurry); it ends
    /// the visit and moves the order to `done` or `paused`.
    pub fn check_out(&mut self, wo_id: &str, completed: bool) -> EngineResult<()> {
        let wo = self.require_work_order(wo_id)?;
        if wo.checked_in_by.is_none() || wo.checked_out_ms.is_some() {
            return Err(EngineError::Invalid(format!("{wo_id} is not checked in")));
        }
        let now = (self.now)();
        let e = EntityRef::new(domain::WORK_ORDER, wo_id);
        self.set_field(&e, field::CHECKED_OUT_MS, now.to_string())?;
        let status = if completed {
            Status::Done
        } else {
            Status::Paused
        };
        self.set_field(&e, field::STATUS, status.as_str())?;
        Ok(())
    }

    // ----- location -----------------------------------------------------

    fn refresh_fences(&mut self) {
        for site in domain::all_sites(&self.state) {
            if let Some(shape) = site.shape {
                self.fences.upsert(&site.id, shape);
            }
        }
    }

    pub fn presence(&self, site_id: &str) -> Presence {
        self.fences.presence(site_id)
    }

    pub fn last_fix(&self) -> Option<Fix> {
        self.last_fix
    }

    /// Feed one platform location fix. Updates geofences (logging events to
    /// the local DB) and, if a recording is active, the track.
    pub fn on_location(&mut self, fix: Fix) -> EngineResult<Vec<GeofenceEvent>> {
        if !fix.pos.is_valid() {
            return Err(EngineError::Invalid("fix outside WGS84 range".into()));
        }
        self.last_fix = Some(fix);
        if let Some((_, rec)) = self.recorder.as_mut() {
            rec.push(fix);
        }
        let events = self.fences.update(&fix);
        for ev in &events {
            self.store.log_fence_event(ev)?;
        }
        Ok(events)
    }

    pub fn start_track(&mut self, wo_id: Option<&str>) -> EngineResult<()> {
        if self.recorder.is_some() {
            return Err(EngineError::Invalid("a track is already recording".into()));
        }
        self.recorder = Some((
            wo_id.map(str::to_string),
            TrackRecorder::new(RecorderConfig::default()),
        ));
        Ok(())
    }

    pub fn is_recording(&self) -> bool {
        self.recorder.is_some()
    }

    pub fn stop_track(&mut self) -> EngineResult<TrackSummary> {
        let (wo, rec) = self
            .recorder
            .take()
            .ok_or_else(|| EngineError::Invalid("no track recording".into()))?;
        let raw = rec.stats();
        let simplified = rec.simplified(TRACK_EPSILON_M);
        let id = self.store.save_track(wo.as_deref(), &simplified, &raw)?;
        Ok(TrackSummary {
            track_id: id,
            raw,
            simplified_points: simplified.len(),
        })
    }

    // ----- reads ----------------------------------------------------------

    pub fn work_orders(&self) -> Vec<WorkOrder> {
        domain::all_work_orders(&self.state)
    }

    pub fn work_order(&self, id: &str) -> Option<WorkOrder> {
        domain::work_order_view(&self.state, id)
    }

    pub fn sites(&self) -> Vec<Site> {
        domain::all_sites(&self.state)
    }

    pub fn pending_ops(&self) -> EngineResult<usize> {
        Ok(self.store.outbox_len()?)
    }

    pub fn fingerprint(&self) -> u64 {
        self.state.fingerprint()
    }

    // ----- sync -----------------------------------------------------------

    /// One sync round: drain the outbox, then pull everything new.
    ///
    /// Push failures do not abort the pull when the transport can still read
    /// (a relay may reject writes while allowing reads); transport errors are
    /// reported in the result rather than as `Err`, because "offline" is a
    /// normal state for this app, not an exceptional one.
    pub fn sync(&mut self, transport: &mut dyn SyncTransport) -> EngineResult<SyncReport> {
        let mut report = SyncReport::default();
        loop {
            let batch = self.store.outbox_batch(SYNC_BATCH)?;
            if batch.is_empty() {
                break;
            }
            let ops: Vec<Op> = batch.into_iter().map(|e| e.op).collect();
            match transport.push(&ops) {
                Ok(acked) => {
                    report.pushed += self.store.outbox_ack(&acked)?;
                    if acked.len() < ops.len() {
                        break; // partial ack: retry the rest next round
                    }
                }
                Err(err) => {
                    let ids: Vec<Hlc> = ops.iter().map(|o| o.id.clone()).collect();
                    self.store.outbox_fail(&ids, &err.to_string())?;
                    report.error = Some(err.to_string());
                    break;
                }
            }
        }

        let mut cursor: u64 = self
            .store
            .meta_get(META_CURSOR)?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        loop {
            match transport.pull(cursor, SYNC_BATCH) {
                Ok((ops, next)) => {
                    report.pulled += ops.len();
                    let fresh = self.store.ingest_remote(&ops)?;
                    let now = (self.now)();
                    for op in &fresh {
                        self.clock.observe(&op.id, now);
                        self.state.apply(op);
                    }
                    report.applied += fresh.len();
                    cursor = next;
                    self.store.meta_set(META_CURSOR, &cursor.to_string())?;
                    if ops.is_empty() {
                        break;
                    }
                }
                Err(err) => {
                    report.error.get_or_insert(err.to_string());
                    break;
                }
            }
        }
        self.store
            .meta_set(META_HLC, &self.clock.last().to_string())?;
        if report.applied > 0 {
            self.refresh_fences();
        }
        report.pending = self.store.outbox_len()?;
        Ok(report)
    }
}
