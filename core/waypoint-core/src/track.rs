//! GPS track recording: noisy fixes in, a compact polyline and stats out.

use crate::geo::{haversine_m, point_segment_distance, LatLon, LocalProjection};
use serde::{Deserialize, Serialize};

/// One location sample as delivered by the platform location provider.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Fix {
    pub pos: LatLon,
    pub timestamp_ms: i64,
    /// Horizontal accuracy radius in metres (68% confidence), as the OS reports it.
    pub accuracy_m: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct RecorderConfig {
    /// Fixes with a worse accuracy than this are dropped.
    pub max_accuracy_m: f64,
    /// A fix closer than this to the previous accepted one is treated as jitter.
    pub min_step_m: f64,
    /// Implied speeds above this (m/s) are treated as outliers (multipath jumps).
    pub max_speed_mps: f64,
}

impl Default for RecorderConfig {
    fn default() -> Self {
        Self {
            max_accuracy_m: 30.0,
            min_step_m: 2.0,
            max_speed_mps: 70.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TrackStats {
    pub points_raw: usize,
    pub points_kept: usize,
    pub distance_m: f64,
    pub duration_s: f64,
    pub moving_s: f64,
    pub avg_speed_mps: f64,
    pub max_speed_mps: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixVerdict {
    Accepted,
    RejectedAccuracy,
    RejectedJitter,
    RejectedOutOfOrder,
    RejectedSpeed,
    RejectedInvalid,
}

/// Accumulates filtered fixes. Filtering happens on ingest so the in-memory
/// buffer only ever holds plausible points; simplification happens on demand.
#[derive(Debug, Default)]
pub struct TrackRecorder {
    cfg: RecorderConfig,
    fixes: Vec<Fix>,
    seen: usize,
}

/// Below this speed a segment counts as standing still for `moving_s`.
const MOVING_THRESHOLD_MPS: f64 = 0.5;

impl TrackRecorder {
    pub fn new(cfg: RecorderConfig) -> Self {
        Self {
            cfg,
            fixes: Vec::new(),
            seen: 0,
        }
    }

    pub fn push(&mut self, fix: Fix) -> FixVerdict {
        self.seen += 1;
        if !fix.pos.is_valid() || !fix.accuracy_m.is_finite() {
            return FixVerdict::RejectedInvalid;
        }
        if fix.accuracy_m > self.cfg.max_accuracy_m {
            return FixVerdict::RejectedAccuracy;
        }
        if let Some(last) = self.fixes.last() {
            if fix.timestamp_ms <= last.timestamp_ms {
                return FixVerdict::RejectedOutOfOrder;
            }
            let d = haversine_m(last.pos, fix.pos);
            if d < self.cfg.min_step_m {
                return FixVerdict::RejectedJitter;
            }
            let dt = (fix.timestamp_ms - last.timestamp_ms) as f64 / 1000.0;
            if d / dt > self.cfg.max_speed_mps {
                return FixVerdict::RejectedSpeed;
            }
        }
        self.fixes.push(fix);
        FixVerdict::Accepted
    }

    pub fn fixes(&self) -> &[Fix] {
        &self.fixes
    }

    pub fn stats(&self) -> TrackStats {
        stats_for(&self.fixes, self.seen)
    }

    pub fn simplified(&self, epsilon_m: f64) -> Vec<Fix> {
        douglas_peucker(&self.fixes, epsilon_m)
    }
}

pub fn stats_for(fixes: &[Fix], points_raw: usize) -> TrackStats {
    let mut distance = 0.0;
    let mut moving = 0.0;
    let mut vmax: f64 = 0.0;
    for w in fixes.windows(2) {
        let d = haversine_m(w[0].pos, w[1].pos);
        let dt = (w[1].timestamp_ms - w[0].timestamp_ms) as f64 / 1000.0;
        distance += d;
        if dt > 0.0 {
            let v = d / dt;
            vmax = vmax.max(v);
            if v >= MOVING_THRESHOLD_MPS {
                moving += dt;
            }
        }
    }
    let duration = match (fixes.first(), fixes.last()) {
        (Some(a), Some(b)) => (b.timestamp_ms - a.timestamp_ms) as f64 / 1000.0,
        _ => 0.0,
    };
    TrackStats {
        points_raw,
        points_kept: fixes.len(),
        distance_m: distance,
        duration_s: duration,
        moving_s: moving,
        avg_speed_mps: if moving > 0.0 { distance / moving } else { 0.0 },
        max_speed_mps: vmax,
    }
}

/// Ramer-Douglas-Peucker simplification with tolerance in metres.
///
/// Iterative (explicit stack) so a multi-hour track cannot overflow the call
/// stack on a phone's small thread stacks. Endpoints are always kept.
pub fn douglas_peucker(fixes: &[Fix], epsilon_m: f64) -> Vec<Fix> {
    if fixes.len() < 3 {
        return fixes.to_vec();
    }
    let proj = LocalProjection::new(fixes[0].pos);
    let pts: Vec<(f64, f64)> = fixes.iter().map(|f| proj.project(f.pos)).collect();
    let mut keep = vec![false; fixes.len()];
    keep[0] = true;
    keep[fixes.len() - 1] = true;
    let mut stack = vec![(0usize, fixes.len() - 1)];
    while let Some((start, end)) = stack.pop() {
        if end <= start + 1 {
            continue;
        }
        let (mut idx, mut dmax) = (start, 0.0);
        for (i, p) in pts.iter().enumerate().take(end).skip(start + 1) {
            let d = point_segment_distance(*p, pts[start], pts[end]);
            if d > dmax {
                (idx, dmax) = (i, d);
            }
        }
        if dmax > epsilon_m {
            keep[idx] = true;
            stack.push((start, idx));
            stack.push((idx, end));
        }
    }
    fixes
        .iter()
        .zip(keep)
        .filter_map(|(f, k)| k.then_some(*f))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fix(lat: f64, lon: f64, t_s: i64) -> Fix {
        Fix {
            pos: LatLon::new(lat, lon),
            timestamp_ms: t_s * 1000,
            accuracy_m: 5.0,
        }
    }

    #[test]
    fn straight_line_collapses_to_endpoints() {
        let line: Vec<Fix> = (0..50)
            .map(|i| fix(45.0 + i as f64 * 1e-4, 7.0, i))
            .collect();
        let s = douglas_peucker(&line, 1.0);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0], line[0]);
        assert_eq!(s[1], line[49]);
    }

    #[test]
    fn corner_is_preserved() {
        let mut pts: Vec<Fix> = (0..10)
            .map(|i| fix(45.0 + i as f64 * 1e-4, 7.0, i))
            .collect();
        pts.extend((1..10).map(|i| fix(45.0009, 7.0 + i as f64 * 1e-4, 9 + i)));
        let s = douglas_peucker(&pts, 2.0);
        assert_eq!(s.len(), 3);
        assert_eq!(s[1], pts[9]);
    }

    #[test]
    fn simplification_stays_within_epsilon() {
        // A zig-zag with 3 m amplitude: every dropped point must be within eps
        // of the simplified polyline.
        let pts: Vec<Fix> = (0..200)
            .map(|i| {
                let wiggle = if i % 2 == 0 { 0.0 } else { 3e-5 };
                fix(45.0 + i as f64 * 5e-5, 7.0 + wiggle, i)
            })
            .collect();
        let eps = 5.0;
        let s = douglas_peucker(&pts, eps);
        assert!(s.len() < pts.len());
        let proj = LocalProjection::new(pts[0].pos);
        let sp: Vec<_> = s.iter().map(|f| proj.project(f.pos)).collect();
        for p in &pts {
            let pp = proj.project(p.pos);
            let d = sp
                .windows(2)
                .map(|w| point_segment_distance(pp, w[0], w[1]))
                .fold(f64::INFINITY, f64::min);
            assert!(d <= eps + 1e-9, "point off by {d}");
        }
    }

    #[test]
    fn recorder_filters_noise() {
        let mut r = TrackRecorder::new(RecorderConfig::default());
        assert_eq!(r.push(fix(45.0, 7.0, 0)), FixVerdict::Accepted);
        let mut bad = fix(45.001, 7.0, 10);
        bad.accuracy_m = 80.0;
        assert_eq!(r.push(bad), FixVerdict::RejectedAccuracy);
        assert_eq!(r.push(fix(45.000001, 7.0, 11)), FixVerdict::RejectedJitter);
        assert_eq!(r.push(fix(45.1, 7.0, 12)), FixVerdict::RejectedSpeed);
        assert_eq!(r.push(fix(45.0005, 7.0, 0)), FixVerdict::RejectedOutOfOrder);
        assert_eq!(r.push(fix(45.0005, 7.0, 30)), FixVerdict::Accepted);
        assert_eq!(r.push(fix(f64::NAN, 7.0, 31)), FixVerdict::RejectedInvalid);
        let st = r.stats();
        assert_eq!(st.points_raw, 7);
        assert_eq!(st.points_kept, 2);
        assert!((st.distance_m - 55.6).abs() < 0.5);
        assert_eq!(st.duration_s, 30.0);
    }

    #[test]
    fn stats_separate_moving_from_stopped_time() {
        let pts = vec![
            fix(45.0, 7.0, 0),
            fix(45.0009, 7.0, 100),     // ~100 m in 100 s: moving
            fix(45.00090001, 7.0, 400), // ~0 m in 300 s: stopped
        ];
        let st = stats_for(&pts, 3);
        assert_eq!(st.duration_s, 400.0);
        assert_eq!(st.moving_s, 100.0);
        assert!((st.avg_speed_mps - 1.0).abs() < 0.01);
    }
}
