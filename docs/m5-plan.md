# M5 plan: apps (term, files)

Read README.md, docs/performance.md and docs/m4-plan.md first. M5 adds the two first-party apps `term` (terminal) and `files` (file manager). Every step leaves `cargo build`, `cargo clippy --all-targets` and `cargo test --workspace` warning-free/passing. Agents never run the compositor or a service on the live session (`wayland-1`, never `--drm`, never signal by name, never execute binaries from target/ by hand). QA runs only against the headless weston host (`weston --backend=headless --socket=aurora-qa`) through scripts, and a scenario whose binary is not built is skipped.

## Architecture decisions

- **Process model:** `aurora-term` and `aurora-files` are ordinary xdg-toplevel Wayland clients (tiled by the compositor like any window), plus optional IPC clients. Unlike M4 services they are not daemons: one process per window, started by a bind or the launcher. A crash loses one window only. Neither needs the IPC to work: with no socket they still run (theme from `theme.toml`, spawn via `xdg-open`/`Command`).
- **Rendering: tiny-skia + cosmic-text through `ui` into wl_shm; no wgpu in M5.** The README says "GPU terminal". Deviation, same reasoning as M4: the `ui` crate has no GPU backend, headless weston QA needs pixel-exact shm output, and a terminal is cheap on the CPU if it is cell-based, damage-tracked and cached. "GPU" stays the long-term target: the term renderer is written against `Painter` plus a cell-grid layer so a wgpu glyph-atlas backend can replace the draw loop later without touching emulation or input. The README crate table is updated to say so. Revisit only if the term perf budget below is missed on hardware.
- **Missing toolkit pieces (phase 0, shared):** `ui::runtime` today only creates layer and session-lock surfaces. Both apps need (1) an `xdg_toplevel` surface kind (title, app_id, min size, configure/resize, close request, fullscreen/maximize requests, optional xdg-decoration "server side" request since Aurora draws borders), (2) clipboard and primary selection via `wl_data_device` and `zwp_primary_selection` (offer text, receive text on a calloop fd, no blocking read), (3) key repeat that works for toplevels (rate/delay from `wl_keyboard.repeat_info`, driven by calloop), (4) cursor shape (`wp-cursor-shape`, falling back to themed cursor), (5) a monospace cell-metric helper in `ui::text` (cell width/height from the theme mono font at the surface scale, snapped to whole physical pixels). These land first as one small stream and are the only edits to `crates/ui` in M5.
- **Theme:** `aurora-theme` palette and `fonts.mono_family/mono_size` are used as is. Both apps subscribe to `Topic::Theme` and repaint without restart; with no IPC they read `theme.toml` once. The 16 ANSI colors are derived from the palette in a pure function in each crate (term only: `ansi16(&Palette) -> [Rgba; 16]`, with `[term.colors]` overrides in `theme.toml`/`config` deliberately out of scope). A theme change only invalidates the glyph cache and repaints everything once.
- **IPC use:** Spawn requests go through `Request::Spawn { argv }` (compositor activation token, no shell). Fallback when the socket is absent: `std::process::Command` detached. `term` sets the window title through xdg only; the bar already reads titles from the compositor. No new IPC messages are needed in M5; if a stream finds it needs one, it proposes it in its notes and the integration phase adds it at the end of the enums.

### term (`crates/term`, binary `aurora-term`)

