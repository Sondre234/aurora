#!/bin/bash
# Scripted QA for the nested backend: one hermetic Aurora per scenario.
#   scripts/qa-nested.sh [scenario ...]     default: all
# Scenarios: keys emergency reload tiling workspaces layer xwayland multi robust
#            anim effects overview xscale   (M3; written against the log contract in
#            docs/m3-plan.md, a missing line fails as MISSING log contract line)
#   anim      animation start/idle lines, dump current=/anim=, settles onto the target, off = snap
#   effects   shader programs compiled at startup, rounded corner and shadow pixels (grim+magick)
#   overview  open/close logs, dump: overview, Escape closes, emergency chords work while open
#   xscale    the xwayland scale key loads without warnings, an X11 client still maps
#            ipc services theme shell launcher notifd lock   (M4; written against the log
#            contract in docs/m4-plan.md; a scenario whose binary is not built SKIPs)
#   ipc       socket listens, auroractl snapshot/events/raw, ws: log follows a switch, a stopped
#             subscriber and a garbage client do no harm, dump: ipc
#   services  [services.*] supervision: backoff restarts, reload never double-starts, disable
#             and shutdown stop the process, children get AURORA_IPC_SOCK and WAYLAND_DISPLAY
#   theme     theme.toml change + reload pushes Event::Theme; broken file keeps the old theme
#   shell     aurora-shell: ready line, top layer with exclusive zone, bar pixels, live theme
#   launcher  aurora-launcher daemon starts hidden, `toggle` shows/hides, typing changes the
#             view, Return launches a fixture desktop entry
#   notifd    aurora-notifd on a PRIVATE dbus-daemon only: shown, replace, close, expiry
#   lock      aurora-lock: surfaces, lock: locked, binds refused, emergency chords, client
#             death stays locked, unlock only with the right test credential (else SKIP part)
#            term files   (M5; written against the log contract in docs/m5-plan.md, SKIP when
#            aurora-term / aurora-files are not built in AURORA_BIN_DIR; the key-script and
#            input-file parts need binaries built with the `qa-hooks` cargo feature)
#   term      aurora-term -e fixture: ready/spawn lines, toplevel + glyph pixels, TERM env, SIGWINCH
#             on layout change, exit closes the window, typed line round trip, 50 MB flood with a
#             responsive compositor, live theme, kill leaves the rest alone, works without IPC
#   files     aurora-files on a fixture tree: ready entries, toplevel, live theme, then via
#             AURORA_FILES_TEST_SCRIPT select/mkdir/rename/copy/move/trash/delete with filesystem
#             and $XDG_DATA_HOME/Trash assertions (scratch only), chmod 000 dir, kill, no IPC
#            display   (needs the qa_display example: cargo build -p aurora-comp --examples)
#   display   wlr-output-management heads/apply/test/cancel and reload, output power over the
#             protocol, a bind and IPC, gamma refused on the nested backend, tearing control
#
# Isolation: every scenario sets AURORA_IPC_SOCK to a file in its scratch dir, or in a private
# mktemp dir when that path is too long for a socket (never touch the live
# $XDG_RUNTIME_DIR/aurora/ipc.sock) and points DBUS_SESSION_BUS_ADDRESS at a dead path, so
# no client can reach your session bus (and dunst). notifd gets its own dbus-daemon.
#   AURORA_BIN_DIR  where aurora-shell, aurora-launcher, aurora-notifd, aurora-lock and
#                   auroractl live, default: the directory of AURORA_BIN
#
# Needs a Wayland host to nest in (WAYLAND_DISPLAY), which must NOT be your real session:
# the nested window appears there. Use a headless compositor. Never runs --drm. Only ever
# signals processes it started itself, by pid, never by name.
#   SCRATCH=dir   working directory (logs, screenshots), default $TMPDIR/aurora-qa
#   AURORA_BIN    binary, default target/debug/aurora-comp
set -u

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BIN=${AURORA_BIN:-$ROOT/target/debug/aurora-comp}
SCRATCH=${SCRATCH:-${TMPDIR:-/tmp}/aurora-qa}
HOST=${WAYLAND_DISPLAY:-}

if [ -z "$HOST" ]; then
    echo "refusing to run: WAYLAND_DISPLAY is not set (need a host compositor to nest in)" >&2
    exit 2
fi
if [ "$HOST" = wayland-1 ]; then
    echo "refusing to run on wayland-1 (normally the live desktop); nest in a headless host" >&2
    exit 2
fi
[ -x "$BIN" ] || { echo "no binary at $BIN, run cargo build" >&2; exit 2; }
mkdir -p "$SCRATCH"

# Nothing started here may reach the user's session bus: an explicit dead address wins over the
# $XDG_RUNTIME_DIR/bus fallback that dbus libraries apply. The notifd scenario overrides it.
export DBUS_SESSION_BUS_ADDRESS="unix:path=$SCRATCH/no-such-bus"
BINDIR=${AURORA_BIN_DIR:-$(dirname "$BIN")}

PASS=0 FAIL=0 SKIP=0
IPC="" IPCDIR="" PBUS="" PBPID=""
APID="" SOCK="" XD="" LOG="" OFF=0 LOFF=0 SDIR="" NAME="" CFG="" LASTPID="" MATCH=""
CPIDS=()

pass() { PASS=$((PASS + 1)); echo "PASS [$NAME] $1"; }
fail() { FAIL=$((FAIL + 1)); echo "FAIL [$NAME] $1"; }
contains() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }
matches() { grep -qaE -- "$1" <<<"$2"; }
# ok NAME COMMAND...   passes when the command succeeds
ok() { local n=$1; shift; if "$@" >/dev/null 2>&1; then pass "$n"; else fail "$n"; fi; }

wl() { env -u DISPLAY WAYLAND_DISPLAY="$SOCK" AURORA_IPC_SOCK="$IPC" "$@"; }
xc() { env -u WAYLAND_DISPLAY DISPLAY="$XD" "$@"; }

client() {
    [ -n "$SOCK" ] || { fail "client without SOCK"; return 1; }
    # No function in between: $! must be the client itself (env execs it), so kill works.
    env -u DISPLAY WAYLAND_DISPLAY="$SOCK" AURORA_IPC_SOCK="$IPC" "$@" >>"$SDIR/clients.log" 2>&1 &
    LASTPID=$!
    CPIDS+=("$LASTPID")
}
xclient() {
    [ -n "$XD" ] || { fail "xclient without XD"; return 1; }
    xc "$@" >>"$SDIR/clients.log" 2>&1 &
    LASTPID=$!
    CPIDS+=("$LASTPID")
}

alive() { [ -n "$APID" ] && kill -0 "$APID" 2>/dev/null; }

kill_clients() {
    local p
    for p in "${CPIDS[@]+"${CPIDS[@]}"}"; do
        kill -CONT "$p" 2>/dev/null
        kill "$p" 2>/dev/null
    done
    CPIDS=()
}

stop_aurora() {
    kill_clients
    if alive; then
        kill "$APID" 2>/dev/null
        local i
        for i in $(seq 50); do alive || break; sleep 0.1; done
        alive && kill -KILL "$APID" 2>/dev/null
    fi
    [ -n "$APID" ] && wait "$APID" 2>/dev/null
    APID=""
}
stop_pbus() {
    [ -n "$PBPID" ] && kill "$PBPID" 2>/dev/null
    [ -n "$PBPID" ] && wait "$PBPID" 2>/dev/null
    PBPID=""
}
trap 'stop_aurora; stop_pbus; exit 130' INT TERM
trap 'stop_aurora; stop_pbus; [ -z "$IPCDIR" ] || rm -rf "$IPCDIR"' EXIT

mark() { OFF=$(stat -c %s "$LOG" 2>/dev/null || echo 0); }
newlog() { tail -c +$((OFF + 1)) "$LOG" 2>/dev/null | sed -E 's/\x1b\[[0-9;]*m//g'; }
# wait_log REGEX SECS: searches what was logged after the last mark; sets MATCH.
wait_log() {
    local i n=$(($2 * 10))
    for i in $(seq "$n"); do
        MATCH=$(newlog | grep -aE -m1 -- "$1") && return 0
        sleep 0.1
    done
    MATCH=""
    return 1
}
need() {
    if wait_log "$1" "${2:-5}"; then pass "log: $1"; else fail "MISSING log contract line: $1"; fi
}
# Asserts something was NOT logged since the mark, after giving it a moment to appear.
absent() {
    sleep "${3:-0.4}"
    if newlog | grep -qaE -- "$1"; then
        fail "$2 (unexpected: $(newlog | grep -aE -m1 -- "$1" | cut -c1-160))"
    else
        pass "$2"
    fi
}
# need_boot REGEX [SECS]: like need, but searches the whole log (startup lines precede the mark).
need_boot() {
    local keep=$OFF
    OFF=0
    need "$@"
    OFF=$keep
}
count_log() { newlog | grep -acE -- "$1"; }

begin() {
    NAME=$1
    SDIR=$SCRATCH/$NAME
    rm -rf "$SDIR"
    mkdir -p "$SDIR/cfg/aurora" "$SDIR/state" "$SDIR/run"
    cp "$ROOT/scripts/qa/base.toml" "$SDIR/cfg/aurora/config.toml"
    CFG=$SDIR/cfg/aurora/config.toml
    LOG=$SDIR/state/aurora/comp.log
    : >"$SDIR/clients.log"
    SOCK="" XD=""
    IPC=$SDIR/run/ipc.sock
    # sun_path holds 108 bytes: under a deep SCRATCH the socket goes to a short private dir.
    if [ ${#IPC} -ge 108 ]; then
        [ -n "$IPCDIR" ] || IPCDIR=$(mktemp -d "${XDG_RUNTIME_DIR:-/tmp}/aurora-qa.XXXXXX")
        IPC=$IPCDIR/$NAME.sock
    fi
    echo "== $NAME"
}

# Adds bind lines (stdin) to the top of [keybinds] in the scenario config.
add_binds() {
    cat >"$SDIR/binds.txt"
    sed -i "/^\[keybinds\]/r $SDIR/binds.txt" "$CFG"
}

launch() {
    env WAYLAND_DISPLAY="$HOST" \
        XDG_STATE_HOME="$SDIR/state" XDG_CONFIG_HOME="$SDIR/cfg" \
        XKB_DEFAULT_LAYOUT=us XKB_DEFAULT_VARIANT=altgr-intl AURORA_QA_DIR="$SDIR/run" \
        AURORA_IPC_SOCK="$IPC" \
        "$BIN" --winit --qa --timeout "${QA_TIMEOUT:-40}" --config "$CFG" -c true \
        >"$SDIR/stdout.log" 2>&1 &
    APID=$!
    OFF=0 LOFF=0
    if ! wait_log 'aurora listening' 15; then
        fail "aurora did not start"
        return 1
    fi
    SOCK=$(sed -E 's/.*socket="?([^" ]*)"?.*/\1/' <<<"$MATCH")
    [ -n "$SOCK" ] || { fail "no socket name in: $MATCH"; return 1; }
    { [ "$SOCK" != "$HOST" ] && [ "$SOCK" != wayland-1 ]; } || { fail "bad socket $SOCK"; return 1; }
    mark
    return 0
}

# Searches from the launch (comp.log is rotated per run), not the last mark: Xwayland can be
# ready before launch marks.
wait_x() {
    OFF=$LOFF
    if wait_log 'xwayland: ready display=' 15; then
        XD=$(sed -E 's/.*display=(:[0-9]+).*/\1/' <<<"$MATCH")
        if [ -n "$XD" ]; then pass "xwayland ready $XD"; else fail "no display in: $MATCH"; fi
    else
        fail "MISSING log contract line: xwayland: ready"
    fi
    mark
}

# key MODS KEY: MODS is + separated wtype modifier names (logo shift ctrl altgr alt), may be empty.
key() {
    local mods=$1 k=$2 a=() m
    local IFS=+
    for m in $mods; do a+=(-M "$m"); done
    a+=(-k "$k")
    for m in $mods; do a+=(-m "$m"); done
    IFS=$' \t\n'
    wl wtype "${a[@]}"
    sleep 0.15
}

DUMP=""
dump() {
    mark
    if ! alive; then fail "dump: aurora is not running"; DUMP=""; return 1; fi
    kill -USR2 "$APID"
    if wait_log 'dump: end' 5; then
        DUMP=$(newlog | grep -a 'dump: ' | sed -E 's/.*(dump: .*)/\1/')
    else
        fail "MISSING log contract line: dump: end"
        DUMP=""
        return 1
    fi
}
# dumpwin APP FIELD -> value (rect gives "x,y WxH")
dumpwin() {
    local line
    line=$(grep -a "^dump: win .* app_id=\"$1\"" <<<"$DUMP" | head -1)
    case "$2" in
        rect) sed -nE 's/.* rect=(-?[0-9]+,-?[0-9]+ [0-9]+x[0-9]+).*/\1/p' <<<"$line" ;;
        current) sed -nE 's/.* current=(-?[0-9]+,-?[0-9]+ [0-9]+x[0-9]+) anim=.*/\1/p' <<<"$line" ;;
        *) sed -nE "s/.* $2=([^ ]+).*/\1/p" <<<"$line" ;;
    esac
}
dumpline() { grep -a "^dump: $1" <<<"$DUMP" | head -1; }
# "x,y WxH" -> "x y w h"
rect4() { sed -E 's/[,x ]/ /g' <<<"$1"; }

