//! Rounded window corners. A custom texture program clips the window's main surface to a
//! rounded rectangle (the window geometry), applied through a wrapper element because the
//! pinned Smithay has no clipped surface element. The shader is unrun so far: it was written
//! against the program contract in Smithay's `compile_custom_texture_shader` docs.
use smithay::{
    backend::renderer::{
        Texture,
        element::{
            Element, Id, Kind, RenderElement,
            surface::{WaylandSurfaceRenderElement, WaylandSurfaceTexture},
        },
        gles::{
            GlesError, GlesFrame, GlesRenderer, GlesTexProgram, Uniform, UniformName, UniformType,
        },
        utils::{CommitCounter, DamageSet, OpaqueRegions},
    },
    utils::{Buffer, Physical, Point, Rectangle, Scale, Transform, user_data::UserDataMap},
};

/// Clips to a rounded rectangle. `rect` is in pixels relative to the drawn element, `src` and
/// `tex_size` undo Smithay's texture matrix (source crop, `flip` for y-inverted textures) so
/// the fragment knows its position inside the element.
pub const SHADER: &str = r#"
//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

uniform vec2 tex_size;
uniform vec4 src;
uniform vec2 dst_size;
uniform vec4 rect;
uniform float radius;
uniform float flip;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

void main() {
    vec2 v = v_coords;
    if (flip > 0.5) {
        v.y = -v.y;
    }
    vec2 p = (v * tex_size - src.xy) / src.zw * dst_size;
    vec2 half_size = rect.zw * 0.5;
    vec2 q = abs(p - rect.xy - half_size) - (half_size - vec2(radius));
    float d = length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - radius;
    float cover = clamp(0.5 - d, 0.0, 1.0);

    vec4 color = texture2D(tex, v_coords);
#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0);
#endif
    color = color * alpha * cover;

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
"#;

pub fn uniform_names() -> Vec<UniformName<'static>> {
    vec![
        UniformName::new("tex_size", UniformType::_2f),
        UniformName::new("src", UniformType::_4f),
        UniformName::new("dst_size", UniformType::_2f),
        UniformName::new("rect", UniformType::_4f),
        UniformName::new("radius", UniformType::_1f),
        UniformName::new("flip", UniformType::_1f),
    ]
}

/// Radius that fits a `w` x `h` rectangle: at most half of the short side, never negative.
pub fn clamp_radius(radius: i32, w: i32, h: i32) -> i32 {
    radius.min(w / 2).min(h / 2).max(0)
}

/// The four `r` x `r` squares at the corners of `rect`, which a rounded window does not fully
/// cover.
pub fn corner_squares<K>(rect: Rectangle<i32, K>, r: i32) -> [Rectangle<i32, K>; 4] {
    let (x, y, w, h) = (rect.loc.x, rect.loc.y, rect.size.w, rect.size.h);
    [
        Rectangle::new((x, y).into(), (r, r).into()),
        Rectangle::new((x + w - r, y).into(), (r, r).into()),
        Rectangle::new((x, y + h - r).into(), (r, r).into()),
        Rectangle::new((x + w - r, y + h - r).into(), (r, r).into()),
    ]
}

/// Signed distance from `p` to a rounded rectangle at `loc` with `size` and corner `radius`:
/// negative inside. The shaders compute the same thing.
#[cfg(test)]
pub fn rounded_distance(p: (f32, f32), loc: (f32, f32), size: (f32, f32), radius: f32) -> f32 {
    let (hx, hy) = (size.0 * 0.5, size.1 * 0.5);
    let qx = (p.0 - loc.0 - hx).abs() - (hx - radius);
    let qy = (p.1 - loc.1 - hy).abs() - (hy - radius);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - radius
}

/// Pixel coverage for a signed distance, one pixel wide antialiasing.
#[cfg(test)]
pub fn coverage(distance: f32) -> f32 {
    (0.5 - distance).clamp(0.0, 1.0)
}

/// A window surface clipped to `geo` (output-space physical pixels) with rounded corners.
pub struct RoundedSurface {
    inner: WaylandSurfaceRenderElement<GlesRenderer>,
    program: GlesTexProgram,
    geo: Rectangle<i32, Physical>,
    radius: i32,
}

