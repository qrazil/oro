# Oro benchmark results

Run with `./bench/run.sh` (see `--help`). Every program prints a result and the
harness checks oro and CPython agree before reporting a time.

## Machine

| | |
|---|---|
| CPU | Intel Core i7-8750H @ 2.20GHz (12 threads) |
| OS | Linux 6.14.5 (Fedora 40) |
| rustc | 1.97.1 |
| CPython | 3.12.10 |
| method | best-of-N wall clock (N=3 in the log below, N=5 for the summary) |

## The programs

| bench | what it stresses |
|---|---|
| `fib` | the call path: `fib(27)`, ~400k frame push/bind/return cycles |
| `loop` | raw dispatch: a 3M-iteration `while` with integer arithmetic |
| `strjoin` | 200k f-string formats into a list, then `join` |
| `dictops` | 500k integer-keyed dict writes, then a full iteration + lookup scan |
| `dictstr` | string-keyed dicts: 200k distinct keys, then 500k hits on one small record |
| `oo` | attribute load/store, method calls, construction, `super()` |
| `genpipe` | three chained generators over 300k elements (frame suspend/resume) |
| `exc` | raise/catch on 2/3 of 200k iterations, with a `finally` on every one |
| `listbuild` | 400k list appends, then indexed read-modify-write |
| `builtins` | 4 global lookups + 4 native calls per iteration, 300k iterations |
| `chain` | the collection protocol (`.filter`/`.map`/`.reduce` with `=>`) — oro-only, CPython twin in `chain.py` |
| `json` | `json.parse` + `json.stringify` over five payload shapes — oro-only, CPython twin in `json_twin.py` |

## Where things stand

Two optimization passes have run. Pass two is `perf/pass-two`, rebased onto
`feat/sane-defaults` after the JSON work landed and **re-measured against it** —
the baseline below is that branch's own binary, not pass one's, so the slice
fix and the iterative teardown that arrived in the meantime are already in the
"before" column. Interleaved A/B, both binaries pinned to one core,
best-of-13:

| bench | baseline | after pass two | improvement | vs CPython (baseline → now) |
|---|---|---|---|---|
| fib | 0.1657s | **0.0844s** | **-49%** | 4.07x → **1.95x** |
| loop | 0.6494s | **0.2239s** | **-66%** | 1.41x → **0.50x** |
| strjoin | 0.1257s | **0.1035s** | **-18%** | 2.00x → **1.62x** |
| strops | 0.3045s | **0.2193s** | **-28%** | 1.84x → **1.34x** |
| dictops | 0.3679s | **0.2685s** | **-27%** | 1.81x → **1.32x** |
| dictstr | 0.5082s | **0.3534s** | **-30%** | 1.95x → **1.32x** |
| oo | 0.3374s | **0.1906s** | **-44%** | 2.47x → **1.39x** |
| genpipe | 0.1654s | **0.0978s** | **-41%** | 2.74x → **1.66x** |
| exc | 0.1377s | **0.0808s** | **-41%** | 1.41x → **0.82x** |
| listbuild | 0.3082s | **0.1615s** | **-48%** | 1.68x → **0.88x** |
| builtins | 0.2499s | **0.1685s** | **-33%** | 0.96x → **0.64x** |
| chain | 0.1585s | **0.1223s** | **-23%** | 2.53x → **1.92x** |
| json | 0.0950s | **0.0938s** | -1% | 0.70x → **0.70x** |
| **mean** | | | **-34.4%** | |

**-37.2%** across the twelve interpreter benchmarks; `json` is included for
honesty and barely moves, because its time is inside a native codec that this
pass never touches. Five benchmarks are now faster than CPython 3.12 outright —
`loop` at 0.50x, `builtins` at 0.64x, `json` at 0.70x, `exc` at 0.82x,
`listbuild` at 0.88x — and nothing in the suite is worse than 1.95x.

The A/A control for that session — the baseline binary against a byte-identical
copy of itself, same harness, same pinning — was **mean +0.16%, worst single
benchmark 3.38%** (`json`, the shortest program in the suite at 0.09s and the
noisiest for it).

The per-step tables in "Pass two — per-optimization log" below are the numbers
each change was measured at when it was made, against pass one's binary. They
have not been restated against the new baseline: each is a comparison of two
binaries that differed by exactly one commit, which is what makes it evidence,
and re-running them all against a moved baseline would only add noise. The
table above is the one that says where the branch actually lands.

## Where pass one left things

Best-of-5, against the pre-optimization baseline further down.

| bench | baseline | now | improvement | vs CPython (was → now) |
|---|---|---|---|---|
| fib | 0.295s | **0.160s** | **-46%** | 7.20x → **3.90x** |
| loop | 0.897s | **0.605s** | **-33%** | 2.13x → **1.39x** |
| strjoin | 0.144s | **0.115s** | **-20%** | 2.36x → **1.89x** |
| dictops | 0.430s | **0.352s** | **-18%** | 2.21x → **1.81x** |
| oo | 0.482s | **0.319s** | **-34%** | 3.80x → **2.51x** |
| genpipe | 0.215s | **0.164s** | **-24%** | 3.71x → **2.83x** |
| exc | 0.204s | **0.130s** | **-36%** | 2.19x → **1.35x** |
| listbuild | 0.416s | **0.302s** | **-27%** | 2.39x → **1.72x** |
| builtins | 0.265s¹ | **0.233s** | **-12%** | 1.10x → **0.95x** |
| chain | 0.220s | **0.148s** | **-33%** | 3.67x → **2.47x** |
| json | 6.52s² | **0.101s** | **-98%** | 42x → **0.65x** |

² `json` was added last, for the same reason `builtins` was: nothing in the
suite touched the standard library's own hot path, so nobody had measured it.
Its "baseline" is the Oro-written codec it replaced. See the section below.

¹ `builtins` was added part-way through (step 10 explains why); its "baseline"
is its first measurement, taken before the change it motivated.

`size_of::<Op>()` = **8** (was 48), and `Op` is now `Copy`.
Binary: 2.12 MB → 2.59 MB, all of it from `opt-level = 3`.

## Baseline — `opt-level = "s"` (commit before any optimization)

| bench | oro | CPython | oro/CPython |
|---|---|---|---|
| fib | 0.295s | 0.041s | 7.20x |
| loop | 0.897s | 0.422s | 2.13x |
| strjoin | 0.144s | 0.061s | 2.36x |
| dictops | 0.430s | 0.195s | 2.21x |
| oo | 0.482s | 0.127s | 3.80x |
| genpipe | 0.215s | 0.058s | 3.71x |
| exc | 0.204s | 0.093s | 2.19x |
| listbuild | 0.416s | 0.174s | 2.39x |
| chain | 0.220s | 0.060s | 3.67x |

`size_of::<Op>()` = 48, `size_of::<Value>()` = 16.
(`builtins` did not exist yet — see step 10.)

## Pass one — per-optimization log

Each step is best-of-3, compared against the step above it. Regressions and
no-ops are kept in the log on purpose.

### 1. `opt-level = "s"` → `3`

Binary 2.12 MB → 2.58 MB (+464 KB). `size_of::<Op>()` unchanged at 48.

