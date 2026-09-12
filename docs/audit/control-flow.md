# Control flow: one situation, one spelling

*An audit of Oro's loops, conditionals, early exits and the block scope that
shapes all three. Analysis only — nothing here has been changed.*

The earlier audit (`docs/one-way-audit.md`) asked whether two constructs were
redundant, and cut the ones that were. This asks a sharper question, and it does
not lead to cuts:

> Several constructs legitimately coexist. `for x in xs`, `for i in range(n)` and
> `while cond` are all needed — a condition is not a sequence. But **for any
> given situation, one of them is right.** Find the situations, name the right
> spelling for each, and check whether the tree actually uses it.

The standard is the owner's: *all code written for a given task by any developer
or AI should look the same.* Where two competent people would write the same job
differently, that is a finding — even when both spellings deserve to exist. The
output of this kind of audit is therefore mostly **rules**, not removals. Nine
of the twelve situations below end in "the tree is already right, write it
down"; three end in a named disagreement with a file and a line.

Everything here was checked by running it against `./target/release/oro` at
0.2.0. Every rewrite proposed is in this document only after being executed
side-by-side with the original and producing identical output. Line numbers are
as of `f84d654`; `std/` and `examples/` are being edited concurrently, so treat
them as locators rather than coordinates.

Ground already settled elsewhere is not re-argued here: `for i in range(n)` over
manual counters (§7b of the first audit, being applied now), `.enumerate()` over
the three index spellings (§7c), dict iteration yielding pairs (§7d), `for`
versus chains as a laziness split (§9), keeping truthiness (§10), `finally`
being earned (§13). Where this document touches those, it is to state the case
they do not cover.

---

## 0. The situations, ranked

| # | Situation | The right spelling | Tree agrees? | Confidence |
|---|---|---|---|---|
| 1 | A `try` whose failure ends the iteration | act in the `except` clause | **no** — `std/http.oro:1387` | high |
| 2 | Walk two sequences in lockstep | `for a, b in xs.zip(ys)` | **no** — `std/http.oro:1566` | high |
| 3 | A value, or a default when it is absent | `== null`, explicitly | yes, unanimously — unwritten | high |
| 4 | Dispatch on one of N constant values | dict for a value, `match` for statements | no site exists | medium |
| 5 | Skip an element that does not qualify | `continue`, once there are two guards | **split** — `1387` vs `1537` | medium |
| 6 | Read a stream until it stops giving bytes | prime, then repeat at the foot | yes, 7–0 | medium |
| 7 | Carry a value out of a block | pre-bind, and only a value | yes, 11–1 (same site as §1) | high |
| 8 | Leave a function early | guard clause, `return` where you know | yes | high |
| 9 | Choose between two values | pre-bind the default, then `if` | yes | high |
| 10 | Stop a loop from inside | `return` if you can, `break` if you cannot | yes | high |
| 11 | Walk a sequence backwards | `xs.reversed()` | no evidence either way | low |
| 12 | Count how many times something happened | `while`, counter outside | yes | high |

§§1–5 are the findings. §§6–12 are settlements: questions closed so they do not
get reopened at the freeze.

---

## 1. A `try` whose failure ends the iteration

**The situation.** Inside a loop, an operation may fail, and what the failure
means is "stop" or "go round again" rather than a value.

**The spellings.** Both work. `break`, `continue`, `return` and `raise` all
escape a `try` body or an `except` clause correctly, and `finally` still runs on
the way out — verified on all four.

```python
# (a) act in the clause
while true:
    conn = null
    try:
        conn = ln.accept()
    except ValueError as e:
        break
    except ConnectionError as e:
        continue

# (b) set a flag, test it after the block
while true:
    conn = null
    closed = false
    try:
        conn = ln.accept()
    except ValueError as e:
        closed = true
    except ConnectionError as e:
        conn = null
    if closed:
        break
    if conn == null:
        continue
```

**Which is right: (a).** An `except` clause exists to say what happens when that
exception arrives; a flag defers the answer to a test ten lines below, where the
reader has to hold "what does `closed` mean" in their head to get there.

**The tree.** Twelve `try` blocks in `std/` and `examples/`. Nine take form (a)
and carry nothing. Two pre-bind a **value** that is genuinely read after the
block and could not be written any other way — `copied` in
`examples/client.oro:98` and `handed_over` in `std/http.oro:2287`, the latter
read from a `finally`, which by construction runs on every path and so cannot be
replaced by a `break` in any clause. Both are correct; §7 states the rule that
covers them.