# shot NAME [grim args]: captures through Aurora's own screencopy.
shot() {
    local out=$SDIR/$1.png
    rm -f "$out"
    wl timeout 8 grim "${@:2}" "$out" 2>>"$SDIR/clients.log"
    # shot runs in $(...): a fail here would be lost with its subshell, so leave a marker for
    # end_scenario to count.
    [ -s "$out" ] || { echo "screenshot $1" >>"$SDIR/shotfail"; return 1; }
    echo "$out"
}
# pixel FILE X Y -> RRGGBB
pixel() { magick "$1" -format "%[hex:p{$2,$3}]" info: 2>/dev/null | cut -c1-6; }

# snap VAR NAME [grim args]: like `VAR=$(shot ...)`, but a failed capture is counted here (shot
# runs in a subshell, so its failures would be lost) and the scenario carries on with VAR empty.
snap() {
    local out
    if out=$(shot "${@:2}"); then
        printf -v "$1" %s "$out"
    else
        FAIL=$((FAIL + 1))
        printf -v "$1" ''
        [ -z "$out" ] || echo "$out"
    fi
}

end_scenario() {
    [ -s "$SDIR/shotfail" ] && fail "$(head -1 "$SDIR/shotfail") failed (no frame from the host?)"
    if grep -qaiE 'panicked' "$LOG" "$SDIR/stdout.log" 2>/dev/null; then
        fail "no panic in logs"
    else
        pass "no panic in logs"
    fi
    stop_aurora
}

term() { client foot -a "$1" sleep 1000; }

# ------------------------------------------------------------------------------------------

sc_keys() {
    begin keys
    launch || { end_scenario; return; }
    term typer
    need ':typer:' 10
    mark

    key logo+altgr q
    absent 'action: spawn true' "Super+AltGr+q does not match Super+q"
    mark
    key logo q
    need 'action: spawn true' 3

    mark
    key logo+shift 1
    need 'action: move-to-workspace 1' 3
    absent 'action: workspace 1$' "Super+Shift+1 is not workspace 1"
    mark
    key logo 1
    need 'action: workspace 1' 3

    # Caps Lock is a lock, not a modifier for binds.
    key "" Caps_Lock
    mark
    key logo q
    need 'action: spawn true' 3
    key "" Caps_Lock

    # Typing reaches the focused client, AltGr combinations included.
    rm -f "$SDIR/run/typed"
    client foot -a reader sh -c "IFS= read -r l; printf %s \"\$l\" > \"$SDIR/run/typed\"; sleep 1000"
    need ':reader:' 10
    sleep 0.5
    wl wtype 'äa'
    key "" Return
    sleep 0.5
    ok "typed 'äa' arrives intact" test "$(cat "$SDIR/run/typed" 2>/dev/null)" = "äa"

    # Bound keys are swallowed including their release.
    wl wtype -M logo -P q -s 500 -p q -m logo &
    local wp=$!
    sleep 0.25
    dump
    if matches 'suppressed=\[[0-9]' "$(dumpline mods)"; then pass "bound key is suppressed while held"; else fail "bound key is suppressed while held ($(dumpline mods))"; fi
    wait "$wp"
    sleep 0.2
    dump
    if matches 'suppressed=\[\]' "$(dumpline mods)"; then pass "suppressed clears on release"; else fail "suppressed clears on release ($(dumpline mods))"; fi
    if matches 'pressed=\[\]' "$(dumpline mods)"; then pass "no stuck keys"; else fail "no stuck keys ($(dumpline mods))"; fi

    # Repeat binds keep firing while held.
    mark
    wl wtype -M logo -M ctrl -P Right -s 700 -p Right -m ctrl -m logo
    local n
    n=$(count_log 'action: resize-split right')
    if [ "$n" -ge 3 ]; then pass "repeat bind fired $n times"; else fail "repeat bind fired only $n times"; fi
    mark
    absent 'action: resize-split right' "repeat stops after release" 0.6
    end_scenario
}

sc_emergency() {
    begin emergency
    add_binds <<EOF
"Ctrl+Alt+BackSpace" = "spawn touch $SDIR/run/pwned"
"Ctrl+AltGr+BackSpace" = "spawn touch $SDIR/run/pwned"
"Ctrl+Alt+F1" = "spawn touch $SDIR/run/pwned"
"Ctrl+AltGr+F1" = "spawn touch $SDIR/run/pwned"
EOF
    launch || { end_scenario; return; }
    OFF=0
    local warns
    warns=$(count_log 'config: warning .*reserved emergency chord')
    if [ "$warns" -ge 4 ]; then pass "4 emergency chord binds rejected ($warns warnings)"; else fail "emergency chord warnings: $warns (want 4)"; fi
    mark
    key ctrl+altgr F1
    need 'VT switch requested' 3
    sleep 0.3
    if alive; then pass "survives Ctrl+AltGr+F1"; else fail "aurora died on Ctrl+AltGr+F1"; fi
    key ctrl+alt F1
    sleep 0.3
    if alive; then pass "survives Ctrl+Alt+F1"; else fail "aurora died on Ctrl+Alt+F1"; fi
    mark
    key ctrl+altgr BackSpace
    need 'quitting: quit chord' 3
    local i
    for i in $(seq 50); do alive || break; sleep 0.1; done
    if alive; then fail "aurora still running 5 s after the quit chord"; else pass "quit chord exits within 5 s"; fi
    if [ ! -e "$SDIR/run/pwned" ]; then pass "config bound commands never ran"; else fail "pwned file exists"; fi
    end_scenario
}

sc_reload() {
    begin reload
    launch || { end_scenario; return; }
    local binds
    cp "$CFG" "$SDIR/good.toml"

    printf 'general = = garbage\n[[[' >"$CFG"
    mark
    kill -USR1 "$APID"
    need 'config: error .*keeping previous' 3
    if alive; then pass "survives garbage config"; else fail "died on garbage config"; fi
    dump
    binds=$(dumpline mods | sed -nE 's/.*binds=([0-9]+).*/\1/p')
    if [ "${binds:-0}" -gt 10 ]; then pass "previous binds kept ($binds)"; else fail "binds after garbage: ${binds:-none}"; fi

    cp "$SDIR/good.toml" "$CFG"
    sed -i '/^\[keybinds\]/a "Mod+F12" = "frobnicate now"' "$CFG"
    mark
    kill -USR1 "$APID"
    need 'config: warning .*frobnicate' 3
    need 'config: loaded .*warnings=[1-9]' 3

    rm -f "$CFG"
    mark
    kill -USR1 "$APID"
    need 'config: loaded' 3
    absent 'config: error' "missing file is not an error"
    dump
    binds=$(dumpline mods | sed -nE 's/.*binds=([0-9]+).*/\1/p')
    if [ "${binds:-0}" -gt 10 ]; then pass "defaults active without a file ($binds binds)"; else fail "binds without file: ${binds:-none}"; fi

    cp "$SDIR/good.toml" "$CFG"
    mark
    key logo+shift r
    need 'action: reload-config' 3
    need 'config: loaded .*warnings=0' 3

    mark
    kill -USR1 "$APID"
    kill -USR1 "$APID"
    sleep 1
    local n
    n=$(count_log 'config: loaded')
    if [ "$n" -ge 1 ]; then pass "double SIGUSR1 handled ($n reloads)"; else fail "double SIGUSR1 produced no reload"; fi
    if alive; then pass "alive after double SIGUSR1"; else fail "died after double SIGUSR1"; fi
    end_scenario
}

sc_tiling() {
    begin tiling
    launch || { end_scenario; return; }
    term a1; need ':a1:' 10
    term a2; need ':a2:' 10
    term a3; need ':a3:' 10
    sleep 0.5
    dump
    local usable ux uy uw uh x y w h x1 y1 w1 h1 x2 y2 w2 h2
    usable=$(dumpline out | sed -nE 's/.* usable=(.*)$/\1/p')
    read -r ux uy uw uh <<<"$(rect4 "$usable")"
    local ids=(a1 a2 a3) r rects=() good=1 i j
    for r in "${ids[@]}"; do rects+=("$(rect4 "$(dumpwin "$r" rect)")"); done
    for i in 0 1 2; do
        read -r x y w h <<<"${rects[$i]}"
        [ -n "${x:-}" ] || { good=0; continue; }
        { [ "$x" -ge "$ux" ] && [ "$y" -ge "$uy" ] && [ $((x + w)) -le $((ux + uw)) ] && [ $((y + h)) -le $((uy + uh)) ]; } || good=0
    done
    if [ $good = 1 ]; then pass "all rects inside usable area ($usable)"; else fail "rect outside usable: ${rects[*]} vs $usable"; fi
    good=1
    for i in 0 1 2; do for j in 0 1 2; do
        [ $i -lt $j ] || continue
        read -r x1 y1 w1 h1 <<<"${rects[$i]}"
        read -r x2 y2 w2 h2 <<<"${rects[$j]}"
        if [ $((x1 < x2 + w2)) = 1 ] && [ $((x2 < x1 + w1)) = 1 ] && [ $((y1 < y2 + h2)) = 1 ] && [ $((y2 < y1 + h1)) = 1 ]; then good=0; fi
    done; done
    if [ $good = 1 ]; then pass "rects are disjoint"; else fail "rects overlap: ${rects[*]}"; fi
    local minx=99999 miny=99999
    for i in 0 1 2; do
        read -r x y w h <<<"${rects[$i]}"
        [ "$x" -lt "$minx" ] && minx=$x
        [ "$y" -lt "$miny" ] && miny=$y
    done
    if [ $((minx - ux)) -ge 10 ] && [ $((miny - uy)) -ge 10 ]; then pass "outer gap >= gaps_out"; else fail "outer gap: min x=$minx y=$miny usable=$usable"; fi

    local before after
    before=$(dumpline focus)
    key logo Left
    dump
    after=$(dumpline focus)
    if [ "$before" != "$after" ]; then pass "focus left changed focus"; else fail "focus left did nothing ($before)"; fi
    key logo Right
    dump
    if [ "$(dumpline focus)" = "$before" ]; then pass "focus right returns"; else fail "focus right: $(dumpline focus) vs $before"; fi

    # Borders: red on the focused window, blue on the others.
    local png fid foc="" px
    png=$(shot tiling -o winit) || { end_scenario; return; }
    fid=$(sed -nE 's/.*kbd=([0-9]+).*/\1/p' <<<"$before")
    for r in "${ids[@]}"; do
        grep -qa "^dump: win $fid .*app_id=\"$r\"" <<<"$DUMP" && foc=$r
    done
    for r in "${ids[@]}"; do
        read -r x y w h <<<"$(rect4 "$(dumpwin "$r" rect)")"
        px=$(pixel "$png" $((x + w / 2)) $((y - 2)))
        if [ "$r" = "$foc" ]; then
            if [ "${px^^}" = FF0000 ]; then pass "$r (focused) border is red"; else fail "$r (focused) border pixel $px"; fi
        else
            if [ "${px^^}" = 0000FF ]; then pass "$r border is blue"; else fail "$r border pixel $px"; fi
        fi
    done
    end_scenario
}

