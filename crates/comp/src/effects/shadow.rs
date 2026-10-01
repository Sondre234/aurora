//! Window shadows and rounded borders, both drawn as pixel shader elements behind the window
//! surface. Elements live in a per-window `DecoCache`: the canvas and uniforms are rewritten
//! (which bumps the commit counter and so repaints) only when an input actually changed, so a
//! steady window adds no damage. The shaders are unrun so far.
use smithay::{
    backend::renderer::{
        element::Kind,
        gles::{GlesPixelProgram, Uniform, UniformName, UniformType, element::PixelShaderElement},
    },
    utils::{Logical, Rectangle},
};

/// Soft box shadow of a rounded rectangle: an erf-like falloff over `sigma` either side of the
/// edge. `rect` is the shadowing rectangle in logical pixels relative to the canvas.
pub const SHADOW_SHADER: &str = r#"
precision highp float;
uniform float alpha;
uniform vec2 size;
varying vec2 v_coords;

uniform vec4 color;
uniform vec4 rect;
uniform float radius;
uniform float sigma;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

void main() {
    vec2 p = v_coords * size;
    vec2 half_size = rect.zw * 0.5;
    vec2 q = abs(p - rect.xy - half_size) - (half_size - vec2(radius));
    float d = length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - radius;
    float s = 1.0 - smoothstep(-sigma, sigma, d);
    float a = color.a * s;
    vec4 c = vec4(color.rgb * a, a) * alpha;

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        c = vec4(0.0, 0.2, 0.0, 0.2) + c * 0.8;
#endif

    gl_FragColor = c;
}
"#;

/// A ring that follows the rounding: the outer edge has radius `radius + bw`, the inner edge
/// `radius`, so the window's rounded corner sits exactly inside it. The canvas is the window
/// geometry grown by `bw`; `aa` is the size of one physical pixel in logical pixels.
pub const BORDER_SHADER: &str = r#"
precision highp float;
uniform float alpha;
uniform vec2 size;
varying vec2 v_coords;

uniform vec4 color;
uniform float bw;
uniform float radius;
uniform float aa;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

float dist(vec2 p, vec2 loc, vec2 rsize, float r) {
    vec2 half_size = rsize * 0.5;
    vec2 q = abs(p - loc - half_size) - (half_size - vec2(r));
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}

void main() {
    vec2 p = v_coords * size;
    float outer = dist(p, vec2(0.0), size, radius + bw);
    float inner = dist(p, vec2(bw), size - vec2(2.0 * bw), radius);
    float cover = clamp(0.5 - outer / aa, 0.0, 1.0) * clamp(0.5 + inner / aa, 0.0, 1.0);
    float a = color.a * cover;
    vec4 c = vec4(color.rgb * a, a) * alpha;

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        c = vec4(0.0, 0.2, 0.0, 0.2) + c * 0.8;
#endif

    gl_FragColor = c;
}
"#;

pub fn shadow_uniform_names() -> Vec<UniformName<'static>> {
    vec![
        UniformName::new("color", UniformType::_4f),
        UniformName::new("rect", UniformType::_4f),
        UniformName::new("radius", UniformType::_1f),
        UniformName::new("sigma", UniformType::_1f),
    ]
}

pub fn border_uniform_names() -> Vec<UniformName<'static>> {
    vec![
        UniformName::new("color", UniformType::_4f),
        UniformName::new("bw", UniformType::_1f),
        UniformName::new("radius", UniformType::_1f),
        UniformName::new("aa", UniformType::_1f),
    ]
}

/// How far below the window the shadow is shifted.
pub fn shadow_offset_y(blur: i32) -> i32 {
    blur / 3
}

/// The shadow canvas for a window at `geo`: the geometry grown by the blur radius on every
/// side plus the offset, so the falloff never hits the canvas edge. `None` when there is
/// nothing to draw.
pub fn shadow_area(geo: Rectangle<i32, Logical>, blur: i32) -> Option<Rectangle<i32, Logical>> {
    if blur <= 0 || geo.size.w <= 0 || geo.size.h <= 0 {
        return None;
    }
    let grow = blur + shadow_offset_y(blur);
    Some(Rectangle::new(
        (geo.loc.x - grow, geo.loc.y - grow).into(),
        (geo.size.w + 2 * grow, geo.size.h + 2 * grow).into(),
    ))
}

/// How far a shadow reaches beyond the window on any side, for culling.
pub fn shadow_reach(blur: i32) -> i32 {
    if blur <= 0 {
        0
    } else {
        blur + shadow_offset_y(blur)
    }
}

/// CPU twin of the shadow falloff for signed distance `d` (negative inside).
#[cfg(test)]
pub fn shadow_falloff(d: f32, sigma: f32) -> f32 {
    let t = ((d + sigma) / (2.0 * sigma)).clamp(0.0, 1.0);
    1.0 - t * t * (3.0 - 2.0 * t)
}

/// The border canvas: the geometry grown by `bw`.
pub fn border_area(geo: Rectangle<i32, Logical>, bw: i32) -> Rectangle<i32, Logical> {
    Rectangle::new(
        (geo.loc.x - bw, geo.loc.y - bw).into(),
        (geo.size.w + 2 * bw, geo.size.h + 2 * bw).into(),
    )
}

type Rgba = [f32; 4];

#[derive(PartialEq)]
struct ShadowKey {
    area: Rectangle<i32, Logical>,
    blur: i32,
    rounding: i32,
    color: Rgba,
}

