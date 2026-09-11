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
| `strjoin` | 200k f-string formats into a list, then `join` — oro-only since `sep.join(xs)` was cut, CPython twin in `strjoin.py` |
| `dictops` | 500k integer-keyed dict writes, then a full iteration + lookup scan |
| `dictstr` | string-keyed dicts: 200k distinct keys, then 500k hits on one small record |
| `oo` | attribute load/store, method calls, construction, `super()` |
| `genpipe` | three chained generators over 300k elements (frame suspend/resume) |
| `exc` | raise/catch on 2/3 of 200k iterations, with a `finally` on every one |
| `listbuild` | 400k list appends, then indexed read-modify-write |
| `builtins` | 4 global lookups + 4 native calls per iteration, 300k iterations |
| `chain` | the collection protocol (`.filter`/`.map`/`.reduce` with `=>`) — oro-only, CPython twin in `chain.py` |
| `json` | `json.parse` + `json.stringify` over five payload shapes — oro-only, CPython twin in `json_twin.py` |

## One program's barrier changed spelling

`fuse.oro`'s fourth shape is a chain with a barrier in the middle, and that
barrier was the native `sorted()` until keyed sorting became one spelling and
`xs.sorted()` was cut. It is now `sort_by(x => x)`, which is the same sort
reached through a callback: the keys are an Oro call per element, where
`sorted()` compared the elements natively and called nothing.

So **the `fuse` row above is not comparable with runs taken after this change**,
and the same is true of the "four steps with a `sorted` barrier in the middle"
line in the fusion table below. Nothing else in the suite moved — no other
benchmark used a cut spelling — and only `fuse.oro`'s `d` shape was touched.
The rest of that file, and every other program here, is byte for byte what it
was. When the suite is next rebaselined, `fuse` needs a fresh pair of numbers
rather than a comparison against these.

## Why the benchmarks still count by hand

Every program here spells a bounded count the long way:

```python
i = 0
while i < 200000:
    ...
    i = i + 1
```

Oro's rule is the other one — **`for i in range(n)` for a bounded count,
`while` for a condition** — and the standard library, the corpus and the
examples were all rewritten to it. The benchmarks were deliberately not, and
the reason is the same measurement that makes the rule worth having.

Measured on one core, best of nine, `loop` at 3M iterations:

| form | oro | cpython | ratio |
|---|---|---|---|
| `i = 0; while i < n: …; i = i + 1` | 0.203s | 0.445s | 0.46x |
| `for i in range(n): …` | 0.139s | 0.345s | 0.40x |

**A benchmark's loop is inside its measurement.** `range` steps in Rust, so the
`for` form is 31% faster here without a single instruction of the VM having
changed — and CPython's own gain is smaller (22%), so even the *ratio* column
moves, by 13%, in the direction that reads as an Oro improvement. Rewriting the
counters would have made all thirteen rows better at once, permanently, for
nothing, and would have made every number above incomparable with the four
optimization passes that produced them.

So the manual counter stays here and only here, each file says so at the top,
and `loop.oro` — where the counter genuinely *is* the subject — says it at
length. If the suite is ever rebaselined for another reason, this is the first
thing to change, and both columns must be retaken together.

## Where things stand

Three optimization passes have run. Pass three is `perf/pass-three`, measured
against pass two's tip. Interleaved A/B, both binaries pinned to one core,
best-of-15:

| bench | pass two | after pass three | improvement | vs CPython (pass two -> now) |
|---|---|---|---|---|
| fib | 0.0858s | 0.0882s | +2.9% | 2.05x -> **2.00x** |
| loop | 0.2340s | **0.2259s** | **-3.5%** | 0.48x -> **0.45x** |
| strjoin | 0.1065s | **0.0987s** | **-7.3%** | 1.59x -> **1.47x** |
| strops | 0.2251s | **0.2024s** | **-10.1%** | 1.27x -> **1.14x** |
| dictops | 0.2772s | 0.2761s | -0.4% | 1.30x -> **1.37x** |
| dictstr | 0.3708s | **0.3366s** | **-9.2%** | 1.39x -> **1.25x** |
| oo | 0.1991s | 0.1972s | -1.0% | 1.43x -> **1.39x** |
| genpipe | 0.1034s | 0.1024s | -1.0% | 1.70x -> **1.64x** |
| exc | 0.0844s | 0.0837s | -0.8% | 0.83x -> **0.78x** |
| listbuild | 0.1671s | **0.1536s** | **-8.1%** | 0.86x -> **0.80x** |
| builtins | 0.1667s | **0.1616s** | **-3.1%** | 0.59x -> **0.58x** |
| chain | 0.1276s | **0.1194s** | **-6.4%** | 1.92x -> **1.86x** |
| json | 0.0978s | **0.0946s** | **-3.3%** | 0.72x -> **0.66x** |
| **mean** | | | **-3.9%** min, **-4.6%** median | |

