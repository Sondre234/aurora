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

## M4 service budgets and metrics

Services are separate processes, so their cost never lands on the compositor's paint path.
The compositor side of M4 (IPC, supervision, theme) runs on the main loop but only does
small, bounded work: the snapshot diff is cheap, per-client queues are capped, nothing
blocks on a client. Targets below are initial and get measured on the hardware checklist in
[m4-plan.md](m4-plan.md).

| Item | Budget | Metric |
|---|---|---|
| IPC snapshot diff + broadcast | negligible next to a frame, no allocation when nothing changed; never on the render path | `ipc: broadcast topic=<t> clients=<n>` (only when something changed) |
| IPC client queue | coalesced per state; over 256 KiB unsent the client is resynced with a snapshot, over 2 MiB it is disconnected | `ipc: slow client name=<n> resynced`, `dump: ipc clients=<n>` |
| IPC frames | small typed messages, capped at 1 MiB, large buffers only by fd | n/a |
| Idle service cost | ~0 CPU and 0 GPU: no timers except the clock (once a minute), no frame callbacks while nothing changes, damage regions only | client-side `perf:` lines per service |
| Resident memory per service | a few MB each (wl_shm pool, glyph cache), all services together well under 200 MB | `dump: service <name> pid=<p> restarts=<n>` then `/proc/<pid>/status` |
| Glyph/shaping cache (`ui`) | per process, byte-budgeted with LRU eviction (initial 16 MB) | hit rate, bytes, evictions in a `perf:` line every 5 s while active |
| Launcher | app index and fuzzy tables built once at startup, surface created once and shown/hidden: toggle to first frame under one refresh interval | `launcher: ready apps=<n>`, `launcher: show`/`hide` |
| Bar | retained surface, repaints only on IPC delta, theme change or the minute tick | `shell: ready outputs=<n>` |
| Supervision | restarts back off 500 ms doubling to 30 s; a run of 10 s resets it; no restart storm can burn CPU | `service: started`, `service: exited ... restart=<ms>` |
| Theme push | one `Event::Theme` per real change (identical file: nothing), services repaint without restart | `theme: changed rev=<n>` |

QA proves the log contract (`scripts/qa-nested.sh ipc services theme shell launcher
notifd lock`); memory, idle CPU and toggle latency are on the hardware checklist.

## M5 cache budgets

`term` and `files` are ordinary windows (one process each), so their cost never lands on the
compositor's paint path. Same cache rules as above: owner, invalidation, byte budget, metric.
Metrics are `perf:` lines (info, on change or every 5 s while active, never while idle).
Budgets are initial and measured on the hardware checklist in [m5-plan.md](m5-plan.md).

| Cache / item | Owner | Invalidated by | Budget / eviction | Metric |
|---|---|---|---|---|
| Term glyph cache (coverage masks per char, bold, italic; fg applied at blit) | `term` render | theme change (fonts/scale), never by color | per process, byte budget (initial 16 MB), LRU | hit rate, bytes, evictions |
| Term scrollback | `term` backend | `--scrollback`, resize reflow | 10 000 lines default, hard cap 200 000; steady-state RSS under 150 MB | `dump: term cols= rows= scrollback=` |
| Term pty reads | `term` pty | n/a | at most 64 KiB per loop wakeup, so floods never starve input or painting | n/a |
| Term repaint | `term` app | emulator line damage | at most one repaint per frame callback, shm damage rects only; idle costs zero frames; cursor blink repaints one cell and stops when unfocused | `term: frame ms=<n>` (debug) |
| Files listing | `files` model | directory change (debounced `notify`), navigation | generation-tagged worker reads, stale results dropped; 100 000 entries virtualized (only visible rows laid out and painted) | `files: navigate path= entries=` |
| Files icons | `files` view | theme change, scale change | pre-rasterized at the display scale, byte-budgeted LRU (initial 16 MB) | hit rate, bytes, evictions |
| Files operations | `files` ops worker | n/a | one worker, progress and cancel, never on the UI thread | `files: op start/done` |

Frame-time and resource targets for M5:

- Idle terminal and idle file manager schedule no frames and no timers except the focused
  cursor blink; an idle `term` logs no repaints.
- Full 200x60 screen paint in `term` under 4 ms in release.
- `cat` of a 100 MB file leaves compositor frame time unaffected.
- Theme change invalidates the glyph cache and repaints once, no restart.

QA proves the log contract and the flood case (`scripts/qa-nested.sh term files`); paint
time, RSS, idle CPU, 100k-entry scrolling and 144 Hz behavior are on the hardware checklist.
