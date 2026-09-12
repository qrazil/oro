# Functions, scope and objects: which spelling is right where

*An audit of the situations in which Oro offers more than one legitimate
spelling for a function, a parameter, an object or a binding — naming the
right spelling for each, and checking whether the tree uses it. Analysis
only; nothing here has been changed.*

`docs/one-way-audit.md` asked "are these redundant?" and answered by cutting.
This asks a sharper question, and cuts almost nothing:

> Several spellings can legitimately coexist. But **for any given situation,
> one of them is right.** Find the situations, name the right spelling for
> each, and check whether the tree actually uses it.

The standard is the owner's: *all code written for a given task by any
developer or AI should look the same.* A language can pass the redundancy test
and still fail this one. `def` and `=>` are not two ways to do one job — one
takes statements — but there is still a line between them, and if the line is
unwritten then two authors will put it in two places. That is the failure this
document is looking for: not a duplicate feature, but **an unstated rule**.

Everything below was checked by running it. `./target/release/oro` at 0.2.0 is
the oracle for every behavioural claim, and every count is a `grep` over
`std/`, `examples/`, `corpus/`, `bench/` and `tests/` — 120 `.oro` files.

Line numbers are given as `file:line` and anchored to a function name as well,
because `std/` and `examples/` are under edit as this is written.

---

## 0. The verdicts, ranked

| # | Situation | Verdict | Confidence |
|---|---|---|---|
| 1 | A tuning parameter at the call site: positional or named | **name it — and `spawn` currently makes that impossible** | high |
| 2 | `def` versus `=>`: named helper or inline callback | **the rule is clear; the stdlib uses neither half of it** | high |
| 3 | A second name versus a keyword: `quote_plus` beside `unquote(plus=)` | **one rule, applied to the decoder and not the encoder** | medium |
| 4 | `__str__` versus `__repr__` | **`__repr__` alone is the default; and `repr()` has a real inconsistency** | high |
| 5 | `__ne__` | **a dunder outside the documented set, and the one that can contradict `__eq__`** | high |
| 6 | A record: class, dict, or tuple | **the line is real and the tree follows it — but names the wrong reason** | high |
| 7 | Method versus free function | **the rule exists, is written down in the design doc, and holds** | high |
| 8 | Varying behaviour: subclass hook, function argument, or duck-typed attribute | **three mechanisms, one job, in one file** | medium |
| 9 | Defaults no call site ever takes | **three sibling helpers, three different shapes** | high |
| 10 | Sharing mutable state: `global`, field, dict, channel | **settled — the tree already agrees, unanimously and silently** | high |
| 11 | `spawn(f, *args)` versus `spawn(() => f(...))` | **settled — `*args`, 53 to 1** | high |
| 12 | Dunders that nothing defines | **five with no definition at all — keep them; but `__len__` has one, and it is half of `len`'s justification** | medium |

§13 records what is **earned** — where two spellings both hold their place and
the question should not be reopened.

---

## 1. A tuning parameter at the call site, and the one that cannot be named

This is the largest unstated rule in the language, it has a clean answer, and
the answer is currently unavailable at the most important call site in the
standard library.

### The situation

Oro has positional parameters, parameters with defaults, `*args`, `**kwargs`,
and call-site `*`/`**` unpacking. It has **no keyword-only marker** and **no
positional-only marker** — both are parser errors:

```python
def f(a, *, b=1): ...     # expected a parameter name after `*`, found `,`
def g(a, /, b): ...       # expected a parameter name, found `/`
```

So every parameter that has a default can be reached two ways, and the author
of the function cannot forbid either one:

```python
http.fetch("GET", url, null, null, 30, 1024)         # positional
http.fetch("GET", url, timeout=30, max_body=1024)    # named
```

Both are legal, both are in the tree, and they are the same call.

### The right spelling

> **A parameter you pass because it names a variant of the operation is
> written with its keyword. A run of `null`s to reach an argument is never the
> right spelling.**

Stated from the reader's side, which is the side that matters: if a reader who
does not know the function's signature cannot say what an argument *is*, name
it. `unquote(s, true)` is unreadable; `unquote(s, plus=true)` is not.

### What the tree does

The tree obeys this rule *perfectly* for one class of parameter and *never*
for the other, and nobody has noticed because the two classes never meet in
one call.

Keyword parameters that select a **variant**, counted at call sites across the
whole tree:

| keyword | call sites |
|---|---|
| `safe=` | 45 |
| `side=` | 41 |
| `reverse=` | 30 |
| `key=` | 17 |
| `status=` | 13 |
| `params=` | 12 |
| `indent=` | 10 |
| `plus=` | 9 |
| `headers=` / `body=` | 8 each |

Keyword parameters that are a **tuning number or a collaborator**, counted the
same way:

| keyword | call sites |
|---|---|
| `max_conns=` | **0** |
| `max_requests=` | **0** |
| `timeout=` | **0** |
| `drain=` | **0** |
| `ready=` | **0** |
| `max_body=` | **0** |
| `watch=` | **0** |
| `fail=` | **0** |
| `where=` | **0** |
| `kind=` | **0** |
| `plus_is_space=` | **0** |

Eleven parameters, every one of them declared with a default and a name, and
**not one of them is ever passed by its name anywhere in 120 files.** Every
one is reached positionally. Here is what that looks like:

```python
# corpus/divergence/62_http_client_socket.oro:210
http.fetch("GET", base + "/big", null, null, _DEADLINE, 1024)

# corpus/divergence/60_http_serve.oro:108
spawn(http.serve, "127.0.0.1:0", handler, one, 8, 2)

# corpus/divergence/60_http_serve.oro:121
spawn(http.serve, "127.0.0.1:0", handler, full, 1)

# std/http.oro:742, inside _parse_query
out[_percent_decode(pair, true, fail, where).to_str()] = ""
```

`8, 2` is `max_conns=8, max_requests=2`. `full, 1` is `ready=full,
max_conns=1`. `true` is `plus_is_space=true`. The file that contains
`spawn(http.serve, "127.0.0.1:0", handler, one, 8, 2)` is obliged to explain
the numbers in a comment above it, which is the tell: a call site that needs a
comment to say what its arguments are is a call site missing its keywords.

### Why it happens, and this is the part that matters

Two of those three sites **cannot** be written with keywords, because
`spawn` refuses them:

```python
def f(a, b=1, c=2):
    return f"{a} {b} {c}"

spawn(f, 1, 2, 3)      # fine
spawn(f, 1, c=9)       # TypeError: spawn() takes no keyword arguments
```