sc_workspaces() {
    begin workspaces
    launch || { end_scenario; return; }
    term w1; need ':w1:' 10
    term spin; need ':spin:' 10
    mark
    key logo+shift 2
    need 'action: move-to-workspace 2' 3
    need 'ws: visible' 3
    sleep 0.5
    dump
    local hid vis f1 f2
    hid=$(grep -a '^dump: win ' <<<"$DUMP" | grep -a 'mapped=0' | sed -nE 's/.*app_id="([^"]*)".*/\1/p' | head -1)
    vis=$(grep -a '^dump: win ' <<<"$DUMP" | grep -a 'mapped=1' | sed -nE 's/.*app_id="([^"]*)".*/\1/p' | head -1)
    if [ -z "$hid" ] || [ -z "$vis" ]; then
        fail "expected one hidden and one visible window: $(grep -a '^dump: win' <<<"$DUMP" | cut -c1-120)"
    else
        pass "one window hidden ($hid), one visible ($vis)"
        f1=$(dumpwin "$hid" frames_sent)
        f2=$(dumpwin "$vis" frames_sent)
        sleep 2
        dump
        if [ "$(dumpwin "$hid" frames_sent)" = "$f1" ]; then pass "hidden window gets no frames ($f1)"; else fail "hidden frames_sent moved $f1 -> $(dumpwin "$hid" frames_sent)"; fi
        if [ "$(dumpwin "$vis" frames_sent)" -gt "$f2" ] 2>/dev/null; then pass "visible window keeps getting frames"; else fail "visible frames_sent stuck at $f2"; fi
    fi
    mark
    key logo 1
    need 'ws: visible winit=' 3
    end_scenario
}

# start_pbus: private session bus in $SDIR/run/bus (never the user's bus); exports it.
start_pbus() {
    PBUS="unix:path=$SDIR/run/bus"
    env -u DBUS_SESSION_BUS_ADDRESS dbus-daemon --session --nofork --address="$PBUS" \
        >"$SDIR/dbus.log" 2>&1 &
    PBPID=$!
    if ! waitfor 5 test -S "$SDIR/run/bus"; then
        fail "private dbus-daemon did not start: $(head -c 200 "$SDIR/dbus.log")"
        stop_pbus
        return 1
    fi
    export DBUS_SESSION_BUS_ADDRESS="$PBUS"
}

# waybar needs a working session bus (it aborts with "Could not connect" without one).
sc_layer() {
    begin layer
    command -v dbus-daemon >/dev/null || { skip "dbus-daemon is not installed"; return; }
    start_pbus || return
    layer_body
    stop_pbus
    export DBUS_SESSION_BUS_ADDRESS="unix:path=$SCRATCH/no-such-bus"
}

layer_body() {
    launch || { end_scenario; return; }
    client waybar -c "$ROOT/scripts/qa/waybar.jsonc" -s "$ROOT/scripts/qa/waybar.css"
    need 'layer: out=winit usable=0,30' 10
    sleep 0.6
    local png px x y w h
    png=$(shot layer -o winit) || { end_scenario; return; }
    px=$(pixel "$png" 300 10)
    if [ "${px^^}" = 123456 ]; then pass "bar pixel is 123456"; else fail "bar pixel $px"; fi
    term l1; need ':l1:' 10
    sleep 0.3
    dump
    read -r x y w h <<<"$(rect4 "$(dumpwin l1 rect)")"
    if [ "${y:-0}" -ge 30 ]; then pass "window sits below the bar (y=$y)"; else fail "window y=${y:-?} overlaps the bar"; fi
    key logo f
    dump
    if [ "$(dumpwin l1 fs)" = 1 ]; then pass "window goes fullscreen with the bar present"; else fail "fs=$(dumpwin l1 fs)"; fi
    end_scenario
}

sc_xwayland() {
    begin xwayland
    launch || { end_scenario; return; }
    wait_x
    [ -n "$XD" ] || { end_scenario; return; }
    xclient kitty --class xk -o linux_display_server=x11 sleep 1000
    need ':xk:' 15
    sleep 0.5
    dump
    local line xid r rx ry rw rh X="" Y="" WIDTH="" HEIGHT=""
    line=$(grep -a '^dump: win .*app_id="xk"' <<<"$DUMP" | head -1)
    if contains "$line" 'kind=x11'; then pass "X11 window has kind=x11"; else fail "no kind=x11 line: $line"; fi
    xid=$(sed -nE 's/.* xid=([0-9]+).*/\1/p' <<<"$line")
    r=$(rect4 "$(dumpwin xk rect)")
    if [ -n "$xid" ] && command -v xdotool >/dev/null; then
        eval "$(xc xdotool getwindowgeometry --shell "$xid" 2>/dev/null)"
        read -r rx ry rw rh <<<"$r"
        if [ "$X" = "$rx" ] && [ "$Y" = "$ry" ] && [ "$WIDTH" = "$rw" ] && [ "$HEIGHT" = "$rh" ]; then
            pass "X11 geometry agrees with the layout ($r)"
        else
            fail "X11 says ${X:-?},${Y:-?} ${WIDTH:-?}x${HEIGHT:-?}, layout says $r"
        fi
    else
        fail "cannot compare geometry (xdotool or xid missing)"
    fi
    key logo f
    dump
    if [ "$(dumpwin xk fs)" = 1 ]; then pass "fullscreen state set"; else fail "fs=$(dumpwin xk fs)"; fi
    if [ -n "$xid" ] && command -v xprop >/dev/null; then
        if contains "$(xc xprop -id "$xid" _NET_WM_STATE 2>&1)" FULLSCREEN; then pass "_NET_WM_STATE_FULLSCREEN set"; else fail "no _NET_WM_STATE_FULLSCREEN"; fi
    fi
    key logo f
    key logo F4
    sleep 0.5
    if [ "$(cat "$SDIR/run/display" 2>/dev/null)" = "$XD" ]; then pass "DISPLAY exported to spawned commands"; else fail "spawned DISPLAY=$(cat "$SDIR/run/display" 2>/dev/null) want $XD"; fi
    if [ "$(cat "$SDIR/run/wayland" 2>/dev/null)" = "$SOCK" ]; then pass "WAYLAND_DISPLAY exported to spawned commands"; else fail "spawned WAYLAND_DISPLAY wrong"; fi
    if grep -qaiE 'panicked' "$LOG" "$SDIR/stdout.log"; then fail "no panic in logs"; else pass "no panic in logs"; fi
    kill_clients
    kill "$APID" 2>/dev/null
    local i gone=0
    for i in $(seq 50); do alive || break; sleep 0.1; done
    for i in $(seq 50); do
        pgrep -f "^Xwayland $XD( |\$)" >/dev/null || { gone=1; break; }
        sleep 0.1
    done
    if [ $gone = 1 ]; then pass "no Xwayland $XD left after exit"; else fail "Xwayland $XD still running"; fi
    if grep -qa 'xwayland: exited' "$LOG"; then pass "log: xwayland: exited"; else fail "MISSING log contract line: xwayland: exited"; fi
    APID=""
}

sc_multi() {
    begin multi
    launch || { end_scenario; return; }
    term m1; need ':m1:' 10
    mark
    key logo F1
    need 'output: added name=HEADLESS-2' 5
    term m2; need ':m2:' 10
    key logo+shift period
    sleep 0.4
    dump
    local n
    n=$(grep -ac '^dump: out ' <<<"$DUMP")
    if [ "$n" = 2 ]; then pass "two outputs in the dump"; else fail "outputs in dump: $n"; fi
    mark
    key logo F2
    need 'output: removed name=HEADLESS-2 rescued=[1-9]' 5
    sleep 0.4
    dump
    local geo gx gy gw gh good=1 app x y w h
    geo=$(dumpline out | sed -nE 's/.* geo=([-0-9]+,[-0-9]+ [0-9]+x[0-9]+).*/\1/p')
    read -r gx gy gw gh <<<"$(rect4 "$geo")"
    for app in m1 m2; do
        read -r x y w h <<<"$(rect4 "$(dumpwin $app rect)")"
        { [ -n "${x:-}" ] && [ "$x" -ge "$gx" ] && [ "$y" -ge "$gy" ] && [ $((x + w)) -le $((gx + gw)) ] && [ $((y + h)) -le $((gy + gh)) ]; } || good=0
    done
    if [ $good = 1 ]; then pass "rescued windows are inside the remaining output ($geo)"; else fail "window outside $geo after removal"; fi
    mark
    key logo F1
    need 'output: added name=HEADLESS-2 .*returned=[1-9]' 5
    end_scenario
}

sc_robust() {
    QA_TIMEOUT=60
    begin robust
    launch || { end_scenario; return; }
    term r0; need ':r0:' 10
    local i p
    for i in $(seq 25); do
        wl wtype -M logo -k Left -k Right -k Up -k Down -m logo -M logo -M shift -k Left -k Right -m shift -m logo \
            -M logo -k w -k w -k f -k f -k j -k j -m logo
    done
    if alive; then pass "survives bind spam"; else fail "died during bind spam"; fi
    local pids=()
    for i in $(seq 30); do
        wl foot -a "churn$i" sleep 1000 >/dev/null 2>&1 &
        pids+=($!)
        if [ $((i % 10)) = 0 ]; then
            sleep 1.5
            for p in "${pids[@]}"; do kill "$p" 2>/dev/null; done
            pids=()
            sleep 0.5
        fi
    done
    sleep 1
    if alive; then pass "survives 30 client spawn/kill"; else fail "died during client churn"; fi
    client foot -a frozen sleep 1000
    local frozen=$LASTPID
    need ':frozen:' 10
    kill -STOP "$frozen"
    key logo c
    key logo Left
    key logo+shift Right
    sleep 0.5
    if dump; then pass "dump answers with a stopped client"; fi
    kill -CONT "$frozen"
    kill "$frozen" 2>/dev/null
    key logo+shift 3
    key logo 1
    sleep 0.5
    if dump; then pass "dump answers at the end"; fi
    if alive; then pass "aurora alive at the end"; else fail "aurora died"; fi
    end_scenario
    QA_TIMEOUT=40
}

# Appends a config section (stdin) to the scenario config.
append_cfg() { cat >>"$CFG"; }

# boot_no_warning WHAT: no `config: warning` in the whole log since launch.
boot_no_warning() {
    if grep -qaE 'config: warning' "$LOG"; then
        fail "$1 (config warning: $(grep -aE -m1 'config: warning' "$LOG" | cut -c1-160))"
    else
        pass "$1"
    fi
}

# settled APP: in the last dump the drawn rect equals the layout rect and anim=0.
settled() {
    local r c a
    r=$(dumpwin "$1" rect)
    c=$(dumpwin "$1" current)
    a=$(dumpwin "$1" anim)
    if [ -z "$c" ] || [ -z "$a" ]; then
        fail "MISSING log contract line: dump: win $1 current=<x>,<y> <w>x<h> anim=0|1"
    elif [ "$c" = "$r" ] && [ "$a" = 0 ]; then
        pass "$1 settled on its target ($r)"
    else
        fail "$1 not settled: rect=$r current=$c anim=$a"
    fi
}

sc_anim() {
    begin anim
    append_cfg <<EOF

[animations]
enabled = true
duration_ms = 1500
curve = "ease-out"
EOF
    launch || { end_scenario; return; }
    term an1; need ':an1:' 10
    term an2; need ':an2:' 10
    # Let the open animations finish; then the desktop must be idle.
    need 'anim: idle' 8
    sleep 0.3
    dump
    settled an1
    settled an2
    mark
    absent 'anim: start' "idle desktop starts no animation" 1.2

    # A layout change moves windows: start line, mid-flight dump, then idle and settled.
    mark
    key logo j
    need 'anim: start kind=[a-z_]+ win=[0-9]+' 3
    dump
    if grep -qaE '^dump: win .* anim=1' <<<"$DUMP"; then
        pass "a window reports anim=1 mid-flight"
    else
        fail "no window with anim=1 right after the start line: $(grep -a '^dump: win' <<<"$DUMP" | cut -c1-200 | head -2)"
    fi
    mark
    need 'anim: idle' 6
    sleep 0.3
    dump
    settled an1
    settled an2

    # enabled = false snaps exactly like M2: no animation lines, current == rect at once.
    sed -i 's/^enabled = true/enabled = false/' "$CFG"
    mark
    kill -USR1 "$APID"
    need 'config: loaded .*warnings=0' 3
    mark
    key logo j
    sleep 0.3
    dump
    settled an1
    settled an2
    absent 'anim: start' "no animation starts when disabled"
    end_scenario
}

