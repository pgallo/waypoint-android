//! End-to-end: local outbox -> relay -> local database on another device, including
//! offline retries, process restarts and a skewed device clock.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use waypoint_core::engine::{Clock, Engine};
use waypoint_core::{fixtures, CheckInState, InMemoryRelay, OfflineTransport, Presence, Status};

const T0: i64 = 1_790_000_000_000;

fn clock(t: &Arc<AtomicI64>, skew: i64) -> Clock {
    let t = t.clone();
    Box::new(move || t.load(Ordering::SeqCst) + skew)
}

#[test]
fn outbox_survives_offline_and_restart_then_converges() {
    let dir = tempfile::tempdir().unwrap();
    let path_a = dir.path().join("a.db");
    let path_b = dir.path().join("b.db");
    let now = Arc::new(AtomicI64::new(T0));
    let mut relay = InMemoryRelay::new();

    {
        let mut a = Engine::open(&path_a, "dev-a", clock(&now, 0)).unwrap();
        fixtures::seed(&mut a).unwrap();
        let queued = a.pending_ops().unwrap();
        assert!(queued > 10);
        // Out of coverage: nothing leaves, nothing is lost.
        let r = a.sync(&mut OfflineTransport).unwrap();
        assert_eq!(r.pushed, 0);
        assert_eq!(r.pending, queued);
        assert!(r.error.is_some());
        let batch = a.store().outbox_batch(1).unwrap();
        assert_eq!(batch[0].attempts, 1);
    }

    // Process restart: state and outbox are rebuilt from the database.
    now.fetch_add(10_000, Ordering::SeqCst);
    let mut a = Engine::open(&path_a, "dev-a", clock(&now, 0)).unwrap();
    assert_eq!(a.work_orders().len(), 3);
    let r = a.sync(&mut relay).unwrap();
    assert_eq!(r.pending, 0);
    assert_eq!(r.pushed, relay.len());
    // Own ops come back on pull but are already stored, so nothing re-applies.
    assert_eq!(r.applied, 0);

    // Device b runs five minutes slow.
    let mut b = Engine::open(&path_b, "dev-b", clock(&now, -300_000)).unwrap();
    b.sync(&mut relay).unwrap();
    assert_eq!(a.fingerprint(), b.fingerprint());

    // b edits after seeing a's ops; its stamps must still sort after them
    // despite the slow clock (HLC observe on pull).
    b.set_status("wo-1002", Status::Paused).unwrap();
    let (stamp, _) = b
        .state()
        .get_stamped(
            &waypoint_core::EntityRef::new("work_order", "wo-1002"),
            "status",
        )
        .unwrap();
    assert!(stamp.wall_ms >= T0);
    b.sync(&mut relay).unwrap();
    a.sync(&mut relay).unwrap();
    assert_eq!(a.work_order("wo-1002").unwrap().status, Status::Paused);

    // Restart b; the relay cursor is persisted, so it pulls nothing old.
    drop(b);
    let mut b = Engine::open(&path_b, "dev-b", clock(&now, -300_000)).unwrap();
    let r = b.sync(&mut relay).unwrap();
    assert_eq!(r.pulled, 0);
    assert_eq!(a.fingerprint(), b.fingerprint());
    assert_eq!(a.store().op_count().unwrap(), b.store().op_count().unwrap());
}

#[test]
fn concurrent_note_edit_and_tag_remove_converge() {
    let now = Arc::new(AtomicI64::new(T0));
    let mut relay = InMemoryRelay::new();
    let mut a = Engine::in_memory("dev-a", clock(&now, 0)).unwrap();
    fixtures::seed(&mut a).unwrap();
    a.sync(&mut relay).unwrap();
    let mut b = Engine::in_memory("dev-b", clock(&now, 0)).unwrap();
    b.sync(&mut relay).unwrap();

    now.fetch_add(1_000, Ordering::SeqCst);
    let note = a.add_note("wo-1001", "first draft").unwrap();
    a.sync(&mut relay).unwrap();
    b.sync(&mut relay).unwrap();

    // Both devices "edit" the note offline: remove + add of a new version.
    now.fetch_add(1_000, Ordering::SeqCst);
    a.remove_note("wo-1001", &note).unwrap();
    a.add_note("wo-1001", "edited on a").unwrap();
    b.remove_note("wo-1001", &note).unwrap();
    b.add_note("wo-1001", "edited on b").unwrap();
    // Concurrent tag ops: a removes, b re-adds the same tag.
    a.remove_site_tag("site-pump-07", "confined-space").unwrap();
    b.add_site_tag("site-pump-07", "confined-space").unwrap();

    for _ in 0..2 {
        a.sync(&mut relay).unwrap();
        b.sync(&mut relay).unwrap();
    }
    assert_eq!(a.fingerprint(), b.fingerprint());
    let texts: Vec<String> = a
        .work_order("wo-1001")
        .unwrap()
        .notes
        .into_iter()
        .map(|n| n.text)
        .collect();
    assert_eq!(texts, vec!["edited on a", "edited on b"]);
    // Add-wins: b's add carried a tag a never observed.
    let pump = a
        .sites()
        .into_iter()
        .find(|s| s.id == "site-pump-07")
        .unwrap();
    assert_eq!(pump.tags, vec!["confined-space"]);
}

#[test]
fn geofence_gates_check_in_and_track_is_persisted() {
    let now = Arc::new(AtomicI64::new(T0));
    let mut e = Engine::in_memory("dev-a", clock(&now, 0)).unwrap();
    fixtures::seed(&mut e).unwrap();
    assert_eq!(
        e.check_in_state("wo-1003").unwrap(),
        CheckInState::SiteHasNoFence
    );
    assert_eq!(
        e.check_in_state("wo-1001").unwrap(),
        CheckInState::NoLocationYet
    );

    e.start_track(Some("wo-1001")).unwrap();
    let mut states = Vec::new();
    let mut events = Vec::new();
    for fix in fixtures::walk_to_pump_station(T0) {
        now.store(fix.timestamp_ms, Ordering::SeqCst);
        events.extend(e.on_location(fix).unwrap());
        let s = e.check_in_state("wo-1001").unwrap();
        if states.last() != Some(&s) {
            states.push(s.clone());
        }
        if s == CheckInState::Allowed {
            e.check_in("wo-1001").unwrap();
        }
    }
    assert_eq!(
        states,
        vec![
            CheckInState::NoLocationYet,
            CheckInState::OutsideFence,
            CheckInState::Allowed,
            CheckInState::AlreadyCheckedIn { by: "dev-a".into() },
        ]
    );
    // One enter and one exit despite noise, a poor fix and a multipath jump.
    assert_eq!(events.len(), 2);
    assert_eq!(e.presence("site-pump-07"), Presence::Outside);
    assert_eq!(e.store().fence_event_count().unwrap(), 2);

    e.check_out("wo-1001", true).unwrap();
    assert_eq!(e.check_in_state("wo-1001").unwrap(), CheckInState::Closed);
    let summary = e.stop_track().unwrap();
    assert!(summary.simplified_points < summary.raw.points_kept / 4);
    assert!((1200.0..1500.0).contains(&summary.raw.distance_m));
    let tracks = e.store().tracks().unwrap();
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].work_order_id.as_deref(), Some("wo-1001"));
    assert_eq!(tracks[0].points.len(), summary.simplified_points);
}
