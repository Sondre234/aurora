//! Toast images: `image-data`, `image-path` and `app_icon`, in that order of preference.
//!
//! Deliberately small: PNG only (no SVG, no icon theme inheritance). An icon *name* is
//! looked up as `<dir>/icons/hicolor/<size>/apps/<name>.png` in the XDG data dirs and as
//! `/usr/share/pixmaps/<name>.png`. Loading runs on the main thread when a toast is
//! built; files over [`MAX_FILE_BYTES`] or larger than [`MAX_IMAGE_DIM`] are refused, so
//! a decode costs a few milliseconds at most.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use aurora_ui::Image;

use crate::hints::{Hints, MAX_IMAGE_DIM};

pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

const SIZES: [&str; 5] = ["64x64", "48x48", "128x128", "32x32", "256x256"];

/// Where an `image-path` / `app_icon` string may point, best first.
pub fn candidate_paths(name: &str, data_dirs: &[PathBuf]) -> Vec<PathBuf> {
    if name.is_empty() {
        return Vec::new();
    }
    if let Some(p) = name.strip_prefix("file://") {
        return vec![PathBuf::from(percent_decode(p))];
    }
    if name.starts_with('/') {
        return vec![PathBuf::from(name)];
    }
    // A bare name only: no traversal out of the icon directories.
    if name.contains('/') || name.contains("..") {
        return Vec::new();
    }
    let mut out = Vec::new();
    for dir in data_dirs {
        for size in SIZES {
            out.push(
                dir.join("icons/hicolor")
                    .join(size)
                    .join("apps")
                    .join(format!("{name}.png")),
            );
        }
    }
    out.push(PathBuf::from(format!("/usr/share/pixmaps/{name}.png")));
    out
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(hex) = s.get(i + 1..i + 3)
            && let Ok(v) = u8::from_str_radix(hex, 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `$XDG_DATA_HOME` and `$XDG_DATA_DIRS` with their spec defaults.
pub fn data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(h) => dirs.push(PathBuf::from(h)),
        None => {
            if let Some(home) = std::env::var_os("HOME") {
                dirs.push(PathBuf::from(home).join(".local/share"));
            }
        }
    }
    let system = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    dirs.extend(
        system
            .split(':')
            .filter(|d| !d.is_empty())
            .map(PathBuf::from),
    );
    dirs
}

/// Decodes a PNG file into a UI image.
pub fn load_png(path: &Path) -> Option<Image> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let mut decoder = png::Decoder::new(BufReader::new(File::open(path).ok()?));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let (w, h) = {
        let i = reader.info();
        (i.width, i.height)
    };
    if w == 0 || h == 0 || w > MAX_IMAGE_DIM || h > MAX_IMAGE_DIM {
        return None;
    }
    let mut buf = vec![0; reader.output_buffer_size()?];
    let frame = reader.next_frame(&mut buf).ok()?;
    let data = &buf[..frame.buffer_size()];
    let rgba: Vec<u8> = match frame.color_type {
        png::ColorType::Rgba => data.to_vec(),
        png::ColorType::Rgb => data
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => data
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => data.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    Image::from_rgba(frame.width, frame.height, rgba)
}

fn load_named(name: &str, dirs: &[PathBuf]) -> Option<Image> {
    candidate_paths(name, dirs).iter().find_map(|p| load_png(p))
}

/// The image a toast shows: `image-data`, then `image-path`, then `app_icon`.
pub fn resolve(hints: &Hints, app_icon: &str) -> Option<Image> {
    if let Some(raw) = &hints.image
        && let Some((w, h, rgba)) = raw.to_rgba()
        && let Some(img) = Image::from_rgba(w, h, rgba)
    {
        return Some(img);
    }
    let dirs = data_dirs();
    if let Some(p) = &hints.image_path
        && let Some(img) = load_named(p, &dirs)
    {
        return Some(img);
    }
    load_named(app_icon, &dirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates() {
        let dirs = vec![PathBuf::from("/d1"), PathBuf::from("/d2")];
        assert!(candidate_paths("", &dirs).is_empty());
        assert_eq!(
            candidate_paths("/abs/i.png", &dirs),
            vec![PathBuf::from("/abs/i.png")]
        );
        assert_eq!(
            candidate_paths("file:///a%20b/i.png", &dirs),
            vec![PathBuf::from("/a b/i.png")]
        );
        assert!(candidate_paths("../evil", &dirs).is_empty());
        assert!(candidate_paths("a/b", &dirs).is_empty());
        let c = candidate_paths("firefox", &dirs);
        assert_eq!(
            c[0],
            PathBuf::from("/d1/icons/hicolor/64x64/apps/firefox.png")
        );
        assert_eq!(c.len(), 2 * SIZES.len() + 1);
        assert_eq!(
            c.last().unwrap(),
            &PathBuf::from("/usr/share/pixmaps/firefox.png")
        );
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a%2Fb%zz%4"), "a/b%zz%4");
        assert_eq!(percent_decode("plain"), "plain");
    }

    fn write_png(path: &Path, color: png::ColorType, w: u32, h: u32, data: &[u8]) {
        let file = File::create(path).unwrap();
        let mut enc = png::Encoder::new(file, w, h);
        enc.set_color(color);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(data).unwrap();
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aurora-notifd-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn png_decoding() {
        let dir = temp_dir("png");
        let rgba = dir.join("rgba.png");
        write_png(
            &rgba,
            png::ColorType::Rgba,
            2,
            1,
            &[1, 2, 3, 255, 4, 5, 6, 255],
        );
        let img = load_png(&rgba).unwrap();
        assert_eq!((img.width(), img.height()), (2, 1));
        let gray = dir.join("gray.png");
        write_png(&gray, png::ColorType::Grayscale, 1, 2, &[10, 20]);
        assert_eq!(load_png(&gray).unwrap().height(), 2);
        let rgb = dir.join("rgb.png");
        write_png(&rgb, png::ColorType::Rgb, 1, 1, &[9, 8, 7]);
        assert!(load_png(&rgb).is_some());
        assert!(load_png(&dir.join("missing.png")).is_none());
        let junk = dir.join("junk.png");
        std::fs::write(&junk, b"not a png").unwrap();
        assert!(load_png(&junk).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_prefers_image_data() {
        use crate::hints::RawImage;
        let hints = Hints {
            image: Some(std::sync::Arc::new(RawImage {
                width: 1,
                height: 1,
                rowstride: 3,
                has_alpha: false,
                bits_per_sample: 8,
                channels: 3,
                data: vec![1, 2, 3],
            })),
            image_path: Some("/nonexistent.png".into()),
            ..Hints::default()
        };
        assert!(resolve(&hints, "whatever").is_some());
        assert!(resolve(&Hints::default(), "").is_none());
    }
}