`src/vm/sched.rs:1457`. `do_spawn` rejects a non-empty kwargs vector and then
calls `bind_call(f, None, args, Vec::new())` — it passes an empty keyword list
through to the callee unconditionally. There is no stated reason for the
refusal anywhere in `src/`, `README.md` or `docs/`, and `spawn` has no keyword
parameters of its own, so there is no ambiguity to protect against.

The consequence is precise and it is bad: **`http.serve` is the function in
the standard library most likely to be spawned, and running it in the
background is the one context in which none of its five keyword parameters can
be named.** A language whose whole thesis is one legible spelling has put its
flagship API behind a call form that forbids the legible one.

### Is seven parameters the right shape at all?

```python
def serve(addr, handler, ready=null, max_conns=_MAX_CONNS,
          max_requests=_MAX_REQUESTS_PER_CONN, timeout=_CONN_TIMEOUT,
          drain=_DRAIN_SECONDS):
```

Two required, five optional. Run the ergonomics test — write each alternative
at the three real call sites.

**An options dict.**

```python
spawn(http.serve, "127.0.0.1:0", handler, one, {"max_conns": 8, "max_requests": 2})
```

Readable, and it survives `spawn`. It also turns five checked parameter names
into five unchecked dict keys: `{"max_conn": 8}` is a typo the runtime cannot
see, the defaults stop being documented in the signature, and the language has
just grown a second calling convention for one function. Reject.

**A `Server` class.**

```python
srv = http.Server("127.0.0.1:0", handler)
srv.max_conns = 8
srv.max_requests = 2
spawn(srv.run, one)
```

Four lines where there was one, a two-phase object where a function did the
job, and a lifetime for a caller to get wrong — which is exactly what
`docs/stdlib-server-design.md` §3 refused when it made the listener the handle
and the channel the way to pass it. Reject.

**A keyword-only marker (`def serve(addr, handler, *, ready=null, ...)`).**
This is CPython's answer and it is the wrong one here, for a reason specific to
Oro: it would not fix the `spawn` sites at all. `spawn` takes no keywords, so a
keyword-only `serve` becomes *unspawnable with any option set*. A rule that
makes the common case illegal is not a rule.

**Keep the seven; let `spawn` forward keywords.**

```python
spawn(http.serve, "127.0.0.1:0", handler, ready=one, max_conns=8, max_requests=2)
```

One line, every number named, no new name in `http`, no new shape, and no
change to any existing call. It is additive — nothing that runs today stops
running — and in the VM it is passing `kwargs` where `Vec::new()` is passed
now.

**Verdict.** The seven-parameter shape is right: they are genuinely the numbers
a deployment must change, they are documented in the signature where a reader
finds them, and no smaller shape survives the ergonomics test. What is wrong is
that the readable spelling is unavailable through `spawn`, and that the rule
"name a tuning argument" is nowhere written down. Both are cheap to fix and
neither is a cut.

**Confidence: high** on the rule and on the diagnosis; **medium** on changing
`spawn`, because it is a language change under a freeze and I have not looked
for a reason the refusal might be load-bearing that is not in the source.

### The corollary: `*args` and `**kwargs` are almost unused, and that is fine

Across 120 `.oro` files, a `*args` or `**kwargs` parameter is declared in
**four** places: `corpus/core/07_varargs.oro`, `corpus/divergence/67_dict_pairs.oro`,
and two functions in `tests/programs/varargs.oro`. Call-site `*`/`**`
unpacking appears in the same four files and nowhere else. Zero in `std/`,
zero in `examples/`, zero in `bench/`.

This is not a dead feature to cut — `spawn(f, *args)` is variadic in the VM and
`min(a, b, c)` is variadic, so the *capability* is load-bearing. It is a
feature with a narrow right answer:

> **Write `*args` when the function genuinely forwards an unknown call to
> something else. Everything else takes named parameters.**

The tree already obeys this without being told. No change needed; it is worth
one sentence in the README so that the next author does not reach for
`**options` when they mean four parameters.

---

## 2. `def` versus `=>`, and a standard library with no lambdas in it

### The situation

```python
def double(n):          #  a statement, a name, any number of statements
    return n * 3

double = x => x * 3     #  an expression, one expression as its body
xs.map(x => x * 3)      #  an expression, unnamed, at the point of use
```

These are not two ways to do one job, and `docs/one-way-audit.md` §11 is right
that there is no case where both apply and one is better. But that is a
statement about the *feature*, not about the *situation*, and there are three
distinct situations here with three different right answers.

### The rules

> **1. A function that has a name is a `def`.** A lambda bound to a name is a
> `def` written in a shape that cannot grow a second statement.
>
> **2. A callback used once, at the point it is passed, is a `=>`.** Lifting it
> to a `def` puts the body somewhere the reader is not.
>
> **3. A callback that needs statements is a nested `def`, not a lambda
> calling a helper.** A nested `def` closes over the enclosing function's
> names by read, which is the whole of what a lambda would have captured.
>
> **4. Neither can appear inside an f-string field.** This is enforced, with a
> message naming the fix.

Rule 4 is worth recording as settled, because the enforcement is exemplary:

```python
print(f"{xs.map(x => x * 2)}")
# a lambda cannot appear inside an f-string field — assign it to a name
# first, e.g. `doubled = xs.map(x => x * 2)` then `f"{doubled}"`
```

That is a removal that names its replacement, which is this language's
convention, and there is nothing to audit. It applies to a lambda anywhere in
the field, not only at the top level, and there is exactly one spelling of the
workaround.

### What the tree does — rules 1 and 2

Rule 1 holds. A lambda is bound to a name in five places, all of them inside
`corpus/divergence/34_lambdas_and_chains.oro`, which is the file whose job is
to exercise lambdas. Nothing else in the tree names a lambda. Good.

Rule 2 is where it falls apart, and the number is stark:

| | `def` | `=>` |
|---|---|---|
| `std/` (2,579 lines) | 123 | **0** |
| `examples/` | 14 | 3 |
| `corpus/` | 289 | 117 |
| `bench/` | 19 | 9 |
| `tests/` | 11 | 0 |

`grep '=>' std/` returns four lines. One is the string literal
`_TOKEN_DELIMS = '"(),/:;<=>?@[\\]{}'` (`std/http.oro:189`), which contains
`<=>` by coincidence. The other three are comments — two of them *showing* a
lambda in a usage example (`std/http.oro:1295`, `:1500`) that the module then
does not write.