- **Emulation:** `alacritty_terminal` (grid, scrollback, selection, damage, vte parser, xterm modes, bracketed paste, alt screen, mouse reporting, OSC title, OSC 52 write only). Chosen over raw `vte` because it supplies the grid, resize/reflow, selection and damage tracking that would otherwise be thousands of lines. Pinned to an exact version in `crates/term/Cargo.toml`. A thin `Backend` wrapper owns `Term<Listener>`, so the rest of the crate never names alacritty types in its public API (replaceable, and unit-testable with fakes).
- **PTY:** `rustix` (openpt/grantpt/unlockpt, `fork`-free: spawn with `std::process::Command` + `pre_exec` doing `setsid`, `TIOCSCTTY`, dup2), nonblocking master fd registered as a calloop source, reads capped per wakeup (64 KiB) so output floods never starve input or painting. Resize via `TIOCSWINSZ`. Child reaped by pid; exit code shown, window closes on exit unless `--hold`. Environment: `TERM=xterm-256color`, `COLORTERM=truecolor`, `TERM_PROGRAM=aurora-term`; the shell is `$SHELL` or `-e cmd args...`.
- **Painting:** cell grid to pixels. Per-process glyph cache keyed (char, bold, italic, fg-independent coverage mask) with a byte budget and a `perf` metric per performance.md; fg/bg applied at blit. Damage from the emulator's line damage becomes shm damage rects; idle terminal costs zero frames. Output is coalesced to at most one repaint per frame callback. Cursor blink is a calloop timer that repaints only the cursor cell and stops when unfocused.
- **Input:** a pure `encode_key(KeyEvent, Modes) -> Vec<u8>` (legacy xterm encoding: arrows with application-cursor mode, F1-F20, modifiers as `CSI 1;m`, Alt as ESC prefix, Ctrl letters, Home/End/PgUp/PgDn/Ins/Del, keypad). No kitty keyboard protocol in M5. Paste wraps in bracketed-paste markers when the app enabled them and strips embedded `ESC[201~`. Mouse reporting (X10, SGR 1006) when enabled; otherwise the mouse selects.
- **Selection and clipboard:** drag, double-click word, triple-click line, shift-click extend; selection also owns the primary selection (middle-click paste). Ctrl+Shift+C / Ctrl+Shift+V use the clipboard. Scrollback 10 000 lines default (`--scrollback N`, cap 200 000), mouse wheel and Shift+PgUp/PgDn scroll, any key press snaps to the bottom.
- **Window:** xdg-toplevel, title from OSC 0/2 (default `aurora-term`), app_id `aurora-term`, size snapped to whole cells in `configure` (the compositor tiles, so a non-multiple remainder is padded with the bg color), padding 4 px. `--class`, `--title`, `--cwd`, `-e cmd...`.
- **In scope for v1:** everything above plus truecolor, 256 colors, bold/italic/underline/inverse/strikethrough, wide (CJK) cells and combining marks via cosmic-text shaping per cell run, alt screen, DECSET mouse/focus-reporting modes alacritty supports, OSC 8 hyperlinks stored but opened only on Ctrl+click through `xdg-open`.
- **Out of scope:** ligatures, kitty keyboard/graphics protocol, sixel, tabs/splits (the compositor tiles), IME/text-input-v3, search in scrollback, config file beyond the theme, OSC 52 read, per-app color overrides, wgpu backend, background blur or opacity beyond the theme `bg` alpha (compositor blurs behind translucent windows already).
- **Perf budget:** `cat` of a 100 MB file finishes with the compositor frame time unaffected; idle terminal logs no repaints; steady-state RSS under 150 MB with default scrollback; paint of a full 200x60 screen under 4 ms in release (measured, logged as `term: frame ms=<n>` at debug only).

### files (`crates/files`, binary `aurora-files`)

