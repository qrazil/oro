# Oro

Oro (named for the *ouroboros*) is a small, deliberately frozen subset of
Python, implemented in Rust and executed on a bytecode virtual machine. It
compiles to a single self-contained binary. The lexer, parser, compiler, and VM
are all hand-written.

**Minimal and steady library imports on the Rust side.** Not zero — that is a
number, and a number is a thing to defend rather than a policy. The policy is
that a dependency is taken only for a problem that is *hard*, never for one
that is merely tedious, and only where the crate itself is stable enough that
importing it is not a standing commitment to track someone else's churn. Each
one is named here with the reason.

- [`regex`](https://docs.rs/regex) (4 transitive), behind the `re` module. A
  Thompson-NFA engine with a guaranteed linear-time match, so no pattern can
  ReDoS an Oro program. A correct linear-time engine is a serious project and a
  hand-rolled backtracker hangs on inputs like `(a+)+b`. See [the `re`
  module](#standard-library-surface).
- [`mio`](https://docs.rs/mio) (2 transitive: `libc` and `log`), behind the I/O
  reactor — the thing that lets a socket read park a green thread instead of
  stopping the VM. std exposes no readiness API: there is no `epoll`, `kqueue`
  or IOCP in it, so this is one of the few places where "do it yourself" means
  writing per-platform `unsafe` syscall bindings, and this crate has no
  `unsafe` in it. mio is the same battle-tested wrapper tokio is built on with
  the runtime taken off the top — which matters, because a runtime is the one
  thing Oro must *not* import: every Oro value is an `Rc`, so the ready queue
  can never be `Send`, and the scheduler is therefore Oro's own by
  construction. tokio would have been eight crates to buy a scheduler thrown
  away on the first line of use. The reasoning is in
  [`docs/stdlib-server-design.md`](docs/stdlib-server-design.md) §3.

- [`libc`](https://docs.rs/libc) (0 transitive), and it was already here —
  mio's own dependency, listed separately now because `net` calls it directly.
  It buys exactly one thing: `SO_REUSEPORT`, which has to be set on a socket
  *between* `socket(2)` and `bind(2)`, and `std::net::TcpListener::bind` does
  socket-creation, `setsockopt`, `bind` and `listen` inside one call with no
  hook in the middle. That is checked against the std source rather than
  assumed, and mio offers no way in either. **This is the one place in the crate
  with `unsafe` in it** — four syscalls in `src/net/reuseport.rs`, each with the
  invariant that makes it sound written at the call site.

  The honest alternative was [`socket2`](https://docs.rs/socket2), which wraps
  exactly this in a safe API and is a fine crate. It was declined because
  `setsockopt` is *tedious, not hard* — the line the policy above draws, and the
  same line that had timers written by hand below — and because taking it would
  not have removed the `unsafe` from the program, only moved it into somebody
  else's repository. What it would genuinely have bought is "no `unsafe`
  authored here", which is worth something and is not worth a permanent
  dependency. That property is now *enforced* rather than asserted anyway:
  `#![deny(unsafe_code)]` sits at both crate roots and exactly one module opts
  out, so a second `unsafe` block anywhere in Oro is a compile error rather than
  a thing to notice in review.

Timers are the one thing mio does not have, and they are not imported either:
`epoll_wait` already takes a timeout, so "wake this task in *n* ms" is a sorted
list of deadlines whose head becomes that argument. That is a small amount of
code with no new concepts in it, which is exactly the line the policy above
draws — hard problems are bought, tedious ones are written.

The single-file static musl build survives that: `libc` is a *bindings* crate —
declarations, not an implementation — and musl still links statically. Checked
rather than assumed, on every release: `cargo build --release --target
x86_64-unknown-linux-musl` produces a `static-pie linked` binary that `ldd`
calls `statically linked`, and mio cost it 16 KiB (0.6%).

The point of Oro is not to be a bigger Python. It is to be a *smaller* one that
never grows: one way to do each thing, a language and API that freeze, and a
compatibility guarantee that keeps the whole Python tooling ecosystem working
for free.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/qrazil/oro/main/install.sh | sh
```

This detects your OS/arch, downloads the matching prebuilt binary, **verifies
its SHA256**, and installs it (to `~/.local/bin` by default). Linux builds are
static musl binaries — one file, no libc dependency, runs on Alpine and old
distros alike. Prebuilt for `x86_64`/`aarch64` on Linux and macOS.

- **Pin a version:** `curl -fsSL …/install.sh | ORO_VERSION=v0.2.0 sh`
- **Custom location:** `… | ORO_INSTALL_DIR=/opt/bin sh`
- The installer never edits your shell rc files; if the install directory isn't
  on `PATH`, it prints the exact `export PATH=…` line to add yourself.

**Prefer not to pipe curl into a shell?** That's a reasonable stance — you're
running code you haven't read. Download `install.sh` and read it first, or skip
it entirely: grab the archive for your platform from the
[releases page](https://github.com/qrazil/oro/releases), verify it against
`SHA256SUMS`, `tar -xzf` it, and move the `oro` binary onto your `PATH`.

**Uninstall:** delete the binary — `rm ~/.local/bin/oro` (or wherever you put
it). Oro writes nothing else.

## The thesis: one way to do each thing, and it freezes

Most languages accrete. Each release adds a second (then third) way to spell
something, and every addition is a permanent tax on everyone who reads code in
that language. Oro takes the opposite bet: pick one spelling for each thing,
then **stop**. The language freezes. The standard API freezes. Features do not
accrete.

The model is [rhai](https://rhai.rs/): a scripting language that has held a
stable 1.x for years without breaking its users. Boring, in the way a load-
bearing tool should be boring. Oro would rather be finished than fashionable.

**The freeze starts at 1.0, not now.** A freeze is what you earn after finding
the right set, not a constraint you adopt before exploring — freezing a language
that is still missing `enumerate` or still has a broken `split()` just locks in
the wrong thing. 0.x moves, sometimes breakingly (see
[Migrating](#migrating-from-01)); the promise is that it moves *loudly*, with an
error naming the replacement, and that it stops at 1.0.

A frozen language is not a limitation to apologize for — it is the feature. You
can learn all of it. You can hold all of it in your head. Code written today
reads the same as code written years from now, because there is no "modern Oro"
that makes the old way look dated.

## Compatibility: Oro reads like Python, but no longer *is* Python

Oro originally promised that **every Oro program is also a valid Python
program**. That promise bought two quite different things, at very different
prices, and it has since been unbundled:

- **Grammatical familiarity — kept.** Oro uses Python's grammar: indentation,
  `def`, `class`, colons, f-strings. Every `.oro` file is something syntax
  highlighters, editors, and formatters already understand, so Oro ships no
  tooling of its own and never needs to. This costs nothing, because it
  constrains neither semantics nor the standard library.
- **Semantic parity — dropped.** Running the same file under CPython and
  getting the same answer is what forced Oro to inherit CPython's defaults, and
  it capped Oro at "Python, but less" — a strict subset can never have a feature
  Python lacks. Programs using `proc`, `=>` lambdas, `.map`/`.filter`, or the
  `to_` casts will not run under CPython at all.

**CPython is still the oracle for the computational core** — arithmetic,
bigints, strings, containers, control flow, sorting, formatting. That layer has
no reason to diverge, surprise there is pure cost, and it is where subtle bugs
actually hide: every correctness bug the corpus has caught (`sorted()` missing,
`split()` ignoring `maxsplit`, `round()` not rounding half-to-even, floats never
using exponent form) lived in it. `corpus/core/` is generated by running CPython,
and stays that way.

**Divergence is deliberate, documented, and still tested.** Programs that
intentionally differ live in `corpus/divergence/` with reviewed `.expected`
files, and `run.sh` checks them on every run. "Intentionally different" must not
be allowed to decay into "quietly broken".

One rule survives from the old invariant and is worth keeping: **where syntax is
identical, behaviour must be identical.** Diverging loudly (a new name, a new
spelling) is fine; the same code silently meaning two different things is not.
That is why a multi-segment `import a.b.c` — which Python binds to `a` and a
last-segment rule would bind to `c` — is rejected outright rather than quietly
redefined: a dotted import must use `as` (`import a.b.c as c`).

## The frozen feature set

Implemented and working today:

- **Values:** `int` (inline `i64`, promoting to arbitrary-precision bignum on
  overflow), `float`, `bool`, `str`, `bytes`, `null`, `list`, `tuple`, `dict`,
  `range`, and functions (including closures over *read* access). There is no
  `set` type — `set()` raises, pointing at a `dict` of `true` values.
- **Numeric literals:** decimal, `0x` hex, `0o` octal, `0b` binary (prefix
  letters and hex digits case-insensitive), `1e10` / `1.5e-3` / `.5` floats, and
  `_` as a digit separator anywhere between two digits — `1_000`, `0xff_ff`,
  `1_000.5`, `1e1_0`. CPython's grammar exactly, which is stricter than it
  looks: `1__0` and `1_` are errors, `_1` is a name, a radix prefix needs a
  digit after it, and `01` is refused outright rather than read as 1 or as
  octal — `0o` is the spelling for what that meant. A literal may not run into
  a name either, so `0x1f` is a number and `123abc` is an error, where both
  used to lex as a number followed by an identifier and fail somewhere else,
  about something else. `oro fmt` reprints a literal in the spelling it was
  written in: `0xff` and `255` are the same value and not the same statement
  about it.
- **`bytes`, a second type and not a redefinition of `str`.** `b"..."` (and
  `rb"..."`) is a sequence of octets: `b[i]` is an `int`, `b[i:j]` is `bytes`,
  iteration yields `int`s, and `len` counts octets. Cross the boundary
  explicitly with `s.to_bytes()` (UTF-8, cannot fail) and `b.to_str()` (UTF-8,
  strict — invalid input raises rather than substituting replacement
  characters). Making `str` *be* bytes would have been one type fewer, but it
  changes what `len(s)`, `s[i]` and `for c in s` mean under an unchanged
  spelling, which is the one thing Oro will not do — and it would put character
  indexing permanently beyond CPython's reach as an oracle.
- **Control flow:** `if` / `elif` / `else`, `while`, `for … in …`,
  `break`, `continue`, `pass`, and `match` (a value-only switch — see below).
- **Functions:** parameters are names, optionally with a default, and the `=` is
  the whole calling convention — no default means positional-only, a default
  means keyword-only. There is no `*args`, no `**kwargs` and no call-site
  `*`/`**`: a function that takes any number of values takes a list, one that
  takes named options takes a dict, and `apply(f, args=xs, kwargs=d)` forwards
  either into a call. **A default is evaluated on each call that omits it**, not
  once when the `def` runs, so `def f(x=[])` hands out a fresh list every time
  rather than accumulating into one — Python's mutable-default trap is not in
  Oro. Deep and mutual recursion work (the VM never recurses in
  Rust — see [Architecture](#architecture)).
- **Classes:** single inheritance, `__init__`/instance attributes/methods,
  class-level attributes, `super()`, and a fixed dunder set —
  `__str__`, `__repr__`, `__eq__`, `__len__`, the arithmetic dunders
  (`__add__`…`__pow__`), and the comparisons (`__lt__`/`__gt__`/`__le__`/`__ge__`).
- **Exceptions:** `try`/`except`/`finally`/`raise`, `except E as e`, bare `raise`
  to re-raise, a real exception hierarchy (`BaseException`→`Exception`→
  `ValueError`, `TypeError`, `KeyError`, `IndexError`, …) that `except` matches by
  inheritance, and user exceptions via `class MyError(Exception)`. Every runtime
  fault (index out of range, division by zero, missing key, …) raises the same
  exception type CPython uses, so it is catchable.
- **Generators:** `yield`, generator objects, `for` iteration, `StopIteration`
  on exhaustion, and generators consuming generators — all on the heap frame
  stack, so pipelines never grow the native stack.
- **`global`** for mutating module-level state from a function.
- **Modules:** `import a.b.c` / `import x as y`, built-in `sys`, `os`, `time`,
  `re`, and `proc`; `io`, `json` and `http`, which are written in Oro and
  shipped as source inside the binary (`json`'s codec is the one place a
  stdlib module reaches into Rust for more than a constructor — parsing JSON is
  a per-byte loop, and per-byte loops are Rust's half of the boundary); and user
  modules loaded from the script's directory (run once, cached).
- **Byte streams:** `open(path, mode="r")` (`r`/`w`/`a`, and every one of them
  **bytes**) returning a stream with `read(n)`/`write(b)`, plus
  `read_until(delim, limit)` and `close()`; `io.read`, `io.copy` and
  `io.buffer` in `std/io.oro`; `sys.stdout`/`stderr`/`stdin` as real streams on
  fd 1/2/0. Streams close deterministically at end of scope (no `with`), and
  there is no `flush()` anywhere in the language. See
  [The io protocol](#the-io-protocol).
- **f-strings** with the full format mini-language: `{x:.2f}`, `{n:05d}`,
  `{x:,}`, alignment (`<^>`), sign/`#`/`0` flags, the `!r`/`!s` conversions, and
  nested specs like `{x:.{p}f}`.
- **Builtins:** `print` (with `sep=`/`end=`), `len`, `repr`, `type`, `abs`,
  `clamp`, `round`, `chr`, `ord`, `open`, and the three concurrency names.
  **A builtin takes scalars; a collection method takes a collection** — that is
  the line, and it is why the list is short. `min`/`max` are collection
  reductions only (`xs.min()` / `xs.max()`); the two-argument scalar form was
  cut for `clamp(v, min=…, max=…)`, which reads as the bound it is. `len` is the
  one name on both sides of the line, and the reason is stated where the cut is
  (below). See `corpus/divergence/65_builtin_method_line.oro`.
- **Type keywords:** `bool`, `int`, `float`, `str`, `bytes`, `list`, `tuple`,
  `dict`, `range`, and the runtime handles `File`, `Buffer`, `TcpStream`,
  `TcpListener`, `Pattern`, `Match`, `Task`, `Channel`. Each *is* the type, and
  `type(x)` answers with that same value — so **`type(x) == str`** is the one
  way to ask what something is, and it works identically for a user class
  (`type(p) == Point`). They are keywords: `dict = {}` is a compile error
  naming the reason, which is what keeps the test from silently becoming a
  comparison against someone's local variable. A type name is not callable
  (`range(n)` is the exception — a range has no literal to build it with); to
  convert, use the `to_` casts below, and to build an empty one, the literal.
- **Methods:** the `str`/`bytes` surface — fifteen names, the same fifteen on
  both types (`bytes` adds `hex` and `scan`) — plus the `list`/`dict` methods,
  the `to_` conversions on every value, and the collection protocol below.
  `b.scan(allowed)` is how many bytes at the front of `b` are all in the
  `allowed` set, so `b.scan(set) == len(b)` asks "is every byte of this field
  in the class my grammar allows" in one call into Rust, and
  `b.scan(everything_but_the_delimiters)` is a multi-delimiter find. It is on
  `bytes` and not on `str` because that is where per-octet protocol work
  lives.

  `strip` `split` `find` `count` `startswith` `endswith` `rm_prefix`
  `rm_suffix` `upper` `lower` `replace` `is_digit` `is_alpha`
  `is_alnum` `is_space`

  Three of them take a keyword, and only their own. `strip(chars=null,
  side="both")` takes `side="left"` / `"right"` — which is why there is no
  `lstrip`/`rstrip` — and `chars` is a character *set*, CPython's cutset
  semantics unchanged. `split(sep=null, maxsplit=-1, side="left")` takes the
  same `side`, in two values rather than three, because that is where
  `maxsplit` counts its splits from — which is why there is no `rsplit`.
  `find(sub, start=, end=, reverse=false)` takes `reverse=true` for the last
  occurrence, spelled the way `sort(reverse=…)` already is, which is why
  there is no `rfind`. `count(sub, start=, end=)` searches the same window `find`
  does, by the same rules — "how many" and "where" are asked over the same
  region of the same string, or they are two surfaces pretending to be one. `rm_prefix`/`rm_suffix` remove
  a *literal* affix and exist precisely because `strip(chars)` gets mistaken
  for one: `"ping.png".strip(chars=".png", side="right")` is `"pi"`, and
  `"ping.png".rm_suffix(".png")` is what was meant. The four `is_*` predicates
  are whole-sequence and answer `false` for an empty sequence, as CPython's do.
- **The collection protocol**, on lists, tuples, dicts, ranges and generators.
  Chains replaced comprehensions; this is what lets them replace *loops* too.

  *Without a callback:* `sum(start=0)` `min` `max` `len` `first` `last`
  `reverse` `unique` `take(n)` `drop(n)` `chunk(n)` `flatten`
  `zip(other, ...)` `join(sep)`.
  `zip` takes any number of further sequences and truncates to the shortest, so
  `a.zip(b, c)` is a three-way zip and not a two-way one with `c` thrown away.
  The counts are required: `take()` with none is an error rather than a
  guess, and `take(n, 99)` is one rather than an ignored argument.

  *With one:* `map` `filter` `flat_map` `sort(f, reverse=false)` `group_by`
  `partition` `find` `any` `all` `count` `min_by` `max_by` `unique_by`
  `take_while` `drop_while` `reduce(init, f)`. Each callback is required —
  truthiness is `xs.any(x => x)`, and `[0, 1, 2, ""].count()` read as a length
  and was not one. `find`, `any` and `all` short-circuit.

  **Sorting is `sort`, and it returns a new collection.** `xs.sort(f)` answers a
  new collection ordered by the keys `f` gives, and the elements' own order is
  `xs.sort(x => x)`: the function is the operand, so it is the positional
  argument, as it is for `min_by` and `group_by`. `reverse=true` is *stable*
  descending and is not `xs.sort(f).reverse()`: reversing a sorted sequence
  flips the *ties* too, where `reverse=` leaves them in the order they arrived.
  There is **no in-place sort or reverse** — every collection operation returns
  a new collection, so `xs.sort(f)` and `xs.reverse()` are rebound
  (`xs = xs.sort(f)`) rather than mutating, and the aliasing bug where
  `b = a; a.sort()` reorders what `b` sees cannot be written. (This is a silent
  difference from Python's mutating `list.sort`; see "Migrating from 0.1".)

  Two rules govern the whole set. **Operations that select or reorder preserve
  the receiver's type** (a tuple stays a tuple, a dict stays a dict); operations
  that reshape the data return a list. `sort` reorders, so `t.sort(f)` is
  a tuple and `d.sort(f)` is a dict. It is only a list where there is no other
  shape to keep — a range or a generator. And **a callback with two or more
  parameters destructures its element exactly as `for` does**, while a callback
  with one takes it whole. A dict's element is its `(key, value)` pair, so
  `d.filter((k, v) => v > 1)` reads the pair `for k, v in d` does, and the pairs
  the protocol makes itself feed the next step the same way:

  ```python
  names.zip(ages).filter((name, age) => age >= 18).map((name, age) => name)
  orders.group_by(o => o.region).to_list().map((region, rows) => (region, rows.len()))
  ```

  A chain is **value-only** — there is no index parameter and no chain spelling
  for an index. Needing the index is one of the reasons to write a `for i, x in
  xs` loop instead: one source, one meaning for `i`. In a chain, `i` would be
  ambiguous the moment a `filter` came before the `map` (index in the filtered
  result, or the original?), so the parameter simply does not exist.

  What counts is the parameters without a default: `def f(x, n=2)`
  takes the element whole, a defaulted parameter never counts (it is
  keyword-only), a bound method's
  `self` does not, and `reduce` counts those after the accumulator —
  `d.reduce(0, (acc, k, v) => acc + v)`. Any element `for` can unpack will do,
  and a length mismatch is `for`'s `ValueError`. A native callable (`len`,
  `s.startswith`) declares nothing to count; it is handed a dict's pair as two
  arguments and any other element whole.
  Type preservation has a consequence worth stating on its own: because
  `d.map(f)` answers with a dict, `f` must answer with a `(key, value)` pair.
  `d.map((k, v) => (k, v * 2))` is the shape; `d.map((k, v) => v)` raises
  `rebuilding a dict needs (key, value) pairs, not 'int'` — a `RuntimeError`
  today, where `TypeError` is what it is describing. `d.filter(...)` has no
  such rule: it passes the dict's own pairs through.

  ```python
  orders.filter(o => o.paid).group_by(o => o.region)
  ```

  **A chain runs in one pass.** `xs.filter(p).map(f).filter(q)` walks `xs`
  once, applies the three operations to each element in turn, and builds one
  collection: the intermediates a reader might imagine between the steps are
  not built. The compiler recognises the chain (the intermediate has no name
  and no second reader, which is a property of the expression) and the VM runs
  it as one job. Two things follow that are worth knowing about.

  *Some steps are barriers.* `sort` `reverse` `unique`
  `unique_by` `chunk` `flatten` `zip` `group_by` `partition`
  `min_by` `max_by` and `take_while` each need the finished intermediate before
  they can answer, so a chain fuses the runs of `map`/`filter` between them and
  materialises at each barrier. `xs.map(f).filter(p).sort(k).map(g)` is two
  passes and one sort, not four passes. `take_while` is on that list for a
  reason worth knowing on its own: it calls its predicate on **every** element,
  not just the ones up to the first false, and fusing it would have quietly
  changed how many times a user predicate runs.

  *Short-circuiting reaches back through the chain.* `xs.map(f).first()` calls
  `f` once, not once per element, and `take(n)`, `find`, `any` and `all` stop
  the whole upstream pass the same way. Before chains were fused, `find`, `any`
  and `all` short-circuited only their *own* step, which was the least useful
  place for the saving to be.

  The one thing a program can tell the difference by is **the order two
  callbacks in different steps run in**. `xs.map(f).map(g)` runs
  `f(x0), g(y0), f(x1), g(y1)` — the order a `for` loop would run them — and
  not every `f` followed by every `g`. Mutating the receiver from inside a
  callback is unaffected, fused or not: the receiver is copied before the first
  callback runs, so a chain never sees its own source change under it.

  Note `xs.join(", ")` and **not** `", ".join(xs)`, which is cut: the sequence
  is the subject and the separator the detail, and this way it ends a chain
  instead of sending the reader back to the front of the line.
- **Lambdas:** `x => x * 2`, `(a, b) => a + b`, `() => 0`.
- **Generators work everywhere.** A generator can be passed to any builtin that
  consumes an iterable (`sum`, `sort`, `min`, …) and can start a chain
  (`g().map(f)`), not just drive a `for` loop. Builtins run in Rust and can
  never re-enter the interpreter, so the generator is drained a frame at a time
  and the call is retried — the native stack never grows with it.
- **Green threads: `spawn`, `chan` and `yield_now`, seven names in total.**
  `spawn(f, …)` starts `f(…)` as a task and returns a handle;
  `t.join()` waits and returns the function's value. `chan()` is a rendezvous
  and `chan(cap=n)` a buffer of `n`, with `send`, `recv`, `close`, and
  `for msg in ch` iterating until the channel is closed and drained.
  `yield_now()` hands the CPU to the next ready task and answers `null` — the
  one way to yield without touching a channel, and a no-op rather than a
  deadlock when nothing else is ready. It is spelled `yield_now` and not
  `yield` because `yield` is a keyword, and because that is the term of art
  (Rust's `yield_now`, Go's `Gosched`). A task costs a few hundred bytes and a handful of small
  allocations — frames are already heap cells, so nothing is copied and the
  native stack is never involved.

  Three consequences worth stating plainly. **Scheduling is cooperative**: a
  task yields at a channel operation or a `yield_now()` and nowhere else, so a
  CPU-bound handler starves its peers until it finishes. **There is no parallelism inside one VM**,
  which is why there is no `select` and no locks — a shutdown flag is a
  variable, a cache is a `dict`, and sharing them is free. And **an uncaught
  exception in a task kills only that task**: it is re-raised in whoever joins
  it, or, if nobody ever does, printed when the handle is dropped — as
  `task failed: app.oro:42:9: KeyError: 'user'`, because in a server log the
  fact that the *program* is still running is the most important thing on the
  line — with the process exiting 1. Neither Go's answer (kill the process) nor Python's (print
  and exit 0 anyway). The reasoning is `docs/stdlib-server-design.md` §3.
- **Python truthiness** and Python's cross-type numeric equality (`1 == 1.0 ==
  true`).
- **Value types compare by content, reference types by identity** — CPython's
  split. A function, generator, class, instance, module, stream, pattern, match,
  task or channel is equal to itself and to nothing else, and is a `dict` key,
  which is what makes a registry keyed by connection or by task writable at all.
  Two closures over one code object are two functions. A `builtin` is compared
  by name rather than address (`len == len`, and the builtin cache is per call
  site, so there is no one address); a bound method by receiver and function, so
  `a.m == a.m` is true even though a fresh one is built on every access — as in
  CPython, where `==` is likewise the operator that sees through
  that. `list` and `dict` stay unhashable, and so
  does an instance of a class that defines `__eq__`: CPython clears `__hash__`
  there and lets the class define one back, and Oro's dunder set has no
  `__hash__`, so it is permanent — `def __hash__` is a compile error naming the
  replacement, not a method that runs and does nothing. The rule stated
  directly: **`__eq__` is a declaration that this is a value type, a value type
  with custom equality is not a key, and the key is the value it compares by**
  (`d[(self.row, self.col)]`). The case for leaving it that way — and the
  tuple key that replaces it — is
  [`docs/hash-and-equality.md`](docs/hash-and-equality.md). There is no `hash()` builtin, and dict order
  is insertion order, so hashing by address is not observable from a program —
  nothing about a run depends on where the allocator put something.

### What is cut, and why

Each of these is omitted on purpose. The reason matters more than the list.

- **No walrus (`:=`).** Assignment is a statement; a second assignment operator
  that also returns a value is precisely the "more than one way" the thesis
  rejects.
- **No chained assignment (`a = b = c`).** Tuple unpacking already spells it —
  `a, b = 1, 2` — and spells the case chaining cannot, where the values differ.
  Chaining also lies about what it made: `a = b = []` binds *one* list to both
  names, so appending through `a` changes `b`, while `a, b = [], []` makes two.
  A second spelling that is shorter only when every value is identical, and
  silently aliases when they are mutable, is not worth the word.
- **No `is` (and no `is not`).** `==` already compares a function, generator,
  class, instance, module, stream, pattern, match, task or channel *by
  identity*, and `null`, ints and bools by value, so on everything with an
  address the two operators agree and `is` is a second spelling of an answer the
  language already gives. They disagreed in exactly two places, and neither is a
  reason to keep an operator: two mutable containers with equal contents
  (`[1, 2] is [1, 2]` was false where `==` is true), and a class that defines
  `__eq__`.

  What settled it is the third case, the one where `is` could not be made to
  agree with *CPython*. `"hel" + "lo" is "hello"` is `True` under CPython and was
  `false` here, because CPython's answer is a fact about its string-interning
  table — implementation-defined, and not the same across versions or build
  flags. Oro cannot match it without carrying an interning table it has no other
  use for, so this was identical syntax quietly meaning two different things, in
  the computational core, with no fix available. That is the one thing the
  compatibility rule above forbids, and the operator was the only removable part
  of it.

  It is also the operator behind the most-asked beginner question in Python —
  why `x is y` is true for `5` and false for `1000` — which exists only because
  a language has two similar comparisons whose overlap is an implementation
  detail. The cost is worth naming rather than hiding: the *aliasing* question,
  "are these two equal lists the same list", is no longer expressible at all.
  Nothing in the language, the standard library or the corpus was asking it.
- **No comprehensions.** Not because a second iteration construct is
  intolerable, but because a comprehension reads *inside-out* — in
  `[f(x) for x in xs if p(x)]` the iteration is in the middle, the transform on
  the left, the filter on the right — and it does not compose: past two steps
  you must nest, and each nesting level reverses the reading direction again.
  Oro uses method chaining instead, which reads in the order it runs and stays
  flat at any depth:

  ```python
  xs.filter(x => x > 1).map(x => x * x).filter(x => x < 30)
  ```

  `map`/`filter` are **type-preserving**: a list gives a list, a tuple a tuple,
  a dict a dict, so a chain never silently changes the shape of the data. A
  callback with several parameters destructures its element as `for` does —
  `d.filter((k, v) => v > 1)`, `pairs.map((a, b) => a * b)` — and `map`
  over a dict returns the `(key, value)` pair. `range` and generators have no
  literal to rebuild, so they yield a list.
- **No type annotations.** `def f(a: int) -> int` and `x: int = 5` are rejected
  at the parser, in all three positions. They used to parse and be thrown away,
  which is the one outcome worse than either having them or not: the header
  said `int` and the function happily took a string. Oro has no static checker
  and is not getting one, so an annotation would be a comment with syntax —
  write a comment, or a name that says what it holds.
- **No decorators.** Implicit `f = deco(f)` rewriting hides control flow behind
  an `@` sigil; write the wrapping explicitly if you want it.
- **No metaclasses / `__getattr__` / dynamic attribute hooks.** These make it
  impossible to know what an attribute access does by reading it. Oro keeps
  attribute access legible.
- **No `eval` / `exec`.** A language that can grow arbitrary code at runtime is
  not a frozen language, and it defeats every static tool.
- **No multiple inheritance.** MRO linearization is a whole subsystem of subtle
  behavior for a feature single inheritance covers in practice.
- **No `with`.** Not needed — see [Divergences](#deliberate-divergences-from-python).
- **No `lambda` keyword.** Oro spells an anonymous function `x => x * 2`,
  `(a, b) => a + b`, or `() => 0`. The body is a single expression; anything
  needing statements is a `def`.
- **No `nonlocal`.** Mutating an enclosing function's locals through a keyword is
  a rare need with an outsized implementation and readability cost; use `global`
  or an object. (Reading captured variables works fine.)
- **No or-patterns in `match` (`case a | b:`).** See below.
- **No semicolons and no inline suites (`if x: y`).** Both are second ways to
  write a block, and together they are much of why Python needs autoformatters
  at all. One statement per line; one block form.
- **No sets.** A set is a dict with no values, and the uses that matter are
  already covered: `{k: true}` with `k in d` gives O(1) membership, and a dedup
  loop keeps insertion order (unlike `set()`, whose arbitrary order is a real
  bug source). Only set *algebra* (union/intersection/difference) is a genuine
  gap, and that is rare in scripting — when wanted, it belongs in a stdlib `Set`
  class written in Oro, the same split as JSON and CSV (primitives in Rust,
  everything else in Oro), not in the frozen core. `{1, 2, 3}` and `set()` each
  give an error pointing at a dict or a list. **Tuples stay** — they are the
  only hashable composite, so `counts[(host, port)]` has no substitute, and they
  are load-bearing for multiple return and `a, b = b, a`.

- **No `bytearray`, and no growable byte buffer.** Oro already has one frozen
  idiom for building a string incrementally — append to a list, `join` at the
  end — and it builds `bytes` just as well, with the same performance and zero
  new types. The one case that genuinely needs a mutable window is a buffered
  reader, which belongs in Rust and never shows Oro code its buffer.

- **No `lstrip`/`rstrip`, no `rfind`, no `index`, no `rsplit`, no `zfill`.**
  Five names removed from the string surface, each because one name already
  covers it — but only four of them were replaced by an existing name, and the
  fifth is worth reading as a correction.

  `lstrip`/`rstrip` are `strip(side="left"/"right")`: three methods for one
  operation, distinguished by a letter, is exactly the accretion the thesis
  rejects — and the letter is the least readable part of the call. `rfind` is
  `find(sub, reverse=true)`, for the same reason and with the keyword Oro
  already uses for direction in `sort`. `index` is `find` that raises instead
  of answering `-1`; two spellings of one search, and the one that raises makes
  every caller choose between a `try` and a method they did not need.

  `rsplit` is a different *answer* from `split`, not a different need
  (`"a=b=c"` split once from the right is `["a=b", "c"]`, from the left
  `["a", "b=c"]`) — so the name went and the answer stayed, as
  `split(sep=…, maxsplit=…, side="right")`. That was not the first attempt. The
  first was to point at `find(sep, reverse=true)`, on the theory that the case
  wanting a right split is really "split off the last field"; it is, and the
  replacement was still wrong, because `find` hands back an *index* and leaves
  the caller to write `s[:i]`, `s[i + 1:]` and the `-1` check — three chances
  to be off by one where there had been none. A removal has to leave the
  capability behind. `side=` does, on the keyword `strip` already uses, and
  costs no new name. `zfill` is fully redundant with the
  format mini-language: `f"{42:05d}"`, `f"{'42':0>5}"`, and `f"{s:0>{w}}"` for
  a width computed at run time — which `zfill` cannot express any more briefly
  and cannot generalise to any other pad character.

  All five raise, naming the replacement. So do CPython's `removeprefix`,
  `removesuffix`, `isdigit`, `isalpha`, `isalnum` and `isspace`, which exist
  here under Oro's own names (`rm_prefix`, `rm_suffix`, `is_digit`, …).

- **No `sum`, `sorted`, `any`, `all`, `enumerate` or `zip` as builtins**, and
  `min`/`max` narrowed to two or more values. The rule is one sentence:

  > **A builtin takes scalars. A collection method takes a collection.**

  Nine builtins used to duplicate nine collection methods — `len sum min max
  sorted any all enumerate zip` — and usage across the tree was split roughly
  down the middle, which settles nothing: a reader has no way to know which
  half the codebase prefers, because it used both. The rule settles it, and
  every cut names its replacement: `xs.sum()`, `xs.sort(x => x)`,
  `xs.any(p)`, `xs.all(p)`, `a.zip(b, c)`. `enumerate` is the one with no method
  either — it went *entirely*, because every `for` now yields `(index, value)`,
  so the whole of it is `for i, x in xs` (see above).

  The duplication was not merely noise. Three of the nine pairs had already
  *drifted* into disagreeing — `a.zip(b, c)` silently discarded `c`,
  `sorted((3, 1, 2))` answered a list where `(3, 1, 2).sorted()` answered a
  tuple, and `enumerate(xs, 1, 9)` accepted the 9 and ignored it. All three
  were found and fixed by making each pair share one body, which is the right
  repair; one spelling is what keeps it repaired.

  **`min` and `max` are collection reductions, and the two-argument scalar form
  is cut for `clamp`.** `min(a, b)` had two readings — the extreme of two values,
  and (far more often) a bound — and the bound reads backwards: `max(v, 0)` makes
  you translate "the maximum of these two" into "floor at zero", and two-sided
  clamping was the nest `min(max(v, 0), 100)`, easy to write inside-out. So the
  bound became `clamp(v, min=…, max=…)` — `std/http.oro` caps its backoff with
  `clamp(backoff * 2, max=_ACCEPT_BACKOFF_MAX)` — and `min`/`max` keep the one
  job a bound cannot express, reducing a whole collection (`xs.min()`,
  `xs.max()`). Each name has one meaning where `min` had two.

  **`len` is the one exception, and it is named rather than quietly kept.** It
  is the only one of the nine that reaches `str` and `bytes`, which are
  deliberately outside the collection protocol, and the only one that
  dispatches a dunder — `__len__` — on a user class. Cutting `len(x)` would
  reserve `.len()` on every user object, which the io protocol's own freeze
  note warns against doing lightly; cutting `.len()` would break
  `xs.filter(p).len()` back to `len(xs.filter(p))` and send the reader to the
  front of the line. One exception with a stated reason is not the same thing
  as nine unexplained pairs.

  **What the cut costs, named.** `sorted("ba")` and `min(b"ba")` worked,
  because a builtin read its argument through the *iteration* protocol and that
  reaches `str` and `bytes`. The collection protocol does not — `"abc".map(f)`
  has always been an `AttributeError` — so those capabilities now cost a call:
  `"ba".to_list().sort(x => x)`. Giving `str` one collection method out of twenty
  would have been a worse inconsistency than the loss, and `to_list()` is
  already the documented bridge. The error says so at the call site.

- **No `str.join`/`bytes.join`.** `", ".join(xs)` and `xs.join(", ")` were
  byte-for-byte the same operation, with the same type rules and the same
  refusal of a mixed list, and the language shipped both while this document
  argued for one of them. The collection form is the one that survives: the
  sequence is the subject and the separator the detail, so the call ends a
  chain — `pairs.map(encode).join("&")` — instead of sending the reader back
  to the front of the line to find out what is being joined. The str form
  raises, naming it.

  The evidence that two spellings cost something even when both are correct is
  that `std/http.oro` used each of them, 244 lines apart, and nothing in the
  file explains why. Tree-wide it was 26 uses of one and 16 of the other:
  neither had won, and a model asked to join a list had no way to tell which
  the codebase wanted, because the codebase wanted both. That is the whole
  argument for the thesis in one method name.

- **No `re.match`.** It anchors at the start of the string — almost always not
  what people mean, and endlessly confused with `re.search`. Use `re.search`, or
  a leading `^` to anchor on purpose.
- **No regex backreferences or lookaround.** They require backtracking, which
  would forfeit the linear-time (ReDoS-free) guarantee. Do it in two passes:
  match candidates with `re.finditer` (which gives positions), then verify in
  Oro. Go and Rust made the same call.
- **No `subprocess` shell (`shell=True`, `os.system`).** Arguments go straight
  to `execve`, so injection is structurally impossible, not just discouraged.
- **No `datetime` (yet).** Calendar/formatting/parsing is a large surface that
  belongs in Oro on top of `time`, modelled on `java.time` — separate
  `Instant` / `LocalDate` / `ZonedDateTime` types (not one naive-or-aware
  object), immutable, with `Duration` distinct from `Period` — rather than
  Python's single overloaded `datetime`.

- **Type names are not callable.** A type name *is* the type — it is what
  `type(x)` answers with — so there is nothing behind it to call. Conversion is
  a method on the value: `xs.to_list()`, `"42".to_int()`, `"ff".to_int(base=16)`,
  `x.to_str()`, `s.to_float()`, `v.to_bool()`, `pairs.to_dict()`,
  `s.to_bytes()`. Construction is a literal: `[]`, `{}`, `""`, `b""`, `0`. This
  is one spelling per thing, it chains in reading order, and it removes a whole
  error class by construction — a conversion needs something to convert, so
  there is no zero-argument form to confuse with building an empty value.
  (`range(n)` is the single exception, because a range has no literal.)

- **No `isinstance`.** `type(x) == str` is the type test, and it is the only
  one: a type name and `type()`'s answer are the same value, for a builtin type
  and for a user class alike. `isinstance` was a third spelling of that, and the
  one thing it could do that `==` cannot — test a *subclass* — has its own
  keyword: `except` matches an exception against a base by walking the chain,
  which is the one place asking "is this a kind of X?" is the right question.
  Everywhere else the answer is to call the method the subclass overrides,
  which is what inheritance is for.

Also cut: bare `except:`, `try/except/else`, `from x import y`, `import *`,
`del`, `assert`, `__new__`/`__getattr__`/`__setattr__`/`__slots__`, and class
`metaclass=`. (`del` goes with the no-`with` reasoning: block scope plus
refcounting already drops a binding at end of scope, and `d.pop(k)` covers
removing a key. `assert` is cut because it is a statement whose behaviour
depends on an interpreter flag — `raise` is the one way to fail.)

## Deliberate divergences from Python

Oro is a subset, but in nine places it deliberately behaves *differently* from
Python. Each divergence is a place where Python made a choice it could not later
reverse, and Oro — starting fresh, with a single implementation — makes the
choice Python would arguably prefer.

### `true`, `false`, `null` — the three literals are lowercase

```python
print(1 < 2)        # true
print([1 == 1, x.get("missing")])   # [true, null]
```

Python capitalised these because they are singleton *objects*, and Python
capitalises class and singleton names. That is a convention about the
implementation, and it leaked into the syntax: nothing about a boolean literal
wants to look like a class. Nearly every other language spells them lowercase,
and Oro already did too, in the one place it had to — `json.stringify` has
always emitted `true` and `null`, because that is what the wire format says. The
language disagreed with the format its own standard library writes.

**This changes output, not just source.** `repr` and `str` of a bool and of
`null` change everywhere they appear: at the top level, inside containers, in
f-strings, and in `json`. In exchange, the JSON encoder's translation layer is
gone — `_bool_word` and a hard-coded `"null"` collapsed into the value's own
text, because that text *is* the wire form now, in both directions.

`True`, `False` and `None` are rejected at the word, by the lexer, naming the
replacement — never quietly treated as ordinary names that fail as a `NameError`
somewhere else. The type name went with them: `type(null)` is `<class 'null'>`.
`NoneType` was a leftover, pointing at a word this language does not have —
the literal is `null`, the repr is `null`, and `json` writes `null`. It is the
one name in the table changed on purpose, and it costs one line of oracle: the
rest of `type(x)` stays CPython's and stays in `corpus/core/`
(`42_type_names.oro`), with the divergences — this one and the four that never
matched — listed in `corpus/divergence/54_type_names.oro`.

The corpus keeps its oracle through the rename: `corpus/oracle.sh` translates
in both directions — Oro's literals into Python's before CPython sees the
source, CPython's back into Oro's before the output is written. Both mappings
are total and 1:1, so no blind spot is introduced. The one thing it cannot
survive is a program that prints the *string* `"True"`, since that is
indistinguishable from a printed bool in the output; the oracle tokenises every
program and refuses one that puts those words in a string literal, which is
written up in the script's own header.

### Block scope on `if` / `for` / `while` / `try`

Names bound inside a block do **not** leak out of it:

```python
for i, _ in range(10):
    last = i
print(last)   # NameError in Oro; prints 9 in Python
```

Python leaks loop and branch variables into the enclosing function scope. It
fixed exactly this for comprehensions in Python 3 (comprehension variables no
longer leak) but could not retrofit the fix to `for`/`while`/`if` without
breaking decades of code that relies on the leak. Oro has no such legacy, so it
gives every block real lexical scope. This is intentional and correct; it is not
a bug.

**`try` is a block too**, and this is the one that surprises people, because a
`try` body usually exists to *produce* something:

```python
conn = null                 # bind it first — see below
try:
    conn = ln.accept()
except OSError:
    pass
print(conn)                 # without the first line: NameError
```

Since a name is created by assigning to it — Oro has no `let` or `int x;` to
declare one without a value — carrying a result out of a block means binding it
beforehand, to `null` or to whatever the failure case should look like. That
placeholder is the tax this rule charges, and it is worth knowing that it can
hide a failure: if the block does not run, the placeholder flows on instead of
the `NameError` that would have told you. Choose it so the wrong path is loud.

### No truthiness — `if` / `while` / `and` / `or` / `not` take a `bool`

Oro has no truthiness. A condition, and every operand of a boolean operator, is
a `bool` and nothing else; a non-bool is a `TypeError` that names the explicit
test the caller meant:

```python
if xs:          # TypeError: a condition must be a bool, not 'list' — use `len(x) != 0`
if n:           # TypeError: ... not 'int' — use `x != 0`
if x:           # TypeError: ... — use `x != null`   (for a maybe-null value)
```

The rewrites are the ones the standard library already used, unanimously, for
2,400 lines before this was enforced: **absence is `== null`, emptiness is
`len(x) == 0`, a numeric zero is `== 0`.** Not one conditional in `std/` tested
a list, string or bytes for truthiness rather than for length; the eight bare
`if x:` tests in the whole library were all on a `bool`. So this cut breaks
almost nothing in the tree — it writes down what the tree was already doing.

**`and` and `or` are pure boolean operators that return a `bool`**, not
CPython's value-returning versions. That is the cut's second job. In Python
`user.get("name") or "anon"` is the idiom for a fallback, and it silently
replaces a legitimate empty string, `0`, or `[]` — because all three are falsy.
In Oro that expression is a `TypeError` (the left operand of `or` is not a
`bool`), so the trap is not discouraged, it is **unrepresentable**. A fallback
is written where it can be seen: `d.get(k, default="anon")`, or an explicit
`if name == null`.

Why this is sharper in Oro than the truthiness Python keeps: Oro chose sentinel
returns. `s.find(x)` answers `-1` (truthy) and `0` (falsy), so `if s.find(x):`
is wrong at both ends; `d.get(k)` cannot tell a missing key from a key holding
`0`. A language whose lookups answer with values rather than raising cannot
afford truthiness, and the standard library had been avoiding the edge without
saying so. Now the language says so. `corpus/divergence/79_no_truthiness.oro`
pins the faults; `corpus/core/04_truthiness.oro` is the explicit tests that
replace them, and still oracles against CPython, which agrees on every line.

### The loop rule: `for … in range(n)` counts, `while` waits

> **A loop over a known count is `for i, _ in range(n)`. A loop over a
> collection is `for _, x in` the collection, or a chain. `while` is for a
> condition that is not a count — a poll, an EOF drain, an accept loop.** And a
> loop whose index exists only to reach into two sequences at once is `a.zip(b)`.

(Every `for` binds an `(index, value)` pair — see [above](#every-for-yields-index-value) — so the count loop keeps the index and discards the value, and the collection loop does the reverse.)

**Half of this rule the grammar now enforces.** A `while name < bound` whose
body steps `name` by an integer constant — the hand-rolled counter — is a
compile error naming `for`:

```python
i = 0
while i < 10:      # TypeError at compile: this `while` counts `i` by a constant —
    i = i + 1      #   a counted loop is `for i, _ in range(n)`
```

A step by a *runtime* value (`got = got + len(chunk)`), a non-`<`/`>` condition
(the EOF drain `while chunk != b""`), a compound condition, and `while true` are
all genuine conditions and compile — because a condition is not a count. The
collection half (`for _, x in xs` over `while i < len(xs)`) still rests on the
rewrite: `oro fmt` is a formatter, not a linter, and detecting every
index-into-a-collection is not something a compile-time check can do without
false positives. But the counter, which is where the tree had actually gone
wrong, is now unwritable.

It needed writing because the tree had got it backwards. A bounded count was
spelled two ways and the worse one had won 67 to 17:

```python
i = 0                          for i, _ in range(10):
while i < 10:                      work(i)
    work(i)
    i = i + 1
```

The manual form is three lines instead of two, and it has three failure modes
the other one cannot have: forget the increment and it never ends; write a
`continue` above the increment and it never ends; put the increment in the
wrong place and the loop is off by one. It is also **32% slower** — 0.203s
against 0.139s over three million iterations — because `range` steps in Rust
and a hand-written counter steps in the VM, three instructions at a time.

None of that is the main cost. The main cost is that it was in `std/http.oro`,
in the corpus, and in the examples, and **a newcomer reads the standard library
to learn what the language looks like.** A dominant idiom nobody chose is still
a dominant idiom.

`while` keeps its place, and keeps it easily: a condition is not a sequence.
`while chunk != b"":` drains a stream to EOF, `while len(live) > 0 and
time.monotonic() < deadline:` waits out a shutdown, and `while true:` accepts
connections — none of them has a `for` spelling, and all three are in
`std/` unchanged. The one deliberate exception in the other direction is
`bench/progs/`, where the loop is inside the measurement; that file's own
header says so, and the argument is in `bench/RESULTS.md`.

### No `with` — deterministic cleanup instead

Oro has no `with` statement because it does not need one. Reference counting
plus block scope means an object is released the moment its last reference goes
out of scope — deterministically, at end of block:

```python
def read(path):
    f = open(path)          # opened here
    return io.read(f)       # f's last reference dies at return → file closed
```

`with` exists in Python because the *language spec* refuses to promise *when* an
object is destroyed — CPython refcounts, but PyPy and Jython use tracing garbage
collectors, so file handles might close "eventually." `with` is the workaround
for an unspecified destruction time. Oro has exactly one implementation and can
therefore promise the timing, which removes the reason `with` exists.

(The flip side is honest: refcounting alone cannot reclaim reference *cycles* —
see [Known limitations](#known-limitations).)

### The io protocol

Every stream in Oro — a file, a `Buffer`, `sys.stdout` — has the same two
methods, and there are only two:

```
read(n)   -> bytes      # 1..n bytes; b"" at EOF; MAY return fewer than n
write(b)  -> null       # writes all of b, or raises
```

That is a **naming convention, not a declared type**: there is no `class
Reader`, nothing to inherit from and no registration. Any object with a `read`
of that shape is a Reader, which is why `io.read` and `io.copy` work unchanged
on a class you wrote this afternoon. In a dynamic language this costs exactly
nothing, and the alternative — a family of stream types that each guess
differently — is the zoo every scripting language ends up with.

Four consequences, each of which will eventually surprise someone, so they are
stated rather than discovered:

- **`open(path, mode="r")` returns bytes, in all three modes, and there is no
  `"rb"`.** There is no text mode, no `encoding=`, no text wrapper and no line
  iterator. With text mode gone the `b` contrasts with nothing — a letter
  meaning "not the other kind" in a language that has no other kind — so it is
  rejected with a message naming the replacement. Whole-file text is a compose
  of two things that already exist:

  ```python
  f = open(path)
  s = io.read(f).to_str()                              # a whole file, as text
  lines = io.read(f).to_str().strip(chars="\n", side="right").split(sep="\n")  # …its lines
  ```

- **`read(n)` may return fewer than `n` bytes without being at EOF**, because
  that is what a stream is: `n` is a maximum, and the answer is "what has
  arrived". EOF is an empty return, not an exception — every stream ends, and
  the normal termination of the most common loop in systems programming is not
  a fault. Code that needs *exactly* `n` bytes calls `io.read(r, fixed_size=n)`, which
  loops and raises `EOFError` if the stream ends short. `r.read(n)` is the raw
  primitive; `io.read(...)` does the whole job.

- **`print` and streams are separate worlds.** `print` takes `str`; streams
  take `bytes` and nothing else, in either direction, anywhere in the language.
  `sys.stdout.write("hi")` is a `TypeError`; the spellings are `print("hi")`
  and `sys.stdout.write(b"hi")`, and both reach fd 1 in program order. This is a
  real edge to trip over, and it is the price of having one stream protocol
  rather than one for text and one for bytes.

- **There is no `flush()`, anywhere.** Every reader buffers internally, so
  `read_until(b"\r\n\r\n", 65536)` is one syscall per 8 KiB rather than one per
  byte, and nothing in Oro can see or size that buffer — an API whose correct
  use is "always, right away" is not an API, it is a default someone forgot to
  set. Every *writer* is unbuffered, which is why `flush` can be absent: it is
  one of the great silent bugs, where the program is correct, the tests pass,
  the last response of every connection is truncated, and nothing raises. The
  thing a buffered writer was for is an idiom the language already has —
  `parts.join(b"")` once, at the call site, where it cannot be forgotten
  halfway.

The bill for all of this is `.to_bytes()` at the point where a program's text
becomes output, and `.to_str()` where its input becomes text. That is the
two-type tax, paid once at a visible boundary instead of smeared across a
family of stream types.

The full reasoning, including what was cut and why, is in
`docs/stdlib-server-design.md` §2.

### Every `for` yields `(index, value)`

There is one loop form. A `for` binds an `(index, value)` pair, and
destructuring selects what you want:

```python
for i, v in xs:        # both
for _, v in xs:        # the value only  (`_` discards)
for i, _ in xs:        # the index only
for x in xs:           # error: a `for` binds a pair — write `for _, x in xs`
```

`_` is a **discard**, not an ordinary name: it binds nothing, so `for _, _ in
xs` is not a duplicate-binding error, `a, _ = pair` drops a component, and
`_ = f()` runs `f` for its effect and throws the result away. Reading `_` back
is a compile error, so a value you discarded cannot be picked up by accident.
(`case _` in a `match` is the unrelated wildcard, and still matches anything.)

**What the index is depends on the iterable, and this is the part to remember:**

| iterable | index | value |
|---|---|---|
| `list`, `tuple`, `str`, `bytes` | position (`0, 1, 2, …`) | the element |
| `range` | position | the number |
| `dict` | the **key** | the value |
| generator | a 0-based counter | the yielded value |

A dict is the one whose index is not a position — `for k, v in d` yields its
entries, key and value. That is not a special case bolted on: **the pair shape
is already this language's answer for a dict everywhere else.** `d.map(f)` hands
`f` the `(key, value)` pair; a dict's `filter`, `find`, `count`, `any` and `all`
callbacks take *two* arguments; `group_by` builds a dict whose entries are what
you then want. So iterating a dict yields the same pair, with the key in the
index slot.

The common case gets slightly noisier — `for _, line in lines` where Python
writes `for line in lines` — and that is an accepted trade. One loop form with a
pair, rather than a bare element that means something different for a dict than
for a list, is what makes human- and model-written Oro converge, and it is less
to hold in your head: there is no `enumerate`, no `range(len(xs))`, no
`.items()`, no bare `for x in d` that quietly hands you a key one place and a
pair the next.

```python
for _, x in xs.enumerate()   # no: `enumerate` is gone entirely
for i, x in xs               # yes: the index is built in
for i in range(len(xs))      # no: redundant, and a single binding besides
d.items()                    # AttributeError: iterating the dict *is* its items
```

There is no index in a chain, either — a chain is value-only, so an index is one
of the reasons to reach for a loop rather than a chain.

A tuple **element** is a nested pattern in the value slot, since the top-level
binding is now `(index, value)`:

```python
for _, (a, b) in pairs       # pairs is a list of 2-tuples
for _, (a, b, c) in triples  # or 3-tuples; the index is still `for i, (a, b, c)`
```

`.keys()` and `.values()` stay: they are how you ask for *one half* of an entry,
a different question, and neither is the pair. **`k in d` still tests keys** —
membership is a hash lookup, not a walk, so `"a" in d` is true and `("a", 1) in
d` is not. That is the thing most likely to break silently, so it is pinned in
`corpus/divergence/67_dict_pairs.oro` along with pair order and mid-loop
mutation; `corpus/divergence/80_for_pairs.oro` pins the index for every iterable
type. `d.sum()` is the one thing that got worse rather than different: it summed
the keys and is now an attempt to add a tuple to an int — `d.values().sum()` is
what it was reaching for.

### `dict.keys()` / `.values()` answer lists

```python
d = {"a": 1}
print(d.keys())        # ['a'] here; dict_keys(['a']) in Python
print(d.keys()[0])     # 'a' here; TypeError in Python — a view is not indexable
```

Eagerness is not the divergence — that is settled and stated above, and `zip`
makes the same choice. The divergence is only the *repr*,
and the question is whether a type should exist whose entire job is to print
differently.

It should not, because a CPython view is not a list in three visible ways: it is
not subscriptable, it compares as a *set* (`d.keys() == ["a"]` is `False` there,
and Oro has no sets to compare against), and it is *live*, seeing keys added
after it was taken. A repr-only view would match CPython on the one line that
prints it and differ on all three — a costume, and worse than either having
views or not having them, because it would look like the thing it is not.

So Oro answers a list, prints a list, and the whole collection protocol works on
it with no second type to learn: `d.keys().sort(k => k)`, `d.values().sum()`,
`d.keys().join("-")`. Recorded, with all three behavioural differences shown,
in `corpus/divergence/59_dict_views.oro`. (There is no `.items()` to ask the
question about — see above — and `d.to_list()` is the list of pairs.) The related case is already settled the
same way: CPython's repr of a `zip` carries a heap address
(`<zip object at 0x7f…>`), which is not reproducible output and could not
be matched even in principle.

### Tabs rejected in leading whitespace

A tab in leading indentation is a hard error, not a width-8 guess. Python's
tab/space ambiguity — where visually identical indentation can parse
differently — is a whole class of real bugs. Oro designs it out by refusing to
guess: indent with spaces, or get a clear error.

### `match` is a value switch, not pattern matching

Oro has `match`, but only the value-comparison subset — a fast switch spelled
the way Python spells it, not structural pattern matching:

```python
match command:
    case "quit":
        ...
    case "help":
        ...
    case Key.ESCAPE:   # dotted name: looked up and compared by value
        ...
    case _:            # optional default
        ...
```

Allowed patterns are literals (`int`, `float`, `str`, `true`, `false`, `null`),
dotted names (`Color.RED`), and the `_` wildcard. There is **no fall-through** —
one case runs and control leaves the `match`. When every case is a literal, the
whole thing compiles to an O(1) jump table rather than a comparison chain; that
performance win over an `if`/`elif` ladder is the actual reason `match` earns
its place, not syntactic sugar.

What is rejected, each with its own error:

- **Bare capture names — `case QUIT:`.** In Python this does *not* compare
  against a variable `QUIT`; it silently *rebinds* `QUIT` to the subject and
  matches everything. It is one of the most reliably surprising features in the
  language. Oro rejects it on purpose and tells you to write a literal or a
  dotted name (`case Cmd.QUIT:`) instead. Rejecting the footgun is the feature.
- **Or-patterns — `case "a" | "b":`.** `|` means "or" only in languages that
  lack an `or` keyword. Python has `or`; reusing `|` for alternation is an
  inconsistency Oro does not inherit. Write separate `case` clauses with the
  same body.
- **Destructuring** — class patterns (`case Point(x=1):`), sequence patterns
  (`case [a, b]:`), and mapping patterns (`case {"k": v}:`). `match` is a
  switch, not binding.
- **Guards (`case x if cond:`)** and **as-patterns (`case 1 as n:`)** — both
  bind or branch in ways a switch should not.

## Architecture

Locked design decisions, and what each one buys:

- **Two-pass compiler with static slot resolution.** A pre-pass walks the AST to
  assign every name a fixed storage slot (local, cell, or free variable) before
  any bytecode is emitted. Name lookup at runtime is an array index, not a hash
  lookup.
- **Heap-allocated call frames; the VM never recurses in Rust.** Calling an Oro
  function pushes a `Frame` onto a `Vec` and continues the single flat
  interpreter loop — it does *not* call the interpreter recursively. Oro-level
  recursion depth is therefore bounded by heap, not by the native C stack, so
  deep recursion does not overflow. This flat design is also what makes
  generators implementable later without stackful coroutines.
- **`Rc` reference counting, not `Arc`.** No Oro value ever crosses a thread, so
  no Oro value pays for an atomic. This used to read "the runtime is
  single-threaded", which was a stronger claim than the design ever needed and
  stopped being true when DNS moved onto helper threads: `getaddrinfo` is
  blocking-only, so a resolver has to wait *somewhere*. What holds, and is what
  the decision actually rests on, is that the boundary carries a hostname in and
  an IP address out — never a `Value`. `Value` is `!Send` and stays `!Send`,
  which is precisely why the VM has to own its own scheduler rather than borrow
  one.
- **`i64` inline, bignum on overflow.** Small integers are unboxed `i64`;
  arithmetic that would overflow transparently promotes to an arbitrary-
  precision integer. You never silently wrap.
- **UTF-8 strings with an ASCII fast path.** Every string is UTF-8 and carries
  an `is_ascii` flag computed once at creation. Indexing and length are O(1) for
  ASCII strings and fall back to correct (O(n)) char handling otherwise.

## Standard library surface

A small set of modules ships in Rust, because each of them is a syscall or a
per-byte loop. Everything else is written in Oro and shipped as source baked
into the binary — `io`, `json` and `http` are there today, and that is the
intended growth path for the standard library, not a temporary arrangement. The
line between the two is measured, not asserted: `json` shipped as pure Oro,
measured 95x CPython's C codec, and its per-byte loop moved to Rust while the
module stayed where it was. `docs/stdlib-server-design.md` §5 has the rule, the
measurement, and the reason the same argument does not move `http`.

- **`sys`** — `argv` (`argv[0]` is the script), `exit(code)` (raises
  `SystemExit`; sets the process exit status if uncaught), `platform`, and
  `stdin`/`stdout`/`stderr` as real byte streams on fd 0/1/2. They are
  unbuffered, like every writer in the language.
- **`os`** — `environ`, `getcwd()`, `listdir()`, `remove()`, `mkdir()`, and
  `path`.
- **`os.path`** — `exists`, `isfile`, `isdir`, `join`, `basename`, `dirname`,
  `splitext`.
- **`open(path, mode="r")`** — `r`/`w`/`a`, all three of them **bytes**; there is
  no `"rb"` and no text mode (see [The io protocol](#the-io-protocol)). The
  stream has `read(n)`, `write(b)`, `read_until(delim, limit)` and `close()`,
  and no `flush()`. It closes when its last reference drops (see the
  `with`-free file lifetime above), so `close()` is for when end of scope is
  too late. `read_until` includes the delimiter and raises `ValueError` if the
  limit is reached without finding one — which is what stops a client sending
  an unbounded header block. Missing files and permission errors raise
  `FileNotFoundError` / `PermissionError`.
- **`io`** — written in Oro, and exactly three functions:
  `io.read(r)` (everything until EOF), `io.read(r, fixed_size=n)` (exactly `n`,
  with `EOFError` if the stream ends short), `io.copy(dst, src)` (returns the
  count), and `io.buffer(b)` (an in-memory Reader and Writer, and the only way to get a
  Reader you can feed literal bytes to). There is deliberately no `io.write`:
  `w.write(b)` already writes everything or raises, so a free function would be
  a second spelling for it. The asymmetry is real, and it is the two directions
  being genuinely different — reading everything requires a loop, writing
  everything does not.
- **`net`** — TCP, and the payoff of the io protocol: two constructors and two
  objects, and nothing else.

  ```python
  import net

  ln = net.listen("127.0.0.1:0")        # SO_REUSEADDR; ":0" = any free port
  ln = net.listen("0.0.0.0:8080", reuseport=true)   # share the port; see below
  print(ln.local)                       # "127.0.0.1:41337" — ask what you got
  conn = ln.accept()                    # -> TcpStream

  head = conn.read_until(b"\r\n\r\n", 65536)
  conn.write(b"HTTP/1.1 204 No Content\r\n\r\n")
  conn.close()
  ```

  A `TcpStream` **is** a Reader and a Writer — the same `read(n)`, `write(b)`,
  `read_until(delim, limit)` and `close()` a file has, with the same meanings —
  so `io.read`, `io.copy` and `io.buffer` work on a socket having never heard of
  one, and `io.copy(out, inp)` is a working TCP proxy. That is not a
  convenience; it is the reason the protocol was fixed before the stdlib
  existed. On top of the four it adds `shutdown_write()` (a half-close: send
  FIN, keep reading — not `close()`, which would drop the answer with it),
  `set_timeout(seconds)` (one deadline for both directions, `null` to clear;
  expiry raises `TimeoutError`), `set_nodelay(on)`, and `peer` / `local` as
  plain address strings.

  **Addresses are strings**, `"host:port"`, with Go's bracket form for IPv6
  (`"[::1]:8080"`). There is no `Address` type: it would buy parsing that is
  rarely wanted, and `addr.find(":", reverse=true)` covers it when it is. A listener has
  `accept()`, `close()` and `local`, and deliberately no `read` — it is not a
  stream of bytes, so it does not pretend to be one.

  **Every one of those calls parks the green thread rather than the VM.**
  `accept`, `read` and `write` are ordinary blocking-looking calls in the
  program and readiness events underneath: the socket is non-blocking, a call
  that cannot finish suspends only the task that made it, and one slow client
  cannot stall a fast one. There is no `async`, no `await` and no second colour
  of function — `conn.read(64)` is the same call inside a task as it is at the
  top level. `set_timeout` is part of that: it is a deadline the scheduler
  enforces, so it bounds the *whole* operation (a `read_until` spanning four
  packets) rather than restarting on each packet the way `SO_RCVTIMEO` did.

  **`net.dial` parks too, and that was the last one.** It used to stop the world
  twice for a hostname — once in the DNS lookup, once in the TCP handshake —
  which froze every task in the VM for anything from microseconds to the five
  seconds a retrying resolver takes. It now parks the calling task for both.
  `getaddrinfo` is blocking-only, so the lookup runs on a small pool of helper
  threads that exchange a hostname for an address and touch nothing else;
  dialling a literal `ip:port` still starts no thread and does no lookup at all.
  See [`docs/stdlib-server-design.md`](docs/stdlib-server-design.md) §4 for why
  the OS resolver rather than one written in Oro, and for the architectural
  claim that had to be corrected to make room for a helper thread.

  Failures use CPython's classes, so the hierarchy stays one hierarchy: a
  refused connect is `ConnectionRefusedError`, a reset peer
  `ConnectionResetError`, a write to a departed one `BrokenPipeError`, a local
  abort `ConnectionAbortedError` — all four under a new `ConnectionError` under
  `OSError` — a deadline `TimeoutError`, and a bind to a port already in use a
  plain `OSError`. Catch at whichever width you mean.

  Absent on purpose: UDP (not a stream, so it cannot satisfy the protocol),
  Unix domain sockets, TLS, and `SO_REUSEPORT`. The reasoning is
  `docs/stdlib-server-design.md` §4.
- **`json`** — `json.parse(text)` and `json.stringify(value, indent=null)`.
  `stringify` requires `str` dict keys rather than silently stringifying an int
  one, and `parse` takes `str`, not octets — the decode is a step the program
  takes, in the open, with `.to_str()`. The module is Oro; the codec under it
  is Rust, because parsing JSON is a per-byte loop and per-byte loops are the
  Rust half of the boundary (`docs/stdlib-server-design.md` §5). Nesting is
  capped at 10 000 containers, which nothing real approaches and which bounds
  what a client can make a server allocate.
- **`http`** — HTTP/1.1 in both directions, written in Oro on top of `io` and
  the `bytes` methods, with **no HTTP-specific Rust primitive anywhere**:
  `read_until` for the header block, `bytes.split` for the lines, `bytes.find`
  for the colon and `bytes.scan` for the grammar classes are generic
  building blocks that earn their place on their own, and everything above them
  is per *message* rather than per *byte*. The one loop underneath that is not
  a `bytes` method is the percent-codec (below), and it is not HTTP either:
  the `%HH` form is RFC 3986 and the set of characters it leaves alone is an
  argument.

  ```python
  import http

  routes = http.Router().add("GET", "/health", req => http.text("ok\n"))
  http.serve("0.0.0.0:8080", req => routes.dispatch(req))
  ```

  `http.serve(addr, handler)` is the accept loop: bind, then **one green
  thread per connection**, for as long as the listener is open. There is no
  `select` and no readiness machine in it — a loop that reads exactly like the
  blocking one-connection-at-a-time server everybody writes first *is*, as
  written, a concurrent one, because `accept`, `read` and `write` park the task
  and not the VM. A handler that raises is a 500 for that request and nothing
  more. Underneath it: `http.read_request(r)` parses one request off any Reader
  (`null` at a clean EOF); `http.write_response(w, req, resp, keep_alive=false)`
  sends the head and a sized body in **one** `write`;
  `http.serve_conn(conn, handler)` is the keep-alive loop;
  `http.Router().add(method, path, handler)` chains routes and binds
  `/users/:id` segments into `req.params`.

  **The client is the same module read backwards.** `http.fetch(method, url)`
  dials, sends, reads the whole answer and hangs up, and hands back the same
  `Response` class a handler builds:

  ```python
  resp = http.fetch("GET", "http://127.0.0.1:8080/health")
  print(resp.status, resp.text())

  http.fetch("POST", url, headers={"content-type": "application/json"},
             body=json.stringify(payload).to_bytes())
  ```

  There is no `http.get`/`http.post`/`http.put`: the method is an argument
  because it *is* an argument, and a wrapper per verb is a set of names that is
  wrong the moment somebody needs `PROPFIND`. There is no session object and no
  pool — one call is one connection, which says `Connection: close` and means
  it — and no redirects, cookies, authentication or retries, because each of
  those is a policy the caller has to own and `resp.header("location")` is
  right there. `http.stream(...)` is the same call that leaves the body on the
  wire for something too large to hold in memory, and hands the caller the
  obligation to `resp.close()`; `fetch` is six lines written on top of it, and
  those six lines are a `max_body` cap and that `close()`.

  **Building a URL is four functions, and the hard part is not the escaping.**

  ```python
  http.quote("a/b c")                       # 'a/b%20c'   — a path
  http.quote("a/b c", safe="")              # 'a%2Fb%20c' — one path segment
  http.quote_plus("a b+c")                  # 'a+b%2Bc'   — a query value
  http.encode_query({"q": "a&b", "page": 2})  # 'q=a%26b&page=2'

  http.fetch("GET", url, params={"q": "a&b"})
  ```

  A caller who wants `?q=a&b` — a query *value* containing an `&` — needs an
  encoder, and the naive one silently sends a different request than the one
  they wrote, splitting one parameter into two. That is why the client shipped
  without a `params=` until there was something correct to build it on. The
  escaping itself is one loop and it is in Rust; what is *not* one answer is
  which characters have to be escaped, because a path segment, a query value,
  a form body and a fragment each have a different set and picking the wrong
  one is how an encoder works until somebody types a `+` or a `/`. So every
  set is in `std/http.oro`, in Oro, spelled out and readable. The names and
  the behaviour are `urllib.parse`'s, checked against it over every octet
  (`corpus/divergence/63_percent.oro` and its twin), with three deliberate
  differences: a malformed escape raises rather than passing through as a
  literal `%`, a non-ASCII `safe=` raises rather than being silently dropped,
  and `urlencode` is `encode_query` and expands a list value into repeated
  keys (`{"t": ["x", "y"]}` is `t=x&t=y`) rather than stringifying the list.
  `http.unquote` is the inverse, and `unquote(quote(b, safe="")) == b` holds
  for arbitrary bytes.

  **`https://` raises at `http.parse_url`, before a socket exists.** Oro has no
  TLS, and the only alternative to refusing is opening a plaintext connection
  to port 443 and sending whatever the caller put in an `Authorization` header
  in the clear. Underneath `fetch`: `http.write_request(w, method, target,
  headers, body=b"")` and `http.read_response(r, method)` are the seam, and they
  take a Reader and a Writer rather than a socket, exactly as `read_request`
  and `write_response` do. `read_response` takes the method because **a
  response is not self-describing** — the reply to a `HEAD` carries the
  `Content-Length` the `GET` would have carried and no body at all, and a
  parser that does not know what was asked will block waiting for one.

  **Shutdown is closing the listener, and nothing else.** `serve` takes an
  optional `ready` channel and sends the bound listener down it before
  accepting anything, which is both how you learn the port when you bound
  `":0"` and how another task stops the server: `ln.close()` wakes the parked
  `accept()`, every connection is told this is its last round, idle keep-alive
  connections are closed at once, in-flight requests get a bounded `drain`
  (five seconds by default), and whatever is still there is closed under
  itself. `serve` then returns a tally of `accepted` / `refused` / `drained` /
  `forced`. Its other keyword arguments are the numbers a deployment has to be
  able to change: `max_conns` (512; at the cap a connection is accepted and
  closed with nothing written), `max_requests` per connection (1000, or a
  negative number for no cap — the `maxsplit=-1` convention) and `timeout` (30s,
  one scheduler deadline covering any single
  read or write on the connection).

  A request body is always a
  Reader, framed by `Content-Length` or `Transfer-Encoding: chunked` — a
  request carrying both is a 400, because guessing which one an intermediary
  meant is how a request smuggles another one in behind it. Everything it is
  handed is bytes an anonymous client chose, so the header block is bounded,
  obsolete line folding is refused, and a header block that goes over the limit
  is a 431 rather than a memory leak. Deliberately absent: `Expect:
  100-continue`, HTTP/2, HTTP/3, WebSocket upgrade, multipart, cookies,
  sessions, static files, compression, and a client. `examples/server.oro` is
  the whole of it in one screen — see [Examples](#examples).
- **`proc`** — exactly one function,
  `run(args, input=…, check=…, quiet=…, cwd=…, env=…, timeout=…)`, returning a
  `CompletedProcess` with `.returncode`, `.ok`, `.truncated`, `.stdout`,
  `.stderr`. `args` is **always a list of separate strings** that go straight to
  `execve` — there is **no `shell=True`**, so shell injection is impossible by
  construction (need a shell? write `["sh", "-c", cmd]` and own it). A bare
  string, or a list whose program contains whitespace (`["git status"]`), is a
  designed error rather than a silent misfire — Oro won't reimplement shell
  quoting to split it.

  The defaults are for orchestration scripts, not for CPython parity — which is
  why the module has a different name (see [Migrating](#migrating-from-01)).
  Output is **both** streamed live and captured, so no script has to choose
  between watching a build and grepping its output; `quiet=true` drops the live
  tee. A nonzero exit **raises** `CommandError` with the tail of stderr in the
  message, because a failed command nobody checked is one of the great sources
  of silent breakage; `check=false` allows one.

  `.stdout` and `.stderr` are **`bytes`**: a child emits octets, and it may well
  emit a JPEG. Decode with `.to_str()` at the point your program knows it is
  text. Missing/inexecutable programs raise `FileNotFoundError` /
  `PermissionError`; a `timeout` raises `TimeoutError`.
- **`re`** — `search`, `findall`, `finditer`, `fullmatch`, `sub`, `split`,
  `compile`. Backed by the `regex` crate's Thompson-NFA engine, so matching is
  **guaranteed linear-time — no ReDoS**. `finditer` yields match objects with
  `.start(n)`/`.end(n)` (character offsets) and `.group(n)`; positions are what
  let you implement backreferences and lookaround in Oro as a two-pass technique
  (match candidates with `re`, verify in code — see the cut list). `re.match` is
  cut. **Backreferences (`\1`) and lookaround (`(?=…)`, `(?<=…)`) are not
  supported** — they force backtracking and destroy the linear-time guarantee,
  the same trade-off Go and Rust have shipped for a decade. The two-pass
  workaround, e.g. doubled-word detection:

  ```python
  import re
  for _, m in re.finditer(r"(\w+)\s+(\w+)", text):
      if m.group(1) == m.group(2):      # the "backreference", checked in code
          print("doubled:", m.group(1))
  ```
- **`time`** — `time()`, `sleep(n)`, `monotonic()`. The naming is genuinely
  unhelpful, so plainly: `time()` is for **when** (timestamps, logs, file
  dates) — wall-clock epoch seconds that *can jump or go backwards* (NTP, DST,
  manual clock changes), so `time.time() - start` can be negative.
  `monotonic()` is for **how long** (elapsed, timeouts, benchmarks) — it only
  ever increases. Use the right one. There is **no `datetime`** (see cut list).

  `sleep(n)` suspends the **calling task**, not the process: other tasks keep
  running, and three tasks sleeping a second each take a second between them.

## Migrating from 0.1

0.2 makes deliberate breaking changes. Each is a *rejection* — old code fails
loudly with a message naming the replacement, never silently doing something
different.

| 0.1 | 0.2 | Why |
|---|---|---|
| `open(p, "r").read()` | `io.read(open(p))` | `open` returns bytes in every mode; a whole-stream read is a free function |
| `open(p, "rb")` | `open(p)` | With text mode gone, the `b` contrasts with nothing |
| `f.readline()` / `f.readlines()` / `for line in f` | `io.read(f).to_str().strip(chars="\n", side="right").split(sep="\n")` | There is no text stream type and no line iterator |
| `f.write("text")` | `f.write("text".to_bytes())` | Streams take bytes, in both directions, everywhere |
| `f.flush()` | *(nothing)* | Writers are unbuffered, so there is nothing pending |
| `sys.stdout` as a name | `sys.stdout.write(b"…")` | It is a real stream on fd 1 now |
| `sys.exit()` | `sys.exit(0)` | The code has no default, so every exit states its status |
| `m.group()` / `m.start()` / `m.end()` | `m.group(0)` / `m.start(0)` / `m.end(0)` | The group index is required; the number means itself, and `(0)` is still valid CPython |
| `re.sub(p, r, s, count)`, and `re`'s `flags` / `maxsplit` / `pos` | *(refused)* | Not implemented, and no longer silently ignored: the count used to be dropped, so every match was replaced |
| `import subprocess` | `import proc` | Different defaults deserve a different name |
| `subprocess.run(a, capture_output=True, text=True)` | `proc.run(a)` | Capture is always on, and the output streams live as well |
| `r.stdout` after a failed command | `proc.run(a, check=false)` first | A nonzero exit now raises `CommandError` |
| `str(x)`, `int(s)`, `float(s)`, `bool(x)` | `x.to_str()`, `s.to_int()`, `s.to_float()`, `x.to_bool()` | Conversion is a method; it chains, and has no confusable empty form |
| `int(s, 16)` | `s.to_int(base=16)` | as above |
| `list(xs)`, `dict(pairs)` | `xs.to_list()`, `pairs.to_dict()` | as above |
| `list()`, `dict()`, `str()`, `int()` | `[]`, `{}`, `""`, `0` | Literals build; type names are not callable |
| `isinstance(x, str)` | `type(x) == str` | A type name *is* the type, so the comparison is the test; there is no second spelling |
| `isinstance(e, SomeBase)` | `except SomeBase:`, or a method the subclass overrides | `except` is the one place a subclass test is the right question; elsewhere it is the question inheritance exists to avoid |
| `str(x)` for an instance | `f"{x}"` | `str` is a type, and a type is not callable; the f-string runs `__str__` |
| `lambda x: x * 2` | `x => x * 2` | Shorter, and the point of a lambda is brevity |
| `x is y` / `x is not y` | `x == y` / `x != y` | `==` already compares reference types by identity; `is` differed from CPython on interned strings and could not be fixed |
| `True` / `False` / `None` | `true` / `false` / `null` | Capitalisation was Python's class-naming convention leaking into syntax |
| `s.lstrip(…)` / `s.rstrip(…)` | `s.strip(chars=…, side="left")` / `side="right"` | One strip with a named end, not three methods |
| `s.rfind(sub)` | `s.find(sub, reverse=true)` | One find with a named direction; `reverse=` as in `sort` |
| `s.index(sub)` | `s.find(sub)` | Two spellings of one search, one of which raises; `-1` is the answer |
| `s.rsplit(sep, n)` | `s.split(sep=…, maxsplit=n, side="right")` | Same answer, on the `side=` keyword `strip` already uses; no second name |
| `s.zfill(n)` | `f"{n:05d}"`, `f"{s:0>5}"`, `f"{s:0>{w}}"` | Fully covered by the format spec, which also pads with anything else |
| `s.removeprefix(p)` / `s.removesuffix(p)` | `s.rm_prefix(p)` / `s.rm_suffix(p)` | Same method, shorter name |
| `s.isdigit()` / `isalpha()` / `isalnum()` / `isspace()` | `s.is_digit()` / `is_alpha()` / `is_alnum()` / `is_space()` | Same predicates, in the language's own naming |
| `sep.join(xs)` | `xs.join(sep)` | One join, on the collection; the sequence is the subject and the call ends a chain |
| `sum(xs)` / `sorted(xs)` / `any(xs)` / `all(xs)` / `zip(a, b)` | `xs.sum()` / `xs.sort(x => x)` / `xs.any(x => x)` / `xs.all(x => x)` / `a.zip(b)` | A builtin takes scalars, a collection method takes a collection |
| `enumerate(xs)` / `xs.enumerate()` | `for i, x in xs` | Gone entirely: every `for` yields `(index, value)`. A chain has no index — needing one is a reason to use a loop |
| `sorted(xs, key=f, reverse=true)` | `xs.sort(f, reverse=true)` | The key is the operand, so it is positional; `reverse=` is a stable descending sort, `.reverse()` is not |
| `xs.sort(key=f)` / `xs.reverse()` (Python's in-place) | `xs = xs.sort(f)` / `xs = xs.reverse()` | **Silent change:** Oro's `sort`/`reverse` return a new collection and do not mutate — rebind. There is no in-place form; `sorted`/`sort_in_place`/`reversed` all raise, naming these. |
| `min(a, b)` / `max(a, b)` | `clamp(v, min=…, max=…)` for a bound; `[a, b].min()` for two values | The scalar form is **cut**; `min`/`max` are reductions (`xs.min()`) |
| `sorted("ba")` / `min(b"ba")` | `"ba".to_list().sort(x => x)` | A `str` and a `bytes` are not collections; `to_list()` is the bridge |

`f"{x}"` is unchanged and is usually the better replacement for `str(x)` in
string building — it also still runs under CPython, which keeps those programs
inside the differential corpus.

## Known limitations

Stated plainly:

- **Reference cycles leak.** Oro reclaims memory by reference counting and has no
  cycle collector, so an object graph with cycles is not freed. This is the same
  trade-off Swift and Rust's `Rc` make. Non-cyclic data (the common case) is
  released promptly and deterministically.
- **You cannot mutate a captured variable from a nested function.** Reading an
  enclosing scope's variables works; rebinding one from an inner function does
  not (and there is no `nonlocal`). Use `global` for module state, hold the
  state on an object, or restructure. Assigning to a name that also exists at
  module scope makes it *local* (exactly as in Python) — Oro turns the resulting
  unbound-variable error into a message that explains the fix.
- **Consuming an infinite generator hangs.** A generator passed to a builtin
  (`sum`, `sort`, `min`, …) or used to start a chain is drained eagerly, so
  `sum(forever())` never returns rather than failing. Inside a `for` loop it
  stays lazy, as it always was.
- **`proc.run` retains at most 64 MiB per stream.** Capture is no longer
  optional, so an unbounded buffer would let a chatty child exhaust memory. Past
  the cap the stream still flows to the terminal in full, only the retained copy
  stops growing, and `.truncated` on the result is `true`.
- **Workers share nothing.** Scaling past one core means N processes
  (`net.listen(addr, reuseport=true)`), and each has its own heap: module-level
  state is per-worker, so counters, caches, in-memory sessions and rate limits
  are all N-way split. Cross-process state needs a database or a file. See
  [Scaling past one core](#scaling-past-one-core-n-processes-one-port).
- **A chain is one eager pass, not a lazy pipeline.** Runs of `map`/`filter`
  are fused, so a chain walks its receiver once and builds one collection
  rather than one per step, and a `first()`/`take(n)`/`find`/`any`/`all` at the
  end stops that pass early. But the pass still happens: the receiver is
  materialised up front, a barrier step (`sort`, `unique`, `chunk`,
  `flatten`, …) materialises again, and nothing is computed on demand. So
  `g().map(f).first()` still drains `g` in full before the chain starts —
  starting a chain on a generator materialises it, and an infinite one hangs.
  Generators remain the lazy escape hatch, and they are lazy in a `for` loop
  and nowhere else.
- **A lambda cannot appear inside an f-string field.** `f"{xs.map(x => x)}"` is
  rejected with a message telling you to bind it to a name first. f-string
  fields are parsed at code-generation time rather than by the parser, so the
  symbol pass has to parse them a second time to see the names inside — and the
  lambda *it* sees is a different node from the one codegen emits, so the scope
  it assigns never reaches the emitted lambda. The real fix is to parse f-string
  fields into the AST like any other expression, which is also what CPython
  moved to in 3.12.
- **Lambda parameters are plain names only** — no defaults and no annotations,
  and the body is a single expression. Anything more is a `def`. With no
  defaults, every lambda parameter is positional-only.
- **Streams take `bytes`, and `print` takes `str`.** `sys.stdout.write("hi")`
  is a `TypeError`; write `print("hi")`, or `sys.stdout.write(b"hi")`, or
  `sys.stdout.write(s.to_bytes())`. The two reach fd 1 in program order — they
  just do not accept the same argument. See
  [The io protocol](#the-io-protocol).
- **There is no `seek`/`tell`, and no read-write mode.** Read-write without
  random access is nearly useless (you can append, or reopen), so `"rw"` is
  really a request for `seek`, which is a genuine building block and should
  exist before the 1.0 freeze. It is deferred rather than cut: every reader
  buffers, so a seek has to invalidate that buffer, and getting that wrong
  produces stale reads that look like data corruption.
- **Output is never block-buffered.** Oro flushes stdout as it goes, so it behaves
  like `python3 -u`. CPython block-buffers when stdout is a pipe, which means a
  program that mixes `print` with an *inherited* subprocess's output can show the
  two interleaved differently under the two runtimes (they agree on a terminal,
  and agree under `python3 -u` anywhere). Only the interleaving differs; every
  line, and each stream's own order, is identical.
- **There is one encoding, and `sys.path` cannot be mutated.** UTF-8 is it:
  no `encoding=` argument exists anywhere, and other encodings are a library
  written in Oro, later. Modules resolve against the one documented search path
  (the script's directory) with no runtime path changes.
- **The stdlib is deliberately small.** `io`, `json` and `http` ship, written
  in Oro on top of the frozen core (with `json`'s per-byte codec in Rust
  underneath it, and `io`'s two primitives likewise); data structures like a
  `Set` class and modules like `csv` are the intended growth area — written in
  Oro, not baked into the runtime.

## The corpus: CPython as an oracle

`corpus/core/*.oro` are small programs, each paired with a `.expected` file.
Crucially, **those `.expected` files are generated by running the program under
CPython 3.12, not by running Oro** (`corpus/oracle.sh` regenerates them). That
makes the corpus an *independent* check on correctness.

Two mechanical renames sit on either side of that run, because Oro spells the
three literals `true`/`false`/`null`: the program's literals become Python's
before CPython sees the source, and CPython's become Oro's before the output is
saved. Both are total and 1:1, so they add nothing a reviewer has to trust — but
they do impose one rule, which `oracle.sh` enforces by tokenising every program
and refusing it otherwise: **an oracled program may not put the words
`True`/`False`/`None` inside a string literal**, because `print("True")` and
`print(True)` produce the same bytes and the outbound rename cannot tell them
apart. Write the word lowercase, or move the program to `divergence/`.

This matters because Oro's own unit-test fixtures are generated from Oro's own
output — they can catch a *regression* (output changed) but can never catch Oro
being *wrong* from the start, because the "expected" value is whatever Oro
already produced. The CPython corpus has no such blind spot: if Oro disagrees
with CPython, Oro is wrong.

It earned its keep immediately. On first run against the corpus, three real bugs
surfaced that **155 internal tests had missed** — `sorted()` was absent, and
`list.sort()` / `list.reverse()` were unimplemented. Every one was a case where
Oro's own fixtures happily agreed with Oro's own (wrong or missing) behavior.

`corpus/divergence/` holds programs that intentionally behave differently from
Python, or that CPython cannot run at all (`proc`, `=>`, `.map`/`.filter`, the
`to_` casts). The oracle cannot generate their `.expected` files, so those are
written and reviewed by hand — **and this is the corpus's one blind spot**: a
reviewed baseline can catch a regression but cannot catch Oro being wrong from
the start, which is exactly the weakness the CPython oracle exists to remove. It
has already bitten once (a baseline was first captured showing a
`FileNotFoundError` from a test that had silently stopped working). Two
consequences follow: keep as much as possible in `core/` where CPython still
checks it — when a test only needs a cast, split the file rather than moving the
whole thing — and read every divergence baseline as if reviewing a diff, because
that review is the only thing standing behind it. Where a program diverges only
in *spelling* — `strip(side="left")` for `lstrip`, `find(sub, reverse=true)` for
`rfind`, `xs.sort(x => x)` for `sorted(xs)` — it carries a `.twin.py`: the same
program in CPython's names, whose output *is* the `.expected`. That puts the
oracle back behind a file CPython cannot run, and the twin is the review. It is
what the builtin/collection-method cut was paid for with: four programs left
`core/` for `divergence/` and every one took a twin along, so their baselines
are still CPython's own output rather than Oro's. `corpus/known-failing/` is the opposite — it
holds programs that are *correct Python which Oro currently gets wrong* and that
we intend to fix. `run.sh` reports its count separately and never fails the build
on it, so those bugs stay **visible** instead of being quietly omitted; when a
fix lands the file moves to `core/`. Keeping the two directories separate matters:
`divergence/` is a design record, `known-failing/` is a bug list, and mixing them
would ruin both.

Some features can't be checked differentially and are covered by manual
differential runs and unit tests instead: anything invocation- or
environment-dependent (`sys.argv`, `sys.exit`, `os.getcwd`, `os.environ`,
`os.listdir` — and note CPython's `argv[0]` under the oracle is `-c`, not the
script), and user-module imports (the helper's extension is `.oro` vs CPython's
`.py`, and CPython searches the working directory rather than the script's).

## Formatting

Oro ships its own canonical formatter, `gofmt`-style: no options, one output.

```sh
oro fmt file.oro            # print the formatted source
oro fmt --write file.oro    # rewrite in place
oro fmt --check file.oro    # exit 1 if it is not already formatted
```

It exists because Oro is no longer parseable by Python's tools — `x => x * 2` is
a Python `SyntaxError`, so `black` and `ruff` cannot read an Oro file that uses a
lambda. (The `to_` casts and `proc` still *parse* as Python; only `=>` breaks it.)

Comments are preserved. A comment the formatter cannot place unambiguously —
inside a multi-line bracketed expression, or trailing a line it did not start —
makes `oro fmt` refuse to run and name the line, rather than move or drop it.
`r"..."` strings keep their raw form, so regexes do not reformat into
backslash-doubled soup.

Collection literals keep the author's line breaks, the way `gofmt` does with
composite literals. One bit of the source decides it — is there a newline
directly after the opening bracket?

```python
codes = [200, 404, 500]     # stays on one line, however long

codes = [                   # stays broken, however short
    200,
    404,
    500,
]
```

The broken form is one element per line with a trailing comma; a nested literal
decides for itself, so a broken one can sit inside a one-line one and the other
way round. This is still "no options, one output" — the output is a
deterministic function of the input, and the input just carries one more bit of
signal. There is no line-width setting and no reflowing: `oro fmt` never
measures a line and never decides to break one for you.

Argument lists and `def` parameter lists always print on one line. An *argument*
that is a literal still keeps its own breaks, which is what makes a long table
readable:

```python
configure({
    "retries": 3,
    "timeout": 30,
})
```

## Building and running

```sh
cargo build --release          # builds the `oro` binary at target/release/oro
./target/release/oro file.oro  # compile and run a program

cargo test                     # unit + integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt                      # the Rust source is rustfmt-formatted (rustfmt.toml)
cargo fmt --check              # CI enforces this — a build fails if it is unformatted

# Inspection modes:
./target/release/oro --tokens file.oro   # dump the token stream
./target/release/oro --ast    file.oro   # dump the parsed AST
```

Run the CPython differential corpus:

```sh
./corpus/oracle.sh   # (re)generate .expected from CPython 3 — needs python3
./corpus/run.sh      # run every core program through oro and diff vs CPython
```

## Examples

`examples/` holds eight runnable programs. Each is one screen, and each opens
with a header saying what it demonstrates and how to run it. The first six are
the language; the last two are the HTTP stack.

```sh
./target/release/oro examples/files.oro          # a file, read line by line, counted, written back out
./target/release/oro examples/json_api.oro       # JSON parsed, reshaped by the collection protocol, emitted
./target/release/oro examples/concurrency.oro    # a worker pool: `spawn`, two channels, `join`
./target/release/oro examples/text.oro           # the string surface, and where its names diverge from Python's
./target/release/oro examples/cli.oro README.md  # `sys.argv`, a diagnostic on stderr, an exit code that means something
./target/release/oro examples/classes.oro        # a value type: `__str__`, `__repr__`, `__eq__`, `__lt__`, one subclass
```

`examples/server.oro` is a working HTTP server in one screen: five routes, a
logging middleware written as a function, and no framework holding any of it.
`examples/client.oro` is the other half — with no argument it starts a server
on an ephemeral port in one green thread and fetches from it in another, so it
needs no network at all:

```sh
./target/release/oro examples/client.oro            # dials a server it starts
./target/release/oro examples/client.oro http://a-url/path
```

```sh
./target/release/oro examples/server.oro            # 127.0.0.1:8080
./target/release/oro examples/server.oro 0.0.0.0:9000
```

Then, from another terminal:

```sh
curl localhost:8080/                    # text
curl localhost:8080/json                # JSON, encoded from a dict
curl localhost:8080/hello/ada           # a bound path segment -> req.params
curl -d '{"n": 1}' localhost:8080/echo  # a request body, decoded and echoed
curl -i localhost:8080/boom             # a handler that raises
```

```
oro http
{"server":"oro","uptime":7.132592982999999,"query":{}}
hello, ada
{"you_sent":{"n":1}}
HTTP/1.1 500 Internal Server Error
```

`/boom` is the interesting one: the handler raises `KeyError`, that request
gets a 500, the server logs one line, and nothing else notices — not the
connection, which serves the next request on it, and not the other clients.

### The part that matters

```sh
curl localhost:8080/slow &   # sleeps two seconds
curl localhost:8080/json     # answered immediately, in the middle of it
```

The server's own log is the proof, and it is a log of *one OS thread*:

```
  7.156  -->  GET /slow
  7.363  -->  GET /json
  7.363  <--  200 /json
  7.569  -->  GET /hello/world
  7.570  <--  200 /hello/world
  9.159  <--  200 /slow
```

Two whole request/response cycles opened and closed inside `/slow`'s two
seconds. There is no `async` in that program, no `await`, no callback and no
event loop written by hand: `slow` is an ordinary function that calls
`time.sleep(2)`, and `time.sleep` parks the green thread running it rather
than the process. The same is true of every socket read and write in the
stack — which is why `http.serve` is a `while true:` around `accept()` and a
`spawn` per connection, and why that is enough.

### Scaling past one core: N processes, one port

Everything above happens on **one** OS thread, and that is the design rather
than a stage it grows out of: every Oro value is an `Rc`, so a value can never
cross a thread, so a VM owns its own scheduler and its own ready queue. More
cores therefore means more *processes*, not more threads — and the way N
processes serve one port is `SO_REUSEPORT`:

```python
# worker.oro — run as many copies of this as you have cores.
import net

def handle(conn):
    conn.read_until(b"\r\n\r\n", 65536)
    conn.write(b"HTTP/1.1 204 No Content\r\n\r\n")
    conn.close()

ln = net.listen("0.0.0.0:8080", reuseport=true)
while true:
    spawn(handle, ln.accept())
```

`reuseport=true` is the entire change to the program. Every worker runs the same
code and binds the same address; the **kernel** hashes each incoming
connection's four-tuple and hands it to exactly one of them. There is no proxy
in front, no shared accept queue and no thundering herd — the connection is
delivered to one worker, which then serves it with the green threads it already
had.

Run them the way you run any other set of processes. Nothing in Oro starts them,
on purpose — a shell, a systemd unit, a supervisor or a container orchestrator
all do it better than a language runtime would, and they handle restarts too:

```sh
# One worker per core, in a shell.
for i in $(seq "$(nproc)"); do
  ./target/release/oro worker.oro &
done
wait
```

```ini
# Or as a systemd template unit — `systemctl start oro-worker@{1..8}`.
[Service]
ExecStart=/usr/local/bin/oro /srv/app/worker.oro
Restart=always
```

**`http.serve` does not take the option yet**, so `examples/server.oro` above is
a one-worker program and N copies of it collide on the port. `serve` calls
`net.listen` for you and has nowhere to pass `reuseport=` through; giving it one
(as `http.serve(addr, handler, workers=N)`) is a separate, small change tracked
in `docs/stdlib-server-design.md` M6. Until then, scaling out means owning the
listener yourself — which is the loop above, and is what `serve` is doing
underneath in any case.

Restarts are the quiet win. Because every worker holds the port independently,
you can stop and restart one at a time and the port is never unbound — the
remaining workers keep serving through it, which is a rolling deploy with no
extra machinery.

**Two things to know before you rely on it.**

*Linux only, and it says so.* `reuseport=true` raises an `OSError` on macOS,
the BSDs and Windows rather than binding without the option. This is deliberate
and the error explains itself. macOS and the BSDs define a `SO_REUSEPORT` that
shares a port **without balancing across it** — same name, different feature,
which is why FreeBSD later added the balancing one as `SO_REUSEPORT_LB`.
Accepting the argument there would give you N workers, one of which received
almost everything, and no way to tell from any log. Develop on macOS with a
single worker; scale out on Linux.

*Nothing is shared between workers, and that is your problem to solve.* This is
the real cost of the model and it is worth being blunt about. Each worker is a
separate process with a separate VM and a separate heap. A module-level `dict`
is **per-worker**: a counter counts that worker's requests, a cache is a cache
with N copies and N miss rates, and a rate limiter limits per worker rather than
per client. In-memory sessions mean a client that lands on a different worker on
its next request is logged out — the kernel balances by four-tuple, so it *will*
land elsewhere.

Nothing in Oro fixes this, and nothing is planned to. Cross-process state goes
where cross-process state goes: a database, Redis, or a file, reached over the
same `net` module. If your server genuinely holds mutable state in memory, one
worker is the honest answer and it will serve far more traffic than most people
expect — the demo above overlaps whole request cycles inside a two-second
handler on a single thread.

Ctrl-C on this demo is a hard kill, because the program has nothing but the
accept loop in it. A *graceful* shutdown needs a second task holding the
listener, and `corpus/divergence/60_http_serve.oro` is that program: it starts
`serve` with a `ready` channel, takes the listener back off it, and closes it
with one request in flight and one idle keep-alive connection open — then
checks that the first was answered and the second was not waited for.

## Layout

- `src/lexer/` — hand-written lexer with an INDENT/DEDENT engine (tabs rejected).
- `src/parser/` — Pratt-style parser producing the AST in `src/ast.rs`.
- `src/compiler/` — two-pass compiler: `symbols.rs` (scope/slot resolution) and
  `codegen.rs` (bytecode emission); ops defined in `src/compiler/mod.rs`.
- `src/vm/` — the flat, heap-framed bytecode interpreter, plus `exceptions.rs`
  (the built-in exception hierarchy) and `modules.rs` (`sys`/`os`/`os.path`).
- `src/format.rs` — the f-string format mini-language.
- `src/bigint.rs` — arbitrary-precision integers for overflow promotion.
- `src/value.rs` — the runtime `Value` type and its containers.
- `std/` — the Oro-written half of the standard library (`io`, `json`, `http`),
  baked into the binary by `src/vm/stdlib.rs`.
- `examples/` — eight runnable programs, one screen each: `files`, `json_api`,
  `concurrency`, `text`, `cli` and `classes` for the language, and
  `server`/`client` for the HTTP stack.
- `corpus/` — the CPython-generated differential test suite.
- `docs/reference.md` — the reference guide: every callable with its exact
  signature, the argument rule, and what each cut Python spelling becomes.
  Where it and this README disagree, the reference was checked against the
  binary and wins.

## License

Licensed under either of MIT or Apache-2.0, at your option.
