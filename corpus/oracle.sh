#!/usr/bin/env bash
# Regenerate .expected from CPython for the layer Oro still keeps in step with
# it: the computational core (arithmetic, strings, containers, control flow,
# sorting). Those core/, later/, and known-failing/ scripts are valid Python 3.6+
# by design, so CPython is ground truth (the known-failing ones are CORRECT
# Python that Oro gets wrong).
#
# divergence/ is excluded: those programs deliberately behave differently, or use
# Oro-only spellings (`proc`, `=>`, `.map`, the `to_` casts) that CPython cannot
# run at all. They are checked by run.sh against reviewed .expected files.
set -uo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
shopt -s nullglob
for f in "$DIR"/core/*.oro "$DIR"/later/*.oro "$DIR"/known-failing/*.oro; do
  out=$(python3 -c "
import sys
sys.setrecursionlimit(20000)
exec(open('$f').read())
" 2>&1)
  if [[ $? -ne 0 ]]; then echo "ORACLE FAIL $(basename "$f")"; echo "$out" | tail -3
  else printf '%s\n' "$out" > "${f%.oro}.expected"; echo "ok $(basename "$f")"; fi
done
