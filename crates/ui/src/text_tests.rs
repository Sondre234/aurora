//! Text cache and metric tests. They skip silently on a machine with no fonts at all.

use std::sync::Arc;

use crate::text::{TextBudgets, TextStyle, TextSystem};

fn sys() -> Option<TextSystem> {
    let t = TextSystem::new();
    t.has_fonts().then_some(t)
}

#[test]
fn shaping_is_cached_and_scale_is_part_of_the_key() {
    let Some(t) = sys() else { return };
    let st = TextStyle::sized(16.0);
    let a = t.shape("hello world", &st, 1.0, None);
    let b = t.shape("hello world", &st, 1.0, None);
    assert!(Arc::ptr_eq(&a, &b));
    let c = t.shape("hello world", &st, 2.0, None);
    assert!(!Arc::ptr_eq(&a, &c));
    // Logical width is about the same at both scales.
    assert!((a.width() - c.width()).abs() < a.width() * 0.1);
    let s = t.stats().shaped;
    assert_eq!((s.hits, s.misses, s.entries), (1, 2, 2));
    t.invalidate_fonts();
    assert_eq!(t.stats().shaped.entries, 0);
}

#[test]
fn shaped_cache_respects_byte_budget() {
    let Some(t) = sys() else { return };
    t.set_budgets(TextBudgets { shaped: 4096, glyphs: 1 << 20 });
    for i in 0..200 {
        t.shape(&format!("some label number {i}"), &TextStyle::default(), 1.0, None);
    }
    let s = t.stats().shaped;
    assert!(s.bytes <= 4096, "bytes {}", s.bytes);
    assert!(s.evictions > 0 && s.entries < 200);
}

#[test]
fn glyph_cache_respects_byte_budget() {
    let Some(t) = sys() else { return };
    t.set_budgets(TextBudgets { shaped: 1 << 20, glyphs: 2048 });
    let s = t.shape("The quick brown fox jumps over the lazy dog", &TextStyle::sized(24.0), 1.0, None);
    for g in &s.glyphs {
        t.glyph(g.key);
    }
    let g = t.stats().glyphs;
    assert!(g.bytes <= 2048 && g.evictions + g.rejected > 0, "{g:?}");
}

#[test]
fn ellipsis_fits_and_wrap_grows() {
    let Some(t) = sys() else { return };
    let long = "a fairly long line of text that will not fit";
    let natural = t.shape(long, &TextStyle::default(), 1.0, None);
    let cut = t.shape(long, &TextStyle::default(), 1.0, Some(100.0));
    assert!(natural.width() > 150.0);
    assert!(cut.width() <= 101.0, "cut {}", cut.width());
    let wrapped = t.shape(long, &TextStyle::default().wrapped(0), 1.0, Some(100.0));
    assert!(wrapped.height() > natural.height() * 1.5);
    let limited = t.shape(long, &TextStyle::default().wrapped(2), 1.0, Some(100.0));
    assert!(limited.height() < wrapped.height() + 0.1 && limited.height() > natural.height() * 1.5);
}

#[test]
fn cursor_mapping_round_trips() {
    let Some(t) = sys() else { return };
    let text = "abcdef";
    let s = t.shape(text, &TextStyle::default(), 1.0, None);
    assert_eq!(s.cursor_x(0), 0.0);
    let mut last = 0.0;
    for i in 1..=text.len() {
        let x = s.cursor_x(i);
        assert!(x > last, "caret {i} at {x} not after {last}");
        last = x;
        assert_eq!(s.hit(x + 0.1), i, "hit at caret {i}");
    }
    assert!((s.cursor_x(text.len()) - s.width()).abs() < 1.0);
    assert_eq!(s.hit(-5.0), 0);
    assert_eq!(s.hit(1000.0), text.len());
    let empty = t.shape("", &TextStyle::default(), 1.0, None);
    assert_eq!((empty.width(), empty.cursor_x(0), empty.hit(3.0)), (0.0, 0.0, 0));
    assert!(empty.height() > 0.0);
}
