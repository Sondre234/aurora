//! Output arrangement as pure functions: where outputs go, which one lies in a direction,
//! which mode a config asks for. The compositor applies the results in `wm/outputs.rs`.
use std::cmp::Ordering;

use aurora_layout::{Dir, Point, Rect, Size};

#[cfg(test)]
use crate::config::Config;
use crate::config::ModeSpec;

/// Compares names with digit runs as numbers, so `DP-3` sorts before `DP-10`.
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let run = |it: &mut std::iter::Peekable<std::str::Chars<'_>>| {
                    let mut n = String::new();
                    while let Some(c) = it.next_if(char::is_ascii_digit) {
                        n.push(c);
                    }
                    n.trim_start_matches('0').to_string()
                };
                let (x, y) = (run(&mut a), run(&mut b));
                let order = x.len().cmp(&y.len()).then_with(|| x.cmp(&y));
                if order != Ordering::Equal {
                    return order;
                }
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                a.next();
                b.next();
            }
        }
    }
}

/// Global positions for outputs of the given logical sizes, in the order given. Outputs the
/// config places come first; the rest are packed left to right by natural connector name
/// after the rightmost placed one. The bounding box is moved to the origin.
#[cfg(test)]
pub fn resolve_positions(outputs: &[(String, Size)], config: &Config) -> Vec<Point> {
    resolve_with(outputs, |name| {
        config
            .outputs
            .iter()
            .find(|r| r.name == name)
            .and_then(|r| r.position)
    })
}

pub fn resolve_with(
    outputs: &[(String, Size)],
    explicit: impl Fn(&str) -> Option<(i32, i32)>,
) -> Vec<Point> {
    let mut pos: Vec<Option<Point>> = outputs
        .iter()
        .map(|(name, _)| explicit(name).map(|(x, y)| Point { x, y }))
        .collect();
    let mut next_x = outputs
        .iter()
        .zip(&pos)
        .filter_map(|((_, size), p)| Some(p.as_ref()?.x.saturating_add(size.w)))
        .max()
        .unwrap_or(0);
    let mut rest: Vec<usize> = (0..outputs.len()).filter(|i| pos[*i].is_none()).collect();
    rest.sort_by(|a, b| natural_cmp(&outputs[*a].0, &outputs[*b].0));
    for i in rest {
        pos[i] = Some(Point { x: next_x, y: 0 });
        next_x = next_x.saturating_add(outputs[i].1.w);
    }

    let mut rects: Vec<Rect> = outputs
        .iter()
        .zip(&pos)
        .map(|((_, size), p)| {
            let p = p.unwrap_or_default();
            Rect::new(p.x, p.y, size.w, size.h)
        })
        .collect();
    for i in 0..rects.len() {
        for j in i + 1..rects.len() {
            if rects[i].intersect(rects[j]).is_some() {
                tracing::warn!(
                    "output: {} and {} overlap, check the positions in the config",
                    outputs[i].0,
                    outputs[j].0
                );
            }
        }
    }
    let min_x = rects.iter().map(|r| r.x).min().unwrap_or(0);
    let min_y = rects.iter().map(|r| r.y).min().unwrap_or(0);
    rects
        .iter_mut()
        .map(|r| Point {
            x: r.x.saturating_sub(min_x),
            y: r.y.saturating_sub(min_y),
        })
        .collect()
}

/// The output nearest to `rects[from]` in `dir`. Outputs sharing a span with it on the
/// other axis win over diagonal ones, then the smallest gap, then the closest centres.
pub fn output_in_dir(rects: &[Rect], from: usize, dir: Dir) -> Option<usize> {
    let a = *rects.get(from)?;
    let (ac, horizontal) = (a.center(), matches!(dir, Dir::Left | Dir::Right));
    rects
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != from)
        .filter(|(_, r)| {
            let c = r.center();
            match dir {
                Dir::Left => c.x < ac.x,
                Dir::Right => c.x > ac.x,
                Dir::Up => c.y < ac.y,
                Dir::Down => c.y > ac.y,
            }
        })
        .min_by_key(|(_, r)| {
            let c = r.center();
            let (overlap, gap, off) = if horizontal {
                let gap = match dir {
                    Dir::Left => a.x - r.right(),
                    _ => r.x - a.right(),
                };
                (
                    a.y < r.bottom() && r.y < a.bottom(),
                    gap,
                    (c.y - ac.y).abs(),
                )
            } else {
                let gap = match dir {
                    Dir::Up => a.y - r.bottom(),
                    _ => r.y - a.bottom(),
                };
                (a.x < r.right() && r.x < a.right(), gap, (c.x - ac.x).abs())
            };
            (!overlap, gap.max(0), off)
        })
        .map(|(i, _)| i)
}

