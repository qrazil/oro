# One way to error: failure and control transfer before the freeze

*Every situation in Oro where failure or non-local control flow can be spelled
more than one way, the spelling that is right for each, and whether the tree
uses it. Analysis only — nothing here has been changed.*

`docs/one-way-audit.md` asked "are these redundant?" and answered it well. This
document asks the sharper question the owner actually asked:

> Several spellings can legitimately coexist. But **for any given situation, one
> of them is right.** Find the situations, name the right spelling for each, and
> check whether the tree actually uses it.

That reframing matters, because most of what follows is not redundancy. `raise
ValueError("bad port")` and `raise BadRequest(msg, 431)` are not two ways to do
one thing; neither are `finally` and a refcount. What *is* wrong is narrower and
worse: places where two spellings of one situation both live in the tree with no
rule choosing between them, and — in three cases — places where the two
spellings do not even agree with each other.

Everything below was checked by running `./target/release/oro` at 0.2.0, not by
reading the parser. Every claim of behaviour has a program behind it, and the
ones that matter are reproduced inline.

---

## 0. The situations, ranked

| # | Situation | Spellings available | Which is right | Tree | Confidence |
|---|---|---|---|---|---|
| 1 | Naming the class of a runtime fault | rendered `String` reclassified by substring; a typed `Step::Raise` | **the typed raise** | **118 : 13 against** | high |
| 2 | Leaving a `finally` block | fall off the end; `return`/`break`/`continue` | **fall off the end; the rest must be a compile error** | clean — by luck | high |
| 3 | Raising a fresh exception | `raise E` · `raise E()` · `raise E("msg")` | **`raise E()` / `raise E("msg")`; `raise E` is not a second thing to allow** | 1 offender | high |
| 4 | Re-raising | bare `raise` · `raise e` | **`raise e`** (already §6 of the first audit) — but see the ambiguity it inherits | 1 offender | high |
| 5 | Choosing *which* exception a shared helper raises | direct `raise` · an overridable `fail` method · a `fail=` callable argument | **the method**; the argument only where there is no receiver | 3 mechanisms live in one file | high |
| 6 | Calling something that never returns | `self.fail(msg)` · `return self.fail(msg)` | **`return self.fail(msg)`**, or better: don't have the shape | split 24 : 1 | medium |
| 7 | Binding the caught exception | `except X:` · `except X as e:` | **bind only if the body reads it** | 7 of 15 in `std/` bind an unused name | high |
| 8 | Catching at a width | one clause per type; `except Exception` | **narrowest clause that names a decision you will make** | correct everywhere | high |
| 9 | Intermediate nodes in the hierarchy | present · absent | **present only if caught at their own width** | 4 dead + 1 phantom | high |
| 10 | Failure as a value or as an exception | sentinel · raise | **the rule the tree already follows, written down** | consistent; one hole | high |
| 11 | Cleanup | `finally` · block scope + refcount | **`finally` for ordering and ownership; scope for lifetime** | correct, 4 of 4 in `std/`+`examples/` | high |
| 12 | Leaving a loop early | `break` · a flag tested after the block | **`break`** | `serve` uses two removable flags | medium |
| 13 | Answering an error status from a handler | `return Response(4xx, …)` · `raise BadRequest(msg, 4xx)` | **return if you are the one writing the response; raise if you are not** | correct, unwritten | medium |
| 14 | Reporting a dead task | re-raised value in `join` · rendered `report` string at drop | **both, and that is the bug** — they carry different information | by construction | medium |
| 15 | Ending the process | `sys.exit(code)` · raise | **`sys.exit` is a raise; there is one spelling** | settled | high |

Sections 15 and 16 record what I expected to find and did not, and what to do
if only three things happen.

---

## 1. The class of a runtime fault is decided by substring-matching English prose

This is the largest failure in the language's error handling, it is
attacker-reachable through the shipped standard library, and it is the one thing
in this document I would fix before anything else.

**Oro has two internal spellings for "this operation failed."**

```rust
// src/vm/mod.rs:1127 — the string channel. 118 call sites.
fn err(&self, message: impl Into<String>) -> VmError {
    Box::new(RuntimeError { message: message.into().into_boxed_str(), … })
}

// src/vm/sched.rs:946 — the typed channel. 13 call sites.
fn raise(&self, class: &str, msg: impl Into<String>) -> Step {
    Step::Raise(self.make_exception_instance(class, vec![Value::str(msg.into())]))
}
```

Both produce a catchable, correctly-typed exception in the program. They differ
in *where the class is named*. The typed one names it at the raise site. The
string one does not name it at all — it renders an English sentence, and five
thousand lines away, at the moment the fault becomes an exception,
`classify_error` guesses the class back out of the sentence:

```rust
// src/vm/mod.rs:6136
fn classify_error(msg: &str) -> &'static str {
    if let Some(kind) = crate::net::classify(m) { return kind; }   // src/net.rs:381
    if let Some(kind) = crate::json::classify(m) { return kind; }  // src/json.rs:45
    if m.starts_with("command failed:")          { "CommandError" }
    else if m.contains("No such file or directory") { "FileNotFoundError" }
    else if m.contains("Permission denied")      { "PermissionError" }
    else if m.contains("timed out")              { "TimeoutError" }
    …
    else if m.contains("division by zero")       { "ZeroDivisionError" }
    else if m.contains("index out of range")     { "IndexError" }
    …
}
```

Three such tables, and the comment above the main one says *"Every message here
is produced by this crate, so the matching is reliable."*

**That sentence is false, and the counterexample is one line of Oro.** Messages
produced by this crate interpolate values produced by the program — and, on a
server, by the client:

```python
"division by zero".to_int()
# -> ZeroDivisionError: invalid literal for int(): 'division by zero'
"No such file or directory".to_int()
# -> FileNotFoundError: invalid literal for int(): 'No such file or directory'
"timed out".to_int()
# -> TimeoutError: invalid literal for int(): 'timed out'
"is not defined".to_int()
# -> NameError: invalid literal for int(): 'is not defined'
"Permission denied".to_int()
# -> PermissionError: invalid literal for int(): 'Permission denied'
"index out of range".to_int()
# -> IndexError: invalid literal for int(): 'index out of range'
{}["No such file or directory"]
# -> FileNotFoundError: key error: 'No such file or directory'
```

Every one of those is a `ValueError` (or a `KeyError`). The class is chosen by
the *contents of the data*.

**Why this is not a curiosity.** Run it through the shipped standard library.
This is the shape every handler written against `std/http.oro` will have:

