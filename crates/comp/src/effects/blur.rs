//! Backdrop blur: dual-Kawase over offscreen textures.
//!
//! The scene builder records a `Request` where a translucent window or layer surface sits in
//! the front-to-back list; `apply` then renders everything behind that point into a texture
//! (a padded copy of the region, so edges sample real pixels), runs the down/up chain and
//! inserts a `BlurElement` at the recorded index. Requests are handled back to front so a
//! blur that sits behind another one is part of that one's backdrop.
//!
//! Cache discipline (docs/performance.md):
//! - owner: `Cache`, one per renderer, entries keyed by (output name, window or layer),
//! - invalidation: a signature of the region and of every element behind it (id, commit
//!   counter, geometry); the blur is recomputed only when it changes, so an idle desktop costs
//!   no GPU work here,
//! - budget: `BUDGET_BYTES` of VRAM, least recently used entries go first, entries unused for
//!   `MAX_IDLE` renders are swept, everything of an output is released when blur is off,
//! - metric: `effects: blur cache` debug lines with entries, bytes, hits and misses.
use std::{
    cell::RefCell,
    collections::{HashMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    rc::Rc,
};

use aurora_layout::WinId;
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, Color32F, Frame, Offscreen, Renderer, Texture,
            damage::OutputDamageTracker,
            element::{
                Element, Id, RenderElement,
                utils::{Relocate, RelocateRenderElement},
            },
            gles::{
                GlesError, GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture, Uniform,
                UniformName, UniformType,
            },
            utils::{CommitCounter, DamageSet},
        },
    },
    output::Output,
    reexports::wayland_server::backend::ObjectId,
    utils::{Buffer, Logical, Physical, Rectangle, Scale, Size, Transform, user_data::UserDataMap},
};

use crate::{
    backend::BACKGROUND,
    scene::{OutputElement, SceneFx},
};

/// VRAM the blur cache may hold, across outputs (docs/performance.md allows ~6 GB for all
/// caches).
pub const BUDGET_BYTES: usize = 1 << 30;
/// Renders an entry may go unused before it is dropped.
const MAX_IDLE: u64 = 1200;
/// Hard cap on the chain depth whatever the config says.
const MAX_PASSES: u32 = 6;
/// Lookups between two metric lines.
const REPORT_EVERY: u64 = 1024;

// ---------------------------------------------------------------------------------------
// Shaders

const DOWN: &str = "#version 100
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
#if defined(DEBUG_FLAGS)
uniform float tint;
#endif
uniform vec2 halfpixel;
uniform float offset;
void main() {
    vec2 o = halfpixel * offset;
    vec4 sum = texture2D(tex, v_coords) * 4.0;
    sum += texture2D(tex, v_coords - o);
    sum += texture2D(tex, v_coords + o);
    sum += texture2D(tex, v_coords + vec2(o.x, -o.y));
    sum += texture2D(tex, v_coords - vec2(o.x, -o.y));
    gl_FragColor = sum / 8.0 * alpha;
}
";

const UP: &str = "#version 100
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
#if defined(DEBUG_FLAGS)
uniform float tint;
#endif
uniform vec2 halfpixel;
uniform float offset;
void main() {
    vec2 o = halfpixel * offset;
    vec4 sum = texture2D(tex, v_coords + vec2(-o.x * 2.0, 0.0));
    sum += texture2D(tex, v_coords + vec2(-o.x, o.y)) * 2.0;
    sum += texture2D(tex, v_coords + vec2(0.0, o.y * 2.0));
    sum += texture2D(tex, v_coords + vec2(o.x, o.y)) * 2.0;
    sum += texture2D(tex, v_coords + vec2(o.x * 2.0, 0.0));
    sum += texture2D(tex, v_coords + vec2(o.x, -o.y)) * 2.0;
    sum += texture2D(tex, v_coords + vec2(0.0, -o.y * 2.0));
    sum += texture2D(tex, v_coords + vec2(-o.x, -o.y)) * 2.0;
    gl_FragColor = sum / 12.0 * alpha;
}
";

/// The two shader programs of the chain.
#[derive(Clone)]
pub struct BlurPrograms {
    down: GlesTexProgram,
    up: GlesTexProgram,
}