- **Model/view split:** a pure model (`Listing`, `Selection`, `Sort`, `History`, `Clipboard`/`Op` queue) with no I/O in its core types, a thin `fs` layer, and a `ui` view. All state transitions are plain functions, testable without a Wayland connection.
- **Listing:** read in a worker thread (result through a calloop channel, generation-tagged so stale reads are dropped), entries carry name, kind (dir/file/symlink/broken), size, mtime, mode, hidden flag. Watch the current directory with `notify` (already in the workspace via launcher) and apply debounced refresh. Directories of 100 000 entries must scroll smoothly: the list is virtualized (only visible rows are laid out and painted); sorting runs on the worker.
- **Navigation:** path bar (editable, Ctrl+L), back/forward/up history, Enter/double-click opens (dir: navigate; file: open), typeahead jump, Home/End/PgUp/PgDn, sidebar of places (home, XDG user dirs from `user-dirs.dirs`, `/`, mounted volumes read from `/proc/self/mountinfo` once at start and on mountinfo change). Last directory is not persisted in M5.
- **Selection:** single, Ctrl-toggle, Shift-range, rubber-band optional (out if time is short), select all, invert.
- **Open:** files go through `xdg-open` (spawned via IPC `Spawn`, fallback `Command`); "Open with" is out. Directories open in place; Ctrl+Enter opens a directory in a new `aurora-files` window; "Open terminal here" spawns `aurora-term --cwd <dir>`.
- **Operations:** copy, cut/move, paste, rename (inline editor, F2), new folder, new file, delete to trash (FreeDesktop trash spec: `$XDG_DATA_HOME/Trash/{files,info}` with `.trashinfo`, home-volume only; cross-device trash is refused with an error toast, not silently permanently deleted), permanent delete behind a confirm (Shift+Delete), undo of the last trash/rename/move in-session. File operations run on a worker with progress, cancel, and conflict policy (ask: skip / replace / keep both, "apply to all"). Copy preserves mode and mtime, symlinks are copied as symlinks, a move across filesystems is copy then verified delete. Operations never follow symlinks out of the tree on delete.
- **Clipboard:** internal cut/copy buffer plus `text/uri-list` and `x-special/gnome-copied-files` on the Wayland clipboard so other apps can paste files; reading others' uri-lists on paste. Pure encode/decode functions.
- **Sorting and filtering:** by name (natural order, case-insensitive, dirs first), size, mtime, kind; ascending/descending; hidden toggle (Ctrl+H); inline filter on `/` (substring, live).
- **Views:** list view with columns (name, size, modified) for v1; icons come from `freedesktop-icons` through the same resvg/png path the launcher uses, pre-rasterized at the display scale into a byte-budgeted LRU. Thumbnails, grid view, and tabs are out.
- **Window:** xdg-toplevel, app_id `aurora-files`, title is the current path. Theme live via IPC.
- **Out of scope:** network/MTP/gvfs, archives, thumbnails, grid/column views, tabs, drag and drop between windows, trash restore UI (the files are in the spec-compliant trash so other tools can restore), permissions editor, search across directories, bookmarks editing, persistence of settings, root/polkit operations.
- **Safety rules inside the app:** destructive operations never run from a QA scenario outside the scenario's scratch directory; the crate rejects any operation whose source is `/` or `$HOME` itself and refuses to trash/delete a mount point.

## Log contract additions

```
term: ready cols=<c> rows=<r> scale=<s> font=<family> cell=<w>x<h>
term: spawn pid=<p> cmd=<argv0>          term: exit pid=<p> code=<c>|signal=<n>
term: resize cols=<c> rows=<r>           term: title <text>        term: theme rev=<n>
term: clipboard set bytes=<n> | term: paste bytes=<n>
files: ready path=<p> entries=<n>        files: navigate path=<p> entries=<n>
files: select count=<n>                  files: open path=<p> via=xdg-open|navigate
files: op start id=<n> kind=copy|move|trash|delete|rename|mkdir items=<n>
files: op done id=<n> ok=<n> failed=<n>  files: theme rev=<n>
```
SIGUSR2 dump (same hook the M4 services use where present) gains `dump: term cols=.. rows=.. scrollback=..` and `dump: files path=.. entries=.. selected=..`. These lines go to stderr through `tracing` at info, in the same format as `launcher: ready`.

## Phases and streams