**The standard library contains no lambdas.** That is the same shape as the
finding in `docs/one-way-audit.md` §7a — zero `.map`/`.filter`/`.reduce` in what is
today 2,388 lines — and it is the same fact seen from the other side, because a
chain without a lambda is a chain with nothing to do. I re-ran the count while
auditing: `std/http.oro` has 24 calls that look like a chain and every one of
them is `str.find` or `bytes.find`. The collection protocol is used zero times
in the standard library.

I am not re-reporting that; §7a already did. What is new here is the
*direction*: the earlier audit read it as "the stdlib should use chains". The
stronger reading is that **a lambda and a chain are one feature, and the
standard library does not use the feature at all.** Sixteen hand-written `for`
loops, six of them a bare append, is what "no lambdas" looks like from the
inside. The README sells `orders.filter(o => o.paid).group_by(o => o.region)`
on the strength of a library that never writes it.

### Rule 3, and the one place the tree gets it wrong

`examples/server.oro` is the language's one-screen demonstration, and the
sentence it is built to prove is *"middleware in a language with first-class
functions is a function that takes a handler and answers a handler. There is no
framework holding this."* Here is how it proves it:

```python
# examples/server.oro:64-72
def logged(h):
    return req => _log(h, req)


def _log(h, req):
    print(f"{time.monotonic() - _STARTED:7.3f}  -->  {req.method} {req.path}")
    resp = h(req)
    print(f"{time.monotonic() - _STARTED:7.3f}  <--  {resp.status} {req.path}")
    return resp
```

Two names, and the second takes `h` as a parameter. `_log` is not a helper
anyone would write on its own — it exists because the lambda's body must be one
expression and the middleware's body is four statements, so the statements were
moved to a top-level function and `h` had to be handed to it explicitly.

The language supports the shape this wanted, and it works today:

```python
def logged(h):
    def wrapped(req):
        print(f"{time.monotonic() - _STARTED:7.3f}  -->  {req.method} {req.path}")
        resp = h(req)
        print(f"{time.monotonic() - _STARTED:7.3f}  <--  {resp.status} {req.path}")
        return resp
    return wrapped
```

One name instead of two, one parameter instead of two, `h` captured rather than
threaded, and the body of the middleware next to the thing it wraps. Verified
against the binary: a nested `def` closing over its enclosing function's
parameter by read is exactly what closures already do.

**Verdict: rule 3, and `examples/server.oro:64-72` is the tree's one
disagreement with it.** The fix is local and it makes the file's own sentence
truer, not less true — a nested `def` *is* the first-class-function answer, and
it needs one name where the current shape needs two.

The second lambda in the same file is correct and should stay:

```python
# examples/server.oro:96
http.serve(addr, logged(req => routes.dispatch(req)), ready)
```

One expression, used once, at the point it is passed. That is rule 2, and it is
the only place in `std/` or `examples/` where rule 2 is exercised at all.

---

## 3. A second name versus a keyword — one rule, applied to half the codec

The language made a deliberate choice here and `docs/one-way-audit.md` §11
records it as settled:

> `strip(side=)`, `split(side=)`, `find(reverse=)` and `json.stringify(indent=)`
> are one operation each with a named parameter, which is the right shape.

and the rule that generalises it, from the `fetch`/`stream` argument:

> **A parameter changes what a function does; a second name is for when it
> changes what the caller must do next.**

That rule is right and I am not reopening it. What I am reporting is that one
module applies it to one direction of a codec and not the other.

### The situation

`std/http.oro`'s percent-codec is four public functions:

```python
http.quote(s, safe="/")        # a path:        space -> %20
http.quote_plus(s, safe="")    # a query value: space -> +, + -> %2B
http.unquote(s, plus=false)    # the inverse of both
http.encode_query(params)
```

`quote` and `quote_plus` are **two names**. `unquote` is **one name and a
keyword**. They are the two halves of one codec, in one section of one file,
and the same question — "is a `+` a space here?" — is answered two different
ways.

### Which is right?

The module's stated reason is compatibility:

> The names are `urllib.parse`'s, and so is the behaviour, byte for byte:
> `quote`, `quote_plus`, `unquote`. Matching an encoder that every caller has
> already met is worth more than a better name would be…

That is a good argument, and it does not survive contact with the fact that
**`urllib.parse.unquote_plus` also exists.** Oro shipped three of urllib's four
names and replaced the fourth with a keyword. So the compatibility argument was
already overridden once, in this section, by the language's own rule — and then
not applied to the other half.

By the language's own rule, the encode direction is the same shape as
`strip`/`lstrip`:

```python
http.quote(s, safe="", plus=true)     # would replace quote_plus(s)
```

### The ergonomics test

`quote_plus` has three real call sites outside the corpus that tests it, all
three inside `encode_query` (`std/http.oro:1767`, `:1770`, `:1772`):

```python
# today
key = quote_plus(_query_bytes(k, "key")).to_str()
out.append(key + "=" + quote_plus(_query_bytes(v, "value")).to_str())

# after
key = quote(_query_bytes(k, "key"), safe="", plus=true).to_str()
out.append(key + "=" + quote(_query_bytes(v, "value"), safe="", plus=true).to_str())
```

That is worse. Materially worse: it is 22 characters longer per call on a line
that is already long, and `safe=""` — which is not the *point* of the call, it
is the default `quote_plus` already has — now has to be written out every time,
because `quote`'s default is `"/"` and a query value with a `/` in it must not
keep it. Two defaults differ between the two functions, not one, and merging
them means every caller of the common case types both.

This is `rsplit` again: correct on paper, and it makes the call site do the
work.

**Verdict: keep `quote_plus`, and write down why.** It is a second name because
it carries a *different default for a different part of the URL*, which is the
thing the module's own header says is the hard part — *"a path segment, a query
value, a form body and a fragment each have their own idea of what may travel
unescaped."* That is a stronger justification than "urllib's names", it is the
module's own argument, and it is the one that generalises: a second name is
right when the two calls disagree about more than one thing.

`unquote(plus=)` is then also right, and for the symmetric reason: decoding has
**no** safe set — there is nothing to leave alone, everything `%HH` comes back —
so the two decode cases differ in exactly one bit, and one bit is a keyword.

**The finding is not a wrong call; it is an unstated one.** The module reads as
though it inherited two shapes, and the next person to add a function to that
section has nothing to follow. One sentence — *two defaults differ, so it is a
second name; one bit differs, so it is a keyword* — settles it and makes both
halves deliberate.