One site takes form (b): **`std/http.oro:1387`, the accept loop.** It pre-binds
`closed` and `failed`, sets them in two `except` clauses, and tests them below.
It is also the only `break` in the entire standard library.

**This is the place the tree most disagrees with itself**, because the rule form
(a) obeys is not merely implicit — `std/http.oro` states it three separate
times, in three comments, about this exact mechanism:

```
:774   # ...because a `try` body has its own scope: a name first bound inside
       # one does not survive it, so the shape that wants no temporary is to
       # return from inside the `try`.
:910   # `if/for/while/try` bodies have real block scope... Returning from
       # inside the `try` is the shape that wants no temporary at all.
:2136  # ...a `try` body has its own scope, so the shape that wants no
       # temporary is to return from inside it
```

Directly below the accept loop, `_conn_task` (`:1446`) does it the documented
way: three `except` clauses, each doing its own work, no flags.

**The rewrite**, run against the original on six scripted accept sequences
(clean, peer-vanished, transient error, error budget exhausted, immediate close,
mixed) and identical on all six including the error-counter reset:

```python
while true:
    # Bound before the `try`: a name first bound in a try body does not
    # survive the block.
    conn = null
    try:
        conn = ln.accept()
    except ValueError as e:
        break
    except ConnectionError as e:
        # A client that went away between the SYN and the accept.
        continue
    except OSError as e:
        errors = errors + 1
        if errors > _MAX_ACCEPT_ERRORS:
            ln.close()
            _drain_live(live, tally, drain)
            raise e
        time.sleep(backoff)
        backoff = min(backoff * 2, _ACCEPT_BACKOFF_MAX)
        continue
    errors = 0
    backoff = _ACCEPT_BACKOFF
    ...
```

Two pre-bindings go, and with them `if closed: break`, `if failed != null:` and
`if conn == null: continue` — the last of which existed only to service the
`ConnectionError` arm and becomes dead. Six lines shorter, and each failure
mode's answer is written where the failure is caught. `conn = null` stays, and
the comment explaining why stays with it: `conn` is read after the block, so
block scope requires it.

**Verdict: act in the clause. A flag before a `try` is for a value that outlives
the block, never for a control decision.** The one site that does otherwise
should move, and `std/http.oro` already contains the sentence that says so.

---

## 2. Walk two sequences in lockstep

**The situation.** Two sequences of the same length, and element *i* of one is
needed alongside element *i* of the other.

**The spellings.**

```python
# (a) manual index
i = 0
while i < len(pat):
    p = pat[i]
    ... segs[i] ...
    i = i + 1

# (b) index over a range
for i in range(len(pat)):
    p = pat[i]
    ... segs[i] ...

# (c) zip
for p, s in pat.zip(segs):
    ...
```

**Which is right: (c).** The loop is over pairs, so the loop variable should be
a pair; the index in (a) and (b) is a mechanism, not part of the problem.

**The tree.** `.zip()` is used **zero times** in `std/`, `examples/` and
`bench/`, and twelve times in `corpus/`, all of them in the files that test the
collection protocol. The one lockstep loop in the standard library —
`_match` at `std/http.oro:1566` — uses form (a).

**Why this matters right now, more than its size suggests.** The §7b rule is
being applied to the tree as this is written: *a loop over a known count is `for
… in range(…)`*. `_match` is a `while i < len(pat)` and will look exactly like a
site that rule covers. Applying it mechanically produces form (b) — which §7c
measured at **zero** uses in the tree and identified as the spelling nobody
wants. The rewrite would replace a bad spelling with a different bad spelling
and make `.zip()`'s absence from the standard library permanent at the freeze.

**The rewrite**, run against the original on five route-matching cases (a bound
parameter, an empty segment, a literal mismatch, an exact match, and two empty
lists) and identical on all five:

```python
def _match(pat, segs):
    if len(pat) != len(segs):
        return null
    params = {}
    for p, s in pat.zip(segs):
        if len(p) > 0 and p[0] == ":":
            if len(s) == 0:
                return null
            params[p[1:]] = s
        elif p != s:
            return null
    return params
```