impl BlurPrograms {
    /// Compiles both programs; `None` (logged) when the driver rejects either, which turns
    /// blur off and nothing else.
    pub fn compile(renderer: &mut GlesRenderer) -> Option<Self> {
        let uniforms = [
            UniformName::new("halfpixel", UniformType::_2f),
            UniformName::new("offset", UniformType::_1f),
        ];
        let mut build = |name: &str, source: &str| {
            renderer
                .compile_custom_texture_shader(source, &uniforms)
                .inspect_err(|err| tracing::warn!(%err, "effects: blur {name} shader failed"))
                .ok()
        };
        Some(Self {
            down: build("down", DOWN)?,
            up: build("up", UP)?,
        })
    }
}

// ---------------------------------------------------------------------------------------
// Pure logic

/// Who a cached blur belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Owner {
    Win(WinId),
    Layer(ObjectId),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    output: String,
    owner: Owner,
}

struct Slot<V> {
    value: V,
    bytes: usize,
    used: u64,
}

/// Byte-budgeted LRU map. `clock` advances once per render (`tick`); `used` records when an
/// entry was last touched, so eviction order and idle sweeps share one notion of age.
struct Lru<K, V> {
    map: HashMap<K, Slot<V>>,
    budget: usize,
    bytes: usize,
    clock: u64,
}

impl<K: Hash + Eq + Clone, V> Lru<K, V> {
    fn new(budget: usize) -> Self {
        Self {
            map: HashMap::new(),
            budget,
            bytes: 0,
            clock: 0,
        }
    }

    fn tick(&mut self) {
        self.clock += 1;
    }

    fn len(&self) -> usize {
        self.map.len()
    }

    /// The entry, marked as used now.
    fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        let clock = self.clock;
        self.map.get_mut(key).map(|slot| {
            slot.used = clock;
            &mut slot.value
        })
    }

    /// Takes the entry out, to be updated and inserted again.
    fn take(&mut self, key: &K) -> Option<V> {
        let slot = self.map.remove(key)?;
        self.bytes -= slot.bytes;
        Some(slot.value)
    }

    /// Stores `value`, then evicts least recently used entries (never this one) until the
    /// budget holds. Returns what was evicted.
    fn insert(&mut self, key: K, value: V, bytes: usize) -> Vec<V> {
        self.take(&key);
        self.bytes += bytes;
        self.map.insert(
            key.clone(),
            Slot {
                value,
                bytes,
                used: self.clock,
            },
        );
        let mut evicted = Vec::new();
        while self.bytes > self.budget {
            let Some(victim) = self
                .map
                .iter()
                .filter(|(k, _)| **k != key)
                .min_by_key(|(_, slot)| slot.used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            evicted.extend(self.take(&victim));
        }
        evicted
    }

    /// Drops entries untouched for more than `max_idle` ticks.
    fn sweep_idle(&mut self, max_idle: u64) -> usize {
        let clock = self.clock;
        self.remove_where(|_, slot_used| clock.saturating_sub(slot_used) > max_idle)
    }

    /// Drops entries whose key matches; `f` gets the key and the entry's last use.
    fn remove_where(&mut self, mut f: impl FnMut(&K, u64) -> bool) -> usize {
        let doomed: Vec<K> = self
            .map
            .iter()
            .filter(|(k, slot)| f(k, slot.used))
            .map(|(k, _)| k.clone())
            .collect();
        for key in &doomed {
            self.take(key);
        }
        doomed.len()
    }
}

/// How far the blur reaches outside its region: the sum of the taps of every level.
fn reach(radius: u32, passes: u32) -> i32 {
    (radius.max(1) as i64 * (1i64 << passes.min(MAX_PASSES)) * 2).min(1024) as i32
}

/// `rect` grown by `by` on every side and clipped to `bounds`.
fn pad_region(
    rect: Rectangle<i32, Physical>,
    by: i32,
    bounds: Rectangle<i32, Physical>,
) -> Rectangle<i32, Physical> {
    let grown = Rectangle::new(
        (rect.loc.x - by, rect.loc.y - by).into(),
        (rect.size.w + 2 * by, rect.size.h + 2 * by).into(),
    );
    grown.intersection(bounds).unwrap_or(rect)
}

