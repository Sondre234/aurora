//! Damage accumulation: a short list of rectangles that collapses into their union when
//! it grows, so the paint loop stays cheap.

use crate::geom::Rect;

const MAX_RECTS: usize = 8;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Damage {
    rects: Vec<Rect>,
}

impl Damage {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    pub fn rects(&self) -> &[Rect] {
        &self.rects
    }

    /// Add a rect. Rects overlapping an existing one are merged into it.
    pub fn add(&mut self, r: Rect) {
        if r.is_empty() {
            return;
        }
        let mut r = r;
        // Absorb every rect that overlaps or touches; repeat while merges cascade.
        while let Some(i) = self.rects.iter().position(|e| e.outset(0.5).intersects(&r)) {
            r = r.union(&self.rects.swap_remove(i));
        }
        self.rects.push(r);
        if self.rects.len() > MAX_RECTS {
            let all = self.rects.iter().fold(Rect::default(), |a, b| a.union(b));
            self.rects.clear();
            self.rects.push(all);
        }
    }

    /// Replace the content with one rect (e.g. the whole surface).
    pub fn set_full(&mut self, r: Rect) {
        self.rects.clear();
        self.add(r);
    }

    /// Take the accumulated rects, leaving the list empty.
    pub fn take(&mut self) -> Vec<Rect> {
        std::mem::take(&mut self.rects)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_overlaps_and_keeps_disjoint() {
        let mut d = Damage::new();
        d.add(Rect::new(0.0, 0.0, 10.0, 10.0));
        d.add(Rect::new(100.0, 0.0, 10.0, 10.0));
        assert_eq!(d.rects().len(), 2);
        d.add(Rect::new(5.0, 5.0, 100.0, 2.0)); // bridges both
        assert_eq!(d.rects(), &[Rect::new(0.0, 0.0, 110.0, 10.0)]);
        d.add(Rect::default());
        assert_eq!(d.take().len(), 1);
        assert!(d.is_empty());
    }

    #[test]
    fn collapses_when_too_many() {
        let mut d = Damage::new();
        for i in 0..20 {
            d.add(Rect::new(i as f32 * 50.0, 0.0, 10.0, 10.0));
        }
        assert!(d.rects().len() <= MAX_RECTS);
        let all = d.rects().iter().fold(Rect::default(), |a, b| a.union(b));
        assert_eq!(all, Rect::new(0.0, 0.0, 19.0 * 50.0 + 10.0, 10.0));
    }
}