| bench | before | after | delta |
|---|---|---|---|
| fib | 0.295s | 0.235s | **-20%** |
| loop | 0.897s | 0.749s | **-17%** |
| strjoin | 0.144s | 0.130s | -10% |
| dictops | 0.430s | 0.386s | -10% |
| oo | 0.482s | 0.408s | **-15%** |
| genpipe | 0.215s | 0.184s | **-14%** |
| exc | 0.204s | 0.158s | **-23%** |
| listbuild | 0.416s | 0.332s | **-20%** |
| chain | 0.220s | 0.180s | **-18%** |

The single cheapest change in the whole list: one word of TOML for 10-23%.

### 2. `Op` 48 → 24 bytes (box the `BuildClass` payload)

`BuildClass` carried an inline `Vec<Rc<str>>`, which alone set `size_of::<Op>()`
to 48. Boxing it into `Rc<ClassSpec>` drops that to **24** — not 16: the
remaining 16-byte payloads are the `Rc<str>` names (`LoadGlobal`, `LoadAttr`,
`StoreAttr`, `ImportModule`) and the two-`usize` variants (`MatchDispatch`,
`SetupLoop`), so 24 is the floor until names are interned (step 3).

| bench | before | after | delta |
|---|---|---|---|
| fib | 0.235s | 0.230s | -2% |
| loop | 0.749s | 0.727s | -3% |
| strjoin | 0.130s | 0.125s | -4% |
| dictops | 0.386s | 0.373s | -3% |
| oo | 0.408s | 0.404s | -1% |
| genpipe | 0.184s | 0.176s | -4% |
| exc | 0.158s | 0.159s | +1% (noise) |
| listbuild | 0.332s | 0.316s | **-5%** |
| chain | 0.180s | 0.178s | -1% |

Small but uniformly in the right direction, which is what halving the width of
the per-dispatch `clone` and doubling instruction-cache density should look
like. The real payoff is step 3, which this unblocks.

### 3. `Op` 24 → 8 bytes and `Copy` (intern every payload)

Names (`LoadGlobal`/`LoadAttr`/`StoreAttr`/`ImportModule`) move into a
per-`CodeObject` `names: Vec<Rc<str>>`; class descriptions into `classes`; the
two instructions that genuinely need two operands (`MatchDispatch`,
`SetupLoop`) into `pairs`. Every remaining operand is a `u32`, so `Op` is one
8-byte `Copy` word — six instructions per cache line instead of one, `ops[pc]`
is a shift instead of a multiply, and the per-dispatch instruction read stops
being a refcount bump.

| bench | before | after | delta |
|---|---|---|---|
| fib | 0.230s | 0.226s | -2% |
| loop | 0.727s | 0.620s | **-15%** |
| strjoin | 0.125s | 0.122s | -2% |
| dictops | 0.373s | 0.363s | -3% |
| oo | 0.404s | 0.379s | **-6%** |
| genpipe | 0.176s | 0.161s | **-9%** |
| exc | 0.159s | 0.154s | -3% |
| listbuild | 0.316s | 0.308s | -3% |
| chain | 0.178s | 0.175s | -2% |

Biggest single win on the dispatch-bound benchmark, exactly where a denser
instruction stream should show up. `fib` barely moves — its cost is not fetch,
it is the two heap allocations per call (Tier 2).

### 4. One borrow for fetch + pc advance

The fetch borrowed the frame stack immutably to read the instruction, dropped
the borrow, then took a second mutable borrow purely to write `pc + 1`. Now
that `Op` is `Copy`, the read ends its own borrow and both fit in one
`last_mut()`.

| bench | before | after | delta |
|---|---|---|---|
| fib | 0.226s | 0.220s | -3% |
| loop | 0.620s | 0.610s | -2% |
| strjoin | 0.122s | 0.119s | -2% |
| dictops | 0.363s | 0.353s | -3% |
| oo | 0.379s | 0.384s | +1% (noise) |
| genpipe | 0.161s | 0.157s | -2% |
| exc | 0.154s | 0.141s | **-8%** |
| listbuild | 0.308s | 0.298s | -3% |
| chain | 0.175s | 0.167s | **-5%** |

### 5. REVERTED — lazy error spans

**Tried and rejected.** The loop reads `frame.code.spans[pc]` and stores
`self.line`/`self.col` on every instruction, purely so a diagnostic can name a
position. Replacing that with a lazily-resolved `(err_code, err_pc)` pair —
storing only the pc per instruction and refreshing the cached code object with
an `Rc::ptr_eq` check when the fetch crosses into a different one — measured as
a **regression**, and not a small one:

| bench | before | after | delta |
|---|---|---|---|
| fib | 0.220s | 0.222s | +1% |
| loop | 0.610s | 0.685s | **+12%** |
| genpipe | 0.157s | 0.170s | **+8%** |

The premise was wrong. `spans` is walked in lockstep with `ops`, so the load is
prefetched and effectively free, and the two adjacent `u32` stores fold into
one. The replacement swapped that for a dependent load of `Option<Rc<…>>`, a
pointer compare and a branch — strictly more work on the hot path to save
something that was not costing anything.

(Correctness of the reverted version was verified first: byte-identical
diagnostics across eight targeted error programs — including the ones where the
faulting frame is popped before the error is built, such as `__init__() should
return None` and an f-string format spec applied after `__str__` returns — plus
all 65 corpus programs. It was reverted purely on the numbers.)

### 6. REVERTED — collapsing repeated frame lookups in hot opcodes

**Tried and rejected as noise.** The hot opcodes each call `self.top()` two or
three times (`LoadFast` reads `locals` then pushes; every binary operator pops
twice; the conditional jumps pop then set `pc`), and each call is a
bounds-checked index into `frames`. Rewriting `LoadConst`, `LoadFast`,
`StoreFast`, `Pop`, `Dup`, `DupTwo`, the arithmetic operators, `Compare` and
the four conditional jumps to take exactly one borrow produced:

| bench | before | after | delta |
|---|---|---|---|
| fib | 0.220s | 0.217s | -1% |
| loop | 0.610s | 0.609s | 0% |
| dictops | 0.353s | 0.364s | +3% |
| oo | 0.384s | 0.378s | -2% |
| exc | 0.141s | 0.147s | +4% |
| listbuild | 0.298s | 0.314s | +5% |

Four benchmarks better, three worse, everything inside ±5% — noise, in both
directions. LLVM had already common-subexpressioned the repeated `last_mut()`
within a match arm; the bounds check it leaves behind is one predictable
compare. Reverted rather than kept, because a wash is not worth the extra
`expect("operand stack underflow")` boilerplate at a dozen call sites.

### 7. Frame pooling

Retired frames keep their buffers instead of freeing them, so a call refills a
`locals` vector and an operand stack rather than asking `malloc` for the two
blocks a return just handed back. Capped at 128 frames; buffers are emptied at
retirement, not on reuse, so pooling never extends a value's lifetime.

