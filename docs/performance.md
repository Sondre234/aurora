# Performance principles

Aurora treats the desktop like a browser engine: retain everything, cache
aggressively, precompute before it's asked for, and never block the paint path.
Memory and VRAM are budgeted resources to spend, not saved. Target machine:
24 GB VRAM, 32 GB RAM.

## Rules

1. **Frame budget is law.** One frame is 1 / refresh rate (6.9 ms at 144 Hz).
   The compositor thread only composites and dispatches input. Anything that can
   take longer (decoding, layout of services, disk, IPC handling, AI) runs elsewhere.
2. **Retain, don't redraw.** Scene state persists between frames. Per-surface
   textures, per-output damage tracking, and layer-style caching of anything
   static (bar, blurred backdrops) so an idle desktop costs ~0 GPU work.
3. **Cache expensive results, invalidate precisely.**
   - Blurred backdrop per region: recomputed only when the pixels behind change.
   - Shadow/border/corner masks: rendered once per size, reused.
   - Glyph atlas + shaped-text cache shared across all `ui` clients.
   - Icons pre-rasterized at the display scale; images decoded once (RAM LRU +
     disk cache of decoded/thumbnailed forms).
   - Compiled shader programs and pipeline caches persisted to disk; all shaders
     built at startup, never on first use (no first-frame hitch).
4. **Precompute and prewarm.** App index, desktop entries, and fuzzy-match tables
   live in memory in the launcher. Window thumbnails for the overview are kept
   live. Frequently used apps can be pre-spawned hidden.
5. **Services stay warm.** Daemons are long-lived and never cold-start on
   interaction. Their UI surfaces are created once and shown/hidden, not rebuilt.
6. **Zero-copy where it matters.** Fullscreen clients get direct scanout, the
   cursor uses the hardware plane, large buffers cross process boundaries as
   dmabuf/shared memory, and IPC payloads are small typed messages.
7. **Pace frames, don't just render them.** Present-time prediction, explicit
   sync on NVIDIA, no unbounded frame queues (latency is a feature).
8. **Measure or it didn't happen.** `tracing` spans on every frame stage, a
   frame-time overlay from M1, and a benchmark scene that CI-style scripts can
   run. A regression in p99 frame time blocks a merge.

## Cache discipline

Aggressive caching fails through stale data and unbounded growth, so every
cache must declare:

- an owner and an explicit invalidation trigger,
- a byte budget with eviction (LRU unless justified),
- a metric (hit rate, size) exposed via tracing.

## Budgets (initial, revisit with profiling)

| Resource | Budget |
|---|---|
| Compositor frame work | < 50% of refresh interval at p99 |
| VRAM for caches | up to ~6 GB (of 24) |
| RAM for caches, all services | up to ~6 GB (of 32) |
| Input-to-photon | as low as the display pipeline allows; measured, not assumed |

## M3 cache budgets and metrics

Each M3 cache declares owner, invalidation, budget and a tracing metric (target `perf`,
emitted at info on state change or every 5 s while active, never per frame when idle).
Budgets are initial; the sum stays inside the ~6 GB VRAM cache budget above.

| Cache | Owner | Invalidated by | Budget / eviction | Metric |
|---|---|---|---|---|
| Shader programs (`effects::Programs`) | renderer user data | never (compiled at startup) | fixed, ~a dozen programs | `effects: programs compiled=<n>` at startup |
| Shadow textures / elements | `effects/shadow.rs`, per window | window size, `shadow_radius`, `shadow_color` | 1 per window, dropped with the window | shadow cache size and rebuilds |
| Rounded corner masks | `effects/corners.rs` | `rounding`, window size | per size, reused | mask rebuilds |
| Blur backdrop | `effects/blur.rs`, per output | damage of whatever is behind the blurred region, `blur_*` keys | LRU, up to ~2 GB VRAM | hit rate, bytes, evictions |
| Overview live thumbnails | `overview/`, per window | that window's commit only | `TextureRenderBuffer` per window, bounded by window count, dropped on close | thumbnails updated per second |
| Close ghosts | `Wm` ghost list | fade end | one texture per closing window, freed when the fade ends | live ghost count |
| Animation state | `Wm` timeline | settle | O(windows), no GPU | `anim: start`/`anim: idle` lines |

Frame-time rules for M3:

- An idle desktop schedules no redraws: after `anim: idle` no frame is requested until
  input or a client commit arrives.
- Animated frames stay under 50% of the refresh interval at p99 (3.5 ms at 144 Hz), with
  blur, shadows and rounding enabled on all three outputs.
- Effects are skipped for fullscreen windows and while `top_hidden`, so direct scanout
  of a fullscreen game is unaffected.
- Blur never recomputes on a frame where nothing behind it changed.

QA proves the log contract lines (`scripts/qa-nested.sh anim effects overview xscale`);
frame pacing, VRAM use and scanout are on the hardware checklist in
[m3-plan.md](m3-plan.md).