/// Chain depth for a region: the config value, limited so the smallest level keeps at least
/// two pixels.
fn effective_passes(want: u32, size: Size<i32, Physical>) -> u32 {
    let mut passes = want.min(MAX_PASSES);
    let small = size.w.min(size.h).max(1);
    while passes > 0 && (small >> passes) < 2 {
        passes -= 1;
    }
    passes
}

/// Sizes of the chain: the region, then each level half the previous (rounded up).
fn level_sizes(size: Size<i32, Physical>, passes: u32) -> Vec<Size<i32, Physical>> {
    let mut sizes = vec![size];
    for _ in 0..passes {
        let last = sizes[sizes.len() - 1];
        sizes.push(((last.w + 1) / 2, (last.h + 1) / 2).into());
    }
    sizes
}

/// VRAM of a chain of RGBA8 textures.
fn chain_bytes(sizes: &[Size<i32, Physical>]) -> usize {
    sizes
        .iter()
        .map(|s| s.w.max(0) as usize * s.h.max(0) as usize * 4)
        .sum()
}

/// Whether `opaque` leaves no part of `target` showing through.
fn covers(target: Rectangle<i32, Physical>, opaque: &[Rectangle<i32, Physical>]) -> bool {
    target.subtract_rects(opaque.iter().copied()).is_empty()
}

fn hash_rect(h: &mut impl Hasher, r: Rectangle<i32, Physical>) {
    (r.loc.x, r.loc.y, r.size.w, r.size.h).hash(h);
}

// ---------------------------------------------------------------------------------------
// Cache

struct Entry {
    chain: Vec<GlesTexture>,
    sizes: Vec<Size<i32, Physical>>,
    /// The padded region these textures show, output-local.
    region: Rectangle<i32, Physical>,
    sig: u64,
    id: Id,
    commit: CommitCounter,
}

struct Cache {
    lru: Lru<Key, Entry>,
    hits: u64,
    misses: u64,
}

impl Cache {
    fn new() -> Self {
        Self {
            lru: Lru::new(BUDGET_BYTES),
            hits: 0,
            misses: 0,
        }
    }

    fn report(&mut self) {
        if (self.hits + self.misses).is_multiple_of(REPORT_EVERY) && self.hits + self.misses > 0 {
            let total = (self.hits + self.misses) as f64;
            tracing::debug!(
                entries = self.lru.len(),
                bytes = self.lru.bytes,
                hits = self.hits,
                misses = self.misses,
                hit_rate = self.hits as f64 / total,
                "effects: blur cache"
            );
        }
    }
}

fn cache(renderer: &GlesRenderer) -> Rc<RefCell<Cache>> {
    let data = renderer.egl_context().user_data();
    data.insert_if_missing(|| Rc::new(RefCell::new(Cache::new())));
    data.get::<Rc<RefCell<Cache>>>()
        .expect("inserted above")
        .clone()
}

/// Frees everything cached for `output`; called when blur is off so its VRAM comes back at
/// once.
pub fn release(renderer: &GlesRenderer, output: &Output) {
    let shared = renderer
        .egl_context()
        .user_data()
        .get::<Rc<RefCell<Cache>>>()
        .cloned();
    if let Some(shared) = shared {
        let name = output.name();
        let freed = shared
            .borrow_mut()
            .lru
            .remove_where(|k, _| k.output == name);
        if freed > 0 {
            tracing::debug!(output = %name, freed, "effects: blur cache released");
        }
    }
}

// ---------------------------------------------------------------------------------------
// Element

/// The blurred region, drawn behind the window or layer it belongs to.
#[derive(Debug)]
pub struct BlurElement {
    id: Id,
    commit: CommitCounter,
    texture: GlesTexture,
    src: Rectangle<f64, Buffer>,
    dst: Rectangle<i32, Physical>,
}

impl Element for BlurElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.src
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.dst
    }

    fn damage_since(
        &self,
        _scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        if commit == Some(self.commit) {
            DamageSet::default()
        } else {
            DamageSet::from_slice(&[Rectangle::from_size(self.dst.size)])
        }
    }

    // Blurred translucency: nothing behind it may be culled, so no opaque regions.
}

impl RenderElement<GlesRenderer> for BlurElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        frame.render_texture_from_to(
            &self.texture,
            src,
            dst,
            damage,
            opaque_regions,
            Transform::Normal,
            1.0,
            None,
            &[],
        )
    }
}

