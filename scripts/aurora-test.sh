#!/bin/bash
# Start Aurora on a TTY with every M4 service enabled, for hand testing. This is what the
# `aurora` and `aurora-long` shell aliases run.
#
#   scripts/aurora-test.sh [extra aurora-comp args]
#
# What it does, in order:
#   1. cargo build --release --workspace (a no-op when nothing changed), so the binaries you
#      test are never stale. Aborts if the build fails.
#   2. Builds a merged config in $XDG_RUNTIME_DIR/aurora-test/ from your real
#      ~/.config/aurora/config.toml (never edited) plus: [services.shell],
#      [services.launcher], [services.lock] and the binds Mod+space (launcher) and Mod+l
#      (lock), each only if your file does not already define them. theme.toml is linked in
#      next to it so the services follow your theme. [services.notifd] is added only when
#      nothing owns org.freedesktop.Notifications on your session bus (dunst would win).
#      [session] import_environment is forced off, so a test run never repoints the
#      systemd/D-Bus environment of the session you may have open on another VT.
#   3. Puts target/release on PATH so the services are found by name, and runs
#      aurora-comp --drm with the merged config.
#
# Edits to your real config.toml take effect on the next launch, not on Mod+Shift+r.
# Environment: AURORA_TIMEOUT (seconds, default 30, 0 = no timeout),
#              AURORA_TEST_BASE (config to start from), AURORA_NO_BUILD=1 (skip the build),
#              AURORA_BACKEND (drm by default; winit for nested runs),
#              AURORA_DRY_RUN=1 (write the merged config, print it, do not start).
set -eu

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BIN=$ROOT/target/release
BASE=${AURORA_TEST_BASE:-${XDG_CONFIG_HOME:-$HOME/.config}/aurora/config.toml}
TIMEOUT=${AURORA_TIMEOUT:-30}
OUT=${XDG_RUNTIME_DIR:-/tmp}/aurora-test

if [ -z "${AURORA_NO_BUILD:-}" ]; then
    (cd "$ROOT" && cargo build --release --workspace --quiet) || {
        echo "aurora-test: release build failed, not starting" >&2
        exit 1
    }
fi
[ -x "$BIN/aurora-comp" ] || { echo "aurora-test: no $BIN/aurora-comp" >&2; exit 1; }

mkdir -p "$OUT"
chmod 700 "$OUT"
CFG=$OUT/config.toml
if [ -f "$BASE" ]; then cp "$BASE" "$CFG"; else : >"$CFG"; fi
BASEDIR=$(dirname "$BASE")
[ -f "$BASEDIR/theme.toml" ] && ln -sf "$BASEDIR/theme.toml" "$OUT/theme.toml"

# Inserts the given lines right under the [keybinds] header (adds the table when missing).
add_binds() {
    local tmp=$OUT/config.tmp
    if grep -q '^\[keybinds\]' "$CFG"; then
        awk -v add="$1" '{ print } /^\[keybinds\]/ { print add }' "$CFG" >"$tmp"
        mv "$tmp" "$CFG"
    else
        printf '\n[keybinds]\n%s\n' "$1" >>"$CFG"
    fi
}
has() { grep -qF -- "$1" "$CFG"; }

binds=""
has '"Mod+space"' || binds+='"Mod+space" = "spawn aurora-launcher toggle"'$'\n'
has '"Mod+l"' || binds+='"Mod+l" = "lock"'$'\n'
[ -z "$binds" ] || add_binds "${binds%$'\n'}"

service() { # name, extra lines
    has "[services.$1]" && return 0
    printf '\n[services.%s]\ncommand = "aurora-%s"\n%s' "$1" "$1" "$2" >>"$CFG"
}
service shell 'restart = "always"'$'\n'
service launcher 'restart = "always"'$'\n'
service lock 'autostart = false'$'\n''restart = "always"'$'\n'

owner=""
if command -v busctl >/dev/null 2>&1; then
    owner=$(busctl --user --no-pager status org.freedesktop.Notifications 2>/dev/null | head -1 || true)
fi
if [ -z "$owner" ]; then
    service notifd ''
else
    echo "aurora-test: org.freedesktop.Notifications is owned ($owner); not starting aurora-notifd" >&2
fi

# A test run must not point the systemd/D-Bus environment (portals, polkit) of the session
# you may still have open on another VT at itself: no [session] import_environment.
if grep -q '^[[:space:]]*import_environment[[:space:]]*=' "$CFG"; then
    sed -i 's/^[[:space:]]*import_environment[[:space:]]*=.*/import_environment = false/' "$CFG"
elif grep -q '^\[session\]' "$CFG"; then
    awk '{ print } /^\[session\]/ { print "import_environment = false" }' "$CFG" >"$OUT/config.tmp"
    mv "$OUT/config.tmp" "$CFG"
else
    printf '\n[session]\nimport_environment = false\n' >>"$CFG"
fi

export PATH=$BIN:$PATH
args=(--"${AURORA_BACKEND:-drm}" --config "$CFG")
if [ "$TIMEOUT" = 0 ]; then args+=(--no-timeout); else args+=(--timeout "$TIMEOUT"); fi
echo "aurora-test: config $CFG (base $BASE), timeout ${TIMEOUT}s" >&2
if [ -n "${AURORA_DRY_RUN:-}" ]; then echo "--- $CFG"; cat "$CFG"; echo "--- would run: aurora-comp ${args[*]} $*"; exit 0; fi
exec "$BIN/aurora-comp" "${args[@]}" "$@"
