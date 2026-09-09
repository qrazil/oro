#!/usr/bin/env bash
# Oro benchmark harness.
#
#   ./bench/run.sh                 # run every benchmark, best-of-3, vs CPython
#   ./bench/run.sh -n 5            # best-of-5
#   ./bench/run.sh fib loop        # only the named benchmarks
#   ./bench/run.sh --no-python     # skip the CPython column
#   ./bench/run.sh --oro path/oro  # measure a different oro binary
#   ./bench/run.sh --md            # emit a Markdown table (for RESULTS.md)
#
# Every program prints its result, and the harness *checks* that oro and CPython
# agree before reporting a time — a benchmark that has been accidentally
# optimised into computing the wrong thing is worse than no benchmark.
#
# Most programs are valid Python as written, so CPython runs the very same
# `.oro` file (the interpreter does not care about the extension). Where a
# program uses an Oro-only spelling it declares an `ORO_ONLY` twin: see
# chain.oro / chain.py.
set -uo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$DIR/.." && pwd)"
ORO="$ROOT/target/release/oro"
PY="${PYTHON:-python3}"
N=3
MD=0
RUN_PY=1
SELECT=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    -n) N="$2"; shift 2 ;;
    --oro) ORO="$2"; shift 2 ;;
    --no-python) RUN_PY=0; shift ;;
    --md) MD=1; shift ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) SELECT+=("$1"); shift ;;
  esac
done

# Benchmarks in report order. A `:py=<file>` suffix names a CPython twin for a
# program CPython cannot parse; without it the .oro file is run under CPython.
BENCHES=(
  "fib"
  "loop"
  "strjoin"
  "dictops"
  "oo"
  "genpipe"
  "exc"
  "listbuild"
  "chain:py=chain.py"
)

if [[ ! -x "$ORO" ]]; then
  echo "no oro binary at $ORO — run: cargo build --release" >&2
  exit 1
fi

# Best-of-N wall time for one command, in seconds, to 3 decimal places.
best_of() {
  local n="$1"; shift
  local best="" t start end
  for _ in $(seq "$n"); do
    start=$(date +%s.%N)
    "$@" > /dev/null 2>&1
    end=$(date +%s.%N)
    t=$(awk -v a="$start" -v b="$end" 'BEGIN { printf "%.3f", b - a }')
    if [[ -z "$best" ]] || awk -v x="$t" -v y="$best" 'BEGIN { exit !(x < y) }'; then
      best="$t"
    fi
  done
  printf '%s' "$best"
}

rows=()
mismatch=0
for entry in "${BENCHES[@]}"; do
  name="${entry%%:*}"
  twin=""
  [[ "$entry" == *":py="* ]] && twin="$DIR/progs/${entry##*:py=}"

  if [[ ${#SELECT[@]} -gt 0 ]]; then
    found=0
    for s in "${SELECT[@]}"; do [[ "$s" == "$name" ]] && found=1; done
    [[ $found -eq 1 ]] || continue
  fi

  src="$DIR/progs/$name.oro"
  pysrc="${twin:-$src}"

  # Correctness first: oro and CPython must agree on the output.
  oro_out="$("$ORO" "$src" 2>&1)"
  note=""
  if [[ $RUN_PY -eq 1 ]]; then
    py_out="$("$PY" "$pysrc" 2>&1)"
    if [[ "$oro_out" != "$py_out" ]]; then
      echo "MISMATCH in $name:" >&2
      diff <(printf '%s\n' "$oro_out") <(printf '%s\n' "$py_out") | head -8 >&2
      mismatch=1
    fi
    [[ -n "$twin" ]] && note="oro-only (py twin)"
  fi

  oro_t=$(best_of "$N" "$ORO" "$src")
  if [[ $RUN_PY -eq 1 ]]; then
    py_t=$(best_of "$N" "$PY" "$pysrc")
    ratio=$(awk -v a="$oro_t" -v b="$py_t" 'BEGIN { if (b > 0) printf "%.2fx", a / b; else printf "-" }')
  else
    py_t="-"; ratio="-"
  fi
  rows+=("$name|$oro_t|$py_t|$ratio|$note")
done

if [[ $MD -eq 1 ]]; then
  echo "| bench | oro | CPython | oro/CPython | note |"
  echo "|---|---|---|---|---|"
  for r in "${rows[@]}"; do
    IFS='|' read -r a b c d e <<< "$r"
    echo "| $a | ${b}s | ${c}s | $d | $e |"
  done
else
  printf '%-12s %10s %10s %10s  %s\n' bench oro cpython ratio note
  printf '%-12s %10s %10s %10s  %s\n' ------------ ---------- ---------- ---------- ----
  for r in "${rows[@]}"; do
    IFS='|' read -r a b c d e <<< "$r"
    printf '%-12s %10s %10s %10s  %s\n' "$a" "${b}s" "${c}s" "$d" "$e"
  done
fi

[[ $mismatch -eq 0 ]]
