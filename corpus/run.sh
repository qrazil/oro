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
echo "----"
echo "pass $pass  fail $fail"
[[ $fail -eq 0 ]]
