//! Text: cosmic-text shaping behind two byte-budgeted LRU caches.
//!
//! A [`TextSystem`] is a cheap, clonable, single-threaded handle to one shared
//! `FontSystem`, one shaped-text cache and one glyph-bitmap cache. Every surface of a
//! process should clone the same handle so fonts and glyphs are loaded and rasterized once.
//!
//! Cache discipline (see `docs/performance.md`):
//!
//! | Cache | Owner | Invalidated by | Budget / eviction | Metric |
//! |---|---|---|---|---|
//! | shaped text | `TextSystem` | [`TextSystem::invalidate_fonts`] (font set changed); keys carry text, style, device size and width so scale changes miss instead of going stale | [`TextBudgets::shaped`], LRU | `ui cache shaped-text` |
//! | glyph bitmaps | `TextSystem` | [`TextSystem::invalidate_fonts`]; keys carry font, glyph, device size and subpixel bin | [`TextBudgets::glyphs`], LRU | `ui cache glyphs` |
//!
//! Metrics are `tracing` lines on target `perf`, at most every 5 s and only while the
//! cache is in use.
//!
//! Shaping happens at device resolution (`size * scale`); every public measurement is
//! converted back to logical pixels. Glyph origins are snapped to whole device pixels so
//! text is crisp at fractional scales.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use cosmic_text::{
    Attrs, Buffer, CacheKey, Ellipsize, EllipsizeHeightLimit, Family, FontSystem, Metrics, Shaping, Style, SwashCache,
    SwashContent, Weight, Wrap,
};

use crate::cache::{CacheStats, LruCache};

/// Font family selector.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum FontFamily {
    #[default]
    SansSerif,
    Serif,
    Monospace,
    Named(String),
}

/// How a run of text behaves when the available width is too small.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TextWrap {
    /// One line, cut with an ellipsis.
    #[default]
    Ellipsis,
    /// Wrap at word boundaries; `max_lines` of 0 means unlimited, otherwise the last line
    /// is ellipsized.
    Word { max_lines: u16 },
}

/// Font, size and wrapping of a text run. `size` is in logical pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct TextStyle {
    pub size: f32,
    pub weight: u16,
    pub italic: bool,
    pub family: FontFamily,
    /// Line height as a multiple of `size`.
    pub line_height: f32,
    pub wrap: TextWrap,
}

impl Default for TextStyle {
    fn default() -> Self {
        Self { size: 14.0, weight: 400, italic: false, family: FontFamily::SansSerif, line_height: 1.3, wrap: TextWrap::Ellipsis }
    }
}

impl TextStyle {
    pub fn sized(size: f32) -> Self {
        Self { size, ..Self::default() }
    }

    pub fn bold(mut self) -> Self {
        self.weight = 700;
        self
    }

    pub fn mono(mut self) -> Self {
        self.family = FontFamily::Monospace;
        self
    }

    pub fn wrapped(mut self, max_lines: u16) -> Self {
        self.wrap = TextWrap::Word { max_lines };
        self
    }
}

/// Byte budgets for the two text caches.
#[derive(Debug, Clone, Copy)]
pub struct TextBudgets {
    pub shaped: usize,
    pub glyphs: usize,
}

impl Default for TextBudgets {
    fn default() -> Self {
        Self { shaped: 16 << 20, glyphs: 48 << 20 }
    }
}

/// Counters of both caches.
#[derive(Debug, Clone, Copy)]
pub struct TextCacheStats {
    pub shaped: CacheStats,
    pub glyphs: CacheStats,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ShapeKey {
    text: Box<str>,
    size_bits: u32,
    line_height_bits: u32,
    weight: u16,
    italic: bool,
    family: FontFamily,
    wrap: TextWrap,
    /// Device px, ceil; 0 = unconstrained.
    max_w: u32,
}

/// One positioned glyph, relative to the run origin in device pixels.
#[derive(Clone, Copy)]
pub(crate) struct PlacedGlyph {
    pub key: CacheKey,
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy)]
struct Cluster {
    start: usize,
    end: usize,
    /// Device px.
    x: f32,
    w: f32,
}