// ---------------------------------------------------------------------------------------
// Scene hooks

/// A blur the scene wants: `rect` (output-local, logical) is blurred behind the elements
/// before index `at` of the front-to-back list.
pub struct Request {
    at: usize,
    owner: Owner,
    rect: Rectangle<i32, Logical>,
}

/// Whether blur draws on `output` this frame: enabled, programs compiled, no fullscreen
/// window on the output.
pub fn active(fx: &SceneFx, output: &Output) -> bool {
    let d = fx.decoration;
    d.blur
        && d.blur_passes > 0
        && d.blur_radius > 0
        && fx.enabled_on(output)
        && fx.programs.as_ref().is_some_and(|p| p.blur.is_some())
}

/// A request for the element whose pieces are `out[start..]`, pushed last, if that element
/// shows what is behind it (its opaque regions leave part of `rect` uncovered).
pub fn want(
    out: &[OutputElement],
    start: usize,
    owner: Owner,
    rect: Rectangle<i32, Logical>,
    scale: Scale<f64>,
) -> Option<Request> {
    if rect.is_empty() {
        return None;
    }
    let target: Rectangle<i32, Physical> = rect.to_physical_precise_round(scale);
    let opaque: Vec<_> = out[start..]
        .iter()
        .flat_map(|e| e.opaque_regions(scale).into_iter().collect::<Vec<_>>())
        .collect();
    if covers(target, &opaque) {
        return None;
    }
    Some(Request {
        at: out.len(),
        owner,
        rect,
    })
}

/// Turns the requests into `BlurElement`s inserted into `out`. Failures cost that blur only.
pub fn apply(
    renderer: &mut GlesRenderer,
    output: &Output,
    fx: &SceneFx,
    out: &mut Vec<OutputElement>,
    requests: &[Request],
) {
    let shared = cache(renderer);
    let Some(programs) = fx.programs.as_ref().and_then(|p| p.blur.clone()) else {
        return;
    };
    let Some(mode) = output.current_mode() else {
        return;
    };
    let scale = Scale::from(output.current_scale().fractional_scale());
    let bounds = Rectangle::from_size(mode.size);
    let name = output.name();
    let (radius, want_passes) = (fx.decoration.blur_radius, fx.decoration.blur_passes);

    shared.borrow_mut().lru.tick();
    shared.borrow_mut().lru.sweep_idle(MAX_IDLE);

    for request in requests.iter().rev() {
        let target: Rectangle<i32, Physical> = request.rect.to_physical_precise_round(scale);
        let Some(rect) = target.intersection(bounds) else {
            continue;
        };
        let passes = effective_passes(want_passes, rect.size);
        let region = pad_region(rect, reach(radius, passes), bounds);
        let behind = &out[request.at.min(out.len())..];

        let mut hasher = DefaultHasher::new();
        scale.x.to_bits().hash(&mut hasher);
        (passes, radius).hash(&mut hasher);
        hash_rect(&mut hasher, region);
        for element in behind {
            let geometry = element.geometry(scale);
            if geometry.overlaps(region) {
                element.id().hash(&mut hasher);
                element
                    .current_commit()
                    .distance(Some(CommitCounter::default()))
                    .hash(&mut hasher);
                hash_rect(&mut hasher, geometry);
            }
        }
        let sig = hasher.finish();

        let key = Key {
            output: name.clone(),
            owner: request.owner.clone(),
        };
        let src: Rectangle<f64, Buffer> = Rectangle::new(
            (
                (rect.loc.x - region.loc.x) as f64,
                (rect.loc.y - region.loc.y) as f64,
            )
                .into(),
            (rect.size.w as f64, rect.size.h as f64).into(),
        );

        let mut guard = shared.borrow_mut();
        let c = &mut *guard;
        let fresh = c
            .lru
            .get_mut(&key)
            .filter(|e| e.sig == sig && e.region == region && e.chain.len() == passes as usize + 1);
        let element = if let Some(entry) = fresh {
            c.hits += 1;
            BlurElement {
                id: entry.id.clone(),
                commit: entry.commit,
                texture: entry.chain[0].clone(),
                src,
                dst: rect,
            }
        } else {
            c.misses += 1;
            // Reuse the textures when the region kept its size, the common case while
            // something behind animates.
            let old = c.lru.take(&key);
            let sizes = level_sizes(region.size, passes);
            let (mut chain, id, mut commit) = match old {
                Some(e) if e.sizes == sizes => (e.chain, e.id, e.commit),
                _ => (Vec::new(), Id::new(), CommitCounter::default()),
            };
            drop(guard);
            let rendered = (|| -> Result<(), String> {
                if chain.is_empty() {
                    for s in &sizes {
                        chain.push(
                            renderer
                                .create_buffer(Fourcc::Abgr8888, (s.w, s.h).into())
                                .map_err(|e| e.to_string())?,
                        );
                    }
                }
                render_backdrop(renderer, &mut chain[0], region, scale, behind)?;
                run_chain(renderer, &programs, &mut chain, &sizes, radius as f32)
                    .map_err(|e| e.to_string())
            })();
            if let Err(err) = rendered {
                tracing::warn!(%err, "effects: blur failed");
                continue;
            }
            commit.increment();
            let element = BlurElement {
                id: id.clone(),
                commit,
                texture: chain[0].clone(),
                src,
                dst: rect,
            };
            let bytes = chain_bytes(&sizes);
            let mut guard = shared.borrow_mut();
            let evicted = guard.lru.insert(
                key,
                Entry {
                    chain,
                    sizes,
                    region,
                    sig,
                    id,
                    commit,
                },
                bytes,
            );
            if !evicted.is_empty() {
                tracing::debug!(evicted = evicted.len(), "effects: blur cache over budget");
            }
            guard.report();
            out.insert(request.at, OutputElement::Blur(element));
            continue;
        };
        c.report();
        drop(guard);
        out.insert(request.at, OutputElement::Blur(element));
    }
}

