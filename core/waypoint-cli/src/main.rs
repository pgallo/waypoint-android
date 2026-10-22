//! Host demo. Part 1: two tablets edit the same work orders while offline,
//! then sync through a relay and converge. Part 2: one tablet records a walk
//! to a site, gets geofence events and checks in.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use waypoint_core::engine::{Clock, Engine};
use waypoint_core::{fixtures, InMemoryRelay, OfflineTransport, Status, WorkOrder};

const T0: i64 = 1_790_000_000_000;

/// Simulated wall clock per device. `skew_ms` models a tablet whose clock is
/// wrong, which the HLC must tolerate.
fn sim_clock(shared: &Arc<AtomicI64>, skew_ms: i64) -> Clock {
    let s = shared.clone();
    Box::new(move || s.load(Ordering::SeqCst) + skew_ms)
}

fn advance(t: &AtomicI64, ms: i64) {
    t.fetch_add(ms, Ordering::SeqCst);
}

fn print_orders(label: &str, orders: &[WorkOrder]) {
    println!("  [{label}]");
    for w in orders {
        println!(
            "    {:<8} p{} {:<12} {:<28} crew={}",
            w.id,
            w.priority,
            w.status.as_str(),
            w.title,
            w.crew
        );
        for n in &w.notes {
            println!("             note ({}): {}", n.author, n.text);
        }
        for p in &w.photos {
            println!(
                "             photo {}.. {}x{} {}",
                &p.sha256[..8],
                p.width,
                p.height,
                p.caption
            );
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let now = Arc::new(AtomicI64::new(T0));
    let mut relay = InMemoryRelay::new();

    println!("== 1. Offline edits on two devices, then sync ==");
    let mut dispatch = Engine::in_memory("dispatch", sim_clock(&now, 0))?;
    fixtures::seed(&mut dispatch)?;
    dispatch.sync(&mut relay)?;
    let mut a = Engine::in_memory("tablet-a", sim_clock(&now, 0))?;
    // tablet-b's clock runs 90 s slow.
    let mut b = Engine::in_memory("tablet-b", sim_clock(&now, -90_000))?;
    a.sync(&mut relay)?;
    b.sync(&mut relay)?;
    println!("  both tablets start from {} relay ops", relay.len());

    advance(&now, 60_000);
    a.set_title("wo-1001", "Replace pressure sensor and gasket")?;
    a.add_note("wo-1001", "Gasket on inlet flange is cracked")?;
    a.add_site_tag("site-pump-07", "gate-code-4471")?;
    a.set_status("wo-1003", Status::Cancelled)?;
    advance(&now, 30_000);
    b.set_title("wo-1001", "Replace pressure sensor (urgent)")?;
    b.add_note("wo-1001", "Bring 3/4in spanner")?;
    b.remove_site_tag("site-pump-07", "confined-space")?;
    b.attach_photo(
        "wo-1002",
        waypoint_core::PhotoMeta {
            sha256: "9f2c4a1e".repeat(8),
            taken_ms: T0 + 90_000,
            width: 4032,
            height: 3024,
            lat: Some(45.0685),
            lon: Some(7.6907),
            caption: "Bay door corrosion".into(),
        },
    )?;
    // A failed attempt while out of coverage leaves ops queued.
    let off = b.sync(&mut OfflineTransport)?;
    println!(
        "  tablet-b offline attempt: error={:?}, pending={}",
        off.error.unwrap_or_default(),
        off.pending
    );
    println!(
        "  before sync: a={:016x} b={:016x} (diverged)",
        a.fingerprint(),
        b.fingerprint()
    );

    advance(&now, 120_000);
    let ra = a.sync(&mut relay)?;
    let rb = b.sync(&mut relay)?;
    let ra2 = a.sync(&mut relay)?;
    println!(
        "  a: pushed {} pulled {}; b: pushed {} pulled {}; a again: pulled {}",
        ra.pushed, ra.pulled, rb.pushed, rb.pulled, ra2.applied
    );
    println!(
        "  after sync:  a={:016x} b={:016x} converged={}",
        a.fingerprint(),
        b.fingerprint(),
        a.fingerprint() == b.fingerprint()
    );
    print_orders("converged work orders", &a.work_orders());
    let pump = a
        .sites()
        .into_iter()
        .find(|s| s.id == "site-pump-07")
        .unwrap();
    println!("  site-pump-07 tags: {:?}", pump.tags);
    println!(
        "  title conflict resolved by HLC: tablet-b wrote 30 s later in real time but its clock is 90 s slow,"
    );
    println!("  so tablet-a's stamp is higher. Both notes survive (OR-set).");

    println!();
    println!("== 2. Track recording with geofence events ==");
    let fixes = fixtures::walk_to_pump_station(now.load(Ordering::SeqCst));
    a.start_track(Some("wo-1001"))?;
    let mut checked_in = false;
    for fix in &fixes {
        now.store(fix.timestamp_ms, Ordering::SeqCst);
        for ev in a.on_location(*fix)? {
            println!(
                "  t+{:>4}s  {:?} {} (boundary distance {:+.1} m)",
                (ev.timestamp_ms - fixes[0].timestamp_ms) / 1000,
                ev.transition,
                ev.fence_id,
                ev.distance_m
            );
        }
        if !checked_in && a.check_in_state("wo-1001")? == waypoint_core::CheckInState::Allowed {
            a.check_in("wo-1001")?;
            checked_in = true;
            println!("  check-in accepted for wo-1001");
        }
    }
    a.check_out("wo-1001", true)?;
    let s = a.stop_track()?;
    println!(
        "  track #{}: {} raw fixes, {} accepted, {} after Douglas-Peucker (eps {} m)",
        s.track_id,
        s.raw.points_raw,
        s.raw.points_kept,
        s.simplified_points,
        waypoint_core::engine::TRACK_EPSILON_M
    );
    println!(
        "  distance {:.0} m, duration {:.0} s, moving {:.0} s, avg {:.2} m/s",
        s.raw.distance_m, s.raw.duration_s, s.raw.moving_s, s.raw.avg_speed_mps
    );
    let wo = a.work_order("wo-1001").unwrap();
    println!(
        "  wo-1001 status={} checked_in_by={}",
        wo.status.as_str(),
        wo.checked_in_by.unwrap_or_default()
    );
    println!("  pending ops on tablet-a: {}", a.pending_ops()?);
    let r = a.sync(&mut relay)?;
    b.sync(&mut relay)?;
    println!(
        "  synced {} ops; b sees wo-1001 as {}; converged={}",
        r.pushed,
        b.work_order("wo-1001").unwrap().status.as_str(),
        a.fingerprint() == b.fingerprint()
    );
    if a.fingerprint() != b.fingerprint() || !checked_in {
        return Err("demo invariant violated".into());
    }
    Ok(())
}
