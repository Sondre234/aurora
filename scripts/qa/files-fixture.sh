#!/bin/bash
# Recreates the known fixture tree of the files scenario.
#   files-fixture.sh SCRATCH_ROOT DIR
# DIR must be a path strictly below SCRATCH_ROOT (the QA scratch directory); anything else is
# refused, because the tree is deleted first. Result, 6 entries (dirs first when sorted by name):
#   alpha/ (inner.txt)  beta/  a.txt  b.txt  c.txt  zzz-delete-me.txt
set -eu
root=${1:?usage: files-fixture.sh SCRATCH_ROOT DIR}
d=${2:?usage: files-fixture.sh SCRATCH_ROOT DIR}
case "$root" in /?*) ;; *) echo "files-fixture: bad scratch root '$root'" >&2; exit 2 ;; esac
case "$d" in
    "$root"/?*/work | "$root"/?*/work/*) ;;
    *) echo "files-fixture: refusing '$d' (not <scratch>/<scenario>/work)" >&2; exit 2 ;;
esac
case "$d" in *..*) echo "files-fixture: refusing '$d'" >&2; exit 2 ;; esac
chmod -R u+rwX "$d" 2>/dev/null || true
rm -rf "$d"
mkdir -p "$d/alpha" "$d/beta"
printf 'a-content\n' >"$d/a.txt"
printf 'b-content\n' >"$d/b.txt"
printf 'c-content\n' >"$d/c.txt"
printf 'delete-me\n' >"$d/zzz-delete-me.txt"
printf 'inner\n' >"$d/alpha/inner.txt"