/// Index of the mode `spec` asks for among `(width, height, refresh_mhz)` entries: the
/// closest refresh at that size, or the highest one when the spec names none.
pub fn choose_mode(modes: &[(i32, i32, i32)], spec: &ModeSpec) -> Option<usize> {
    let same_size = modes
        .iter()
        .enumerate()
        .filter(|(_, m)| (m.0, m.1) == (spec.width, spec.height));
    match spec.refresh_mhz {
        Some(want) => same_size
            .min_by_key(|(_, m)| (i64::from(m.2) - i64::from(want)).abs())
            .map(|(i, _)| i),
        None => same_size.max_by_key(|(_, m)| m.2).map(|(i, _)| i),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OutputRule;

    fn out(name: &str, w: i32, h: i32) -> (String, Size) {
        (name.to_string(), Size { w, h })
    }

    fn rule(name: &str, position: Option<(i32, i32)>) -> OutputRule {
        OutputRule {
            name: name.into(),
            enabled: true,
            primary: false,
            position,
            mode: None,
            scale: None,
            vrr: Default::default(),
            transform: None,
        }
    }

    fn at(p: &[Point]) -> Vec<(i32, i32)> {
        p.iter().map(|p| (p.x, p.y)).collect()
    }

    #[test]
    fn unplaced_outputs_pack_by_natural_name() {
        let outs = [
            out("HDMI-A-1", 1000, 800),
            out("DP-10", 300, 200),
            out("DP-2", 500, 400),
        ];
        let got = resolve_positions(&outs, &Config::default());
        assert_eq!(at(&got), [(800, 0), (500, 0), (0, 0)]);
    }

    #[test]
    fn explicit_first_then_packed_after_the_rightmost() {
        let config = Config {
            outputs: vec![rule("B", Some((100, 50))), rule("C", None)],
            ..Default::default()
        };
        let outs = [out("A", 400, 300), out("B", 200, 200), out("C", 100, 100)];
        // B ends at x=300, so A and C pack from there; the box starts at (100,50).
        assert_eq!(
            at(&resolve_positions(&outs, &config)),
            [(200, 0), (0, 50), (600, 0)]
        );
    }

    #[test]
    fn negative_origin_is_normalised() {
        let config = Config {
            outputs: vec![rule("A", Some((-1920, -100))), rule("B", Some((0, 0)))],
            ..Default::default()
        };
        let outs = [out("A", 1920, 1080), out("B", 1920, 1080)];
        assert_eq!(
            at(&resolve_positions(&outs, &config)),
            [(0, 0), (1920, 100)]
        );
    }

    #[test]
    fn direction_prefers_shared_span_then_gap() {
        let rects = [
            Rect::new(0, 0, 100, 100),
            Rect::new(100, 0, 100, 100),
            Rect::new(200, 0, 100, 100),
            Rect::new(100, 100, 100, 100),
        ];
        assert_eq!(output_in_dir(&rects, 0, Dir::Right), Some(1));
        assert_eq!(output_in_dir(&rects, 1, Dir::Right), Some(2));
        assert_eq!(output_in_dir(&rects, 1, Dir::Down), Some(3));
        assert_eq!(output_in_dir(&rects, 0, Dir::Left), None);
        assert_eq!(output_in_dir(&rects, 3, Dir::Left), Some(0));
    }

    #[test]
    fn mode_choice_picks_closest_refresh() {
        let modes = [
            (2560, 1440, 60_000),
            (2560, 1440, 144_000),
            (1920, 1080, 60_000),
        ];
        let spec = |r| ModeSpec {
            width: 2560,
            height: 1440,
            refresh_mhz: r,
        };
        assert_eq!(choose_mode(&modes, &spec(Some(120_000))), Some(1));
        assert_eq!(choose_mode(&modes, &spec(None)), Some(1));
        let missing = ModeSpec {
            width: 800,
            height: 600,
            refresh_mhz: None,
        };
        assert_eq!(choose_mode(&modes, &missing), None);
    }
}
