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

PASS=0 FAIL=0
APID="" SOCK="" XD="" LOG="" OFF=0 SDIR="" NAME="" CFG="" LASTPID="" MATCH=""
CPIDS=()

pass() { PASS=$((PASS + 1)); echo "PASS [$NAME] $1"; }
fail() { FAIL=$((FAIL + 1)); echo "FAIL [$NAME] $1"; }
contains() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }
matches() { grep -qaE -- "$1" <<<"$2"; }
# ok NAME COMMAND...   passes when the command succeeds
ok() { local n=$1; shift; if "$@" >/dev/null 2>&1; then pass "$n"; else fail "$n"; fi; }

wl() { env -u DISPLAY WAYLAND_DISPLAY="$SOCK" "$@"; }
xc() { env -u WAYLAND_DISPLAY DISPLAY="$XD" "$@"; }

client() {
    [ -n "$SOCK" ] || { fail "client without SOCK"; return 1; }
    wl "$@" >>"$SDIR/clients.log" 2>&1 &
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
trap 'stop_aurora; exit 130' INT TERM
trap 'stop_aurora' EXIT

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
        "$BIN" --winit --qa --timeout "${QA_TIMEOUT:-40}" --config "$CFG" -c true \
        >"$SDIR/stdout.log" 2>&1 &
    APID=$!
    OFF=0
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

wait_x() {
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
    wl grim "${@:2}" "$out" 2>>"$SDIR/clients.log"
    [ -s "$out" ] || { fail "screenshot $1"; return 1; }
    echo "$out"
}
# pixel FILE X Y -> RRGGBB
pixel() { magick "$1" -format "%[hex:p{$2,$3}]" info: 2>/dev/null | cut -c1-6; }

end_scenario() {
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

sc_layer() {
    begin layer
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
    if [ -z "$(dumpline overview)" ] || matches 'open=0' "$(dumpline overview)"; then pass "no open overview in the dump before opening"; else fail "overview listed while closed: $(dumpline overview)"; fi

    mark
    key logo F3
    need 'overview: open' 3
    sleep 0.3
    dump
    if [ -n "$(dumpline overview)" ] && ! matches 'open=0' "$(dumpline overview)"; then
        pass "dump: overview while open"
    else
        fail "MISSING log contract line: dump: overview (while open): '$(dumpline overview)'"
    fi

    mark
    key "" Escape
    need 'overview: close' 3
    sleep 0.5
    dump
    if [ -z "$(dumpline overview)" ] || matches 'open=0' "$(dumpline overview)"; then pass "Escape closed the overview"; else fail "overview still open: $(dumpline overview)"; fi

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

ALL=(keys emergency reload tiling workspaces layer xwayland multi robust anim effects overview xscale)
if [ $# -eq 0 ]; then set -- "${ALL[@]}"; fi
for s in "$@"; do
    declare -F "sc_$s" >/dev/null || { echo "unknown scenario $s" >&2; exit 2; }
done
for s in "$@"; do
    "sc_$s"
    stop_aurora
done
echo "== summary: $PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
