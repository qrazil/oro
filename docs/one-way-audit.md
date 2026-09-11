# One way to do each thing: an audit before the freeze

*A survey of every place Oro currently spells one job two ways, what each pair
costs, and which half to cut. Analysis only — nothing here has been changed.*

The standard this document is measured against is the owner's:

> All code written by any developer or AI for a given task should look the
> same. If there is only one way to do a thing, there is not much ambiguity.

That is a stronger test than "is this feature useful". A second spelling can be
perfectly good and still fail it, because the cost is not paid by the person who
writes the code — it is paid by everyone who reads it, forever, and by every
model that has to guess which of two forms the codebase prefers. A frozen
language pays that cost on a permanent schedule.

The method here is the one the existing cuts used. For each candidate: name both
spellings with real code from the tree, ask whether they are genuinely the same
job, `grep` the tree for what breaks, and then apply **the ergonomics test that
`rsplit` failed** — write out the replacement at a real call site in
`std/http.oro`. A cut that is correct on paper and produces index arithmetic at
the call site is a wrong cut. `find(sep, reverse=true)` was correct on paper.

Everything below was checked by running it, not by reading the parser.
`./target/release/oro` at 0.2.0 is the oracle for every claim of behaviour.

---

## 0. The recommendations, ranked

| # | Candidate | Verdict | Confidence |
|---|---|---|---|
| 1 | `sep.join(xs)` alongside `xs.join(sep)` | **cut `str.join`/`bytes.join`** | high |
| 2 | Nine builtins that duplicate collection methods | **cut six, narrow two, keep `len`** — the two that *disagreed* are fixed | high |
| 3 | Six spellings of "sort this" | **cut `sorted()`, `list.sort`, `list.reverse`** | high |
| 4 | `type(x) == "<class 'bytes'>"` alongside `isinstance` | **cut the string form; close the gap that forces it** | high |
| 5 | Five exception classes nothing raises or catches | **cut `LookupError`, `ArithmeticError`, `NotImplementedError`, `StopIteration`** | high |
| 6 | Bare `raise` alongside `raise e` | **cut bare `raise`** | high |
| 7 | `while i < n: … i = i + 1` alongside `for i in range(n)` | **not a cut — a rule, and the stdlib is the offender** | high |
| 8 | Three spellings of "iterate a dict" | **cut bare `for k in d:`** | medium |
| 9 | Three spellings of "loop with an index" | **settle on `.enumerate()`** | medium |
| 10 | `ModuleNotFoundError` under `ImportError` | **merge, weakly** | low |
| 11 | Exception versus sentinel as a failure signal | **keep — but write the rule down** | high |
| 12 | `for` versus chains | **both earned; the line needs stating** | high |

Sections 13 onward record what is *earned* and why, and the "more than one way"
problems that are not redundancies.

---

## 1. `join` is spelled twice, and `std/http.oro` uses both

This is the clearest failure in the language, and the README already argues the
case — against a spelling it still ships.

The README, in the collection protocol:

> Note `xs.join(", ")` rather than `", ".join(xs)`: the sequence is the subject
> and the separator the detail, and this way it ends a chain instead of sending
> the reader back to the front of the line.

"Rather than" describes a choice that was never made. Both forms work today:

```python
["a", "b"].join("-")     # 'a-b'   — collection protocol
"-".join(["a", "b"])     # 'a-b'   — str method, CPython's spelling
b"-".join([b"a", b"b"])  # b'a-b'  — and on bytes too
```

They are byte-for-byte the same operation with the same type rules — both refuse
a mixed list with `join() requires str elements, found 'int'`. There is no case
one covers and the other does not.

**The evidence that this is not theoretical.** `std/http.oro` uses both, 244
lines apart:

```python
# std/http.oro:1537 — the collection form
return Response(405, {"allow": allowed.join(", ")}, b"method not allowed\n")

# std/http.oro:1781 — the str form, in the same file
return "&".join(out)
```

Across all `.oro` files: 26 collection-form calls, 16 str-form calls. Neither has
won. A model asked to join a list in Oro today has no way to know which the
codebase wants, because the codebase wants both.

**What breaks.** The 16 str-form sites are `corpus/core/03_strings.oro:4`,
`07_varargs.oro:14`, `15_textproc.oro:9`, `35_bytes.oro:65` (three calls),
`39_method_arity.oro` (six arity-error probes), `tests/programs/strings.oro:14`,
`bench/progs/strjoin.oro:8`, `corpus/divergence/63_percent.oro:29`, and
`std/http.oro:1781`.

**The real cost, named honestly.** Five of those are in `corpus/core/`, which is
the CPython-oracled half of the corpus. `xs.join(sep)` is not Python, so cutting
`str.join` moves those programs to `corpus/divergence/` and loses the oracle on
them. That is the same trade §7 item 3 of the design doc records regretting for
file I/O, and it deserves the same scrutiny.

It survives it, for a reason that did not apply to file I/O: `join` is not
subtle. The oracle earns its keep on float repr, half-to-even rounding, sort
stability and `split`'s `maxsplit` — behaviours where Oro can be quietly wrong.
Concatenating a list of strings with a separator has one possible answer, and
the mechanism for keeping oracle coverage anyway already exists: a `.twin.py`
carrying `", ".join(names)` next to the Oro file's `names.join(", ")`, which is
exactly what `corpus/divergence/52_removed_string_methods.oro` does for `lstrip`
and `rfind`.

**The ergonomics test.** At the one real call site:

```python
return "&".join(out)      # today
return out.join("&")      # after
```

