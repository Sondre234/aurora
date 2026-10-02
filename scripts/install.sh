#!/bin/bash
# Installs Aurora for daily use, for the current user only. Idempotent: run it again after
# every pull. Never needs root; the one system-wide step (the display manager's session
# entry) is printed for you to run, not run.
#
#   scripts/install.sh [--dry-run]
#
# What it does, in order:
#   1. cargo build --release --workspace
#   2. Installs the binaries (aurora-comp, auroractl, aurora-shell, aurora-launcher,
#      aurora-notifd, aurora-lock, aurora-term, aurora-files) and scripts/aurora-session into
#      $PREFIX/bin (default ~/.local/bin). A running session keeps its old binaries: install
#      replaces the files, it does not write into them.
#   3. contrib/portals/aurora-portals.conf -> $XDG_CONFIG_HOME/xdg-desktop-portal/
#   4. contrib/systemd/aurora-session.target -> $XDG_CONFIG_HOME/systemd/user/, then
#      systemctl --user daemon-reload
#   5. Writes target/aurora.desktop (contrib/aurora.desktop with Exec= pointing at the
#      installed aurora-session, since SDDM does not look in ~/.local and may not have it on
#      PATH) and prints the sudo command that copies it to /usr/share/wayland-sessions/.
#
# --dry-run prints every step and changes nothing (no build, no files written).
# Environment: PREFIX (default ~/.local), XDG_CONFIG_HOME (default ~/.config).
set -eu

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
PREFIX=${PREFIX:-$HOME/.local}
BIN_DIR=$PREFIX/bin
CONFIG=${XDG_CONFIG_HOME:-$HOME/.config}
# [[bin]] names from crates/*/Cargo.toml (ctl's binary is its package name).
BINARIES=(aurora-comp auroractl aurora-shell aurora-launcher aurora-notifd aurora-lock aurora-term aurora-files)
DESKTOP_OUT=$ROOT/target/aurora.desktop
SESSIONS=/usr/share/wayland-sessions

DRY=""
for arg in "$@"; do
    case $arg in
        --dry-run | -n) DRY=1 ;;
        -h | --help) sed -n '2,24p' "$0"; exit 0 ;;
        *) echo "install.sh: unknown argument $arg (try --dry-run)" >&2; exit 2 ;;
    esac
done

run() {
    if [ -n "$DRY" ]; then
        printf '+'; printf ' %q' "$@"; printf '\n'
    else
        "$@"
    fi
}

run cargo build --release --workspace --manifest-path "$ROOT/Cargo.toml"

for bin in "${BINARIES[@]}"; do
    src=$ROOT/target/release/$bin
    if [ -z "$DRY" ] && [ ! -x "$src" ]; then
        echo "install.sh: $src was not built" >&2
        exit 1
    fi
    run install -Dm755 "$src" "$BIN_DIR/$bin"
done
run install -Dm755 "$ROOT/scripts/aurora-session" "$BIN_DIR/aurora-session"

run install -Dm644 "$ROOT/contrib/portals/aurora-portals.conf" \
    "$CONFIG/xdg-desktop-portal/aurora-portals.conf"
run install -Dm644 "$ROOT/contrib/systemd/aurora-session.target" \
    "$CONFIG/systemd/user/aurora-session.target"
if command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1; then
    run systemctl --user daemon-reload
else
    echo "install.sh: no systemd user manager reachable, skipping daemon-reload" >&2
fi

if [ -n "$DRY" ]; then
    echo "+ write $DESKTOP_OUT with Exec=$BIN_DIR/aurora-session"
else
    mkdir -p "$(dirname "$DESKTOP_OUT")"
    sed -e "s|^Exec=.*|Exec=$BIN_DIR/aurora-session|" \
        -e "s|^TryExec=.*|TryExec=$BIN_DIR/aurora-session|" \
        "$ROOT/contrib/aurora.desktop" >"$DESKTOP_OUT"
fi

echo
if cmp -s "$DESKTOP_OUT" "$SESSIONS/aurora.desktop" 2>/dev/null; then
    echo "$SESSIONS/aurora.desktop is up to date."
else
    echo "To list Aurora in SDDM, run this once yourself (the only step that needs root):"
    printf '  sudo install -Dm644 %q %q\n' "$DESKTOP_OUT" "$SESSIONS/aurora.desktop"
fi
config_file=$CONFIG/aurora/config.toml
if [ ! -f "$config_file" ]; then
    echo "No $config_file yet: start from config/aurora.example.toml and contrib/config.daily.toml."
fi
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "Note: $BIN_DIR is not on PATH; binds that spawn aurora-* by name need it there." ;;
esac
