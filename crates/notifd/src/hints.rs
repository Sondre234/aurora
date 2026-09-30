//! Hint parsing. The D-Bus layer converts `a{sv}` into [`Hint`] values (which carry no
//! zbus types, so this file is unit-testable) and [`Hints::parse`] extracts what the
//! daemon understands: urgency, the three image hints, category, desktop entry and the
//! `resident` / `transient` flags. Unknown hints are ignored.

use std::collections::HashMap;
use std::sync::Arc;

/// Images larger than this in either dimension are refused (toasts show them at ~48 px).
pub const MAX_IMAGE_DIM: u32 = 2048;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Urgency {
    Low,
    #[default]
    Normal,
    Critical,
}

impl Urgency {
    /// Spec values 0, 1, 2; anything else is treated as normal.
    pub fn from_level(v: i64) -> Self {
        match v {
            0 => Urgency::Low,
            2 => Urgency::Critical,
            _ => Urgency::Normal,
        }
    }
}

/// `(iiibiiay)` from the `image-data` hint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawImage {
    pub width: i32,
    pub height: i32,
    pub rowstride: i32,
    pub has_alpha: bool,
    pub bits_per_sample: i32,
    pub channels: i32,
    pub data: Vec<u8>,
}

impl RawImage {
    /// Straight RGBA8, or `None` when the header and data disagree or the format is not
    /// 8-bit RGB(A).
    pub fn to_rgba(&self) -> Option<(u32, u32, Vec<u8>)> {
        let (w, h) = (
            u32::try_from(self.width).ok()?,
            u32::try_from(self.height).ok()?,
        );
        if w == 0 || h == 0 || w > MAX_IMAGE_DIM || h > MAX_IMAGE_DIM {
            return None;
        }
        let ch = usize::try_from(self.channels).ok()?;
        if self.bits_per_sample != 8 || !(ch == 3 || ch == 4) || (ch == 4) != self.has_alpha {
            return None;
        }
        let stride = usize::try_from(self.rowstride).ok()?;
        let row = w as usize * ch;
        if stride < row {
            return None;
        }
        // The last row may omit its padding.
        let needed = stride * (h as usize - 1) + row;
        if self.data.len() < needed {
            return None;
        }
        let mut out = Vec::with_capacity(w as usize * h as usize * 4);
        for y in 0..h as usize {
            let line = &self.data[y * stride..y * stride + row];
            for px in line.chunks_exact(ch) {
                out.extend_from_slice(&px[..3]);
                out.push(if ch == 4 { px[3] } else { 255 });
            }
        }
        Some((w, h, out))
    }
}

/// A hint value reduced to the shapes we care about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hint {
    Bool(bool),
    Int(i64),
    Str(String),
    Image(RawImage),
    Other,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hints {
    pub urgency: Urgency,
    /// `image-data`, else the deprecated `image_data`, else `icon_data`.
    pub image: Option<Arc<RawImage>>,
    /// `image-path` (a file path, `file://` URI or icon name).
    pub image_path: Option<String>,
    pub category: Option<String>,
    pub desktop_entry: Option<String>,
    /// Stays after an action is invoked.
    pub resident: bool,
    pub transient: bool,
}

impl Hints {
    pub fn parse(map: &HashMap<String, Hint>) -> Self {
        let mut h = Hints::default();
        if let Some(v) = map.get("urgency") {
            h.urgency = match v {
                Hint::Int(n) => Urgency::from_level(*n),
                Hint::Bool(b) => Urgency::from_level(*b as i64),
                _ => Urgency::Normal,
            };
        }
        for key in ["image-data", "image_data", "icon_data"] {
            if let Some(Hint::Image(img)) = map.get(key) {
                h.image = Some(Arc::new(img.clone()));
                break;
            }
        }
        for key in ["image-path", "image_path"] {
            if let Some(Hint::Str(s)) = map.get(key)
                && !s.is_empty()
            {
                h.image_path = Some(s.clone());
                break;
            }
        }
        h.category = string(map, "category");
        h.desktop_entry = string(map, "desktop-entry");
        h.resident = flag(map, "resident");
        h.transient = flag(map, "transient");
        h
    }
}

