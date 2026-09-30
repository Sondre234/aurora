//! tiny-skia backend of [`Painter`], rendering into an ARGB8888 pixel buffer.
//!
//! The target is raw little-endian `wl_shm` ARGB8888 memory (bytes B, G, R, A,
//! premultiplied, stride `width * 4`). tiny-skia blends channel-wise and treats the bytes
//! as RGBA, so this backend hands it colors with red and blue exchanged; nothing is copied
//! or swizzled per frame. Use [`PixelBuffer`] for headless rendering and tests.
//!
//! Caches owned by [`PaintCaches`] (one per process is enough, pass it to every painter):
//!
//! | Cache | Invalidated by | Budget | Metric |
//! |---|---|---|---|
//! | scaled images / icons | key holds image id, device size and tint, so never stale; [`PaintCaches::clear`] on demand | [`PaintBudgets::images`], LRU | `ui cache images` |
//! | shadows | key holds device size, radius, blur and color | [`PaintBudgets::shadows`], LRU | `ui cache shadows` |
//!
//! Pixel snapping: rectangles are snapped to whole device pixels, so edges stay crisp at
//! fractional scales. Clips are rectangles in whole device pixels.

use std::rc::Rc;

use tiny_skia::{
    BlendMode, FillRule, FilterQuality, IntSize, Mask, Paint, Path, PathBuilder, Pixmap, PixmapMut,
    PixmapPaint, Rect as SkRect, Stroke, Transform,
};

use crate::cache::{CacheStats, LruCache};
use crate::geom::{Color, Point, Rect, Size};
use crate::painter::{Image, Painter};
use crate::text::{ShapedText, TextSystem};

/// Byte budgets of [`PaintCaches`].
#[derive(Debug, Clone, Copy)]
pub struct PaintBudgets {
    pub images: usize,
    pub shadows: usize,
}