| bench | before | after | delta |
|---|---|---|---|
| fib | 0.220s | 0.208s | **-5%** |
| loop | 0.610s | 0.607s | 0% |
| strjoin | 0.119s | 0.116s | -3% |
| dictops | 0.353s | 0.357s | +1% |
| oo | 0.384s | 0.374s | -3% |
| genpipe | 0.157s | 0.158s | +1% |
| exc | 0.141s | 0.140s | -1% |
| listbuild | 0.298s | 0.304s | +2% |
| chain | 0.167s | 0.164s | -2% |

Less than hoped, and the reason is instructive: `locals` and `stack` were not
the only two allocations per call. `bind_call` also collected a
`Vec<&ParamInfo>` and a `vec![None; n]` on *every* call — see step 8.

### 8. A static argument-binding path

`bind_call`'s doc comment described two paths — a static one binding positional
arguments straight into their slots, and a dynamic one matching by name — but
the code only ever ran the dynamic one. It collected a `Vec<&ParamInfo>`, a
`vec![None; n]` and a leftovers vector before it could bind anything, on every
call, however simple. The static path (no keywords, no `*args`/`**kwargs`,
every parameter covered by an argument or its default) now binds with **zero
allocations**. `self` is passed to `bind_call` separately instead of being
prepended into a freshly allocated argument vector, removing another allocation
and a full argument copy from every method call.

| bench | before | after | delta |
|---|---|---|---|
| fib | 0.208s | 0.178s | **-14%** |
| loop | 0.607s | 0.623s | +3% (noise; no calls) |
| strjoin | 0.116s | 0.115s | -1% |
| dictops | 0.357s | 0.357s | 0% |
| oo | 0.374s | 0.335s | **-10%** |
| genpipe | 0.158s | 0.166s | +5% |
| exc | 0.140s | 0.136s | -3% |
| listbuild | 0.304s | 0.302s | -1% |
| chain | 0.164s | 0.146s | **-11%** |

Verified byte-identical diagnostics across thirteen error programs covering
every argument-binding failure — too many positional, too few, unexpected
keyword, multiple values for one argument, defaults, `*args`, `**kwargs`,
methods, and keyword calls to methods — plus all 65 corpus programs.

### 9. Attribute maps keyed by `Rc<str>`, not `String`

`self.x = v` did `fields.insert(name.to_string(), value)` — a fresh heap
allocation for a name the code object already owned, on every attribute store.
`Instance.fields`, `Class.members` and `Module.members` are now keyed by
`Rc<str>`, so storing a name is a refcount bump; lookups are unchanged
(`Rc<str>: Borrow<str>`). `Class::find` and `Class::is_subclass` also walk the
base chain by reference instead of cloning an `Rc` per level.

| bench | before | after | delta |
|---|---|---|---|
| fib | 0.178s | 0.177s | -1% |
| loop | 0.623s | 0.630s | +1% |
| strjoin | 0.115s | 0.118s | +3% |
| dictops | 0.357s | 0.354s | -1% |
| oo | 0.335s | 0.324s | **-3%** |
| genpipe | 0.166s | 0.163s | -2% |
| exc | 0.136s | 0.131s | **-4%** |
| listbuild | 0.302s | 0.303s | 0% |
| chain | 0.146s | 0.149s | +2% |

Real but small, and only where it should be: attribute-heavy code and exception
construction (which builds an instance with an `args` field per raise).
Everything else is inside this machine's ±3% run-to-run drift.

### 10. An inline cache for `LoadGlobal` (and a benchmark that finds it)

The suite had a hole: nothing exercised builtin lookup in a loop, which is
something real scripts do constantly (`len(xs)` inside a `while`). `builtins`
was added to close it — 4 global lookups and 4 native calls per iteration —
and measured **0.265s vs CPython's 0.242s** before any change here.

`LoadGlobal` was hashing a string against the exception registry, then walking
a match arm per builtin name, then **allocating a fresh `Rc<Builtin>` wrapper**
to hand back — every time. A per-call-site cache on the code object removes all
three. It needs no invalidation: Oro has no assignable module namespace, so the
set of globals cannot change while a program runs. Only builtins are cached —
exception classes have a per-VM `Rc` identity that a second `Vm` running the
same code object must not inherit, whereas a builtin has no observable identity
at all (`Value::Builtin` is unhashable and never compares equal, even to
itself).

| bench | before | after | delta |
|---|---|---|---|
| builtins | 0.265s | 0.245s | **-8%** |
| oo | 0.324s | 0.315s | -3% |
| listbuild | 0.303s | 0.295s | -3% |
| fib | 0.177s | 0.186s | +5% (noise; no globals) |
| everything else | | | within drift |

`builtins` now runs at **0.98x CPython** — the first benchmark in the suite
where Oro is ahead.

### 11. Bind call arguments straight off the operand stack

After frame pooling and the static binding path, one allocation was left on the
call path and `do_call` made it unconditionally: `popn(n)` split the arguments
off the caller's operand stack into a fresh `Vec` purely to hand them to
`invoke`. For an ordinary Oro function with positional parameters they can be
moved straight from the caller's stack into the callee's slots — no vector, no
re-copy, each value moved exactly once. Everything else (builtins, methods,
classes, `*args`, keywords, a wrong arity that owes a diagnostic) falls through
to the general path unchanged.

| bench | before | after | delta |
|---|---|---|---|
| fib | 0.186s | 0.166s | **-11%** |
| builtins | 0.245s | 0.232s | **-5%** |
| everything else | | | within drift |

Verified byte-identical across fifteen error programs — now including a runaway
recursion caught by `except` and resumed, since this moved the `MAX_FRAMES`
check relative to the stack pops — plus all 65 corpus programs.

### 12. REVERTED — extending the fast call path to bound methods

**Tried and rejected as a wash.** The same allocation-free binding, extended to
`Value::Method` with a user function, so `obj.m(x)` would also bind straight off
the operand stack. Best-of-7, A/B against the commit above:

| bench | without | with | delta |
|---|---|---|---|
| oo | 0.318s | 0.305s | **-4%** |
| chain | 0.147s | 0.152s | **+3%** |
| genpipe | 0.158s | 0.160s | +1% |
| strjoin | 0.116s | 0.123s | +6% |
| listbuild | 0.305s | 0.313s | +3% |

It helps user-method calls and hurts *native*-method calls, which now pay an
extra `Value::Method` match and a `MethodKind::Native` bailout before falling
through to the general path. That trade is bad for Oro specifically: the
collection protocol (`.map`, `.filter`, `.append`, `.join`) means native method
calls are at least as common as user ones, and three benchmarks got slower to
make one faster.

The real cost in `oo` is elsewhere anyway: `LoadAttr` still allocates an
`Rc<BoundMethod>` for every method access, which the next section discusses.

## `json`, and the hole that let it ship at 95x

The suite had a second hole, of the same shape as the one `builtins` closed and
much larger. Nothing exercised the standard library's own hot path, so nobody
measured `std/json.oro` — a character-at-a-time parser written in Oro — until a
server benchmark made it obvious. It was **95x** CPython's C `json` to parse a
1 KB payload and about **50x** to emit one, which is more CPU than the HTTP
layer, the router and the interpreter put together for a JSON API handler.