impl RoundedSurface {
    /// Gives the element back when it cannot be rounded (rotated buffer, solid colour, no
    /// radius): the caller then draws it plain.
    pub fn new(
        inner: WaylandSurfaceRenderElement<GlesRenderer>,
        program: GlesTexProgram,
        geo: Rectangle<i32, Physical>,
        radius: i32,
    ) -> Result<Self, Box<WaylandSurfaceRenderElement<GlesRenderer>>> {
        let rounded = radius > 0
            && inner.transform() == Transform::Normal
            && matches!(inner.texture(), WaylandSurfaceTexture::Texture(_));
        if !rounded {
            return Err(Box::new(inner));
        }
        Ok(Self {
            inner,
            program,
            geo,
            radius: clamp_radius(radius, geo.size.w, geo.size.h),
        })
    }
}

impl Element for RoundedSurface {
    fn id(&self) -> &Id {
        self.inner.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }

    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        self.inner.location(scale)
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }

    fn transform(&self) -> Transform {
        self.inner.transform()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }

    /// The surface's opaque regions minus the four corner squares, so the damage tracker never
    /// skips what lies behind a rounded corner.
    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        let regions = self.inner.opaque_regions(scale);
        if regions.is_empty() {
            return regions;
        }
        let origin = self.inner.geometry(scale).loc;
        let mut corners = corner_squares(self.geo, self.radius);
        for c in &mut corners {
            c.loc -= origin;
        }
        let rects = Rectangle::subtract_rects_many(regions.iter().copied(), corners);
        OpaqueRegions::from_slice(&rects)
    }

    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }

    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl RenderElement<GlesRenderer> for RoundedSurface {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        let WaylandSurfaceTexture::Texture(texture) = self.inner.texture() else {
            return self
                .inner
                .draw(frame, src, dst, damage, opaque_regions, cache);
        };
        let tex = texture.size();
        let rect = self.geo.loc - dst.loc;
        let uniforms = vec![
            Uniform::new("tex_size", (tex.w as f32, tex.h as f32)),
            Uniform::new(
                "src",
                (
                    src.loc.x as f32,
                    src.loc.y as f32,
                    src.size.w as f32,
                    src.size.h as f32,
                ),
            ),
            Uniform::new("dst_size", (dst.size.w as f32, dst.size.h as f32)),
            Uniform::new(
                "rect",
                (
                    rect.x as f32,
                    rect.y as f32,
                    self.geo.size.w as f32,
                    self.geo.size.h as f32,
                ),
            ),
            Uniform::new("radius", self.radius as f32),
            Uniform::new("flip", if texture.is_y_inverted() { 1.0f32 } else { 0.0 }),
        ];
        frame.override_default_tex_program(self.program.clone(), uniforms);
        let result = self
            .inner
            .draw(frame, src, dst, damage, opaque_regions, cache);
        frame.clear_tex_program_override();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radius_fits_the_rectangle() {
        assert_eq!(clamp_radius(10, 100, 100), 10);
        assert_eq!(clamp_radius(50, 100, 20), 10);
        assert_eq!(clamp_radius(-3, 100, 100), 0);
    }

    #[test]
    fn corner_squares_sit_in_the_corners() {
        let r: Rectangle<i32, Physical> = Rectangle::new((10, 20).into(), (100, 50).into());
        let c = corner_squares(r, 8);
        assert_eq!(c[0], Rectangle::new((10, 20).into(), (8, 8).into()));
        assert_eq!(c[3], Rectangle::new((102, 62).into(), (8, 8).into()));
    }

    #[test]
    fn distance_is_negative_inside_and_clips_corners() {
        let d = |p| rounded_distance(p, (0.0, 0.0), (100.0, 50.0), 10.0);
        assert!(d((50.0, 25.0)) < -20.0);
        assert!(d((-1.0, 25.0)) > 0.0);
        assert_eq!(coverage(d((0.5, 0.5))), 0.0);
        assert_eq!(coverage(d((50.0, 0.5))), 1.0);
        assert_eq!(coverage(d((0.5, 25.0))), 1.0);
    }

    #[test]
    fn square_corners_when_radius_zero() {
        let d = rounded_distance((0.5, 0.5), (0.0, 0.0), (100.0, 50.0), 0.0);
        assert_eq!(coverage(d), 1.0);
    }
}