```python
def user(req):
    try:
        return http.text(f"user {req.params['id'].to_int()}\n")
    except ValueError as e:
        raise http.BadRequest("id must be a number")

routes = http.Router()
routes.add("GET", "/users/:id", user)
```

driven through `http.serve_conn` over an `io.buffer`, exactly as
`corpus/divergence/` drives it:

```
/users/12                                -> HTTP/1.1 200 OK
/users/abc                               -> HTTP/1.1 400 Bad Request
/users/timed%20out                       -> HTTP/1.1 500 Internal Server Error
    http: handler raised <class 'TimeoutError'>: invalid literal for int(): 'timed out'
/users/No%20such%20file%20or%20directory -> HTTP/1.1 500 Internal Server Error
    http: handler raised <class 'FileNotFoundError'>: invalid literal for int(): 'No such file or directory'
/users/Permission%20denied               -> HTTP/1.1 500 Internal Server Error
    http: handler raised <class 'PermissionError'>: invalid literal for int(): 'Permission denied'
```

The client chooses whether it gets a 400 or a 500, by choosing the text of a
path segment. `TimeoutError` is an `OSError`, so the handler's `except
ValueError` does not catch it; it reaches `_dispatch` (`std/http.oro:1241`),
hits `except Exception`, and becomes a 500 with a log line. That is correct
behaviour by `_dispatch` and a wrong answer to the request.

**The same shape is one added site away from being worse on the parse path.**
`_try_read` (`:1226`) catches only `BadRequest` and `ValueError`, so an
`OSError` manufactured this way would escape `serve_conn` into `_conn_task`'s
`except OSError as e: pass` (`:1448`) and **close the connection silently, with
no response and no log line** — the one catch in the file whose comment says it
is for "how connections end". I could not find a reachable site in today's
parser: `_content_length` (`:817`) validates digits before `to_int`, and every
other parse fault is an Oro-level `raise BadRequest` that never passes through
`classify_error` at all. The parser is safe today by good luck rather than by
construction, and the luck is one `.to_int()` deep.

**The second symptom: faults that land on `RuntimeError` because no arm
matched.** These are CPython-typed faults in a language whose README promises
*"Every runtime fault … raises the same exception type CPython uses, so it is
catchable"*:

```python
[1, 2, 3][0:"a"]        # RuntimeError: indices must be integers, not 'str'
                        # CPython: TypeError
f"{1:Q}"                # RuntimeError: Invalid format specifier 'Q'
                        # CPython: ValueError
(5).to_bytes()          # RuntimeError: 'int' object has no conversion to bytes
{"a": 1}.map((k, v) => v)
# RuntimeError: rebuilding a dict needs (key, value) pairs, not 'int'
```

The last one the README already knows about and prints as a known wart
(`README.md:300`). It is not a wart in `map`. It is `classify_error` having no
arm that matches, and it will recur for every message anyone adds.

**The third symptom, and the tell that the two channels have already
collided.** `src/value.rs:1530` exists only to paper over the difference:

```rust
/// "argument" is an already-rendered *message*, not a constructor argument.
/// A runtime fault travels as a `String` — `key error: 'nope'` — and is turned
/// into an exception instance at the point it is raised, by which time the key
/// itself is gone and only its rendering survives.
pub const RENDERED_MESSAGE: &str = "\u{0}rendered";
```

The consequence is observable from Oro. **The same class, raised the two ways,
has different `args`:**

```python
try:    {}["user"]
except KeyError as e:   print(repr(e.args))   # ("'user'",)   <- quotes baked in

try:    raise KeyError("user")
except KeyError as e:   print(repr(e.args))   # ('user',)
```

`e.args[0]` is the missing key when a program raised it and the *repr* of the
missing key when the VM did. CPython gives `'user'` in both. A handler that does
`except KeyError as e: return _error_response(400, f"missing field {e.args[0]}")`
prints `missing field 'user'` or `missing field user` depending on who raised.

**Which spelling is right.** The typed one. The class is a decision, it is made
where the fault is detected, and there is nothing about `"invalid literal for
int()"` that a reader of `to_int` should have to look up in a table in another
file to learn is a `ValueError`.

**Check the tree.** 118 `self.err(` against 13 `self.raise(` — and the
distribution is the whole argument for the direction of the fix: **twelve of the
thirteen typed sites are in `src/vm/sched.rs`**, the newest file in the VM.

I did not have to infer why. The typed helper's own doc comment makes this
document's argument, and scopes it to new code:

```rust
// src/vm/sched.rs:940
/// Raise `class` with `msg`.
///
/// The concurrency surface names its exception classes rather than
/// spelling a message that `classify_error` will recognise. Everything
/// here is new, so there is no reason to route a brand-new diagnostic
/// through a substring table built for the old ones.
```

That is the verdict, already written down, already implemented, and already
fenced off to one file. The string channel is not the chosen design — it is the
pre-existing one, and the only question left is whether the fence stays where it
is when the language freezes. It should not: "there is no reason to route a
brand-new diagnostic through a substring table" is not an argument about
newness, it is an argument about substring tables, and it applies with more
force to the 118 old sites than to the 13 new ones, because the old sites are the
ones whose messages carry user data.

**The ergonomics test.** At a real site:

```rust
// today
return Err(self.err(format!("invalid literal for int(): {}", s.repr())));
// after
return Ok(self.raise("ValueError", format!("invalid literal for int(): {}", s.repr())));
```

One word longer, and `classify_error`, `net::classify` and `json::classify` — 130
lines of substring guessing across three files — all delete.

**Cost, named honestly.** 380 `return Err(…)` sites across `src/` return a bare
`String` to a caller that has no `&self` to raise from; `VResult<T> =
Result<T, String>` is the signature of every native builtin, every stream method
and every `_pct`/`_json` entry point. Converting all of them is a large
mechanical change to the Rust/builtin boundary, not a small one. The cheap
version that gets 90% of the value: change `VResult`'s error type from `String`
to `(&'static str, String)` — a class and a message — defaulting to
`("RuntimeError", msg)` where no one has picked yet, and delete the three
classify tables as each caller is converted. Until then, the two sites that
carry attacker data through `classify_error` (`to_int`'s literal and `KeyError`'s
key) should be raised typed today; that alone closes the reachable hole.

**Confidence: high** on the diagnosis and the direction, **medium** on the
migration being worth doing in one go before 1.0 rather than incrementally.

---

## 2. `return`/`break`/`continue` inside `finally` silently discard an in-flight exception

```python
def swallow():
    try:
        raise ValueError("lost?")
    finally:
        return "returned from finally"

print(swallow())        # 'returned from finally'. The ValueError is gone.
```

