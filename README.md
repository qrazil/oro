# Oro

Oro (named for the *ouroboros*) is a small, deliberately frozen subset of
Python, implemented in Rust and executed on a bytecode virtual machine. It
compiles to a single self-contained binary with **zero runtime dependencies**
(std only — the lexer, parser, compiler, and VM are all hand-written).

The point of Oro is not to be a bigger Python. It is to be a *smaller* one that
never grows: one way to do each thing, a language and API that freeze, and a
compatibility guarantee that keeps the whole Python tooling ecosystem working
for free.

## The thesis: one way to do each thing, and it freezes

Most languages accrete. Each release adds a second (then third) way to spell
something, and every addition is a permanent tax on everyone who reads code in
that language. Oro takes the opposite bet: pick one spelling for each thing,
then **stop**. The language freezes. The standard API freezes. Features do not
accrete.

The model is [rhai](https://rhai.rs/): a scripting language that has held a
stable 1.x for years without breaking its users. Boring, in the way a load-
bearing tool should be boring. Oro would rather be finished than fashionable.

A frozen language is not a limitation to apologize for — it is the feature. You
can learn all of it. You can hold all of it in your head. Code written today
reads the same as code written years from now, because there is no "modern Oro"
that makes the old way look dated.

## Compatibility: every Oro program is a Python program

**Every valid Oro program is also a valid Python program** (Python 3.6+ for the
core language; the `match` statement, if you use it, requires 3.10+). The
reverse is explicitly **not** promised — Python has enormous surface area Oro
deliberately omits.

This one-directional property is what makes Oro cheap to own:

- **Editors, highlighters, and formatters just work.** Oro is a subset of
  Python's grammar, so every `.oro` file is something `black`, syntax
  highlighters, and language servers already understand. Oro ships no tooling of
  its own and never needs to.
- **CPython is a free correctness oracle.** Because an Oro program is a Python
  program, you can run it under CPython and treat that output as ground truth.
  Oro's test corpus (see [The corpus](#the-corpus-cpython-as-an-oracle)) is
  built on exactly this.

To keep the property intact, Oro is *more* restrictive than Python wherever it
differs — it rejects things Python accepts, never the other way around. Reject-
not-accept is the invariant that guarantees the subset relationship holds.

**One knowing exception:** a multi-segment `import a.b.c` *without* `as` binds
the last segment (`c`), Go-style, where Python binds the first (`a`). So that
one form is not valid-Python-equivalent. Single imports (`import os`) and
aliased ones (`import a.b.c as name`) bind identically in both; use `as` when
you want the guarantee to hold for a dotted import.

## The frozen feature set

Implemented and working today:

- **Values:** `int` (inline `i64`, promoting to arbitrary-precision bignum on
  overflow), `float`, `bool`, `str`, `None`, `list`, `tuple`, `dict`, `set`,
  `range`, and functions (including closures over *read* access).
- **Control flow:** `if` / `elif` / `else`, `while`, `for … in …`,
  `break`, `continue`, `pass`, and `match` (a value-only switch — see below).
- **Functions:** positional params, defaults, `*args`, `**kwargs`, and the call-
  site `*`/`**` unpacking that mirrors them. Deep and mutual recursion work
  (the VM never recurses in Rust — see [Architecture](#architecture)).
- **Classes:** single inheritance, `__init__`/instance attributes/methods,
  class-level attributes, `super()`, `isinstance()`, and a fixed dunder set —
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
- **Modules:** `import a.b.c` / `import x as y`, built-in `sys` and `os`, and
  user modules loaded from the script's directory (run once, cached).
- **File I/O:** `open(path, mode)` (`r`/`w`/`a`, UTF-8 text) with
  `read`/`readline`/`readlines`/`write`/`close` and line iteration; files close
  deterministically at end of scope (no `with`).
- **f-strings** with the full format mini-language: `{x:.2f}`, `{n:05d}`,
  `{x:,}`, alignment (`<^>`), sign/`#`/`0` flags, the `!r`/`!s` conversions, and
  nested specs like `{x:.{p}f}`.
- **Builtins:** `print`, `len`, `range`, `str`, `repr`, `int`, `float`, `bool`,
  `type`, `abs`, `min`, `max`, `sum`, `sorted`, `isinstance`, `open`, plus the
  common `str`/`list`/`dict` methods.
- **Python truthiness** and Python's cross-type numeric equality (`1 == 1.0 ==
  True`).

### What is cut, and why

Each of these is omitted on purpose. The reason matters more than the list.

- **No walrus (`:=`).** Assignment is a statement; a second assignment operator
  that also returns a value is precisely the "more than one way" the thesis
  rejects.
- **No comprehensions.** `[f(x) for x in xs if p(x)]` is a second iteration
  construct layered on top of `for`. One loop syntax is enough.
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
- **No `lambda`.** A second, weaker way to define a function. Use `def`.
- **No `nonlocal`.** Mutating an enclosing function's locals through a keyword is
  a rare need with an outsized implementation and readability cost; use `global`
  or an object. (Reading captured variables works fine.)
- **No or-patterns in `match` (`case a | b:`).** See below.
- **No semicolons and no inline suites (`if x: y`).** Both are second ways to
  write a block, and together they are much of why Python needs autoformatters
  at all. One statement per line; one block form.
- **No sets.** A set is a dict with no values, and the uses that matter are
  already covered: `{k: True}` with `k in d` gives O(1) membership, and a dedup
  loop keeps insertion order (unlike `set()`, whose arbitrary order is a real
  bug source). Only set *algebra* (union/intersection/difference) is a genuine
  gap, and that is rare in scripting — when wanted, it belongs in a stdlib `Set`
  class written in Oro, the same split as JSON and CSV (primitives in Rust,
  everything else in Oro), not in the frozen core. `{1, 2, 3}` and `set()` each
  give an error pointing at a dict or a list. **Tuples stay** — they are the
  only hashable composite, so `counts[(host, port)]` has no substitute, and they
  are load-bearing for multiple return, `a, b = b, a`, and `*args`.

Also cut: bare `except:`, `try/except/else`, `from x import y`, `import *`,
`__new__`/`__getattr__`/`__setattr__`/`__slots__`, and class `metaclass=`.

## Deliberate divergences from Python

Oro is a subset, but in four places it deliberately behaves *differently* from
Python. Each divergence is a place where Python made a choice it could not later
reverse, and Oro — starting fresh, with a single implementation — makes the
choice Python would arguably prefer.

### Block scope on `if` / `for` / `while`

Names bound inside a block do **not** leak out of it:

```python
for i in range(10):
    last = i
print(last)   # NameError in Oro; prints 9 in Python
```

Python leaks loop and branch variables into the enclosing function scope. It
fixed exactly this for comprehensions in Python 3 (comprehension variables no
longer leak) but could not retrofit the fix to `for`/`while`/`if` without
breaking decades of code that relies on the leak. Oro has no such legacy, so it
gives every block real lexical scope. This is intentional and correct; it is not
a bug.

### No `with` — deterministic cleanup instead

Oro has no `with` statement because it does not need one. Reference counting
plus block scope means an object is released the moment its last reference goes
out of scope — deterministically, at end of block:

```python
def read(path):
    f = open(path)      # opened here
    return f.read()     # f's last reference dies at return → file closed
```

`with` exists in Python because the *language spec* refuses to promise *when* an
object is destroyed — CPython refcounts, but PyPy and Jython use tracing garbage
collectors, so file handles might close "eventually." `with` is the workaround
for an unspecified destruction time. Oro has exactly one implementation and can
therefore promise the timing, which removes the reason `with` exists.

(The flip side is honest: refcounting alone cannot reclaim reference *cycles* —
see [Known limitations](#known-limitations).)

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

Allowed patterns are literals (`int`, `float`, `str`, `True`, `False`, `None`),
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
- **`Rc` reference counting, not `Arc`.** The runtime is single-threaded, so it
  pays for no atomic operations.
- **`i64` inline, bignum on overflow.** Small integers are unboxed `i64`;
  arithmetic that would overflow transparently promotes to an arbitrary-
  precision integer. You never silently wrap.
- **UTF-8 strings with an ASCII fast path.** Every string is UTF-8 and carries
  an `is_ascii` flag computed once at creation. Indexing and length are O(1) for
  ASCII strings and fall back to correct (O(n)) char handling otherwise.

## Standard library surface

Two built-in modules ship in the core; everything else is meant to grow as Oro
written on top of it.

- **`sys`** — `argv` (`argv[0]` is the script), `exit(code)` (raises
  `SystemExit`; sets the process exit status if uncaught), `platform`,
  `stdin`/`stdout`/`stderr`.
- **`os`** — `environ`, `getcwd()`, `listdir()`, `remove()`, `mkdir()`, and
  `path`.
- **`os.path`** — `exists`, `isfile`, `isdir`, `join`, `basename`, `dirname`,
  `splitext`.
- **`open(path, mode)`** — `r`/`w`/`a`, UTF-8 text. The file object has
  `read()`, `readline()`, `readlines()`, `write()`, `close()`, and iterates line
  by line. It closes when its last reference drops (see the `with`-free file
  lifetime above). Missing files and permission errors raise `FileNotFoundError`
  / `PermissionError`.

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
- **An instance inside a stringified container shows the default form.**
  `print(obj)`, `str(obj)`, `repr(obj)`, and f-strings run an instance's
  `__str__`/`__repr__` correctly, but an instance *nested* in a container that is
  itself stringified — `print([obj])` — shows `<Class object>` instead. Container
  stringification happens in native code that cannot re-enter the VM to run each
  element's dunder; doing it properly means rewriting recursive `repr` as an
  iterative state machine over the frame stack, deferred for now.
- **`break`/`continue` do not run an enclosing `finally`.** A `finally` runs on
  normal completion, on a handled or propagating exception, and on `return` — but
  a `break` or `continue` that jumps out of a `try` skips its `finally`. Rare;
  documented rather than fixed.
- **Binary/encoding file modes, and `sys.path` mutation, are unsupported.**
  `open` is UTF-8 text only (`r`/`w`/`a`); modules resolve against the one
  documented search path (the script's directory) with no runtime path changes.
- **No stdlib beyond `sys`/`os`.** Data structures like a `Set` class, and
  modules like `json`/`csv`, are the intended growth area — written in Oro on top
  of the frozen core, not baked into it.

## The corpus: CPython as an oracle

`corpus/core/*.oro` are small programs, each paired with a `.expected` file.
Crucially, **those `.expected` files are generated by running the program under
CPython 3.12, not by running Oro** (`corpus/oracle.sh` regenerates them). That
makes the corpus an *independent* check on correctness.

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
Python (the block-scope and `with`-free examples above); it is excluded from the
differential run for that reason.

Some features can't be checked differentially and are covered by manual
differential runs and unit tests instead: anything invocation- or
environment-dependent (`sys.argv`, `sys.exit`, `os.getcwd`, `os.environ`,
`os.listdir` — and note CPython's `argv[0]` under the oracle is `-c`, not the
script), and user-module imports (the helper's extension is `.oro` vs CPython's
`.py`, and CPython searches the working directory rather than the script's).

## Building and running

```sh
cargo build --release          # builds the `oro` binary at target/release/oro
./target/release/oro file.oro  # compile and run a program

cargo test                     # unit + integration tests
cargo clippy --all-targets -- -D warnings

# Inspection modes:
./target/release/oro --tokens file.oro   # dump the token stream
./target/release/oro --ast    file.oro   # dump the parsed AST
```

Run the CPython differential corpus:

```sh
./corpus/oracle.sh   # (re)generate .expected from CPython 3 — needs python3
./corpus/run.sh      # run every core program through oro and diff vs CPython
```

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
- `corpus/` — the CPython-generated differential test suite.

## License

Licensed under either of MIT or Apache-2.0, at your option.