sc_effects() {
    begin effects
    append_cfg <<EOF

[animations]
enabled = false

[decoration]
rounding = 24
shadow = true
shadow_radius = 30
shadow_color = "#000000ff"
blur = false
EOF
    launch || { end_scenario; return; }
    need_boot 'effects: programs compiled=[1-9]' 5
    term fx; need ':fx:' 10
    sleep 0.6
    dump
    local x y w h png1 png2 gx gy
    read -r x y w h <<<"$(rect4 "$(dumpwin fx rect)")"
    if [ -z "${x:-}" ]; then fail "no rect for fx"; end_scenario; return; fi
    settled fx
    # A gap pixel beside the window: outside the 4 px border, inside the 10 px outer gap.
    gx=$((x - 8))
    gy=$((y + h / 2))
    png1=$(shot fx-on -o winit) || { end_scenario; return; }
    local c_on e_on s_on c_off e_off s_off
    c_on=$(pixel "$png1" $((x + 1)) $((y + 1)))
    e_on=$(pixel "$png1" $((x + w / 2)) $((y - 2)))
    s_on=$(pixel "$png1" "$gx" "$gy")

    # Square and shadowless for comparison, through a live reload.
    sed -i 's/^rounding = .*/rounding = 0/; s/^shadow = .*/shadow = false/' "$CFG"
    mark
    kill -USR1 "$APID"
    need 'config: loaded .*warnings=0' 3
    sleep 0.6
    png2=$(shot fx-off -o winit) || { end_scenario; return; }
    c_off=$(pixel "$png2" $((x + 1)) $((y + 1)))
    e_off=$(pixel "$png2" $((x + w / 2)) $((y - 2)))
    s_off=$(pixel "$png2" "$gx" "$gy")

    if [ "$c_on" != "$c_off" ]; then pass "rounding 24 changes the corner pixel ($c_off -> $c_on)"; else fail "corner pixel unchanged by rounding: $c_on"; fi
    if [ "$e_on" = "$e_off" ]; then pass "top edge middle is untouched by rounding ($e_on)"; else fail "top edge pixel moved: $e_off -> $e_on"; fi
    if [ "$s_on" != "$s_off" ]; then pass "shadow darkens the gap beside the window ($s_off -> $s_on)"; else fail "no shadow at $gx,$gy: $s_on"; fi
    end_scenario
}

sc_overview() {
    begin overview
    add_binds <<EOF
"Mod+F3" = "overview"
EOF
    launch || { end_scenario; return; }
    boot_no_warning "the overview action and bind are accepted"
    term ov1; need ':ov1:' 10
    term ov2; need ':ov2:' 10
    sleep 0.3
    dump
    if [ -z "$(dumpline overview)" ] || matches 'state=closed|open=0' "$(dumpline overview)"; then pass "no open overview in the dump before opening"; else fail "overview listed while closed: $(dumpline overview)"; fi

    mark
    key logo F3
    need 'overview: open' 3
    sleep 0.3
    dump
    if matches 'state=open' "$(dumpline overview)"; then
        pass "dump: overview while open"
    else
        fail "MISSING log contract line: dump: overview (while open): '$(dumpline overview)'"
    fi

    mark
    key "" Escape
    need 'overview: close' 3
    sleep 0.5
    dump
    if [ -z "$(dumpline overview)" ] || matches 'state=closed|open=0' "$(dumpline overview)"; then pass "Escape closed the overview"; else fail "overview still open: $(dumpline overview)"; fi

    # The bind toggles it.
    mark
    key logo F3
    need 'overview: open' 3
    mark
    key logo F3
    need 'overview: close' 3

    # Emergency chords are not intercepted while it is open.
    mark
    key logo F3
    need 'overview: open' 3
    mark
    key ctrl+altgr F1
    need 'VT switch requested' 3
    sleep 0.3
    if alive; then pass "survives Ctrl+AltGr+F1 with the overview open"; else fail "died on the VT chord in the overview"; fi
    mark
    key ctrl+altgr BackSpace
    need 'quitting: quit chord' 3
    local i
    for i in $(seq 50); do alive || break; sleep 0.1; done
    if alive; then fail "quit chord ignored while the overview is open"; else pass "quit chord works with the overview open"; fi
    end_scenario
}

sc_xscale() {
    begin xscale
    append_cfg <<EOF

[xwayland]
scale = 1.25
EOF
    launch || { end_scenario; return; }
    need_boot 'config: loaded .*warnings=0' 5
    boot_no_warning "the xwayland scale key is accepted"
    wait_x
    [ -n "$XD" ] || { end_scenario; return; }
    xclient kitty --class xs -o linux_display_server=x11 sleep 1000
    need ':xs:' 15
    sleep 0.5
    dump
    if contains "$(grep -a '^dump: win .*app_id="xs"' <<<"$DUMP" | head -1)" 'kind=x11'; then pass "X11 window maps with the scale key set"; else fail "no kind=x11 window xs"; fi
    mark
    kill -USR1 "$APID"
    need 'config: loaded .*warnings=0' 3
    end_scenario
}

# ------------------------------------------------------------------------------------------
# M4 scenarios (docs/m4-plan.md log contract)

skip() { SKIP=$((SKIP + 1)); echo "SKIP [$NAME] $1"; }
# require NAME BIN...: names the scenario; SKIPs (returns 1) when a binary is not built.
require() {
    NAME=$1
    shift
    local b
    for b in "$@"; do
        if [ ! -x "$BINDIR/$b" ]; then
            skip "$b is not built at $BINDIR/$b (cargo build, or set AURORA_BIN_DIR)"
            return 1
        fi
    done
    return 0
}
ctl() { env -u DISPLAY AURORA_IPC_SOCK="$IPC" "$BINDIR/auroractl" "$@"; }
# Never background `ctl` (a function): $! would be a subshell, so kill/STOP would miss auroractl.
# Runs a service binary as a Wayland client with the scenario's socket and IPC path.
svc() { local b=$1; shift; client "$BINDIR/$b" "$@"; }

# Output of the clients (services started with `svc`), ANSI stripped.
clog() { sed -E 's/\x1b\[[0-9;]*m//g' "$SDIR/clients.log" 2>/dev/null; }
# need_client REGEX [SECS]: a line in the client output (whole file, not marked).
need_client() {
    local i n=$((${2:-8} * 10))
    for i in $(seq "$n"); do
        if clog | grep -qaE -- "$1"; then pass "client log: $1"; return 0; fi
        sleep 0.1
    done
    fail "MISSING log contract line: $1"
    return 1
}
# wait_client_count REGEX N SECS: waits until at least N client lines match.
wait_client_count() {
    local i n=$(($3 * 10))
    for i in $(seq "$n"); do
        [ "$(clog | grep -acE -- "$1")" -ge "$2" ] && return 0
        sleep 0.1
    done
    return 1
}
# diffpx A.png B.png -> number of differing pixels
diffpx() { magick compare -metric AE "$1" "$2" null: 2>&1 | awk '{print int($1)}'; }
# waitfor SECS COMMAND...: retries until the command succeeds.
waitfor() {
    local i n=$(($1 * 10))
    shift
    for i in $(seq "$n"); do
        "$@" >/dev/null 2>&1 && return 0
        sleep 0.1
    done
    return 1
}
# started_ge NAME N: the service was started at least N times since the mark (re-evaluated per poll).
started_ge() { [ "$(count_log "service: started name=$1 ")" -ge "$2" ]; }
pid_dead() { ! kill -0 "$1" 2>/dev/null; }
# pid_from_log NAME: pid of the last `service: started name=NAME pid=N` since the last mark.
pid_from_log() { newlog | grep -a "service: started name=$1 " | tail -1 | sed -E 's/.* pid=([0-9]+).*/\1/'; }
need_ctl() {
    [ -x "$BINDIR/auroractl" ] && return 0
    skip "auroractl is not built at $BINDIR/auroractl"
    return 1
}

sc_ipc() {
    NAME=ipc
    need_ctl || return
    begin ipc
    launch || { end_scenario; return; }
    need_boot 'ipc: listening path=' 5
    local boot
    boot=$(OFF=0 newlog | grep -a -m1 'ipc: listening path=')
    if contains "$boot" "path=$IPC"; then pass "listens on AURORA_IPC_SOCK"; else fail "ipc: listening path is not $IPC ($boot)"; fi
    ok "socket file exists" test -S "$IPC"

    client foot -a ipcw -T qa-title sh -c "sleep 2; printf '\\033]2;retitled\\007'; sleep 1000"
    need ':ipcw:' 10
    mark
    env -u DISPLAY AURORA_IPC_SOCK="$IPC" "$BINDIR/auroractl" events >"$SDIR/events.log" 2>&1 &
    local evpid=$!
    CPIDS+=("$evpid")
    need 'ipc: client connected name=auroractl proto=[0-9]+' 5
    sleep 0.3

    local snap
    snap=$(ctl snapshot | tr -d ' \n')
    if contains "$snap" '"name":"winit"'; then pass "snapshot lists the output"; else fail "snapshot has no output winit: ${snap:0:200}"; fi
    if contains "$snap" '"app_id":"ipcw"'; then pass "snapshot lists the window"; else fail "snapshot has no window ipcw"; fi
    if contains "$snap" '"title":"qa-title"'; then pass "snapshot carries the window title"; else fail "snapshot title missing"; fi
    if matches '"workspaces":\[\{' "$snap"; then pass "snapshot lists workspaces"; else fail "snapshot has no workspaces"; fi

    # A title change reaches subscribers.
    if waitfor 8 grep -qa retitled "$SDIR/events.log"; then pass "title change arrives as an event"; else fail "no retitled event: $(head -c 300 "$SDIR/events.log")"; fi

    # Switching workspace over the socket logs like the bind does, and is broadcast.
    mark
    local reply
    reply=$(ctl raw '{"SwitchWorkspace":{"output":null,"index":2}}' | tr -d ' \n')
    if [ "$reply" = '"Ok"' ]; then pass "raw SwitchWorkspace answers Ok"; else fail "SwitchWorkspace reply: $reply"; fi
    need 'ws: visible winit=2' 3
    need 'ipc: broadcast topic=Workspaces clients=[1-9]' 3
    if waitfor 3 grep -qa 'WorkspaceChanged' "$SDIR/events.log"; then pass "WorkspaceChanged event delivered"; else fail "no WorkspaceChanged in events"; fi
    ctl raw '{"SwitchWorkspace":{"output":null,"index":1}}' >/dev/null
    reply=$(ctl raw '"ListWindows"' | tr -d ' \n')
    if contains "$reply" '"ipcw"'; then pass "raw ListWindows lists the window"; else fail "ListWindows: ${reply:0:160}"; fi

    dump
    if matches '^dump: ipc clients=[1-9]' "$(dumpline ipc)"; then pass "dump: ipc clients"; else fail "MISSING log contract line: dump: ipc clients=<n> ('$(dumpline ipc)')"; fi

    # A stopped subscriber must not stall the compositor or other clients.
    env -u DISPLAY AURORA_IPC_SOCK="$IPC" "$BINDIR/auroractl" events >/dev/null 2>&1 &
    local slow=$!
    CPIDS+=("$slow")
    sleep 0.3
    kill -STOP "$slow"
    local i
    for i in $(seq 120); do
        ctl raw "{\"SwitchWorkspace\":{\"output\":null,\"index\":$((i % 2 + 1))}}" >/dev/null 2>&1 || break
    done
    if timeout 5 env AURORA_IPC_SOCK="$IPC" "$BINDIR/auroractl" snapshot >/dev/null 2>&1; then pass "snapshot still answers with a stopped subscriber"; else fail "snapshot stalled with a stopped subscriber"; fi
    if alive; then pass "compositor alive after 120 switches"; else fail "compositor died with a stopped subscriber"; fi
    kill -CONT "$slow" 2>/dev/null
    kill "$slow" 2>/dev/null

    # Garbage and oversized frames: the client is dropped, everyone else carries on.
    if command -v python3 >/dev/null; then
        python3 - "$IPC" <<'PY'
import socket, sys
for payload in (b"garbage\xff\xff\xff\xff" * 64, b"\xff\xff\xff\xff", b"\x00\x00\x00\x00", b"\x05\x00\x00\x00abcde"):
    s = socket.socket(socket.AF_UNIX)
    s.connect(sys.argv[1])
    try:
        s.sendall(payload)
    except OSError:
        pass
    s.close()
PY
        sleep 0.5
        if alive; then pass "compositor alive after garbage clients"; else fail "compositor died on garbage clients"; fi
        if ctl snapshot >/dev/null 2>&1; then pass "ipc still serves after garbage clients"; else fail "ipc dead after garbage clients"; fi
    else
        skip "python3 missing, garbage client part not run"
    fi

    mark
    kill "$evpid" 2>/dev/null
    need 'ipc: client gone name=auroractl' 5
    end_scenario
}

