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
| `ipc` | Typed protocol shared by every service (length-prefixed postcard over a unix socket) |
| `ctl` | `auroractl`: debug and scripting client for the IPC |
| `ui` | Shared UI toolkit: `Painter` trait, tiny-skia + cosmic-text into wl_shm (a wgpu backend can follow) |
| `theme` | Palette, fonts, motion curves, pushed live to services |
| `shell` | `aurora-shell`: bar (workspaces, focused title, clock) |
| `launcher` | `aurora-launcher`: app launcher daemon, toggled with `aurora-launcher toggle` |
| `notifd` | `aurora-notifd`: `org.freedesktop.Notifications` |
| `lock` | `aurora-lock`: `ext-session-lock` screen locker |
| `settingsd` | Config daemon + GUI |
| `term` | `aurora-term`: terminal (alacritty_terminal emulation, tiny-skia cell renderer first, GPU backend later) |
| `files` | `aurora-files`: file manager |

Crates are added as their milestone is reached.

## Milestones

- [x] **M0** Nested compositor under an existing desktop (winit backend), one client shows
- [x] **M1** Real session: DRM backend on the 4090, libinput, launch from a TTY
- [x] **M2** Usable: tiling, workspaces, XWayland, layer-shell, config, multi-monitor. Verified on hardware
- [x] **M3** The look: animation engine, rounded corners, shadows, blur, live overview, X11 scaling. Verified on hardware
- [x] **M4** Services: `ipc`, `theme`, `ui`, shell bar, launcher, notifd, lock. Verified on hardware. Plan in [docs/m4-plan.md](docs/m4-plan.md)
- [ ] **M5** `term` and `files`. Plan in [docs/m5-plan.md](docs/m5-plan.md)

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
| Mod+Tab | live workspace overview (Escape closes, click focuses, drag a thumbnail to another workspace to move it) |
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

Known limits: there is no ext-workspace protocol (M4 services read workspaces over the
IPC instead; ext-foreign-toplevel-list is served). X11 apps are unscaled by default, so they look
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

## Using it (M4)

Status: the compositor side (IPC, supervision, theme, session lock) is in; the service
binaries land per stream, see [docs/m4-plan.md](docs/m4-plan.md). Nothing is started for
you, everything below is opt-in.

**Services.** A `[services.<name>]` table in `config.toml` makes Aurora start and supervise
a helper after its socket exists (keys: `command`, `enabled`, `autostart`, `restart`
`never|on-failure|always`, `backoff_ms`, `max_backoff_ms`; commented examples in
[config/aurora.example.toml](config/aurora.example.toml)). A dying service is restarted with
doubling backoff, a reload never starts a running one twice, and all of them are stopped
when the compositor exits. Children get `WAYLAND_DISPLAY` and `AURORA_IPC_SOCK`. Example:

```toml
[services.shell]
command = "aurora-shell"
restart = "always"
[services.launcher]
command = "aurora-launcher"
restart = "always"
[services.lock]          # started only by the `lock` action, never at login
command = "aurora-lock"
autostart = false
restart = "always"       # restarted only while the session is locked
```

**Binds.** `"Mod+space" = "spawn aurora-launcher toggle"` and `"Mod+l" = "lock"` are the
suggested ones (not bound by default). While locked every bind except the emergency chords
is refused, and a dead lock client leaves the session locked.

**IPC socket.** `$XDG_RUNTIME_DIR/aurora/ipc.sock` (directory 0700, same-uid peers only),
overridden by `AURORA_IPC_SOCK`. Clients get a full snapshot on connect, then deltas;
topics are workspaces, windows, focus, outputs, theme and config. Slow subscribers are
coalesced, resynced with a fresh snapshot, and finally dropped; they never stall the
compositor.

**auroractl.** `auroractl snapshot` prints outputs, workspaces, windows (with titles) as
JSON, `auroractl events [topic...]` prints one JSON event per line, and
`auroractl raw '{"SwitchWorkspace":{"output":null,"index":2}}'` sends any request
(`'"ListWindows"'`, `'"ReloadConfig"'`, `'"Lock"'`, ...).

**Theme.** `theme.toml` next to `config.toml` (sections `[palette]`, `[fonts]`, `[shape]`,
`[motion]`, every key optional, colors as `#rrggbb[aa]`). Reload (Super+Shift+r or
SIGUSR1) re-reads it and pushes the new theme to every service, which repaint without a
restart. A syntax error keeps the current theme and logs why.

**notifd and dunst.** `aurora-notifd` speaks `org.freedesktop.Notifications`; run it
only when you mean to replace dunst. QA uses a private `dbus-daemon`, never your session bus.