**Confidence: medium.** The verdict is that the tree is right and the reason is
missing, which is a weaker finding than a disagreement. I looked for a third
case to test the rule against and did not find one in the standard library.

---

## 4. `__str__` versus `__repr__` — and a genuine inconsistency in `repr()`

### The situation

The classic overlap, and the tree's second-most-defined dunder pair:

| dunder | definitions across the tree | in `std/` |
|---|---|---|
| `__init__` | 37 | 10 |
| `__repr__` | 24 | 5 |
| `__eq__` | 13 | 0 |
| `__str__` | 10 | 5 |

Every class in `std/http.oro` that defines either defines **both**, and they
differ only by decoration:

```python
# std/http.oro:461-465, Response
def __str__(self):
    return f"{self.status} {self.reason_phrase()}"

def __repr__(self):
    return f"<Response {self.status} {self.reason_phrase()}>"
```

The same pattern in `BadRequest` (`:330`), `BadResponse` (`:347`),
`Request` (`:397`) and `Url` (`:1875`). Five classes, ten methods.

### The right spelling

Measured against the binary, the two are not symmetric:

```python
class A:
    def __str__(self):
        return "S"

class B:
    def __repr__(self):
        return "R"
```

| position | `A` (`__str__` only) | `B` (`__repr__` only) |
|---|---|---|
| `print(x)` | `S` | `R` |
| `f"{x}"` | `S` | `R` |
| `f"{x!r}"` | `S` | `R` |
| `[x]` | **`<A object>`** | `R` |
| `{"k": x}` | **`<A object>`** | `R` |
| `repr(x)` | `S` | `R` |

> **`__repr__` covers every position. `__str__` covers four of six.**

So for a class that wants one human rendering — which is nearly every class —
the right single dunder is `__repr__`, and `__str__` is the one to skip.
Define both only when a reader is genuinely owed two renderings: an unquoted
one for a message and an unambiguous one for a container or a debugger.

### The inconsistency

The `A` column has a real defect in it, and it is worth fixing before the
freeze:

```python
print(repr(a))     # S            — repr() fell back to __str__
print([a])         # [<A object>] — the same question, a different answer
```

`repr(x)` and `x` inside a container are the same operation, and Oro answers
them differently for a class that defines `__str__` and not `__repr__`. CPython
answers `<__main__.A object at 0x…>` in both positions, because `repr` never
falls back to `__str__` — only `str` falls back to `__repr__`. Oro has added the
reverse fallback at the top level and not inside a container.

This is the `sorted`/`.sorted()` tuple disagreement from
`docs/one-way-audit.md` §2, one level down: one question, two answers,
depending on where it is asked. I would close it by removing the extra
fallback — `repr()` answers `<A object>` when there is no `__repr__`, matching
CPython and matching what a container already does. The alternative (fall back
inside containers too) makes `[a]` print `[S]`, which loses the quoting that
`repr` inside a container exists to provide.

### What the tree does

Of the five `__repr__`s in `std/http.oro`, **none is exercised anywhere in the
tree.** No program calls `repr()` on a `Request`, `Response`, `Url`,
`BadRequest` or `BadResponse`; none puts one in a list or a dict that is
printed; `!r` appears zero times in `std/`. The five `__str__`s *are*
load-bearing — `f"{e}"` in `_try_read` (`std/http.oro:1230`, `:1233`) and
`_dispatch` (`:1250`, `:1252`) turns a `BadRequest` into a status line's
message.

**Verdict.** The rule is `__repr__` by default, both only when two renderings
are owed. `BadRequest` and `BadResponse` owe two — the `__str__` *is* the
message the client sees, and `<BadRequest ...>` must not be. The other three
are decoration: `Request.__repr__` differs from `Request.__str__` by two angle
brackets and a class name, is never called, and is the kind of method that
exists because the one above it did.

I would not remove them — five unexercised methods is a small tax and a
`__repr__` on a public type is defensible on its own. But the *rule* is what is
missing, and without it the next class in the file gets a pair too, on no
argument at all.

**Confidence: high** on the rule and on the `repr()` inconsistency, which is
measured; **medium** on whether the three decorative pairs are worth touching.

---

## 5. `__ne__` — a dunder outside the documented set, and the only one that can lie

This is the sharpest single finding in the document and it takes four lines to
state.

The README, `README.md:196-198`:

> a fixed dunder set — `__str__`, `__repr__`, `__eq__`, `__len__`, the
> arithmetic dunders (`__add__`…`__pow__`), and the comparisons
> (`__lt__`/`__gt__`/`__le__`/`__ge__`).

`__ne__` is not in that list. `__ne__` appears **zero times in `README.md`**.

`__ne__` is implemented. `src/vm/mod.rs:6497` dispatches it,
`:6518` reflects to it rather than to `__eq__`, `try_compare_op` takes a
discriminant test on every `!=` specifically so that a class defining `__ne__`
without `__eq__` still reaches it (`src/vm/mod.rs:6394`), and
`corpus/core/44_comparison_dunders.oro:118` oracles it against CPython.

So the "fixed dunder set" has a member the specification does not list. That
alone would be a documentation bug. What makes it a *spelling* question is what
the member does:

```python
class Q:
    def __eq__(self, other):
        return true
    def __ne__(self, other):
        return true

q = Q()
print(q == Q(), q != Q())      # true true
```

`__eq__` alone cannot produce that. Oro negates it: a class with `__eq__` and
no `__ne__` answers `!=` as `not __eq__`, verified. `__ne__` is the only way to
make a class say that two values are both equal and unequal.

### The right spelling

> **Define `__eq__`. Never define `__ne__`.** `!=` is the negation of `==`, the
> VM computes it that way, and a class that answers both directly is a class
> that can contradict itself.

### What the tree does

One definition, in `corpus/core/44_comparison_dunders.oro:118`, in a `Ne` class
whose entire job is to demonstrate that `__ne__` is its own dunder and that
containers do not consult it. Nothing else in 120 files defines one, and
`std/` defines no `__eq__` at all.

So the tree agrees with the verdict. The problem is entirely that the rule is
not written anywhere, and that the *list* a reader would check against is
missing the entry. A model asked to write an Oro class with value equality has
no way to learn that `__ne__` exists and should not be used — it will either
not know (fine) or know from CPython and reach for it (not fine, because
CPython's own guidance since 3.0 is the same *don't*, and CPython at least
derives `__ne__` from `__eq__` by default, which Oro also does).

