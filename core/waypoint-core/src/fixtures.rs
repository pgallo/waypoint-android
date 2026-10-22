//! Synthetic sites, work orders and a walking route used by the demo, the
//! FFI "seed" call and the integration tests. Coordinates are an arbitrary
//! patch of open ground; names are invented.

use crate::engine::{Engine, EngineResult};
use crate::geo::LatLon;
use crate::geofence::Shape;
use crate::track::Fix;

pub const DEPOT: LatLon = LatLon::new(45.0700, 7.6800);
pub const PUMP_STATION: LatLon = LatLon::new(45.0745, 7.6850);

pub fn seed(engine: &mut Engine) -> EngineResult<()> {
    engine.upsert_site(
        "site-pump-07",
        "Pump station 7",
        Some(Shape::Circle {
            center: PUMP_STATION,
            radius_m: 60.0,
        }),
    )?;
    engine.upsert_site(
        "site-sub-12",
        "Substation 12",
        Some(Shape::Polygon {
            vertices: vec![
                LatLon::new(45.0680, 7.6900),
                LatLon::new(45.0680, 7.6915),
                LatLon::new(45.0690, 7.6915),
                LatLon::new(45.0690, 7.6900),
            ],
        }),
    )?;
    engine.upsert_site("site-hq", "Yard office", None)?;
    engine.create_work_order(
        "wo-1001",
        "Replace pressure sensor",
        "site-pump-07",
        "crew-north",
        1,
    )?;
    engine.create_work_order(
        "wo-1002",
        "Inspect transformer bay",
        "site-sub-12",
        "crew-north",
        2,
    )?;
    engine.create_work_order(
        "wo-1003",
        "Quarterly safety walk",
        "site-hq",
        "crew-north",
        4,
    )?;
    engine.add_site_tag("site-pump-07", "confined-space")?;
    Ok(())
}

/// A walk from the depot to the pump station, a few minutes on site, then
/// back out, sampled at 5 s with deterministic lateral noise and a couple of
/// junk fixes (a bad-accuracy fix and a multipath jump).
pub fn walk_to_pump_station(start_ms: i64) -> Vec<Fix> {
    let mut out = Vec::new();
    let mut t = start_ms;
    let mut push = |lat: f64, lon: f64, acc: f64, t: &mut i64| {
        out.push(Fix {
            pos: LatLon::new(lat, lon),
            timestamp_ms: *t,
            accuracy_m: acc,
        });
        *t += 5_000;
    };
    // Deterministic jitter in [-1.5 m, 1.5 m] expressed in degrees.
    let noise = |i: usize| ((i * 7919 % 13) as f64 - 6.0) * 0.25 / 111_195.0;
    // Leg 1: north 450 m, ~1.4 m/s.
    let steps = 64;
    for i in 0..=steps {
        let f = i as f64 / steps as f64;
        push(
            DEPOT.lat + f * 0.0045 + noise(i),
            DEPOT.lon + noise(i + 3),
            6.0,
            &mut t,
        );
    }
    // Leg 2: east 390 m to the station centre.
    for i in 1..=56 {
        let f = i as f64 / 56.0;
        push(
            PUMP_STATION.lat + noise(i),
            DEPOT.lon + f * 0.0050 + noise(i + 5),
            6.0,
            &mut t,
        );
        if i == 20 {
            push(PUMP_STATION.lat, DEPOT.lon + f * 0.0050, 85.0, &mut t); // poor fix
        }
        if i == 30 {
            push(PUMP_STATION.lat + 0.01, DEPOT.lon, 8.0, &mut t); // ~1 km jump
        }
    }
    // On site: small wander for ~3 minutes.
    for i in 0..36 {
        push(
            PUMP_STATION.lat + noise(i) * 4.0,
            PUMP_STATION.lon + noise(i + 1) * 4.0,
            5.0,
            &mut t,
        );
    }
    // Leave east 300 m.
    for i in 1..=40 {
        let f = i as f64 / 40.0;
        push(
            PUMP_STATION.lat + noise(i),
            PUMP_STATION.lon + f * 0.0038,
            6.0,
            &mut t,
        );
    }
    out
}
