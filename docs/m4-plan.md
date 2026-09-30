# M4 plan: services (ipc, theme, ui, shell, launcher, notifd, lock)

Read README.md, docs/performance.md, docs/m2-plan.md and docs/m3-plan.md first. `term` and `files` are M5. Every step leaves `cargo build`, `cargo clippy --all-targets` and `cargo test --workspace` warning-free/passing. Agents never run the compositor or a service on the live session (`wayland-1`, never `--drm`, never signal by name, never execute binaries from target/ by hand). QA runs only against the headless weston host (`weston --backend=headless --socket=aurora-qa`) through scripts.

## Architecture decisions

- **Process model:** the compositor stays small. shell (bar), launcher, notifd and lock are separate binaries, each a Wayland client (layer-shell / ext-session-lock) plus an IPC client. A crashing service never takes the session down. A dead lock client never unlocks.
- **Rendering: `ui` is backend-agnostic with a `Painter` trait; the first and default backend is tiny-skia + cosmic-text into wl_shm** (double/triple-buffered pool, damage regions, fractional scale via wp-fractional-scale + viewporter). Reasons: headless QA under weston with pixel-exact screenshots, safest for the lock screen, a few MB per always-warm process, idle cost zero. A wgpu backend can be added later for GPU-heavy surfaces (this deviates from the README's "wgpu + cosmic-text"; README updated). Glyph/shaping cache is per process, byte-budgeted with a tracing metric per docs/performance.md.
- **IPC:** unix stream socket `$XDG_RUNTIME_DIR/aurora/ipc.sock` (dir 0700, `AURORA_IPC_SOCK` override for QA, stale socket unlinked, `SO_PEERCRED` uid check). Wire: u32 LE length prefix + postcard of `Frame { id, body }`, frames capped at 1 MiB. First frame is `Hello { proto_version, client }`; enums only ever grow at the end. Bodies: `Request/Response/Error` (with id), `Event`, `Subscribe/Unsubscribe(topics)`. Topics: Workspaces, Windows, Focus, Outputs, Theme, Config. Connect yields a full snapshot then deltas. The compositor is the hub: per-client bounded queues, slow subscribers coalesced/dropped, non-blocking calloop sources, never on the paint path. Broadcast by diffing a cheap snapshot in the main loop closure (next to `finish_slides`), not by hooking every mutation. Large buffers by SCM_RIGHTS fd only. `auroractl` (new tiny crate) speaks JSON lines to a debug subcommand and prints snapshots/events.
- **Requests a bar/launcher needs:** switch workspace, focus window, close window, list windows/outputs, spawn (through the compositor's `spawn`, with activation token), reload config, open/close overview, set theme.
- **Title/app_id:** `WinData` gains a title; implement `XdgShellHandler::title_changed/app_id_changed` and the X11 equivalent so events fire.
- **Protocols added to the compositor:** `ext-session-lock` (Smithay `session_lock`), `ext-foreign-toplevel-list` (Smithay). Not added: wlr-foreign-toplevel, ext-workspace (IPC replaces them for our own services; revisit for third-party bars).
- **Session lock:** `Aurora.lock: Option<LockState>`; while locked: `output_elements` returns only the lock surface (or opaque black) before anything else, binds refused (`binds_allowed` false), keyboard focus forced to the lock surface, `hit_test` returns lock surface or nothing, overview/drags/activation guarded, emergency chords (quit, VT) still work. `lock()` is confirmed only after every output has a committed lock surface. Lock client death keeps the session locked (black), and a new lock client may take over.
- **Launcher:** Overlay layer, Exclusive keyboard interactivity (already supported; fullscreen hides Top layers, so not Top). App index built at startup from desktop entries (`freedesktop-desktop-entry`), fuzzy match tables in memory, UI surface created once and shown/hidden (performance.md rule 5), launches via IPC `Spawn`.
- **Shell (bar):** Top layer, exclusive zone, per output; workspaces widget and focused title from IPC, clock, blur-friendly translucent background (compositor blurs behind Top layers), theme pushed live. Quick settings and workspace-overview widgets later in M4 only if time; not required.
- **notifd:** implements `org.freedesktop.Notifications` with `zbus`, renders toasts as Overlay/Top layer surfaces with actions/expire/replace/close semantics. `dunst` owns the name on the user's session: notifd uses `--replace` only when asked; QA and agents use a private `dbus-daemon`, never the session bus.
- **Services supervision:** `[services]` config section (name, command, restart policy with backoff, enabled). The spawn reaper thread posts exits into a calloop channel; restart happens on the loop. Reload never double-starts. Exports `AURORA_IPC_SOCK` via `spawn_env`. The lock service is started on demand by the `lock` action and never auto-restarted into an unlocked state.
- **Theme:** `theme` crate = palette, fonts, radius/gaps, motion curves (reuses anim curve text format), TOML loaded by the compositor and pushed as `Event::Theme` snapshot + deltas; services repaint without restart.
- **Tests only for pure logic:** ipc framing/versioning, theme parsing, fuzzy matcher, notification spec state machine, ui layout/text cache budget, compositor snapshot diff, lock state machine. GUI behaviour is covered by QA scenarios against weston.

## Log contract additions

```
ipc: listening path=<p>                 ipc: client connected name=<n> proto=<v> | ipc: client gone name=<n>
ipc: broadcast topic=<t> clients=<n>    service: started name=<n> pid=<p> | service: exited name=<n> code=<c> restart=<ms>|none
lock: requested | lock: surface output=<n> | lock: locked | lock: unlocked
shell: ready outputs=<n>   launcher: ready apps=<n>   launcher: show|hide   notifd: ready name=<owner>   notifd: shown id=<n>
```
SIGUSR2 dump gains `dump: ipc clients=<n>`, `dump: lock state=unlocked|locking|locked surfaces=<n>`, `dump: service <name> pid=.. restarts=..`.

## Phases and streams

**Phase 1 (parallel, independent files):**
- **P1-A `ipc` + `theme` crates** (pure: types, framing, versioning, theme parse, tests) and the `auroractl` skeleton.
- **P1-B compositor session-lock** (protocol, `LockState`, render/input/focus guards, dump, tests of the pure state machine).
- **P1-C `ui` crate** (Painter trait, tiny-skia backend, cosmic-text text + byte-budgeted glyph cache, basic widgets/layout, sctk layer-shell + session-lock surface runner with shm pool and fractional scale, headless pixel-render tests that need no Wayland, a demo-free public API). Takes colors as plain values (theme wiring happens in phase 3).

**Phase 2 (after phase 1 merged):**
- **P2-A compositor IPC server + snapshot/events + title tracking + ext-foreign-toplevel-list + service supervision + `spawn_env`** using the `ipc` crate.

**Phase 3 (parallel, after phase 2 merged; one crate each, disjoint files):**
- **shell** (bar), **launcher**, **notifd**, **lock** (client + `lock` action glue), **QA + docs** (new scenarios per service against weston; private dbus-daemon for notifd; README/performance updates).

**Phase 4 (serial):** merge, resolve conflicts, full build/clippy/tests, release build, run QA under weston, hardware checklist.

## Hardware checklist (real machine, from a TTY)

- [ ] shell bar shows per-output workspaces, focused title and clock on all three outputs; exclusive zone respected; fullscreen game hides it.
- [ ] launcher opens instantly on the keybind, types, launches, closes; no focus loss afterwards.
- [ ] notifd replaces dunst (only when started deliberately), toasts show, actions and expiry work.
- [ ] lock: locks all outputs, windows never visible, pointer/keys blocked, correct unlock, `Ctrl+AltGr+BackSpace` and VT switch still work while locked, killing the lock client keeps the session locked.
- [ ] killing shell/launcher/notifd never disturbs windows; supervision restarts them with backoff.
- [ ] theme change reaches all services live.
