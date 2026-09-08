#!/usr/bin/env bash
# Run every core script through the oro binary and diff against .expected.
set -uo pipefail
ORO="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/target/release/oro}"
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
pass=0; fail=0
for f in "$DIR"/core/*.oro; do
  exp="${f%.oro}.expected"
  got=$("$ORO" "$f" 2>&1)
  if [[ "$got" == "$(cat "$exp")" ]]; then
    pass=$((pass+1))
  else
    fail=$((fail+1))
    echo "FAIL $(basename "$f")"
    diff <(printf '%s\n' "$got") "$exp" | head -12
  fi
done
# Divergence: programs that deliberately do NOT match CPython (they may not even
# run under it), so the oracle cannot generate their .expected. Those files are
# reviewed by hand — but they are still *checked*, because "intentionally
# different" must not become "quietly broken".
if compgen -G "$DIR/divergence/*.oro" > /dev/null; then
  for f in "$DIR"/divergence/*.oro; do
    exp="${f%.oro}.expected"
    [[ -f "$exp" ]] || continue
    # Diagnostics carry the script path; strip the repo prefix so the reviewed
    # .expected files stay portable.
    got=$("$ORO" "$f" 2>&1 | sed "s#$DIR/##g")
    if [[ "$got" == "$(cat "$exp")" ]]; then
      pass=$((pass+1))
    else
      fail=$((fail+1))
      echo "FAIL (divergence) $(basename "$f")"
      diff <(printf '%s\n' "$got") "$exp" | head -12
    fi
  done
fi

echo "----"
echo "pass $pass  fail $fail"

# Known-failing: correct Python that Oro gets wrong today. Reported separately
# and NEVER failing the build — the point is that they stay visible. A case that
# unexpectedly starts passing is flagged (it should be promoted to core/).
if compgen -G "$DIR/known-failing/*.oro" > /dev/null; then
  kf_still=0; kf_now=0
  for f in "$DIR"/known-failing/*.oro; do
    exp="${f%.oro}.expected"
    [[ -f "$exp" ]] || continue
    got=$("$ORO" "$f" 2>&1)
    if [[ "$got" == "$(cat "$exp")" ]]; then
      kf_now=$((kf_now+1))
      echo "known-failing NOW PASSES (promote to core/): $(basename "$f")"
    else
      kf_still=$((kf_still+1))
    fi
  done
  echo "known-failing: $kf_still still broken, $kf_now now passing"
fi

[[ $fail -eq 0 ]]