**Verdict: add `__ne__` to the README's list and state the rule next to it.**
The alternative — refusing `def __ne__` the way `__hash__` is refused
(`src/compiler/codegen.rs:1749`) — is available and I do not recommend it: the
behaviour is CPython's, it is oracled, and a class that defines `__ne__`
consistently is merely redundant rather than wrong. A `__hash__` that silently
does nothing was a different problem. But this is the closest call in the
document and I would not argue hard against the refusal.

**Confidence: high** that the README is wrong and the rule is needed; **medium**
on refuse-versus-document.

---

## 6. A record: class, dict, or tuple

### The situation

Three shapes hold a handful of named fields, and all three are in `std/http.oro`
within 200 lines of each other:

```python
# a class — std/http.oro:1280
class _Live:
    def __init__(self, conn):
        self.conn = conn
        self.peer = conn.peer
        self.task = null
        self.busy = false
        self.stopping = false

# a dict — std/http.oro:1382, inside serve
tally = {"accepted": 0, "refused": 0, "drained": 0, "forced": 0}

# a tuple — std/http.oro:1510, inside Router.add
self.patterns.append((method, path.split("/"), handler))
```

Each has exactly the shape the other two could have taken. `_Live` has no
methods beyond `__init__`; it is five fields. `tally` is four fixed keys, never
grown, never iterated by key. `patterns` is a three-field record, stored in a
list and destructured on every read.

### The rules

> **A tuple** is right when the record is built in one place and destructured
> immediately at every read — a multiple return, consumed at the call.
>
> **A dict** is right when the thing is *data*: the caller will index it with a
> computed key, iterate it, or hand it to `json.stringify`.
>
> **A class** is right when the thing has *behaviour* or a *lifetime* — when
> something other than reading its fields happens to it.

### What the tree does

**Tuples.** `std/http.oro` returns a tuple from six functions —
`_HeadParser.request_line` (`:632`), `_ResponseParser.status_line` (`:2088`),
`_split_authority` (`:720`), `_split_host_port` (`:1953`), `_path_and_query`
(`:704`), `_civil_from_days` (`:1005`) — and every one is destructured on the
line that receives it:

```python
method, target, version = p.request_line()          # :763
host, port = _split_host_port(authority, url)       # :1929
year, month, day = _civil_from_days(days)           # :978
```

Never stored, never passed on, never indexed by number. That is textbook and it
is the whole justification for tuples surviving the cut of `set`. No
disagreement.

**The one stored tuple** is `Router.patterns`, and it is the boundary case the
rule is for. It is read in two places, both of which destructure:

```python
# std/http.oro:1538, Router.handler_for
for m, pat, fn in self.patterns:

# std/http.oro:1552, Router.allowed
for m, pat, fn in self.patterns:
    if _match(pat, segs) != null and m not in out:
```

The second binds `fn` and never uses it — the tuple's tax, paid once: you name
every field to get one. That is the signal that a record has outgrown a tuple,
and it is one occurrence, in a three-field record, inside a class that already
exists. I would leave it. A `_Route` class to save one unused binding is five
lines for nothing, and the rule holds: two readers, both destructuring, both
agreeing on the names.

**The class/dict split is right, and the file gives the wrong reason for it.**
`_Live`'s own comment:

> It is a class rather than a dict because `entry.busy = true` in the
> connection loop must not be a dict subscript in the hot path, and because
> the two names are the whole contract.

The performance half is measurably true. Three million read-modify-writes on
this binary:

```
r.n = r.n + 1            0.312s      # attribute
d["n"] = d["n"] + 1      0.680s      # dict subscript
```

2.2× — a real difference, and the same order as the 35% the `for`/`while`
finding turned on.

But as a *reason* it is the wrong one, because it generalises wrongly. `tally`
is mutated on the same hot path, in the accept loop, once per connection
(`tally["accepted"] = tally["accepted"] + 1`, `std/http.oro:1431`), and `tally`
is correctly a dict. If "hot path" were the rule, `tally` would be a class and
`serve` would return an object the caller has to learn.

The reason that actually separates them is the third rule above. `_Live` has a
**lifetime**: it is registered in `live`, its `busy` flag is written by one task
and read by another, it holds a `Task` handle, and it is removed in a `finally`.
`tally` is **data**: four numbers handed to a caller who will print them or
serialize them, and a dict is what `json.stringify` takes.

**Verdict: both are right; the stated reason is wrong and should be replaced.**
This matters more than it looks, because the comment is the only place in the
tree where the class-versus-dict question is argued, and it is what the next
author will copy.

### And the answer on `std/json.oro`

The brief asks whether `std/http.oro` having thirteen classes — up from the
eight `docs/hash-and-equality.md` counted — and `std/json.oro` having none is
principled or accidental.

**Principled, and the rule above is exactly why.**

`json.parse` answers with a `dict`, a `list`, a `str`, an `int`, a `float`, a
`bool` or `null`. Every one of those is a value the frozen core already has, and
a `JsonObject` class wrapping a dict would be a second representation of a thing
the language can already spell — the caller would immediately want to index it,
iterate it and hand it back to `stringify`, which is the definition of *data*.
`json.stringify` takes the same values back. There is no lifetime anywhere in
the module: nothing is open, nothing must be closed, nothing outlives the call.
Two free functions is the whole shape, and `std/json.oro` is 69 lines of which
about 55 are the argument for why the loop underneath is in Rust.

`std/io.oro` has none for the same reason read from the other end: its three
functions operate on objects *someone else* owns. A `class Reader` is precisely
what the io protocol refused, and a `Buffer` is a Rust type whose constructor
lives in the module. Zero classes is the protocol working.

`std/http.oro` has thirteen because HTTP is full of things with lifetimes and
behaviour that the core has no spelling for. Check each against the rule:

| class | behaviour or lifetime | verdict |
|---|---|---|
| `Request` | `text()`/`json()` consume a Reader once | earned |
| `Response` | `close()` owns a socket; body is bytes *or* a Reader | earned |
| `Url` | `authority()`/`host_header()` — two renderings of one parse | earned |
| `Router` | `add`/`dispatch`, and two indexes to keep in step | earned |
| `BadRequest` / `BadResponse` | exceptions; `except` needs a class | earned |
| `_HeadParser` / `_ResponseParser` | a cursor `i` advanced across four methods | earned |
| `_LimitReader` / `_ChunkedReader` + their two response subclasses | Readers: state between `read(n)` calls | earned |
| `_Live` | registered, mutated across tasks, removed in a `finally` | earned |