```python
def swallow_break():
    for i in range(3):
        try:
            raise ValueError("lost too?")
        finally:
            break
    return "broke out"

print(swallow_break())  # 'broke out'. Also gone.
```

Both run. Neither warns. This is CPython's behaviour, faithfully inherited —
and CPython itself has since decided it was a mistake: PEP 765 makes
`return`/`break`/`continue` in a `finally` a `SyntaxWarning` in 3.14, on the
grounds that it is always either a bug or better written another way.

Oro is about to freeze, has no legacy to protect, and already makes a **compile
error** out of much smaller traps — `def __hash__`, a tab in leading
indentation, `dict = {}`. This is a larger trap than any of them: it is the one
construct in the language that can make a `raise` have no effect at all, and it
does so invisibly, in the block whose entire purpose is that it always runs.

**Which spelling is right: fall off the end of the `finally`.** A `finally` is
for the statements that must happen on every path. A path decision does not
belong in it, because by the time it runs there is already a path in flight and
the only thing a `return` there can do is cancel it.

**Check the tree.** Nothing in `std/`, `examples/`, `bench/` or `corpus/` writes
a `return`, `break` or `continue` inside a `finally` body. `corpus/core/25_break
_continue_finally.oro` tests `break` and `continue` *inside the `try`*, which is
the legitimate case and must keep working — the whole file is the proof that
they run the `finally` on the way out. The rule to enforce is narrow and the
corpus already respects it:

> A `return`, `break` or `continue` whose nearest enclosing block is a `finally`
> body is a compile error. The same statements in the `try` or an `except` are
> fine, and run the `finally` on their way out.

**One related behaviour I checked and would leave alone.** A `raise` inside a
`finally` replaces the in-flight exception:

```python
try:
    raise ValueError("first")
finally:
    raise TypeError("second")       # TypeError wins; the ValueError is gone
```

That is also lossy, and it is also CPython's answer — but unlike `return` it is
*visible*: a `raise` in a cleanup block is a thing a reader can see going wrong.
Oro has no exception chaining (`__context__`) to preserve the first one with, so
the alternatives are "keep CPython's rule" or "invent chaining before the
freeze". Keep the rule.

**Confidence: high.**

---

## 3. `raise E` and `raise E()` are the same statement

The first audit looked at bare `raise` versus `raise e` and stopped. There is a
second pair underneath, and unlike `raise X()` versus `raise X("msg")` — which
are genuinely two different things and were rightly left alone — these two are
byte-for-byte identical:

```python
raise ValueError       # ValueError()  args ()  str ''
raise ValueError()     # ValueError()  args ()  str ''
```

Same class, same `args`, same `repr`, same `str`. There is no case one covers
and the other does not.

**Which is right: `raise E()`.** Everywhere else in Oro a class name in
expression position is a *value* and a call constructs. `type(p) == Point` is
the type test precisely because `Point` is the type rather than something that
makes one. `raise E` is the single place in the language where naming a class
implicitly calls it.

**And the implicit call is not merely inconsistent — it makes `raise <name>`
ambiguous.** The statement means two different things depending on the runtime
type of its operand:

```python
# std/http.oro:1411 — `failed` holds an instance. This RE-RAISES it.
raise failed

# corpus/core/21_exception_features.oro:102 — a class. This CONSTRUCTS one.
raise RuntimeError
```

and a name settles nothing, because a name can hold either:

```python
E = ValueError
raise E                 # ValueError()  — constructed
x = ValueError("held")
raise x                 # ValueError('held') — the same object
```

That matters more after the first audit's §6 cut lands. Once bare `raise` is
gone, `raise e` becomes the *only* way to re-raise, and the tree will grow
`raise <name>` sites — every one of which a reader has to trace to a binding to
learn whether it is re-raising something or minting something.