It is shorter, it reads in the order it runs, and it chains — which is the whole
argument. `_parse_query` could then finish
`pairs.map(…).join("&")` in one expression instead of a `for` loop and a list.

**Verdict: cut `str.join` and `bytes.join`.** Raise `AttributeError` naming
`xs.join(sep)`, alongside `lstrip`, `rfind`, `index`, `rsplit` and `zfill` in
`cut_method_message` (`src/builtins/mod.rs:706`). The str/bytes surface goes from
sixteen names to fifteen, and the README's own sentence becomes true.

---

## 2. Nine builtins duplicate nine collection methods

This is the largest redundant surface in the language and the least discussed.
Every one of these pairs works today:

```python
len(xs)         xs.len()
sum(xs)         xs.sum()
min(xs)         xs.min()
max(xs)         xs.max()
sorted(xs)      xs.sorted()
any(xs)         xs.any()
all(xs)         xs.all()
enumerate(xs)   xs.enumerate()
zip(a, b)       a.zip(b)
```

Run against the binary, seven of the nine produce identical output on a list.
Usage across the tree is split down the middle and settles nothing:

| operation | prefix builtin | chain method |
|---|---|---|
| `sorted` | 35 | 14 (+6 `.sort_by`) |
| `len` | 118 | 2 |
| `min` | 7 | 4 |
| `sum` | 6 | 8 |
| `max` | 1 | 3 |
| `any` | 1 | 8 |
| `all` | 0 | 3 |
| `enumerate` | 3 | 2 |
| `zip` | 2 | 2 |

Two of the pairs did not merely duplicate — they **disagreed**, which is worse.
Both are now **fixed**; this section records what they were and which way each
went, because the *direction* is the part that binds the rest of the cut.

```python
zip([1, 2], [3, 4], [5, 6])     # [(1, 3, 5), (2, 4, 6)]
[1, 2].zip([3, 4], [5, 6])      # was [(1, 3), (2, 4)] — third argument silently dropped
```

`seq_native_method`'s `zip` arm read `args.first()` and ignored the rest. **The
method is now variadic**, and both spellings run one body (`zip_cols`), so
`a.zip(b, c)` is `zip(a, b, c)` at every arity including `a.zip()`. The method
was the side that moved because the method is the side that survives the rule
below: freezing a one-sequence `.zip` would have made the builtin uncuttable.

```python
sorted((3, 1, 2))       # was [1, 2, 3] — always a list; now (1, 2, 3)
(3, 1, 2).sorted()      # (1, 2, 3)  — type-preserving
```

Here the divergence was deliberate on the chain's side — type preservation is a
stated rule of the protocol, and `sorted` sits in the same arm as `reversed`,
`unique`, `take` and `drop`, all of which rebuild the receiver's shape — and
accidental on the builtin's, which simply hardcoded `Value::List`. **The builtin
now preserves the shape too**, on all four of its paths (native, `key=`,
`reverse=`, and `sorted` passed as a value). The CPython-oracle argument for
always answering a list is real but loses: the README already promises
type preservation for reordering, `sorted` is a reordering, and picking the list
would have meant changing `.sorted()` now and changing it back when the builtin
goes. Nothing in the tree passed a tuple to `sorted()`, so the change cost no
churn; `corpus/core/48_sorted_shape.oro` pins the cases that stay CPython's.

**What is left, and it is not these two.** A builtin walks a dict as its *keys*
(the iteration protocol) and a collection method walks it as its `(key, value)`
*pairs* (the collection protocol):

```python
sorted({"b": 1})        # ['b']      — the keys
{"b": 1}.sorted()       # {'b': 1}   — the dict, sorted
min({"b": 1, "a": 2})   # 'a'        vs  ('a', 2)
enumerate({"b": 1})     # [(0, 'b')] vs  [(0, ('b', 1))]
```

That is **one** divergence, not six: it hits `sorted`, `min`, `max`, `sum`,
`enumerate` and `zip` identically, and it is the only case in which any of the
nine pairs still answer different things. It is also not a bug in those six
names — it is a language-level question ("what is a dict's element?") already
answered twice, deliberately, in two protocols, and changing the builtin half
would put `enumerate(d)` at odds with `for x in d`. The cut below dissolves it
for free: when the collection-taking builtins go, only the method answer
remains. So neither answer should be entrenched first.

### The rule to draw

**A builtin takes scalars. A collection method takes a collection.** That single
sentence resolves eight of the nine, and it is the rule the language already
follows everywhere else — `to_str()` is a method because it has a receiver, and
`str()` is not callable for exactly this reason.

Applied:

- **`any`, `all` — cut the builtins.** Usage is 1 and 0 against 8 and 3. There
  is no capability on the builtin side. This is free.
- **`enumerate`, `zip` — cut the builtins.** Both gaps are closed. The chain's
  `enumerate` already took a start (`opt_int_arg(&args, 0, "enumerate", 0)`), so
  `xs.enumerate(1)` covers `enumerate(xs, 1)`; it now also *refuses* a second
  argument instead of ignoring it, which `enumerate(xs, 1, 9)` always did.
  `zip` is variadic, so `a.zip(b, c)` covers `zip(a, b, c)`.
- **`sum` — cut the builtin.** The only builtin-only case is `sum(xs, start)`,
  used nowhere in the tree; give `.sum(start)` the argument, or let
  `.reduce(start, (a, b) => a + b)` have it.