**Phase 0 (serial, one agent): `ui` additions** listed under "Missing toolkit pieces": xdg-toplevel surface, clipboard + primary selection, toplevel key repeat, cursor shape, monospace cell metrics. Pure parts (cell metrics, repeat scheduler, selection offer mime handling) get unit tests; the rest is covered by phase 1 QA. Also adds `crates/term` and `crates/files` to nothing: members are `crates/*`, so new crates need no workspace edit. Merged before phase 1 starts.

**Phase 1 (parallel, disjoint files):**
- **Stream TERM** owns `crates/term/**` only. Modules: `pty`, `backend` (alacritty wrapper), `keys` (encoding, pure), `colors` (ansi16 + 256 cube, pure), `render` (grid to Painter, glyph cache), `select` (selection/click logic), `app` (window, calloop wiring), `main`. Unit tests for key encoding, color tables, click-count/selection logic, bracketed-paste sanitizing, pty winsize math, cell snapping. Adds `aurora-term` deps (`alacritty_terminal`, `rustix`) to its own `Cargo.toml` only.
- **Stream FILES** owns `crates/files/**` only. Modules: `model` (listing, sort, selection, history), `ops` (planner and executor, conflict policy), `trash` (spec impl), `uri` (uri-list/copied-files codec), `places`, `view`, `app`, `main`. Unit tests for natural sort, sort modes, selection algebra, history, op planning and conflict resolution, trash path/`.trashinfo` generation and percent-encoding, uri-list codec, operation guards (refuse `/`, `$HOME`), and executor tests that run only inside a `tempfile` directory created by the test.
- **Stream QA** owns the two new scenarios in `scripts/qa-nested.sh`, fixtures under `scripts/qa/`, and docs (README "Using it (M5)", `config/aurora.example.toml` bind examples, performance.md "M5 cache budgets"). May start from the log contract above before the binaries exist (scenarios SKIP without them).

**Shared-file touchpoints (coordinate, do not both edit):**
- Workspace `Cargo.toml`: `members = ["crates/*"]` needs no edit. Only phase 0 may add a `[workspace.dependencies]` entry; term and files declare their external deps (alacritty_terminal, rustix, tempfile, notify) locally to avoid conflicts. `Cargo.lock` is regenerated at merge time; on conflict take either side and run `cargo build`.
- `scripts/qa-nested.sh`: only the QA stream edits it (new `sc_term`, `sc_files`, header comment, `ALL=` list, binary lookup in `AURORA_BIN_DIR`). TERM and FILES streams send their scenario requirements as a short list in their final report; the QA stream implements them.
- `crates/ui/**`: only phase 0. If a phase 1 stream needs a toolkit change it works around it locally and reports it; the integration phase decides.
- `README.md`: only the QA/docs stream after phase 1; the status line and crate table are updated now with this plan.
- `config/aurora.example.toml`: QA/docs stream (commented `[services]`-style examples are not needed, apps are launched by binds: `"Mod+q" = "spawn aurora-term"`, `"Mod+e" = "spawn aurora-files"`).

**Phase 2 (serial):** merge streams with `--no-ff`, resolve conflicts, full build/clippy/tests, `cargo build --release` (the user tests the release `aurora` alias), run QA under weston, then the hardware checklist.

## QA scenarios (headless weston, via `scripts/qa-nested.sh term files`)

Same isolation rules as M4 (scratch `AURORA_IPC_SOCK`, dead D-Bus address, only pids the script started). Both scenarios use the pixel and log helpers already in the script (`grim` + `magick`, `need_client`, `wait_client_count`).

