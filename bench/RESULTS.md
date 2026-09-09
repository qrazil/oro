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
| method | best-of-3 wall clock |

## The programs

| bench | what it stresses |
|---|---|
| `fib` | the call path: `fib(27)`, ~400k frame push/bind/return cycles |
| `loop` | raw dispatch: a 3M-iteration `while` with integer arithmetic |
| `strjoin` | 200k f-string formats into a list, then `join` |
| `dictops` | 500k integer-keyed dict writes, then a full iteration + lookup scan |
| `oo` | attribute load/store, method calls, construction, `super()` |
| `genpipe` | three chained generators over 300k elements (frame suspend/resume) |
| `exc` | raise/catch on 2/3 of 200k iterations, with a `finally` on every one |
| `listbuild` | 400k list appends, then indexed read-modify-write |
| `chain` | the collection protocol (`.filter`/`.map`/`.reduce` with `=>`) — oro-only, CPython twin in `chain.py` |

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

## Per-optimization log

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
