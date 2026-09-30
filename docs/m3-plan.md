# M3 plan: the look (animation engine, blur/shadows/rounded corners, live overview, X11 scale)

Read README.md, docs/performance.md and docs/m2-plan.md first. Every step leaves `cargo build` and `cargo clippy --all-targets` warning-free. Never run `--drm` in verification; nested runs use `--winit --qa --timeout 20..40` against a headless host. After changes that the user will test, `cargo build --release` (the `aurora` alias uses the release binary).

## Architecture decisions

- **Geometry stays logical-final; animation is render-only.** The layout still produces `target`; configures still carry the final size at once (intermediate sizes cause client jitter). `WinData.current` becomes the animated drawn rect (position, plus a scale factor for size change), advanced by `Wm::tick(now)`. The `Space` location stays at `target`, so hit-testing and input use the final geometry. Render applies the `current - target` offset and scale.
- **One clock, one tick.** `crates/comp/src/anim/` (pure, no Smithay): `Curve` (cubic-bezier + spring), `Animated<T>` (f32, point, rect), `Timeline`. `Wm::tick(now) -> bool` (returns "still animating") is called at the start of `render_surface` (DRM) and the winit `Redraw` handler. Time source is the presentation clock in DRM `frame_finish`, `state.clock` elsewhere.
- **Keep-alive.** While `tick` reports active, the DRM output keeps `damaged = true` after each render (`frame_finish` re-damages); idle desktop still costs zero. Animated elements must bump their commit counter or the damage tracker will not repaint them.
- **New element variants** go into `OutputElement` in `scene.rs` only via the scaffold step, so later streams add variants in their own modules (`effects/shadow.rs`, `effects/blur.rs`, `overview/`) and only a one-line enum entry each.
- **Shaders:** all compiled at startup into `effects::Programs` stored on the renderer user data (no first-use hitch). Rounded corners: custom `GlesTexProgram` applied to window surface elements (no ClippedSurface in the pinned Smithay). Shadows: `PixelShaderElement` behind the window, size cached per window. Blur: dual-Kawase on offscreen textures, per-output backdrop cache invalidated by damage of what is behind; budgeted by performance.md (VRAM up to ~6 GB, metric via tracing).
- **Opaque regions:** translucent or blurred elements return empty opaque regions; rounded windows report corners as non-opaque.
- **Direct scanout:** fullscreen window with no effects must still scan out; effects are skipped for fullscreen and while `top_hidden`.
- **Closing windows:** a `Ghost { tex, rect, start, z, output }` list in `Wm`; the last frame is rendered to a `GlesTexture` before unmap and drawn by the tick until the fade ends. Ghosts take no input or frame callbacks.
- **Workspace slide:** both workspaces mapped in the Space for the duration of the slide so they get frame callbacks; `hide_invisible` runs when the slide ends.
- **Overview:** `overview/` holds state (`Option<Overview>` in `Aurora`), layout of thumbnails (pure function, unit tested), live thumbnails rendered per window into retained `TextureRenderBuffer`s updated on the window's commit only. Input is intercepted early in `filter_key`, `surface_under` and pointer button/axis handlers; emergency chords keep working. Toggle action `overview`, default bind Mod+Tab... (choose a free chord in the example config).
- **Config:** new `[animations]` (`enabled`, `duration_ms`, `curve`, per-kind overrides `window_move`, `window_open`, `window_close`, `workspace`, `fade`) and `[decoration]` (`rounding`, `shadow`, `shadow_radius`, `shadow_color`, `blur`, `blur_passes`, `blur_radius`, `inactive_opacity`). Border keys stay in `[general]` for compatibility. Every key goes in `config/aurora.example.toml` (the parse test guards it). Reload applies live.
- **Log contract additions:** `anim: start kind=<k> win=<id>`, `anim: idle`, `effects: programs compiled=<n>`, `overview: open|close`, dump `win` gets `current=<x>,<y> <w>x<h> anim=0|1`, new `dump: overview`.
- **Tests only for pure logic:** curves, Animated, overview thumbnail layout, config resolve.

## Steps and streams

**Step 1 (serial, foundation):** `anim/` module + tests, `Wm::tick`, redraw keep-alive in DRM/winit/headless, `[animations]`/`[decoration]` config, `effects/` skeleton with `Programs` and scene enum scaffolding, dump fields. No visible behaviour change with `enabled = false`.

**Step 2 (parallel, each in its own git worktree off the step-1 commit; all touching disjoint files where possible):**

- **A. Window animations:** move/resize interpolation, open (scale+fade), close ghosts, focus/inactive opacity, workspace slide. Files: `wm/apply.rs`, `wm/workspaces.rs`, `wm/window.rs` (render offset/scale), ghost code.
- **B. Rounded corners + shadows:** shaders and elements in `effects/corners.rs`, `effects/shadow.rs`, hooked from `wm/window.rs` render path, borders follow the rounding.
- **C. Blur:** `effects/blur.rs` for layer-shell surfaces (bar, launcher) and windows with translucency, backdrop cache with LRU/byte budget and tracing metrics, config keys, skip on fullscreen.
- **D. Live overview:** `overview/` module, action, input interception, thumbnails, click to focus/move window between workspaces, open/close animation using the anim engine.
- **E. X11 scaling:** `xwayland.scale` config key with `-force-xrandr-emulation` (or per-output unscaling), so X11 apps render sharp on scaled outputs; update README known limits.
- **F. QA + docs:** new `scripts/qa-nested.sh` scenarios (anim, effects, overview, xscale), screenshot pixel checks, `docs/performance.md` budgets and metrics, README.

**Step 3 (serial):** merge streams into `feat/m3-look`, resolve conflicts in `scene.rs`/`window.rs`, full build, clippy, tests, QA run, release build, hardware checklist.

## Hardware checklist (run on the real machine, from a TTY)

- [ ] 144 Hz animations hold frame pacing on all three outputs (no dropped frames in the frame-time log), idle desktop schedules no redraws.
- [ ] Fullscreen game still direct-scans out; effects are off while fullscreen.
- [ ] Blur behind waybar and translucent terminal is correct after the window behind changes, VRAM stays inside budget.
- [ ] Overview shows live thumbnails on every output, click focuses, emergency chords still work inside it.
- [ ] X11 (Proton) app on the 1.25 scaled output is sharp and correctly sized; pointer mapping is correct.