/// A laid-out run of text. Measurements are logical pixels.
pub struct ShapedText {
    pub(crate) glyphs: Vec<PlacedGlyph>,
    clusters: Vec<Cluster>,
    text_len: usize,
    scale: f32,
    width: f32,
    height: f32,
    baseline: f32,
    /// Device px width of the widest line, for end-of-text cursor placement.
    end_x: f32,
}

impl ShapedText {
    pub fn width(&self) -> f32 {
        self.width
    }

    pub fn height(&self) -> f32 {
        self.height
    }

    /// Distance from the top of the run to the first baseline.
    pub fn baseline(&self) -> f32 {
        self.baseline
    }

    pub fn glyph_count(&self) -> usize {
        self.glyphs.len()
    }

    /// Horizontal position of the caret before byte index `idx` of the text (first line,
    /// left-to-right text). Indexes inside a ligature are interpolated.
    pub fn cursor_x(&self, idx: usize) -> f32 {
        for c in &self.clusters {
            if idx == c.start {
                return c.x / self.scale;
            }
            if idx > c.start && idx < c.end {
                let f = (idx - c.start) as f32 / (c.end - c.start) as f32;
                return (c.x + c.w * f) / self.scale;
            }
        }
        if idx >= self.text_len { self.end_x / self.scale } else { 0.0 }
    }

    /// Byte index of the caret position nearest to logical x (first line).
    pub fn hit(&self, x: f32) -> usize {
        let dx = x * self.scale;
        for c in &self.clusters {
            if dx < c.x + c.w * 0.5 {
                return c.start;
            }
        }
        self.text_len
    }

    fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.glyphs.len() * std::mem::size_of::<PlacedGlyph>()
            + self.clusters.len() * std::mem::size_of::<Cluster>()
    }
}

/// A rasterized glyph bitmap.
pub(crate) struct GlyphImage {
    pub left: i32,
    pub top: i32,
    pub w: u32,
    pub h: u32,
    /// `true`: `data` is straight RGBA (color glyph); `false`: 8-bit coverage.
    pub color: bool,
    pub data: Vec<u8>,
}

struct Inner {
    fonts: FontSystem,
    swash: SwashCache,
    shaped: LruCache<ShapeKey, Arc<ShapedText>>,
    glyphs: LruCache<CacheKey, Arc<GlyphImage>>,
}

/// Shared text engine. See the module docs.
#[derive(Clone)]
pub struct TextSystem(Rc<RefCell<Inner>>);

impl Default for TextSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl TextSystem {
    /// Load system fonts (this takes a moment; do it once at service start).
    pub fn new() -> Self {
        Self::with_font_system(FontSystem::new(), TextBudgets::default())
    }

    /// Use a prepared font system, e.g. one holding only bundled fonts.
    pub fn with_font_system(fonts: FontSystem, budgets: TextBudgets) -> Self {
        Self(Rc::new(RefCell::new(Inner {
            fonts,
            swash: SwashCache::new(),
            shaped: LruCache::new("shaped-text", budgets.shaped),
            glyphs: LruCache::new("glyphs", budgets.glyphs),
        })))
    }

    /// Whether any font face is available at all.
    pub fn has_fonts(&self) -> bool {
        self.0.borrow_mut().fonts.db_mut().faces().next().is_some()
    }

    /// Drop both caches. Call after changing the font set (e.g. after adding a font or a
    /// theme font change); text and scale changes never need this.
    pub fn invalidate_fonts(&self) {
        let mut i = self.0.borrow_mut();
        i.shaped.clear();
        i.glyphs.clear();
    }

    pub fn set_budgets(&self, b: TextBudgets) {
        let mut i = self.0.borrow_mut();
        i.shaped.set_budget(b.shaped);
        i.glyphs.set_budget(b.glyphs);
    }

    pub fn stats(&self) -> TextCacheStats {
        let i = self.0.borrow();
        TextCacheStats { shaped: i.shaped.stats(), glyphs: i.glyphs.stats() }
    }