sc_services() {
    NAME=services
    begin services
    cat >"$SDIR/run/fake.sh" <<EOS
#!/bin/sh
echo \$\$ >> "$SDIR/run/fake.starts"
sleep 0.2
exit 3
EOS
    cat >"$SDIR/run/steady.sh" <<EOS
#!/bin/sh
echo \$\$ >> "$SDIR/run/steady.starts"
printf %s "\$AURORA_IPC_SOCK" > "$SDIR/run/steady.ipc"
printf %s "\$WAYLAND_DISPLAY" > "$SDIR/run/steady.wl"
exec sleep 1000
EOS
    chmod +x "$SDIR/run/fake.sh" "$SDIR/run/steady.sh"
    append_cfg <<EOF

[services.fake]
command = "$SDIR/run/fake.sh"
restart = "on-failure"
backoff_ms = 100
max_backoff_ms = 800

[services.steady]
command = "$SDIR/run/steady.sh"
restart = "always"
EOF
    QA_TIMEOUT=60
    launch || { end_scenario; QA_TIMEOUT=40; return; }
    OFF=0
    need 'service: started name=fake pid=[0-9]+' 5
    need 'service: started name=steady pid=[0-9]+' 5
    # A service that keeps exiting is restarted with growing delays, capped by max_backoff_ms.
    if waitfor 10 started_ge fake 4; then pass "fake restarted 3 times"; else fail "fake started only $(count_log 'service: started name=fake ') times"; fi
    need 'service: exited name=fake code=3 restart=[0-9]+' 3
    local delays d1 d2 d3
    delays=$(newlog | grep -a 'service: exited name=fake' | sed -E 's/.* restart=([0-9a-z]+).*/\1/' | head -4 | tr '\n' ' ')
    read -r d1 d2 d3 _ <<<"$delays"
    if [ "${d1:-x}" -ge 100 ] 2>/dev/null && [ "${d2:-0}" -gt "${d1:-x}" ] 2>/dev/null && [ "${d3:-0}" -gt "${d2:-0}" ] 2>/dev/null; then
        pass "backoff grows: $delays"
    else
        fail "backoff delays do not grow: $delays"
    fi
    if newlog | grep -aE 'service: exited name=fake.*restart=[0-9]+' | sed -E 's/.* restart=([0-9]+).*/\1/' | awk '$1 > 800 {bad=1} END {exit bad}'; then pass "backoff never exceeds max_backoff_ms"; else fail "a restart delay above 800 ms"; fi

    # Supervised children see the compositor's IPC socket and Wayland display.
    if waitfor 3 test -s "$SDIR/run/steady.ipc"; then
        if [ "$(cat "$SDIR/run/steady.ipc")" = "$IPC" ]; then pass "service gets AURORA_IPC_SOCK"; else fail "service AURORA_IPC_SOCK=$(cat "$SDIR/run/steady.ipc")"; fi
        if [ "$(cat "$SDIR/run/steady.wl")" = "$SOCK" ]; then pass "service gets WAYLAND_DISPLAY"; else fail "service WAYLAND_DISPLAY=$(cat "$SDIR/run/steady.wl")"; fi
    else
        fail "steady service did not run"
    fi

    # Reload never double-starts a running service.
    local spid r
    spid=$(pid_from_log steady)
    for r in 1 2 3; do
        mark
        kill -USR1 "$APID"
        need 'config: loaded' 3
    done
    if [ "$(wc -l <"$SDIR/run/steady.starts")" = 1 ]; then pass "3 reloads did not restart or double-start steady"; else fail "steady started $(wc -l <"$SDIR/run/steady.starts") times"; fi
    dump
    if matches "^dump: service steady pid=[0-9]+ restarts=0" "$(dumpline 'service steady')"; then pass "dump: service steady"; else fail "MISSING log contract line: dump: service steady pid=<p> restarts=0 ('$(dumpline 'service steady')')"; fi
    if matches '^dump: service fake pid=.* restarts=[1-9]' "$(dumpline 'service fake')"; then pass "dump: service fake counts restarts"; else fail "dump: service fake: '$(dumpline 'service fake')'"; fi

    # Disabling through a reload stops it; re-enabling starts it again (once).
    sed -i '/^\[services.steady\]/a enabled = false' "$CFG"
    mark
    kill -USR1 "$APID"
    need 'service: stopping name=steady pid=[0-9]+' 3
    if [ -n "$spid" ] && waitfor 4 pid_dead "$spid"; then pass "disabled service process is gone"; else fail "steady pid $spid still alive after disable"; fi
    absent 'service: started name=steady' "a disabled service is not restarted" 1.5
    sed -i '/^enabled = false/d' "$CFG"
    mark
    kill -USR1 "$APID"
    need 'service: started name=steady pid=[0-9]+' 3
    sleep 0.5
    if [ "$(count_log 'service: started name=steady ')" = 1 ]; then pass "re-enabled service started exactly once"; else fail "steady started $(count_log 'service: started name=steady ') times"; fi
    spid=$(pid_from_log steady)

    # Shutdown stops everything it supervises.
    mark
    kill "$APID"
    local i
    for i in $(seq 60); do alive || break; sleep 0.1; done
    if alive; then fail "aurora still running 6 s after SIGTERM"; else pass "aurora exits on SIGTERM"; fi
    need 'service: stopping name=steady pid=[0-9]+' 3
    if [ -n "$spid" ] && waitfor 3 pid_dead "$spid"; then pass "service stopped at shutdown"; else fail "steady pid $spid outlived the compositor"; fi
    if [ -s "$SDIR/run/steady.starts" ]; then
        local sp
        sp=$(tail -1 "$SDIR/run/steady.starts")
        if pid_dead "$sp"; then pass "service child process is gone"; else fail "child $sp outlived the compositor"; fi
    fi
    wait "$APID" 2>/dev/null
    APID=""
    QA_TIMEOUT=40
}

sc_theme() {
    NAME=theme
    need_ctl || return
    begin theme
    local TH="$SDIR/cfg/aurora/theme.toml"
    printf '[palette]\naccent = "#010203"\n' >"$TH"
    launch || { end_scenario; return; }
    need_boot 'theme: loaded path=.* warnings=0' 5
    env -u DISPLAY AURORA_IPC_SOCK="$IPC" "$BINDIR/auroractl" events theme >"$SDIR/events.log" 2>&1 &
    local evpid=$!
    CPIDS+=("$evpid")
    need 'ipc: client connected name=auroractl' 5
    sleep 0.3
    local cur
    cur=$(ctl raw '"GetTheme"' | tr -d ' \n')
    if contains "$cur" '1,2,3,255'; then pass "initial theme.toml is live (accent 1,2,3)"; else fail "GetTheme does not show the file's accent: ${cur:0:200}"; fi

    printf '[palette]\naccent = "#abcdef"\n' >"$TH"
    mark
    kill -USR1 "$APID"
    need 'theme: changed rev=[0-9]+' 3
    if waitfor 3 grep -qa '171,205,239' "$SDIR/events.log"; then pass "Event::Theme pushed with the new accent"; else fail "no Theme event with 171,205,239: $(head -c 300 "$SDIR/events.log")"; fi
    need 'ipc: broadcast topic=Theme clients=[1-9]' 3

    # No change, no push.
    mark
    kill -USR1 "$APID"
    need 'config: loaded' 3
    absent 'theme: changed' "an unchanged theme.toml pushes nothing"

    # A syntax error keeps the current theme; a bad value only warns.
    printf '[palette\naccent = = ' >"$TH"
    mark
    kill -USR1 "$APID"
    need 'theme: error .*keeping previous' 3
    cur=$(ctl raw '"GetTheme"' | tr -d ' \n')
    if contains "$cur" '171,205,239'; then pass "broken theme.toml keeps the previous theme"; else fail "theme after a broken file: ${cur:0:200}"; fi
    printf '[palette]\naccent = "not-a-color"\n' >"$TH"
    mark
    kill -USR1 "$APID"
    need 'theme: warning' 3
    if alive; then pass "alive after a bad theme value"; else fail "died on a bad theme value"; fi
    end_scenario
}

# bar_hits PNG Y RRGGBB WIDTH: how many of 9 samples along row Y have that color.
bar_hits() {
    local i hits=0 px
    for i in 1 2 3 4 5 6 7 8 9; do
        px=$(pixel "$1" $(($4 * i / 10)) "$2")
        [ "${px^^}" = "$3" ] && hits=$((hits + 1))
    done
    echo "$hits"
}

sc_shell() {
    require shell aurora-shell || return
    begin shell
    # Opaque bar so the pixel check is exact, whatever the widgets draw.
    printf '[palette]\nbg = "#123456"\n' >"$SDIR/cfg/aurora/theme.toml"
    launch || { end_scenario; return; }
    mark
    svc aurora-shell
    local spid=$LASTPID
    need_client 'shell: ready outputs=[1-9]' 15
    need 'layer: out=winit usable=0,[1-9][0-9]* ' 10
    local bary
    bary=$(sed -E 's/.*usable=0,([0-9]+) .*/\1/' <<<"$MATCH")
    sleep 0.8
    dump
    local gx gy gw gh png hits px
    read -r gx gy gw gh <<<"$(rect4 "$(dumpline out | sed -nE 's/.* geo=([-0-9]+,[-0-9]+ [0-9]+x[0-9]+).*/\1/p')")"
    png=$(shot bar -o winit) || { end_scenario; return; }
    hits=$(bar_hits "$png" $((bary / 2)) 123456 "$gw")
    if [ "$hits" -ge 6 ]; then pass "bar strip is the theme bg ($hits/9 samples are 123456)"; else fail "bar strip pixels are not 123456 ($hits/9), e.g. $(pixel "$png" $((gw / 10)) $((bary / 2)))"; fi
    px=$(pixel "$png" $((gw / 2)) $((bary + 6)))
    if [ "${px^^}" != 123456 ]; then pass "below the bar is not bar colored"; else fail "pixel below the bar is bar colored"; fi

    # Tiled windows stay below the exclusive zone.
    term sh1; need ':sh1:' 10
    sleep 0.3
    dump
    local x y w h
    read -r x y w h <<<"$(rect4 "$(dumpwin sh1 rect)")"
    if [ "${y:-0}" -ge "$bary" ]; then pass "window sits below the bar (y=$y, zone $bary)"; else fail "window y=${y:-?} overlaps the bar zone $bary"; fi

    # The theme reaches the running shell without a restart.
    printf '[palette]\nbg = "#654321"\n' >"$SDIR/cfg/aurora/theme.toml"
    mark
    kill -USR1 "$APID"
    need 'theme: changed rev=' 3
    sleep 0.8
    png=$(shot bar2 -o winit) || { end_scenario; return; }
    hits=$(bar_hits "$png" $((bary / 2)) 654321 "$gw")
    if [ "$hits" -ge 6 ]; then pass "bar repainted with the new theme ($hits/9)"; else fail "bar did not follow the theme ($hits/9 are 654321)"; fi

    # A dying bar releases its zone and never hurts windows.
    mark
    kill "$spid" 2>/dev/null
    need 'layer: out=winit usable=0,0 ' 5
    if alive; then pass "compositor alive after the shell died"; else fail "compositor died with the shell"; fi
    dump
    if [ -n "$(dumpwin sh1 rect)" ]; then pass "window survives the shell"; else fail "window sh1 gone after the shell died"; fi
    end_scenario
}