Thirteen for thirteen. Not one is a bag of fields, and the two that come
closest — `_Live` and `Url` — have a lifetime and a pair of methods
respectively. The difference between the modules is not a difference in taste;
it is that one module returns values and the other returns things.

**Confidence: high.**

---

## 7. Methods versus free functions — settled, and the rule is already written

The brief asks whether there is a rule behind `io.read(r)` being free while
`r.read(n)` is a method, and `http.fetch(...)` being free while `resp.close()`
is a method. There is, it is in `docs/stdlib-server-design.md`, and I could not
find a place in the tree that breaks it.

The design doc states half of it explicitly, about `read_until`
(`docs/stdlib-server-design.md:535`):

> It has to be a method rather than a free function because it must see inside
> the reader's buffer: written over `read(n)` it would either read a byte at a
> time, or over-read past the delimiter with nowhere to put the excess.

and the other half about `io.copy` (`:643`):

> it works on a file, a socket, a `Buffer`, or an Oro class someone wrote this
> afternoon, and it never learned about any of them.

Put together:

> **A method is for what only the receiver's own insides can do. A free
> function is for what can be written against the protocol alone.**

And a second, narrower rule that the http module follows without stating:

> **A function whose subject is a stream is a free function**, because a
> stream is a naming convention and not a type you can add methods to.

Checked against everything in `std/`:

| call | side | why |
|---|---|---|
| `r.read(n)`, `r.write(b)` | method | the fd and the buffer |
| `r.read_until(d, lim)` | method | must see the buffer |
| `r.close()`, `conn.set_timeout(s)` | method | owns the resource |
| `io.read(r, n)`, `io.copy(dst, src)` | free | a loop over `read(n)` |
| `io.buffer(b)` | free | a constructor has no receiver |
| `http.read_request(r)`, `http.write_response(w, …)` | free | subject is a stream |
| `http.write_request(w, …)`, `http.read_response(r, …)` | free | subject is a stream |
| `http.serve_conn(conn, handler)`, `http.serve(addr, …)` | free | subject is a stream/address |
| `http.fetch(…)`, `http.stream(…)` | free | no receiver exists yet — they *make* the Response |
| `resp.close()`, `resp.bytes()/text()/json()` | method | reads `self.conn` / `self.body` |
| `req.header(name)` | method | reads `self.headers` |
| `http.text(s, status)`, `http.json_response(v, status)` | free | constructors for `Response` |
| `http.should_keep_alive(req, resp)` | free | **two** subjects, so neither is the receiver |
| `http.quote/unquote/encode_query` | free | `str`, `bytes` and `dict` cannot take methods |
| `Router.add/dispatch/handler_for/allowed` | method | two indexes as state |

Fifteen rows and no disagreement. `should_keep_alive` is the interesting one,
and it is right: a function of a request *and* a response has no single
receiver, and `req.should_keep_alive(resp)` would claim one.

The one exception the module itself flags is `_ChunkedReader.chunk_size`
(`std/http.oro:937`):

> A method rather than the free function it used to be, purely so that it
> reports through `self.fail` and the subclass gets it for nothing.

That is a method that touches no state but `self.fail`. It is not a violation of
the rule so much as a consequence of §8 below — the failure hook is the reason,
and it is the mechanism that is the problem, not this method.

**Verdict: settled, both earned, no changes.** Worth lifting the two sentences
out of `docs/stdlib-server-design.md` into the README next to the io protocol,
because a reader looking for the rule currently has to find it in a 3,050-line
design document, inside a subsection about `read_until`.

---

## 8. Three mechanisms for varying one behaviour, in one file

### The situation

`std/http.oro` parameterises "which exception does this raise, and what noun
goes in the message" **three different ways**:

**(a) A function passed as an argument.** Three `_fail_*` free functions
(`std/http.oro:361-370`) and a `where` string threaded beside them:

```python
def _percent_decode(b, plus_is_space, fail=_fail_request, where="the request target"):
def _path_and_query(target, fail, where):
def _parse_query(qs, fail=_fail_request, where="the request target"):
def _content_length(s, fail=_fail_request):
```

**(b) An overridable method on a subclass.** `_HeadParser.fail` (`:598`)
overridden by `_ResponseParser.fail` (`:2058`); `_ChunkedReader.fail` (`:876`)
by `_ResponseChunkedReader.fail` (`:2216`); `_LimitReader.truncated` (`:843`) by
`_ResponseLimitReader.truncated` (`:2208`).

**(c) A duck-typed attribute on an object handed in.** `serve_conn`'s `watch`
(`:1191`), documented in the file as a second naming-convention protocol:

> `watch`, if given, is anything with two attributes, in the same
> naming-convention-not-a-declared-type spirit as the io protocol.

`docs/one-way-audit.md` §12 spotted (a) and (b) and filed it as low priority.
Having read the file, I think the priority is right but the framing is wrong:
these are not two mechanisms for one job, they are **three mechanisms for three
jobs that happen to look alike**, and the line between them is drawable.

### The rules

> **Inheritance** when the variants share a body. `_HeadParser.headers()` is 40
> lines, it is identical in both directions, and the only thing that differs is
> which exception it raises. That is what an overridable hook is for, and the
> file says so.
>
> **Duck typing** when the collaborator is supplied by a *different* module or
> a caller. `watch` is `serve`'s object handed to `serve_conn`; a declared type
> would couple the two functions that the module deliberately keeps separable,
> and the io protocol already established the convention.
>
> **A function argument** when there is no object to hang it on — a free
> function with no receiver.

Under that reading, (a) is not a design choice at all. It is what is left when a
free function needs to vary and has no `self`. Its existence is downstream of
§7's "the subject is a stream" rule, which correctly made `_path_and_query` and
`_percent_decode` free functions.

### Where it is genuinely wrong

The mechanisms are fine. The *plumbing* is not, and it shows up as §9.

Also worth naming: two hooks in the same family have different names and
different shapes. `_ChunkedReader.fail(msg, status)` raises;
`_LimitReader.truncated()` returns `b""`. Both are "the subclass says what a
broken body means". The asymmetry is defensible — `truncated` is asked *for a
value* the base class returns, and its request-side answer is genuinely "act
like EOF" — but a reader meeting the second one after the first has to work
that out. One sentence in `_LimitReader` would do it.

**Confidence: medium.** This is the section I am least sure about. The rule
above is a reconstruction, not something the file states, and I did not find a
fourth case to test it against.