`json` was added to close that hole permanently, and then the codec moved to
Rust (`docs/stdlib-server-design.md` §5 is the argument, and the honest account
of how the design document put it on the wrong side of its own rule).

| | oro | CPython | oro/CPython |
|---|---|---|---|
| `json`, Oro codec | 6.52s | 0.155s | 42x |
| `json`, Rust codec | **0.101s** | 0.155s | **0.65x** |

Per operation, best-of-many on a warm cache:

| payload | parse before | parse after | vs CPython | stringify after | vs CPython |
|---|---|---|---|---|---|
| 976 B object | 1.31 ms | **0.016 ms** | 1.15x | **0.009 ms** | **0.54x** |
| 86 KB array | 122 ms | **1.85 ms** | 1.47x | **0.84 ms** | **0.56x** |
| 1.2 KB, 200 deep | 2.18 ms | **0.052 ms** | 2.22x | **0.011 ms** | **0.25x** |
| 95 KB strings | 96.7 ms | **0.305 ms** | **0.86x** | **0.106 ms** | **0.29x** |
| 46 KB numbers | 68.1 ms | **0.158 ms** | **0.24x** | **1.09 ms** | 0.85x |

Two things found on the way there are about the *language*, not about JSON, and
both are worth more than the module was.

### `s[i:j]` was O(len(s))

Every string slice collected the whole string into a `Vec<char>` and then built
a `Vec<usize>` holding one index per selected element — so an O(slice) operation
cost O(string), and any loop walking a large string a token at a time was
quadratic. Nothing in this suite slices a large string in a loop, so nothing
caught it.

| | before | after |
|---|---|---|
| 9722 four-character slices of a 38 KB string | 496 ms | **3.47 ms** (143x) |
| `json.parse` of 38 KB of integers | 323 ms | **76 ms** |
| ...and its scaling, 1000 -> 8000 elements | 3.5 -> 8.3 us/byte | **2.9 -> 1.95 us/byte** |

The `step == 1` case is now a byte-range copy: O(1) index arithmetic for ASCII,
one walk to the end offset otherwise. `bytes`, `list` and `tuple` got the same
treatment. Verified against CPython on 1205 slice cases, byte-identical.

### Freeing a nested value was recursive, and that was an `abort()`

Not found by a benchmark — found by running `cargo test` in the profile the
project's gate actually uses. The JSON depth test passed under `--release` and
**aborted the test runner under plain `cargo test`**, because freeing a value is
naturally recursive (a list's drop glue drops its elements, each of which may be
a list) and a debug build's frames are larger. A stack overflow is `abort()`:
not a panic, not an Oro exception, nothing a program can catch.

It was never a JSON bug. A list nested by a `while` loop in pure Oro aborted the
same way, and had since containers existed:

| | before | after |
|---|---|---|
| 100 000-deep list, debug build | `SIGABRT` | frees |
| 300 000-deep list, release build | `SIGABRT` | frees |
| 1 000 000-deep list, debug build | `SIGABRT` | frees |

Teardown is now a worklist rather than a call stack (`mod teardown` in
`src/value.rs`): a container's `Drop` moves its children into a thread-local
queue and leaves an empty container for the glue to free, and the outermost drop
drains the queue in a loop. Stack depth is constant in nesting depth.

**Where the hook goes was the whole performance story, and it took four
measured attempts.** The obvious spelling — `impl Drop for Value` — taxes
*every* value drop in the program, integers included:

| bench | `Drop for Value` |
|---|---|
| `loop` | **+7.74%** min, **+8.61%** median |
| `fib` | **+4.27%** min, **+4.84%** median |

on two benchmarks that contain no container at all. Moving the hook onto the
four container *payloads* (`OroList`, `OroTuple`, `OroDict`, `Instance` — the
first two are newtypes that exist for no other reason) means only a program that
frees a container pays. Three further rounds went after what was left, and it is
worth recording which ones paid, because two of the three did not:

| attempt | effect |
|---|---|
| hook on the payloads, not on `Value` | `loop`/`fib` back to zero — the whole win |
| `#[inline(never)]` on the four drops | nothing (the cost was not glue size) |
| stop allocating a `Vec` per dict/instance freed | real; `oo`/`exc` recovered ~1% |
| one thread-local access per *level* instead of per node | small, kept |
| skip the queue entirely for all-leaf containers | nothing measurable, kept on principle |

A probe build — the newtypes present, the `Drop` bodies emptied — measured
**within noise of the original**, which is what established that the residual
cost is the worklist itself and not the type change or code layout.

**What it costs, finally.** Interleaved A/B, pinned, best-of-21, against an A/A
control taken in the same session:

| bench | A/A floor | Δ min | Δ median |
|---|---|---|---|
| `fib` | +0.02% | −0.15% | +0.68% |
| `loop` | −0.74% | +1.07% | +0.51% |
| `strjoin` | −0.44% | +4.04% | +1.59% |
| `strops` | −0.41% | +1.54% | +0.90% |
| `dictops` | +0.19% | +3.71% | +3.20% |
| `oo` | +0.60% | +0.86% | +2.46% |
| `genpipe` | −0.10% | −1.87% | −0.55% |
| `exc` | −0.21% | +2.36% | +2.52% |
| `listbuild` | −0.20% | +1.27% | +0.97% |
| `builtins` | −0.28% | +2.62% | +2.38% |
| `chain` | +0.52% | +1.13% | +2.03% |
| `json` | −2.67% | +7.99% | **+6.50%** |

About 1-2% on the general suite and 6-7% on `json`, which frees more containers
per second than anything else here — and note `json`'s own A/A floor is ±3%, so
its true cost is nearer 5% than 8%. That is the price of an `abort()` reachable
from socket input not being reachable, on a benchmark that still runs 60x faster
than the module it replaced.

### The branch, end to end

`d65e900` (the branch point) against the tip, same harness, best-of-21. The
slice fix more than pays for the teardown everywhere except `dictops`:

| bench | Δ min | Δ median |
|---|---|---|
| `fib` | −2.04% | −1.83% |
| `loop` | −4.20% | −4.18% |
| `strjoin` | −3.76% | −3.05% |
| `strops` | −1.63% | −0.80% |
| `dictops` | +2.16% | +2.20% |
| `oo` | −0.63% | +0.29% |
| `genpipe` | −5.90% | −3.01% |
| `exc` | −0.20% | +0.04% |
| `listbuild` | **−8.87%** | **−8.88%** |
| `builtins` | −0.79% | −1.28% |
| `chain` | −3.10% | −2.86% |
| `json` | | **6.52s → 0.11s** |

### An instance attribute costs 2.07x a local

`std/json.oro` held its parse cursor in `self.pos`, and its own comment blamed
Oro's lack of `nonlocal`. Measured directly — the same 2M-iteration counting
loop, once through `self.pos` and once through a local, in the same program:

| | time |
|---|---|
| `while self.pos < n: self.pos = self.pos + 1` | 562.9 ms |
| `while pos < n: pos = pos + 1` | 271.4 ms |
| ratio | **2.07x** |

