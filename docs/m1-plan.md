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
