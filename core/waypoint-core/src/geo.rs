//! Small geodesy toolkit. Distances along the ground use the haversine formula;
//! planar operations (segment distance, point in polygon) use a local
//! equirectangular projection, which is accurate to well under a metre for the
//! few-kilometre extents a field crew works in.

use serde::{Deserialize, Serialize};

pub const EARTH_RADIUS_M: f64 = 6_371_008.8;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LatLon {
    pub lat: f64,
    pub lon: f64,
}

impl LatLon {
    pub const fn new(lat: f64, lon: f64) -> Self {
        Self { lat, lon }
    }

    pub fn is_valid(&self) -> bool {
        self.lat.is_finite()
            && self.lon.is_finite()
            && (-90.0..=90.0).contains(&self.lat)
            && (-180.0..=180.0).contains(&self.lon)
    }
}

/// Great-circle distance in metres.
pub fn haversine_m(a: LatLon, b: LatLon) -> f64 {
    let (p1, p2) = (a.lat.to_radians(), b.lat.to_radians());
    let dp = p2 - p1;
    let dl = (b.lon - a.lon).to_radians();
    let h = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * h.sqrt().min(1.0).asin()
}

/// Equirectangular projection centred on `origin`, producing metres east/north.
#[derive(Debug, Clone, Copy)]
pub struct LocalProjection {
    origin: LatLon,
    cos_lat: f64,
}

impl LocalProjection {
    pub fn new(origin: LatLon) -> Self {
        Self {
            origin,
            cos_lat: origin.lat.to_radians().cos(),
        }
    }

    pub fn project(&self, p: LatLon) -> (f64, f64) {
        let x = (p.lon - self.origin.lon).to_radians() * EARTH_RADIUS_M * self.cos_lat;
        let y = (p.lat - self.origin.lat).to_radians() * EARTH_RADIUS_M;
        (x, y)
    }
}

/// Distance from point `p` to segment `a-b` in the plane.
pub fn point_segment_distance(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 {
        0.0
    } else {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0)
    };
    let (cx, cy) = (a.0 + t * dx, a.1 + t * dy);
    ((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt()
}

/// Even-odd ray casting test in the plane.
pub fn point_in_polygon(p: (f64, f64), poly: &[(f64, f64)]) -> bool {
    let mut inside = false;
    let n = poly.len();
    let mut j = n.wrapping_sub(1);
    for i in 0..n {
        let (xi, yi) = poly[i];
        let (xj, yj) = poly[j];
        if (yi > p.1) != (yj > p.1) && p.0 < (xj - xi) * (p.1 - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haversine_one_degree_of_latitude() {
        let d = haversine_m(LatLon::new(0.0, 0.0), LatLon::new(1.0, 0.0));
        assert!((d - 111_195.0).abs() < 5.0, "got {d}");
    }

    #[test]
    fn projection_agrees_with_haversine_at_short_range() {
        let o = LatLon::new(45.07, 7.68);
        let p = LatLon::new(45.075, 7.69);
        let proj = LocalProjection::new(o);
        let (x, y) = proj.project(p);
        let planar = (x * x + y * y).sqrt();
        assert!((planar - haversine_m(o, p)).abs() < 1.0);
    }

    #[test]
    fn segment_distance_clamps_to_endpoints() {
        assert_eq!(
            point_segment_distance((0.0, 5.0), (0.0, 0.0), (10.0, 0.0)),
            5.0
        );
        assert_eq!(
            point_segment_distance((-3.0, 4.0), (0.0, 0.0), (10.0, 0.0)),
            5.0
        );
    }

    #[test]
    fn polygon_contains() {
        let sq = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)];
        assert!(point_in_polygon((5.0, 5.0), &sq));
        assert!(!point_in_polygon((15.0, 5.0), &sq));
    }
}
