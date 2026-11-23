//! Helpers for rules on keypoints: joint angles, distances that don't depend
//! on how far the person stands, visibility, and One-Euro smoothing.

use crate::results::Keypoint;

/// A keypoint under this confidence counts as missing.
pub const MIN_VISIBLE: f32 = 0.3;

pub fn visible(p: &Keypoint, min: f32) -> bool {
    p.confidence >= min && p.x.is_finite() && p.y.is_finite()
}

pub fn distance(a: (f32, f32), b: (f32, f32)) -> f32 {
    (a.0 - b.0).hypot(a.1 - b.1)
}

/// The angle at `b` between `a` and `c`, in degrees (0..180); `None` when
/// a side has no length.
pub fn angle(a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> Option<f32> {
    let (ux, uy) = (a.0 - b.0, a.1 - b.1);
    let (vx, vy) = (c.0 - b.0, c.1 - b.1);
    let (lu, lv) = (ux.hypot(uy), vx.hypot(vy));
    if lu <= f32::EPSILON || lv <= f32::EPSILON {
        return None;
    }
    let cos = ((ux * vx + uy * vy) / (lu * lv)).clamp(-1.0, 1.0);
    Some(cos.acos().to_degrees())
}

/// The distance between the middle of the shoulders and of the hips.
pub fn torso_length(shoulders: [(f32, f32); 2], hips: [(f32, f32); 2]) -> f32 {
    let middle = |p: [(f32, f32); 2]| ((p[0].0 + p[1].0) / 2.0, (p[0].1 + p[1].1) / 2.0);
    distance(middle(shoulders), middle(hips))
}

/// A One-Euro filter's settings: less jitter when still, less lag when moving.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OneEuroSettings {
    /// Hz: lower = smoother at rest.
    pub min_cutoff: f32,
    /// How fast the cutoff rises with speed: higher = less lag.
    pub beta: f32,
}

/// A One-Euro filter on one value (Casiez et al., 2012).
#[derive(Clone, Copy, Debug, Default)]
pub struct OneEuro {
    last: Option<(f32, f32, f64)>,
}

const DERIVATIVE_CUTOFF: f32 = 1.0;

fn alpha(cutoff: f32, dt: f32) -> f32 {
    let tau = 1.0 / (std::f32::consts::TAU * cutoff);
    1.0 / (1.0 + tau / dt)
}

impl OneEuro {
    /// The filtered value at `time` (seconds).
    pub fn filter(&mut self, value: f32, time: f64, s: OneEuroSettings) -> f32 {
        let Some((x, dx, t)) = self.last else {
            self.last = Some((value, 0.0, time));
            return value;
        };
        let dt = (time - t) as f32;
        if dt <= 0.0 {
            return x;
        }
        let raw = (value - x) / dt;
        let a = alpha(DERIVATIVE_CUTOFF, dt);
        let dx = dx + a * (raw - dx);
        let cutoff = s.min_cutoff + s.beta * dx.abs();
        let x = x + alpha(cutoff, dt) * (value - x);
        self.last = Some((x, dx, time));
        x
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: OneEuroSettings = OneEuroSettings {
        min_cutoff: 1.0,
        beta: 0.05,
    };

    #[test]
    fn angles() {
        let near = |a: Option<f32>, b: f32| (a.unwrap() - b).abs() < 0.01;
        assert!(near(angle((1.0, 0.0), (0.0, 0.0), (0.0, 1.0)), 90.0));
        assert!(near(angle((1.0, 0.0), (0.0, 0.0), (-1.0, 0.0)), 180.0));
        assert!(near(angle((1.0, 0.0), (0.0, 0.0), (2.0, 0.0)), 0.0));
        assert_eq!(angle((0.0, 0.0), (0.0, 0.0), (1.0, 0.0)), None);
    }

    #[test]
    fn torso_is_between_the_middles() {
        let l = torso_length([(0.0, 0.0), (10.0, 0.0)], [(2.0, 20.0), (8.0, 20.0)]);
        assert_eq!(l, 20.0);
    }

    #[test]
    fn visibility() {
        let p = |c, x| Keypoint {
            x,
            y: 0.0,
            confidence: c,
        };
        assert!(visible(&p(0.5, 1.0), MIN_VISIBLE));
        assert!(!visible(&p(0.1, 1.0), MIN_VISIBLE));
        assert!(!visible(&p(0.9, f32::NAN), MIN_VISIBLE));
    }

    #[test]
    fn jitter_shrinks_and_a_real_move_is_followed() {
        let mut f = OneEuro::default();
        let mut t = 0.0;
        let mut spread: f32 = 0.0;
        for i in 0..120 {
            t += 1.0 / 30.0;
            let noisy = 100.0 + if i % 2 == 0 { 2.0 } else { -2.0 };
            let out = f.filter(noisy, t, S);
            if i > 30 {
                spread = spread.max((out - 100.0).abs());
            }
        }
        assert!(spread < 1.0, "{spread}");
        let mut out = 0.0;
        for _ in 0..15 {
            t += 1.0 / 30.0;
            out = f.filter(200.0, t, S);
        }
        assert!(out > 190.0, "{out}");
    }

    #[test]
    fn a_repeated_time_changes_nothing() {
        let mut f = OneEuro::default();
        assert_eq!(f.filter(5.0, 1.0, S), 5.0);
        assert_eq!(f.filter(50.0, 1.0, S), 5.0);
    }
}
