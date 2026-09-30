# aurora

A Wayland compositor and desktop environment for high-end hardware only.
Target: RTX 4090 (proprietary driver, explicit sync), Intel 12th gen+, 32 GB RAM,
high-refresh displays. No fallback paths, no low-end compromises.

## Architecture

The compositor stays small and strict. Everything else is a separate process
talking to it over Wayland protocols and one IPC bus, so a crash in the bar or
file manager never takes down the session.

```
                    ┌───────────────────────────────┐
                    │  comp         (Smithay, Rust) │  owns: GPU, input, windows,
                    │  render · layout · animation  │  effects, focus, outputs
                    └──────┬─────────────┬──────────┘
        wayland protocols  │             │  IPC bus (unix socket, typed msgs)
     (layer-shell, etc.)   │             │
   ┌────────┬────────┬─────┴──┬────────┬─┴──────┬────────┬────────┐
   │ shell  │launcher│ notifd │ lock   │settingsd│ term  │ files  │
   └────────┴────────┴────────┴────────┴─────────┴───────┴────────┘
                    shared crates: ipc, ui, theme
```

| Crate | Job |
|---|---|
| `comp` | Compositor: DRM/KMS, GBM, EGL, tiling, animation, blur/shadow shaders |
| `ipc` | Typed protocol shared by every service |
| `ui` | Shared GPU UI toolkit (wgpu + cosmic-text) |
| `theme` | Palette, fonts, motion curves, pushed live to services |
| `shell` | Bar, workspace overview, quick settings |
| `launcher` | App launcher with local-LLM hook |
| `notifd` | `org.freedesktop.Notifications` |
| `lock` | `ext-session-lock` screen locker |
| `settingsd` | Config daemon + GUI |
| `term` | GPU terminal |
| `files` | File manager |

Crates are added as their milestone is reached.

## Milestones

- [x] **M0** Nested compositor under an existing desktop (winit backend), one client shows
- [x] **M1** Real session: DRM backend on the 4090, libinput, launch from a TTY
- [x] **M2** Usable: tiling, workspaces, XWayland, layer-shell, config, multi-monitor. Verified on hardware
- [ ] **M3** The look: animation engine (window move/open/close, workspace slide), rounded corners, shadows and blur, live overview, scaled X11. Plan in [docs/m3-plan.md](docs/m3-plan.md)
- [ ] **M4+** Services: `ipc`, shell, launcher, notifd, lock, then `term` and `files`

## Using it (M2)

Config lives in `~/.config/aurora/config.toml`; nothing is generated for you. Copy
[config/aurora.example.toml](config/aurora.example.toml), which documents every option,
the outputs of the author's machine and the default binds. A missing file means defaults,
a broken file keeps the previous settings and logs why. Reload with Super+Shift+r or
`kill -USR1 <aurora-comp pid>`.

Default binds (`Mod` is Super):

| Keys | Action |
|---|---|
| Mod+q | spawn kitty |
| Mod+c | close window |
| Mod+w | toggle floating |
| Mod+f | fullscreen |
| Mod+j | toggle split direction |
| Mod+Left/Right/Up/Down | focus in direction |
| Mod+Shift+arrows | move window in direction |
| Mod+Ctrl+arrows | resize the split (repeats) |
| Mod+, / Mod+. | focus output left / right |
| Mod+1..0 | workspace 1..10 |
| Mod+Shift+1..0 | move window to workspace 1..10 |
| Mod+left drag / right drag | move / resize floating window |
| Mod+wheel | next / previous workspace |
| Mod+Shift+r | reload config |
| Mod+Shift+Escape | end the focused client's shortcuts inhibitor |
| Mod+m | quit |

Emergency chords are hardcoded and cannot be rebound or removed: Ctrl+Alt+BackSpace or
Ctrl+AltGr+BackSpace quits, Ctrl+Alt+F1..F12 or Ctrl+AltGr+F1..F12 switches VT.

Known limits: there is no ext-workspace or foreign-toplevel protocol yet, so a bar's
workspace widget waits for the M4 IPC. X11 apps are unscaled by default, so they look
blurry on a scaled output; set `[xwayland] scale` to that output's scale (read when
XWayland starts, see the example config) for sharp native-resolution X11 windows (not yet
verified on hardware). wlr-screencopy is not offered (ext-image-copy-capture is, which is what grim uses). Aurora owns
`zwp_virtual_keyboard_v1` and routes its keys through the bind table, so any client can
inject key presses and trigger binds; set `allow_virtual_keyboard = false` to turn that off.

## Using it (M3)

Placeholder, filled in when the M3 streams are merged. The new config sections, all
documented in [config/aurora.example.toml](config/aurora.example.toml) and applied live on
reload:

- `[animations]`: `enabled`, `duration_ms` (0..=10000), `curve` (`linear`, `ease-out`,
  `ease-in-out`, `spring [damping]`, `bezier x1 y1 x2 y2`), and per-kind overrides
  `window_move`, `window_open`, `window_close`, `workspace`, `fade` (each may set
  `enabled`, `duration_ms`, `curve`). `enabled = false` snaps like M2.
- `[decoration]`: `rounding` (px), `shadow`, `shadow_radius`, `shadow_color`
  (`#rrggbb[aa]`), `blur`, `blur_passes`, `blur_radius`, `inactive_opacity`. Skipped for
  fullscreen windows.
- Overview bind and `xwayland.scale`: to be documented when they land.

Scripted checks: run `scripts/qa-nested.sh` with `WAYLAND_DISPLAY` set to a headless host
(never your live session). See [docs/m2-plan.md](docs/m2-plan.md).

## Performance

The desktop is built like a browser engine: retained scene, aggressive caching,
precompute and prewarm, nothing slow on the paint path. See
[docs/performance.md](docs/performance.md).

## Workflow

`master` is always buildable. Work happens on `feat/*` branches, committed often,
merged with `--no-ff` so features stay visible in history.
