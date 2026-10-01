#!/bin/bash
# Fixture for the term scenario: floods the terminal with 50 MB of output, then marks
# completion and exits 0.
out=${QA_OUT:?QA_OUT not set}
yes 'aurora flood line 0123456789 abcdefghijklmnopqrstuvwxyz' | head -c 50000000
: >"$out/flood.done"
exit 0