    /// Shape `text` for a surface of the given `scale`. `max_width` is in logical pixels:
    /// with [`TextWrap::Ellipsis`] the run is cut to it, with [`TextWrap::Word`] it wraps
    /// at it. `None` means unconstrained. Results are cached.
    pub fn shape(&self, text: &str, style: &TextStyle, scale: f32, max_width: Option<f32>) -> Arc<ShapedText> {
        let scale = scale.max(0.25);
        let px = (style.size * scale).max(1.0);
        let key = ShapeKey {
            text: text.into(),
            size_bits: px.to_bits(),
            line_height_bits: style.line_height.to_bits(),
            weight: style.weight,
            italic: style.italic,
            family: style.family.clone(),
            wrap: style.wrap,
            max_w: max_width.map_or(0, |w| (w.max(1.0) * scale).ceil() as u32),
        };
        let mut i = self.0.borrow_mut();
        if let Some(hit) = i.shaped.get(&key) {
            return hit;
        }
        let shaped = Arc::new(shape_uncached(&mut i.fonts, text, style, px, scale, key.max_w));
        let bytes = shaped.bytes() + key.text.len();
        i.shaped.insert(key, shaped.clone(), bytes);
        shaped
    }

    /// Cached coverage/color bitmap of one glyph.
    pub(crate) fn glyph(&self, key: CacheKey) -> Arc<GlyphImage> {
        let mut i = self.0.borrow_mut();
        if let Some(g) = i.glyphs.get(&key) {
            return g;
        }
        let Inner { fonts, swash, glyphs, .. } = &mut *i;
        let img = match swash.get_image_uncached(fonts, key) {
            Some(im) if im.placement.width > 0 && im.placement.height > 0 => GlyphImage {
                left: im.placement.left,
                top: im.placement.top,
                w: im.placement.width,
                h: im.placement.height,
                color: matches!(im.content, SwashContent::Color),
                // Subpixel masks are not requested; treat any non-color content as 8-bit.
                data: im.data,
            },
            _ => GlyphImage { left: 0, top: 0, w: 0, h: 0, color: false, data: Vec::new() },
        };
        let bytes = std::mem::size_of::<GlyphImage>() + img.data.len();
        let img = Arc::new(img);
        glyphs.insert(key, img.clone(), bytes);
        img
    }
}

fn shape_uncached(fonts: &mut FontSystem, text: &str, style: &TextStyle, px: f32, scale: f32, max_w: u32) -> ShapedText {
    let family = match &style.family {
        FontFamily::SansSerif => Family::SansSerif,
        FontFamily::Serif => Family::Serif,
        FontFamily::Monospace => Family::Monospace,
        FontFamily::Named(n) => Family::Name(n),
    };
    let attrs = Attrs::new().family(family).weight(Weight(style.weight)).style(if style.italic {
        Style::Italic
    } else {
        Style::Normal
    });
    let line_h = (px * style.line_height).ceil();
    let mut buf = Buffer::new(fonts, Metrics::new(px, line_h));
    let width = (max_w > 0).then_some(max_w as f32);
    match style.wrap {
        TextWrap::Ellipsis => {
            buf.set_wrap(Wrap::None);
            if width.is_some() {
                buf.set_ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)));
            }
        }
        TextWrap::Word { max_lines } => {
            buf.set_wrap(Wrap::WordOrGlyph);
            if max_lines > 0 && width.is_some() {
                buf.set_ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(max_lines as usize)));
            }
        }
    }
    buf.set_size(width, None);
    buf.set_text(text, &attrs, Shaping::Advanced, None);
    buf.shape_until_scroll(fonts, false);

    let mut glyphs = Vec::new();
    let mut clusters = Vec::new();
    let (mut w, mut bottom, mut baseline, mut first) = (0f32, 0f32, None, true);
    for run in buf.layout_runs() {
        w = w.max(run.line_w);
        bottom = bottom.max(run.line_top + run.line_height);
        baseline.get_or_insert(run.line_y);
        for g in run.glyphs {
            let p = g.physical((0.0, run.line_y), 1.0);
            glyphs.push(PlacedGlyph { key: p.cache_key, x: p.x, y: p.y });
            if first {
                clusters.push(Cluster { start: g.start, end: g.end, x: g.x, w: g.w });
            }
        }
        first = false;
    }
    let bottom = if bottom > 0.0 { bottom } else { line_h };
    ShapedText {
        glyphs,
        clusters,
        text_len: text.len(),
        scale,
        width: w / scale,
        height: bottom / scale,
        baseline: baseline.unwrap_or(px) / scale,
        end_x: w,
    }
}