---

## 9. Defaults that no call site ever takes

A small finding with an unusually clean fix, and the sharpest evidence for §1.

Three sibling helpers in `std/http.oro` take the same `fail`/`where` pair, in
three different shapes:

```python
def _percent_decode(b, plus_is_space, fail=_fail_request, where="the request target"):   # :540
def _path_and_query(target, fail, where):                                                # :699
def _parse_query(qs, fail=_fail_request, where="the request target"):                    # :735
```

One has no defaults. Two have defaults. Now count the call sites:

```python
# _percent_decode — five calls, all of them pass fail and where
std/http.oro:702   _percent_decode(target, false, fail, where)
std/http.oro:703   _percent_decode(target[0:q], false, fail, where)
std/http.oro:742   _percent_decode(pair, true, fail, where)
std/http.oro:744   _percent_decode(pair[0:e], true, fail, where)
std/http.oro:745   _percent_decode(pair[e + 1:], true, fail, where)

# _parse_query — one call, and it passes both
std/http.oro:704   _parse_query(target[q + 1:], fail, where)
```

**Six call sites, zero of which take the default.** The defaults on
`_percent_decode` and `_parse_query` are dead: they document a behaviour no
caller uses, they make two of three siblings look optional when they are not,
and they are the reason the two functions do not look like the third.

`_content_length(s, fail=_fail_request)` (`:817`) is the one where the default
is live — taken at `:814`, overridden at `:2193` — and it is correctly shaped.

**Verdict: drop the dead defaults, and make the three siblings one shape.** A
default that no caller takes is a lie about the function's contract, and the
disagreement between `_path_and_query`'s honest signature and its two
neighbours' is exactly the "two authors, two shapes" failure this document is
about.

While there: `plus_is_space` is a bare positional boolean at five call sites.
`_percent_decode(pair, true, fail, where)` requires the reader to go and look up
what `true` means, and the module's own public surface already solved this — it
is `unquote(s, plus=false)` one screen away. Give it a default of `false` and
pass `plus_is_space=true` at the two query sites, or rename the parameter
`plus` to match the public one.

**Confidence: high.** This is mechanical and has no ergonomic cost — the
replacement is shorter at three of five sites.

---

## 10. Sharing mutable state — settled, unanimously, and silently

The brief asks whether block scope plus `global` plus read-only closures leaves
two ways to share state. It leaves **five**, and the tree has already picked
one without anyone writing it down.

### The situation

Block scope covers `if`/`for`/`while`/`try` with no declaration syntax, so a
result leaving a block must be bound first. `global` exists; `nonlocal` does
not; closures capture by read only — verified:

```python
def make():
    n = 0
    def bump():
        n = n + 1        # RuntimeError: local variable referenced before assignment
        return n
    return bump
```

So a function that must change something outside itself has five spellings:

```python
global count                     # 1. a module global
box[0] = box[0] + 1              # 2. a one-element list as a cell
state["n"] = state["n"] + 1      # 3. a dict
entry.busy = true                # 4. an object field
ch.send(value)                   # 5. a channel
```

All five work.

### The rules

> **An object field** is the answer for state two functions or two tasks share.
> It has a name, the name is checked, and the object says what the state is
> about.
>
> **A dict** when the keys are computed — a registry, a cache, a tally handed
> back to a caller.
>
> **A channel** when the point is the *handover*, not the storage: one task
> telling another that something happened.
>
> **`global`** for a module-level constant read by many functions, and for
> nothing that is written after start-up.
>
> **Never the list-as-a-cell.** `box[0]` names nothing.

### What the tree does

It agrees, completely, and it never says so.

- **`global` appears six times in 120 files, and all six are in
  `corpus/core/16_global.oro`** — the program whose job is to test `global`.
  Zero in `std/`, `examples/`, `bench/` or `tests/`.
- **The list-as-a-cell appears zero times.** `grep '\[0\] = '` over every
  `.oro` file returns nothing.
- **The object field is the shape that carries every piece of cross-task state
  in the standard library.** `_Live.busy` is written by `serve_conn` and read by
  `_drain_live`; `_Live.stopping` the other way; `_Live.task` written by `serve`
  and read by the drain.
- **The dict is the shape for the two computed-key collections.** `live[conn]`
  and `tally`.
- **The channel is the shape for the one handover.** `ready.send(ln)` in
  `serve` (`std/http.oro:1384`), which the module argues for explicitly against
  the `global` alternative:

  > It is the whole shutdown API, and it is deliberately not a module-level
  > `_listener` plus a `shutdown()` the way §3 sketched it — a module global
  > makes `serve` non-reentrant and one-server-per-VM for no gain.

That paragraph is the best short argument against `global` in the tree, and it
is buried in a comment about shutdown.

### The placeholder tax, which is the real cost of block scope

Carrying a value out of a block needs a binding first, and `std/http.oro` pays
it six times. The canonical instance is also the clearest, and the file
comments on it:

```python
# std/http.oro:1390-1402, inside serve
# Bound before the `try`, not inside it: a name first bound in a try
# body does not survive the block.
conn = null
closed = false
failed = null
try:
    conn = ln.accept()
except ValueError as e:
    closed = true
...
```

Three placeholders for one `accept()`. There is a second spelling that needs
none, and the module uses it elsewhere — return from inside the `try`:

```python
# std/http.oro:913-917, _ChunkedReader.terminator
# `if/for/while/try` bodies have real block scope, so a name first bound
# inside a `try` does not survive it. Returning from inside the `try` is
# the shape that wants no temporary at all.
def terminator(self):
    try:
        return io.read(self.r, 2)
    except EOFError as e:
        return self.fail("connection ended before a chunk terminator")
```

> **If the block's whole job is to produce a value, return from inside it and
> bind nothing. Use a placeholder only when the block is one step of several in
> a function that continues afterwards.**

`serve`'s accept loop genuinely continues afterwards — it has to count errors,
back off, and go round — so its three placeholders are correct. The rule
separates the two cases cleanly and both instances in the file are on the right
side of it. Nothing to change; the rule needs writing down, next to the README's
block-scope note, which currently states the tax and not the way to avoid it.

**Verdict: settled, both earned, and the tree is already unanimous.** The whole
of the work here is documentation. That makes it the cheapest item in this
document and — because it is the one a newcomer hits on day one — arguably the
highest-value paragraph the README is missing.

---

## 11. `spawn(f, *args)` versus a thunk — settled

