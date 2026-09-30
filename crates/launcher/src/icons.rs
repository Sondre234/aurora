//! Icon lookup, rasterization and the byte-budgeted icon cache.
//!
//! Lookup goes through the icon theme (`freedesktop-icons`), decoding happens on a worker
//! thread ([`IconLoader`]) so a cold icon never stalls a keystroke, and the result is a
//! small pre-rasterized RGBA image (PNG decoded, SVG rendered with resvg, both resampled
//! to `px` pixels). The main thread keeps them in [`IconCache`]: owner the launcher,
//! invalidated when the icon theme or the app index changes, LRU under a byte budget,
//! with a `perf` tracing metric (see `aurora_ui::cache`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use aurora_ui::cache::{CacheStats, LruCache};
use aurora_ui::Image;

/// Decoded straight-alpha RGBA, `w * h * 4` bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawIcon {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

impl RawIcon {
    pub fn bytes(&self) -> usize {
        self.rgba.len()
    }
}

/// Finds the file for an icon name (or returns an absolute path as is).
pub fn find_icon(name: &str, theme: Option<&str>, px: u16) -> Option<PathBuf> {
    if name.starts_with('/') {
        return Path::new(name).is_file().then(|| PathBuf::from(name));
    }
    let themes = theme.into_iter().chain(["hicolor"]);
    for t in themes {
        if let Some(p) = freedesktop_icons::lookup(name)
            .with_size(px)
            .with_theme(t)
            .with_cache()
            .find()
        {
            return Some(p);
        }
    }
    ["png", "svg"]
        .iter()
        .map(|ext| PathBuf::from(format!("/usr/share/pixmaps/{name}.{ext}")))
        .find(|p| p.is_file())
}

/// Decodes a PNG or SVG file to at most `px` x `px`, keeping the aspect ratio.
pub fn load_icon(path: &Path, px: u32) -> Option<RawIcon> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let raw = match ext.as_str() {
        "png" => decode_png(path)?,
        "svg" => render_svg(path, px)?,
        _ => return None,
    };
    Some(fit(&raw, px))
}