sc_launcher() {
    require launcher aurora-launcher || return
    begin launcher
    mkdir -p "$SDIR/data/applications"
    cat >"$SDIR/data/applications/qa-hello.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=QaHello
Exec=touch $SDIR/run/launched
Terminal=false
EOF
    launch || { end_scenario; return; }
    # The daemon and every `toggle` share this socket; keep it out of the user's real
    # $XDG_RUNTIME_DIR/aurora/launcher.sock.
    export AURORA_LAUNCHER_SOCK="$SDIR/run/launcher.sock"
    client env XDG_DATA_DIRS="$SDIR/data" XDG_DATA_HOME="$SDIR/data" "$BINDIR/aurora-launcher"
    need_client 'launcher: ready apps=[1-9]' 15
    sleep 0.5
    if clog | grep -qa 'launcher: show'; then fail "launcher showed itself at startup"; else pass "daemon starts hidden"; fi
    local before shown typed after
    snap before l-hidden -o winit

    # `toggle` is a second invocation that talks to the running daemon (over IPC or a socket).
    wl env XDG_DATA_DIRS="$SDIR/data" "$BINDIR/aurora-launcher" toggle >>"$SDIR/clients.log" 2>&1
    if wait_client_count 'launcher: show' 1 5; then pass "toggle shows"; else fail "MISSING log contract line: launcher: show"; fi
    sleep 0.5
    snap shown l-shown -o winit
    if [ -z "$before" ] || [ -z "$shown" ]; then :; elif [ "$(diffpx "$before" "$shown")" -gt 500 ]; then pass "screen differs while the launcher is shown"; else fail "no visible launcher surface (diff $(diffpx "$before" "$shown") px)"; fi

    wl wtype 'qahel'
    sleep 0.6
    snap typed l-typed -o winit
    if [ -z "$shown" ] || [ -z "$typed" ]; then :; elif [ "$(diffpx "$shown" "$typed")" -gt 50 ]; then pass "typing changes the view"; else fail "typing 'qahel' changed nothing on screen"; fi

    # Escape hides it again.
    key "" Escape
    if wait_client_count 'launcher: hide' 1 5; then pass "Escape hides"; else fail "MISSING log contract line: launcher: hide (Escape)"; fi
    wl "$BINDIR/aurora-launcher" toggle >>"$SDIR/clients.log" 2>&1
    if wait_client_count 'launcher: show' 2 5; then pass "second toggle shows again"; else fail "second launcher: show missing"; fi
    wl "$BINDIR/aurora-launcher" toggle >>"$SDIR/clients.log" 2>&1
    if wait_client_count 'launcher: hide' 2 5; then pass "toggle hides"; else fail "second launcher: hide missing"; fi
    sleep 0.4
    snap after l-after -o winit
    if [ -z "$before" ] || [ -z "$after" ]; then :; elif [ "$(diffpx "$before" "$after")" -lt 50 ]; then pass "hidden again leaves no residue"; else fail "screen differs after hide ($(diffpx "$before" "$after") px)"; fi

    # A name and Return launch the fixture entry through IPC Spawn.
    wl "$BINDIR/aurora-launcher" toggle >>"$SDIR/clients.log" 2>&1
    wait_client_count 'launcher: show' 3 5 || fail "launcher: show (third) missing"
    sleep 0.4
    wl wtype 'qahel'
    sleep 0.4
    key "" Return
    if waitfor 5 test -e "$SDIR/run/launched"; then pass "Return launched the selected entry"; else fail "fixture entry was not launched"; fi
    if wait_client_count 'launcher: hide' 3 5; then pass "launcher hides after launching"; else fail "launcher did not hide after launch"; fi
    unset AURORA_LAUNCHER_SOCK
    end_scenario
}

NOTIF_DEST=(--dest org.freedesktop.Notifications --object-path /org/freedesktop/Notifications)
notif_call() { gdbus call --session "${NOTIF_DEST[@]}" --method "org.freedesktop.Notifications.$1" "${@:2}" 2>&1; }

sc_notifd() {
    require notifd aurora-notifd || return
    begin notifd
    local tool
    for tool in dbus-daemon notify-send gdbus; do
        command -v "$tool" >/dev/null || { skip "$tool is not installed"; return; }
    done
    # The private bus. Its address is explicit and lives in the scratch dir; the user's
    # session bus is never named anywhere in this scenario.
    PBUS="unix:path=$SDIR/run/bus"
    env -u DBUS_SESSION_BUS_ADDRESS dbus-daemon --session --nofork --address="$PBUS" \
        >"$SDIR/dbus.log" 2>&1 &
    PBPID=$!
    if ! waitfor 5 test -S "$SDIR/run/bus"; then
        fail "private dbus-daemon did not start: $(head -c 200 "$SDIR/dbus.log")"
        stop_pbus
        return
    fi
    export DBUS_SESSION_BUS_ADDRESS="$PBUS"
    notifd_body
    end_scenario
    stop_pbus
    export DBUS_SESSION_BUS_ADDRESS="unix:path=$SCRATCH/no-such-bus"
}

notifd_body() {
    case "$DBUS_SESSION_BUS_ADDRESS" in "unix:path=$SDIR/"*) ;; *) fail "refusing: bus address is not private"; return ;; esac
    launch || return
    svc aurora-notifd
    local npid=$LASTPID
    need_client 'notifd: ready name=' 15
    local out
    out=$(notif_call GetServerInformation)
    if contains "$out" 'aurora'; then pass "GetServerInformation answers on the private bus ($out)"; else fail "GetServerInformation: $out"; fi
    out=$(notif_call GetCapabilities)
    if contains "$out" 'body'; then pass "capabilities include body"; else fail "GetCapabilities: $out"; fi

    sleep 0.5
    local base shown gone id1 id2 n
    base=$(shot n-base -o winit) || return
    notify-send -a qa -t 0 "QA title" "QA body text" >>"$SDIR/clients.log" 2>&1
    need_client 'notifd: shown id=[0-9]+' 8
    sleep 0.6
    shown=$(shot n-shown -o winit) || return
    if [ "$(diffpx "$base" "$shown")" -gt 500 ]; then pass "a toast is on screen"; else fail "no toast pixels ($(diffpx "$base" "$shown") px)"; fi

    # Replace keeps the id (replaces_id).
    out=$(notif_call Notify qa 0 "" "First" "one" '[]' '{}' 0)
    id1=$(sed -nE 's/^\(uint32 ([0-9]+),\)$/\1/p' <<<"$out")
    out=$(notif_call Notify qa "${id1:-0}" "" "First" "replaced" '[]' '{}' 0)
    id2=$(sed -nE 's/^\(uint32 ([0-9]+),\)$/\1/p' <<<"$out")
    if [ -n "$id1" ] && [ "$id1" = "$id2" ]; then pass "replaces_id keeps the id ($id1)"; else fail "replace changed the id: '$id1' -> '$id2' ($out)"; fi
    need_client "notifd: shown id=${id1:-0}" 5

    # CloseNotification removes toasts (ids are small integers, close the first dozen).
    for n in $(seq 1 12); do notif_call CloseNotification "$n" >/dev/null; done
    sleep 0.8
    gone=$(shot n-closed -o winit) || return
    if [ "$(diffpx "$base" "$gone")" -lt 50 ]; then pass "all toasts gone after CloseNotification"; else fail "toast residue after close ($(diffpx "$base" "$gone") px)"; fi

    # Expiry removes a timed toast on its own.
    notify-send -a qa -t 1200 "brief" "expires" >>"$SDIR/clients.log" 2>&1
    sleep 0.6
    gone=$(shot n-exp1 -o winit) || return
    if [ "$(diffpx "$base" "$gone")" -gt 500 ]; then pass "timed toast visible"; else fail "timed toast not visible"; fi
    sleep 2.5
    gone=$(shot n-exp2 -o winit) || return
    if [ "$(diffpx "$base" "$gone")" -lt 50 ]; then pass "timed toast expired"; else fail "timed toast still visible after expiry"; fi

    # Killing notifd never disturbs the compositor.
    kill "$npid" 2>/dev/null
    sleep 0.5
    if alive; then pass "compositor alive after notifd died"; else fail "compositor died with notifd"; fi
}

sc_lock() {
    require lock aurora-lock || return
    begin lock
    local CRED="" cmd="$BINDIR/aurora-lock"
    # Test-only authenticator: the lock crate's feature docs must define a test credential
    # env. Assumed name AURORA_LOCK_TEST_PASSWORD (aurora-lock built with its test feature).
    if grep -rqs 'AURORA_LOCK_TEST_PASSWORD' "$ROOT/crates/lock" 2>/dev/null; then
        CRED=qa-secret
        cmd="env AURORA_LOCK_TEST_PASSWORD=$CRED $BINDIR/aurora-lock"
    fi
    add_binds <<EOF
"Mod+F5" = "lock"
"Mod+F6" = "spawn touch $SDIR/run/leak"
EOF
    append_cfg <<EOF

[services.lock]
command = "$cmd"
autostart = false
restart = "always"
backoff_ms = 100
max_backoff_ms = 400
EOF
    launch || { end_scenario; return; }
    OFF=0
    absent 'service: started name=lock' "the lock service is not started at boot" 0.5
    term lk; need ':lk:' 10
    sleep 0.5
    dump
    local x y w h png px
    read -r x y w h <<<"$(rect4 "$(dumpwin lk rect)")"
    mark
    key logo F5
    need 'lock: requested' 3
    need 'service: started name=lock pid=[0-9]+' 3
    need 'lock: surface output=winit' 10
    need 'lock: locked' 10
    sleep 0.3
    dump
    if matches '^dump: lock state=locked surfaces=[1-9]' "$(dumpline lock)"; then pass "dump: lock state=locked"; else fail "MISSING log contract line: dump: lock state=locked ('$(dumpline lock)')"; fi
    png=$(shot locked -o winit) || { end_scenario; return; }
    px=$(pixel "$png" $((x + w / 2)) $((y - 2)))
    if [ "${px^^}" != FF0000 ] && [ "${px^^}" != 0000FF ]; then pass "no window border visible while locked"; else fail "window border pixel visible while locked: $px"; fi

    # Binds are refused.
    mark
    key logo F6
    key logo 2
    key logo q
    absent 'action: ' "binds are refused while locked" 0.8
    if [ ! -e "$SDIR/run/leak" ]; then pass "spawn bind did not run"; else fail "bind ran while locked"; fi
    # Requests that move things, and Unlock from anyone but the lock client, are denied over IPC.
    if [ -x "$BINDIR/auroractl" ]; then
        local reply
        reply=$(ctl raw '{"SwitchWorkspace":{"output":null,"index":3}}' 2>&1 | tr -d ' \n')
        if contains "$reply" 'Denied'; then pass "IPC SwitchWorkspace is Denied while locked"; else fail "SwitchWorkspace while locked: $reply"; fi
        reply=$(ctl raw '"Unlock"' 2>&1 | tr -d ' \n')
        if contains "$reply" 'Denied'; then pass "IPC Unlock from another client is Denied"; else fail "Unlock from auroractl: $reply"; fi
        dump
        if matches 'state=locked' "$(dumpline lock)"; then pass "still locked after the Unlock attempt"; else fail "lock state changed: '$(dumpline lock)'"; fi
    fi

    # The emergency VT chord keeps working.
    mark
    key ctrl+altgr F1
    need 'VT switch requested' 3
    if alive; then pass "survives the VT chord while locked"; else fail "died on the VT chord"; fi

    # A dead lock client keeps the session locked; the supervisor starts a new one.
    local lpid
    mark
    lpid=$(OFF=0 pid_from_log lock)
    if [ -n "$lpid" ]; then kill "$lpid" 2>/dev/null; else fail "no lock pid in the log"; fi
    need 'lock: client gone, session stays locked' 5
    need 'service: started name=lock pid=[0-9]+' 5
    need 'lock: surface output=winit' 10
    sleep 0.3
    dump
    if matches 'state=locked' "$(dumpline lock)"; then pass "still locked after the lock client died"; else fail "lock state after client death: '$(dumpline lock)'"; fi

    if [ -n "$CRED" ]; then
        mark
        wl wtype "wrong-password"
        key "" Return
        absent 'lock: unlocked' "a wrong credential does not unlock" 1.2
        wl wtype "$CRED"
        key "" Return
        need 'lock: unlocked' 5
        dump
        if matches 'state=unlocked' "$(dumpline lock)"; then pass "unlocked with the right credential"; else fail "dump after unlock: '$(dumpline lock)'"; fi
        mark
        key logo 2
        need 'action: workspace 2' 3
        key logo 1
        key logo F5
        need 'lock: locked' 10
    else
        skip "unlock with a test credential: crates/lock does not define AURORA_LOCK_TEST_PASSWORD yet (the lock crate's feature docs must, see docs/m4-plan.md)"
    fi

    # The quit chord works even when locked.
    mark
    key ctrl+altgr BackSpace
    need 'quitting: quit chord' 3
    local i
    for i in $(seq 50); do alive || break; sleep 0.1; done
    if alive; then fail "quit chord ignored while locked"; else pass "quit chord works while locked"; fi
    end_scenario
}

# ------------------------------------------------------------------------------------------
# M5 scenarios (docs/m5-plan.md log contract). Written against the plan before the apps existed;
# a scenario whose binary is not built SKIPs. Fixtures live in scripts/qa/.
# Apps run with HOME, XDG_CONFIG_HOME and XDG_DATA_HOME inside the scenario's scratch dir, so they
# never see the real home, theme or trash.

