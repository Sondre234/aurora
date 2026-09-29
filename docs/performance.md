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