impl Default for PaintBudgets {
    fn default() -> Self {
        Self {
            images: 32 << 20,
            shadows: 16 << 20,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ImageKey {
    id: u64,
    w: u32,
    h: u32,
    tint: Option<Color>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ShadowKey {
    w: u32,
    h: u32,
    radius: u32,
    blur: u32,
    color: Color,
}

/// Raster caches plus the shared [`TextSystem`], persisting across frames and surfaces.
pub struct PaintCaches {
    text: TextSystem,
    images: LruCache<ImageKey, Rc<Pixmap>>,
    shadows: LruCache<ShadowKey, Rc<Pixmap>>,
}

impl PaintCaches {
    pub fn new(text: TextSystem) -> Self {
        Self::with_budgets(text, PaintBudgets::default())
    }

    pub fn with_budgets(text: TextSystem, b: PaintBudgets) -> Self {
        Self {
            text,
            images: LruCache::new("images", b.images),
            shadows: LruCache::new("shadows", b.shadows),
        }
    }

    pub fn text(&self) -> &TextSystem {
        &self.text
    }

    /// Drop scaled images and shadows.
    pub fn clear(&mut self) {
        self.images.clear();
        self.shadows.clear();
    }

    /// `(images, shadows)` counters.
    pub fn stats(&self) -> (CacheStats, CacheStats) {
        (self.images.stats(), self.shadows.stats())
    }
}

/// An owned ARGB8888 render target for headless rendering.
pub struct PixelBuffer {
    w: u32,
    h: u32,
    data: Vec<u8>,
}

impl PixelBuffer {
    /// Fully transparent buffer. Dimensions are clamped to at least 1.
    pub fn new(w: u32, h: u32) -> Self {
        let (w, h) = (w.max(1), h.max(1));
        Self {
            w,
            h,
            data: vec![0; w as usize * h as usize * 4],
        }
    }

    pub fn width(&self) -> u32 {
        self.w
    }

    pub fn height(&self) -> u32 {
        self.h
    }

    /// Raw ARGB8888 bytes (B, G, R, A per pixel, premultiplied).
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    /// Premultiplied pixel as `[a, r, g, b]`. Panics when out of bounds.
    pub fn argb(&self, x: u32, y: u32) -> [u8; 4] {
        let i = (y as usize * self.w as usize + x as usize) * 4;
        [
            self.data[i + 3],
            self.data[i + 2],
            self.data[i + 1],
            self.data[i],
        ]
    }

    /// Un-premultiplied pixel color. Panics when out of bounds.
    pub fn pixel(&self, x: u32, y: u32) -> Color {
        let [a, r, g, b] = self.argb(x, y);
        if a == 0 {
            return Color::TRANSPARENT;
        }
        let un = |c: u8| ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
        Color::rgba(un(r), un(g), un(b), a)
    }

    /// A painter over the whole buffer at the given scale.
    pub fn painter<'a>(&'a mut self, scale: f32, caches: &'a mut PaintCaches) -> SkiaPainter<'a> {
        SkiaPainter::new(&mut self.data, self.w, self.h, scale, caches)
            .expect("buffer sized by construction")
    }
}

#[derive(Clone)]
struct State {
    tx: f32,
    ty: f32,
    /// Clip in device pixels: x0, y0, x1, y1.
    clip: [i32; 4],
    mask: Option<Rc<Mask>>,
}

/// The tiny-skia painter. Cheap to construct per frame.
pub struct SkiaPainter<'a> {
    pix: PixmapMut<'a>,
    caches: &'a mut PaintCaches,
    scale: f32,
    st: State,
    stack: Vec<State>,
}

impl<'a> SkiaPainter<'a> {
    /// Paint into `data`, which must hold exactly `w * h * 4` bytes of ARGB8888.
    pub fn new(
        data: &'a mut [u8],
        w: u32,
        h: u32,
        scale: f32,
        caches: &'a mut PaintCaches,
    ) -> Option<Self> {
        let pix = PixmapMut::from_bytes(data, w, h)?;
        Some(Self {
            pix,
            caches,
            scale: scale.max(0.25),
            st: State {
                tx: 0.0,
                ty: 0.0,
                clip: [0, 0, w as i32, h as i32],
                mask: None,
            },
            stack: Vec::new(),
        })
    }

    /// Logical size of the target.
    pub fn logical_size(&self) -> Size {
        Size::new(
            self.pix.width() as f32 / self.scale,
            self.pix.height() as f32 / self.scale,
        )
    }

    fn snap(&self, r: Rect) -> [i32; 4] {
        let s = self.scale;
        [
            ((r.x + self.st.tx) * s).round() as i32,
            ((r.y + self.st.ty) * s).round() as i32,
            ((r.right() + self.st.tx) * s).round() as i32,
            ((r.bottom() + self.st.ty) * s).round() as i32,
        ]
    }

    fn visible(&self, b: [i32; 4]) -> bool {
        let c = self.st.clip;
        b[2] > b[0] && b[3] > b[1] && b[0] < c[2] && b[2] > c[0] && b[1] < c[3] && b[3] > c[1]
    }

    /// Mask needed to honor the clip for a draw covering `b`; `None` when `b` is inside
    /// the clip (and the surface), which is the common case.
    fn mask_for(&mut self, b: [i32; 4]) -> Option<Rc<Mask>> {
        let c = self.st.clip;
        let (w, h) = (self.pix.width() as i32, self.pix.height() as i32);
        let inside_clip = b[0] >= c[0] && b[1] >= c[1] && b[2] <= c[2] && b[3] <= c[3];
        if inside_clip && b[0] >= 0 && b[1] >= 0 && b[2] <= w && b[3] <= h {
            return None;
        }
        if self.st.mask.is_none() {
            let mut m = Mask::new(w as u32, h as u32)?;
            let rect = SkRect::from_ltrb(c[0] as f32, c[1] as f32, c[2] as f32, c[3] as f32)?;
            m.fill_path(
                &PathBuilder::from_rect(rect),
                FillRule::Winding,
                false,
                Transform::identity(),
            );
            self.st.mask = Some(Rc::new(m));
        }
        self.st.mask.clone()
    }

    fn paint(color: Color, blend: BlendMode) -> Paint<'static> {
        let mut p = Paint::default();
        // Exchange red and blue: the target bytes are B, G, R, A.
        p.set_color_rgba8(color.b, color.g, color.r, color.a);
        p.blend_mode = blend;
        p.anti_alias = true;
        p
    }

    fn rrect_path(b: [i32; 4], radius: f32, inset: f32) -> Option<Path> {
        let (x0, y0) = (b[0] as f32 + inset, b[1] as f32 + inset);
        let (x1, y1) = (b[2] as f32 - inset, b[3] as f32 - inset);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        let r = radius.min((x1 - x0) * 0.5).min((y1 - y0) * 0.5).max(0.0);
        let mut pb = PathBuilder::new();
        if r < 0.5 {
            pb.push_rect(SkRect::from_ltrb(x0, y0, x1, y1)?);
        } else {
            let k = r * (1.0 - 0.552_284_8);
            pb.move_to(x0 + r, y0);
            pb.line_to(x1 - r, y0);
            pb.cubic_to(x1 - k, y0, x1, y0 + k, x1, y0 + r);
            pb.line_to(x1, y1 - r);
            pb.cubic_to(x1, y1 - k, x1 - k, y1, x1 - r, y1);
            pb.line_to(x0 + r, y1);
            pb.cubic_to(x0 + k, y1, x0, y1 - k, x0, y1 - r);
            pb.line_to(x0, y0 + r);
            pb.cubic_to(x0, y0 + k, x0 + k, y0, x0 + r, y0);
            pb.close();
        }
        pb.finish()
    }

    fn blit(&mut self, pm: &Pixmap, x: i32, y: i32) {
        let b = [x, y, x + pm.width() as i32, y + pm.height() as i32];
        if !self.visible(b) {
            return;
        }
        let mask = self.mask_for(b);
        self.pix.draw_pixmap(
            x,
            y,
            pm.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            mask.as_deref(),
        );
    }
}

impl Painter for SkiaPainter<'_> {
    fn scale(&self) -> f32 {
        self.scale
    }

    fn save(&mut self) {
        self.stack.push(self.st.clone());
    }

    fn restore(&mut self) {
        if let Some(s) = self.stack.pop() {
            self.st = s;
        }
    }

    fn translate(&mut self, dx: f32, dy: f32) {
        self.st.tx += dx;
        self.st.ty += dy;
    }

    fn clip_rect(&mut self, r: Rect) {
        let b = self.snap(r);
        let c = &mut self.st.clip;
        *c = [
            c[0].max(b[0]),
            c[1].max(b[1]),
            c[2].min(b[2]),
            c[3].min(b[3]),
        ];
        if c[2] < c[0] {
            c[2] = c[0];
        }
        if c[3] < c[1] {
            c[3] = c[1];
        }
        self.st.mask = None;
    }

    fn clear_rect(&mut self, r: Rect) {
        let b = self.snap(r);
        if !self.visible(b) {
            return;
        }
        let mask = self.mask_for(b);
        let Some(rect) = SkRect::from_ltrb(b[0] as f32, b[1] as f32, b[2] as f32, b[3] as f32)
        else {
            return;
        };
        let mut p = Self::paint(Color::TRANSPARENT, BlendMode::Source);
        p.anti_alias = false;
        self.pix
            .fill_rect(rect, &p, Transform::identity(), mask.as_deref());
    }

    fn fill_rounded_rect(&mut self, r: Rect, radius: f32, color: Color) {
        if color.a == 0 {
            return;
        }
        let b = self.snap(r);
        if !self.visible(b) {
            return;
        }
        let mask = self.mask_for(b);
        let rad = radius * self.scale;
        if rad < 0.5 {
            let Some(rect) = SkRect::from_ltrb(b[0] as f32, b[1] as f32, b[2] as f32, b[3] as f32)
            else {
                return;
            };
            let mut p = Self::paint(color, BlendMode::SourceOver);
            p.anti_alias = false;
            self.pix
                .fill_rect(rect, &p, Transform::identity(), mask.as_deref());
        } else if let Some(path) = Self::rrect_path(b, rad, 0.0) {
            let p = Self::paint(color, BlendMode::SourceOver);
            self.pix.fill_path(
                &path,
                &p,
                FillRule::Winding,
                Transform::identity(),
                mask.as_deref(),
            );
        }
    }

    fn stroke_rounded_rect(&mut self, r: Rect, radius: f32, width: f32, color: Color) {
        if color.a == 0 || width <= 0.0 {
            return;
        }
        let b = self.snap(r);
        if !self.visible(b) {
            return;
        }
        let mask = self.mask_for(b);
        let w = (width * self.scale).max(1.0).round();
        let Some(path) = Self::rrect_path(b, (radius * self.scale - w * 0.5).max(0.0), w * 0.5)
        else {
            return;
        };
        let p = Self::paint(color, BlendMode::SourceOver);
        let stroke = Stroke {
            width: w,
            ..Stroke::default()
        };
        self.pix
            .stroke_path(&path, &p, &stroke, Transform::identity(), mask.as_deref());
    }

    fn shadow(&mut self, r: Rect, radius: f32, blur: f32, offset: Point, color: Color) {
        if color.a == 0 || blur <= 0.0 {
            return;
        }
        let b = self.snap(r.translate(offset.x, offset.y));
        let (w, h) = ((b[2] - b[0]).max(0) as u32, (b[3] - b[1]).max(0) as u32);
        if w == 0 || h == 0 {
            return;
        }
        let blur_px = ((blur * self.scale).round() as u32).max(1);
        let key = ShadowKey {
            w,
            h,
            radius: (radius * self.scale).round() as u32,
            blur: blur_px,
            color,
        };
        let pm = match self.caches.shadows.get(&key) {
            Some(p) => p,
            None => {
                let Some(pm) = build_shadow(&key) else { return };
                let pm = Rc::new(pm);
                let bytes = pm.data().len();
                self.caches.shadows.insert(key, pm.clone(), bytes);
                pm
            }
        };
        self.blit(&pm, b[0] - blur_px as i32, b[1] - blur_px as i32);
    }

    fn draw_text(&mut self, text: &ShapedText, origin: Point, color: Color) {
        if color.a == 0 {
            return;
        }
        let ox = ((origin.x + self.st.tx) * self.scale).round() as i32;
        let oy = ((origin.y + self.st.ty) * self.scale).round() as i32;
        let c = self.st.clip;
        let (pw, ph) = (self.pix.width() as i32, self.pix.height() as i32);
        let (cx0, cy0, cx1, cy1) = (c[0].max(0), c[1].max(0), c[2].min(pw), c[3].min(ph));
        let ts = self.caches.text.clone();
        let data = self.pix.data_mut();
        for g in &text.glyphs {
            let gi = ts.glyph(g.key);
            if gi.w == 0 {
                continue;
            }
            let gx = ox + g.x + gi.left;
            let gy = oy + g.y - gi.top;
            let (x0, x1) = (gx.max(cx0), (gx + gi.w as i32).min(cx1));
            let (y0, y1) = (gy.max(cy0), (gy + gi.h as i32).min(cy1));
            if x0 >= x1 || y0 >= y1 {
                continue;
            }
            for y in y0..y1 {
                let srow = ((y - gy) as usize * gi.w as usize + (x0 - gx) as usize)
                    * if gi.color { 4 } else { 1 };
                let mut si = srow;
                let mut di = (y as usize * pw as usize + x0 as usize) * 4;
                for _ in x0..x1 {
                    let (cov, src) = if gi.color {
                        let s = &gi.data[si..si + 4];
                        si += 4;
                        // Color glyph: its own color, alpha scaled by the text alpha.
                        (s[3] as u32 * color.a as u32 / 255, [s[2], s[1], s[0]])
                    } else {
                        let v = gi.data[si] as u32;
                        si += 1;
                        (v * color.a as u32 / 255, [color.b, color.g, color.r])
                    };
                    if cov != 0 {
                        let inv = 255 - cov;
                        for (k, s) in src.iter().enumerate() {
                            let d = data[di + k] as u32;
                            data[di + k] = ((*s as u32 * cov + 127) / 255 + (d * inv + 127) / 255)
                                .min(255) as u8;
                        }
                        let d = data[di + 3] as u32;
                        data[di + 3] = (cov + (d * inv + 127) / 255).min(255) as u8;
                    }
                    di += 4;
                }
            }
        }
    }

    fn draw_image(&mut self, img: &Image, dest: Rect, tint: Option<Color>) {
        let b = self.snap(dest);
        let (w, h) = ((b[2] - b[0]).max(0) as u32, (b[3] - b[1]).max(0) as u32);
        if w == 0 || h == 0 || !self.visible(b) {
            return;
        }
        let key = ImageKey {
            id: img.id,
            w,
            h,
            tint,
        };
        let pm = match self.caches.images.get(&key) {
            Some(p) => p,
            None => {
                let Some(pm) = scale_image(img, w, h, tint) else {
                    return;
                };
                let pm = Rc::new(pm);
                let bytes = pm.data().len();
                self.caches.images.insert(key, pm.clone(), bytes);
                pm
            }
        };
        self.blit(&pm, b[0], b[1]);
    }
}

/// Scale a premultiplied RGBA image to `w x h` and convert to target byte order.
fn scale_image(img: &Image, w: u32, h: u32, tint: Option<Color>) -> Option<Pixmap> {
    let src = Pixmap::from_vec(img.rgba.to_vec(), IntSize::from_wh(img.w, img.h)?)?;
    let mut dst = Pixmap::new(w, h)?;
    let paint = PixmapPaint {
        quality: FilterQuality::Bilinear,
        ..PixmapPaint::default()
    };
    let tf = Transform::from_scale(w as f32 / img.w as f32, h as f32 / img.h as f32);
    dst.draw_pixmap(0, 0, src.as_ref(), &paint, tf, None);
    for px in dst.data_mut().as_chunks_mut::<4>().0 {
        if let Some(t) = tint {
            let a = px[3] as u32 * t.a as u32 / 255;
            px[0] = (t.b as u32 * a / 255) as u8;
            px[1] = (t.g as u32 * a / 255) as u8;
            px[2] = (t.r as u32 * a / 255) as u8;
            px[3] = a as u8;
        } else {
            px.swap(0, 2);
        }
    }
    Some(dst)
}

/// Blurred rounded-rect alpha, colorized into target byte order. Size is the rect plus
/// `blur` on every side.
fn build_shadow(k: &ShadowKey) -> Option<Pixmap> {
    let b = k.blur;
    let (w, h) = (k.w + 2 * b, k.h + 2 * b);
    let mut shape = Pixmap::new(w, h)?;
    let path = SkiaPainter::rrect_path(
        [b as i32, b as i32, (b + k.w) as i32, (b + k.h) as i32],
        k.radius as f32,
        0.0,
    )?;
    let mut p = Paint::default();
    p.set_color_rgba8(0, 0, 0, 255);
    shape.fill_path(&path, &p, FillRule::Winding, Transform::identity(), None);
    let mut alpha: Vec<u8> = shape
        .data()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|px| px[3])
        .collect();
    let r = (b / 2).max(1) as usize;
    for _ in 0..3 {
        box_blur(&mut alpha, w as usize, h as usize, r);
    }
    let mut out = Pixmap::new(w, h)?;
    for (px, a) in out.data_mut().as_chunks_mut::<4>().0.iter_mut().zip(alpha) {
        let a = a as u32 * k.color.a as u32 / 255;
        px[0] = (k.color.b as u32 * a / 255) as u8;
        px[1] = (k.color.g as u32 * a / 255) as u8;
        px[2] = (k.color.r as u32 * a / 255) as u8;
        px[3] = a as u8;
    }
    Some(out)
}

