# M1 plan: real session (DRM/KMS backend)

Single-GPU design: plain GlesRenderer on the RTX 4090, no renderer_multi, no
use_system_lib, no XWayland, no DRM leasing. Multi-GPU (monitor on the iGPU) is
a documented non-goal. Verification is cargo build / clippy / static reading
only; never run --drm while the Hyprland session is live.

Global rules: keep panic=unwind, never process::exit on the normal path, no
unwrap/expect after the session is open, follow docs/performance.md.

## Steps

1. Backend abstraction, CLI, logging. `Backend` enum {Winit, Drm} in Aurora
   state, winit moved behind it, cli parsing (--winit/--drm/--timeout/--no-timeout/-c),
   log file + panic hook, timeout timer, signals.
2. Cargo features and input plumbing. smithay features, drm-extras/xcursor deps,
   libseat session, udev, libinput, keymap resolution, relative pointer + clamp,
   shortcut filter (quit chord, VT switch).
3. DRM device, GBM/EGL renderer, outputs. Primary GPU pick, DrmOutputManager,
   DrmScanner, wl_output per connector, highest-refresh mode.
4. Render loop. vblank pacing, frame callbacks, presentation feedback,
   hardware cursor, hotplug.
5. dmabuf + explicit sync. Feedback, syncobj, pre-commit blocker.
6. Safety hardening. Watchdog, pause/resume, drop order, docs.

See the per-step instructions handed to implementers; key decisions:

- Primary GPU: AURORA_DRM_DEVICE override, else prefer sysfs driver nvidia, else
  udev primary_gpu. Other nodes skipped with a warning.
- Overlay planes cleared on nvidia. Formats Abgr2101010, Argb2101010, Abgr8888,
  Argb8888; AURORA_DISABLE_10BIT escape hatch.
- Keymap: AURORA_XKB_FILE or ~/.config/aurora/keymap.xkb, then XKB_DEFAULT_*,
  then /etc/X11/xorg.conf.d/00-keyboard.conf, then us. Bad keymap falls back.
- Quit chord matched by raw syms (BackSpace, Terminate_Server) and keycode; VT
  switch by XF86Switch_VT_n or ctrl+alt+Fn; never gated by shortcut inhibitors.
- Safety: timeout timer (DRM default 120s), SIGINT/TERM/HUP source, watchdog
  thread that _exit()s at timeout+5s, log rotated to comp.log.1.
- Drop order: state before event loop (seat lives in the session notifier).
- Session inactive flag gates all rendering; resume re-renders and rescans.

## Running the DRM backend

Only from a real TTY login (Ctrl+Alt+F3, log in, run `aurora-comp --drm`), never from
inside another compositor. Prefer `--timeout <secs>` on every run.

- Session: libseat. With `seatd.service` inactive it falls back to logind, which needs
  the TTY login above. To use seatd instead: `systemctl enable --now seatd` and add the
  user to the `seat` group (re-login).
- `--drm` defaults to a 120s timeout; `--no-timeout` disables it and logs a warning.
  Auto mode picks nested winit if WAYLAND_DISPLAY or DISPLAY is set, otherwise DRM.
- Logs: stderr plus `~/.local/state/aurora/comp.log` (previous run in `comp.log.1`).
  Read it over ssh or from another TTY after a black screen.

### Safety features and escape hatches

- Ctrl+Alt+Backspace quits, handled before clients see the key.
- Ctrl+Alt+F1..F12 switches VT through the libseat session.
- `--timeout` stops the loop; a watchdog thread hard-exits at timeout+5s if the loop is
  wedged (it never touches DRM; closing the seat fds hands the VT back).
- `kill -TERM <pid>` (or INT/HUP) from ssh stops the loop cleanly.
- Panic hook logs message and backtrace; profiles keep panic=unwind so Drop guards run.
  Drop order: outputs, DrmOutputManager/DrmDevice, renderer, then the seat session.
- Session disable pauses libinput and every DRM device and cancels pending frames;
  enable resumes them, rescans connectors (hotplug while away) and re-damages all outputs.
  Started on an inactive VT, it stays dark until the session is enabled.

### Environment variables

- `AURORA_DRM_DEVICE`: force the primary GPU node (e.g. `/dev/dri/card1`).
- `AURORA_XKB_FILE`: keymap file; else `~/.config/aurora/keymap.xkb`, then `XKB_DEFAULT_*`,
  then `/etc/X11/xorg.conf.d/00-keyboard.conf`, then plain us.
- `AURORA_DISABLE_10BIT`: restrict scanout to 8 bit formats.
- `XCURSOR_THEME`, `XCURSOR_SIZE`: cursor theme and size (default theme, 24).
- `RUST_LOG`: log filter (default `info`).

### Known limits

- With `nvidia_drm.modeset=1 fbdev=1` an unclean teardown can leave a black VT. That is
  why runs should use `--timeout`; if it happens, switch VT away and back or ssh in.
- Multi-GPU (a monitor on the iGPU) is deferred; non-primary GPUs are skipped.