/// Draws `elements` (output-local) into `texture`, which shows `region` of the output.
fn render_backdrop(
    renderer: &mut GlesRenderer,
    texture: &mut GlesTexture,
    region: Rectangle<i32, Physical>,
    scale: Scale<f64>,
    elements: &[OutputElement],
) -> Result<(), String> {
    let shifted: Vec<_> = elements
        .iter()
        .map(|e| RelocateRenderElement::from_element(e, region.loc.upscale(-1), Relocate::Relative))
        .collect();
    let mut tracker = OutputDamageTracker::new(region.size, scale, Transform::Normal);
    let mut target = renderer.bind(texture).map_err(|e| e.to_string())?;
    tracker
        .render_output(renderer, &mut target, 0, &shifted, BACKGROUND)
        .map_err(|e| format!("{e:?}"))?;
    Ok(())
}

/// Down passes chain[0] -> chain[n], then up passes back to chain[0]; each level is reused as
/// the up target of its own size since its down result has been consumed by then.
fn run_chain(
    renderer: &mut GlesRenderer,
    programs: &BlurPrograms,
    chain: &mut [GlesTexture],
    sizes: &[Size<i32, Physical>],
    offset: f32,
) -> Result<(), GlesError> {
    let levels = chain.len() - 1;
    for i in 0..levels {
        let (a, b) = chain.split_at_mut(i + 1);
        pass(
            renderer,
            &programs.down,
            &a[i],
            &mut b[0],
            sizes[i + 1],
            offset,
        )?;
    }
    for i in (0..levels).rev() {
        let (a, b) = chain.split_at_mut(i + 1);
        pass(renderer, &programs.up, &b[0], &mut a[i], sizes[i], offset)?;
    }
    Ok(())
}