Q=$ROOT/scripts/qa
APPENV=()
# binhas BIN STRING: the binary contains STRING (is the qa-hooks feature compiled in?).
binhas() { grep -qaF -- "$2" "$BINDIR/$1" 2>/dev/null; }
# appc BIN ARGS...: an app as a Wayland client in the scenario's hermetic environment.
# APPENV holds extra NAME=value pairs for the next call.
appc() {
    local b=$1
    shift
    client env HOME="$SDIR/home" XDG_CONFIG_HOME="$SDIR/cfg" XDG_DATA_HOME="$SDIR/data" \
        QA_OUT="$SDIR/run" "${APPENV[@]+"${APPENV[@]}"}" "$BINDIR/$b" "$@"
}
m5_begin() {
    begin "$1"
    mkdir -p "$SDIR/home" "$SDIR/data"
    APPENV=()
}
# win_wait APP_ID SECS: until the compositor dump lists a window with that app_id (DUMP is fresh).
win_wait() {
    local i
    for i in $(seq $(($2 * 2))); do
        dump || return 1
        [ -n "$(dumpwin "$1" rect)" ] && return 0
        sleep 0.5
    done
    return 1
}
# expect_count REGEX N SECS LABEL: at least N client log lines match.
expect_count() {
    if wait_client_count "$1" "$2" "$3"; then pass "client log: $4 (x$2)"; else fail "MISSING log contract line: $4 (want $2)"; fi
}
# samples PNG APP_ID: five pixels along the lower part of that window (DUMP must be fresh).
samples() {
    local x y w h i out=""
    read -r x y w h <<<"$(rect4 "$(dumpwin "$2" rect)")"
    for i in 1 2 3 4 5; do out+="$(pixel "$1" $((x + w * i / 6)) $((y + h * 4 / 5))) "; done
    echo "$out"
}
# changed "A B C.." "D E F.." -> how many positions differ
changed() {
    local -a a b
    local i n=0
    read -ra a <<<"$1"
    read -ra b <<<"$2"
    for i in "${!a[@]}"; do [ "${a[$i]}" = "${b[$i]:-}" ] || n=$((n + 1)); done
    echo "$n"
}
# theme_follow APP_ID LOG_REGEX: pushes a new bg through theme.toml + reload; the app logs its
# theme line and its pixels change without a restart. The app window must be mapped.
theme_follow() {
    local app=$1 re=$2 before after sb sa base
    win_wait "$app" 10 || { fail "$app window not mapped for the theme check"; return; }
    sleep 0.5
    snap before "$app-theme1" -o winit
    sb=$(samples "$before" "$app")
    base=$(clog | grep -acE -- "$re")
    printf '[palette]\nbg = "#443322"\n' >"$SDIR/cfg/aurora/theme.toml"
    mark
    kill -USR1 "$APID"
    need 'theme: changed rev=[0-9]+' 3
    expect_count "$re" $((base + 1)) 5 "$re"
    sleep 1
    snap after "$app-theme2" -o winit
    if [ -z "$before" ] || [ -z "$after" ]; then return; fi
    dump
    sa=$(samples "$after" "$app")
    if [ "$(changed "$sb" "$sa")" -ge 3 ]; then pass "$app repainted with the new theme (${sb% } -> ${sa% })"; else fail "$app pixels did not follow the theme (${sb% } -> ${sa% })"; fi
}

sc_term() {
    require term aurora-term || return
    m5_begin term
    local run=$SDIR/run
    printf '[palette]\nbg = "#112233"\n' >"$SDIR/cfg/aurora/theme.toml"
    launch || { end_scenario; return; }
    local empty full x y w h colors i
    snap empty t-empty -o winit

    # 1. Starts, spawns the fixture on a pty, maps an aurora-term toplevel and paints glyphs.
    appc aurora-term -e "$Q/term-hello.sh"
    local tpid=$LASTPID
    need_client 'term: ready cols=[0-9]+ rows=[0-9]+ scale=[0-9.]+ font=.* cell=[0-9]+x[0-9]+' 15
    need_client 'term: spawn pid=[0-9]+ cmd=.*term-hello' 5
    if waitfor 10 test -e "$run/hello.ready"; then pass "fixture ran inside the pty"; else fail "fixture never started (no hello.ready)"; fi
    if win_wait aurora-term 10; then pass "aurora-term toplevel mapped"; else fail "no window with app_id aurora-term in the dump"; end_scenario; return; fi
    sleep 0.8
    snap full t-full -o winit
    if [ -n "$empty" ] && [ -n "$full" ]; then
        if [ "$(diffpx "$empty" "$full")" -gt 500 ]; then pass "screen differs from the empty frame"; else fail "no visible terminal surface (diff $(diffpx "$empty" "$full") px)"; fi
        read -r x y w h <<<"$(rect4 "$(dumpwin aurora-term rect)")"
        colors=$(magick "$full" -crop "${w}x${h}+${x}+${y}" +repage -format %k info: 2>/dev/null)
        if [ "${colors:-0}" -gt 3 ]; then pass "glyph pixels are not background only ($colors colors in the window)"; else fail "window is a flat color (${colors:-?} colors), no glyphs painted"; fi
    fi
    if matches '^term=xterm-256color colorterm=truecolor program=aurora-term tty0 tty1 ctty$' "$(cat "$run/term-env" 2>/dev/null)"; then pass "child sees TERM/COLORTERM/TERM_PROGRAM and a controlling tty"; else fail "child environment: $(cat "$run/term-env" 2>/dev/null)"; fi

    # 2. A layout change resizes the pty: SIGWINCH reaches the child with a new size.
    local rbase
    rbase=$(clog | grep -acE 'term: resize cols=')
    term sz1
    need ':sz1:' 10
    # The line count is re-read on every try (a plain `test "$(..)"` would expand only once).
    if waitfor 6 bash -c '[ "$(wc -l <"$1")" -ge 2 ]' _ "$run/size.log"; then pass "child got SIGWINCH on resize"; else fail "no SIGWINCH after a layout change (size.log: $(tr '\n' ';' <"$run/size.log"))"; fi
    if [ "$(head -1 "$run/size.log")" != "$(tail -1 "$run/size.log")" ]; then pass "stty size changed: $(head -1 "$run/size.log") -> $(tail -1 "$run/size.log")"; else fail "stty size did not change"; fi
    expect_count 'term: resize cols=[0-9]+ rows=[0-9]+' $((rbase + 1)) 5 'term: resize'

    # Without --hold the window closes when the child exits.
    : >"$run/quit"
    expect_count 'term: exit pid=[0-9]+ code=0' 1 8 'term: exit code=0'
    local gone=0
    for i in 1 2 3 4 5 6; do
        dump
        [ -z "$(dumpwin aurora-term rect)" ] && { gone=1; break; }
        sleep 0.5
    done
    if [ "$gone" = 1 ]; then pass "window closed when the shell exited"; else fail "aurora-term window still mapped after exit"; fi
    if waitfor 5 pid_dead "$tpid"; then pass "aurora-term process ended"; else fail "aurora-term still running after the child exited"; fi

    # 3. Typed bytes round-trip through the pty. Preferred: the qa-hooks input file; else wtype.
    rm -f "$run/typed" "$run/echo.ready"
    if binhas aurora-term AURORA_TERM_TEST_INPUT; then
        printf 'qa-roundtrip\n' >"$SDIR/input.bin"
        APPENV=(AURORA_TERM_TEST_INPUT="$SDIR/input.bin")
        appc aurora-term -e "$Q/term-echo.sh"
        APPENV=()
    else
        appc aurora-term -e "$Q/term-echo.sh"
        win_wait aurora-term 10
        waitfor 5 test -e "$run/echo.ready"
        sleep 0.5
        wl wtype 'qa-roundtrip'
        key "" Return
    fi
    if waitfor 10 test -s "$run/typed" && [ "$(cat "$run/typed")" = qa-roundtrip ]; then pass "typed line reached the child through the pty"; else fail "round trip failed (typed: '$(cat "$run/typed" 2>/dev/null)')"; fi
    expect_count 'term: exit pid=[0-9]+ code=0' 2 8 'term: exit code=0'

    # 4. Flooding: 50 MB of output finishes in time and the compositor keeps answering. The
    # fixture holds the output until flood.go, then snapshots are probed back to back until it
    # is done; at least one answer must arrive while the flood is still running.
    rm -f "$run/flood.ready" "$run/flood.go" "$run/flood.done"
    appc aurora-term -e "$Q/term-flood.sh"
    waitfor 10 test -e "$run/flood.ready" || fail "flood fixture never started (no flood.ready)"
    local t0=$SECONDS probes=0 answered=0 during=0
    : >"$run/flood.go"
    if [ -x "$BINDIR/auroractl" ]; then
        while [ "$probes" -lt 3 ] || { [ ! -e "$run/flood.done" ] && [ $((SECONDS - t0)) -lt 60 ]; }; do
            probes=$((probes + 1))
            if timeout 5 env -u DISPLAY AURORA_IPC_SOCK="$IPC" "$BINDIR/auroractl" snapshot >/dev/null 2>&1; then
                answered=$((answered + 1))
                [ -e "$run/flood.done" ] || during=$((during + 1))
            fi
            sleep 0.05
        done
        if [ "$answered" = "$probes" ] && [ "$during" -ge 1 ]; then
            pass "compositor answers auroractl snapshot during a flood ($during answered mid-flood, $answered/$probes total)"
        else
            fail "auroractl snapshot during the flood: $answered/$probes answered, $during while it ran"
        fi
    else
        skip "auroractl not built: flood responsiveness check"
    fi
    if waitfor 60 test -e "$run/flood.done"; then pass "50 MB flood completed in $((SECONDS - t0)) s"; else fail "flood did not finish within 60 s"; fi
    expect_count 'term: exit pid=[0-9]+ code=0' 3 10 'term: exit code=0'
    if alive; then pass "compositor alive after the flood"; else fail "compositor died during the flood"; fi

    # 5. Theme: a changed theme.toml repaints the running terminal.
    rm -f "$run/quit" "$run/hello.ready"
    appc aurora-term -e "$Q/term-hello.sh"
    local hpid=$LASTPID
    waitfor 10 test -e "$run/hello.ready"
    theme_follow aurora-term 'term: theme rev=[0-9]+'

    # 6. Killing one terminal leaves the compositor and other windows alone.
    kill "$hpid" 2>/dev/null
    waitfor 5 pid_dead "$hpid"
    sleep 0.5
    dump
    if alive && [ -n "$(dumpwin sz1 rect)" ] && [ -z "$(dumpwin aurora-term rect)" ]; then pass "kill <pid> removes only the terminal window"; else fail "state after killing aurora-term is wrong: $(grep -a '^dump: win' <<<"$DUMP" | cut -c1-100 | tr '\n' '|')"; fi

    # 7. No IPC socket: still starts, draws, stays up.
    rm -f "$run/quit" "$run/hello.ready"
    APPENV=(AURORA_IPC_SOCK="$SDIR/run/missing.sock")
    appc aurora-term -e "$Q/term-hello.sh"
    APPENV=()
    local npid=$LASTPID
    expect_count 'term: ready cols=' 5 15 'term: ready (no IPC)'
    if win_wait aurora-term 10; then pass "maps without an IPC socket"; else fail "no window without IPC"; fi
    sleep 1
    if kill -0 "$npid" 2>/dev/null; then pass "stays up without IPC"; else fail "aurora-term exited without IPC"; fi
    kill "$npid" 2>/dev/null
    : >"$run/quit"

    # Clipboard (term: clipboard set / paste bytes) needs a pointer drag; there is no injector
    # on the headless host, it is on the hardware checklist.
    NAME=term
    skip "clipboard: selection needs pointer injection, covered on hardware"
    end_scenario
}

# ------------------------------------------------------------------------------------------

# fstart LABEL KEYS...: fresh fixture, then aurora-files on it driven by a key script
# (qa-hooks). One argument per script line, format of crates/files/src/testscript.rs: key names
# with ctrl+/shift+/alt+ prefixes, `type <string>` types a string.
FP_PID=""
FP_BASE=0
fstart() {
    local label=$1
    shift
    bash "$Q/files-fixture.sh" "$SCRATCH" "$SDIR/work" || { fail "fixture for $label"; FP_PID=""; return 1; }
    printf '%s\n' "$@" >"$SDIR/script-$label.txt"
    FP_BASE=$(clog | grep -acE 'files: op done id=[0-9]+ ok=[1-9][0-9]* failed=0')
    APPENV=(AURORA_FILES_TEST_SCRIPT="$SDIR/script-$label.txt")
    appc aurora-files "$SDIR/work"
    APPENV=()
    FP_PID=$LASTPID
}
# fwait KIND: the op started and finished with nothing failed, counted per app instance.
fwait() {
    if wait_client_count "files: op start id=[0-9]+ kind=$1 " 1 15 && wait_client_count 'files: op done id=[0-9]+ ok=[1-9][0-9]* failed=0' $((FP_BASE + 1)) 15; then
        pass "files: op $1 start + done"
    else
        fail "MISSING log contract line: files: op start/done kind=$1 ($(clog | grep -a 'files: op' | tail -2 | tr '\n' '|'))"
    fi
}
fend() {
    [ -n "$FP_PID" ] || return 0
    kill "$FP_PID" 2>/dev/null
    waitfor 5 pid_dead "$FP_PID"
    FP_PID=""
}

