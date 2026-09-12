#!/usr/bin/env bash
# Regenerate .expected from CPython for the layer Oro still keeps in step with
# it: the computational core (arithmetic, strings, containers, control flow,
# sorting). Those core/, later/, and known-failing/ scripts are Python 3.6+ up to
# the spelling of three literals (below), so CPython is ground truth (the
# known-failing ones are CORRECT Python that Oro gets wrong).
#
# divergence/ is excluded: those programs deliberately behave differently, or use
# Oro-only spellings (`proc`, `=>`, `.map`, the `to_` casts) that CPython cannot
# run at all. They are checked by run.sh against reviewed .expected files.
#
# --- The two translations ---------------------------------------------------
#
# Oro spells the three literals `true` / `false` / `null` where Python spells
# them `True` / `False` / `None`. That is a rename, not a semantic change, so
# the oracle bridges it in both directions:
#
#   in   the program's `true`/`false`/`null` become Python's spellings before
#        CPython sees the source, so a program with a bool in it still runs;
#   out  CPython's `True`/`False`/`None` become Oro's before the output is
#        written as the expectation, so `print(1 < 2)` expects `true`.
#
# Both mappings are total, mechanical and 1:1, so unlike a hand-reviewed
# baseline they add no blind spot. 13 of the core programs print a bool or a
# null somewhere; without this they would lose their oracle for what they
# actually test (truthiness, `dict.get()` misses, `any`/`all`) merely because a
# bool reached stdout.
#
# The *inbound* translation is exact: it runs over CPython's own tokenizer and
# rewrites only NAME tokens, so `"true"` inside a string is untouched.
#
# The *outbound* one cannot be, because it only has the bytes. It rewrites at
# word boundaries, which is enough to leave `NoneType` — a type name, and part
# of CPython's own messages — alone.
#
# --- The hazard, and the guard ----------------------------------------------
#
# A program that printed the *string* "True" would be silently mistranslated:
# CPython writes `True`, the outbound rule rewrites it to `true`, and Oro writes
# `True` — a mismatch with no bug behind it. The output cannot distinguish the
# two cases, because `print(True)` and `print("True")` produce the same five
# bytes. Quote-awareness does not save it either: it would fix `repr("True")`,
# which shows `'True'`, and in exchange would start *skipping* real bools inside
# repr'd containers (`[True, None]`), which is the common case.
#
# So the rule is enforced at the source instead. Every program is tokenised and
# refused if it puts True/False/None inside a string literal. The cost is real
# but narrow: an oracled program cannot print those three words as data. Write
# them lowercase, build the word (`"Tr" + "ue"`), or move the program to
# divergence/, where the baseline is reviewed by hand.
set -uo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
shopt -s nullglob
status=0
tmp="$(mktemp -t oro-oracle-XXXXXX.py)"
trap 'rm -f "$tmp"' EXIT

for f in "$DIR"/core/*.oro "$DIR"/later/*.oro "$DIR"/known-failing/*.oro; do
  if ! python3 - "$f" > "$tmp" <<'PY'
import io, re, sys, token, tokenize

path = sys.argv[1]
src = open(path, encoding="utf-8").read()
# Oro's spelling -> Python's. Only NAME tokens, so string contents are safe.
INBOUND = {"true": "True", "false": "False", "null": "None"}
BANNED = re.compile(r"\b(True|False|None)\b")
STRINGS = {token.STRING, getattr(token, "FSTRING_MIDDLE", -1)}

try:
    toks = list(tokenize.generate_tokens(io.StringIO(src).readline))
except Exception as e:  # not tokenisable: let CPython report it on the real run
    sys.stdout.write(src)
    sys.exit(0)

for t in toks:
    if t.type in STRINGS and BANNED.search(t.string):
        sys.stderr.write(f"line {t.start[0]}: {t.string[:60]}\n")
        sys.exit(2)

lines = src.splitlines(keepends=True)
edits = [
    (t.start[0], t.start[1], t.end[1], INBOUND[t.string])
    for t in toks
    if t.type == token.NAME and t.string in INBOUND
]
for ln, c0, c1, new in sorted(edits, reverse=True):
    line = lines[ln - 1]
    lines[ln - 1] = line[:c0] + new + line[c1:]
# Oro's `for` binds an (index, value) pair for every iterable; CPython's binds
# the element. `enumerate(E)` yields exactly Oro's pair for every non-dict
# iterable — position and element for a sequence or range, a 0-based counter for
# a generator — and no core program iterates a dict (that is a divergence). So
# wrap each for-header's iterable, and the two languages agree line for line.
FOR = re.compile(r'^(\s*)for (.+) in (.+):(\s*)$')
lines = [FOR.sub(r'\1for \2 in enumerate(\3):\4', ln) for ln in lines]
sys.stdout.write("".join(lines))
PY
  then
    echo "ORACLE UNSAFE $(basename "$f") — True/False/None inside a string literal"
    echo "  the outbound True->true rule cannot tell that from a printed bool; see the header"
    status=1
    continue
  fi

  out=$(python3 -c "
import sys
sys.setrecursionlimit(20000)
exec(open('$tmp').read())
" 2>&1)
  if [[ $? -ne 0 ]]; then echo "ORACLE FAIL $(basename "$f")"; echo "$out" | tail -3; status=1
  else
    printf '%s\n' "$out" \
      | sed -E 's/\bTrue\b/true/g; s/\bFalse\b/false/g; s/\bNone\b/null/g' \
      > "${f%.oro}.expected"
    echo "ok $(basename "$f")"
  fi
done
exit $status