That is a general fact about Oro, not about JSON: `LoadAttr` hashes the name
into the instance's field map on every read *and* every write. It is the
standing argument for item 2 below, and it is being paid by every program
written in the obvious object-oriented style — `oo` measures the method-call
half of it, and nothing measures this half.

### What the JSON work cost the rest of the suite

Nothing, and if anything the reverse. Measured interleaved A/B against the
pre-change binary — the two alternate on every repetition, which one goes first
alternates too, pinned to one core, best-of-13:

| bench | before | after | Δ min | Δ median |
|---|---|---|---|---|
| `fib` | 0.2015 | 0.2036 | +1.08% | +0.02% |
| `loop` | 0.7765 | 0.7518 | −3.19% | −1.65% |
| `strjoin` | 0.1494 | 0.1491 | −0.21% | +0.18% |
| `strops` | 0.3591 | 0.3542 | −1.38% | −1.20% |
| `dictops` | 0.4424 | 0.4435 | +0.26% | −0.94% |
| `oo` | 0.4014 | 0.3964 | −1.24% | −1.86% |
| `genpipe` | 0.2020 | 0.2004 | −0.77% | −0.42% |
| `exc` | 0.1645 | 0.1620 | −1.54% | −1.14% |
| `listbuild` | 0.3722 | 0.3620 | −2.72% | −3.24% |
| `builtins` | 0.2957 | 0.2879 | −2.64% | −1.36% |
| `chain` | 0.1875 | 0.1863 | −0.63% | −1.62% |
| **mean** | | | **−1.18%** | **−1.20%** |

Read against this machine's A/A floor, which the mio section below records as
mean +0.19% and worst 1.80% pinned (and up to +5% un-pinned). Every benchmark
is inside that band or on the good side of it, so the honest reading is "no
change, possibly a small win from the slice path" rather than a claimed 1.2%.

(That was measured before the iterative teardown below, which is the other half
of the bill. "The branch, end to end" further down is the number that counts.)

## Pass two — per-optimization log

Method, and it is not the same as pass one's. Every number below is an
**interleaved A/B**: the two binaries alternate on every repetition and which
goes first alternates too, so neither owns the warm slot, and the whole thing
is pinned to one core. Each session opens with an **A/A control** — a binary
against a byte-identical copy of itself — to establish that session's noise
floor before any claim is read against it. The floors measured were **mean
-0.14%, worst 2.14%** at the start and **mean -0.07%, worst 2.76%** at the end.
A single-benchmark move under about 3% is not reported as real.

Every commit was gated on: `cargo test` **in debug** (401 after the rebase; a
release-only gate once let a stack overflow through, so this one is not
optional), `./corpus/run.sh` (80 pass, 0 fail, 0 known-failing),
`cargo clippy --all-targets -- -D warnings`, the `size_of` tripwires, and a
**byte-for-byte diagnostic differential** — stdout, stderr and exit code of a
set of error-producing programs, compared against the pass's base binary. That
set grew from 28 programs to **91** as the pass went on, with new programs written for each change that could plausibly move a
message: integer overflow promoting to `Big`, mixed-type and dunder
comparisons, negative and out-of-range subscripts on load and on store, an
unhashable dict key, a list mutated during iteration, `super()` with and
without arguments, a `yield` inside a method, method arity and not-callable
errors, recursion through a method caught and uncaught.

### 13. `Instance.fields`: an association list, not a `HashMap`

Instances carry a handful of attributes and their names are short identifiers,
so hashing one with SipHash costs more than comparing it against every entry
there is. `Fields` is a `Vec<(Rc<str>, Value)>` scanned linearly, leading with
a pointer comparison because a field is usually stored under the very
`Rc<str>` the code object interned. Nothing anywhere iterates an instance's
fields — only `get` and `insert`, and Oro has no `del obj.x` — so the order is
unobservable.

| bench | before | after | delta |
|---|---|---|---|
| oo | 0.3687 | 0.3213 | **-12.9%** |
| exc | 0.1488 | 0.1412 | **-5.1%** |
| loop | 0.7217 | 0.6908 | **-4.3%** |
| genpipe | 0.1830 | 0.1761 | **-3.8%** |
| listbuild | 0.3380 | 0.3276 | **-3.1%** |
| **mean** | | | **-3.4%** |

`exc` moves because every raise builds an exception instance with an `args`
field.

### 14. REJECTED — an inline `LoadAttr` fast path in `step`

**Tried and reverted.** Reading the instance field directly in `step`'s
`LoadAttr` arm — no `Rc<str>` clone, receiver overwritten in place, no call
into the large `get_attr` — measured, on top of step 13, as a **+2.6%
regression**, and bought nothing on `oo` (-0.5%, inside the floor):

| bench | without | with | delta |
|---|---|---|---|
| genpipe | 0.1801 | 0.1903 | **+5.7%** |
| listbuild | 0.3293 | 0.3477 | **+5.6%** |
| loop | 0.6981 | 0.7235 | **+3.6%** |
| exc | 0.1427 | 0.1479 | **+3.6%** |
| oo | 0.3261 | 0.3244 | -0.5% |

The benchmarks that got slower execute the arm zero times. Once the map lookup
was no longer the cost, the arm was pure code growth in the function every
instruction passes through — the layout hazard this project keeps rediscovering.

### 15. `Class.members`: the same association list

The other `HashMap` on the attribute path, and on a hotter one: `obj.m()`
reaches the class table only *after* missing in the instance. `Module.members`
is deliberately left a `HashMap` — a module has an order of magnitude more
members and is not on any loop's critical path.

| bench | before | after | delta |
|---|---|---|---|
| oo | 0.3251 | 0.3041 | **-6.4%** |
| chain | 0.1687 | 0.1666 | -1.3% |
| **mean** | | | **-0.5%** |

Only `oo` moves, and it is the only benchmark that calls user methods in a loop.

### 16. Execute the loop-body instructions in the dispatch loop itself

**The largest single change either pass has produced, and it was not on
anyone's list.**

`Vm::step` is one enormous match returning `Result<Step, RuntimeError>` — 48
bytes, written through a hidden return pointer and read back — and far too
large for LLVM to inline into `run_slice`. Every `i = i + 1` paid a call, a
48-byte store and a 48-byte load to move one integer between a local slot and
the operand stack. On `loop`, that protocol was about two thirds of the total
runtime.

`run_slice` now executes `LoadFast`, `StoreFast`, `LoadConst`, `Jump`,
`BinAdd`/`Sub`/`Mul` on two `Int`s, `Compare` on two `Int`s, and the two
conditional jumps on a `Bool` before it reaches `step`. The discipline that
makes it a speed change and not a semantic one: **an arm that applies only to a
shape leaves the operand stack untouched when the shape is wrong and falls out
of the match**, so `step` below runs exactly as it always did. Integer
overflow, a `Big`, a `Float`, a string, a user `__add__`, an unbound local,
`in`/`not in`, a non-`Bool` condition — every one declines. Nothing here is the
only place a case is handled.

This is *not* the superinstruction fusion pass one ranked third. No instruction
is fused, no instruction index moves, and no jump target — nor any op index
stored inside a `MatchDispatch` table constant — needs relocating. The win was
never the number of instructions; it was the cost of dispatching one.