- **`min`, `max` — narrow, do not cut.** The variadic scalar form is
  load-bearing and has no chain spelling:

  ```python
  # std/http.oro:1437
  backoff = min(backoff * 2, _ACCEPT_BACKOFF_MAX)
  ```

  `[backoff * 2, _ACCEPT_BACKOFF_MAX].min()` allocates a list to clamp a float
  and reads worse. That is the ergonomics test failing, so `min(a, b)` stays.
  What goes is the *single-iterable* form: `min(xs)` becomes `xs.min()`, and
  `min` becomes a two-or-more-argument builtin with one meaning instead of two.
- **`sorted` — cut the builtin**, and see §3, which is a bigger mess than this
  one. Until then the builtin means what the method means: it preserves the
  argument's shape.
- **`len` — keep both, as the one named exception.** It is the only one of the
  nine that (a) works on `str` and `bytes`, which are deliberately outside the
  collection protocol, and (b) is the dispatch point for the `__len__` dunder. A
  cut in either direction has a real cost: cutting `len(x)` means reserving
  `.len()` on every user object, which the io protocol's own freeze note warns
  against doing lightly with `read` and `write`; cutting `.len()` breaks
  `xs.filter(p).len()` back to `len(xs.filter(p))` and sends the reader to the
  front of the line. Two spellings of length, stated as an exception with its
  reason, is better than either.

**What breaks.** The builtin-form uses of `any`/`all`/`enumerate`/`zip`/`sum` are
concentrated in `corpus/divergence/33_seq_builtins.oro` (five of five for
`enumerate`/`zip`) and a handful of core programs. `min`/`max`/`sorted` are
wider. Every one has a mechanical rewrite. Nothing in `std/http.oro` uses any of
the nine builtins except `min` (which stays), `len`, and `sorted` (§3).

---

## 3. Sorting is spelled six ways

```python
sorted(xs)                        # a new collection of xs's type
sorted(xs, key=f)                 # ditto, keyed
sorted(xs, key=f, reverse=true)   # ditto, keyed and reversed
xs.sorted()                       # a new collection of the receiver's type
xs.sort_by(f)                     # ditto, keyed
xs.sort()                         # in place, returns null
```

plus `xs.reverse()` (in place, returns `null`) and `xs.reversed()` (a new
collection). Eight names for one idea.

Two things here are worse than plain duplication.

**`sort` and `sorted` differ by two letters and by whether they mutate.** This is
the exact shape of the `lstrip`/`strip(side=)` complaint the README makes —
"three methods for one operation, distinguished by a letter, is exactly the
accretion the thesis rejects" — except that here the letter does not name a
direction, it names a side effect. And the in-place one returns `null`:

```python
xs = [3, 1, 2]
xs.sort()          # -> null; xs is now [1, 2, 3]
```

`xs = xs.sort()` silently binds `null`. That is one of the two or three most
reliably-hit beginner traps in Python, it is the same *class* of trap as `is`
(two similar spellings whose difference is invisible at the call site), and Oro
inherited it without argument.

**The keyed sort has two unrelated interfaces.** `sorted(key=f, reverse=true)` is
one call; the chain equivalent is `xs.sort_by(f).reversed()`, two. And
`xs.sorted(key=f)` is an error — `sorted() takes no keyword arguments` — so the
chain's `.sorted()` and the builtin's `sorted()` are not the same function
wearing two hats, they are two functions with one name and different arities.

**Verdict.** Cut `sorted()`, `list.sort()` and `list.reverse()`. The surviving
set is `xs.sorted()`, `xs.sort_by(f)` and `xs.reversed()` — three names, all
type-preserving, all returning a value, all chainable, no keyword arguments and
no null returns. `sorted(xs, key=f, reverse=true)` becomes
`xs.sort_by(f).reversed()`, which is one character longer and reads in the order
it runs.

**The ergonomics test.** `std/http.oro` sorts once:

```python
# std/http.oro:1566, in Router.allowed
return sorted(out)      # today
return out.sorted()     # after
```

Fine. `corpus/core/33_sorted_key_reverse.oro` is the only concentration of
`key=`/`reverse=` usage and rewrites mechanically.

**Where I am less sure.** Cutting in-place `sort` gives up sorting a large list
without allocating a second one. `xs = xs.sorted()` frees the original on the
rebind, so the peak is 2× briefly rather than 1×. For a language whose stated
limitation list already includes "`map`/`filter` are eager… a long chain over a
large list allocates once per step", this is consistent rather than a new sin —
but if there is a real program sorting a list big enough for that to matter, the
in-place form earns its place and the recommendation is wrong. I could not find
one in the tree.

---

## 4. `isinstance` exists, and the standard library does not use it

`std/http.oro` opens with eight constants:

```python
_BYTES = "<class 'bytes'>"
_STR = "<class 'str'>"
_INT = "<class 'int'>"
_FLOAT = "<class 'float'>"
_BOOL = "<class 'bool'>"
_LIST = "<class 'list'>"
_DICT = "<class 'dict'>"
```

and dispatches on them throughout:

```python
# std/http.oro:1836
def _query_bytes(v, part):
    t = type(v)
    if t == _STR:
        return v.to_bytes()
    if t == _BYTES:
        return v
    if t == _INT or t == _FLOAT or t == _BOOL:
        return f"{v}".to_bytes()
```

`isinstance` is a documented builtin and answers all seven of these:

```python
isinstance(b"x", bytes)    # true
isinstance("x", str)       # true
isinstance(1, int)         # true
```

So there are two ways to ask what a value is, and the standard library uses the
one that compares against the *rendered repr of a type object* — a string whose
exact spelling (`"<class 'bytes'>"`, angle brackets and inner quotes included) is
now load-bearing for the standard library's correctness, and which the README
has already changed once (`NoneType` → `null`).