fn string(map: &HashMap<String, Hint>, key: &str) -> Option<String> {
    match map.get(key) {
        Some(Hint::Str(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn flag(map: &HashMap<String, Hint>, key: &str) -> bool {
    match map.get(key) {
        Some(Hint::Bool(b)) => *b,
        Some(Hint::Int(n)) => *n != 0,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: Vec<(&str, Hint)>) -> HashMap<String, Hint> {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    fn img(w: i32, h: i32, stride: i32, alpha: bool, data: Vec<u8>) -> RawImage {
        RawImage {
            width: w,
            height: h,
            rowstride: stride,
            has_alpha: alpha,
            bits_per_sample: 8,
            channels: if alpha { 4 } else { 3 },
            data,
        }
    }

    #[test]
    fn urgency_levels() {
        let u = |v| Hints::parse(&map(vec![("urgency", v)])).urgency;
        assert_eq!(u(Hint::Int(0)), Urgency::Low);
        assert_eq!(u(Hint::Int(1)), Urgency::Normal);
        assert_eq!(u(Hint::Int(2)), Urgency::Critical);
        assert_eq!(u(Hint::Int(9)), Urgency::Normal);
        assert_eq!(u(Hint::Str("x".into())), Urgency::Normal);
        assert_eq!(Hints::parse(&HashMap::new()).urgency, Urgency::Normal);
    }

    #[test]
    fn strings_and_flags() {
        let h = Hints::parse(&map(vec![
            ("category", Hint::Str("email.arrived".into())),
            ("desktop-entry", Hint::Str("".into())),
            ("resident", Hint::Bool(true)),
            ("transient", Hint::Int(0)),
            ("image-path", Hint::Str("/tmp/a.png".into())),
            ("x-unknown", Hint::Other),
        ]));
        assert_eq!(h.category.as_deref(), Some("email.arrived"));
        assert_eq!(h.desktop_entry, None);
        assert!(h.resident && !h.transient);
        assert_eq!(h.image_path.as_deref(), Some("/tmp/a.png"));
    }

    #[test]
    fn image_hint_precedence() {
        let a = img(1, 1, 3, false, vec![1, 2, 3]);
        let b = img(1, 1, 3, false, vec![4, 5, 6]);
        let h = Hints::parse(&map(vec![
            ("icon_data", Hint::Image(b.clone())),
            ("image-data", Hint::Image(a.clone())),
        ]));
        assert_eq!(h.image.as_deref(), Some(&a));
        let h = Hints::parse(&map(vec![("icon_data", Hint::Image(b.clone()))]));
        assert_eq!(h.image.as_deref(), Some(&b));
    }

    #[test]
    fn rgb_converts_with_opaque_alpha() {
        let (w, h, px) = img(2, 1, 6, false, vec![1, 2, 3, 4, 5, 6])
            .to_rgba()
            .unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(px, vec![1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn rowstride_padding_is_skipped() {
        // 1x2 RGBA with stride 8: each row has 4 bytes of padding, the last row omits it.
        let data = vec![1, 2, 3, 4, 9, 9, 9, 9, 5, 6, 7, 8];
        let (_, _, px) = img(1, 2, 8, true, data).to_rgba().unwrap();
        assert_eq!(px, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn bad_images_are_refused() {
        assert!(
            img(2, 1, 6, false, vec![0; 5]).to_rgba().is_none(),
            "short data"
        );
        assert!(
            img(2, 1, 5, false, vec![0; 6]).to_rgba().is_none(),
            "stride too small"
        );
        assert!(img(0, 1, 3, false, vec![]).to_rgba().is_none());
        assert!(img(-1, 1, 3, false, vec![0; 3]).to_rgba().is_none());
        assert!(
            img(1, 1, 3, true, vec![0; 3]).to_rgba().is_none(),
            "alpha mismatch"
        );
        let mut sixteen = img(1, 1, 3, false, vec![0; 3]);
        sixteen.bits_per_sample = 16;
        assert!(sixteen.to_rgba().is_none());
        let huge = img(MAX_IMAGE_DIM as i32 + 1, 1, 0, false, vec![]);
        assert!(huge.to_rgba().is_none());
    }
}
