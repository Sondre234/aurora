//! Easing curves over normalized time: `value(t)` maps `t` in 0..=1 to progress, with
//! `value(0) = 0` and `value(1) = 1`. Springs may overshoot in between.

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Curve {
    Linear,
    /// CSS `cubic-bezier(x1, y1, x2, y2)`; x values are clamped to 0..=1.
    CubicBezier(f32, f32, f32, f32),
    /// Damped spring released at 0 toward 1, time-scaled so it has settled at `t = 1`.
    /// `damping_ratio` is clamped to 0.1..=1.0 (1.0 is critically damped, no overshoot).
    Spring {
        damping_ratio: f32,
    },
}

impl Curve {
    pub const EASE_OUT: Curve = Curve::CubicBezier(0.22, 1.0, 0.36, 1.0);
    pub const EASE_IN_OUT: Curve = Curve::CubicBezier(0.65, 0.0, 0.35, 1.0);

    /// Progress at normalized time `t` (clamped to 0..=1).
    pub fn value(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        if t <= 0.0 {
            return 0.0;
        }
        if t >= 1.0 {
            return 1.0;
        }
        match self {
            Curve::Linear => t,
            Curve::CubicBezier(x1, y1, x2, y2) => bezier(x1, y1, x2, y2, t),
            Curve::Spring { damping_ratio } => spring(damping_ratio, t),
        }
    }

    /// Parses `linear`, `ease-out`, `ease-in-out`, `spring`, `spring <ratio>` or
    /// `bezier x1 y1 x2 y2` (also `cubic-bezier(x1, y1, x2, y2)`).
    pub fn parse(text: &str) -> Option<Curve> {
        let text = text.trim().to_ascii_lowercase();
        let inner = text
            .strip_prefix("cubic-bezier(")
            .and_then(|t| t.strip_suffix(')'))
            .map(|t| format!("bezier {t}"));
        let text = inner.as_deref().unwrap_or(&text);
        let words: Vec<&str> = text
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|w| !w.is_empty())
            .collect();
        let num = |w: &str| w.parse::<f32>().ok().filter(|v| v.is_finite());
        match words.as_slice() {
            ["linear"] => Some(Curve::Linear),
            ["ease-out"] => Some(Curve::EASE_OUT),
            ["ease-in-out"] => Some(Curve::EASE_IN_OUT),
            ["spring"] => Some(Curve::Spring { damping_ratio: 0.8 }),
            ["spring", r] => Some(Curve::Spring {
                damping_ratio: num(r)?,
            }),
            ["bezier", a, b, c, d] => {
                let (x1, y1, x2, y2) = (num(a)?, num(b)?, num(c)?, num(d)?);
                ((0.0..=1.0).contains(&x1) && (0.0..=1.0).contains(&x2))
                    .then_some(Curve::CubicBezier(x1, y1, x2, y2))
            }
            _ => None,
        }
    }
}

/// Solves x(s) = t for the bezier parameter s by Newton with a bisection fallback, then
/// returns y(s).
fn bezier(x1: f32, y1: f32, x2: f32, y2: f32, t: f32) -> f32 {
    let (x1, x2) = (x1.clamp(0.0, 1.0) as f64, x2.clamp(0.0, 1.0) as f64);
    let (y1, y2) = (y1 as f64, y2 as f64);
    let t = t as f64;
    let coord = |a: f64, b: f64, s: f64| {
        let u = 1.0 - s;
        3.0 * u * u * s * a + 3.0 * u * s * s * b + s * s * s
    };
    let slope = |a: f64, b: f64, s: f64| {
        let u = 1.0 - s;
        3.0 * u * u * a + 6.0 * u * s * (b - a) + 3.0 * s * s * (1.0 - b)
    };
    let mut s = t;
    for _ in 0..8 {
        let err = coord(x1, x2, s) - t;
        if err.abs() < 1e-7 {
            return coord(y1, y2, s) as f32;
        }
        let d = slope(x1, x2, s);
        if d.abs() < 1e-6 {
            break;
        }
        s -= err / d;
    }
    let (mut lo, mut hi) = (0.0, 1.0);
    s = t;
    for _ in 0..40 {
        let x = coord(x1, x2, s);
        if (x - t).abs() < 1e-7 {
            break;
        }
        if x < t {
            lo = s;
        } else {
            hi = s;
        }
        s = (lo + hi) / 2.0;
    }
    coord(y1, y2, s) as f32
}