**But there is a real gap underneath, and it is why this happened.** The two
stream types have no name to pass to `isinstance`:

```python
isinstance(b, Buffer)      # NameError: name 'Buffer' is not defined
isinstance(f, File)        # NameError: name 'File' is not defined
```

`std/io.oro` is therefore *forced* into the string comparison:

```python
# std/io.oro:52
_FILE = "<class 'File'>"
_BUFFER = "<class 'Buffer'>"
...
    t = type(r)
    if t == _FILE or t == _BUFFER:
        return _io.read_all(r)
```

with a comment that reads like an apology: *"A type check in a language that
avoids them is a small smell; three lines of dispatch is the honest price."* The
smell is not the type check. It is that the type check has no spelling.

**Verdict.** Two changes, one cut:

1. **Bind the runtime type names** — `File`, `Buffer`, `TcpStream`, `TcpListener`,
   `Task`, `Chan`, `Pattern`, `Match`, `Generator` — as globals `isinstance`
   accepts, the same way the exception classes are bound. Then `std/io.oro`'s
   fast path is `isinstance(r, File) or isinstance(r, Buffer)` and the two magic
   strings go.
2. **Rewrite `std/http.oro`'s seven constants as `isinstance` calls.**
3. Then the redundancy is gone by usage, and whether to *forbid* `type(x) == …`
   is a smaller question. I would not forbid it: `type(x)` has to exist for
   printing and for diagnostics (`f"a query {part} cannot be {t}"` is a good
   message), and a value returned for display that also happens to compare is
   not a second API. What must not survive is the standard library treating it as
   one.

**Ergonomics.** `if t == _STR:` becomes `if isinstance(v, str):`. Longer by four
characters, and it stops being a string comparison against a repr.

### What was actually done, and why it was the other way round

This verdict kept the wrong half. It counted two spellings — `isinstance` and
the repr string — and did not notice there was a **third**: `str` the value, a
shadowable global that `type()` never answered with. Three spellings of one
idea, and the reason the standard library reached for the repr string was not
that `isinstance` was missing names. It was that `type(x) == str` was *false*.

So the fix was underneath both: make the type names **keywords**, and make
`type(x)` answer with the very value the keyword denotes. Then `type(x) == str`
is true, the repr strings have nothing to compare against and are gone from
`std/http.oro` and `std/io.oro`, and the gap this section identified — `File`
and `Buffer` having no name — closes as a side effect, because a keyword is a
name. `isinstance` was then the only spelling left with no job, and it went.

The one thing `isinstance` could do that `==` cannot is a **subclass** test.
Audited across `std/`, `examples/`, `bench/` and `corpus/`: the tree contains
thirteen subclasses and *zero* subclass tests outside the file that existed to
demonstrate `isinstance`. Six of the thirteen are exception classes, matched by
`except`, which walks the chain in the VM and never went through `isinstance`;
the other seven are used by overriding a method, which is what inheritance is
for. See `corpus/divergence/66_type_keywords.oro`.

---

## 5. Five exception classes that nothing raises and nothing catches

A census of every `raise` and every `except` in `src/` and all 114 `.oro` files:

| class | raised | caught | verdict |
|---|---|---|---|
| `LookupError` | 0 | 0 | **dead** |
| `ArithmeticError` | 0 | 0 | **dead** |
| `NotImplementedError` | 0 | 0 | **dead** |
| `StopIteration` | 0 | 0 | **dead, and documented as live** |
| `BaseException` | 0 | 0 | keep — structural |

