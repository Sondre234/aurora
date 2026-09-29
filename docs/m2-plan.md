# M2 plan: usable (tiling, workspaces, XWayland, layer-shell, config, multi-monitor)

Read README.md, docs/performance.md and docs/m1-plan.md first. This plan merges four designs (tiling engine, workspaces/outputs/config, protocols/XWayland, input/verification). Every step leaves `cargo build` and `cargo clippy --all-targets` warning-free and is committed on its own. Never run `--drm` in verification; nested runs use `--winit --timeout 20..40`.

## Architecture decisions

- **Pure layout crate.** `crates/layout` (`aurora-layout`, std only, no Smithay, no serde) owns `Rect`, `Dir`, the `TilingLayout` trait, `Dwindle`, `Workspace` (tiled tree, floating list, fullscreen, MRU focus) and `geom` (directional neighbour, resize edges, drop side). Output: plain retained `Placement` rects. Public methods tolerate stale or unknown `WinId` (return false/None).
- **Wm registry is the source of truth; Space is a projection.** `crates/comp/src/wm/` holds `Wm { windows: HashMap<WinId, WinData>, by_surface, by_x11, workspaces, outputs, focused, ... }`. `WinData { target: Rect, current: Rect, ... }`; M2 sets `current = target`, M3 animates `current` toward `target`. One function (`wm::apply`) diffs placements onto `Space<WindowElement>`: map/relocate/unmap, configure only when size or state changed, redraw only touched outputs. Nothing else edits the Space.
- **Hidden workspaces are unmapped from the Space.** So they render nothing and get no frame callbacks or presentation feedback. Because unmapped windows are not in `space.elements()`, all lookups go through `Wm.by_surface`/`by_x11`, commits of hidden windows still call `window.on_commit()`, and Wm prunes dead windows itself (`Window::alive()` in `toplevel_destroyed` and next to `space.refresh()`).
- **Workspaces are global and numbered** (default 10, cap 1..=32, Hyprland semantics). Each output shows exactly one; `workspace N` on a workspace visible elsewhere focuses that output. Empty workspaces are empty Vecs.
- **Two-phase map.** `new_toplevel` creates a Pending record only. On the initial bufferless commit (app_id/title/parent/min-max known) rules run, the window is inserted into the layout and the first configure already carries tiled size and states. First buffer commit sets Mapped, focus and redraw. X11 windows evaluate rules in `map_window_request`.
- **WindowElement** wraps `Window` plus a retained `Deco` (4 `SolidColorBuffer` borders, focused flag, z index). z-index: tiled 30, floating 31, fullscreen 50 (above Top layer, below Overlay 60), override-redirect 55. Rendering uses ONE shared `output_elements()` builder (new `scene.rs`, `OutputElement` moved out of `backend/drm/render.rs`) used by DRM, winit and screencopy. Front to back: cursor, Overlay layers, Top layers (skipped when the output has a fullscreen window), unmanaged X11, workspace windows, Bottom, Background. `space_render_elements` is retired (it always draws Top layers over fullscreen and defeats direct scanout).
- **Seat focus types** become `FocusTarget { Wl(WlSurface), X11(X11Surface) }` (keyboard and pointer), with `TabletSeatHandler { ToolFocus = WlSurface }` stub. Grabs hold `WinId` and plain rects only (they must be `Send`).
- **Config** `~/.config/aurora/config.toml` (serde + toml, only new deps). Resolve pipeline `RawConfig -> Config::resolve -> (Config, warnings)`. Ladder: missing file = defaults; TOML syntax error = keep previous (defaults at first start) with line:col; bad section = section defaults; bad list item = item dropped; unknown key = warning. `Arc<Config>` in Aurora. Reload on SIGUSR1 and the `reload-config` action, never auto-watched. Autostart runs once per process.
- **Keybinds.** Pure `Mods` bitset (SHIFT CTRL ALT LOGO ALTGR, locks ignored), `Chord {mods, level-0 latin keysym via raw_latin_sym_or_raw_current_sym}`; binds name the unshifted key (`Super+Shift+1`), AltGr is its own bit so `Super+AltGr+q` never matches `Super+q`. User binds merge over built-in defaults (the embedded `config/aurora.example.toml`), `"none"` removes a default. Filter order: (a) hardcoded `emergency::classify` (quit, VT switch; ignores inhibitors, layers, config), (b) config binds if `binds_allowed()` or the bind has `bypass_inhibit`, (c) forward. Config chords equal to an emergency chord are rejected with a warning. Default mod = Super; nothing requires left Alt.
- **Virtual keyboard: Aurora owns zwp_virtual_keyboard_v1** (do NOT create Smithay's `VirtualKeyboardManagerState`; it bypasses the key filter and swaps the seat keymap). vk keycode -> keysym via per-vk `xkb::State` -> Aurora keycode via `keycode_for_keysym`; modifiers requests become synthetic modifier key presses; everything goes through `Aurora::on_key(KeyboardSource::Auxiliary, ..)` so wtype triggers binds. Gated by `general.allow_virtual_keyboard` (default true).
- **Screencopy: ext-image-copy-capture only** (grim speaks it; Smithay has no wlr-screencopy). `frame()` never renders inline: it queues and calls `queue_redraw_output`; both backends call `serve_captures` after render. Do not advertise fifo, commit-timing, tearing-control.
- **XWayland eager**, env passed via `Command::env` (never `set_var`), rate-limited respawn (5 per 60 s) because `-terminate` is hardcoded, selection bridge mandatory, override-redirect windows in a separate unmanaged `Space` above workspace windows and below Top layers. Output layout must be normalised so the bounding-box origin is (0,0); every output move goes through one helper doing both `space.map_output` and `output.change_current_state(.., Some(pos))`.
- **Robustness rule.** Remove every client-reachable `unwrap/expect` (all `toplevel().unwrap()`, `element_geometry().unwrap()`, `client_compositor_state`, `dnd_requested`, winit bind/render/submit, popup grab).
- **Tests only for pure logic:** layout crate, config resolve (including parsing the shipped example), keybind/chord/action parsing, emergency table, `resolve_positions`, glob matcher. No glue tests.

## Log contract (stable; QA greps these; emit at info, state changes only)

```
action: <action text>           spawn: <cmd>
config: loaded path=<p> binds=<n> warnings=<n> | config: error <msg> keeping previous | config: warning <item>: <reason>
output: added name=<n> geo=<x>,<y> <w>x<h>@<hz> scale=<s> | output: removed name=<n> rescued=<k> to=<other>
layout: ws=<n> out=<name> [<id>:<app_id>:<x>,<y> <w>x<h>[ float][ fs] ...]
focus: <id>:<app_id>@ws<n>/<out> | focus: none
ws: visible <out>=<n> ...       layer: out=<n> usable=<x>,<y> <w>x<h>
xwayland: ready display=:<N> | xwayland: exited
```

SIGUSR2 state dump: `dump: begin <seq>`, `dump: mods=.. pressed=.. suppressed=.. binds=.. repeat=..`, `dump: out <name> geo=.. scale=.. ws=.. usable=..`, `dump: ws <n> out=.. layout=.. windows=.. focus=..`, `dump: win <id> app_id=.. title=.. kind=wl|x11|or ws=.. rect=<x>,<y> <w>x<h> float=0|1 fs=0|1 frames_sent=<u64> mapped=0|1`, `dump: layer ..`, `dump: focus kbd=.. pointer=..`, `dump: end <seq>`.

## Steps

1. **Config, CLI flags, emergency module, reload.** `config/{mod,raw,keybind}.rs`, `emergency.rs` (moved from input.rs, with table test), `config/aurora.example.toml`, `--config`, `--qa`, SIGUSR1 in safety.rs, `spawn.rs` (sh -c, sigmask reset, setsid, reaper thread, env on Command). Output config structs parsed but not yet consumed.
2. **Pure layout crate + tests.** Dwindle, Workspace, geom, TilingLayout trait.
3. **Focus/scene refactor with identical behaviour.** `FocusTarget`, `WindowElement`+`Deco`, `Space<WindowElement>`, shared `scene.rs` builder, `Protocols` struct, fix `client_compositor_state`, purge unwraps.
4. **Wm registry and tiled window lifecycle.** Two-phase map, layout, gaps, borders, focus, close, focus-after-close, xdg toplevel handlers, hidden-window commit routing.
5. **Keybind engine, actions, virtual keyboard, QA hooks.** `input/` split with `on_key`/pointer primitives, `Action`, `actions.rs`, key repeat, wheel binds, own vk global, debug.rs (SIGUSR2 dump, `--qa` debug actions).
6. **Workspaces and moving windows.**
7. **Floating, fullscreen, maximize, grabs.** Rules (float heuristics, workspace), float move/resize, tiled resize, mouse binds, xdg move/resize/fullscreen requests.
8. **Layer-shell and exclusive zones.**
9. **Multi-output, fractional scale, viewporter, hotplug rescue.**
10. **XWayland.**
11. **Remaining protocols and screencopy.**
12. **QA script, docs, README.**

Per-step instructions, done-criteria and smoke tests are carried by the workflow plan; this file records the decisions above and is the reference for later fixers.

## XWayland notes (step 10)

- `xwayland.rs` owns the lifecycle, `handlers/xwm.rs` the callbacks and selection bridge, `wm/x11.rs` the window logic. Managed X11 windows are ordinary `WinData` (`Wm.by_x11`, `by_surface` once Xwayland pairs the wl_surface) and go through the same rules, layout and focus as xdg toplevels; rules match `class` for X11 and `app_id` for Wayland. Override-redirect windows live in `XWaylandState.unmanaged`, drawn above workspace windows and below Top layers, hit-tested the same way, never tiled.
- X11 windows are not scaled: their coordinates are the global logical ones, so with a fractional output scale they render at scale 1 and are upscaled by nothing (documented limit).
- The display number is chosen by us (first without lock or socket) so smithay never probes other sessions' leftovers. Children get `DISPLAY` from `Aurora::spawn_env()` as soon as the server is spawned. Xwayland is not observed to exit when its last client does here, but smithay hardcodes `-terminate`, so exits are handled: teardown from an idle callback, restart on the same display. Only instances that lived under 10 s count towards the limit of 5 restarts per 60 s.
- Smithay ignores every X event carrying the sequence number of `set_randr_primary_output`, and events only carry a newer one once the manager sends another request. `xwayland_ready` therefore sets the cursor after it; reordering them silences the window manager.
- Clipboard and primary selection bridge both ways; X clients may only touch selections while an X11 window has the keyboard.
- Shutdown: `shutdown_xwayland()` runs after the event loop returns on every exit path (quit chord, timeout, signals) and drops the window manager and the server before the state.

## Protocol notes (step 11)

- **Decoration:** xdg-decoration always answers ServerSide (borders are the compositor's); the mode rides the initial configure, a later request only sends a configure if the first one already went out.
- **Activation:** client tokens need our seat and a serial no older than the keyboard's last enter; tokens older than 10 s are ignored and pruned. Tokens minted by `Aurora::spawn_env` (`XDG_ACTIVATION_TOKEN`) always focus a visible window; client tokens focus only with `focus_on_activate = true`, else the window is marked urgent (`urgent=1` in the dump, cleared on focus). Hidden-workspace windows are only marked.
- **Shortcuts inhibit:** the inhibitor of the keyboard-focused surface is active, the old one stands down on focus change. `binds_allowed()` then refuses binds except `bypass_inhibit` ones (`revoke-inhibit` is bound that way by default). Emergency chords never consult it.
- **Pointer constraints:** held only while the surface has both pointer and keyboard focus. Locked: absolute motion is dropped, the client gets relative motion only. Confined: per-axis fallback to the last allowed position. Layout changes re-run `pointer.motion` so stale constraints drop. A destroyed active lock warps to its cursor position hint.
- **Popup grabs:** rejected with `popup_done` unless the serial is backed by a grab or is no older than the keyboard's last enter.
- **Screencopy:** ext-image-copy-capture for outputs, shm Argb8888/Xrgb8888 at the mode size (what grim 1.5 uses; wlr-screencopy is not offered). A frame request queues a redraw and waits in `Captures.pending`; the output's own render path calls `capture::serve` with the elements it is about to draw (DRM: pointer first, so sessions without cursors skip them). One offscreen render per cursor variant, CPU readback, so this suits screenshots rather than video. Not advertised: fifo, commit-timing, tearing-control.