- **term:** starts with `-e` a fixture shell script from `scripts/qa/` that prints known text, then `ready` log line and a toplevel is mapped (`dump:` shows an app_id `aurora-term` window). Screenshot differs from the empty frame and a glyph region is non-background. Output marker round-trips: typing is injected with `wtype`/virtual keyboard only if available on the headless host, else the fixture script prints a marker itself and the scenario checks the pty path via the `term: spawn`/`term: exit code=0` lines and `SIGWINCH` on resize (fixture prints `stty size` after a layout change). `--hold`-less exit closes the window. A flood fixture (`yes | head -c 50M`) completes within a time limit and the compositor stays responsive (`auroractl snapshot` answers). Theme scenario: change `theme.toml`, reload, `term: theme rev=<n>` appears and the background pixel changes. Clipboard: `term: clipboard set` after a scripted selection is skipped when no input injector exists.
- **files:** started in a scratch directory with known fixture contents; `files: ready entries=<n>` matches the fixture, toplevel mapped. `--select`/`--cd` style startup flags are not added; instead the app takes the start directory as argv[1] and honors a hidden test-only script via `AURORA_FILES_TEST_SCRIPT` (a file of key names fed to the input handler), which the QA scenario uses to navigate, select, create a folder, rename, copy, trash and delete, asserting the filesystem state in scratch plus the `files: op done` lines. The trash assertion checks `$XDG_DATA_HOME/Trash` under the scratch dir (the scenario sets `XDG_DATA_HOME`, never touching the real trash). A `chmod 000` directory produces an error state, not a crash. Theme push as in term.
- Both: killing the client with `kill <pid>` from the script leaves the compositor and other windows unaffected; both survive a missing `AURORA_IPC_SOCK`.

`AURORA_FILES_TEST_SCRIPT` is the one test hook allowed in the apps: it is compiled only with the `qa-hooks` cargo feature, never in a default build, and the QA stream builds with that feature explicitly. The same pattern (`AURORA_TERM_TEST_INPUT`, a file of raw bytes written to the pty as if typed) is permitted in term.

## Tests

Only pure logic gets unit tests (matches the M4 rule): key encoding, color tables, selection/click logic, bracketed-paste sanitizing, cell snapping and winsize math (term); sort, selection algebra, history, op planner, conflict policy, trash naming/info files, uri-list codec, path guards (files), plus temp-dir executor tests. No test opens a Wayland connection, forks a shell on the live session, or touches the real home or trash. GUI behavior is covered by QA scenarios.

## Hardware checklist (real machine, from a TTY)

- [ ] term: opens from its bind, shell prompt appears, fonts match the theme mono font, crisp at the monitor scales (fractional scale, multi-monitor move keeps sharpness).
- [ ] term: `vim`/`htop`/`less` (alt screen, mouse), `ls --color`, 256 and truecolor test scripts, wide chars and emoji, `btop` smoothness at 144 Hz.
- [ ] term: key encoding (arrows, F-keys, Ctrl/Alt combos, Home/End) in a shell, vim and a Glove80 layer test; key repeat feels right.
- [ ] term: selection, primary-selection middle-click paste, Ctrl+Shift+C/V against another client, bracketed paste of multi-line text.
- [ ] term: `cat` of a large file does not stall the compositor; idle terminal shows no repaint/CPU use; resize while running `ncurses` apps is correct.
- [ ] term: exit code handling, closing the window kills the shell's foreground job via SIGHUP.
- [ ] files: opens at `$HOME`, lists a large directory (100k files) smoothly, sorting and hidden toggle work.
- [ ] files: open a text file, an image and a PDF through xdg-open; "Open terminal here" works.
- [ ] files: copy/move/rename/mkdir/trash/permanent delete on real test data under a scratch directory only; trashed files appear in other file managers' trash; cross-filesystem move (e.g. to a USB stick) verified.
- [ ] files: clipboard interop (copy a file here, paste into another file manager, and the reverse); directory auto-refresh on external changes.
- [ ] theme change reaches both apps live; killing either app never disturbs other windows.

## Decisions to revisit later

- A wgpu glyph-atlas backend for term if the CPU paint budget is missed at 4K/144 Hz.
- IPC messages for "open path in files" / "new terminal in cwd of focused window" once there is a use for them.
- kitty keyboard protocol, tabs, thumbnails.
