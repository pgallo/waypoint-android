//! Geofences with enter/exit events and hysteresis.
//!
//! A raw inside/outside test flaps when a worker stands near the boundary and
//! the fix wanders by a few metres. Two mechanisms suppress that:
//! a band around the boundary (`hysteresis_m`) inside which the state never
//! changes, and a confirmation count: the new side must be observed on
//! `confirm_fixes` consecutive fixes before an event fires. Fixes whose
//! accuracy radius is larger than the band cannot say which side we are on,
//! so they are ignored entirely.

use crate::geo::{haversine_m, point_in_polygon, point_segment_distance, LatLon, LocalProjection};
use crate::track::Fix;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Shape {
    Circle { center: LatLon, radius_m: f64 },
    Polygon { vertices: Vec<LatLon> },
}

impl Shape {
    /// Signed distance to the boundary in metres: negative inside, positive outside.
    pub fn signed_distance_m(&self, p: LatLon) -> f64 {
        match self {
            Shape::Circle { center, radius_m } => haversine_m(*center, p) - radius_m,
            Shape::Polygon { vertices } => {
                if vertices.len() < 3 {
                    return f64::INFINITY;
                }
                let proj = LocalProjection::new(vertices[0]);
                let poly: Vec<(f64, f64)> = vertices.iter().map(|v| proj.project(*v)).collect();
                let q = proj.project(p);
                let n = poly.len();
                let edge = (0..n)
                    .map(|i| point_segment_distance(q, poly[i], poly[(i + 1) % n]))
                    .fold(f64::INFINITY, f64::min);
                if point_in_polygon(q, &poly) {
                    -edge
                } else {
                    edge
                }
            }
        }
    }