Three lines shorter, `segs[i]` written once instead of three times, and the
length guard above it is now what makes the `zip` truncation harmless rather
than something a reader has to notice.

**The ergonomics test.** This *is* the real call site — `_match` is on the
request path of every parameterised route in the language. It gets shorter and
the index arithmetic disappears rather than moving.

**Verdict: `for a, b in xs.zip(ys)`, and `_match` should be rewritten to it
rather than to `range(len(pat))`.** Worth saying before the §7b sweep reaches
that line.

---

## 3. A value, or a default when it is absent

**The situation.** A value may be missing, and something has to stand in for it.
This is the most common conditional in the tree.

**The spellings.** Four, and they are not interchangeable:

```python
x = maybe or default              # (a) falsy stands in
if maybe == null:                 # (b) null stands in
    maybe = default
x = d.get(k, default)             # (c) a missing key stands in
def f(headers=null):              # (d) an omitted argument stands in
```

**Which is right: (b), (c) and (d), each for its own question — and never (a).**
(c) is the spelling when the source is a dict, (d) when the source is the call
site, (b) otherwise. (a) is the odd one out because it answers a *different*
question: it substitutes for anything falsy, and in Oro `0`, `""`, `b""`, `[]`
and `{}` are all falsy and all legitimate values.

**The tree.** This is a case of unanimity that nobody has written down. Across
`std/`:

| spelling | uses |
|---|---|
| `== null` / `!= null` | 43 |
| `len(x) == 0` / `> 0` | 15 |
| `== b""` / `!= b""` / `== ""` | 12 |
| `.get(k, default)` | present, e.g. `_REASONS.get(status, "Unknown")` |
| `x or default` | **0** |

All 17 `or` operators in `std/`, across 15 lines, combine two comparisons that
already answer `true` or `false`. Not once does the standard library use `or` to
supply a fallback, and not once does it test a list, string or bytes value for
truthiness rather than for length or emptiness. Only eight conditionals in all
of `std/` test a bare name for truthiness (seven `if`, one `elif`), and all
eight are on a `bool` — `self.done`, `streamed`, `suppress`, `chunked`, `keep`,
`closed`.

This is worth stating plainly because the first audit's §10 kept truthiness
partly on the strength of this idiom: *"`and`/`or` return an operand rather than
a bool, which makes `a or "default"` the language's idiom for a fallback."* It
is the language's available idiom; it is not the tree's, anywhere, at all. That
does not reopen the cut — §10's second reason (truthiness is in the
CPython-oracled computational core, and diverging would be gratuitous) stands on
its own and is sufficient. But the *style* question it left open has an answer,
and the answer is the one the standard library has been giving silently for
2,400 lines.

**The reason the tree is right**, and it is Oro-specific rather than taste: Oro
chose sentinel returns (§8 of the first audit). `s.find(x)` answers `-1`, which
is truthy, and `0`, which is falsy — so `if s.find(x):` is wrong at both ends.
`d.get(k)` cannot tell a missing key from a key holding `0` or `""`. In a
language whose lookups answer with values rather than raising, truthiness is
sharper than it is in Python, and the standard library has been avoiding the
edge without ever saying so.

**Verdict: absence is `== null`, emptiness is `len(x) == 0`, and `or` combines
booleans.** Truthiness stays in the language, as §10 decided; this is the rule
for when to use it, which is: on a `bool`, and there. One paragraph in the
README next to the io protocol's EOF note.

---

## 4. Dispatch on one of N constant values

**The situation.** A value is one of a known, fixed set, and each member means
something different.

**The spellings.**

```python
match status:               # (a)
    case 200:
        ...
    case 404:
        ...

if status == 200:           # (b)
    ...
elif status == 404:
    ...

_REASONS = {200: "OK", 404: "Not Found"}    # (c)
_REASONS.get(status, "Unknown")
```

**Which is right.** These split cleanly by what the cases *produce*. If every
case produces a **value**, it is a dict (c) — a table is data, and writing data
as code is how a 60-entry status table becomes a 180-line function. If cases run
**statements**, it is `match` (a), which the README justifies on an O(1) jump
table rather than on syntax. `if`/`elif` (b) is for **predicates** — ranges,
compound tests, anything that is not equality against a constant.