/// Unit-step response of a damped oscillator whose envelope e^(-zeta*w*t) has decayed to 0.1%
/// at t = 1, so the curve ends where it should.
fn spring(damping_ratio: f32, t: f32) -> f32 {
    let zeta = damping_ratio.clamp(0.1, 1.0) as f64;
    let t = t as f64;
    let decay = 6.9; // ln(1000)
    let w = decay / zeta; // natural frequency in rad per normalized time
    let x = if zeta >= 0.999 {
        1.0 - (-decay * t).exp() * (1.0 + decay * t)
    } else {
        let wd = w * (1.0 - zeta * zeta).sqrt();
        1.0 - (-decay * t).exp() * ((wd * t).cos() + decay / wd * (wd * t).sin())
    };
    x as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_exact() {
        for c in [
            Curve::Linear,
            Curve::EASE_OUT,
            Curve::EASE_IN_OUT,
            Curve::Spring { damping_ratio: 0.5 },
        ] {
            assert_eq!(c.value(0.0), 0.0);
            assert_eq!(c.value(1.0), 1.0);
            assert_eq!(c.value(-3.0), 0.0);
            assert_eq!(c.value(7.0), 1.0);
        }
    }

    #[test]
    fn linear_bezier_is_identity() {
        let c = Curve::CubicBezier(1.0 / 3.0, 1.0 / 3.0, 2.0 / 3.0, 2.0 / 3.0);
        for i in 0..=10 {
            let t = i as f32 / 10.0;
            assert!((c.value(t) - t).abs() < 1e-3, "{t}");
        }
    }

    #[test]
    fn ease_out_is_fast_then_slow_and_monotonic() {
        let c = Curve::EASE_OUT;
        assert!(c.value(0.2) > 0.4);
        let mut prev = 0.0;
        for i in 1..=100 {
            let v = c.value(i as f32 / 100.0);
            assert!(v >= prev - 1e-6);
            prev = v;
        }
    }

    #[test]
    fn ease_in_out_is_symmetric() {
        let c = Curve::EASE_IN_OUT;
        assert!((c.value(0.5) - 0.5).abs() < 1e-3);
        assert!((c.value(0.25) + c.value(0.75) - 1.0).abs() < 1e-3);
    }

    #[test]
    fn spring_overshoots_unless_critical() {
        let peak = |z| {
            (1..100)
                .map(|i| Curve::Spring { damping_ratio: z }.value(i as f32 / 100.0))
                .fold(0.0f32, f32::max)
        };
        assert!(peak(0.4) > 1.05);
        assert!(peak(1.0) <= 1.0 + 1e-4);
    }

    #[test]
    fn spring_is_settled_near_the_end() {
        let v = Curve::Spring { damping_ratio: 0.5 }.value(0.999);
        assert!((v - 1.0).abs() < 0.01, "{v}");
    }

    #[test]
    fn parsing() {
        assert_eq!(Curve::parse("linear"), Some(Curve::Linear));
        assert_eq!(Curve::parse(" Ease-Out "), Some(Curve::EASE_OUT));
        assert_eq!(
            Curve::parse("bezier 0.1 0.2 0.3 1"),
            Some(Curve::CubicBezier(0.1, 0.2, 0.3, 1.0))
        );
        assert_eq!(
            Curve::parse("cubic-bezier(0.1, 0.2, 0.3, 1.0)"),
            Some(Curve::CubicBezier(0.1, 0.2, 0.3, 1.0))
        );
        assert_eq!(
            Curve::parse("spring 0.5"),
            Some(Curve::Spring { damping_ratio: 0.5 })
        );
        assert_eq!(Curve::parse("bezier 2 0 0 1"), None);
        assert_eq!(Curve::parse("wobble"), None);
        assert_eq!(Curve::parse("bezier 0 0 1"), None);
    }
}
