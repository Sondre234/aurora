//! The drawing interface widgets paint through. Backends (the tiny-skia one lives in
//! [`crate::skia`]) implement [`Painter`]; widgets never see a backend type.
//!
//! Coordinates are logical pixels. The painter keeps a state stack ([`Painter::save`] /
//! [`Painter::restore`]) holding a translation and a clip rectangle.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::geom::{Color, Point, Rect};
use crate::text::ShapedText;

/// A decoded raster image or icon: premultiplied RGBA8, cheap to clone.
#[derive(Clone)]
pub struct Image {
    pub(crate) id: u64,
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) rgba: Arc<Vec<u8>>,
}

static NEXT_IMAGE_ID: AtomicU64 = AtomicU64::new(1);

impl Image {
    /// Build from straight (non-premultiplied) RGBA8. Returns `None` if the length does
    /// not match `w * h * 4` or a dimension is zero.
    pub fn from_rgba(w: u32, h: u32, mut rgba: Vec<u8>) -> Option<Self> {
        if w == 0 || h == 0 || rgba.len() != w as usize * h as usize * 4 {
            return None;
        }
        for px in rgba.as_chunks_mut::<4>().0 {
            let a = px[3] as u32;
            for c in &mut px[..3] {
                *c = ((*c as u32 * a + 127) / 255) as u8;
            }
        }
        Some(Self { id: NEXT_IMAGE_ID.fetch_add(1, Ordering::Relaxed), w, h, rgba: Arc::new(rgba) })
    }

    pub fn width(&self) -> u32 {
        self.w
    }

    pub fn height(&self) -> u32 {
        self.h
    }
}

/// Drawing operations. See the module docs for the coordinate model.
pub trait Painter {
    /// Device pixels per logical pixel.
    fn scale(&self) -> f32;

    /// Push the current translation and clip.
    fn save(&mut self);
    /// Pop back to the state at the matching [`Painter::save`].
    fn restore(&mut self);
    /// Shift the origin of everything drawn afterwards.
    fn translate(&mut self, dx: f32, dy: f32);
    /// Intersect the clip with `r` (in the current translated space).
    fn clip_rect(&mut self, r: Rect);

    /// Reset a rectangle to fully transparent (replace, not blend).
    fn clear_rect(&mut self, r: Rect);
    fn fill_rounded_rect(&mut self, r: Rect, radius: f32, color: Color);
    /// Stroke drawn inside `r`.
    fn stroke_rounded_rect(&mut self, r: Rect, radius: f32, width: f32, color: Color);
    /// Cheap blurred drop shadow of a rounded rect (cached per size).
    fn shadow(&mut self, r: Rect, radius: f32, blur: f32, offset: Point, color: Color);
    /// Draw shaped text with its top-left at `origin`.
    fn draw_text(&mut self, text: &ShapedText, origin: Point, color: Color);
    /// Draw an image stretched into `dest`. With `tint`, the image's alpha is used as a
    /// mask and recolored (monochrome icons).
    fn draw_image(&mut self, img: &Image, dest: Rect, tint: Option<Color>);

    fn fill_rect(&mut self, r: Rect, color: Color) {
        self.fill_rounded_rect(r, 0.0, color);
    }
}
