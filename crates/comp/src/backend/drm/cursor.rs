use std::{collections::HashMap, fs};

use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    input::pointer::CursorIcon,
    utils::{Physical, Point, Transform},
};
use xcursor::{CursorTheme, parser::parse_xcursor};

const DEFAULT_SIZE: u32 = 24;
/// Budget for decoded cursor images; a handful of named cursors at 2x is well under 1 MiB.
const BUDGET_BYTES: usize = 4 << 20;
/// Client cursor surfaces above this are composited instead of using the cursor plane.
pub const MAX_PLANE_SIZE: i32 = 256;

#[derive(Clone)]
pub struct CursorImage {
    pub buffer: MemoryRenderBuffer,
    /// Hotspot in buffer pixels.
    pub hotspot: Point<i32, Physical>,
    bytes: usize,
}

struct Entry {
    image: CursorImage,
    bytes: usize,
    last_used: u64,
}

/// Decoded xcursor images for the named cursor shapes.
///
/// Owner: the DRM backend. Invalidated by `set_theme` (theme or size change). Budgeted
/// by `BUDGET_BYTES` with LRU eviction. Only the first frame of an animated cursor is
/// used, so an idle desktop never repaints for the cursor.
pub struct CursorCache {
    name: String,
    size: u32,
    theme: CursorTheme,
    entries: HashMap<(CursorIcon, i32), Entry>,
    bytes: usize,
    tick: u64,
}

impl CursorCache {
    /// Theme and size from XCURSOR_THEME / XCURSOR_SIZE.
    pub fn from_env() -> Self {
        let name = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".into());
        let size = std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|s| *s > 0)
            .unwrap_or(DEFAULT_SIZE);
        let mut cache = Self {
            theme: CursorTheme::load(&name),
            name: String::new(),
            size: 0,
            entries: HashMap::new(),
            bytes: 0,
            tick: 0,
        };
        cache.set_theme(&name, size);
        cache
    }

    /// Drops every cached image when the theme or size differs.
    pub fn set_theme(&mut self, name: &str, size: u32) {
        if self.name == name && self.size == size {
            return;
        }
        tracing::info!(theme = name, size, "cursor theme");
        self.name = name.to_string();
        self.size = size;
        self.theme = CursorTheme::load(name);
        self.entries.clear();
        self.bytes = 0;
        // Decode the arrow now so the first frame never reads from disk.
        self.get(CursorIcon::Default, 1);
    }

    pub fn get(&mut self, icon: CursorIcon, scale: i32) -> CursorImage {
        self.tick += 1;
        let key = (icon, scale);
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.last_used = self.tick;
            return entry.image.clone();
        }

        // A missing theme is cached too, so it never retries the disk per frame.
        let image = self
            .load(icon, scale)
            .unwrap_or_else(|| fallback_image(scale));
        let bytes = image.bytes;
        self.bytes += bytes;
        self.entries.insert(
            key,
            Entry {
                image: image.clone(),
                bytes,
                last_used: self.tick,
            },
        );
        while self.bytes > BUDGET_BYTES {
            let Some(oldest) = self
                .entries
                .iter()
                .filter(|(k, _)| **k != key)
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| *k)
            else {
                break;
            };
            if let Some(evicted) = self.entries.remove(&oldest) {
                self.bytes -= evicted.bytes;
            }
        }
        image
    }

    fn load(&self, icon: CursorIcon, scale: i32) -> Option<CursorImage> {
        let names = std::iter::once(icon.name())
            .chain(icon.alt_names().iter().copied())
            .chain(std::iter::once("default"));
        let images = names.into_iter().find_map(|name| {
            let path = self.theme.load_icon(name)?;
            parse_xcursor(&fs::read(path).ok()?)
        })?;

        let target = self.size as i32 * scale;
        let nearest = images
            .iter()
            .min_by_key(|image| (target - image.size as i32).abs())?;
        let image = images
            .iter()
            .find(|i| i.width == nearest.width && i.height == nearest.height)?;

        // `pixels_rgba` is in file order, which is little-endian ARGB.
        let buffer = MemoryRenderBuffer::from_slice(
            &image.pixels_rgba,
            Fourcc::Argb8888,
            (image.width as i32, image.height as i32),
            scale,
            Transform::Normal,
            None,
        );
        Some(CursorImage {
            buffer,
            hotspot: (image.xhot as i32, image.yhot as i32).into(),
            bytes: image.pixels_rgba.len(),
        })
    }
}

/// Plain white square with a black border, for when no theme provides any cursor.
fn fallback_image(scale: i32) -> CursorImage {
    let side = 16 * scale;
    let mut pixels = Vec::with_capacity((side * side * 4) as usize);
    for y in 0..side {
        for x in 0..side {
            let edge = x < scale || y < scale || x >= side - scale || y >= side - scale;
            let v = if edge { 0 } else { 255 };
            pixels.extend_from_slice(&[v, v, v, 255]);
        }
    }
    CursorImage {
        buffer: MemoryRenderBuffer::from_slice(
            &pixels,
            Fourcc::Argb8888,
            (side, side),
            scale,
            Transform::Normal,
            None,
        ),
        hotspot: (0, 0).into(),
        bytes: pixels.len(),
    }
}