`BaseException` earns its place by *never* being caught: it is what keeps
`SystemExit` outside the reach of `except Exception`, which `std/http.oro:1248`
relies on explicitly (*"BaseException — SystemExit — is deliberately not
caught"*). A root that exists to be excluded is doing work.

The other four are not. `LookupError` and `ArithmeticError` are intermediate
nodes with no purpose: nothing raises them, nothing catches them, and no program
in the tree ever catches at their width. Compare the two intermediate nodes that
*do* earn their place:

```python
# std/http.oro:1405 — catches all four ConnectionError children at once
except ConnectionError as e:
    conn = null
# std/http.oro:1456 — catches the whole OSError subtree, deliberately
except OSError as e:
    pass
```

That is what an intermediate node is for, and `ConnectionError` is never raised
at its own width — only caught there. `LookupError` gets neither half.
`ArithmeticError` has exactly one child (`ZeroDivisionError`) and no second one
coming: Oro promotes to bignum rather than overflowing, so `OverflowError` is
structurally impossible, and there is no `FloatingPointError`. It is a node with
one child, which is not a hierarchy.

`NotImplementedError` is raised nowhere in 30,000 lines of Rust and 114 Oro
programs. It is a name in a table.

**`StopIteration` is the one to look at twice**, because the README says this:

> **Generators:** `yield`, generator objects, `for` iteration, **`StopIteration`
> on exhaustion**, and generators consuming generators

It does not happen:

```python
def g():
    yield 1
it = g()
for v in it:
    print("got", v)      # got 1
for v in it:
    print("again", v)    # nothing — no exception, no values
```

Generator exhaustion is signalled internally by the VM and never becomes a
`StopIteration` an Oro program can see. So this is a dead class *and* a false
sentence in the README, and the two have been protecting each other.

**Verdict.** Cut `LookupError`, `ArithmeticError`, `NotImplementedError` and
`StopIteration` from `src/vm/exceptions.rs`; reparent `KeyError` and
`IndexError` directly under `Exception` and `ZeroDivisionError` under
`Exception`. Correct the README's generator sentence. The hierarchy goes from 29
classes to 25, and every remaining node is either raised, caught, or structural.

**The cost, named.** `except LookupError` and `except ArithmeticError` are valid
CPython and become a `NameError` in Oro. Nothing in the tree writes either, and
neither is a common idiom — but this is a place where Oro stops being able to run
a Python program that used to work, and it should raise a message naming
`KeyError`/`IndexError`/`ZeroDivisionError` rather than a bare `NameError`, the
way the cut string methods do.

---

## 6. Bare `raise` is a second spelling of `raise e`

```python
try:
    risky()
except ValueError:
    raise            # re-raise the current exception

try:
    risky()
except ValueError as e:
    raise e          # re-raise the bound one
```

Both produce `ValueError: boom` with the same type and the same message. Oro has
no traceback objects, so there is nothing the bare form preserves that the named
form loses — the difference CPython has here does not exist.

Bare `raise` appears **once** in the entire tree, in
`corpus/core/21_exception_features.oro:93`, which is a test of bare `raise`.
`std/`, `examples/`, `bench/` and `tests/` never use it. The one place the
standard library re-raises does it with a name, and has to, because the raise
happens two branches later:

```python
# std/http.oro:1400-1419
failed = null
try:
    conn = ln.accept()
except OSError as e:
    failed = e
...
if failed != null:
    errors = errors + 1
    if errors > _MAX_ACCEPT_ERRORS:
        ...
        raise failed
```

Since `except:` bare is already cut, every handler that wants to re-raise can
name the exception with `as e` — the shape the module already uses everywhere.

**Verdict: cut bare `raise`.** It is a keyword form with an implicit subject in a
language that rejected `:=`, decorators, `with` and `del` for having implicit
subjects. The precedent is exact, the corpus cost is one file, and `raise e` is
two characters longer and says what it re-raises.

---

## 7. Looping: the answer is not the one the question expected

The concern was that there are too many ways to loop. There are — but not where
it looks, and the biggest problem is the reverse of the one suspected.

### 7a. The census

Across all 114 `.oro` files: **187 `for` statements, 82 `while` statements, and
about 20 uses of `.map`/`.filter` combined** — of which 19 are inside the four
files that *test* the collection protocol.

> **`std/http.oro` — 2,396 lines, the largest program in the language — contains
> zero uses of `.map`, `.filter`, `.reduce`, `.any` or `.find`.** It hand-writes
> 16 `for` loops, six of which are appends and two of which are `.any` and
> `.find` written longhand.

Those two:

```python
# std/http.oro:580 — this is .any()
def _has_token(value, token):
    if value == null:
        return false
    for part in value.split(","):
        if part.strip().lower() == token:
            return true
    return false

# after
    return value.split(",").any(p => p.strip().lower() == token)
```

Chains are a headline feature, they replaced comprehensions, and the standard
library does not use them. That is not a redundancy to cut — it is a claim the
tree does not currently support, and it should be fixed by using them, before the
freeze makes the current state the record.

### 7b. The genuine loop redundancy: two spellings of a count loop

Of the 82 `while` loops, **67 are a manual counter** — a hand-rolled `range`:

```python
# std/http.oro:199-204, building a character class at import
_tchars = []
_tb = 33
while _tb < 127:
    if f"{_tb:c}" not in _TOKEN_DELIMS:
        _tchars.append(_tb)
    _tb = _tb + 1
_TOKEN_OK = _tchars.to_bytes()
```

`for … in range(…)` is used 17 times. So the tree spells a bounded count loop two
ways, and the four-times-more-common spelling is the one that needs a
hand-maintained induction variable, leaves the counter bound after the loop, and
gets the increment wrong if you write `continue`.

The obvious defence would be speed. It is not true — measured on this binary,
three million iterations:

```
i = 0; while i < 3000000: total = total + i; i = i + 1     0.23s
for i in range(3000000): total = total + i                 0.15s
```

**The `for` loop is 35% faster.** The manual counter is longer, more error-prone
*and* slower, and it is what `std/http.oro`, `std/io.oro` (three of three loops)
and nine of thirteen benchmark programs are written in.

**Verdict: not a cut.** You cannot cut `while i < n` — it is a `while` with a
comparison in it, and `while` is not going anywhere (a condition is not a
sequence: `std/http.oro:1488`'s `while len(live) > 0 and time.monotonic() <
deadline:` has no `for` spelling at all). This is a **rule plus a rewrite**:

> A loop over a known count is `for … in range(…)`. A loop over a collection is
> `for … in` the collection, or a chain. `while` is for a condition that is not
> a count — a poll, an EOF drain, an accept loop.

Write it in the README next to the block-scope note, and rewrite the 67 sites.
The 10 EOF-sentinel `while` loops (`while chunk != b"":`) and the four `while
true:` loops are correct as they stand.

The line above matters more than a style guide usually would, because there is
nothing in Oro that can enforce it. `oro fmt` is a formatter, not a linter, and
"no options, one output" means it will never grow a rule. The rewrite is the
enforcement.

### 7c. Three ways to loop with an index

```python
for i in range(len(xs)):    # 0 uses in the tree
for i, x in enumerate(xs):  # 3 uses
for i, x in xs.enumerate(): # 2 uses
i = 0
while i < len(xs):          # 3 uses
```

`range(len(xs))` appears **zero** times, which is a small mercy. §2 cuts the
`enumerate` builtin, which leaves `.enumerate()` as the one spelling and folds
this question into that one.

### 7d. Three ways to iterate a dict

```python
for k in d:            # implicit keys
for k in d.keys():     # explicit keys
for k, v in d.items(): # pairs
```

The first two are the same operation. `d.keys()` already answers a real list —
that is a documented divergence with its own corpus file — so `for k in d:` is
not saving an allocation, it is saving five characters and asking the reader to
remember that iterating a dict means iterating its keys rather than its entries,
which is the one thing about dict iteration that people get wrong.

**Verdict: cut bare `for k in d:`**, with an error naming `.keys()` and
`.items()`. Medium confidence — it is CPython-identical behaviour under
CPython-identical syntax, so it is not *wrong*, and cutting it costs oracle
coverage on any core program that uses it. The argument for cutting anyway is
that Oro already broke with CPython on what `.keys()` *returns*, so the "views
are cheap, iteration is the fast path" reasoning that justifies the implicit form
in Python does not hold here.

---

## 8. Two ways to fail — and the rule is right but unwritten

Oro signals failure two ways, and both are load-bearing:

| API | failure signal |
|---|---|
| `str.find` / `bytes.find` | `-1` |
| `list.find(pred)` / `dict.find(pred)` | `null` |
| `dict.get(k)` | `null` |
| `Reader.read(n)` | `b""` at EOF |
| `http.read_request(r)` | `null` at a clean EOF |
| `dict.pop(k)` | raises `KeyError` |
| `io.read(r, n)` | raises `EOFError` |
| everything in `net`, `json`, `proc` | raises |

This looks like the sin, and it is not, because the split is principled: **a
lookup that can legitimately find nothing answers with a value; an operation that
was asked to do something and could not, raises.** `find` returning `-1` is not
a failure, it is the answer to "where is this" when the answer is "nowhere".
EOF is not a failure — the design doc says so in as many words: *"every stream
ends, and the normal termination of the most common loop in systems programming
is not a fault."*

The evidence that the boundary is real: the crossings are all in one direction.
`_try_read` (`std/http.oro:1234`), `_drain` (`:1143`) and `io.read(r, n)`
(`std/io.oro:95`) each convert an exception *down* into a sentinel, because the
caller's loop is sentinel-driven. Nothing in the tree converts a sentinel up into
an exception except `io.read(r, n)`, which is the documented pairing of a raw
primitive with a whole-job function.

**Verdict: keep both, and write the rule down.** It is currently discoverable
only by reading forty call sites. One paragraph in the README, next to the io
protocol's EOF note, settles it before the freeze rather than after.

**One inconsistency inside it.** `find` uses two different sentinels depending on
the receiver:

```python
"abc".find("z")              # -1
[1, 2, 3].find(x => x > 5)   # null
```

That is correct in both cases — an index has a sentinel below its range and an
element does not — but it means `xs.find(…)` and `s.find(…)` are two functions
with one name, distinguished by whether the argument is a lambda. `count` has the
same shape (`"aab".count("a")` is 2 by substring; `[1,0,2].count()` is 2 by
truthiness). I would leave both: the names mean "search" and "tally" in both
protocols, which is a consistency worth more than the collision costs. It is
worth a sentence in the README so nobody discovers it by writing
`xs.find("a")` and getting `null` instead of an index.

---

## 9. `for` versus chains — earned, and the line needs stating

This was the specific worry, and the two are not the same job.

**The decisive difference is laziness, and it is already documented as a
limitation rather than as a design line.** From Known Limitations:

> Consuming an infinite generator hangs. A generator passed to a builtin (`sum`,
> `sorted`, `join`, …) or used to start a chain is drained eagerly, so
> `sum(forever())` never returns rather than failing. **Inside a `for` loop it
> stays lazy**, as it always was.

So `for` is the lazy iteration construct and a chain is the eager one. They are
not two spellings of one thing; they are the two halves of a genuine split, and
`for msg in ch:` — iterating a channel that is filled by another task — is only
expressible on the lazy side. A chain over a channel would deadlock.

Three more things `for` does that a chain cannot:

- **Statements.** A lambda body is one expression. `_ChunkedReader.chunk_size`
  accumulates a hex value with a lookup, a null check, a raise and an
  accumulation — four statements per element. `reduce` cannot take it.
- **Early exit with a side effect.** `Router.handler_for` (`std/http.oro:1544`)
  finds a route *and* writes `req.params` before returning. `.find()` returns the
  element and drops the binding.
- **Two accumulators.** `Router.allowed` appends to one list from two loops.

And the reverse: a chain does one thing `for` cannot, which is be an
*expression* — `orders.filter(o => o.paid).group_by(o => o.region)` is a value,
not a block.

**Verdict: both earned.** What is missing is the line, which should go in the
README where the collection protocol is introduced:

> A chain is an expression: it consumes a whole collection, eagerly, and produces
> a value. A `for` is a statement: it runs statements per element, lazily, and
> can stop early. If the loop body is a single append of an expression, it is a
> chain written longhand.

That last sentence is the one that resolves the 42 accumulate-shaped `for` loops
in the tree — including six in `std/http.oro` — without needing to cut anything.

---

## 10. Conditionals: `match`, `if`, and truthiness

**`match` versus `if`/`elif` is earned, narrowly, and on a performance claim that
should be verified before the freeze.** The README is honest that the overlap is
total and that the justification is entirely the jump table:

> When every case is a literal, the whole thing compiles to an O(1) jump table
> rather than a comparison chain; that performance win over an `if`/`elif` ladder
> is the actual reason `match` earns its place, not syntactic sugar.

That is the right kind of argument — a second spelling that buys a different
complexity class is not a second spelling of the same thing. But it is the *only*
argument, so it should be measured, and I did not find a benchmark for it in
`bench/`. If the jump table is not there, or is not faster than the ladder at
realistic case counts, `match` is sugar and the argument for keeping it
evaporates. This is the one recommendation in the document that is contingent on
a measurement nobody has taken.

**Truthiness is the surprise.** Oro has full Python truthiness — `[]`, `""`,
`0`, `b""`, `{}` and `null` are all falsy — and the standard library **never uses
it**. Every conditional in `std/http.oro` on a non-boolean is written out:

- 63 explicit tests (`len(x) == 0`, `!= null`, `== b""`, `== ""`)
- 16 bare `if x:` tests, and **all sixteen are on a `bool`** — `self.done`,
  `streamed`, `keep_alive`, `suppress`, `chunked`, `keep`, `closed`,
  `entry.stopping`, `entry.busy`, `handed_over`, `while true`

Not once does the standard library write `if xs:` for a list or `if s:` for a
string. So `if len(parts) == 0:` and `if not parts:` are two spellings of one
test, and the language's own largest program declines to use one of them.

**Verdict: keep truthiness, and say why.** The cut is available and I do not
recommend it, for two reasons that are stronger than the redundancy:

1. `and`/`or` return an operand rather than a bool, which makes `a or "default"`
   the language's idiom for a fallback. That requires knowing whether `a` is
   falsy. Restricting `if` to bools without restricting `or` is incoherent, and
   restricting `or` loses a real capability.
2. It is in the computational core, `corpus/core/04_truthiness.oro` oracles it
   against CPython, and the divergence would be gratuitous.

But the stdlib's unanimous choice is worth reading as data rather than accident.
Truthiness is genuinely more dangerous in Oro than in Python, because Oro chose
sentinel returns: `if s.find(x):` is wrong when the match is at position 0 and
also wrong when there is no match (`-1` is truthy), and `if d.get(k):` cannot
distinguish a missing key from a key holding `0`. The README should say that
explicitly and recommend the explicit test — which is what the standard library
already does, unanimously, without saying so.

---

## 11. Strings, collections, functions — mostly settled

The recently-redesigned string surface holds up. `strip(side=)`, `split(side=)`,
`find(reverse=)` and `count` searching `find`'s window are one operation each
with a named parameter, which is the right shape and is the same shape
`json.stringify(indent=)` and `chan(n)` take. Nothing there needs cutting except
`join` (§1).

**`to_str()` versus f-string interpolation.** Both exist and both are right.
`x.to_str()` is a conversion producing a value; `f"{x}"` is a template. They
overlap only in `s = x.to_str()` versus `s = f"{x}"`, and the migration table
already picks the winner for string *building*. The one thing worth noting is
that `+` is a third spelling in that narrow case, and the standard library uses
all three — 68 f-strings, eight `+` concatenations:

```python
# std/http.oro:1516
self.exact[method + " " + path] = handler
# would be
self.exact[f"{method} {path}"] = handler
```

I would not cut `+` — it is the concatenation operator, it is oracled, and
cutting an operator to force a template is absurd. But the line is worth stating:
**`+` joins two strings you already have; an f-string is for anything that needs
formatting or more than two pieces.** Under that rule `method + " " + path` is an
f-string and `existing + ", " + value` is too.

**Collections.** `[]`/`{}` literals versus `to_list()`/`to_dict()` casts do not
overlap — construction versus conversion, and the README's argument that a
conversion needs something to convert is exactly right. `d.get(k)` versus `d[k]`
in a `try` is not two spellings either: one asks and one demands, and the
sentinel/exception rule in §8 covers which to reach for.

The one wart is that `xs.to_list()` on a list is a no-op that still allocates,
and `"abc".to_list()` is the only bridge from a string into the collection
protocol. Neither is a redundancy; the second is a gap worth remembering when
someone asks why `"abc".map(f)` is an `AttributeError`.

**Functions.** `def` versus `=>` is genuinely two things: a lambda body is one
expression and cannot hold statements, defaults, `*args` or `**kwargs`. There is
no case where both apply and one is better. `logged(h)` in
`examples/server.oro` shows them composing rather than competing — a `def` that
returns a lambda that calls a `def`.

`http.fetch` versus `http.stream` — the pair the brief asked about specifically —
**holds**, and the argument in the module is the right one:

> The same call as `fetch` with one difference, and the difference is *lifetime*
> rather than an option: this hands back a live connection, and the caller owns
> it until they call `resp.close()`. That is not something a keyword argument can
> say, which is why this is a second name and not `fetch(stream=true)` — a flag
> that changes what a caller must do afterwards is a flag that will be missed.

That generalises into a rule the rest of the standard library already obeys and
should state: **a parameter changes what a function does; a second name is for
when it changes what the caller must do next.** `json.stringify(indent=)`,
`chan(n)`, `split(side=)` and `sorted(reverse=)` are all the first kind.
`fetch`/`stream` is the only instance of the second, and there should not be a
third without the same argument.

---

## 12. Not redundancy, but still "more than one way"

Four things that are not two spellings of one job but do let one job be written
two ways.

**`is_digit` means two different things.** On `str` it is Unicode-numeric; on
`bytes` it is ASCII:

```python
"٣".is_digit()      # true  (Arabic-Indic three)
"٣".to_int()        # ValueError: invalid literal for int(): '٣'
```

So `s.is_digit()` does not mean "this will parse as a number", which is what
every caller wants it for. `std/http.oro` knows this and hand-rolls the check
rather than using the method:

```python
# std/http.oro:828, validating a Content-Length
for ch in s:
    if ch not in "0123456789":
        fail(f"malformed Content-Length '{s}'")
```

and elsewhere uses a *third* spelling for the same question on bytes:

```python
# std/http.oro:1969, validating a port
if len(digits) == 0 or digits.scan(_DIGIT_OK) != len(digits):
```

Three spellings of "is this all digits", in one file, because the shortest one is
wrong. This matches CPython and is therefore oracled, but it is worth a README
line: `is_digit` on `str` follows Unicode and is not a numeric-parse test.

**`find(x) >= 0` is used as a containment test seven times in `std/http.oro`**
where `in` exists and reads better:

```python
# std/http.oro:1038
if name.find("\r") >= 0 or name.find("\n") >= 0 or name.find(":") >= 0:
# in
if "\r" in name or "\n" in name or ":" in name:
```

Both earned — `find` answers "where" and `in` answers "whether" — but a `find`
whose result is only compared against zero is `in` written longhand, and the
standard library writes it longhand more often than not.

**Two mechanisms for injecting a failure mode.** `std/http.oro` parameterises
"which exception does this raise" two different ways in one file: as a function
argument (`_percent_decode(b, plus, fail=_fail_request, where=…)`, with three
`_fail_*` functions) and as an overridable method (`_HeadParser.fail` /
`_ResponseParser.fail`, `_ChunkedReader.fail`, `_LimitReader.truncated`). Both
solve "the request side and the response side disagree about who is at fault".
Two mechanisms for one problem in one module is the same sin one level up; the
method form is the better one (it inherits, and `headers()` is shared verbatim by
both parsers), and the three `_fail_*` free functions exist only because
`_percent_decode` and `_parse_query` are free functions rather than methods.
Low priority, but worth resolving before the module freezes.

**`Router.add` chains and the example does not chain.** `add` returns `self`
*"so routes chain in the reading order Oro prefers"*, and `examples/server.oro`
writes six separate statements. Two spellings, one advertised, the other
demonstrated. Pick one — the statements read better at six routes, which suggests
the chaining return is the part to drop.

**Already settled, worth recording as closed.** `docs/stdlib-server-design.md`
§7 item 10 asks that `chan(0)` be an explicit synonym for `chan()` rather than an
error. It is: `chan(0)` runs. That item can be marked closed.

---

## 13. What is earned, and why — settled before the freeze

Recorded so these questions do not get reopened.

**`for` and `while`.** A condition is not a sequence. `while len(live) > 0 and
time.monotonic() < deadline:` has no `for` spelling. Both stay. (The *manual
counter* idiom inside `while` is the problem — §7b.)

**`try`/`finally` versus block scope plus refcounting.** The brief asked whether
`finally` overlaps with the mechanism that replaced `with`. It does not. There
are fifteen `finally` blocks in the tree; six touch resources, and the three in
`std/` each do something refcounting structurally cannot:

- `_conn_task` (`std/http.oro:1463`) removes an entry from a registry dict that
  *outlives the block* — the refcount never drops, because `live` holds it.
- `stream` (`:2296`) closes the socket on every path except success, where
  ownership transfers to `resp.conn`. A refcount cannot distinguish "the frame
  ended" from "the frame ended having given the socket away"; the `handed_over`
  flag encodes exactly that.
- `fetch` (`:2259`) closes *after* the return value is computed and before the
  frame leaves. Destruction of a returned value cannot be made to happen there.

The other nine are `log.append(...)` in the corpus programs that prove `break`,
`continue` and nested `try` all run their `finally`. **`finally` is earned**, and
its job is ordering and ownership transfer, not lifetime — which is worth saying
in the README, because "Oro has no `with` because refcounting is
deterministic" invites exactly the wrong inference.

**`BadRequest` and `BadResponse` as separate classes.** The module's own
argument — *"`except http.BadRequest` in a handler that makes an outbound call
must not swallow 'the service I called is broken' and report it to my client as
*their* mistake"* — is correct and is the best short justification for a class
in the tree.

**`OSError` and `ConnectionError` as intermediate nodes.** Both are caught at
their own width in production code and `ConnectionError` is never raised at its
own width, which is precisely what an intermediate node is for. Contrast
`LookupError`, which gets neither (§5).

**`io.read(r, n)` versus `r.read(n)`.** Not two spellings: one is a syscall's
worth and the other is a loop with an `EOFError`. The design doc's framing —
*"`r.read(n)` is the raw primitive; `io.read(...)` does the whole job"* — is the
right one, and the absence of `io.write` is a genuine asymmetry rather than an
inconsistency, for the reason already given.

**No `http.get`/`post`/`put`.** The method is an argument because it is an
argument. Correct, and the same argument kills any future per-verb wrapper.

**`chan()`/`chan(n)`, `json.stringify(indent=)`, `strip(side=)`,
`split(side=)`, `find(reverse=)`.** All one name with a parameter, which is the
shape. Consistent.

---

## 14. If only three things happen

1. **Cut `str.join`/`bytes.join`** (§1). One name, one file of corpus churn, and
   it makes a sentence the README already prints become true.
2. **Draw the builtin/method line and cut the six builtins it removes** (§2, §3).
   This is the largest ambiguity surface in the language and it is currently
   split 50/50 in usage. Two of the pairs silently disagreed; those two are
   fixed (§2), which removes the urgency but not the case — nine names still
   mean the same nine things.
3. **Rewrite the 67 manual-counter loops as `for … in range(…)` and write the
   loop rule down** (§7b). Nothing is cut, the code gets shorter and 35% faster,
   and the language stops having a dominant idiom that nobody chose.