**Verdict: require the call.** `raise <expr>` raises the instance `<expr>`
evaluates to; a class there is a `TypeError` naming `E()`. Combined with the
existing refusal (`raise "oops"` already answers *"exceptions must derive from
BaseException, not 'str'"*), the rule becomes one sentence: **`raise` takes an
exception instance.**

**The ergonomics test.** `std/http.oro:1411` (`raise failed`) is unchanged — it
already raises an instance. The only site in the tree that moves is
`corpus/core/21_exception_features.oro:102`, and it moves by two characters:

```python
raise RuntimeError      # today
raise RuntimeError()    # after
```

That file is `corpus/core/`, so it is CPython-oracled and `raise RuntimeError`
is valid Python. Cutting it costs one oracled line — the same trade §1 of the
first audit takes for `str.join`, at a hundredth of the volume, and a `.twin.py`
is not even needed because `raise RuntimeError()` is *also* valid Python and
prints the same thing. This cut is free.

**Confidence: high.**

---

## 4. `except X:` versus `except X as e:` — the standard library always binds

Both are legal. `corpus/core/21_exception_features.oro:39` writes `except
Exception:`; every other handler in the tree writes `as e`.

**Which is right: bind if and only if the body reads it.** The binding is the
handler's one piece of documentation — it tells a reader, in the header, whether
this handler is going to look at what went wrong or merely note that it did.
Binding unconditionally throws that away, and it is the same argument Oro
already made against `is`: two similar spellings whose difference is invisible
at the call site are worse than one.

**Check the tree, and `std/http.oro` is the offender.** Seven of its fifteen
handlers bind a name they never read:

| site | handler | uses `e`? |
|---|---|---|
| `std/http.oro:781` | `except ValueError as e:` → `raise BadRequest(f"header block exceeded {_MAX_HEAD} bytes", 431)` | **no** |
| `std/http.oro:916` | `except EOFError as e:` → `return self.fail("connection ended before a chunk terminator")` | **no** |
| `std/http.oro:1138` | `except BadRequest as e:` → `return false` | **no** |
| `std/http.oro:1395` | `except ValueError as e:` → `closed = true` | **no** |
| `std/http.oro:1397` | `except ConnectionError as e:` → `conn = null` | **no** |
| `std/http.oro:1448` | `except OSError as e:` → `pass` | **no** |
| `std/http.oro:2144` | `except ValueError as e:` → `raise BadResponse(…)` | **no** |
| the other eight | | yes |

The two most interesting are `:781` and `:2144`: both
catch a `ValueError` from `read_until`'s limit and translate it into a
`BadRequest`/`BadResponse` — and both *discard the original message* while
binding it. Dropping the `as e` there says out loud what the code does, which is
that the original diagnostic is deliberately replaced (correctly: `read_until()
found no delimiter in the first 8192 bytes` is an implementation detail and
"header block exceeded 8192 bytes" is the fact).

**The ergonomics test.** Seven deletions of four characters each, no other change.

**Confidence: high** on the rule, **high** on the census (counted by hand from
the bodies, listed above so it can be rechecked).

---

## 5. The hierarchy: four dead classes, one phantom, and what a node is for

The first audit's census said `LookupError`, `ArithmeticError`,
`NotImplementedError` and `StopIteration` are raised 0 times and caught 0 times,
plus `BaseException` which earns its place structurally. **I re-ran it. All five
verdicts hold. Two of them need a footnote, and the census missed a sixth class
with the opposite problem.**

| class | raised | caught | *constructed* | verdict |
|---|---|---|---|---|
| `LookupError` | 0 | 0 | **1** (`corpus/core/46_exception_str.oro:75`) | **dead — cut** |
| `ArithmeticError` | 0 | 0 | 0 | **dead — cut** |
| `NotImplementedError` | 0 | 0 | 0 | **dead — cut** |
| `StopIteration` | 0 | 0 | **1** (`corpus/core/46_exception_str.oro:80`) | **dead — cut, and fix the README** |
| `BaseException` | 0 | 0 | 0 | **keep — structural** |

**Footnote one: two of the four are not entirely unreferenced.**
`corpus/core/46_exception_str.oro` builds `LookupError("l")` and
`StopIteration("s")` in a list that proves every class in the family has a plain
`__str__`. That is not a raise and not a catch, but it is a compile-time name
reference in a CPython-oracled program, and the cut breaks it. The cost is two
list entries in one file; the file's point is *"nothing else in the hierarchy
reprs anything"*, which survives losing two of its ten witnesses. Name it rather
than discover it.

**Footnote two: there is a sixth class, and it has the opposite problem.**
`RecursionError` does not exist in `src/vm/exceptions.rs` — and four
places in the tree say it does:

```
src/value.rs:1593       "…is what finally turns a cycle into a RecursionError"
src/json.rs:36          "…and raises RecursionError at 20 000"
src/vm/mod.rs:575       "…with `RecursionError: maximum recursion depth exceeded`"
corpus/core/44_comparison_dunders.oro:283
                        print("--- a cycle is a RecursionError, not a hang…")
```

and the line four lines below that last one catches `RuntimeError`, because
that is what actually arrives:

```python
x = []; x.append(x); y = []; y.append(y)
try:    print(x == y)
except RuntimeError as e:   print(e)    # maximum recursion depth exceeded
```

CPython's `RecursionError` is a subclass of `RuntimeError`, so catching
`RuntimeError` works in both languages and the corpus program is *correct*. What
is wrong is four comments describing a class that is not there — the same
protecting-each-other failure the first audit found between `StopIteration` and
the README, one layer down. Either add `RecursionError` under `RuntimeError`
(which makes those four sentences true and gives a program a way to catch a
runaway recursion without also catching every uncategorised internal fault) or
correct the four sentences. **I would add it**, and it is the one class in this
section I would add rather than cut: unlike the four dead ones it has a fault
that actually occurs, a place that actually raises it, and — once §1 is fixed —
no other way to name that fault, because `RuntimeError` is where everything
unclassified lands.

**Should an intermediate node that nothing catches at its own width exist?**
No, and the contrast inside this one file is the whole argument:

```python
# std/http.oro:1397 — ConnectionError, caught at its own width, never raised there
except ConnectionError as e:
    conn = null
# std/http.oro:1448 — the whole OSError subtree, deliberately
except OSError as e:
    pass
```

An intermediate node's job is to be a *catchable width*. `ConnectionError` is
never raised at its own width and earns its place entirely by being caught at
it. `LookupError` is neither raised nor caught at its own width, and there is no
program for which "I want to handle a missing key and an out-of-range index the
same way, and nothing else" is a real requirement — the two arise from different
mistakes and want different fixes. `ArithmeticError` is worse: it has exactly one
child and no second one coming, because Oro promotes to bignum (so `OverflowError`
is structurally impossible) and has no `FloatingPointError`. A node with one child
is not a hierarchy, it is an alias.

So the rule to write down is:

> An intermediate exception class exists to be caught at its own width. If no
> program would write `except ThatName`, it is not a node — it is a synonym for
> its child, and the child is the name to use.

which `ConnectionError` and `OSError` pass, `ImportError`/`ModuleNotFoundError`
scrapes (the first audit's §10, "merge, weakly" — I agree and have nothing to
add), and `LookupError`/`ArithmeticError` fail.

**Verdict: the first audit's cut stands.** 29 classes to 25, reparent `KeyError`
and `IndexError` under `Exception` and `ZeroDivisionError` under `Exception`;
add `RecursionError` under `RuntimeError` and it is 26. Fix the README's
generator sentence (`README.md:205`), which promises a `StopIteration` on
exhaustion that never arrives, and fix or realise the four `RecursionError`
comments. Give `except LookupError` and `except ArithmeticError` the same
naming-the-replacement `NameError` the cut string methods get.

**Confidence: high.**

---

## 6. Three mechanisms for "which exception does this raise", in one file

`std/http.oro` parameterises the failure type three ways. The first audit found
two of them (§12) and called it low priority. It is not low priority — it is the
richest single-situation ambiguity in the tree, and it is in the module the
language is shipping as its showpiece.

**Spelling A — raise it directly.** 50 sites.

```python
# std/http.oro:686
raise BadRequest("a fragment must not be sent in a request target")
```

**Spelling B — an overridable `fail` method.** Four definitions
(`_HeadParser.fail` `:598`, `_ChunkedReader.fail` `:876`, `_ResponseParser.fail`
`:2058`, `_ResponseChunkedReader.fail` `:2216`), 25 call sites.

```python
# std/http.oro:612
self.fail("empty request line")
```

**Spelling C — a `fail=` callable argument.** Three free functions
(`_fail_request` `:361`, `_fail_url` `:365`, `_fail_response` `:369`), three
call sites (`:543`, `:819`, `:822`), five pass-through sites.

```python
# std/http.oro:540
def _percent_decode(b, plus_is_space, fail=_fail_request, where="the request target"):
    out = _pct.decode(b, plus_is_space)
    if out == null:
        fail(f"{_escape_fault(b)} percent-escape in {where}")
    return out
```

**Spelling D, which the first audit missed — an overridable hook that returns on
one side and raises on the other.**

```python
# std/http.oro:843, _LimitReader
def truncated(self):
    return b""                                  # a sentinel

# std/http.oro:2208, _ResponseLimitReader
def truncated(self):
    raise BadResponse(f"the response body ended {self.remaining} bytes short…")

# std/http.oro:855 — the one call site, identical for both
if chunk == b"":
    return self.truncated()
```

**Which is right for which situation.** These are not four ways to do one thing;
they are four answers to three genuinely different questions, and only one pair
is redundant.

1. **The failure type is fixed and known here** → **spelling A, a direct
   `raise`.** `read_request` (`:756`) is only ever a server. `parse_url`
   (`:1901`) is only ever a URL the program wrote. There is nothing to
   parameterise and a hook would be indirection with one implementation.

2. **The same code runs on both sides of the wire and the *caller is an
   object*** → **spelling B, the method.** This is the right answer and the file
   mostly knows it: `_HeadParser.headers()` — the load-bearing half of the
   parser, identical in both directions — is inherited verbatim by
   `_ResponseParser` and reports through whichever `fail` the subclass
   installed. The comment at `:593` makes the argument itself. `_ChunkedReader`
   is the same shape, and `chunk_size` was *moved* from a free function to a
   method for exactly this reason (`:935`).

3. **The same code runs on both sides and there is no receiver** → **spelling
   C**, and it exists only because `_percent_decode`, `_path_and_query`,
   `_parse_query` and `_content_length` are free functions. The first audit said
   the method form is the better one. I agree, and I would go further: **the
   three `_fail_*` free functions are a symptom, not a mechanism.** Two of them
   have exactly one caller each (`_fail_url` at `:1932` and `:2341`,
   `_fail_response` at `:2193`), and the third is a default argument in four
   signatures. The honest fix is not to pick B over C — it is to notice that
   `_percent_decode`/`_parse_query`/`_content_length` are the *only* parser
   pieces without a receiver, and that giving them one (a tiny `_Codec` with a
   `fail`, or moving them onto the parser classes that already have it) collapses
   C into B and deletes three functions, one `where=` parameter and four default
   arguments.

4. **Spelling D is not redundant with any of them and is the best thing in the
   file.** `truncated()` is the single place in the tree where the
   sentinel-versus-exception decision itself is parameterised — the base class
   answers `b""` because on the way *in* a short body is a connection to close,
   and the subclass raises because on the way *out* a short body is half of a
   JSON document. The call site `return self.truncated()` is written identically
   for both. That is exactly the right shape, it is load-bearing, and it should
   be kept and pointed at.

**Verdict:** A where the type is fixed, B where there is a receiver, D where the
*kind* of signal differs and not just the class. Cut C by giving its four
functions a receiver. Net: three mechanisms become two, and the two are
distinguished by a rule a reader can apply without reading the module.

**Confidence: high** on the ranking, **medium** on the receiver refactor being
worth the churn this close to a freeze — it touches eight signatures for a
readability win, and if it does not happen, the fallback is one sentence in the
module header saying C exists only for the receiverless four.

---

## 7. A call that never returns, spelled two ways in one class

A consequence of spelling B, and worth its own entry because it is a
*control-transfer* problem rather than a naming one. `fail` raises. It never
returns. But it is called as an ordinary statement:

```python
# std/http.oro:612-619, _HeadParser.request_line
if self.i >= self.n:
    self.fail("empty request line")
parts = self.lines[self.i].split(b" ")      # only reachable because fail raised
```

and, once, as a returned value:

```python
# std/http.oro:914-917, _ChunkedReader.terminator
try:
    return io.read(self.r, 2)
except EOFError as e:
    return self.fail("connection ended before a chunk terminator")
```

Twenty-four sites of the first shape, one of the second. Both are correct today.
The first shape is the one that worries me: it is invisible non-local control
flow. Nothing at the call site says the next line is unreachable, the compiler
cannot know it, and a future `fail` override that *returned* instead of raising
— which is a perfectly natural thing for someone to write, given that the sibling
hook `truncated()` deliberately does exactly that — turns `request_line` into an
`IndexError` on the line after.

**Which is right: `return self.fail(…)`.** It is two extra tokens, it makes the
non-return visible at every call site, and it makes the hook's contract "produce
the value or don't come back", which is the contract `truncated()` already has
and which makes the two hooks the same shape instead of two shapes.

**The ergonomics test.** At the densest run of call sites:

```python
# today                                    # after
if len(parts) != 3:                        if len(parts) != 3:
    self.fail("malformed request line")        return self.fail("malformed request line")
```

Longer, and in `_HeadParser.headers()` (`:640`–`:667`) several of the `fail`
calls are inside a loop where `return` would change what the reader expects to
happen next — which is the point, because a `raise` there *does* leave the loop.

I am **less confident** here than anywhere else in this document. The
alternative — leave the 21 bare calls and write "every `fail` raises" in the
class docstring — is cheap and nearly as good, and 24 rewrites for a hazard that
has not fired is the kind of change that is easy to argue for and hard to
justify. What is *not* defensible is the current 24:1 split, where the one
`return self.fail` at `:917` teaches a reader that the others are different when
they are not. Pick one.

**Confidence: medium.**

---

## 8. Error-as-value versus exception: the rule is right, and it has one hole

The first audit's §8 got this right and I will not relitigate it. The split is
principled — **a lookup that can legitimately find nothing answers with a value;
an operation that was asked to do something and could not, raises** — and the
evidence is that the crossings run one way only. Verified against the binary:

```python
"abc".find("z")                 # -1
b"abc".find(b"z")               # -1
[1, 2, 3].find(x => x > 5)      # null
{"a": 1}.find((k, v) => v > 5)  # null
{}.get("k")                     # null
{}.pop("k")                     # KeyError: 'k'
```

`_pct.decode` returning `null` is the same rule applied one level down, and its
own docstring makes the argument better than I can:

> Refused here is `null`, and not a raise, because **which** exception it
> becomes is policy and policy is Oro's: the same bad escape is a 400 on the way
> in and a `ValueError` for a URL the program itself got wrong… A codec that
> raised would have had to pick one of those answers for both callers.

That is consistent with `find` and with everything else: the codec is the thing
that knows *whether* it decoded and not *what that means*, which is exactly the
lookup case. It is also the same insight as `_LimitReader.truncated` (§6.4) from
the other direction — one defers the class, the other defers the *kind* of
signal — and the two should be documented together as the language's answer to
"I can detect the fault but I cannot name it."

**The hole: `null` as a sentinel collides with `null` as a value.**

```python
xs = [1, null, 3]
xs.find(x => x == null)     # null   — found it
xs.find(x => x == 99)       # null   — didn't find it
```

Indistinguishable. `dict.get` has the same collision (`{"a": null}.get("a")` and
`{}.get("b")` are both `null`) — but `dict` has `in`, which answers the question
`get` cannot, and `std/http.oro` uses exactly that pairing in `_has_token`
(`:572`) and `should_keep_alive`. **`list.find` has no `in` for a predicate.**
There is no spelling of "did the predicate match anything" that does not go
through the sentinel.

This is not a redundancy and it is not urgent — `-1` has the same hole in
principle and does not in practice, because an index is never negative, which is
precisely why `find` on a string can use it. It *is* a place where the stated
rule ("a lookup answers with a value") has a case it cannot express, and a
frozen language should say so rather than leave it to be found. One sentence in
the README next to the `find` note: **`find` on a collection answers the element
or `null`; a collection that may contain `null` needs `.any(pred)` to ask
whether, and `.filter(pred)` to ask which.**

**Where I checked for a violation and found none.** Nothing in `std/` returns
`false` or `-1` to mean "an operation failed". `_drain` (`:1135`) returns
`false`, and that is not a failure — it is the answer to "is this connection
worth reusing", which is a question with two legitimate answers. `read_request`
returning `null` at a clean EOF is likewise the lookup case: there was no
request, which is what a closed keep-alive connection looks like and is not a
fault. The one conversion *down* from exception to sentinel in that file
(`_drain`'s `except BadRequest as e: return false`) has a two-line comment
explaining that a desynchronised stream and an over-large body need the same
answer. That is the rule being applied, not broken.

**Verdict: keep both, write the rule down, name the `null` hole.**

**Confidence: high.**

---

## 9. `finally` versus block scope plus refcounting — the earlier conclusion holds

The first audit expected these to be redundant and found they are not:
`finally`'s job is **ordering and ownership transfer**, not lifetime. I re-checked
all four non-corpus sites and the conclusion holds. It is worth restating more
precisely, because "ordering and ownership" is two different jobs and each site
does exactly one of them.

**Ownership transfer — `stream` (`std/http.oro:2280`).** The load-bearing case.

```python
handed_over = false
try:
    …
    resp.conn = conn
    handed_over = true
    return resp
finally:
    if not handed_over:
        conn.close()
```

A refcount cannot distinguish "the frame ended" from "the frame ended having
given the socket away", because in both cases the frame's reference dies. The
`handed_over` flag *is* the distinction, and `finally` is the only construct
that can read it on every exit path. Nothing else in the language expresses
this. The comment at `:2296` already says why a catch-per-exception-type is the
wrong alternative — *"the list of ways a socket can fail is not a list this
function wants to keep current"* — and that is right.

**Ordering — `fetch` (`:2249`).**

```python
resp = stream(…)
try:
    resp.body = _read_capped(resp.body, max_body)
    return resp
finally:
    resp.close()
```

The close must happen *after* the return value is computed and *before* the
frame leaves. Destruction of a returned value cannot be scheduled there by any
scope rule, because the value is escaping.

**Ordering, again, with an external observer — `_conn_task` (`:1445`).**

```python
finally:
    entry.conn.close()
    live.pop(entry.conn, null)
```

`live` outlives the block and holds the entry, so the refcount never drops. The
comment adds the part that makes it a *scheduling* argument rather than a
lifetime one: *"the last statement of the task, with no park point after it, so
under cooperative scheduling 'gone from the registry' and 'the task has
finished' are the same instant to every other task in the VM."* That is a
statement about ordering relative to other green threads, which refcounting has
nothing to say about.

**The caller's side — `examples/client.oro:99`.**

```python
try:
    copied = io.copy(sink, resp.body)
finally:
    resp.close()
```

This is the ownership transfer of `stream` seen from the other end, and it is
the price the example advertises in its own comment. Correct.

**Where is `finally` cargo-culted?** Nowhere, in `std/` or `examples/`. All four
sites do one of the two jobs. The remaining sixteen `finally` mentions in
`corpus/` and `bench/` are `log.append(…)` in programs that exist to *prove*
`break`, `continue` and nested `try` run their `finally` — which is a test, not
a use.

**So the sentence to put in the README** — next to "Oro has no `with` because
refcounting is deterministic", which invites exactly the wrong inference:

> Refcounting decides *when* a thing is released. `finally` decides **in what
> order**, and whether it is released **at all** — the two jobs a scope rule
> cannot do, because a scope cannot know that a value escaped or that another
> task is watching.

**Confidence: high.**

---

## 10. `break` versus a flag: `serve`'s accept loop carries two it does not need

`serve` (`std/http.oro:1374`) binds three variables before its `try` and tests
all three after it:

```python
while true:
    conn = null
    closed = false
    failed = null
    try:
        conn = ln.accept()
    except ValueError as e:
        closed = true
    except ConnectionError as e:
        conn = null
    except OSError as e:
        failed = e
    if closed:
        break
    if failed != null:
        errors = errors + 1
        if errors > _MAX_ACCEPT_ERRORS:
            ln.close()
            _drain_live(live, tally, drain)
            raise failed
        time.sleep(backoff)
        backoff = min(backoff * 2, _ACCEPT_BACKOFF_MAX)
        continue
    if conn == null:
        continue
    …
```

The comment explains one of the three: *"Bound before the `try`, not inside it:
a name first bound in a try body does not survive the block."* That is true of
`conn` and is the block-scope tax the README documents. It is not true of
`closed` or `failed`, which are never assigned in the `try` body at all — they
exist only to carry a decision out of a handler.

**They do not need to.** `break` and `continue` work from inside an `except`
handler; I checked:

```python
i = 0
while true:
    i = i + 1
    try:
        if i == 3: raise ValueError("stop")
        print("tick", i)
    except ValueError as e:
        break
print("out at", i)      # tick 1 / tick 2 / out at 3
```

**Which is right: `break`.** A flag tested after the block is a `break` written
longhand, for the same reason the first audit's §7b calls a manual counter a
`for` written longhand: the reader has to hold a variable across ten lines to
learn something the keyword says in place.

**The ergonomics test, at the real call site:**

```python
while true:
    conn = null                          # still needed: block scope
    try:
        conn = ln.accept()
    except ValueError as e:
        break                            # the listener is closed
    except ConnectionError as e:
        continue                         # client vanished between SYN and accept
    except OSError as e:
        errors = errors + 1
        if errors > _MAX_ACCEPT_ERRORS:
            ln.close()
            _drain_live(live, tally, drain)
            raise e
        time.sleep(backoff)
        backoff = min(backoff * 2, _ACCEPT_BACKOFF_MAX)
        continue
    …
```

Two bindings gone, three post-block `if`s gone, eight lines shorter, and each
handler now says what it does where it catches. It also deletes the `raise
failed` at `:1411` in favour of `raise e`, which is the site §3 flagged as the
one place in the tree relying on `raise <name>` meaning re-raise — so the two
recommendations reinforce.

**What I am less sure about.** The one thing the current shape buys is that the
backoff/`raise` block sits at one indent level instead of two, and that block is
the least-exercised path in the server. If the maintainer's reason for the flag
is "I want the escalation policy visible at the top level of the loop rather
than buried in a handler", that is a real argument and it beats mine. The
`closed` flag has no such defence — it is three lines from its `break` and buys
nothing.

**Confidence: medium** on the whole rewrite, **high** on `closed`.

---

## 11. Answering an error status: `return` a response or `raise`

Two spellings, both in `std/http.oro`, both correct, no rule written down.

```python
# std/http.oro:1528, Router.dispatch — error as a value
return Response(405, {"allow": allowed.join(", ")}, b"method not allowed\n")
return Response(404, {}, b"not found\n")

# std/http.oro:325 + 686 — error as an exception, with the status on it
raise BadRequest("a fragment must not be sent in a request target")
raise BadRequest(f"header block exceeded {_MAX_HEAD} bytes", 431)
```

A handler can use either. `raise http.BadRequest("no such user", 404)` and
`return http.text("not found\n", 404)` produce the same bytes on the wire,
because `_dispatch` (`:1241`) catches `BadRequest` and turns it back into a
`Response` carrying `e.status`.

**Which is right: return if you are the code that writes the response; raise if
you are not.** `Router.dispatch` *is* the response writer — a 404 is the route
table's answer, not a fault, and there is nothing to unwind past. A parser eight
frames deep inside `read_request` cannot return a `Response`; the status is
discovered somewhere that has no access to the return path, and `BadRequest`
exists precisely to carry it out. The proof that this is the real distinction is
that `_dispatch` converts one into the other at exactly the boundary where the
return path becomes available again.

That gives the rule for handler authors, which is the audience that needs it:

> Raise `BadRequest` when the status is decided somewhere that cannot build the
> response — in a helper, a parser, a validator. Return a `Response` when you
> are the handler and you are choosing what to send. A `raise` at the top of a
> handler body is a `return` written longhand.

**And the class split is right.** `BadRequest` versus `BadResponse` — *"`except
http.BadRequest` in a handler that makes an outbound call must not swallow 'the
service I called is broken' and report it to my client as *their* mistake"* — is
the best short justification for a class in the tree, and the first audit was
right to record it as earned. `BadResponse` deliberately carries no status,
because there is nobody to answer, which is what makes it a sibling rather than
a subclass.

**Check the tree.** `examples/server.oro` uses neither: every handler returns,
and `boom` demonstrates the 500 by raising a `KeyError`. Nothing in the tree
raises `BadRequest` from a handler, so the rule is currently untested by
example. That is the gap — the module documents `http.BadRequest(message,
status=400)` in its API header (`std/http.oro:26`) and never shows a handler
using it. One route in `examples/server.oro` would fix it.

**Confidence: medium** — I am confident the rule is the right one and less
confident that the `Router`'s 404 path should stay a `return` rather than being
made uniform. It should: a 404 is not an exceptional condition, it is the most
common response a router produces after 200.

---

## 12. One dead task, two diagnostic surfaces, and only one has the line number

`src/task.rs:33` stores a failed task's exception twice:

```rust
Failed {
    exc: Value,
    /// The diagnostic line, rendered at the moment of death because that is
    /// the only moment the faulting line, column and script path are known.
    report: String,
},
```

Both are real, and which one the program sees depends on who observes the
failure:

```python
def boom():
    d = {}
    return d["user"]

t = spawn(boom)
try:
    t.join()
except KeyError as e:
    print(type(e), f"{e}")      # <class 'KeyError'> 'user'   — no position

spawn(boom)                     # nobody joins
# stderr: task failed: t4.oro:3:12: KeyError: 'user'
# exit code 1
```

The joiner gets a value with a class and a message and **no file, line or
column**. The drop report gets a string with the position and **no value to
inspect**. Same fault, two surfaces, and each carries what the other lacks.

**This is the "two ways for the same fault to be reported" the brief asks
about, and unlike §1 it is not an accident** — the comment is explicit that the
rendering has to happen at the moment of death. The `Failed`/`FailedJoined`
split is `docs/stdlib-server-design.md` §3's rule 3 and it is correct: an
exception nobody claimed is printed, one a `join` claimed is not, and nothing is
ever both. That design is right and I would not touch it.

**What is wrong is narrower: the position is in the wrong place.** It is not a
property of "nobody joined"; it is a property of the fault. The reason the
report has to be rendered early is that the exception *value* does not carry
where it was raised — so a joiner has no way to log it, and a server that joins
its workers deliberately (which `_drain_live` at `std/http.oro:1487` does) has a
strictly worse log than one that leaks handles.

**Verdict: put the position on the exception, not on a parallel string.** Two
fields on the instance — the same NUL-prefixed, unreachable-from-Oro trick
`RENDERED_MESSAGE` and the `sys.exit` sentinel already use — and `report`
becomes a rendering of `exc` computed at drop time from data the value already
holds. One fault, one surface, and `except E as e` in a joiner can log the line
number for the first time.

**Confidence: medium.** I have not checked what it costs to thread the raising
frame's source/line/col into `make_exception_instance` for every raise rather
than only for task failures, and if the answer is "a Value clone on the hot
path of every `raise`" the calculus changes. The asymmetry is real either way
and should be written down even if it is not fixed.

---

## 13. `sys.exit` versus raising — settled, and worth recording as settled

There is one spelling, because `sys.exit` **is** a raise:

```rust
// src/vm/modules.rs:426 — encodes the request as a sentinel error
Err(format!("\u{0}exit\u{0}{code}"))
// src/vm/mod.rs:5042 — recognised and turned into a real SystemExit instance
```

and it behaves like one, verified:

```python
import sys
try:
    sys.exit(3)
finally:
    print("finally ran")        # runs
# exit code 3
```

```python
try:
    sys.exit(3)
except SystemExit as e:
    print("code =", e.args)     # (3,) — catchable, and the exit is cancelled
print("cancelled")              # exit code 0
```

```python
try:
    sys.exit(3)
except Exception as e:
    print("BAD")                # never printed — SystemExit is under BaseException
```

That last one is the whole reason `BaseException` exists, and `std/http.oro:1248`
relies on it by name (*"BaseException — SystemExit — is deliberately not
caught"*). The design is coherent: `sys.exit` is not a second control-transfer
mechanism competing with `raise`, it is a named exception with a default
top-level handler, and `finally` sees it like everything else.

**One note for §1's ledger.** The sentinel is the fourth thing riding the string
channel (`"\u{0}exit\u{0}3"` is parsed back out of the message at
`src/vm/mod.rs:5045`), and it is the one use of that channel I would *keep* — it
is a single well-known string with a NUL prefix that no program can produce,
rather than a substring match on prose. Worth saying explicitly when the other
three tables go, so nobody deletes it by association.

**Confidence: high.**

---

## 14. Catching at a width, and what must never be caught

Census of every `except` clause in the tree, by class:

| class | clauses | | class | clauses |
|---|---|---|---|---|
| `TypeError` | 76 | | `EOFError` | 6 |
| `ValueError` | 58 | | `OSError` | 4 |
| `AttributeError` | 31 | | `ChannelClosed` | 4 |
| `KeyError` | 14 | | `TimeoutError` | 3 |
| `Exception` | 9 | | `FileNotFoundError` | 3 |
| `RuntimeError` | 7 | | `ConnectionError` | 3 |
| `http.BadRequest` | 7 | | others | 1 each |

`except BaseException` appears **zero** times, which is the correct number and is
what earns `BaseException` its place.

`except Exception` appears nine times: seven in `corpus/`, where the point is to
demonstrate the width, and two in `std/http.oro` — `_dispatch:1253` and
`_conn_task:1453`. Both are last-resort handlers at a task or request boundary
whose job is *"anything else is a bug, and gets exactly one line naming the
peer"*. That is the one situation where a wide catch is right: a boundary past
which a failure would take down something larger than the unit that failed.
Both log the type (`f"{type(e)}: {e}"`), which is what makes a wide catch
acceptable — it does not hide what it caught.

**The rule:** catch at the narrowest width that names a decision you are going
to make. `except OSError: pass` in `_conn_task` is wide *on purpose*, and the
comment says which four faults it means and why they are one decision ("a reset
peer, a broken pipe, a deadline, or the drain closing the socket under a parked
read — is how connections end and is not worth a line"). A wide catch with that
comment is narrow in the only sense that matters.

**What must never be caught:** `BaseException`, and therefore `SystemExit`. The
tree honours this. `except (A, B)` is already cut and fails loudly —
*"catching classes that do not inherit from BaseException is not allowed (got
'tuple')"* — which is a slightly confusing message for a construct that was cut
on purpose rather than one that was never supported; it should name the
replacement (two clauses) the way the cut string methods do.

**Confidence: high.**

---

## 15. What I expected to find and did not

Recorded so these do not get re-audited.

**No `KeyboardInterrupt`, and nothing sees ctrl-c.** There is no such class in
`src/vm/exceptions.rs`, no signal handling anywhere in `src/`, and
`examples/server.oro:93` prints *"ctrl-c to stop"*. So ctrl-c is process death:
`_conn_task`'s `finally` does not run, `serve`'s drain does not run, and the
in-flight responses are truncated. I expected to find either a handler or a
deliberate note and found neither. This is not a second spelling of anything —
it is a gap — but it undercuts §9's "finally is ownership transfer" claim at the
one moment a server most needs it, and a language that ships a graceful-shutdown
drain should say whether the advertised way to stop the server reaches it.
**Confidence: low** that this is unintentional; it may be a deliberate "signals
are not in 1.0", in which case one sentence in the README settles it.

**No sentinel-instead-of-exception violation in `std/`.** I went looking for a
function that returns `false` or `-1` where the language's answer is a raise,
and there isn't one. The two candidates (`_drain` returning `false`,
`read_request` returning `null`) are both the lookup case correctly applied.

**No cargo-culted `finally`.** All four non-test sites do real work. I expected
at least one "close it just in case" and found none.

**No `try` used for control flow.** No EAFP loops, no `except KeyError` standing
in for `in`, no exception used to exit a nested loop. The tree consistently uses
`in`, `.get(k, default)` and `break`.

**`except X` with no clause body doing nothing.** Two `pass` handlers exist
(`std/http.oro:1448` and the corpus); both are commented. No silent swallows.

**Exception chaining.** There is none — no `raise … from`, no `__context__`, no
`__cause__`. I expected to want it at `std/http.oro:781` and `:2144`, where a
`ValueError` from `read_until` is deliberately replaced by a `BadRequest`/
`BadResponse` that discards it. I decided I do not: the discarded message is an
implementation detail (`read_until() found no delimiter in the first 8192
bytes`) and the replacement is the fact. Chaining would be a second spelling of
`raise` for a case the tree does not have. **Leave it cut, and say so** — it is
the kind of omission that looks like an oversight rather than a decision.

---

## 16. If only three things happen

1. **Stop deciding an exception's class by substring-matching its message**
   (§1). It is the only defect in this document that is reachable from the
   network, it produces wrong classes on attacker-chosen data, it leaks
   `RuntimeError` for four faults CPython types, and it has already forced one
   workaround (`RENDERED_MESSAGE`) that makes `e.args` mean two different things.
   The cheap first step — a class alongside the message in `VResult`, and the
   two attacker-reachable sites converted — is small and closes the hole.

2. **Make `return`/`break`/`continue` in a `finally` a compile error** (§2). It
   is the one construct in the language that can make a `raise` do nothing, the
   corpus does not use it, CPython itself has deprecated it, and Oro already
   makes compile errors out of much smaller traps. Zero churn.

3. **Require `raise` to take an instance** (§3), which cuts `raise E` and
   removes the ambiguity where `raise <name>` means "construct" or "re-raise"
   depending on what the name holds. One corpus line changes, by two characters,
   and it lands with the first audit's already-agreed cut of bare `raise` to
   leave the language with exactly one spelling of each of the two jobs: `raise
   E("msg")` to fail, `raise e` to re-raise.

Everything else here is smaller. The next tier, in order: cut the four dead
exception classes and add `RecursionError` (§5); drop the seven unused `as e`
bindings and the `closed` flag in `serve` (§4, §10); collapse the three
failure-injection mechanisms in `std/http.oro` to two (§6); and write down the
four rules this document produced that cost nothing to state — the
value-versus-raise rule with its `null` hole, the intermediate-node rule, the
`finally`-is-ordering rule, and the return-versus-raise rule for handler
authors.
