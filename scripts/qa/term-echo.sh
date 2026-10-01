#!/bin/bash
# Fixture for the term scenario: reads one line typed into the terminal, stores it in
# $QA_OUT/typed and exits 0.
out=${QA_OUT:?QA_OUT not set}
: >"$out/echo.ready"
IFS= read -r line
printf '%s' "$line" >"$out/typed"
exit 0