    pub fn is_valid(&self) -> bool {
        match self {
            Shape::Circle { center, radius_m } => {
                center.is_valid() && radius_m.is_finite() && *radius_m > 0.0
            }
            Shape::Polygon { vertices } => {
                vertices.len() >= 3 && vertices.iter().all(LatLon::is_valid)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Presence {
    Unknown,
    Inside,
    Outside,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Transition {
    Enter,
    Exit,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeofenceEvent {
    pub fence_id: String,
    pub transition: Transition,
    pub timestamp_ms: i64,
    pub distance_m: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct HysteresisConfig {
    pub hysteresis_m: f64,
    pub confirm_fixes: u32,
}

impl Default for HysteresisConfig {
    fn default() -> Self {
        Self {
            hysteresis_m: 15.0,
            confirm_fixes: 2,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Geofence {
    pub id: String,
    pub shape: Shape,
    presence: Presence,
    pending: Option<(Presence, u32)>,
}

impl Geofence {
    pub fn new(id: impl Into<String>, shape: Shape) -> Self {
        Self {
            id: id.into(),
            shape,
            presence: Presence::Unknown,
            pending: None,
        }
    }

    pub fn presence(&self) -> Presence {
        self.presence
    }

    /// Feed one fix; returns an event when a confirmed transition happens.
    ///
    /// The first confirmed side after `Unknown` only emits an event if it is
    /// `Inside` (starting a shift outside a site is not an "exit").
    pub fn update(&mut self, fix: &Fix, cfg: &HysteresisConfig) -> Option<GeofenceEvent> {
        if fix.accuracy_m > cfg.hysteresis_m.max(1.0) * 2.0 {
            return None;
        }
        let d = self.shape.signed_distance_m(fix.pos);
        let observed = if d <= -cfg.hysteresis_m {
            Presence::Inside
        } else if d >= cfg.hysteresis_m {
            Presence::Outside
        } else {
            // Inside the dead band: no evidence either way, keep pending count.
            return None;
        };
        if observed == self.presence {
            self.pending = None;
            return None;
        }
        let count = match self.pending {
            Some((p, c)) if p == observed => c + 1,
            _ => 1,
        };
        if count < cfg.confirm_fixes.max(1) {
            self.pending = Some((observed, count));
            return None;
        }
        let previous = self.presence;
        self.presence = observed;
        self.pending = None;
        let transition = match (previous, observed) {
            (_, Presence::Inside) => Transition::Enter,
            (Presence::Inside, Presence::Outside) => Transition::Exit,
            _ => return None,
        };
        Some(GeofenceEvent {
            fence_id: self.id.clone(),
            transition,
            timestamp_ms: fix.timestamp_ms,
            distance_m: d,
        })
    }
}

/// A set of fences evaluated together against each incoming fix.
#[derive(Debug, Default)]
pub struct GeofenceMonitor {
    cfg: HysteresisConfig,
    fences: Vec<Geofence>,
}

impl GeofenceMonitor {
    pub fn new(cfg: HysteresisConfig) -> Self {
        Self {
            cfg,
            fences: Vec::new(),
        }
    }

    /// Adds or replaces a fence. Replacing keeps the current presence only if
    /// the shape is unchanged, since a moved boundary invalidates it.
    pub fn upsert(&mut self, id: &str, shape: Shape) {
        if let Some(f) = self.fences.iter_mut().find(|f| f.id == id) {
            if f.shape != shape {
                *f = Geofence::new(id, shape);
            }
        } else {
            self.fences.push(Geofence::new(id, shape));
        }
    }

    pub fn presence(&self, id: &str) -> Presence {
        self.fences
            .iter()
            .find(|f| f.id == id)
            .map_or(Presence::Unknown, Geofence::presence)
    }

    pub fn update(&mut self, fix: &Fix) -> Vec<GeofenceEvent> {
        let cfg = self.cfg;
        self.fences
            .iter_mut()
            .filter_map(|f| f.update(fix, &cfg))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CENTER: LatLon = LatLon::new(45.0, 7.0);

    fn fix_at_north(m: f64, t: i64) -> Fix {
        // Offset north by `m` metres from CENTER.
        Fix {
            pos: LatLon::new(CENTER.lat + m / 111_195.0, CENTER.lon),
            timestamp_ms: t * 1000,
            accuracy_m: 5.0,
        }
    }

    fn circle() -> Shape {
        Shape::Circle {
            center: CENTER,
            radius_m: 100.0,
        }
    }

    #[test]
    fn polygon_signed_distance() {
        let sq = Shape::Polygon {
            vertices: vec![
                LatLon::new(45.0, 7.0),
                LatLon::new(45.0, 7.002),
                LatLon::new(45.002, 7.002),
                LatLon::new(45.002, 7.0),
            ],
        };
        assert!(sq.signed_distance_m(LatLon::new(45.001, 7.001)) < -70.0);
        let out = sq.signed_distance_m(LatLon::new(45.003, 7.001));
        assert!((out - 111.2).abs() < 1.0, "{out}");
    }

    #[test]
    fn enter_and_exit_with_confirmation() {
        let cfg = HysteresisConfig::default();
        let mut g = Geofence::new("s1", circle());
        assert_eq!(g.update(&fix_at_north(300.0, 0), &cfg), None);
        assert_eq!(g.update(&fix_at_north(300.0, 1), &cfg), None); // Unknown -> Outside, silent
        assert_eq!(g.presence(), Presence::Outside);
        assert_eq!(g.update(&fix_at_north(50.0, 2), &cfg), None); // first inside fix: pending
        let ev = g.update(&fix_at_north(40.0, 3), &cfg).unwrap();
        assert_eq!(ev.transition, Transition::Enter);
        assert_eq!(ev.timestamp_ms, 3000);
        g.update(&fix_at_north(200.0, 4), &cfg);
        let ev = g.update(&fix_at_north(200.0, 5), &cfg).unwrap();
        assert_eq!(ev.transition, Transition::Exit);
    }

    #[test]
    fn jitter_across_boundary_does_not_flap() {
        let cfg = HysteresisConfig::default();
        let mut g = Geofence::new("s1", circle());
        for t in 0..3 {
            g.update(&fix_at_north(20.0, t), &cfg);
        }
        assert_eq!(g.presence(), Presence::Inside);
        // Oscillate +/- 10 m around the 100 m boundary: all within the band.
        let events: Vec<_> = (3..60)
            .filter_map(|t| {
                let m = if t % 2 == 0 { 90.0 } else { 110.0 };
                g.update(&fix_at_north(m, t), &cfg)
            })
            .collect();
        assert!(events.is_empty());
        assert_eq!(g.presence(), Presence::Inside);
    }

    #[test]
    fn single_outlier_is_not_confirmed() {
        let cfg = HysteresisConfig::default();
        let mut g = Geofence::new("s1", circle());
        for t in 0..3 {
            g.update(&fix_at_north(0.0, t), &cfg);
        }
        assert_eq!(g.update(&fix_at_north(500.0, 3), &cfg), None);
        assert_eq!(g.update(&fix_at_north(0.0, 4), &cfg), None);
        assert_eq!(g.presence(), Presence::Inside);
    }

    #[test]
    fn inaccurate_fix_is_ignored() {
        let cfg = HysteresisConfig::default();
        let mut g = Geofence::new("s1", circle());
        let mut f = fix_at_north(0.0, 0);
        f.accuracy_m = 200.0;
        g.update(&f, &cfg);
        g.update(&f, &cfg);
        assert_eq!(g.presence(), Presence::Unknown);
    }

    #[test]
    fn monitor_reset_on_shape_change() {
        let mut m = GeofenceMonitor::new(HysteresisConfig::default());
        m.upsert("s1", circle());
        m.update(&fix_at_north(0.0, 0));
        m.update(&fix_at_north(0.0, 1));
        assert_eq!(m.presence("s1"), Presence::Inside);
        m.upsert("s1", circle());
        assert_eq!(m.presence("s1"), Presence::Inside);
        m.upsert(
            "s1",
            Shape::Circle {
                center: CENTER,
                radius_m: 50.0,
            },
        );
        assert_eq!(m.presence("s1"), Presence::Unknown);
        assert_eq!(m.presence("nope"), Presence::Unknown);
    }
}