/// In-place separable box blur of an 8-bit plane with window `2r + 1`, zero outside.
fn box_blur(a: &mut [u8], w: usize, h: usize, r: usize) {
    let div = (2 * r + 1) as u32;
    let mut line = vec![0u8; w.max(h)];
    let pass = |get: &dyn Fn(usize) -> u8, len: usize, line: &mut [u8]| {
        let mut sum: u32 = (0..=r.min(len - 1)).map(|i| get(i) as u32).sum();
        for (i, out) in line.iter_mut().enumerate().take(len) {
            *out = ((sum + div / 2) / div) as u8;
            if i + r + 1 < len {
                sum += get(i + r + 1) as u32;
            }
            if i >= r {
                sum -= get(i - r) as u32;
            }
        }
    };
    for y in 0..h {
        let row = &mut a[y * w..(y + 1) * w];
        let src = row.to_vec();
        pass(&|i| src[i], w, &mut line);
        row.copy_from_slice(&line[..w]);
    }
    for x in 0..w {
        let col: Vec<u8> = (0..h).map(|y| a[y * w + x]).collect();
        pass(&|i| col[i], h, &mut line);
        for y in 0..h {
            a[y * w + x] = line[y];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caches() -> PaintCaches {
        PaintCaches::new(TextSystem::new())
    }

    #[test]
    fn fill_is_argb_premultiplied() {
        let mut c = caches();
        let mut buf = PixelBuffer::new(20, 20);
        {
            let mut p = buf.painter(1.0, &mut c);
            p.fill_rect(Rect::new(0.0, 0.0, 10.0, 10.0), Color::rgb(255, 0, 0));
            p.fill_rect(
                Rect::new(10.0, 0.0, 10.0, 10.0),
                Color::rgba(0, 0, 255, 128),
            );
        }
        assert_eq!(buf.argb(5, 5), [255, 255, 0, 0]);
        assert_eq!(buf.pixel(5, 5), Color::rgb(255, 0, 0));
        let [a, r, g, b] = buf.argb(15, 5);
        assert_eq!((a, r, g), (128, 0, 0));
        assert!((127..=129).contains(&b));
        assert_eq!(buf.argb(5, 15), [0, 0, 0, 0]);
        // Raw bytes are B, G, R, A.
        assert_eq!(&buf.bytes()[(5 * 20 + 5) * 4..][..4], &[0, 0, 255, 255]);
    }

    #[test]
    fn clip_and_translate() {
        let mut c = caches();
        let mut buf = PixelBuffer::new(20, 20);
        {
            let mut p = buf.painter(1.0, &mut c);
            p.save();
            p.translate(5.0, 5.0);
            p.clip_rect(Rect::new(0.0, 0.0, 5.0, 5.0));
            p.fill_rect(Rect::new(-100.0, -100.0, 300.0, 300.0), Color::WHITE);
            p.restore();
            p.fill_rect(Rect::new(0.0, 0.0, 2.0, 2.0), Color::BLACK);
        }
        assert_eq!(buf.pixel(4, 4), Color::TRANSPARENT);
        assert_eq!(buf.pixel(5, 5), Color::WHITE);
        assert_eq!(buf.pixel(9, 9), Color::WHITE);
        assert_eq!(buf.pixel(10, 10), Color::TRANSPARENT);
        assert_eq!(buf.pixel(0, 0), Color::BLACK);
    }

    #[test]
    fn rounded_corner_and_clear() {
        let mut c = caches();
        let mut buf = PixelBuffer::new(40, 40);
        let mut p = buf.painter(1.0, &mut c);
        p.fill_rounded_rect(Rect::new(0.0, 0.0, 40.0, 40.0), 12.0, Color::WHITE);
        p.clear_rect(Rect::new(20.0, 20.0, 20.0, 20.0));
        drop(p);
        assert_eq!(buf.pixel(0, 0).a, 0, "corner cut away");
        assert_eq!(buf.pixel(20, 5), Color::WHITE);
        assert_eq!(buf.pixel(30, 30), Color::TRANSPARENT);
    }

    #[test]
    fn fractional_scale_snaps_edges() {
        let mut c = caches();
        let mut buf = PixelBuffer::new(30, 30);
        buf.painter(1.5, &mut c)
            .fill_rect(Rect::new(2.0, 2.0, 10.0, 10.0), Color::WHITE);
        // 2*1.5=3 .. 12*1.5=18: crisp, no partial coverage at the edge.
        assert_eq!(buf.pixel(3, 3), Color::WHITE);
        assert_eq!(buf.pixel(17, 17), Color::WHITE);
        assert_eq!(buf.pixel(2, 10), Color::TRANSPARENT);
        assert_eq!(buf.pixel(18, 10), Color::TRANSPARENT);
    }

    #[test]
    fn stroke_stays_inside() {
        let mut c = caches();
        let mut buf = PixelBuffer::new(20, 20);
        buf.painter(1.0, &mut c).stroke_rounded_rect(
            Rect::new(2.0, 2.0, 16.0, 16.0),
            0.0,
            2.0,
            Color::WHITE,
        );
        assert_eq!(buf.pixel(2, 10), Color::WHITE);
        assert_eq!(buf.pixel(3, 10), Color::WHITE);
        assert_eq!(buf.pixel(1, 10).a, 0);
        assert_eq!(buf.pixel(10, 10).a, 0);
    }

    #[test]
    fn image_scaled_and_tinted() {
        let mut c = caches();
        let img = Image::from_rgba(2, 2, vec![255; 16]).unwrap();
        let mut buf = PixelBuffer::new(20, 10);
        {
            let mut p = buf.painter(1.0, &mut c);
            p.draw_image(&img, Rect::new(0.0, 0.0, 8.0, 8.0), None);
            p.draw_image(
                &img,
                Rect::new(10.0, 0.0, 8.0, 8.0),
                Some(Color::rgb(0, 255, 0)),
            );
        }
        assert_eq!(buf.pixel(4, 4), Color::WHITE);
        assert_eq!(buf.pixel(14, 4), Color::rgb(0, 255, 0));
        assert_eq!(buf.pixel(9, 4).a, 0);
        let (images, _) = c.stats();
        assert_eq!(images.entries, 2);
        // Second draw of the same image at the same size hits the cache.
        buf.painter(1.0, &mut c)
            .draw_image(&img, Rect::new(0.0, 0.0, 8.0, 8.0), None);
        assert_eq!(c.stats().0.hits, 1);
    }

    #[test]
    fn shadow_fades_outward_and_caches() {
        let mut c = caches();
        let mut buf = PixelBuffer::new(60, 60);
        let draw = |buf: &mut PixelBuffer, c: &mut PaintCaches| {
            buf.painter(1.0, c).shadow(
                Rect::new(20.0, 20.0, 20.0, 20.0),
                4.0,
                10.0,
                Point::new(0.0, 0.0),
                Color::BLACK,
            );
        };
        draw(&mut buf, &mut c);
        let center = buf.pixel(30, 30).a;
        let near = buf.pixel(18, 30).a;
        let far = buf.pixel(12, 30).a;
        assert!(center > 200, "center {center}");
        assert!(
            near < center && far < near && far > 0 || far == 0,
            "{center} {near} {far}"
        );
        draw(&mut buf, &mut c);
        assert_eq!(c.stats().1.hits, 1);
    }

    #[test]
    fn text_draws_pixels() {
        let ts = TextSystem::new();
        if !ts.has_fonts() {
            return;
        }
        let mut c = PaintCaches::new(ts.clone());
        let shaped = ts.shape("Hello", &crate::text::TextStyle::sized(20.0), 1.0, None);
        assert!(shaped.width() > 20.0 && shaped.height() >= 20.0);
        let mut buf = PixelBuffer::new(120, 40);
        buf.painter(1.0, &mut c)
            .draw_text(&shaped, Point::new(4.0, 4.0), Color::WHITE);
        let lit = (0..40)
            .flat_map(|y| (0..120).map(move |x| (x, y)))
            .filter(|&(x, y)| buf.pixel(x, y).a > 0)
            .count();
        assert!(lit > 50, "lit {lit}");
        // Nothing outside the run's box (plus a pixel of overhang).
        assert_eq!(buf.pixel(119, 39).a, 0);
    }
}
