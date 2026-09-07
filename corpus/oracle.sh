#!/usr/bin/env bash
# Regenerate .expected from CPython. Every core/, later/, and known-failing/
# script is valid Python 3.6+ by design, so CPython is ground truth (the
# known-failing ones are CORRECT Python that Oro gets wrong). divergence/ is
# excluded — those intentionally differ from Python.
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
