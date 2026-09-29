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

- [ ] **M0** Nested compositor under an existing desktop (winit backend), one client shows
- [ ] **M1** Real session: DRM backend on the 4090, libinput, launch from a TTY
- [ ] **M2** Usable: tiling, workspaces, XWayland, layer-shell
- [ ] **M3** The look: animation engine, blur/shadows, live overview
- [ ] **M4+** Services: `ipc`, shell, launcher, notifd, lock, then `term` and `files`

## Workflow

`master` is always buildable. Work happens on `feat/*` branches, committed often,
merged with `--no-ff` so features stay visible in history.
