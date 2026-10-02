#!/bin/bash
# Fixture for the term scenario: once $QA_OUT/flood.go exists, floods the terminal with 50 MB of
# output, then marks completion and exits 0. The go file lets the scenario start its
# responsiveness probes together with the output instead of racing it.
out=${QA_OUT:?QA_OUT not set}
: >"$out/flood.ready"
while [ ! -e "$out/flood.go" ]; do sleep 0.05; done
yes 'aurora flood line 0123456789 abcdefghijklmnopqrstuvwxyz' | head -c 50000000
: >"$out/flood.done"
exit 0
