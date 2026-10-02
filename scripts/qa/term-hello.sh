#!/bin/bash
# Fixture for the term scenario (run as `aurora-term -e term-hello.sh`). Needs QA_OUT (a scratch dir).
# Prints known text, records what the pty looks like from the inside, logs `stty size` on every
# SIGWINCH, and exits 0 once $QA_OUT/quit exists.
out=${QA_OUT:?QA_OUT not set}
printf 'AURORA-TERM-QA hello\n'
printf '\033[1mbold\033[0m \033[31mred\033[0m \033[38;2;10;200;30mtruecolor\033[0m\n'
# Test the tty before redirecting: inside the redirected block stdout is the file.
tty=""
[ -t 0 ] && tty+=" tty0"
[ -t 1 ] && tty+=" tty1"
# /dev/tty only opens for a process with a controlling terminal (setsid + TIOCSCTTY).
{ : </dev/tty; } 2>/dev/null && tty+=" ctty"
printf 'term=%s colorterm=%s program=%s%s\n' "${TERM:-}" "${COLORTERM:-}" "${TERM_PROGRAM:-}" "$tty" >"$out/term-env"
stty size >"$out/size.log"
trap 'stty size >>"$out/size.log"' WINCH
: >"$out/hello.ready"
while [ ! -e "$out/quit" ]; do sleep 0.1; done
exit 0