**The tree.** `match` appears **six times in the whole repository**, in two
files, and both files are corpus programs whose job is to test `match`:
`corpus/core/17_match.oro` (five) and `corpus/core/41_numeric_literals.oro`
(one). There are **zero uses in `std/`, `examples/`, `bench/` and `tests/`.**

The honest reading is not that the standard library ignored `match`. It is that
**the standard library has no constant dispatch to statements at all.** I checked
every `elif` in `std/` — there are six — and none is an equality ladder over
constants:

- `:666` `elif name in _SINGLE_VALUED:` — a membership test
- `:683` `elif target.startswith(b"https://"):` — a prefix test
- `:1057` `elif streamed:` — a bool
- `:1068` `elif req.version == "1.0":` — one comparison, no ladder
- `:1573` `elif p != segs[i]:` — inequality
- `:2005` `elif len(body) > 0:` — a length test

All six are predicates, and all six are correctly `if`/`elif`. The one genuine
constant-to-value dispatch in the language, `_REASONS` at `:267`, is correctly a
dict. `_is_bodiless` (`:1011`) is a range test and correctly an `if`.

**Verdict: the rule above is right and the tree obeys it. `match`'s problem is
not misuse, it is that it has no user.** That sharpens rather than settles the
first audit's §10: `match` is kept on a performance claim that is not merely
unmeasured — it is *unexercised*, by every line of Oro in this repository that
was written to do a job rather than to test a feature. A construct with no
load-bearing call site and one contingent justification is the weakest thing in
the control-flow surface going into a freeze.

I am not recommending a cut, and I want to be clear why not: `match` costs
nothing to keep, the jump table is a real complexity-class argument rather than
a syntactic one, and the absence of a call site in a 2,400-line HTTP module is
weak evidence about the programs users will write — a state machine, a
tokeniser, or an opcode dispatcher is exactly where it would land and this
repository contains none in Oro. But the benchmark §10 asked for should exist
before 1.0, because it is the whole case, and the third spelling (c) should be
written down alongside it: **a constant-to-value map is a dict, not a `match`
whose every arm is a `return`.** That is the mistake `match` invites, and the
one `_REASONS` avoided.

*Confidence: medium.* The rule is solid. The judgment that `match` should
survive on zero users rests on programs that do not exist yet.

---

## 5. Skip an element that does not qualify

**The situation.** Inside a loop, some elements are not interesting.

**The spellings.**

```python
for x in xs:            # (a) invert and continue
    if not wanted(x):
        continue
    ... body ...

for x in xs:            # (b) nest the body
    if wanted(x):
        ... body ...
```

**Which is right.** (b) for a one-line body, (a) once there are two guards or the
body is more than a line or two. The line is not arbitrary: with one guard and
one line, (b) reads as a filter and (a) adds a negation the reader has to undo.
With two guards, (b) costs two indent levels of the real work and (a) costs
nothing.

A one-guard, one-line loop body has a third answer that supersedes both, and the
first audit's §9 already gave it: *"if the loop body is a single append of an
expression, it is a chain written longhand."* So the situation this section
covers is specifically the **multi-guard** loop.

**The tree is split, in one file, between two adjacent methods.**

`std/http.oro:1387`, the accept loop, is the model: four guards, each an
`if <bad>:` that rejects and goes round (or, once, out), and the happy path
stays at one indent level throughout a forty-five-line loop. It is the best control-flow writing in the language.

`std/http.oro:1537`, `Router.handler_for`, is the same situation written the
other way and is the **deepest code in the standard library at five levels**:

```python
for m, pat, fn in self.patterns:
    if m == method:
        params = _match(pat, segs)
        if params != null:
            req.params = params
            return fn
```

Two guards, nested. Flattened:

```python
for m, pat, fn in self.patterns:
    if m != method:
        continue
    params = _match(pat, segs)
    if params == null:
        continue
    req.params = params
    return fn
```

Same length, two levels shallower, and the two rejections read as rejections.
Its neighbour `Router.dispatch` (`:1514`) already writes the same shape as a
guard cascade at function level — `if h != null: return h(req)`, three times —
so the file contains the flat style at function scope and the nested style at
loop scope, in two methods printed one after the other.

The other three single-`if` loop bodies in `std/` (`:575`, `:820`, `:1552`) are
all one-line and all correctly nested — and all three are separately the
chain-written-longhand case §9 flagged, so they will change for a different
reason.