| bench | before | after | delta |
|---|---|---|---|
| loop | 0.6848 | 0.2446 | **-64%** |
| listbuild | 0.3171 | 0.1861 | **-41%** |
| fib | 0.1793 | 0.1073 | **-40%** |
| genpipe | 0.1724 | 0.1070 | **-38%** |
| exc | 0.1388 | 0.0858 | **-38%** |
| strops | 0.3177 | 0.2202 | **-31%** |
| builtins | 0.2526 | 0.1781 | **-29%** |
| dictops | 0.3796 | 0.2846 | **-25%** |
| chain | 0.1644 | 0.1250 | **-24%** |
| oo | 0.2997 | 0.2332 | **-22%** |
| strjoin | 0.1292 | 0.1044 | **-19%** |
| **mean** | | | **-34%** |

### 17. The ordinary call and the ordinary return, in the dispatch loop too

The same treatment for the two instructions `fib` is made of. `fast_call_target`
already decides whether a call site is a plain Oro function bindable straight
off the operand stack; a return is plain when there is no `finally` to run on
the way out, an outer frame to return into, and nothing for the VM to do with
the value but push it.

| bench | before | after | delta |
|---|---|---|---|
| fib | 0.1169 | 0.1015 | **-13.1%** |
| exc | 0.0940 | 0.0882 | **-6.1%** |
| loop | 0.2726 | 0.2623 | **-3.8%** |
| strops | 0.2436 | 0.2541 | **+4.3%** |
| **mean** | | | **-1.3%** |

`strops` executes no user calls; its +4.3% is the loop getting larger. Step 18
took it back.

### 18. No arm guards in the dispatch loop's fast-path match

**One `if` on one arm of a `match` costs the whole `match` its jump table.**
LLVM cannot lay out a dense switch when reaching an arm depends on a runtime
predicate; it falls back to a compare chain, and *every* instruction in the
program pays. Two arms had picked one up — `Compare` guarded against
`in`/`not in`, `Call` against the frame limit. Both conditions moved inside the
arm body, where they decline by falling out of the fast path rather than by
never entering it.

| bench | before | after | delta |
|---|---|---|---|
| loop | 0.2829 | 0.2677 | **-5.4%** |
| listbuild | 0.2282 | 0.2197 | **-3.7%** (median) |
| oo | 0.2915 | 0.2809 | **-3.6%** (median) |
| chain | 0.1508 | 0.1461 | **-3.1%** |
| **mean** | | | **-0.9%** min, **-1.9%** median |

Ten of eleven benchmarks improve on the median.

### 19. Bind native methods to the interned name

`get_attr` took a `&str`, so every native method access rebuilt the string with
`Rc::from(name)` — a heap allocation and a copy per `xs.append`, `s.split`,
`ys.map`, to store a name the code object already owned. It takes the
`Rc<str>` now.

| bench | before | after | delta |
|---|---|---|---|
| strops | 0.2511 | 0.2332 | **-7.1%** |
| chain | 0.1460 | 0.1410 | **-3.4%** (median) |
| listbuild | 0.1977 | 0.1938 | -2.0% |
| strjoin | 0.1117 | 0.1096 | -1.8% |
| exc | 0.0874 | 0.0898 | +2.7% |
| builtins | 0.1915 | 0.1972 | +3.0% |
| **mean** | | | **-0.4%** min, **+0.3%** median |

Kept despite a mean of roughly zero. The four benchmarks that make native
method calls all improve, the one that makes the most of them improves 7%, and
the ones that move the other way execute none of the changed code.

### 20. Attributes, subscripts, globals and the stack ops, in the dispatch loop

`Pop`, `LoadNone`, a `LoadGlobal` that hits the resolved-builtin cache, a
`LoadAttr` that finds an instance field, and `xs[i]` / `xs[i] = v` on a list
with a non-negative in-range integer index.

| bench | before | after | delta |
|---|---|---|---|
| listbuild | 0.2033 | 0.1765 | **-13.2%** |
| oo | 0.2653 | 0.2364 | **-10.9%** |
| builtins | 0.2048 | 0.1946 | **-5.0%** |
| loop | 0.2715 | 0.2813 | **+3.6%** |
| **mean** | | | **-2.7%** |

Worth setting beside step 14: the *same* `LoadAttr` fast path that was a
regression inside `step` is part of a 10.9% gain on `oo` inside `run_slice`.
Pass one ranked a `LoadAttr` inline cache second and estimated 10% on `oo`.
`oo` got its 10% — from an association list and a fast path, with no cache, no
class-pointer key and no version counter to invalidate.

### 21. Dict subscripts, iteration and the last stack ops, in the dispatch loop

`d[k]` and `d[k] = v` for the four key shapes that are hashable by
construction, `ForIter` on an ordinary iterator, `Dup`, `RotTwo`, `LoadCell`,
`LoadFree`, `ListAppend`. Every benchmark in the suite improved.

| bench | before | after | delta |
|---|---|---|---|
| dictops | 0.3397 | 0.3114 | **-8.3%** |
| loop | 0.2691 | 0.2482 | **-7.8%** |
| fib | 0.1019 | 0.0947 | **-7.1%** |
| exc | 0.0917 | 0.0865 | **-5.6%** |
| strjoin | 0.1160 | 0.1116 | **-3.8%** |
| strops | 0.2359 | 0.2287 | **-3.1%** |
| **mean** | | | **-4.0%** |

`loop` executes none of these arms and is 7.8% faster anyway: steps 17 and 20
had cost it 3.6% on layout between them, and a fuller fast-path match hands it
back.

### 22. A `LoadMethod` / `CallMethod` pair

Pass one's item 1. `LoadMethod` pops the receiver and pushes three slots — a
tag, an auxiliary value and the receiver-or-callable — which `CallMethod`
consumes along with the arguments above them. The tag distinguishes an Oro
method (`Value::Class(defclass)`, function beneath, receiver which is also its
`self`), a native method (`Value::Unbound`; the name rides on the instruction
through `CodeObject::pairs`), and something that is not a method at all
(`Value::None`).

Codegen emits one instruction for one instruction, so no index moves and
nothing needs relocating; `LoadMethod` carries the attribute's own position, so
a missing attribute still reports where the attribute is written.
`resolve_method` agrees with `get_attr` case for case and defers to it outright
for the two receivers that never yield a method.

| bench | before | after | delta |
|---|---|---|---|
| oo | 0.2329 | 0.2119 | **-9.0%** |
| strops | 0.2588 | 0.2465 | **-4.8%** |
| exc | 0.0908 | 0.0874 | **-3.7%** |
| loop | 0.2653 | 0.2558 | **-3.6%** |
| fib | 0.0976 | 0.0947 | **-3.0%** |
| strjoin | 0.1219 | 0.1183 | **-3.0%** |
| **mean** | | | **-2.9%** |

**Removing the `Rc<BoundMethod>` is not where the win is.** A native method
call — `xs.append(i)`, which builds no frame — did not measurably change at
all: glibc hands back a hot 56-byte block for about what three extra
operand-stack slots cost. The win is that a method call can now bind its
arguments straight off the stack, the way a plain function call has since pass
one, because there is no longer a wrapper in the way. Pass one's step 12
rejected exactly that binding when it had to be bolted onto `Value::Method`; as
its own instruction it costs the native path nothing.

