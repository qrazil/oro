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
| `oo` | attribute load/store, method calls, construction, `super()` |
| `genpipe` | three chained generators over 300k elements (frame suspend/resume) |
| `exc` | raise/catch on 2/3 of 200k iterations, with a `finally` on every one |
| `listbuild` | 400k list appends, then indexed read-modify-write |
| `builtins` | 4 global lookups + 4 native calls per iteration, 300k iterations |
| `chain` | the collection protocol (`.filter`/`.map`/`.reduce` with `=>`) — oro-only, CPython twin in `chain.py` |

## Where things stand

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

## What is left, ranked

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