**Verdict: `continue` once there are two guards; nest a single one-line filter,
or make it a chain.** One site to move.

*Confidence: medium.* The principle is not in dispute; where exactly the line
falls between one guard and two is a judgment, and I would not rewrite a
one-guard loop on the strength of it.

---

## 6. Read a stream until it stops giving bytes

**The situation.** The most common loop in systems programming, and the one Oro
has no first-class construct for.

**The spellings.**

```python
# (a) prime, then repeat at the foot
chunk = r.read(_CHUNK)
while chunk != b"":
    parts.append(chunk)
    chunk = r.read(_CHUNK)

# (b) fetch in the body, test with an if
while true:
    chunk = r.read(_CHUNK)
    if chunk == b"":
        break
    parts.append(chunk)
```

There is no third. Python's answer to this exact shape is the walrus —
`while (chunk := r.read(n)) != b"":` — and Oro cut the walrus deliberately and
correctly ("a second assignment operator that also returns a value is precisely
the 'more than one way' the thesis rejects"). **The duplicated fetch in (a) is
the bill for that cut**, and it is worth naming as a bill rather than pretending
it is free. Block scope makes it structural, not stylistic: the loop condition is
evaluated in the enclosing scope, so `chunk` must be bound there, so it must be
fetched there.

**Which is right: (a).** The termination condition belongs in the loop header,
because "when does this stop reading" is the question a reviewer of a network
library is actually asking, and (b) moves the answer into the body. The cost is
one repeated expression; the benefit is that the header is true.

**The tree agrees, 7–0.** `std/io.oro:77` (`io.read`), `:106` (`io.copy`),
`std/http.oro:1103` (`_write_chunked`), `:1147` (`_drain_loop`), `:2381`
(`_read_capped`), `:924` (`skip_trailer`), and `:555` (`_escape_fault`, the same
shape over `b.find`). I checked every one for the failure mode the duplication
invites — a primed fetch that does not match the repeated one — and **none of
them is wrong.** `skip_trailer` writes `self.r.read_until(b"\r\n",
_MAX_TRAILER_LINE)` twice, identically; `_drain_loop` writes `body.read(left)`
twice with `left` correctly decremented in between.

**The one apparent counter-example is not one.** `io.read`'s two branches drain a
stream two different ways, eight lines apart:

```python
# std/io.oro:77 — no n: prime and repeat
chunk = r.read(_CHUNK)
while chunk != b"":
    ...
# std/io.oro:86 — with n: fetch in the body
while got < n:
    chunk = r.read(n - got)
    if chunk == b"":
        raise EOFError(...)
```

That looked like this section's `join` moment and it is not, because the two
loops have different exit conditions. The second is bounded by a **count the
loop already knows**, not by a sentinel it has to fetch, so its header is
already true and the sentinel is a genuine exception case inside it. The rule
that covers both:

> When the loop's exit is a value the loop itself fetches, prime the fetch before
> the loop and repeat it at the foot, so the condition names the sentinel. When
> the exit is a count or a deadline the loop already holds, put the fetch in the
> body and let a sentinel be an `if`.

**Verdict: the tree is right; write the rule down.** I ran (b) against (a) for
`io.copy` on five inputs (empty, one byte, exact multiple, partial reads, single
read) — identical results and identical read counts, so there is no syscall
argument either way and this is purely about where the reader looks.

*Confidence: medium, and this is the one verdict in the document I hold most
loosely.* A competent reviewer could reasonably prefer (b) on the strength of
"the advancing call is written once", which is a real correctness argument even
though no site in this tree has fallen to it. What decides it for me is that the
tree is already unanimous and none of the seven is wrong: a 7–0 idiom that has
never produced a bug is worth more than the marginal safety of changing all
seven. If a future site *does* get the two fetches out of step, that is the
evidence that flips this, and it should flip it.

---

## 7. Carry a value out of a block

**The situation.** Oro has block scope on `if`, `for`, `while` and `try`, and no
declaration syntax — a name is created by assigning to it. So a value computed
inside a block and needed outside it has to be arranged for in advance.

The exact mechanics, verified:

```python
try:
    v = 1
except ValueError as e:
    pass
print(v)            # NameError — the try body's binding did not survive

v = null            # pre-bound in the enclosing scope
try:
    v = 1
except ValueError as e:
    pass
print(v)            # 1 — the inner assignment rebound the outer name

for i in range(3):
    pass
print(i)            # NameError

i = 0
while i < 3:
    i = i + 1
print(i)            # 3 — `i` was the enclosing scope's all along

try:
    raise ValueError("x")
except ValueError as e:
    pass
print(e)            # NameError — `e` does not survive its clause either
```

**The spellings.** Pre-bind to `null`, pre-bind to the failure value, or
restructure so nothing crosses the boundary — which usually means returning from
inside the block, and usually means a two-line function.

**Which is right: restructure first, pre-bind only a value.** `std/http.oro`
argues this itself, three times, and `_read_head` (`:778`), `terminator`
(`:913`), `_drain` (`:1135`), `_try_read` (`:1226`), `_dispatch` (`:1241`) and
`_read_response_head` (`:2141`) are all functions that exist for no other reason
than to turn a carried value into a `return`. `_read_head` and
`_read_response_head` are four duplicated lines and the comment at `:2136` says
outright that the duplication is worth its cost. It is: the alternative is a
temporary and a flag.

Where a value genuinely must cross — `copied` in `examples/client.oro:98`,
`handed_over` in `std/http.oro:2287`, `conn` in the accept loop — pre-bind it,
and heed the README's warning to choose the placeholder so the wrong path is
loud.

**The tree agrees, 11–1**, and the one exception is §1's: the accept loop
pre-binds two control flags alongside one real value.

**One thing worth stating that the README does not.** The `null` placeholder is
at its most dangerous in the `if`/`else` case, where it is *dead code*:

```python
x = null            # never read — both branches assign
if c:
    x = 1
else:
    x = 2
```

Here the placeholder exists purely to create the name, contributes nothing, and
silently becomes the answer if someone later adds a third branch that does not
assign. §9 has the better spelling.

**Verdict: return from inside the block where you can; pre-bind a value where
you cannot; never pre-bind a control decision.**

---

## 8. Leave a function early

**The situation.** A function has preconditions, or several candidate answers
tried in order.

**The spellings.** A guard cascade of `if … return` at the top, versus a single
exit with the body nested under the negation of each guard, versus a `result`
variable assigned and returned at the end.

**Which is right: guard clauses, returning as soon as the answer is known.** The
single-exit style is a C idiom that exists because C has no destructors; Oro has
deterministic release at end of scope and `finally` for ordering, so there is
nothing a single exit buys. The `result` variable is worse still here, because
block scope means it has to be pre-bound, which is §7's dead placeholder again.

**The tree agrees, and does so emphatically.** `std/` contains **one `break`**
and four `continue`s across 16 `for` loops and 20 `while` loops, against 156
`return`s. Loops end by
returning from the function they are in. `_has_token` (`:574`) returns `true`
from inside its loop; `handler_for` (`:1532`) returns `fn`; `Router.dispatch`
(`:1514`) is a four-step guard cascade with four `return`s and no nesting at all;
`write_response` (`:1047`) and `write_request` (`:1995`) both `return` from the
middle to skip the streaming tail.

Nothing needs to change. It is worth recording because a reader coming from a
single-exit culture will see 11 `return`s in a 40-line function and think it is
accidental.

**Verdict: guard clauses, and `return` at the point the answer is known.
Settled.**

---

## 9. Choose between two values

**The situation.** One of two values, depending on a condition. This is the
situation a ternary would cover, and **Oro has no conditional expression** —
`y = "a" if x else "b"` is a parse error. That is consistent with the thesis (a
ternary is a second spelling of `if`) and it means the job has to be done with
statements.

**The spellings.**

```python
x = null            # (a) pre-bind a placeholder, assign in both arms
if c:
    x = 1
else:
    x = 2

x = 2               # (b) pre-bind the default, override in one arm
if c:
    x = 1

def pick(c):        # (c) a function with a guard
    if c:
        return 1
    return 2
```

**Which is right: (b), and (c) when the choice is worth a name.** (a) is (b) with
a dead line in front of it — the `null` is never read, and it is the one thing
standing between a later third branch and a `NameError` that would have caught
it. (b) states the default as the default, which is also the order the
`_request_headers` / `default=` idiom uses everywhere else in the language.

**The tree agrees.** `write_response` (`std/http.oro:1053`) is the model:

```python
suppress = req.method == "HEAD"
chunked = false
if _is_bodiless(resp.status):
    suppress = true
elif streamed:
    if req.version != "1.0":
        chunked = true
        h["transfer-encoding"] = "chunked"
else:
    h["content-length"] = f"{len(body)}"
```

Both flags are given their common-case value first and overridden by exception.
I found no instance of form (a) in `std/`.

**Verdict: pre-bind the default, override in the exception. Settled — and worth
a README line, because the absence of a ternary is the kind of thing a newcomer
discovers by writing one.**

---

## 10. Stop a loop from inside

**The situation.** The loop has found what it came for, or has to abandon.

**The spellings.** `return` (if the loop is the function's whole job), `break`
(if there is work after the loop), or a flag tested in the loop condition.

**Which is right: `return` if you can, `break` if you cannot, and never a flag.**
A flag in the condition means the loop runs one more time after the decision is
made, which is either a bug or a subtlety the reader has to verify.

**The tree agrees**, to the point of 1 `break` in the whole standard library. The
flag-in-condition idiom appears nowhere. The one `break` (`std/http.oro:1405`)
is in the accept loop, which has a drain and a `return tally` after it and so
genuinely cannot `return` — and under §1's rewrite it becomes a `break` in the
`except` clause, which is still the right construct in the right place.

**Verdict: settled.** Recorded because "there is only one `break` in `std/`"
reads like a gap and is not one — it is what a codebase looks like when loops
are small enough to be functions.

---

## 11. Walk a sequence backwards

**The situation.** Iterate from the end.

**The spellings.** All three work:

```python
for x in xs.reversed():             # (a) collection protocol, type-preserving
for x in xs[::-1]:                  # (b) a slice with a negative step
for i in range(len(xs) - 1, -1, -1) # (c) a descending range of indices
```

**Which is right: (a).** It says what it means, it is the protocol's own
spelling, and it preserves the receiver's type per the stated rule. (b) builds
the same list by a route that reads as punctuation. (c) is `range(len(xs))` with
two extra arguments to get wrong, and §7c already established that
`range(len(xs))` is the spelling nobody wants.

**The tree.** Nothing in `std/`, `examples/` or `bench/` iterates backwards at
all, so there is no usage to measure. `reversed()` and `[::-1]` both appear only
in corpus programs testing them.

**Verdict: `xs.reversed()`, on principle rather than on evidence.**

*Confidence: low* — not because the answer is doubtful but because no call site
in the tree exercises it, so this is a rule written ahead of its first user.

---

## 12. Count how many times something happened

**The situation.** A loop whose exit condition is not a count, but which has to
report how many iterations ran.

**Why this needs its own entry.** The §7b rule now being applied — *a loop over a
known count is `for … in range(…)`* — is right, and the sweep it licenses will
pass over loops that look like manual counters and are not. Under block scope,
`for i in range(n)` does **not** leak `i`, so any loop whose counter is read
after it cannot be mechanically converted.

**The right spelling** is the one the tree uses: a counter bound in the enclosing
scope, incremented in the body, with a `while` whose condition is the real exit
test.

```python
served = 0
while max_requests == null or served < max_requests:
    ...
    served = served + 1
return served
```

**The sites this protects**, all correct as they stand and none of them the §7b
anti-pattern: `std/http.oro:1192` (`served`, which is both the driver and the
return value), `:2110` (`interim`, a guard count on 1xx responses), `:636`
(`count`, a header-count guard), `std/io.oro:86` (`got`, bytes accumulated), and
`std/http.oro:947` (`size`, a hex accumulator in a `for`).

**The one genuine carve-out I found** is `bench/progs/strops.oro:7`, which prints
its counter after the loop (`print(hits, n)`). A `for … in range(200000)` there
would leave `n` unbound, so this site needs a hand-written answer rather than a
mechanical one.

**Verdict: settled — and the §7b sweep should test "is the counter read after the
loop" before converting.** I expected this list to be long and it is one entry;
see the closing note.

---

## 13. What is earned, and why — closed before the freeze

Recorded so these do not get reopened.

**`for`, `while`, and chains are three constructs, not three spellings.** The
first audit settled the first two (a condition is not a sequence) and the third
(`for` is lazy, a chain is eager). I re-verified the laziness split, because the
rule in §6 and §12 leans on it: `for x in forever(): … break` terminates, and
`upto(5).map(f)` returns a finished list. They are the two halves of a genuine
split.

**No ternary, no loop-`else`, no walrus, no `do`/`while`.** All four are absent,
all four were cut or never added for the same reason, and all four push work into
statement form. `for … else` and `while … else` are parse errors, which is
right: Python's loop-`else` is the language's most reliably misread construct and
its job — "the loop finished without breaking" — is a `return` in a function or
a flag nobody needs, since §10 shows loops here are small enough to be functions.

**`break`/`continue` inside `try`, and `finally` ordering.** Verified: `break`,
`continue` and `return` all leave a `try` correctly and `finally` runs. This is
what makes §1's rewrite legal and is worth a corpus line if it does not have one
— `corpus/core/25_break_continue_finally.oro` covers it, which is the right file.

**`in` versus `find(x) >= 0`.** §12 of the first audit reported this and it
stands: `find` answers "where", `in` answers "whether", and a `find` compared
only against zero is `in` written longhand. Not re-litigated here.

**`pass` as an `except` body.** `std/http.oro:1449` catches `OSError` and passes,
with three comment lines explaining that a reset peer is how connections end.
There is no second spelling — a bare `pass` with a reason above it is the whole
construct — and it is used correctly at its one site.

---

## 14. Confidence, and what I went looking for and did not find

**Where I am confident:** §1 (the accept-loop flags), §2 (lockstep), §3 (absence
is `== null`), §7, §8, §9, §10, §12. Each is either a measured unanimity or a
single named exception to one, and the two rewrites were run rather than
reasoned.

**Where I am not:** §4 rests on a judgment that `match` survives with zero users,
which is a bet on programs that do not exist in this repository. §5's boundary
between one guard and two is taste. §6 is the verdict I hold most loosely, and I
have said in the section what evidence would flip it. §11 has no evidence at all.

**Three things I expected to find and did not.**

*A flag-driven loop.* The brief asks about `break` versus a flag variable, and I
expected the `found = false … found = true … break` shape somewhere in 10,350
lines. It is not there. The one flag-versus-control case in the tree is §1's,
and it is about a `try`, not a loop condition. This is a codebase that has
already decided.

*Counters whose survival is load-bearing.* I built an analysis to find every
`while i < n` whose `i` is read after the loop, expecting a meaningful carve-out
list for the §7b sweep, and got one real site out of 67 (`bench/progs/strops.oro`
— the others are the same name reused by the next loop, which the rewrite fixes
rather than breaks). The §7b rewrite is safer than it looked.

*Truthiness misuse.* §10 of the first audit warned that truthiness is sharper in
Oro than in Python because of sentinel returns — `if s.find(x):` is wrong at both
ends. I went looking for a site that had fallen into it. There is not one. All
eight bare truthiness tests in `std/` are on a `bool`. The danger is real
and the tree has never once stepped on it.

**And one thing I found that I did not expect.** The strongest finding here
(§1) is not a place where Oro's design is ambiguous. It is a place where
`std/http.oro` writes down the correct rule three separate times, in three
comments about block scope, and then breaks it once, forty lines from a function
that follows it. That is the shape a style rule takes when it lives in comments
instead of in the README: it is known, it is argued, and it is not applied.
Every rule in this document is one paragraph of README away from being
enforceable by a reader, and none of them is enforceable by `oro fmt` — which is
a formatter, not a linter, and under "no options, one output" will never grow a
rule. The rewrite is the enforcement, and the README is the record.

---

## 15. If only three things happen

1. **Move the accept loop's two control flags into their `except` clauses**
   (§1). The file already contains the argument, three times. Six lines shorter,
   verified identical on six scripted sequences, and it ends the one place the
   standard library contradicts its own written rule.
2. **Rewrite `_match` with `.zip()` before the §7b sweep reaches it** (§2).
   Otherwise a correct rule produces `for i in range(len(pat))` — the spelling
   §7c measured at zero uses — and freezes `.zip()` out of the standard library.
3. **Write down the three unwritten unanimities** (§3, §6, §9): absence is
   `== null` and `or` combines booleans; a sentinel-driven loop primes its fetch
   and a count-driven loop does not; and with no ternary in the language, a
   two-way choice pre-binds its default rather than a `null`. Each is already
   what the tree does, none of them is written anywhere, and a frozen language
   with an unwritten dominant idiom has two spellings whether it admits it or
   not.
