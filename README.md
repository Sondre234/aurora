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

## Daily session

Aurora as the session you log into from SDDM, next to Hyprland. `scripts/aurora-test.sh`
stays the tool for hand tests from a TTY (timeout, merged test config).

**Install.** `scripts/install.sh` (try `--dry-run` first; it is idempotent, rerun it after a
pull) builds the release workspace and installs, for your user only:

| What | Where |
|---|---|
| `aurora-comp`, `auroractl`, `aurora-shell`, `aurora-launcher`, `aurora-notifd`, `aurora-lock`, `aurora-term`, `aurora-files`, `aurora-session` | `${PREFIX:-~/.local}/bin` |
| [contrib/portals/aurora-portals.conf](contrib/portals/aurora-portals.conf) | `~/.config/xdg-desktop-portal/` |
| [contrib/systemd/aurora-session.target](contrib/systemd/aurora-session.target) | `~/.config/systemd/user/` (then `daemon-reload`) |

**The sudo step.** SDDM only reads `/usr/share/wayland-sessions`, so install.sh writes
`target/aurora.desktop` (from [contrib/aurora.desktop](contrib/aurora.desktop), with `Exec=`
set to the absolute path of the installed `aurora-session`) and prints the one command to
run yourself:

```sh
sudo install -Dm644 target/aurora.desktop /usr/share/wayland-sessions/aurora.desktop
```

Rerun it only when install.sh says the entry changed (it compares the two). Then pick
"Aurora" in SDDM's session menu.

**What a login runs.** `aurora-session` logs everything to `~/.local/state/aurora/aurora.log`
(the previous login's is `aurora.log.old`; the compositor's own log is `comp.log` next to
it), exports `XDG_SESSION_TYPE=wayland`, `XDG_CURRENT_DESKTOP=Aurora`,
`XDG_SESSION_DESKTOP=aurora`, and runs `aurora-comp --drm --no-timeout` with your real
`~/.config/aurora/config.toml`. It stays alive as the compositor's parent so it can clean up
however the compositor exits.

**Environment and systemd.** Once its socket exists, Aurora runs
`dbus-update-activation-environment --systemd WAYLAND_DISPLAY DISPLAY XDG_CURRENT_DESKTOP
XDG_SESSION_TYPE XDG_SESSION_DESKTOP AURORA_IPC_SOCK` (non-blocking, reaped, killed after
10 s; `session: environment import: ok` or the failure in comp.log), and again if XWayland's
`DISPLAY` only becomes known later. DRM sessions only, never nested; turn it off with
`[session] import_environment = false`. The design follows sway's `sway-session.target`
and niri's `niri-session`: `graphical-session.target` refuses a manual start, so
`aurora-session.target` binds to it. The launcher clears stale session variables from
the systemd user manager (Hyprland's, or a crashed Aurora's), waits until Aurora's import
shows up there, then starts `aurora-session.target`. When the compositor exits it stops
the target, which stops `graphical-session.target` and every unit bound to it, and clears
the variables again. So user units `WantedBy=graphical-session.target` start with Aurora
and see its `WAYLAND_DISPLAY`. For the polkit agent:

```sh
systemctl --user enable hyprpolkitagent.service
```

(Hyprland sessions start it the same way, so enabling it is harmless there.)

**Portals.** `aurora-portals.conf` is picked because `XDG_CURRENT_DESKTOP=Aurora`:
`xdg-desktop-portal-gtk` for everything by default, `xdg-desktop-portal-wlr` for ScreenCast
and Screenshot, `gnome-keyring` for Secret. Install `xdg-desktop-portal-gtk`,
`xdg-desktop-portal-wlr` and `gnome-keyring` (or point Secret at `kwallet` in the file).
Screen sharing through xdg-desktop-portal-wlr needs a capture protocol it speaks; Aurora
offers ext-image-copy-capture, not wlr-screencopy, so this needs a portal-wlr release with
ext-image-copy-capture support (not yet verified on hardware).

**Daily config.** [contrib/config.daily.toml](contrib/config.daily.toml) is a complete,
tested example to merge into your `config.toml`: the shell, launcher, notifd and lock
services, `Mod+space` launcher, `Mod+l` lock, volume/mic/media keys (`wpctl`, `playerctl`),
backlight keys (`brightnessctl`), `Print` / `Shift+Print` screenshots (`grim`, `slurp`,
`wl-copy`), a hyprpolkitagent fallback that only runs when its systemd unit is not enabled,
and this idle recipe:

```sh
swayidle -w timeout 300 'auroractl raw "\"Lock\""' \
    timeout 600 'wlopm --off "*"' resume 'wlopm --on "*"' \
    before-sleep 'auroractl raw "\"Lock\""'
```

`auroractl raw '"Lock"'` asks the compositor to start `[services.lock]`, exactly like the
`lock` bind (it fails with Unsupported when no lock service is configured). Monitors
off/on need wlr-output-power-management-v1 in Aurora (with the `power-off-monitors` /
`power-on-monitors` actions, part of the display work) and `wlopm` from the AUR.

**Input.** `config.toml` now has `[input.keyboard]` (`layout`, `variant`, `model`,
`options`, `rules`, `repeat_rate`, `repeat_delay`), `[input.pointer]` and
`[input.touchpad]` (`accel_profile` flat|adaptive, `accel_speed`, `natural_scroll`,
`left_handed`, `scroll_method`; touchpads also `tap` and `dwt`). Without XKB fields the
keymap comes from `keymap.xkb`, `XKB_DEFAULT_*` or `/etc/X11/xorg.conf.d/00-keyboard.conf`
as before. A reload applies all of it live: a changed keymap is swapped for every client,
repeat info is resent, and libinput settings go to every device (and to devices plugged in
later). Defaults and ranges are in [config/aurora.example.toml](config/aurora.example.toml).

**Leaving.** `Mod+m` (or Ctrl+Alt+BackSpace) quits back to SDDM. If a login fails, read
`~/.local/state/aurora/aurora.log` and `comp.log` from another session or a TTY.

## Performance

The desktop is built like a browser engine: retained scene, aggressive caching,
precompute and prewarm, nothing slow on the paint path. See
[docs/performance.md](docs/performance.md).

## Workflow

`master` is always buildable. Work happens on `feat/*` branches, committed often,
merged with `--no-ff` so features stay visible in history.