Scripted checks: `scripts/qa-nested.sh ipc services theme shell launcher notifd lock`
(same rules as before: a headless host, never your live session). A scenario whose binary
is not built is skipped. Fullscreen windows hide the bar (Top layer); the launcher and
toasts use the Overlay layer.

## Using it (M5)

Status: in progress, plan in [docs/m5-plan.md](docs/m5-plan.md). The `ui` additions the apps
need (xdg-toplevel surfaces, clipboard and primary selection, cursor shape, monospace cell
metrics) are in. `aurora-term` and `aurora-files` land per stream; until a binary exists
below, its section describes the plan, not something you can run yet. Neither is started for
you: they are ordinary windows, one process per window, launched by a bind or the launcher.

**Binds.** Suggested, not bound by default (put them in `[keybinds]`; `Mod+q` is `spawn kitty`
unless you rebind it):

```toml
"Mod+q" = "spawn aurora-term"
"Mod+e" = "spawn aurora-files"
```

**aurora-term** (not yet merged). Terminal on `alacritty_terminal` emulation, rendered with
tiny-skia and cosmic-text into wl_shm (the "GPU terminal" backend is a later replacement of
the draw loop only). Flags: `-e cmd args...`, `--cwd`, `--title`, `--class`, `--hold`,
`--scrollback N`. Without `-e` it runs `$SHELL`. Copy and paste with Ctrl+Shift+C / Ctrl+Shift+V,
selecting also fills the primary selection (middle-click pastes). Not in v1: ligatures, kitty
keyboard and graphics protocols, sixel, tabs and splits (the compositor tiles), search in
scrollback, IME.

**aurora-files** (not yet merged). File manager, list view only: `aurora-files [dir]`
(default `$HOME`). Ctrl+L path bar, Ctrl+H hidden files, `/` filter, F2 rename, Delete moves
to the FreeDesktop trash (home volume only), Shift+Delete deletes permanently after a
confirm, copy/cut/paste also over the Wayland clipboard as `text/uri-list`. It refuses to
operate on `/` or `$HOME` itself. Not in v1: thumbnails, grid view, tabs, archives, network
mounts, trash restore UI.

**Theme and IPC.** Both follow `theme.toml` live (monospace font from `[fonts]`) and repaint
without a restart. Neither needs the IPC socket: without it they read `theme.toml` once
and spawn through plain `Command`.

**Test hooks.** Builds with the `qa-hooks` cargo feature (QA only, never a default build)
read `AURORA_FILES_TEST_SCRIPT` / `AURORA_TERM_TEST_INPUT` to fake input.

Scripted checks: `scripts/qa-nested.sh term files` against a headless host, never your live
session. They skip whatever is not built, and run the key-script and input-file parts only
for `qa-hooks` builds (`cargo build --features qa-hooks` in the app crate). The scenarios
were written from the plan before the binaries existed; check the PASS/FAIL list once the
apps land.

## Protocols

Served by `aurora-comp` beyond core `wl_*`, xdg-shell and XWayland:

| Area | Protocols |
|---|---|
| Buffers and sync | linux-dmabuf (with scanout feedback), linux-drm-syncobj (explicit sync), single-pixel-buffer, viewporter, fractional-scale |
| Presentation and pacing | presentation-time, fifo-v1, commit-timing-v1 (barriers released per output on vblank / frame target), content-type-v1 (stored per window for on-demand VRR), alpha-modifier-v1 (multiplied into fades, inactive opacity and the shadow) |
| Shell | xdg-output, xdg-decoration (server side), xdg-activation, xdg-foreign-v2 (portal dialogs get their parent), xdg-dialog-v1 (modal dialogs float centred on the parent), xdg-toplevel-icon-v1 (stored), xdg-system-bell-v1 (logged), wlr-layer-shell, ext-session-lock, ext-foreign-toplevel-list, cursor-shape |
| Input | relative-pointer, pointer-constraints, pointer-gestures, tablet-v2 (tools, no pads), keyboard-shortcuts-inhibit, text-input-v3, input-method-v2 (IME popups placed at the cursor), virtual-keyboard (through the bind table) |
| Data | data-device, primary-selection, wlr-data-control |
| Capture and idle | ext-image-copy-capture (outputs, shm), idle-inhibit, ext-idle-notify |
| Sandboxing | security-context-v1 |

Clients that connect through a security context (Flatpak) do not see data-control, virtual
keyboard, input method, image capture, session lock, layer shell, foreign toplevel list or
the security context manager itself; the policy table is in `crates/comp/src/sandbox.rs`.

## Performance

The desktop is built like a browser engine: retained scene, aggressive caching,
precompute and prewarm, nothing slow on the paint path. See
[docs/performance.md](docs/performance.md).

## Workflow

`master` is always buildable. Work happens on `feat/*` branches, committed often,
merged with `--no-ff` so features stay visible in history.