### 23. REJECTED — `LoadMethod` / `CallMethod` in the dispatch loop

**Tried and reverted, and it is the most useful rejection of the pass.**
Putting the two new instructions into `run_slice` alongside the others — the
same technique that had just returned -34%, -2.7% and -4.0% — measured as a
**+5.3% regression**:

| bench | before | after | delta |
|---|---|---|---|
| loop | 0.2488 | 0.2869 | **+15.3%** |
| exc | 0.0855 | 0.0930 | **+8.8%** |
| genpipe | 0.1166 | 0.1268 | **+8.8%** |
| builtins | 0.1780 | 0.1933 | **+8.6%** |
| fib | 0.0927 | 0.0993 | **+7.2%** |
| oo | 0.2077 | 0.2086 | +0.4% |

Even `oo`, which executes the new arms half a million times, gained nothing.
**The dispatch loop's fast-path match has a size budget and it is now full.**
Every arm added past this point pays for itself out of the ones already there.
Anyone continuing this work should treat `run_slice` as a fixed budget to be
*reallocated*, and measure removals as seriously as additions.

### 24. One box per generator, not one per `yield`

A `GenBox` held `Box<Frame>`, so every `yield` allocated a box and every resume
freed it — a malloc/free pair per element produced. It holds
`Box<Option<Frame>>`: allocated once when the generator is created, written
through for the rest of its life. The three states are unchanged and read more
directly — finished when `done` is set, suspended when the box holds
`Some(frame)`, being advanced elsewhere when it holds `None`.

| bench | before | after | delta |
|---|---|---|---|
| genpipe | 0.1192 | 0.1132 | **-5.1%** |
| **mean** | | | **0.0%** |

Kept on the same reasoning as step 19: the one benchmark that suspends a
generator is 5% faster, outside the noise floor, and no other benchmark
executes a line of it.

### 25. REJECTED — `HKey::Str(Rc<OroStr>)`, and the benchmark that found it

Pass one's item 6, with the benchmark it asked for. `dictstr` was written
first: 200k writes under distinct string keys, 200k lookups, then half a
million reads and writes of one small record — the shape real code has, where
the same three short keys are hashed over and over. It runs at **1.25x
CPython**.

`HKey::Str` held a `String`, so `d["name"]` copied the whole string onto the
heap to build a probe thrown away a moment later. Sharing it as an
`Rc<OroStr>`, with content hashing and equality, is worth **-8.9%** on
`dictstr` — and **+1% to +4% on eight benchmarks that contain no dict at all**,
for a net **+1.2%**. Two runs at n=9 and n=13 agree; `#[inline]` on the new
`Hash`/`PartialEq` impls does not move it.

| bench | before | after | delta |
|---|---|---|---|
| dictstr | 0.4067 | 0.3705 | **-8.9%** |
| loop | 0.2494 | 0.2593 | **+4.0%** |
| exc | 0.0867 | 0.0900 | **+3.9%** |
| fib | 0.0957 | 0.0980 | +2.4% |
| genpipe | 0.1131 | 0.1161 | +2.7% |
| builtins | 0.1850 | 0.1897 | +2.5% |
| **mean** | | | **+1.2%** |

It is the correct data structure landing in the wrong place. Reverted, because
the suite is the arbiter and the suite says no. **The benchmark stays** — the
next person to touch `HKey` should have something that can see it.

### 26. The rebase, and one real regression it exposed

Rebasing onto `feat/sane-defaults` after the JSON work landed produced two
failures, and only one of them was a merge artifact.

**`Op::CallMethod` bypassed the generator-method fix.** While this pass was in
flight, a method containing `yield` was made to produce a generator, exactly as
a plain `def` does; that fix lives in `Vm::invoke`'s `MethodKind::User` arm.
Step 22 gave `obj.m(...)` a *second* user-method arm in `do_call_method`, which
did not have it, so an ordinary `obj.m()` on a generator method fell through to
`invoke_user`'s refusal — the one meant for dunders and chain callbacks, each
of which has a continuation waiting on a *value* from a frame that runs now.
Both changes were right alone; together the fast path skipped the case.

The fix is not a second copy of the logic but the removal of the first: the
frame-build-and-park is now `Vm::make_method_generator`, and both arms call it.
Two arms that must agree and are written twice will disagree again, so the
repair is the one that makes disagreement unexpressible.

The rebase also dropped, silently, the generator-*receiver* drain from the
native-method arm — `nums().to_list()` surfaced `ForIter target is not an
iterator`, and `58_generator_receiver.oro` caught it — and left one `GenBox`
construction spelling `Box::new(frame)` where step 24 requires
`Box::new(Some(frame))`, which would have panicked on the first resume through
that path. Both restored. The native-method arm is now line-for-line the
baseline's, modulo the receiver's spelling, and that was checked by diffing the
two rather than by reading them.

`super()` inside a generator method (resolved on resume, possibly in another
task), laziness across two `for` loops, a `break` part-way through one, and a
generator crossing a `spawn`/`chan` boundary are all covered by
`47_generator_method.oro` and `58_generator_receiver.oro`, which pass
unmodified, and by program 63 of the diagnostic differential.

## What is left, ranked (after pass two)

The ranking has changed shape. Pass one's list was about *allocations*; after
this pass the interpreter allocates very little on the hot paths, and what is
left is either the size of the dispatch loop or the cost of one specific
protocol.

1. **Reallocate the dispatch loop's fast-path budget.** Step 23 establishes
   that `run_slice` is full: new arms now cost more than they save. Nobody has
   measured which of the *existing* arms are carrying their weight — `LoadNone`,
   `Dup`, `RotTwo`, `LoadCell` and `LoadFree` are all suspects, and each one
   evicted buys room for `CallMethod`, which wants in and would take `oo` and
   `chain` with it. This is a search, not a change: one arm out, measure, keep
   or restore. Estimate 3-6% overall, and it is the only item here that can be
   attempted without a design decision.

2. **Shrink `Result<Step, RuntimeError>`.** It is 48 bytes, and every
   instruction that still goes through `step` — every method call, every
   generator suspend, every f-string, every `MakeFunction` — pays a 48-byte
   store and load for it. `RuntimeError` is `{String, usize, usize}`; boxing it
   in the internal `Result`, or narrowing `line`/`col` to `u32` and boxing the
   message, takes the pair to 32 or 24. That would also make `step` cheaper to
   inline, which is what step 16 shows is worth having. Wide but mechanical
   diff; the public `RuntimeError` can stay as it is behind a boundary
   conversion. Estimate 2-5% on the benchmarks that still reach `step`.

3. **The native method call's remaining string dispatch.** `xs.append(i)` still
   costs about 140ns, and step 22 showed the allocations are only about half of
   it. The other half is roughly fifteen short string comparisons across
   `method_exists`, `invoke_native_method`'s cascade and `call_method`.
   Resolving a native method name to a small enum once — at `LoadMethod`, where
   the name is already in hand — and dispatching on that would remove all of
   them. Est. 10-20% on `strjoin`, `listbuild`, `strops` and `chain`. Medium
   risk: the cascade's *order* is load-bearing and would have to be preserved
   exactly.

