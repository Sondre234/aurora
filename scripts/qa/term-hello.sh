#!/bin/bash
# Fixture for the term scenario (run as `aurora-term -e term-hello.sh`). Needs QA_OUT (a scratch dir).
# Prints known text, records what the pty looks like from the inside, logs `stty size` on every
# SIGWINCH, and exits 0 once $QA_OUT/quit exists.
out=${QA_OUT:?QA_OUT not set}
printf 'AURORA-TERM-QA hello\n'
printf '\033[1mbold\033[0m \033[31mred\033[0m \033[38;2;10;200;30mtruecolor\033[0m\n'
{
    printf 'term=%s colorterm=%s program=%s' "${TERM:-}" "${COLORTERM:-}" "${TERM_PROGRAM:-}"
    [ -t 0 ] && printf ' tty0'
    [ -t 1 ] && printf ' tty1'
    printf '\n'
} >"$out/term-env"
stty size >"$out/size.log"
trap 'stty size >>"$out/size.log"' WINCH
: >"$out/hello.ready"
while [ ! -e "$out/quit" ]; do sleep 0.1; done
exit 0