The A/A control in that session was **mean +0.23% min, worst 1.5%**. `fib`'s
+2.9% is inside its own code-placement band, which this pass measured for the
first time and which is the most important thing it found — see
"What a never-called function is worth" below.

Five benchmarks remain faster than CPython 3.12 and nothing is worse than
2.00x. The `vs CPython` columns come from `./bench/run.sh -n 5` run twice in
one session, once per binary, so both ratios share a CPython measurement; the
delta column is the interleaved A/B, which is the number that carries evidence.
`dictops` is the one place the two disagree (A/B says -0.4%, the ratio says it
got worse) and the ratio is the weaker statistic: CPython's own `dictops` time
moved 2.4% between the two runs.

Two optimization passes preceded it. Pass two is `perf/pass-two`, rebased onto
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

## Pass three — per-optimization log

Method as pass two's: every number is an **interleaved A/B**, pinned to one
core, opened with an **A/A control**. The A/A floors measured were **mean
-0.14% min, worst 1.53%** at the start, **+0.80% min, worst 1.13%** mid-pass,
and **+0.23% min** at the end. The **min** column is the statistic used
throughout; the median column on this machine is two to three times noisier
(the opening A/A read +1.09% mean median with `dictops` at 7.05%, against
-0.14% mean min).

Gates on every commit: `cargo test` in **debug** (424), `./corpus/run.sh` (84
pass, 0 fail, 0 known-failing), `cargo clippy --all-targets -- -D warnings`,
the four size tripwires, and the byte-for-byte diagnostic differential —
**extended from 91 programs to 93** in this pass.

### What a never-called function is worth: +6.6% or -7.8% on `loop`

**Read this before reading any single-benchmark number in this file.**

Three separate correct changes in this pass measured as large regressions on
benchmarks that do not execute a line of the changed code. Rather than accept
that a fourth time, it was measured directly. A `pub fn` was added to
`src/vm/mod.rs` containing nothing but arithmetic on its argument, called from
`main` under `if args.len() > 100_000` so that LTO cannot drop it and the
branch is never taken. It changes no behaviour and executes no instruction.

| probe | binary grows by | `loop` | `fib` | `builtins` | `listbuild` | `dictstr` | `strops` |
|---|---|---|---|---|---|---|---|
| large | +5536 B | **+6.63%** | -1.00% | -2.13% | +1.10% | +0.20% | +0.63% |
| small | +2224 B | **-7.80%** | -2.69% | -2.22% | -0.40% | -0.25% | -0.68% |

`loop` spans **fourteen points** between two binaries that differ only in how
much dead code sits ahead of the dispatch loop. `fib` spans about 3 points,
`builtins` about 2.2; `dictstr`, `listbuild` and `strops` stay under 1.1.

**An A/A control cannot see this**, and that is the point. A/A compares a
binary with a byte-identical copy of itself, so it holds layout fixed and
measures only the machine. It reported -0.14% mean and 1.53% worst in the same
session in which a placement change was worth 6.6% on one benchmark. The A/A
floor bounds *thermal and scheduling* noise. It says nothing about the noise a
recompile introduces, and every A/B in this file is a recompile.

The consequence for method: **judge a change by the benchmarks that execute
it.** A move on a benchmark that does not run the changed code is placement
until proved otherwise, up to that benchmark's band. The suite mean is not a
refuge either — the two probes moved a six-benchmark mean by +0.91% and -2.34%.
This is why `HKey::Str` was rejected in pass two and kept in pass three on
materially the same numbers, and it retroactively qualifies every `loop` figure
in the pass-one and pass-two logs, including step 23's, which is the finding the
whole ranking below was built on.