sc_files() {
    require files aurora-files || return
    m5_begin files
    [ -n "$SDIR" ] || return
    local W=$SDIR/work run=$SDIR/run
    printf '[palette]\nbg = "#112233"\n' >"$SDIR/cfg/aurora/theme.toml"
    launch || { end_scenario; return; }
    local empty full info

    snap empty f-empty -o winit

    # 1. Starts on the fixture directory (argv[1]), lists it, maps a toplevel.
    bash "$Q/files-fixture.sh" "$SCRATCH" "$W" || { fail "fixture"; end_scenario; return; }
    appc aurora-files "$W"
    need_client "files: ready path=$W entries=6" 15
    if win_wait aurora-files 10; then pass "aurora-files toplevel mapped"; else fail "no window with app_id aurora-files"; end_scenario; return; fi
    sleep 0.8
    snap full f-full -o winit
    if [ -n "$empty" ] && [ -n "$full" ]; then
        if [ "$(diffpx "$empty" "$full")" -gt 500 ]; then pass "screen differs from the empty frame"; else fail "no visible files surface (diff $(diffpx "$empty" "$full") px)"; fi
    fi

    # 2. Theme push repaints it.
    FP_PID=$LASTPID
    theme_follow aurora-files 'files: theme rev=[0-9]+'
    fend

    # 3. File operations driven by AURORA_FILES_TEST_SCRIPT (qa-hooks build only). Every phase
    # starts from a fresh fixture; sorted order is: alpha beta a.txt b.txt c.txt zzz-delete-me.txt.
    if binhas aurora-files AURORA_FILES_TEST_SCRIPT; then
        fstart select Home ctrl+a
        expect_count 'files: select count=6' 1 10 'files: select count=6'
        fend

        fstart mkdir ctrl+shift+n 'type qa-newdir' Return
        fwait mkdir
        if waitfor 3 test -d "$W/qa-newdir"; then pass "new folder created"; else fail "qa-newdir missing"; fi
        fend

        fstart rename Home Down Down F2 ctrl+a 'type renamed.txt' Return
        fwait rename
        if [ -f "$W/renamed.txt" ] && [ ! -e "$W/a.txt" ] && [ "$(cat "$W/renamed.txt")" = a-content ]; then pass "a.txt renamed, content intact"; else fail "rename result wrong: $(ls "$W" | tr '\n' ' ')"; fi
        fend

        fstart copy Home Down Down Down ctrl+c Home Return ctrl+v
        fwait copy
        if [ "$(cat "$W/alpha/b.txt" 2>/dev/null)" = b-content ] && [ -f "$W/b.txt" ]; then pass "b.txt copied into alpha, original kept"; else fail "copy result wrong: alpha=$(ls "$W/alpha" | tr '\n' ' ') top=$(ls "$W" | tr '\n' ' ')"; fi
        fend

        fstart move Home Down Down Down Down ctrl+x Home Down Return ctrl+v
        fwait move
        if [ "$(cat "$W/beta/c.txt" 2>/dev/null)" = c-content ] && [ ! -e "$W/c.txt" ]; then pass "c.txt moved into beta"; else fail "move result wrong: beta=$(ls "$W/beta" | tr '\n' ' ') top=$(ls "$W" | tr '\n' ' ')"; fi
        fend

        rm -rf "${SDIR:?}/data/Trash"
        fstart trash End Delete
        fwait trash
        if [ ! -e "$W/zzz-delete-me.txt" ] && [ -f "$SDIR/data/Trash/files/zzz-delete-me.txt" ]; then pass "trashed into \$XDG_DATA_HOME/Trash/files"; else fail "trash result wrong: $(ls "$W" | tr '\n' ' ') trash=$(ls "$SDIR/data/Trash/files" 2>&1 | tr '\n' ' ')"; fi
        info=$SDIR/data/Trash/info/zzz-delete-me.txt.trashinfo
        if grep -q '^\[Trash Info\]' "$info" 2>/dev/null && grep -q "^Path=.*zzz-delete-me.txt" "$info" && grep -q '^DeletionDate=' "$info"; then pass ".trashinfo is spec shaped"; else fail ".trashinfo missing or malformed: $(head -4 "$info" 2>&1 | tr '\n' '|')"; fi
        fend

        rm -rf "${SDIR:?}/data/Trash"
        fstart delete End shift+Delete Return
        fwait delete
        if [ ! -e "$W/zzz-delete-me.txt" ] && [ ! -e "$SDIR/data/Trash/files/zzz-delete-me.txt" ]; then pass "permanent delete removed it without trashing"; else fail "permanent delete result wrong: $(ls "$W" | tr '\n' ' ')"; fi
        fend
        if alive; then pass "compositor alive after the file operations"; else fail "compositor died during the file operations"; fi
    else
        NAME=files
        skip "aurora-files built without qa-hooks (AURORA_FILES_TEST_SCRIPT absent): operation phases (build with --features qa-hooks)"
    fi

    # 4. Unreadable directory: an error state, not a crash. (root ignores modes, so SKIP there.)
    if [ "$(id -u)" = 0 ]; then
        NAME=files
        skip "running as root, chmod 000 does not deny access"
    else
        bash "$Q/files-fixture.sh" "$SCRATCH" "$W" && mkdir "$W/locked" && chmod 000 "$W/locked"
        appc aurora-files "$W/locked"
        local lpid=$LASTPID
        if win_wait aurora-files 10; then pass "window maps on an unreadable directory"; else fail "no window on an unreadable directory"; fi
        sleep 1.5
        if kill -0 "$lpid" 2>/dev/null; then pass "aurora-files survives a chmod 000 directory"; else fail "aurora-files died on an unreadable directory"; fi
        if clog | grep -qaiE 'panicked'; then fail "panic on an unreadable directory"; else pass "no panic on an unreadable directory"; fi
        kill "$lpid" 2>/dev/null
        waitfor 5 pid_dead "$lpid"
        chmod u+rwx "$W/locked"
    fi

    # 5. Independence: kill leaves the compositor and other windows alone; no IPC still works.
    term keep1
    need ':keep1:' 10
    bash "$Q/files-fixture.sh" "$SCRATCH" "$W"
    appc aurora-files "$W"
    local kpid=$LASTPID
    win_wait aurora-files 10
    kill "$kpid" 2>/dev/null
    waitfor 5 pid_dead "$kpid"
    sleep 0.5
    dump
    if alive && [ -n "$(dumpwin keep1 rect)" ] && [ -z "$(dumpwin aurora-files rect)" ]; then pass "kill <pid> removes only the files window"; else fail "state after killing aurora-files is wrong: $(grep -a '^dump: win' <<<"$DUMP" | cut -c1-100 | tr '\n' '|')"; fi
    local nbase
    nbase=$(clog | grep -acE 'files: ready path=')
    APPENV=(AURORA_IPC_SOCK="$SDIR/run/missing.sock")
    appc aurora-files "$W"
    APPENV=()
    local npid=$LASTPID
    expect_count 'files: ready path=' $((nbase + 1)) 15 'files: ready (no IPC)'
    sleep 1
    if kill -0 "$npid" 2>/dev/null; then pass "stays up without IPC"; else fail "aurora-files exited without IPC"; fi
    kill "$npid" 2>/dev/null
    NAME=files
    end_scenario
}

# Display protocols through the qa_display example client (a dev-only build:
# `cargo build -p aurora-comp --examples`), plus the power actions over a bind and IPC.
sc_display() {
    NAME=display
    local qd=${AURORA_QA_DISPLAY:-$ROOT/target/debug/examples/qa_display}
    if [ ! -x "$qd" ]; then
        skip "qa_display is not built at $qd (cargo build -p aurora-comp --examples)"
        return
    fi
    begin display
    add_binds <<'EOF'
"Mod+F10" = "power-off-monitors"
EOF
    launch || { end_scenario; return; }
    local out

    out=$(wl timeout 10 "$qd" heads 2>&1)
    if contains "$out" 'global=zwlr_output_manager_v1 version=4'; then pass "output manager v4 advertised"; else fail "no output manager: ${out:0:200}"; fi
    if matches '^head name=winit enabled=1 mode=[0-9]+x[0-9]+@[0-9]+ pos=0,0 scale=1.00 transform=0 modes=1' "$out"; then pass "winit head described"; else fail "winit head: ${out:0:300}"; fi
    if matches '^serial=Some\([0-9]+\)' "$out"; then pass "done carries a serial"; else fail "no done: ${out:0:300}"; fi

    mark
    out=$(wl timeout 10 "$qd" place winit 0 0 2 2>&1)
    if contains "$out" 'result=succeeded'; then pass "apply succeeds"; else fail "apply: ${out:0:300}"; fi
    need 'output-management: applied 1 head' 3
    if matches 'head name=winit enabled=1 .* scale=2.00' "$out"; then pass "head reports the applied scale"; else fail "scale not applied: ${out:0:300}"; fi

    out=$(wl timeout 10 "$qd" test-scale winit 9 2>&1)
    if contains "$out" 'result=failed'; then pass "test refuses scale 9"; else fail "test scale 9: ${out:0:200}"; fi
    out=$(wl timeout 10 "$qd" test-scale winit 1.5 2>&1)
    if contains "$out" 'result=succeeded'; then pass "test accepts scale 1.5"; else fail "test scale 1.5: ${out:0:200}"; fi
    out=$(wl timeout 10 "$qd" stale 2>&1)
    if contains "$out" 'result=cancelled'; then pass "outdated serial is cancelled"; else fail "stale: ${out:0:200}"; fi

    mark
    kill -USR1 "$APID"
    need 'output-management: reload drops runtime changes to 1 output' 5
    out=$(wl timeout 10 "$qd" heads 2>&1)
    if matches 'head name=winit enabled=1 .* scale=1.00' "$out"; then pass "reload puts the config scale back"; else fail "after reload: ${out:0:300}"; fi

    mark
    out=$(wl timeout 10 "$qd" power winit off 2>&1)
    if contains "$out" 'power modes=[1, 0] failed=false'; then pass "power control reports on, then off"; else fail "power: ${out:0:300}"; fi
    need 'power: output=winit off' 3
    if [ -x "$BINDIR/auroractl" ]; then
        mark
        ctl raw '"PowerOnMonitors"' >/dev/null
        need 'power: output=winit on' 3
        key logo F10
        need 'action: power-off-monitors' 3
        need 'power: output=winit off' 3
        ctl raw '"PowerOnMonitors"' >/dev/null
    else
        skip "auroractl not built, IPC power requests not run"
    fi

    mark
    out=$(wl timeout 10 "$qd" gamma winit 2>&1)
    if contains "$out" 'gamma size=None failed=true'; then pass "gamma control fails on the nested backend"; else fail "gamma: ${out:0:200}"; fi
    need 'gamma: output=winit has no hardware gamma' 3

    out=$(wl timeout 10 "$qd" tearing-twice 2>&1)
    if contains "$out" 'tearing first=ok' && contains "$out" 'tearing second=protocol-error'; then pass "second tearing control is a protocol error"; else fail "tearing: ${out:0:300}"; fi

    if alive; then pass "compositor alive after the display clients"; else fail "compositor died"; fi
    end_scenario
}

ALL=(keys emergency reload tiling workspaces layer xwayland multi robust anim effects overview xscale ipc services theme shell launcher notifd lock term files display)
if [ $# -eq 0 ]; then set -- "${ALL[@]}"; fi
for s in "$@"; do
    declare -F "sc_$s" >/dev/null || { echo "unknown scenario $s" >&2; exit 2; }
done
for s in "$@"; do
    "sc_$s"
    stop_aurora
done
echo "== summary: $PASS passed, $FAIL failed, $SKIP skipped"
[ "$FAIL" = 0 ]