4. **`HKey::Str` again, once item 1 has freed some layout headroom.** Step 25
   is right on the merits and lost on the arithmetic. It is worth retrying
   after any change that moves the dispatch loop, because what defeated it was
   where the compiler put the code.

5. **A contiguous VM-wide value stack.** Unchanged from pass one's item 4:
   frames would hold `(base, len)` into one buffer. Frame pooling and
   stack-binding have taken the allocation win already, so this is now purely
   about locality — and about letting a call bind arguments without touching
   two stacks. Large refactor, modest return.

6. **`kwargs: Vec<(String, Value)>` → `Rc<str>` keys.** Still correct, still
   unmeasurable on this suite, still worth doing when something measures it.

7. **Superinstructions.** Pass one ranked these third and estimated 15-25% on
   `loop`. `loop` is now at **0.48x CPython** and fusing instructions would
   still require the relocation pass that made it risky — including the op
   indices stored as values inside `MatchDispatch` table constants. The premise
   has weakened: dispatch is no longer what `loop` spends its time on. Lowest
   value on this list, and the highest risk.

## What pass one thought was left, ranked (kept for the record)

Items 1, 2, 3 and 6 were all addressed in pass two, three of them by a
different route than the one predicted. See the pass-two ranking at the end.

1. **A `LoadMethod` / `CallMethod` pair.** `obj.m(x)` allocates an
   `Rc<BoundMethod>` at `LoadAttr` purely to carry `(receiver, func)` two
   instructions to the `Call` that immediately destructures and drops it.
   CPython's `LOAD_METHOD`/`CALL_METHOD` pushes the two separately and never
   builds the object. This is the single biggest remaining item for OO code and
   it is what would make step 12 pay off: with no `BoundMethod` to build, the
   fast call path handles methods with no extra dispatch on the native-method
   case. Estimate 10-15% on `oo` and `chain`. Medium risk — a codegen change,
   but a local one (only where a call's callee is an attribute expression), and
   it needs no jump-target remapping.

2. **An inline cache for `LoadAttr` on instances.** Every `self.x` hashes the
   name in the instance's field map; every `obj.method` hashes it *twice* (a
   miss in the fields, then a walk up `Class.members`). A per-call-site cache
   keyed on the receiver's class pointer, with a version counter on class
   mutation, is the standard fix. Harder than the `LoadGlobal` cache because
   instance fields are mutable and can shadow a class member, so the fast path
   has to stay correct when a field appears later. Estimate 10% on `oo`.

   **This is now measured, and it is bigger than that estimate suggests.** A
   loop through `self.pos` is 2.07x the same loop through a local (above). `oo`
   sees the method-call side of attribute access; nothing in the suite is a
   tight loop over a *field*, which is what a parser, a state machine or an
   accumulator class actually is. The estimate is for `oo`; the cost to idiomatic
   Oro is larger, and it is the reason `std/json.oro`'s author reached for a
   class and then paid 2x for it.

3. **Superinstructions.** The `loop` benchmark runs 13 instructions per
   iteration; fusing `LoadFast`+`LoadFast`+`BinAdd` and
   `Compare`+`PopJumpIfFalse` would take it to about 8. Now genuinely cheap to
   *encode* (`Op` is one word), but fusing changes instruction indices, and jump
   targets are absolute — including the op indices stored as values inside a
   `MatchDispatch` table constant. Needs a proper post-codegen relocation pass.
   Estimate 15-25% on `loop`, high risk without that pass done carefully.

4. **A contiguous VM-wide operand stack.** Frames would hold `(base, len)`
   offsets into one buffer, as CPython 3.11 does. Frame pooling already
   recovered most of the allocation win, so this is now mostly about locality
   and about letting a call bind arguments without touching two stacks. Large
   refactor, modest remaining upside.

5. **`kwargs: Vec<(String, Value)>` → `Rc<str>` keys.** A keyword call
   allocates a `String` per keyword argument. Correct to fix, but *unmeasurable
   on this suite* — no benchmark passes keywords in a loop, because idiomatic
   Oro rarely does. Worth doing when something measures it, not before.

6. **`OroDict` string keys.** `HKey::Str(s.s.clone())` clones the whole string
   to build a hash key, so `d["name"]` allocates on every lookup. `dictops` uses
   integer keys and so never sees it; a string-keyed dict benchmark would.

## What the mio reactor cost (M3b)

The reactor's budget was "must not cost anything in programs that never touch
I/O", and the honest answer is **about 1% on average, with the largest single
benchmark at 3.1%, and none of it work the reactor actually does**.

Measured interleaved A/B against the pre-mio binary — the two alternate on every
repetition, and which one goes first alternates too, so neither owns the warm
slot and thermal drift hits both — pinned to one core, best-of-21:

| bench | before | after | Δ min | Δ median |
|---|---|---|---|---|
| `fib` | 0.1908 | 0.1927 | +0.97% | +2.28% |
| `loop` | 0.7777 | 0.7954 | +2.27% | +2.00% |
| `strjoin` | 0.1420 | 0.1427 | +0.48% | +2.62% |
| `strops` | 0.3399 | 0.3505 | +3.10% | +1.97% |
| `dictops` | 0.4408 | 0.4493 | +1.94% | +0.71% |
| `oo` | 0.3889 | 0.3914 | +0.65% | +0.27% |
| `genpipe` | 0.1943 | 0.1928 | −0.76% | −3.68% |
| `exc` | 0.1522 | 0.1549 | +1.79% | +3.06% |
| `listbuild` | 0.3707 | 0.3652 | −1.48% | −0.60% |
| `builtins` | 0.2958 | 0.3018 | +2.03% | +1.29% |
| `chain` | 0.1904 | 0.1921 | +0.93% | +0.36% |
| **mean** | | | **+1.08%** | |

The same harness run **A/A** — the mio binary against a byte-identical copy of
itself, in the same session — gives mean +0.19% and worst 1.80%, which is this
machine's floor and the number the table above should be read against.

Two things are worth writing down, because both cost time to find.

**The `Vm` struct's size is on the dispatch path, and 128 bytes of it was worth
5% on `loop`.** The reactor started as an inline field. It executes no code at
all in a program with no sockets — `is_idle()` short-circuits before any
syscall — and it still cost `loop` and `exc` about 5% and 7%, because it sat
between `Vm`'s hot fields and its cold ones. Boxing it (one 128-byte allocation
per VM, once) recovered all of it. This is the second time on this project that
a struct's *layout* mattered more than its code; it will not be the last.

**Below about 3% this suite cannot tell you anything about a single benchmark on
a machine that is doing something else.** Un-pinned, mid-session, A/A produced
+5.03% on `builtins` and −1.89% on `loop` — from a binary compared with itself.
Pinning to one core and letting the machine cool took the floor to ±1.8%. The
mean across eleven benchmarks is a far steadier statistic than any one of them,
which is why it is the number quoted above.