(Mechanism unidentified, and it is not the obvious ones: the effect does not
follow code size monotonically, `#[inline(never)]` on `Vm::step`, `Vm::invoke`
and `Vm::do_call` does not change it, and reordering an arm *inside*
`run_slice` — which moves nothing else — reads as +0.16% mean. Instruction
cache, iTLB or branch-target-buffer aliasing of the dispatch loop all fit.)

### The instruction profile the suite actually has

Nobody had counted. A throwaway instrumented build (a counter at every fast-path
`continue`, and one keyed by opcode where the loop falls through to `step`) was
run over all thirteen benchmarks. It is what settled step 31 and it is what
picked step 32 as the best remaining candidate, so it is worth keeping.

Fast-path arms, by where they fire:

| arm | fires | verdict |
|---|---|---|
| `LoadFast` / `LoadConst` / `StoreFast` / `BinAdd`-`Sub`-`Mul` / `Compare` / `Jump` / `PopJumpIf*` | tens of millions, every benchmark | core |
| `LoadGlobal` | `builtins` 1.20M (12.9% of its instructions), `strops` 400k | earns |
| `LoadAttr` | `oo` 1.00M (9.7%) | earns |
| `LoadSubscript` | `listbuild` 800k, `dictstr` 700k, `dictops` 500k | earns |
| `StoreSubscript` | `dictstr` 700k, `dictops` 500k, `listbuild` 400k | earns |
| `Call` | `fib` 636k, `exc` 200k | earns |
| `Return` | `fib` 636k, `oo` 400k, `exc` 133k | earns |
| `LoadFree` | `fib` 636k (9.1%), `oo` 200k | earns |
| `Pop` | `listbuild` 400k, `strjoin` 200k, `chain` 200k | modest |
| `ForIter` | `dictops` 500k (`genpipe`'s iterators are generators, which decline) | modest |
| `LoadNone` | `oo` 200k, `json` 5k, otherwise 1 | marginal |
| `LoadCell` | `exc` 66,667, otherwise ~0 | marginal |
| `Dup` | **0** | dead |
| `RotTwo` | **0** | dead |
| `ListAppend` | **0** | dead |

And the hottest instructions still reaching `step`, which is where any future
fast-path arm would have to come from:

| bench | op | count | share of that benchmark |
|---|---|---|---|
| `builtins` | `Call` (a native builtin) | 1,200,001 | 12.9% |
| `exc` | the five block ops | 1,133,334 | 15.5% |
| `strops` | `LoadMethod` + `CallMethod` | 1,200,000 | 14.6% |
| `genpipe` | `ForIter` (generator) + `Yield` | 1,200,003 | 14.0% |
| `chain` | `LoadMethod` + `CallMethod` + `Return` | 800,006 | 16.7% |
| `oo` | `LoadMethod` + `CallMethod` | 800,004 | 7.8% |
| `oo` | `StoreAttr` | 600,007 | 5.8% |
| `listbuild` | `LoadMethod` + `CallMethod` | 800,000 | 5.1% |

Step 32 took the largest of these that fits in one small arm and it still did
not pay, so the numbers in this table should be read as an upper bound on what
is available rather than as a list of opportunities.

### 27. Box the error half of every internal VM `Result` (48 -> 24 bytes)

Pass two's item 2. `Vm::step` returns one `Result<Step, RuntimeError>` per
instruction through a hidden return pointer, written and read back each time.
`Step` is 24 bytes and `RuntimeError` is 40 (`{Box<str>, Rc<str>, u32, u32}`),
so the pair was 48 — twice the width of the half carrying the result, to
describe a condition that arises on well under one instruction in a million.
`vm::VmError` is `Box<RuntimeError>` and every internal fallible VM signature
uses it; the pair is **24 bytes**, the `Result` discriminant riding in `Step`'s
own spare tag values. The public surface (`vm::run`, `vm::run_main`) still
hands back a plain `RuntimeError`, unboxed once per program at the boundary.
The tripwire asserted `<= 48` and now asserts `<= 24`.

| bench | before | after | delta |
|---|---|---|---|
| loop | 0.2386 | 0.2284 | **-4.28%** |
| dictstr | 0.3800 | 0.3716 | **-2.20%** |
| oo | 0.2078 | 0.2036 | **-2.04%** |
| json | 0.1023 | 0.1009 | -1.39% |
| listbuild | 0.1764 | 0.1743 | -1.17% |
| builtins | 0.1767 | 0.1835 | **+3.84%** |
| **mean** | | | **0.00%** |

Kept on a mean of zero. Two independent runs (n=11 and n=13) agree case for
case, so the split is a property of the two binaries. It is the one change in
this pass whose verdict the placement finding does not settle: `builtins`
executes the changed code and got slower, and `builtins`'s own band is 2.2%.
The structural argument carried it — the hot return halves, and everything
built on top of it since is cheaper for that.

### 28. Stop allocating an `Rc<BoundMethod>` on every native method call

Pass two's item 3, and it turned out not to be about string dispatch at all.
`invoke_native_method` ended with

    self.materialize_generator_args(&Self::rebound_method(&receiver, name), ...)

and `materialize_generator_args` *begins* by asking whether any argument is a
generator, answering `None` when none is. The callee it was handed existed only
to be dropped — and `rebound_method` builds an `Rc<BoundMethod>`: a heap
allocation, a receiver clone and a name refcount bump. Every `xs.append(i)`,
`s.split(c)`, `ys.map(f)` and `d.get(k)` in the program paid for one, because a
Rust argument is evaluated before the call that ignores it.

`rebound_method`'s own doc comment says it is "rare by construction, so it can
afford the allocation the ordinary path no longer makes". It was not rare, it
was universal. This is the very allocation the `LoadMethod`/`CallMethod` pair
(step 22) was built to remove, reintroduced one layer down — and step 22's
note that "a native method call did not measurably change at all" is explained
by it: the wrapper was still being built, just somewhere else.

| bench | before | after | n=13 | n=17 |
|---|---|---|---|---|
| strops | 0.2267 | 0.2068 | **-8.15%** | **-8.79%** |
| listbuild | 0.1651 | 0.1549 | **-7.42%** | **-6.20%** |
| strjoin | 0.1070 | 0.1000 | **-6.38%** | **-6.55%** |
| chain | 0.1268 | 0.1188 | **-5.10%** | **-6.33%** |
| json | 0.0995 | 0.0974 | +1.08% | **-2.07%** |
| **mean** | | | **-1.29%** | |

Every benchmark that makes native method calls improves 5-9%. Differential
program 92 was written for it: a generator passed to `list.extend` and to
`str.join`, a generator *receiver* for `to_list` and `filter`, a generator
argument to a builtin, and a diagnostic.

**The string cascade itself was not touched, and on this evidence it is worth
much less than pass two's 10-20% estimate.** `SeqOp::from_name` and its
neighbours look like fifteen comparisons written out, but LLVM buckets a `match`
on `&str` by length first, so `"append"` is compared against `"filter"` and
little else. The allocation was the cost.

### 29. `HKey::Str` shares the string instead of copying it

Pass two's item 4 and its step 25, retried and this time kept. `HKey::Str` held
a `String`, so `d["name"]` allocated a copy of the whole string to build a probe
thrown away a moment later, on every dict read and write. It is
`StrKey(Rc<OroStr>)`: hashing by content (`String`'s own `Hash`, so buckets are
unchanged), equality leading with `Rc::ptr_eq` — a key stored under an interned
name is usually probed with the very same `Rc` — then falling back to content.

| bench | before | after | delta |
|---|---|---|---|
| dictstr | 0.3609 | 0.3319 | **-8.03%** |
| json | 0.0953 | 0.0933 | **-2.07%** |
| loop | 0.2208 | 0.2385 | +8.06% |
| fib | 0.0816 | 0.0887 | +8.76% |
| **mean** | | | +1.01% min, +0.05% median |

Pass two measured almost exactly this (`dictstr` -8.9%, mean +1.2%) and
rejected it because eight dict-free benchmarks got slower. **That reading was
wrong.** `loop` +8% and `fib` +8.8% are inside their placement bands and
neither program contains a dict; `dictstr` -8.0% is eight times its own band
and is the benchmark that was written to see this change. Differential program
93 was written for it: the same string key built four ways, an interned name
probed with a constructed key, numeric normalisation, Unicode keys and a key
that is a prefix of another, tuples containing strings, fifty distinct keys
written then re-read, two bound methods as keys, and a `KeyError`.

### 30. Stop bumping a builtin's refcount on every builtin call

Step 28's shape again, in `Vm::invoke`'s `Value::Builtin` arm: the callee was
built unconditionally so that `materialize_generator_args` could look at the
arguments, find no generator, and answer `None`. A `Value` construction and an
`Rc` bump on every `len(xs)`, `abs(n)`, `min(a, b)`.

| bench | before | after | delta |
|---|---|---|---|
| builtins | 0.1772 | 0.1609 | **-9.20%** |
| loop | 0.2436 | 0.2254 | -7.46% |
| genpipe | 0.1050 | 0.1032 | -1.76% |
| strops | 0.2068 | 0.2033 | -1.69% |
| exc | 0.0854 | 0.0841 | -1.55% |
| **mean** | | | **-2.04%** |

The placement band is visible here from both sides at once. This identical
change, measured against the previous tip, read `builtins` **-7.41%** and `loop`
**+7.47%**, mean +0.46%; measured against this one it reads `builtins` -9.20%
and `loop` -7.46%, mean -2.04%. `builtins` is the benchmark that makes 1.2M
native calls and it is consistent across both; `loop` makes none and swung
fifteen points.

### 31. REJECTED — evicting the dispatch loop's dead fast-path arms

**Pass two's item 1, searched and answered: no.** An instrumented build counted
every fast-path arm across the whole suite. `Dup`, `RotTwo` and `ListAppend`
fire **zero times in all thirteen benchmarks** — `xs.append` is
`LoadMethod`/`CallMethod`, and `ListAppend` is only comprehensions — and
`LoadCell` fires 66,667 times in `exc` and essentially nowhere else. Pass two
named `LoadNone`, `Dup`, `RotTwo`, `LoadCell` and `LoadFree` as suspects; the
counts clear `LoadFree` (636k in `fib`, 9.1% of its instructions) and
`LoadNone` (200k in `oo`).

Evicting the three provably dead arms — leaving them to `step`, which handles
them exactly as it always did — is a **regression**:

| bench | before | after | n=9 | n=13 |
|---|---|---|---|---|
| listbuild | 0.1547 | 0.1644 | **+6.27%** | **+7.49%** |
| builtins | 0.1539 | 0.1586 | **+3.05%** | **+2.86%** |
| oo | 0.1831 | 0.1864 | +1.83% | +0.97% |
| **mean** | | | **+1.29%** | **+2.47%** |

`listbuild`'s band is 1.1% and it lost 6-7% twice, so this is real and not
placement. Nothing improves, because nothing executed the evicted arms in the
first place. **The fast-path match is not a budget with room to be freed:
removing arms that never run still costs, and a fuller match is a cheaper
one** — which is the same thing step 21 saw from the other side ("`loop`
executes none of these arms and is 7.8% faster anyway") and step 18 saw as an
arm guard costing the whole match its jump table.

Judging removals as seriously as additions was the right instruction; the
answer is simply that every current arm earns its place, including three that
never fire.

### 32. REJECTED — a `StoreAttr` fast path in the dispatch loop

The other half of step 20's attribute work, and the profile's best remaining
candidate: `oo` spends 5.8% of its instructions on `StoreAttr` and every one
goes through `step`. An arm for the only receiver that has settable attributes:

| bench | before | after | delta |
|---|---|---|---|
| oo | 0.1971 | 0.1927 | **-2.24%** |
| loop | 0.2252 | 0.2430 | +7.89% |
| exc | 0.0836 | 0.0864 | +3.39% |
| listbuild | 0.1537 | 0.1572 | +2.24% |
| dictstr | 0.3358 | 0.3430 | +2.13% |
| **mean** | | | **+1.98%** |

`oo` is the only benchmark that executes the arm and it gains 2.2%. **Ten of
the other twelve get slower**, most of them by more than their placement bands,
which is a broader and more consistent pattern than placement produces. This is
step 23's finding reproduced with attribution: the dispatch loop's fast-path
match really is full, an added arm is paid for out of the arms already there,
and 5.8% of one benchmark's instructions is not enough to buy in.

### What the pass three branch cost the rest of the system

Nothing measurable. `size_of::<Value>()` is 16, `size_of::<Op>()` is 8,
`size_of::<Step>()` is 24, and `Result<Step, VmError>` is 24 (was 48). The
binary is 2.98 MB, unchanged to three digits. No dependency was added, no
`unsafe` was written, and the two `#![deny(unsafe_code)]` crate roots are
untouched.

## Pass four — chain fusion

Not an optimisation pass but one change, measured by this file's rules.
`xs.filter(p).map(f).filter(q)` walked its receiver three times and built three
collections; it now walks it once and builds one. The README documented the old
behaviour as a known limitation, and it mattered more than a limitation
normally does, because chains are the construct that replaced comprehensions.

### The measurement problem, and a way around it

Every A/B in passes one to three is a recompile, and pass three established
that a recompile alone is worth ±7% on `loop`. Re-measured at the start of this
pass with the same never-called-function probe, on this machine today, it read
**-10.56% on `loop`, +5.66% on `fib`, -3.21% on `exc`, -2.89% on `chain`,
-2.06% on the suite mean**. The effect being looked for here — the allocation a
fused chain does not do — is smaller than that.

So the fusion decision was put behind a **compile-time switch read from the
environment**, temporarily, and the A/B was run on **one binary** with the
switch on and off. The two runs are byte-identical code with identical layout,
so the A/A floor genuinely bounds them: on programs with no chain in them the
switch reads **-0.31% on `loop`, +0.36%** on a 400k `append` loop. That is the
floor these numbers sit above. The switch was removed before the commit.

**This is the technique pass three's "give the placement band a control" item
was asking for, and it generalises**: any change that can be expressed as a
decision rather than as different code can be measured this way, and the
placement band stops mattering.

### What fusion is worth

Fusion on versus off, one binary, n=15, pinned to one core, min and median.
`f` is a named two-line function; 120k-400k element receivers.

| program | d min | d med |
|---|---|---|
| a 400k `append` loop, no chain — the floor | +0.20% | +0.38% |
| `bench/progs/loop.oro`, no chain — the floor | -0.84% | +0.03% |
| `xs.map(f)` — nothing to fuse, the control | +0.14% | +0.10% |
| `xs.filter(p).map(f)` | **-2.17%** | -3.24% |
| `xs.map(f).filter(p)` | **-0.82%** | -1.16% |
| `xs.map(f).map(f)` | **-3.22%** | -3.20% |
| `xs.map(f).map(f).map(f).map(f)` | **-3.73%** | -4.37% |
| four steps, two of them filters | **-0.68%** | -2.19% |
| four steps with a `sorted` barrier in the middle | +0.06% | -1.05% |
| `bench/progs/chain.oro` | **-1.82%** | -3.01% |
| `bench/progs/fuse.oro` | **-0.36%** | -0.31% |
| `xs.map(f).first()`, 400k elements | **-43.73%** | -43.88% |
| `bench/progs/fusesc.oro` | **-97.37%** | -97.38% |

**The two halves of the win are two orders of magnitude apart, and the smaller
one is the one the work was started for.** Removing an allocation per step is
worth 0-4%, and on a four-step chain with a barrier in the middle it is a wash.
Short-circuiting *through* the chain is worth 43% on one program and 97% on
another, because `xs.map(f).first()` called `f` four hundred thousand times to
look at one answer and now calls it once.

The reason the allocation half is so small is worth writing down: **a chain
step's cost is almost entirely the callback, not the collection.** A 400k
`map` with a two-line callback takes 55 ms; a `for` loop making the same 400k
calls and appending takes 54 ms. The intermediate vector is about 5% of a step,
and fusing it away costs some per-element bookkeeping back. Anyone reading
"each step allocates a new collection" as the reason a four-step chain is 3.8x
a one-step chain was reading it wrong: it is 3.8x because it makes 3.8x as many
calls, and fusion does not change that. It changes how many calls a
short-circuit can *avoid*.

### Three ways the first fused version paid for the intermediate twice

All three were found by the switch, and none would have been visible against
the recompile band.

1. **The element was cloned three times per stage** — out of the work list,
   into the held slot, and into the argument vector. A fused chain touches one
   element once per step, so a clone here is a clone per element per step.
   `seq_args` takes the element by value now, and only `filter` — which answers
   about an element it does not replace — keeps a copy.

2. **The terminal recorded every element it was handed.** `map`, `flat_map`,
   `any`, `all`, `count` and `reduce` build their answer out of callback
   results and never read the elements. Unfused that vector is the receiver's
   snapshot and is free; fused it was a second full-length copy built to be
   thrown away.

3. **A `filter` ending a fused run was the terminal rather than a stage.** As
   the terminal it is handed every element and answers with a parallel vector
   of booleans about them — two full-length vectors to build one short one. As
   a stage it does not pass on what it rejects. This is the one that flipped
   `xs.map(f).filter(p)` from **+2.9% to -1.4%**.

### What it cost everything else, and how that was established

The first version cost the rest of the system about 1.7%: `builtins` +1.67%,
`strops` +1.84%, `strjoin` +1.85%, `chain` +1.76% against the pre-fusion
binary — small, but consistent, and above each benchmark's band. Two causes,
both structural rather than incidental:

* **Every native method call asked whether a pipeline was waiting for it.**
  `first()` and `take(n)` end a fused chain and are ordinary native methods, so
  noticing one had arrived meant checking VM state on every `xs.append(i)` in
  every program. Codegen already knows which call flushes — it is the one whose
  receiver it emitted with the hint — so that became a second bit on the
  instruction (`CHAIN_FLUSH`) and the question is answered from a register.
* **The pending list was a field on `Task`,** which the dispatch loop reads on
  every instruction. It does not belong there: a pending chain lives between one
  `CallMethod` and the next, and nothing in that gap can yield, so it is empty
  at every point a task can be switched at. It lives on the `Vm`.

After both: `builtins` +0.67%, `strops` +0.67%, `oo` +0.16%, `chain` +0.55%,
`json` -0.57% — at or inside their bands.

What is left moves and does not settle. Against the pre-fusion binary the
final build reads `loop` +10.0%, `fib` +8.1%, `listbuild` +3.3%,
`fuse` +2.7%: the while-loop programs, which are exactly the ones pass three
measured a 14-point placement band on. **Three builds of this branch differing
only in an `#[inline(never)]` attribute on a cold function and in where two
`&` operations sit read `listbuild` at +1.66%, +2.23% and +3.30%** — a 1.6-point
spread from changes neither program executes. The layout-controlled switch
reads those same programs at the floor. Reported as placement, and the honest
statement is that the residual is not separable from it with the apparatus
available (no `perf` on this machine; retired-instruction counts would settle
it in one command).

### What fuses, what does not, and why

**Stages** (deferrable into a pipeline): `map`, `filter`. Both produce their
output one element at a time, in order, with no view of the whole input.

**Terminals** (can end a fused run): every one of the sixteen callback-taking
chain methods, plus the two native short-circuiting ones, `first()` and
`take(n)`, which fuse as a *limit* on the pass.

**Barriers**: `sort_by`, `reversed`, `unique`, `unique_by`, `chunk`,
`flatten`, `zip`, `enumerate`, `group_by`, `partition`, `min_by`, `max_by`.
Each needs the finished intermediate. A chain fuses the runs between barriers
and materialises at each one.

`take_while` is a barrier for a different reason and it is a documentation
finding as much as an implementation one: **it evaluates its predicate over the
whole receiver**, not up to the first false. Fusing it would have been a silent
change in how many times a user predicate runs. The README now says so.

`flat_map` is a barrier for a third reason: as a stage it would have to call
`iterate_to_vec` per element, and the diagnostic that raises would move from
after the last callback (where it is today) to the middle of the pass. It is
fusable in principle and the work list keeps the `(stage, value)` shape that
would take it.

`map` over a **dict** declines mid-chain. It rebuilds a dict from what the
callback answers, and the check that those answers are `(key, value)` pairs
happens when the dict is built — so a bad callback must still raise after the
whole receiver has been walked. Every other dict step passes the original pair
through and fuses. This was the fiddly case and it is the one place the
implementation says no on purpose.

### Two semantics, one preserved and one deliberately changed

**Mutating the receiver from inside a callback is unchanged**, fused or not.
The classic fusion hazard turned out to be closed already: `seq_receiver`
snapshots the receiver before the first callback runs, so neither form sees its
own source change under it. Checked both ways — a callback that appends to the
receiver and one that pops from it produce identical answers and identical
final receivers under both binaries.

**The order two callbacks in different steps run in does change**, and it has
to: `xs.map(f).map(g)` ran every `f` and then every `g`, and now runs
`f(x0), g(y0), f(x1), g(y1)`. That is the order a `for` loop would run them,
and it is the definition of the pass happening once. Along with the
short-circuit, it is the only way a program can tell.

### Gates

425 tests in debug, corpus 86 pass / 0 fail / 0 known-failing, `oracle.sh` a
no-op on a clean tree, `cargo clippy --all-targets -- -D warnings` clean, and
the four size tripwires (`Value` 16, `Op` 8, `Step` 24, `Result<Step, VmError>`
24 — none of them touched). The diagnostic differential went from **93
programs to 110**: the sixteen new ones are a raise inside a callback on the
first element and mid-pass, a generator callback in a stage and in a terminal,
a barrier mid-chain, a non-callable and a wrong-arity chain step, a dict `map`
answering a non-pair, `first()` on an empty fused result, `take()` with a bad
count and a bad type, an unbound name as a step's argument, a caught error
followed by more chains, a type error inside a stage, chains nested inside
chain callbacks, and tuple/range receivers. **Three of them failed on the first
run** and all three were the same bug: a diagnostic raised after a fused
callback had returned reported the callback's `return` rather than the call it
belongs to, because the VM's position had moved on. Stages and jobs carry their
own span now.

No dependency was added and no `unsafe` was written.

## What is left, ranked (after pass three)

The shape has changed again. Pass one's list was about allocations; pass two's
was about the size of the dispatch loop; what pass three found is that **the
remaining allocations were hiding inside arguments to functions that did not
use them**, and that the measurement apparatus was reporting layout as cost.

1. **Look for the third `materialize_*` argument.** Steps 28 and 30 are the
   same bug found twice, five hundred lines apart: a callee built eagerly for a
   callee-drains-a-generator path that almost never runs. `materialize_receiver`
   has the same signature and two call sites, both already behind a
   `matches!(receiver, Value::Generator(_))` guard — so those are clean — but
   the pattern is worth a sweep. Cheap, and the two instances of it were worth
   6-9% each on the benchmarks that touched them.

2. **Give the placement band a control.** Every A/B in this file is a
   recompile, and a recompile moves `loop` by up to fourteen points. Building
   each binary two or three times with a deliberate, varying dead-code padding
   and taking the *median across builds* would turn a coin flip into a
   measurement, at maybe 3x the wall clock. Nothing else on this list can be
   read confidently until this exists, and two pass-two conclusions (step 23's
   budget, step 25's rejection) are already known to have been distorted by it.

3. **`builtins`, and `Vm::invoke`'s string cascade.** `builtins` is the one
   benchmark that got slower from step 27 while executing its changed code. Its
   `Value::Builtin` arm still tests the callee's name against fourteen literals
   before reaching `(b.func)(args)`, and `do_call` still allocates an argument
   `Vec` per native call (`popn`). A `BuiltinKind` computed at construction
   would skip the cascade — but the construction site is
   `src/builtins/mod.rs:64`, which was off limits to this pass. Est. 5-10% on
   `builtins`; note that `min`, `max` and `len` are VM-inspected names and would
   still enter the cascade, so the flag alone does not finish the job.

4. **`fib`, which is now the worst benchmark in the suite at 2.00x.** It moved
   least of anything this pass and nothing here targeted it. Its instruction mix
   is 9.1% `LoadFree` — the free-variable read is on `fib`'s critical path
   because the recursive name is a captured cell — and the call/return pair it
   is otherwise made of is already in the dispatch loop.

5. **A contiguous VM-wide value stack.** Unchanged from pass two's item 5. Large
   refactor, modest return, and now also a large placement perturbation, which
   makes it hard to evaluate.

6. **`kwargs: Vec<(String, Value)>` -> `Rc<str>` keys.** Still correct, still
   unmeasurable on this suite.

7. **Superinstructions.** Still lowest value and highest risk. `loop` is at
   0.45x CPython, and step 31 makes it *less* likely that shrinking the
   instruction stream is where the remaining time is.

## What pass two thought was left, ranked (kept for the record)

Items 2, 3 and 4 were addressed in pass three; item 1 was searched and
answered "no". See the pass-three ranking above it.

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