#[derive(PartialEq)]
struct BorderKey {
    area: Rectangle<i32, Logical>,
    bw: i32,
    rounding: i32,
    color: Rgba,
    aa_bits: u32,
}

/// Retained shadow and border elements of one window.
#[derive(Default)]
pub struct DecoCache {
    shadow: Option<(ShadowKey, PixelShaderElement)>,
    border: Option<(BorderKey, PixelShaderElement)>,
}

impl DecoCache {
    pub fn clear_shadow(&mut self) {
        self.shadow = None;
    }

    /// The shadow element for a window at `geo` (output-relative logical), rewritten only when
    /// an input changed.
    pub fn shadow(
        &mut self,
        program: &GlesPixelProgram,
        geo: Rectangle<i32, Logical>,
        rounding: i32,
        blur: i32,
        color: Rgba,
    ) -> Option<PixelShaderElement> {
        let Some(area) = shadow_area(geo, blur) else {
            self.shadow = None;
            return None;
        };
        let key = ShadowKey {
            area,
            blur,
            rounding,
            color,
        };
        match &mut self.shadow {
            Some((old, element)) if *old == key => return Some(element.clone()),
            Some((old, element)) => {
                element.resize(area, None);
                element.update_uniforms(shadow_uniforms(&key));
                *old = key;
            }
            None => {
                let element = PixelShaderElement::new(
                    program.clone(),
                    area,
                    None,
                    1.0,
                    shadow_uniforms(&key),
                    Kind::Unspecified,
                );
                self.shadow = Some((key, element));
            }
        }
        self.shadow.as_ref().map(|(_, e)| e.clone())
    }

    /// The rounded border ring around a window at `geo`; `aa` is one physical pixel in
    /// logical pixels.
    pub fn border(
        &mut self,
        program: &GlesPixelProgram,
        geo: Rectangle<i32, Logical>,
        bw: i32,
        rounding: i32,
        color: Rgba,
        aa: f32,
    ) -> Option<PixelShaderElement> {
        if bw <= 0 {
            self.border = None;
            return None;
        }
        let key = BorderKey {
            area: border_area(geo, bw),
            bw,
            rounding,
            color,
            aa_bits: aa.to_bits(),
        };
        match &mut self.border {
            Some((old, element)) if *old == key => return Some(element.clone()),
            Some((old, element)) => {
                element.resize(key.area, None);
                element.update_uniforms(border_uniforms(&key));
                *old = key;
            }
            None => {
                let element = PixelShaderElement::new(
                    program.clone(),
                    key.area,
                    None,
                    1.0,
                    border_uniforms(&key),
                    Kind::Unspecified,
                );
                self.border = Some((key, element));
            }
        }
        self.border.as_ref().map(|(_, e)| e.clone())
    }
}

fn shadow_uniforms(key: &ShadowKey) -> Vec<Uniform<'static>> {
    let off = shadow_offset_y(key.blur);
    let grow = key.blur + off;
    let rect = (
        grow as f32,
        (grow + off) as f32,
        (key.area.size.w - 2 * grow) as f32,
        (key.area.size.h - 2 * grow) as f32,
    );
    let [r, g, b, a] = key.color;
    vec![
        Uniform::new("color", (r, g, b, a)),
        Uniform::new("rect", rect),
        Uniform::new("radius", key.rounding as f32),
        Uniform::new("sigma", (key.blur as f32 * 0.5).max(0.5)),
    ]
}

fn border_uniforms(key: &BorderKey) -> Vec<Uniform<'static>> {
    let [r, g, b, a] = key.color;
    vec![
        Uniform::new("color", (r, g, b, a)),
        Uniform::new("bw", key.bw as f32),
        Uniform::new("radius", key.rounding as f32),
        Uniform::new("aa", f32::from_bits(key.aa_bits)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shadow_area_grows_by_blur_and_offset() {
        let geo = Rectangle::new((100, 50).into(), (400, 300).into());
        let area = shadow_area(geo, 21).unwrap();
        // blur 21, offset 7.
        assert_eq!(area, Rectangle::new((72, 22).into(), (456, 356).into()));
        assert!(shadow_area(geo, 0).is_none());
        assert!(shadow_area(Rectangle::new((0, 0).into(), (0, 10).into()), 5).is_none());
    }

    #[test]
    fn shadow_rect_stays_inside_canvas() {
        let geo = Rectangle::new((0, 0).into(), (200, 100).into());
        let blur = 20;
        let area = shadow_area(geo, blur).unwrap();
        let grow = blur + shadow_offset_y(blur);
        // Shifted rect bottom plus a full blur still fits.
        assert!(grow + shadow_offset_y(blur) + geo.size.h + blur <= area.size.h);
    }

    #[test]
    fn falloff_is_monotonic_and_bounded() {
        let sigma = 10.0;
        assert_eq!(shadow_falloff(-sigma, sigma), 1.0);
        assert_eq!(shadow_falloff(sigma, sigma), 0.0);
        assert!((shadow_falloff(0.0, sigma) - 0.5).abs() < 1e-6);
        let mut last = 1.0;
        for i in -12..=12 {
            let v = shadow_falloff(i as f32, sigma);
            assert!(v <= last + 1e-6);
            last = v;
        }
    }

    #[test]
    fn border_area_grows_by_width() {
        let geo = Rectangle::new((10, 10).into(), (100, 80).into());
        assert_eq!(
            border_area(geo, 2),
            Rectangle::new((8, 8).into(), (104, 84).into())
        );
    }
}