fn decode_png(path: &Path) -> Option<RawIcon> {
    let file = std::fs::File::open(path).ok()?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    let data = &buf[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => data.to_vec(),
        png::ColorType::Rgb => data
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => data
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => data.iter().flat_map(|g| [*g, *g, *g, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    (w > 0 && h > 0 && rgba.len() == w as usize * h as usize * 4).then_some(RawIcon { w, h, rgba })
}

fn render_svg(path: &Path, px: u32) -> Option<RawIcon> {
    let data = std::fs::read(path).ok()?;
    let tree = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default()).ok()?;
    let size = tree.size();
    let longest = size.width().max(size.height());
    if !(longest > 0.0) {
        return None;
    }
    let scale = px as f32 / longest;
    let w = ((size.width() * scale).round() as u32).max(1);
    let h = ((size.height() * scale).round() as u32).max(1);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w, h)?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    // tiny-skia is premultiplied; `Image::from_rgba` wants straight alpha.
    let mut rgba = pixmap.take();
    for p in rgba.chunks_exact_mut(4) {
        let a = p[3] as u32;
        if a != 0 && a != 255 {
            for c in &mut p[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    Some(RawIcon { w, h, rgba })
}

/// Resamples so the longest side is at most `px` (never upscales past `px` either: a 16 px
/// icon is enlarged to `px` so all rows look alike). Area averaging on premultiplied color.
pub fn fit(src: &RawIcon, px: u32) -> RawIcon {
    let longest = src.w.max(src.h);
    if longest == px || px == 0 {
        return src.clone();
    }
    let scale = px as f32 / longest as f32;
    let w = ((src.w as f32 * scale).round() as u32).max(1);
    let h = ((src.h as f32 * scale).round() as u32).max(1);
    let mut out = vec![0u8; w as usize * h as usize * 4];
    for y in 0..h {
        let y0 = (y as f32 / scale).floor() as u32;
        let y1 = (((y + 1) as f32 / scale).ceil() as u32).clamp(y0 + 1, src.h);
        for x in 0..w {
            let x0 = (x as f32 / scale).floor() as u32;
            let x1 = (((x + 1) as f32 / scale).ceil() as u32).clamp(x0 + 1, src.w);
            let (mut r, mut g, mut b, mut a, mut n) = (0u64, 0u64, 0u64, 0u64, 0u64);
            for sy in y0.min(src.h - 1)..y1 {
                for sx in x0.min(src.w - 1)..x1 {
                    let i = (sy as usize * src.w as usize + sx as usize) * 4;
                    let pa = src.rgba[i + 3] as u64;
                    r += src.rgba[i] as u64 * pa;
                    g += src.rgba[i + 1] as u64 * pa;
                    b += src.rgba[i + 2] as u64 * pa;
                    a += pa;
                    n += 1;
                }
            }
            let o = (y as usize * w as usize + x as usize) * 4;
            if a > 0 {
                out[o] = ((r + a / 2) / a) as u8;
                out[o + 1] = ((g + a / 2) / a) as u8;
                out[o + 2] = ((b + a / 2) / a) as u8;
                out[o + 3] = ((a + n / 2) / n) as u8;
            }
        }
    }
    RawIcon { w, h, rgba: out }
}

/// What the worker thread answers: the icon name and its pixels (`None` when the theme
/// has no such icon or it cannot be decoded).
pub type Loaded = (String, Option<RawIcon>);

/// A worker thread that resolves and rasterizes icons on request.
pub struct IconLoader {
    tx: mpsc::Sender<String>,
}

impl IconLoader {
    /// `on_loaded` runs on the worker thread for every finished request (forward it into
    /// the event loop's channel). The thread ends when the loader is dropped.
    pub fn spawn(
        theme: Option<String>,
        px: u32,
        on_loaded: impl Fn(Loaded) + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<String>();
        let spawned = std::thread::Builder::new()
            .name("icons".into())
            .spawn(move || {
                for name in rx {
                    let raw = find_icon(&name, theme.as_deref(), px.min(u16::MAX as u32) as u16)
                        .and_then(|p| load_icon(&p, px));
                    on_loaded((name, raw));
                }
            });
        if let Err(err) = spawned {
            tracing::warn!("launcher: cannot start the icon thread: {err}");
        }
        Self { tx }
    }

    pub fn request(&self, name: String) {
        let _ = self.tx.send(name);
    }
}

/// Icon theme name from `gtk-3.0/settings.ini` via the icons crate, if any.
pub fn default_theme() -> Option<String> {
    freedesktop_icons::default_theme_gtk()
}

/// Ready-to-draw icons under a byte budget. Misses are remembered too (as `None`) so a
/// theme without that icon is not searched again on every keystroke.
pub struct IconCache {
    cache: LruCache<String, Option<Image>>,
    pending: HashSet<String>,
}

const NEGATIVE_BYTES: usize = 64;

impl IconCache {
    pub fn new(budget: usize) -> Self {
        Self {
            cache: LruCache::new("launcher-icons", budget),
            pending: HashSet::new(),
        }
    }

    /// `Some(Some(icon))` cached, `Some(None)` known missing, `None` not loaded yet.
    pub fn get(&mut self, name: &str) -> Option<Option<Image>> {
        self.cache.get(&name.to_string())
    }

    /// True when `name` needs a load request (and marks it requested).
    pub fn should_request(&mut self, name: &str) -> bool {
        !self.cache.contains(&name.to_string()) && self.pending.insert(name.to_string())
    }

    /// Stores a finished load. Returns the image when one could be built.
    pub fn store(&mut self, name: String, raw: Option<RawIcon>) -> Option<Image> {
        self.pending.remove(&name);
        let (image, bytes) = match raw {
            Some(r) => {
                let bytes = r.bytes();
                (Image::from_rgba(r.w, r.h, r.rgba), bytes)
            }
            None => (None, NEGATIVE_BYTES),
        };
        self.cache
            .insert(name, image.clone(), bytes.max(NEGATIVE_BYTES));
        image
    }

    /// Drops everything (icon theme change).
    pub fn clear(&mut self) {
        self.cache.clear();
        self.pending.clear();
    }

    pub fn stats(&self) -> CacheStats {
        self.cache.stats()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, px: [u8; 4]) -> RawIcon {
        RawIcon {
            w,
            h,
            rgba: px.iter().copied().cycle().take((w * h * 4) as usize).collect(),
        }
    }

    #[test]
    fn fit_keeps_aspect_and_color() {
        let big = solid(96, 48, [200, 100, 50, 255]);
        let small = fit(&big, 24);
        assert_eq!((small.w, small.h), (24, 12));
        assert_eq!(&small.rgba[..4], &[200, 100, 50, 255]);
        let up = fit(&solid(16, 16, [10, 20, 30, 255]), 32);
        assert_eq!((up.w, up.h), (32, 32));
        assert_eq!(&up.rgba[..4], &[10, 20, 30, 255]);
        assert_eq!(fit(&big, 96), big);
    }

    #[test]
    fn fit_averages_without_darkening_transparent_edges() {
        // Left half opaque white, right half fully transparent black.
        let mut raw = solid(4, 1, [0, 0, 0, 0]);
        for x in 0..2 {
            raw.rgba[x * 4..x * 4 + 4].copy_from_slice(&[255, 255, 255, 255]);
        }
        let out = fit(&raw, 1);
        assert_eq!((out.w, out.h), (1, 1));
        // Half coverage, but the color stays white rather than gray.
        assert_eq!(&out.rgba[..3], &[255, 255, 255]);
        assert!((120..=135).contains(&out.rgba[3]), "{}", out.rgba[3]);
    }

    #[test]
    fn cache_stores_negative_results_and_respects_the_budget() {
        let mut c = IconCache::new(10_000);
        assert!(c.get("a").is_none());
        assert!(c.should_request("a"));
        assert!(!c.should_request("a"), "already requested");
        assert!(c.store("a".into(), None).is_none());
        assert!(matches!(c.get("a"), Some(None)));
        assert!(!c.should_request("a"), "a known miss is not retried");

        // 32x32x4 = 4096 bytes: only two fit in 10_000.
        for n in ["x", "y", "z"] {
            assert!(c.should_request(n));
            assert!(c.store(n.into(), Some(solid(32, 32, [1, 2, 3, 255]))).is_some());
        }
        let s = c.stats();
        assert!(s.bytes <= 10_000, "{s:?}");
        assert!(s.evictions > 0);
        assert!(c.get("z").is_some());
        c.clear();
        assert!(c.get("z").is_none());
    }

    #[test]
    fn png_round_trip_through_the_decoder() {
        let dir = std::env::temp_dir().join(format!("aurora-launcher-icon-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("i.png");
        {
            let file = std::fs::File::create(&path).unwrap();
            let mut enc = png::Encoder::new(file, 2, 2);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[255, 0, 0, 255].repeat(4)).unwrap();
        }
        let raw = load_icon(&path, 4).unwrap();
        assert_eq!((raw.w, raw.h), (4, 4));
        assert_eq!(&raw.rgba[..4], &[255, 0, 0, 255]);
        assert!(load_icon(&dir.join("missing.png"), 4).is_none());
        assert!(find_icon(path.to_str().unwrap(), None, 4).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn svg_renders() {
        let dir = std::env::temp_dir().join(format!("aurora-launcher-svg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("i.svg");
        std::fs::write(
            &path,
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="#00ff00"/></svg>"##,
        )
        .unwrap();
        let raw = load_icon(&path, 20).unwrap();
        assert_eq!((raw.w, raw.h), (20, 20));
        assert_eq!(&raw.rgba[..4], &[0, 255, 0, 255]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