/// One shader pass from `src` into `dst` (`dst_size` pixels). The half-pixel uniform is the
/// smaller texture's half texel: the destination for a down pass, the source for an up pass.
fn pass(
    renderer: &mut GlesRenderer,
    program: &GlesTexProgram,
    src: &GlesTexture,
    dst: &mut GlesTexture,
    dst_size: Size<i32, Physical>,
    offset: f32,
) -> Result<(), GlesError> {
    let src_size = src.size();
    let small = if (src_size.w as i64) < dst_size.w as i64 {
        (src_size.w, src_size.h)
    } else {
        (dst_size.w, dst_size.h)
    };
    let halfpixel = (0.5 / small.0.max(1) as f32, 0.5 / small.1.max(1) as f32);
    let full = Rectangle::from_size(dst_size);
    let mut target = renderer.bind(dst)?;
    let mut frame = renderer.render(&mut target, dst_size, Transform::Normal)?;
    frame.clear(Color32F::TRANSPARENT, &[full])?;
    frame.render_texture_from_to(
        src,
        Rectangle::from_size(src_size).to_f64(),
        full,
        &[full],
        &[],
        Transform::Normal,
        1.0,
        Some(program),
        &[
            Uniform::new("halfpixel", halfpixel),
            Uniform::new("offset", offset),
        ],
    )?;
    // Same GL context as every later read of `dst`, so its own ordering is enough.
    let _ = frame.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Physical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    #[test]
    fn lru_evicts_least_recently_used_over_budget() {
        let mut lru: Lru<&str, u32> = Lru::new(100);
        assert!(lru.insert("a", 1, 40).is_empty());
        lru.tick();
        assert!(lru.insert("b", 2, 40).is_empty());
        lru.tick();
        lru.get_mut(&"a"); // a is now newer than b
        lru.tick();
        let evicted = lru.insert("c", 3, 40);
        assert_eq!(evicted, vec![2]);
        assert_eq!(lru.bytes, 80);
        assert!(lru.get_mut(&"b").is_none());
        assert!(lru.get_mut(&"a").is_some());
    }

    #[test]
    fn lru_keeps_an_entry_larger_than_the_budget() {
        let mut lru: Lru<u8, u8> = Lru::new(10);
        lru.insert(1, 1, 5);
        let evicted = lru.insert(2, 2, 50);
        assert_eq!(evicted, vec![1]);
        assert_eq!(lru.len(), 1);
        assert_eq!(lru.bytes, 50);
    }

    #[test]
    fn lru_replacing_a_key_frees_its_bytes() {
        let mut lru: Lru<u8, u8> = Lru::new(100);
        lru.insert(1, 1, 60);
        assert!(lru.insert(1, 2, 70).is_empty());
        assert_eq!(lru.bytes, 70);
        assert_eq!(lru.take(&1), Some(2));
        assert_eq!(lru.bytes, 0);
    }

    #[test]
    fn lru_sweeps_idle_and_removes_by_key() {
        let mut lru: Lru<u8, u8> = Lru::new(1000);
        lru.insert(1, 1, 10);
        for _ in 0..5 {
            lru.tick();
        }
        lru.insert(2, 2, 10);
        assert_eq!(lru.sweep_idle(3), 1);
        assert!(lru.get_mut(&1).is_none());
        assert_eq!(lru.remove_where(|k, _| *k == 2), 1);
        assert_eq!((lru.len(), lru.bytes), (0, 0));
    }

    #[test]
    fn padding_clips_to_the_output() {
        let bounds = r(0, 0, 1000, 500);
        assert_eq!(pad_region(r(10, 10, 100, 50), 30, bounds), r(0, 0, 140, 90));
        assert_eq!(
            pad_region(r(900, 450, 100, 50), 30, bounds),
            r(870, 420, 130, 80)
        );
    }

    #[test]
    fn passes_shrink_for_small_regions() {
        assert_eq!(effective_passes(3, (400, 300).into()), 3);
        assert_eq!(effective_passes(9, (400, 300).into()), MAX_PASSES);
        assert_eq!(effective_passes(3, (10, 300).into()), 2);
        assert_eq!(effective_passes(3, (1, 1).into()), 0);
    }

    #[test]
    fn levels_halve_rounding_up() {
        let sizes = level_sizes((101, 50).into(), 3);
        let as_pairs: Vec<_> = sizes.iter().map(|s| (s.w, s.h)).collect();
        assert_eq!(as_pairs, [(101, 50), (51, 25), (26, 13), (13, 7)]);
        assert_eq!(chain_bytes(&sizes), (5050 + 1275 + 338 + 91) * 4);
    }

    #[test]
    fn coverage_needs_the_whole_target() {
        let target = r(0, 0, 100, 100);
        assert!(covers(target, &[r(0, 0, 100, 100)]));
        assert!(covers(target, &[r(0, 0, 50, 100), r(50, 0, 50, 100)]));
        assert!(!covers(target, &[r(0, 0, 100, 99)]));
        assert!(!covers(target, &[]));
    }
}