```python
spawn(http.serve, addr, handler, ready)      # the varargs form
spawn(() => http.serve(addr, handler, ready))  # a zero-argument lambda
```

Both work. Of 54 `spawn` call sites in the tree, 53 use the first and exactly
one uses the second, in `corpus/divergence/54_type_names.oro:58` (`spawn(() => 1)`), where the
lambda is there to be trivial.

**Verdict: `spawn(f, *args)`.** It is what the README documents, it allocates no
closure, and `spawn(f)` with no arguments is the same shape as `spawn(f, a, b)`
rather than a different one. The tree agrees 53 to 1 and needs no change.

The thunk form is not cuttable and should not be — `spawn` takes a function, a
lambda is a function, and a rule forbidding one kind of function argument would
be arbitrary. It is enough that the idiom is written down.

The one cost of this verdict is §1: the varargs form cannot name a keyword. That
is the argument for fixing `spawn`, not for preferring thunks — a thunk that
exists only to let you write `max_conns=8` is a closure allocated to work around
a missing feature, and the next reader will not know which it was.

---

## 12. Dunders nothing defines

A census of every dunder definition in all 120 `.oro` files:

| dunder | definitions | where |
|---|---|---|
| `__init__` | 37 | everywhere |
| `__repr__` | 24 | 5 in `std/` |
| `__eq__` | 13 | 0 in `std/` |
| `__str__` | 10 | 5 in `std/` |
| `__lt__` | 7 | 0 in `std/` |
| `__add__`, `__sub__`, `__mul__`, `__gt__`, `__le__`, `__len__`, `__ne__` | 1 each | 0 in `std/` |
| `__truediv__`, `__floordiv__`, `__mod__`, `__pow__`, `__ge__` | **0** | — |

All five of the zero row work — verified by defining and calling them. They are
live features with no users, which is the shape `docs/one-way-audit.md` §5 used
to cut four exception classes.

I do **not** recommend cutting them, and the reason is different from the
exception-class case. `__add__` through `__pow__` and `__lt__` through `__ge__`
are *sets*: they are the arithmetic operators and the comparison operators, and
a set with a hole in it is worse than a set with an unused member. A `Money`
class that can do `+`, `-` and `*` but not `/` is a worse language than one
where all four work and three of them happen to be unexercised in this tree.
Contrast `LookupError`, which is not one of a set — it is an intermediate node
in a hierarchy, and an intermediate node with no children in use is just a node.

The genuinely interesting row is `__len__`, defined **once** in the whole tree,
in `corpus/core/19_class_features.oro:67`. `docs/one-way-audit.md` §2 keeps the
`len(x)` builtin alongside `.len()` partly on the grounds that it is *"the
dispatch point for the `__len__` dunder"*. That argument is now standing on a
dunder that one corpus program defines and nothing else in the language uses. It
does not overturn the verdict — the other half of that argument, that `len`
works on `str` and `bytes` which are outside the collection protocol, is
untouched and is sufficient on its own — but it is worth recording that half of
the justification is thinner than it reads.

**Confidence: medium.** The set argument is a judgement call and someone could
reasonably say four unused dunders is four too many for a frozen language.

---

## 13. What is earned — recorded so it is not reopened

**`def` and `=>`.** One takes statements. Not two spellings of one thing; see §2
for the line between them.

**Free functions and methods.** The rule is in
`docs/stdlib-server-design.md` and the tree obeys it in fifteen out of fifteen
places I checked. See §7.

**Tuples for multiple return.** Six in `std/http.oro`, every one destructured on
the receiving line. Correct, and the strongest single justification for tuples
surviving the cut of `set`.

**Duck typing for the io protocol, and inheritance for the two reader
variants.** These are not competing answers; they are stacked. `_LimitReader`
and `_ChunkedReader` are Readers *by convention* — nothing declares them, and
`io.read` and `io.copy` work on them having never heard of them. Then
`_ResponseLimitReader` subclasses `_LimitReader` because it shares a 12-line
`read` and differs in one hook. The right rule: **the protocol is a convention;
inheritance is for two variants that share a body.** Both instances in
`std/http.oro` follow it.

**`watch` as a second naming-convention protocol.** Defensible, argued in the
file, and consistent with the io protocol it cites. A declared type would couple
`serve` and `serve_conn`, which the module deliberately keeps separable so the
corpus can drive `serve_conn` with an `io.buffer()`.

**`quote_plus` as a second name.** §3 — it carries a different default for a
different part of the URL, which is two differences, not one.

**`fetch` and `stream` as two names.** Already settled in
`docs/one-way-audit.md` §11 and still right: the difference is *lifetime*, and a
flag that changes what the caller must do afterwards is a flag that will be
missed.

**Seven parameters on `serve`.** §1 — no smaller shape survives the ergonomics
test, and the problem is the call site, not the signature.

**Class constructors take keyword arguments.** Verified: `http.Response(status=404)`
and `http.Response(404, {}, b"x", reason="Nope")` both work, and a user class
with defaults takes them too. Only an exception class with no custom `__init__`
refuses them (`src/vm/mod.rs:2877`), which is right — its arguments are a tuple,
not parameters.

---

## 14. If only three things happen

1. **Write the call-site rule down, and let `spawn` forward keyword
   arguments** (§1). Eleven parameters in the standard library are named in
   their signatures and passed by name zero times in 120 files, and at the two
   most important call sites the readable spelling is a `TypeError`. This is the
   largest gap between what the language offers and what the tree can write, and
   the enabling half is one line in `do_spawn`.

2. **Add `__ne__` to the README's dunder set with the rule beside it, and fix
   `repr()`'s fallback** (§4, §5). The "fixed dunder set" is missing a member,
   and the missing member is the only one that can make a class answer `true` to
   both `==` and `!=`. Separately, `repr(x)` and `[x]` give different answers for
   a class that defines `__str__` and not `__repr__`, which is one question with
   two answers in one program.

3. **Use a lambda somewhere in the standard library** (§2). `std/` is 2,579
   lines with zero `=>` and zero collection-chain calls, while the README sells
   both as headline features and two comments *inside `std/http.oro`* show a
   lambda the file then declines to write. This is the same finding
   `docs/one-way-audit.md` §7a made about chains, and it is one finding, not
   two: a chain with no lambda in it has nothing to do. Fixing
   `examples/server.oro:64` to a nested `def` (§2) is a good first move for the
   opposite reason — it is the one place the tree reaches for a lambda where a
   `def` is right.
