# Compiling Oro

Could Oro, as the language stands today, be compiled to native code — and what
would that actually buy?

The short answer is that it could be, that it would buy about **2×**, that
getting there would cost more engineering than the entire existing
implementation, and that it would be aimed at the wrong target. The gap between
Oro and Go on a real HTTP workload is not mostly interpretation. Half of it is
the standard library breaking the rule §5 of `stdlib-server-design.md` set for
it, and most of the rest is boxing and allocation, which compiling does not
remove. There is a **50-100× win** available in `std/json.oro` this month and a
**1.5-2× win** available in the VM this quarter, and both are ordinary work on
the thing that already exists.

The longer answer is that the two goals in the brief — *Go-class throughput*
and *one small frozen finishable binary* — are in direct conflict, that the
conflict is not close, and that the measurements below say the throughput goal
was aimed at a number that does not matter for the server the owner actually
wants to build.

Everything here was measured on this repository, on this machine, on
2026-09-09, at commit `c31ab20`. Method and machine are the same as
`bench/RESULTS.md`: Intel Core i7-8750H, Linux 6.14.5, rustc 1.97.1, CPython
3.12.10, Go 1.x from `/usr/bin/go`, `oro` built `--release`. Estimates are
labelled as estimates. Where a number here disagrees with one in the brief, the
number here is the one that was re-measured, and the disagreement is called out
rather than smoothed over.

---

## 0. The answer, up front

1. **Nothing in Oro's semantics makes native compilation impossible.** Three
   things make it *hard* and were verified in the source, not assumed: `import`
   is fully dynamic and compiles `.oro` files at runtime, so there is no
   closed-world program; module-level `def` names live in shared cells and can
   be rebound mid-run, and already-compiled callers see the rebinding; and
   heap identity is observable through `is`, `==` and dict keys for 18 of the
   26 `Value` variants, so a compiler may not copy, intern or unbox them.
   Each has a cheap language-level fix, and each fix is a change to a language
   that says it is about to freeze.

2. **Compiling the dynamic semantics as-is buys ~2× (range 1.5-3×).** It
   deletes instruction fetch, program-counter bookkeeping, the 69-arm dispatch
   match and the operand stack. It keeps every 16-byte boxed `Value`, every
   `Rc` increment, every `HashMap` attribute probe, every MRO walk, every
   bignum-overflow branch, and every allocation. On the HTTP workload those are
   the majority of the cost, which is why the multiple is small. This is not
   pessimism; it is what Cython measures on unannotated Python and what the
   decomposition in §1.6 predicts.

3. **The Rust-underneath argument does not hold.** The same HTTP head parse,
   written idiomatically in Rust and in Go, costs **0.95 µs and 1.06 µs**. A
   10% difference between a language with no abstraction penalty and one with a
   tracing GC. The workload is allocation- and hash-bound, so there is no
   headroom under Go for "we have Rust underneath" to occupy. Whatever Oro
   wins, it wins by not being interpreted — not by being built on Rust.

4. **The stdlib is leaking more performance than the interpreter is.**
   `json.parse` on a 1.7 KB payload costs **2.55 ms** in Oro against 22 µs in
   CPython — **115×** — because it is a character-at-a-time parser written in
   Oro, holding its cursor in an instance attribute. `http.read_request`
   spends **41%** of its time in `_is_token` and `_is_field_value`, which walk
   every byte of every header name and value in Oro. §5 of
   `stdlib-server-design.md` set a falsifiable test for exactly this and the
   test fires.

5. **For an I/O-bound server it barely matters, and the numbers are not close.**
   End to end over a real TCP socket, Oro serves ~10,000 req/s on one keep-alive
   connection against Go's ~23,000 — **2.2×**, not the 40× the in-memory
   numbers imply, because both pay the same ~44 µs syscall and round-trip floor.
   Put a 5 ms database query in the handler and Oro's hello-world overhead is
   **2.1% of request latency**. Compiling would take that to 1.1%. Fixing
   `json` takes a realistic JSON handler from **26% to 3%**, which is ten
   times the improvement for a hundredth of the work.

6. **Recommendation: do not compile.** Fix the stdlib, finish the VM
   optimisation list already written in `bench/RESULTS.md`, and plan around a
   ceiling of roughly **15-20k req/s per core** for trivial handlers and
   **6-10k req/s per core** for real JSON ones. If Go-class throughput is
   genuinely required, it is the *frozen, finishable, 2 MB* half of the identity
   that has to give, and it has to give by about 20×: an LLVM-linked Oro is a
   40-80 MB toolchain and a multi-year project, and it still would not reach Go
   on the workload measured here.

7. **And the field has already run this experiment, repeatedly.** Nobody has
   compiled a dynamic language to native code and kept the dynamism — Crystal,
   Codon, Nim, mypyc and Julia's static compiler each work by deleting dynamic
   features, and Codon's deletion list includes heterogeneous collections and
   arbitrary-precision integers, two things Oro has locked in. The measured
   share of CPython's runtime that is *dispatch* — the part compilation removes
   — is **14.2%**. PyPy spent twenty-three years and 690,000 lines to reach
   3-4.3×. CPython's funded expert team got ~1.45× in four years against a 5×
   goal and was then disbanded. Ruby's production JIT buys 5-15% on real Rails
   traffic. **The one intervention in that whole literature with a good ratio —
   CPython's PEP 659 specialising interpreter — is not a compiler at all, buys
   ~25%, and changes nothing about the language.** §5 has the receipts.

---

## 1. What was measured

### 1.1 The interpreter against CPython

`./bench/run.sh -n 3`, reproduced today:

| bench | oro | cpython | ratio |
|---|---|---|---|
| fib | 0.158s | 0.042s | 3.76× |
| loop | 0.620s | 0.450s | 1.38× |
| strjoin | 0.118s | 0.061s | 1.93× |
| strops | 0.293s | 0.153s | 1.92× |
| dictops | 0.381s | 0.209s | 1.82× |
| oo | 0.332s | 0.136s | 2.44× |
| genpipe | 0.161s | 0.061s | 2.64× |
| exc | 0.133s | 0.102s | 1.30× |
| listbuild | 0.294s | 0.172s | 1.71× |
| builtins | 0.232s | 0.251s | 0.92× |
| chain | 0.151s | 0.062s | 2.44× |

Geometric mean **1.89×**. The brief's "~2× slower than CPython" is correct.

It is also the wrong summary statistic for this question, and that matters more
than the number. The spread is 0.92× to 3.76×, and it is not random: Oro is at
parity where the work is a native call (`builtins`), close where the work is
raw dispatch (`loop`, `exc`), and worst where the work is **calls and attribute
access** (`fib` 3.76×, `genpipe` 2.64×, `oo` 2.44×, `chain` 2.44×). HTTP
parsing is calls and attribute access almost exclusively. A server workload
sits at the bad end of this distribution, not the middle, and §1.2 shows by how
much.

### 1.2 One HTTP head parse, four ways

The same algorithm — split the head on CRLF, split the request line on spaces,
then per header line find the colon, lowercase the name, trim the value, insert
into a map — transliterated into four languages and run on the same 110-byte,
four-header request. Idiomatic in each: all four allocate owned strings and
build a real hash map, so none of them is cheating by borrowing.

| implementation | per parse | vs Oro |
|---|---|---|
| Rust (`String`, `HashMap`) | **0.95 µs** | 13.3× faster |
| Go (`string`, `map[string]string`) | **1.06 µs** | 11.9× faster |
| CPython 3.12 | **3.49 µs** | 3.6× faster |
| Oro | **12.66 µs** | — |

Three things fall out of this table, and they are the three most useful facts
in the study.

**Rust and Go tie.** 0.95 against 1.06 µs. A systems language with no
abstraction penalty and a garbage-collected one with a scheduler land 10%
apart, because on this workload the cost is eleven string allocations and four
hash insertions, and both do the same eleven and the same four. There is no
region under Go for "we have Rust underneath" to claim. Whatever the target
number is, Go is already sitting on the floor of it.

**Oro is 3.6× CPython here, not 1.9×.** The headline geometric mean does not
describe server code. Header parsing is the `fib`/`oo` end of the distribution.
Any planning that used 2× has been using a number about 1.9× too optimistic.

**CPython being 3.6× faster is the most actionable line in the table.** CPython
is also a boxed, reference-counted bytecode interpreter with C primitives
underneath. It is not doing anything Oro is forbidden from doing. It is doing
`LOAD_METHOD`/`CALL_METHOD` instead of allocating a bound method per access, it
has a specialising interpreter, and its `bytes`/`str`/`dict` primitives have had
thirty years of attention. **There is a 3.6× gap to the interpreted ceiling
that has nothing to do with compilation**, and `bench/RESULTS.md`'s own
"what is left, ranked" list is a plan for the first half of it.

### 1.3 The full request path

`std/http.oro`, in memory, over an `io.buffer` — no sockets, no syscalls,
best-of-5 over 20,000 iterations:

| | per request |
|---|---|
| `serve_conn`, minimal request, exact route | **108.8 µs** |
| `serve_conn`, 4-header request, `/users/:id` route | **156.6 µs** |
| — `read_request` | 56.6 µs |
| — `write_response` (fresh `Response`) | 30.5 µs |
| — router dispatch (parameterised) | 7.8 µs |
| — `_http_date` | 5.8 µs |
| — `_is_field_value` on one 47-byte value | 10.6 µs |
| — `_is_token` on `b"User-Agent"` | 3.5 µs |
| — `read_until` on the whole head (**the Rust part**) | 0.50 µs |
| — `io.buffer` construction (the floor) | 0.58 µs |

And Go's equivalent, `http.ReadRequest` + `ServeMux` + write a response into an
`httptest.Recorder`, also entirely in memory:

| | per request |
|---|---|
| Go, parse + route + respond | **2.6-3.2 µs** |
| Go, `http.ReadRequest` alone | 1.6-2.0 µs |
| Go, `bufio.NewReader` alone (the floor) | 0.6-0.9 µs |

**The brief understates the gap.** It compares an in-memory Oro figure of 79 µs
against a Go figure of "roughly 10 µs" that must include syscalls, and gets 8×.
Measured like for like, in memory, on this machine, it is **109 µs against
2.9 µs — 38×**. It is better to know that now.

The brief's other figure holds exactly: `read_until` on the whole header block
costs **0.50 µs**, which is **0.32%** of the 156.6 µs request. The Rust
primitive layer is free. The cost is above it.

But "above it" is not the same as "interpretation", and this is where the
brief's framing needs correcting. Of the 156.6 µs:

- **~23 µs is per-byte validation written in Oro.** `_is_token` and
  `_is_field_value` iterate every byte of every header name and value, with a
  dict lookup per byte in the `_is_token` case. Four headers of this request
  carry about 62 bytes of name and value; at the measured ~300 ns/byte that is
  ~19 µs, plus ~4 µs for `_is_target` and the method token. **41% of
  `read_request`.**
- **~5.8 µs is `_http_date`**, recomputed from scratch on every response. Go
  caches the formatted date at one-second granularity. So does every other
  server.
- **The rest** is genuine per-request work — splits, slices, `to_str`, `lower`,
  `strip`, dict inserts, object construction — running through the interpreter.

§5 of `stdlib-server-design.md` wrote the falsification condition itself:

> profile a hello-world request. If Oro-level head parsing is more than ~30% of
> per-request CPU, revisit — and the first thing to reach for is one more
> *generic* primitive (a multi-delimiter `bytes.scan`), not
> `http.parse_request`.

It is 41%, on the validation alone. The test fires. The document's own
prescribed response — a generic primitive, not a protocol-aware one — is the
right one and is discussed in §6.

### 1.4 End to end, over a real socket

This is the measurement that reframes everything. A real Oro server
(`net.listen`, `accept`, `serve_conn`) against a Go client doing synchronous
request/response over one keep-alive connection, and the same client against
Go's `net/http` serving the same body:

| server | per request | req/s (1 connection) |
|---|---|---|
| Oro `std/http.oro` | **94-108 µs** | ~9,200-10,600 |
| Go `net/http` | **43.5-44.2 µs** | ~22,600-23,000 |

**2.2×.** Not 38×.

The reason is that both servers pay the same floor: two context switches, two
loopback traversals, four syscalls, and the client's own work. That floor is
~44 µs on this machine, and it is the whole of Go's number. Oro's ~60 µs of
extra CPU sits on top of a cost neither language controls.

This is not a trick of the benchmark. It is the actual shape of the problem.
Interpretation is a per-request CPU cost; a server's latency is dominated by
things that are not per-request CPU. §7 takes this to its conclusion.

Two honest caveats. The Oro server here is one blocking connection at a time —
the reactor (M3b) has not landed — so this measures *latency*, not throughput
under concurrency. And Oro has no parallelism inside one VM by design, so one
process is one core, where Go's number scales across twelve threads. Both
caveats are about throughput, and §7 handles throughput separately.

### 1.5 JSON, and the rule the stdlib is breaking

| | Oro | CPython (C accelerator) | ratio |
|---|---|---|---|
| `stringify` / `dumps`, 1.7 KB payload | **1,589 µs** | 26.3 µs | **60×** |
| `parse` / `loads`, 1.7 KB payload | **2,547 µs** | 22.1 µs | **115×** |
| `stringify`, one 5-field row | 73 µs | 3.0 µs | 24× |
| `parse`, one 5-field row | 111 µs | 2.4 µs | 46× |

`std/json.oro` is a character-at-a-time parser. Per character it does several
reads of `self.pos`, a string index, a comparison and a write of `self.pos` —
and every one of those `self.pos` touches is a `HashMap` probe into the
instance's field map. Measured directly, scanning a 1,700-character string
one character at a time:

| cursor held in | per character |
|---|---|
| an instance attribute (`self.pos`) | **552 ns** |
| a local variable | **276 ns** |

**A factor of two, for free, from where the cursor lives.** And `std/json.oro`
says in its own comments why it cannot have the free version:

> A class holding `text`/`pos` is the natural shape here: Oro cannot rebind a
> captured variable from a nested function.

No `nonlocal` is a language decision, taken for readability, and its measured
price is that **every per-byte parser written in Oro runs at half speed**.
That is not an argument for adding `nonlocal`. It is an argument that per-byte
parsers do not belong in Oro — which is what §5 of `stdlib-server-design.md`
already says, and which `std/json.oro` is a 381-line violation of.

This is the single most surprising finding in the study, and it is worth stating
plainly: **a realistic JSON API handler in Oro today spends about ten times as
long in `std/json.oro` as it does in the entire HTTP layer, the interpreter,
and the router combined.** Compiling the language would improve the `json`
number by about 2×. Writing it in Rust would improve it by about 60×.

### 1.6 Unit costs, and where the 12.66 µs goes

Marginal cost of one operation, measured as the difference against an empty
`while` loop of the same shape (empty iteration: 136-156 ns for ~9 bytecode
instructions, so **~15-19 ns per instruction**):

| operation | marginal cost | what it is |
|---|---|---|
| `p.x` (instance field read) | 74 ns | `HashMap` probe on `Rc<str>` |
| `p.x = 1` | 70 ns | `HashMap` insert |
| `p.m` (bound-method *access*, no call) | 124 ns | probe, miss, MRO walk, `Rc<BoundMethod>` alloc |
| `p.m()` | 331 ns | the above plus a frame |
| `g(p)` (free function, same body) | 173 ns | **a method call costs 1.9× a plain call** |
| `f0()` (0-arg user call) | 114 ns | frame from the pool, bind, return |
| `len(xs)` (cached global builtin) | 77 ns | the one inline cache in the system |
| `d["beta"]` (string-keyed dict read) | 125 ns | hash, and the key string is cloned |
| `b.find(b":")` on 18 bytes | 204 ns | of which `memchr` is single-digit ns |
| `b[0:14]` (bytes slice) | 282 ns | allocates |
| `b.to_str()` | 232 ns | allocates, validates UTF-8, scans for ASCII |
| `s.lower()` | 186 ns | allocates |
| `f"{t}: {42}"` | 387 ns | allocates |
| `ys.append(1)` | 185 ns | |

Decomposing the 12.66 µs head parse with these:

- **~8.7 µs** is primitive calls *and their call plumbing* — the sum of the
  marginal costs of every `split`, `find`, slice, `to_str`, `lower`, `strip`,
  `len` and dict store the parse performs, at their real argument sizes.
- **~3.9 µs** is the surrounding control flow: the `while`, the indexing, the
  comparisons, the increments, the function call and the tuple build.
- **~0.95 µs of that 8.7** is work Rust also has to do — eleven allocations and
  four hash insertions — since 0.95 µs is what the *entire* Rust version costs.

So the parse is roughly **69% primitive calls and their plumbing (of which about
a ninth is irreducible allocation), and 31% control flow**. That ratio is the
single most important input to §4: a compiler eats the 31% almost entirely, eats
the dispatch-and-stack part of the 69%, and eats none of the allocation.

Note what the plumbing consists of. `b.find(b":")` costs 204 ns and the actual
byte scan is a handful of nanoseconds. The other ~200 ns is: a `LoadAttr` that
probes the receiver's method table, an argument push, a call dispatch, boxing
the `i64` result into a `Value`, a refcount operation, and a stack pop. A
compiler removes the dispatch and the stack traffic. It does not remove the
boxing, the refcount, or the method lookup — not without type information it
does not have.

---

## 2. What in Oro helps a compiler

Everything in this section was verified against `src/`, or by running programs,
rather than taken from the README. Several of the brief's assumptions were
wrong, and those are marked.

**No `eval`, no `exec`, no `compile`, no `getattr`/`setattr`/`hasattr`, no
`globals()`/`locals()`, no `id()`, no `hash()`.** `builtins::lookup`
(`src/builtins/mod.rs:24-65`) enumerates all 29 global names and none of them
is there. Every call target that appears in the source is a name the compiler
can see. This is the precondition for everything else, and Oro genuinely has it.

**No metaclasses, no `__getattr__`, no `__setattr__`, no `__getattribute__`, no
`__new__`, no `__slots__`, no descriptors.** Not merely unimplemented —
*rejected at compile time* with a message, in `codegen.rs:1652-1667` and
`:822-829`. `get_attr` (`src/vm/mod.rs:3663`) is a pure Rust function that can
never re-enter the interpreter. **Attribute access has no user-visible hook at
all**, which is the property that makes shape-based optimisation and eventually
offset-fixed field access legal.

**Classes are closed after definition.** This is the strongest compiler-friendly
fact in the codebase and it was not on the brief's list. `Op::StoreAttr` errors
for every receiver that is not an `Instance` (`src/vm/mod.rs:1015-1022`), and
`Class::members.borrow_mut()` appears nowhere in `src/`. Verified by running it:

```
P.z = 7   →   RuntimeError: cannot set attribute 'z' on 'P' object
```

No monkey-patching of classes, ever. A method resolved once against a class is
correct forever. In Python this single fact is what makes almost all method
inlining unsound; in Oro it is sound.

**Single inheritance.** `Class { base: Option<Rc<Class>> }`
(`src/value.rs:256`). Method resolution is a chain walk, not a C3
linearisation. The chain is fixed at class-definition time.

**A closed 17-name dunder set**: `__init__`, `__str__`, `__repr__`, `__len__`,
`__bool__`, `__eq__`, `__ne__`, `__lt__`, `__gt__`, `__le__`, `__ge__`,
`__add__`, `__sub__`, `__mul__`, `__truediv__`, `__floordiv__`, `__mod__`,
`__pow__`. No `__hash__` — and its absence is load-bearing, since a class
defining `__eq__` is permanently unhashable. A fixed operator protocol means
every operator has a finite, enumerable set of things it can mean.

**No decorators.** Rejected in the parser (`src/parser/mod.rs:103`). Every
function bound to a name at module scope is the function that was written
there — no invisible wrapping.

**Static slot resolution already exists.** The symbol pass assigns every name a
fixed index before codegen; locals are `Vec<Value>` indexed by a `u16`, not a
dictionary (`src/vm/mod.rs:75-80`, `src/compiler/symbols.rs`). A compiler
inherits the entire local-variable analysis for free. This is normally one of
the harder pieces of front-end work for a dynamic language and it is done.

**Block scope and no `nonlocal`.** Names do not leak out of `if`/`for`/`while`
bodies (`ScopeKind::Block`, `src/compiler/symbols.rs:26-28`), and an inner
function can read an enclosing local but cannot rebind it. Closure capture is
therefore *read-only from the closure's side*, which means the set of
mutable-cell variables is small and known statically. Register allocation over
Oro locals is nearly trivial by dynamic-language standards.

**Type names are not callable, and conversion is an explicit method.**
`str()`/`int()`/`list()` exist only to raise
(`src/builtins/mod.rs:474-487`). The cast set is closed and finite:
`to_str to_bytes to_int to_float to_bool to_list to_dict`. Every type
conversion in a program is syntactically visible. A compiler always knows the
result type of a conversion, which is more than can be said for `int(x)` in
Python.

**A fixed global namespace.** `Op::LoadGlobal` resolves only builtins and
exception classes, and the existing inline cache needs no invalidation
"because there is no assignable module namespace"
(`src/compiler/mod.rs:298-302`). Builtin calls are statically bindable.

**`Op` is 8 bytes and `Copy`; `Value` is 16 bytes**, both enforced by tests
(`src/compiler/tests.rs:106-116`, `src/value/tests.rs:10-18`). The IR a
compiler would start from is already dense and already register-shaped.

**One brief assumption that is wrong, in the helpful direction:** function
parameter and return annotations **already parse and are silently discarded**.

```
def f(a: int, b: str = "x") -> str:
    return b
print(f("not an int"))   # prints "x"
```

The syntax slot for gradual typing exists today and currently tells a lie. That
is either an opportunity (§6.3) or a bug, but it is not nothing, and it should
not stay as-is through a freeze.

---

## 3. What blocks it

### 3.1 The three that are real

**`import` is fully dynamic, and it compiles source at runtime.**
`Vm::import_module` (`src/vm/mod.rs:2778-2860`) resolves a name at runtime
against builtin modules, then a cache, then the three `include_str!`-embedded
stdlib modules, then **`<script dir>/a/b/c.oro` read from disk and passed to
`compile_source` while the program is running**. And `import` is an ordinary
statement with no placement restriction — the corpus itself imports inside a
function and inside a `try` (`corpus/divergence/50_task_isolation.oro:100,120`).
Verified by running it.

So **there is no closed-world program**. An AOT compiler cannot know the whole
program without either (a) restricting `import` to module top level and
resolving it statically, or (b) shipping the entire compiler in the binary to
handle the dynamic case, which for a native backend means shipping a code
generator, which for LLVM means shipping LLVM.

This is the single biggest structural blocker, and (a) is a small, defensible
language change. It is still a language change, made to a language whose thesis
is that it stops changing.

**Module-level function names are late-bound, and rebinding is visible to
already-compiled callers.** Verified by running it:

```
def target():  return "first"
def caller():  return target()
print(caller())          # first
def target():  return "second"
print(caller())          # second
```

Module-level names are the module frame's own locals and cells
(`src/compiler/mod.rs:344-346`); a function that reads `target` reads it
through a shared `Rc<RefCell<Value>>`. No call site anywhere in an Oro program
is statically bindable without a guard. Direct calls, inlining and
devirtualisation all require either a guard-and-deoptimise mechanism (a JIT
feature, not an AOT one) or a rule that a module-level `def` name cannot be
rebound. The rule is a good rule and Oro would probably want it anyway. It is
still a language change.

**Heap identity is observable for 18 of 26 `Value` variants.** The just-landed
`Value::identity()` (`src/value.rs:757-774`, commit `c31ab20`) makes twelve
reference types answer `is`, compare by address, and act as dict keys; `Str`,
`Bytes`, `List`, `Tuple`, `Dict` and `Range` were already compared with
`Rc::ptr_eq` in `value_is`. Verified:

```
inst is inst   → true      "ab" is ("a" + "b")  → false
list is list   → true      1 is 1               → true
```

The consequence for a compiler is precise: **it may not copy, duplicate,
intern, or unbox any of those eighteen.** Interning two equal string literals
makes `is` answer `true` where it answered `false`. Stack-allocating a
non-escaping list makes it a different object. Unboxing an instance destroys
its identity as a dict key. The three types that *are* freely unboxable —
`None`, `Bool`, `Int` — are exactly the three that `value_is` compares by value
(`src/vm/mod.rs:4120-4122`).

The mitigation is that the address never becomes a *number*: there is no `id()`
and no `hash()`, and dict order is insertion order, so no program can observe
where the allocator put anything. That keeps the door open for a compiler to
reorder allocations. It does not open the door to eliminating them.

Three inconsistencies here are worth fixing before the freeze rather than after,
and were verified by running them: **`x is x` is `false` for a float**, `false`
for a bignum, and `false` for a bound method (`m = a.m; m is m` → `false`).
The float and bignum cases look like omissions rather than design — and the
`Int`/`Big` boundary being invisible to `==` but visible to `is` is exactly the
kind of thing a frozen language should not freeze by accident.

### 3.2 The ones the brief listed, and what they actually cost

**Every value is a 16-byte tagged union — confirmed, and it is not the problem
people assume.** 16 bytes is two registers. Passing one is cheap. The cost is
not the width; it is that 21 of the 26 variants are `Rc` pointers, so touching
a value means touching a refcount and following a pointer.

**Refcounting on every load — confirmed, and it is on the hot path with no
escape.** `Op::LoadConst` and `Op::LoadFast` both `clone()`
(`src/vm/mod.rs:767,772`); there is no borrowed-access path anywhere in the
dispatch loop. Cycles leak by design, with no collector and no `Weak` anywhere
in `src/`.

A compiler can do genuinely well here — this is one of the places where
compilation pays. Static lifetimes let most `Rc` increments and their matching
decrements cancel: a value loaded, passed to a call, and dropped never needs a
refcount touched at all. Rust's own borrow analysis is not available, but
straightforward escape analysis within a function eliminates a large fraction.
Call it 30-50% of refcount traffic on straight-line code (**estimate**).

**Heterogeneous containers — confirmed.** `[1, "a", null, 3.5, b"z", (1,2),
{"k": 1}]` runs; `{"a": 1, 2: "b", (1,2): null}` runs. `List` is
`Rc<RefCell<Vec<Value>>>`; there is no element type anywhere.

**`null` inhabits every type — confirmed.** `Value::None` is a variant of the
one universal `Value`; there are no variable type declarations. Any inference
scheme therefore infers `T?` for essentially everything and must either box the
option or force the programmer to prove non-nullness.

**First-class functions and closures — confirmed**, and closures capture by
*cell*, shared with the defining scope (`Op::MakeFunction`,
`src/vm/mod.rs:1335-1346`). Functions are hashable identities and dict values.

**Attribute access as a hash lookup — confirmed, and worse than described.**
An instance field read is a `std::collections::HashMap` probe with SipHash-1-3
(`src/value.rs:307`). A *method* access is that probe, then a miss, then a
linear walk up the base chain with one `HashMap` probe per class, then a fresh
`Rc<BoundMethod>` allocation (`src/vm/mod.rs:3736-3744`). **There is no shape,
no hidden class, no offset cache, and no inline cache on attribute access
anywhere in the system** — the only inline cache in Oro is on `LoadGlobal`.

This is where the largest single win in the whole document sits, and it does
not require a compiler. `bench/RESULTS.md` already ranks both fixes first and
second on its own list.

### 3.3 The ones nobody listed

**Every integer `+`, `-`, `*` is a checked operation with a heap-promotion
branch** (`src/vm/arith.rs:145-177`), `**` always routes through `BigInt`, and
`Value::as_number()` **clones the `BigInt` by value** on every arithmetic
operation involving one (`src/value.rs:848`). Worse, `Op::BinAdd` first calls
`instance_method(&a, "__add__")` before reaching arithmetic
(`src/vm/mod.rs:883-899`) — a dunder probe on the hot path of every addition.
A compiler with no type information must emit all of it.

**Deterministic destruction is a specified language guarantee, not an
implementation detail.** The README promises a file closes when its last
reference drops, and `Drop for TaskHandle` (`src/task.rs:82-89`) prints an
unjoined-task failure and sets the process exit status. So: **a compiler may
not extend a value's lifetime, may not sink or hoist a `Stream`, and may not
replace refcounting with tracing GC.** That last one closes off the single
biggest performance lever available to compiled dynamic languages. Every fast
dynamic runtime in §5 — PyPy, LuaJIT, V8, Julia — uses a tracing collector, and
several of them get a large fraction of their win from bump-allocation in a
nursery, which requires one.

**Green threads need resumable frames at six specific sites.** Scheduling is
cooperative with no check inside the dispatch loop; the only way out is a
`Step::Park` from `Recv`, `IterRecv`, `Send`, `Join`, `Import` or `Yield`
(`src/vm/sched.rs:64-89`), and resumption pushes one value onto the sleeping
frame's operand stack. `import` can park, so **every `import` is a yield
point**. A native compiler must materialise a resumable state at each — either
by keeping an explicit frame representation (giving up much of the win) or by
a CPS/state-machine transformation of every function that can transitively
reach one (a large, invasive pass).

**Generators are heap frames, type-erased and downcast on every resume**
(`src/value.rs:313-319`, `src/vm/mod.rs:1164-1167`), and are first-class values
that can cross tasks through a channel.

**Exceptions are VM values with an explicit block stack**, and unwinding
truncates the operand stack *and* five in-flight "job" stacks to depths
recorded at block entry (`src/vm/mod.rs:102-119,3184`). `break` and `continue`
unwind through `finally` too. A compiler must reproduce this precisely, and
cannot use Rust's unwinder to do it (the crate is built `panic = "abort"`).

**The stdlib is only partly frozen.** Three Oro modules are `include_str!`-baked
into the binary — `io`, `json`, `http` (`src/vm/stdlib.rs:14-18`) — and
user modules are `.oro` files read from disk and compiled at run time. The
"frozen closed stdlib" the brief hoped for is true of the standard library and
false of the program.

---

## 4. What compiling as-is would actually buy

Take the question at its narrowest: emit native code for Oro's bytecode
semantics exactly as they are, with no type inference and no language changes,
keeping the same `Value`, the same `Rc`, the same `HashMap` attributes, the
same bignum promotion.

**What it removes.** Instruction fetch — a bounds-checked `frames.last_mut()`,
two array reads, and two unconditional stores of line and column into the task
on *every single instruction* (`src/vm/mod.rs:728-761`). The 69-arm `match`.
The `Result<Step, RuntimeError>` return from `Vm::step` and the branch on it.
The operand stack, for any value whose lifetime is inside one basic block.
Frame setup for calls that do not escape. And a large fraction of refcount
traffic, since matched increments and decrements cancel under static lifetimes.

**What it keeps.** Every allocation. Every `Value` box. Every `HashMap` probe
for every attribute. Every MRO walk. Every `Rc<BoundMethod>`. Every overflow
check and bignum-promotion branch. Every dunder probe before every operator.
Every UTF-8 validation and ASCII scan. The entire body of every Rust primitive.

Applying that to §1.6's decomposition of the head parse — 31% control flow,
69% primitive calls and their plumbing, of which about a ninth is irreducible
allocation — a compiler takes most of the 31%, and perhaps a third of the 69%
(the dispatch and stack-traffic parts of each call, not the boxing, the
refcounting, the method lookup or the allocation). That predicts **1.8-2.3×**
on this workload.

On dispatch-bound code — the `loop` benchmark, arithmetic in a tight `while` —
it would do much better, perhaps **3-5×**, because there the removable fraction
is nearly everything. On call- and attribute-heavy code it does worse. Weighted
toward the server workload that motivates the question:

> **A realistic expectation for AOT-compiling Oro's current dynamic semantics
> is 2×, with a range of 1.5-3×.** (Estimate, from the measured decomposition;
> §5 gives the external evidence.)

Four sanity checks on that number, all of which say it is not pessimistic.

**Where the time actually goes in a language like this has been measured.**
Ismail and Suh's overhead analysis of CPython (IISWC 2018, averaged over 48
benchmarks) attributes **14.2% of total execution time to dispatch**, against
18.4% for C function call overhead, 9.1% for name resolution and 4.8% for
function setup — with 64.9% of runtime being overhead of some kind and only
35.1% useful computation. **Native compilation takes the dispatch slice.
Everything else on that list survives it**, unless the dynamism that requires it
is deleted too. See §5 for the full table.

**The published analogue.** Cython's own documentation says compiling
*unannotated* Python "merely gives a 35% speedup" — 1.35× — and 20-50% in pure
Python mode with no type declarations. That is §6.1, measured, by the project
that has been doing it longest.

**The generous upper bound.** Ertl and Gregg observe that OCaml's *native*
compiler is only 2-15× (average 6×) faster than OCaml's own *bytecode*
interpreter — the same language, the same semantics, compiled against
interpreted. OCaml is statically typed, so its native compiler gets to unbox
everything Oro's could not. 6× is therefore the ceiling for compilation alone,
achieved with every advantage Oro lacks.

**And CPython, which is still interpreting, is already 3.6× faster than Oro on
this workload** (§1.2) — so 2× from compiling would not even reach CPython's
current *interpreted* speed on server code. That is a statement about how much
VM work is left undone, not about how little compilation buys.

---

## 5. The precedents, and what each one teaches

Compiling a dynamic language is not a new idea and its results are unusually
well documented. Two conclusions run through every system below, and it is
worth stating them before the details.

**The first is quantitative.** Ismail and Suh's overhead analysis of CPython
(IISWC 2018, averaged over 48 benchmarks) decomposes where the time actually
goes:

| overhead category | share of total execution time |
|---|---|
| C function call overhead | 18.4% |
| **dispatch** | **14.2%** |
| name resolution | 9.1% |
| function setup / cleanup | 4.8% |
| type checks, boxing, GC, stack, allocation | the remainder |
| **all overhead combined** | **64.9%** |
| actual useful computation | 35.1% |

**Native compilation removes the dispatch slice. Dispatch is 14%.** Everything
else on that list — boxing, dynamic name and attribute resolution, call-frame
setup, allocation, garbage collection — survives compilation unless you also
delete the dynamism that requires it. That single table is the strongest
external support for §4's estimate of 2×, and it is measured on the language
Oro is modelled on.

The other bound, from the opposite direction: Ertl and Gregg (JILP 2003) note
that **OCaml's native compiler is only 2-15× (average 6×) faster than OCaml's
own bytecode interpreter** — same language, same semantics, compiled against
interpreted. And OCaml is *statically typed*, so its native compiler gets to
unbox. 6× is therefore a generous upper bound for what compilation alone can do,
achieved by a language that has every advantage Oro lacks.

**The second conclusion is a product conclusion.** *Nobody has compiled a
dynamic language to native code and kept the dynamism.* Every AOT success below
— Crystal, Codon, Nim, mypyc, Julia's `juliac` — works by deleting dynamic
features. The compiler is not the hard part. The semantic amputation is, and it
is a product decision, not an engineering one.

### 5.1 Crystal — the closest existing thing to "compiled Oro"

Ruby-like syntax, whole-program type inference with union types, LLVM AOT, no
annotations required in the common case. This is the design the question is
really asking about, so it gets the most space — and most of what people
"know" about it turns out to be wrong in interesting ways.

**How the inference actually works, and where it stops.** Two distinct
mechanisms. Locals and method arguments get ordinary dataflow inference, with a
union formed at every join and `Nil` added if any path leaves a variable
unassigned. **Instance, class and global variables get something quite
different: a hard-coded table of about six recognised syntactic shapes** —
literal assignment, `Type.new(...)`, assignment from a type-restricted
parameter, a default parameter value, a class method with a return-type
restriction, a `lib` function return. The compiler needs those types *before*
it can resolve method calls, so it cannot infer them the general way.

Anything outside those six shapes fails, with `can't infer the type of instance
variable '@x' of Foo`, and the only fix is to write `@x : T`. The documented
failure classes include assignment from an instance method's result **even when
that method has a return-type annotation** (crystal-lang/crystal#5653, #3894),
class variables assigned via instance methods (#11805), and — pointedly —
instance variables generated by macros, such as `JSON::Field(presence: true)`
(#11387). Crystal's own maintainers proposed replacing the whole scheme with
*mandatory* instance-variable declarations (#1824), explicitly in order to buy
incremental compilation, and acknowledged that it contradicted the language's
own "no mandatory types" philosophy. It never landed.

**Unions are not heap-boxed — they are wide.** A Crystal union lowers to an LLVM
struct `{ i32 type_id, [N x i64] payload }`, with `N` sized to the widest
member and the tag padded to eight bytes. `UInt32 | UInt16` measures **12
bytes**. So there is no allocation per value — but every union-typed slot is as
wide as its widest member plus a tag, every call on a union is a tag switch, and
the width propagates into every container holding one. For Oro, where containers
are heterogeneous by design and `null` inhabits every type, *almost everything*
would infer as a union.

And separately: **Crystal classes are reference types on a Boehm-Demers-Weiser
conservative garbage collector.** Conservative scanning, over-retention, an
allocator materially slower than `malloc`, stop-the-world mark-sweep.
Compilation does not remove that tax — and Oro would not be allowed to adopt it
at all, because deterministic destruction is a promise in the README (§3.3).

**Compile times, with real numbers, and the cause is not what everyone
assumes.**

| measurement | time |
|---|---|
| fresh Amber web app, initial build | ~25 s |
| fresh Lucky app, initial build | ~29.5 s, then ~5 s per command |
| empty Lucky project, adding one six-line model | ~35 s |
| **Amber app after 100 generated resources** | **~7 minutes, every start and every test run** |
| Crystal compiler self-compile, profiled | **367.8 s** |

That profile is the interesting part:

```
semantic     12.6 s    3.4%
codegen     355.2 s   96.6%
  |- LLVM run_passes            252.3 s   71%
  |- LLVM emit_to_file           93.3 s   25%
```

**The type inference is not the bottleneck. LLVM is.** (Caveat: this looks like
a `--release --single-module` build, where LLVM optimisation dominates; in dev
builds the semantic share is proportionally larger. The direction is robust, the
ratio is release-specific.)

The *incrementality* problem has a different cause — **monomorphisation**. Every
method is instantiated per argument-type combination, once per subclass for
inherited methods, once per `T` for generics even where reference types would
emit identical code. Instantiation clones AST nodes and holds type-check results
until codegen; compiling the compiler consumed about **1 GB** of memory
(crystal-lang/crystal#4864).

**Incremental compilation does not exist, fifteen years in.** The official
issues (#10568, #10198) are open. Parallel codegen landed (PR #14748) but is
*excluded* under `--release`, i.e. not in the slow mode. A February 2026
community fork with file-level incrementality reports warm rebuilds of 300-380
ms — and an independent test measured it **~50% slower on cold builds and ~75%
slower on a one-line rebuild** than the status quo, macOS only. The objection
raised in that thread is the fatal one: type restrictions do not guarantee
actual return types, so a body-only change can alter inferred types elsewhere,
and safe incremental skipping is **semantically unsound in general** under
whole-program inference.

**Binary size, and the fact that decides this for Oro.**

| build | size |
|---|---|
| C, gcc, hello world | 7,168 B |
| Crystal, default `crystal build` | 1,270,216 B |
| Crystal, `--release --no-debug`, stripped | 158,344 B |
| Crystal, empty prelude, direct LibC, stripped | **6,216 B** |
| real 9,000-LOC Crystal daemon, static musl | **6.9 MB** |

**LLVM is not linked into Crystal-produced binaries.** Three independent
confirmations: a minimal Crystal program is 6.2 KB stripped, smaller than gcc's
C hello world, and `libLLVM.so.18` is 114-118 MB, so it demonstrably is not in
there; a real 9,000-line daemon produces a 6.9 MB static binary; and Crystal's
static-linking documentation enumerates the runtime dependencies as libpcre2,
libgc, libevent and libc, with LLVM appearing only in the context of packaging
the *compiler*.

**And this is exactly why Crystal's arrangement is unavailable to Oro.** Crystal
ships a compiler (which links LLVM, and needs a 114 MB build image and a
version-pinning treadmill) and produces binaries (which do not). `oro` is *one
binary that is both*, and `oro script.oro` is the entire user interface.
Compiling Oro means either embedding a code generator in the shipped binary, or
splitting the product in two and giving up the thing that makes it a scripting
language. **Crystal's answer to the binary-size problem does not transfer.**

**What it cost.** Started June 2011 at Manas Technology Solutions;
self-hosting November 2013; first public release June 2014; **1.0 in March
2021 — a decade.** Multithreading became production-ready in **July 2026, fifteen
years in.** Thirteen core team members today, ~450 contributors, and declared
sponsorship of roughly **$2.5M** (84codes at €22,000/month since 2018, Manas at
$5,000/month). At 1.0 the announcement still listed multithreading, Windows and
ARM as not production-ready.

**What it forced on users.** Wikipedia's summary is the cleanest version:
Crystal compiles to efficient native code "at the cost of precluding the dynamic
aspects of Ruby." Concretely: no `eval`, no `send`, no `binding`, no
`instance_eval`/`class_eval`, no runtime `define_method`, no `remove_method`, no
runtime code generation, no dynamic loading. `method_missing` survives only as a
compile-time macro. Metaprogramming is a separate compile-time macro language
you have to learn.

The underlying logic is inescapable and worth carrying into every other section
of this document: **if the compiler must see the whole program in order to infer
types, then nothing can be added to that program after compilation without
invalidating the inference it already did.** Whole-program inference and runtime
dynamism are mutually exclusive by construction. Oro's runtime `import` (§3.1)
is exactly the thing this forbids.

**How fast is it, really?** The best controlled source is a ZHAW study (Crystal
1.0 / LLVM 10 against Ruby 3.0.1 and GCC 10.2, ten runs each), ratios relative
to Crystal:

| benchmark | C | Go | Ruby |
|---|---|---|---|
| recursive Fibonacci | 0.44 | 1.32 | 26.4 |
| iterative Fibonacci | 1.33 | 0.84 | 211 |
| writing lines to files | 2.07 | 1.01 | 10.9 |
| C bindings | 0.99 | 5.40 | 9.30 |
| startup time | 0.22 | 0.73 | 9.95 |
| **TCP sockets** | **0.89** | **0.97** | **1.00** |
| median | 0.89 | 1.01 | 9.95 |

Ruby is about 10× slower at the median — and **identical on sockets**. The
study's own conclusion: *"the performance of TCP sockets were almost the same
for all languages, even Ruby. This shows that when using sockets the bottleneck
is the operating system itself."* That is §7 of this document, measured
independently, four years earlier, in a study whose entire purpose was to show
how much faster Crystal is than Ruby.

At the web level the same collapse appears: Kemal (Crystal) against Sinatra
(Ruby) is cited at roughly 20× on hello-world and **about 1.5× once a
PostgreSQL query with a join is in the loop.**

**The lesson.** Crystal is the existence proof that this can be done and it is
also the price list: a decade, $2.5M, 450 contributors, permanent compile-time
pain that turns out to be LLVM's fault, partial inference that needs annotations
on exactly the construct servers are made of, a conservative GC, and a language
that is no longer the one it resembles — for a win that mostly evaporates when
a socket or a database appears.

### 5.2 Codon — the closest precedent to what is actually being proposed

MIT CSAIL, CC 2023. An AOT LLVM compiler for a Python-*like* language, and the
most directly relevant case study in this document after Crystal.

Its inference, **LTS-DI**, is bidirectional and Hindley-Milner-flavoured,
requires no annotations, and rests on three things: monomorphisation, **delayed
instantiation**, and — the important one — **localization: each function is an
isolated type-checking unit.** That last property is precisely what Crystal
lacks and precisely why Codon can be modular where Crystal cannot. If anyone
ever does build the compiler this document recommends against, *this* is the
architecture to copy, not Crystal's.

**Results, and the gap between the two numbers is the point.** Codon reports
"10-100× or more" over CPython on microbenchmarks. On **about ten real genomics
applications the speedup over the original hand-optimised implementations was
5-10×.**

**What it deletes.** The paper is explicit: "it intentionally omits some dynamic
features." Disallowed: runtime polymorphism, runtime reflection, dynamic
modification of method tables, dynamic addition of class members, metaclasses,
class decorators, and **heterogeneous collections**. Integers are 64-bit, **not
arbitrary precision**. Some numeric operations follow C semantics rather than
Python's.

Look at that list against Oro's feature set. Heterogeneous collections are legal
Oro and are used throughout `std/`. Arbitrary-precision integer promotion is one
of Oro's five locked architectural decisions. **Codon is the strongest available
evidence both that this project is achievable and that the achievable version is
not the language you started with.**

### 5.3 Nim — the static route, and the small-binary champion

Statically typed from 2005, compiles through C. Against Python on
benchmarks-game microbenchmarks: **1.4× to 11.6×** — 10.6× on `fasta`, but only
1.5× on `binary-trees` and 1.4× on `regex-redux`. **No real-application
Nim-versus-Python comparison with hard numbers appears to exist**, so the
"10-100×" folklore should be treated as microbenchmark-only.

Its binaries are the best in this document: with `-d:release`, LTO, strip and
`--opt:size`, **39.5 KiB on Linux**; 26.5-30 KiB with musl; 6.1 KiB via the
`zig cc` backend. It gets there by requiring a C toolchain on the build machine
— a dependency Oro's install story ("one file, verify the hash, done") does not
have.

Timeline: started 2005, 1.0 in September 2019 — fourteen years, essentially one
BDFL until Status.im funded about two FTEs in 2018.

**The lesson.** Being statically typed from the start is the cheap way to native
speed and small binaries. That option was foreclosed when Oro chose Python's
semantics, and it cannot be recovered without becoming a different language
(§6.6).

### 5.4 Julia — type specialisation, and what it costs everywhere else

Julia's LLVM JIT specialises every method on the concrete types of its
arguments. On type-stable numeric code the multiples are real and large:
recursive fib ~71×, mandelbrot ~96×, quicksort ~38× against CPython, from
Julia's own benchmark data.

**And one row in that same data tells the whole story: matrix multiply, Python
85.0 ms against Julia 70.2 ms — 1.2×.** Both call BLAS. Julia's headline
multiples exist only where Python has no C escape hatch. Oro's server workload
*is* the escape-hatch case: the primitives are already Rust.

Three costs:

- **Type instability is a cliff, not a slope.** Code the compiler cannot prove
  type-stable falls back to boxed dynamic dispatch, commonly ~10× slower and
  documented as bad as ~60×. **A language whose containers are heterogeneous and
  whose `null` inhabits every type is type-unstable by construction.**
- **Latency.** "Time to first plot" was tens of seconds — GLMakie 64.6 s on
  Julia 1.7. Native code caching in package images (1.9) fixed most of it
  (GLMakie 1.66 s, a 39× improvement) at the cost of 10-50% longer
  precompilation. It is still not a language you reach for to run a 20-line
  script.
- **Size.** The system image alone is ~173 MB; installs run 300-600 MB.

**And the static-compilation story is the sharpest irony in this whole
document.** `PackageCompiler.jl` produces a minimum app of ~300 MB.
`StaticCompiler.jl` works only by avoiding the GC entirely. The current
experimental `juliac` produces a 1.1-1.7 MB binary — **plus ~91 MB of required
shared libraries** — and, per LWN, **prohibits dynamic dispatch**, so "most
public packages don't work", and compiled programs cannot read files or stdin.
**Julia's static-compilation path forbids the exact feature that is Julia's
entire value proposition.**

### 5.5 PyPy — a tracing JIT on unmodified Python, and the honest number

The most direct answer to "what if we just made the dynamic language fast
without changing it".

**The number has quietly come down.** PyPy's own front page today says **"about
3 times faster than CPython 3.11."** Its FAQ says "3 times the speed of CPython
2.7" for typical programs. PyPy's June 2026 benchmarking post reports **4.3×**
on newer hardware. The "7×" figure people still quote is CPython-2.x-era
folklore that PyPy itself no longer states. **Defensible range: 3-4.3× geometric
mean.** And the suite is not neutral by its own admission: it is "a combination
of real-world Python programs *and benchmarks for which we found PyPy to be slow
(and improved)*."

**What it cost.** Started 2002-2003. EU FP6 grant of €1.3M, a Eurostars grant of
about half a million euro, Mozilla's $200,000. Over 350 lifetime contributors.
Measured by clone: the RPython toolchain is **453,180 lines excluding tests**
(the JIT backend alone is 191,012), and the PyPy interpreter another 165,159 —
**about 690,000 lines of their own code, over twenty-three years, for 3-4.3×.**

**Why it is not more.** `cpyext`, the C-extension layer, pays a border crossing
per trivial operation and marshals arguments because PyPy's object layout is not
`PyObject*`. Warmup, per PyPy's own FAQ: "our JIT has a very high warm-up cost,
meaning that any program is slow at the beginning" — benchmarks need "at least
one second, preferably a few seconds", **which rules out CLI tools, cron jobs
and test runs by PyPy's own documentation**. And the GC is `incminimark`:
incremental, generational, **moving**, nursery-based — no refcounting, therefore
no deterministic finalisation, which is a semantic change Oro is not allowed to
make.

**And warmup is worse than the folk model.** Barrett, Bolz-Tereick, Killick,
Mount and Tratt, "Virtual Machine Warmup Blows Hot and Cold" (OOPSLA 2017),
across eight VMs including PyPy: **only 30.0-43.5% of ⟨VM, benchmark⟩ pairs
consistently reached a steady state of peak performance.** On one machine, 43.5%
of pairs showed "bad inconsistency". The textbook two-phase JIT model is simply
wrong for the majority of cases.

**And the ending.** In February 2026, **NumPy dropped PyPy support** —
merged by a core developer of both projects, citing that PyPy "has not released
a Python 3.12 version", along with a list of technical incompatibilities
(identity-versus-value equality, refcounting differences, inability to break
complex C-extension reference cycles). The "no longer developed" framing is
overstated — 494 commits in the trailing year, a release in May 2026 — but the
underlying complaint is correct: PyPy supports 3.11 while CPython is at 3.13+,
and it has never closed that gap.

**The lesson, and it is the central one for this study.** *Twenty-three years of
the best available work on making an unmodified dynamic language fast produced
3-4.3×, cost 690,000 lines, and did not move the ecosystem* — because the
workloads that mattered were dominated by C extensions and by I/O rather than by
interpretation. Oro is asking the same question and will not get a better answer.

### 5.6 Cython and mypyc — the gradual, annotation-driven path

**Cython** on *unannotated* Python: its own tutorial says compiling as-is
"merely gives a 35% speedup" — **1.35×** — and pure-Python mode with no type
declarations "about 20%-50%". **This is the closest published analogue to §6.1
of this document, and it is why §4's estimate is 2× rather than 10×.** With
static type declarations the same example reaches 4×; converted to a `cdef`
C-level function, 150×. At that point the code is C with Python syntax and will
not run under CPython at all.

**mypyc** gives the most useful single data point in this section: **mypy
compiled with mypyc runs about 4× faster than interpreted.** A real, large
production codebase, not a microbenchmark.

**And the caveat that must travel with the number every time it is cited: mypy
was already 100% type-annotated, because it is a type checker.** The marginal
annotation cost was approximately zero. That asymmetry is exactly what does not
transfer to a normal codebase — or to Oro, where annotations currently parse and
mean nothing (§2).

The second production user is more representative. **Black** got 1.82× on the
initial pull request, later around 1.5-2× — **at a cost of "more than two years"
of integration work**: fixing mypyc compiler bugs, removing incompatible
dataclass decorators, rewriting nested functions as callable class instances
("hacky"), converting negative integer class constants to positive as a
workaround, and skipping mock-based tests. A third measurement found ~25% on
unmodified code, up to 50% with hand-optimisation.

**What mypyc's compiled classes give up**, and this is the list Oro would have
to adopt: no arbitrary monkey-patching ("you can't... [replace] functions or
methods with mocks in tests"); early static binding inside a compiled unit;
`Final` values inlined at compile time so runtime writes have no effect;
`__init__` enforced even through unpickling; `__index__`, `__getattribute__` and
`__delattr__` unsupported; only property/staticmethod/classmethod descriptors;
**single inheritance only**; nested classes and conditionally-defined
functions unsupported; **generator expressions silently converted to list
comprehensions** — a semantic change, not just a performance one; degraded
introspection with unreliable `inspect.signature()` and no debugger breakpoints
inside compiled code; and a boxing/type-check cost at every crossing of the
compiled/interpreted boundary.

**The lesson.** *Full annotations plus restricted dynamism plus a C backend
equals 4× on a codebase that was already fully annotated, and 1.5-2× after two
years of work on one that was not.* That is the realistic top of §6.3.

### 5.7 LuaJIT — the ceiling, and what the ceiling costs

The best result anyone has achieved for a dynamic language of this class.
Independent measurements put it at 3.7-6.7× reference Lua on numeric loops and
around 25× on some; one 2016 measurement had C at 0.014 s, LuaJIT at 0.042 s and
Lua at 1.027 s — **~25× Lua, ~3× slower than C.** LuaJIT's own site claims
">30× in simple micro-benchmarks like function calls" while warning that
"extreme speedups won't be found in real-world applications."

**What it took.** Started 2005 by **Mike Pall, essentially alone**; the 2.0
tracing JIT took roughly six years of sponsored engineering. There is **no
explicit person-year figure in any source** — "about ten years by about one
person" is the best-supported proxy and should be labelled as one. The
interpreter is not a C switch loop: it is **hand-written assembly per
architecture** (x86, x64, ARM, ARM64, PowerPC, MIPS) through Pall's own DynASM
preprocessor, with NaN-boxed values. About 42k lines of C in total, and the
binary stays under a megabyte — the one respect in which it fits Oro's identity.

**What happened afterwards is the actual lesson.** When Pall stepped back around
2015, **LuaJIT 2.1 stayed in beta for roughly a decade** and there is still no
numbered stable release; the project moved to a rolling scheme where the version
is a POSIX timestamp. Maintenance passed de facto to **OpenResty's fork**, with
patches never merged upstream. The "not yet implemented" bytecodes that abort
traces are still a maintained list. Branchy, polymorphic and object-oriented
code remains the structural weakness — "trace explosion" — and the proposed fix
"requires a major redesign" and sits in an open issue. **And the garbage
collector was never finished**: the quad-colour incremental generational
collector designed for LuaJIT 3.0 never shipped, and the realised mitigation
(GC64) raised the allocation ceiling from 2 GB to 128 TB, fixing addressing
rather than the collector.

**Why nobody has replicated it.** A tracing JIT needs, simultaneously, a very
fast assembly interpreter to fall back to, a very fast trace compiler, efficient
collection of abandoned traces, and heuristics against trace explosion — all
hand-written per architecture. PyPy chose meta-tracing specifically to avoid
doing that per language, and paid for it with the 690,000 lines in §5.5.

**The lesson.** The ceiling is real and high, and the entry price is a decade of
one exceptional specialist's life plus a codebase only that specialist could
advance. For a project whose thesis is *finishable*, taking on a
subsystem-nobody-else-can-maintain is a contradiction in terms.

### 5.8 CPython 3.11-3.15 — what a funded expert team gets from a modern JIT

Two results, and the contrast between them is the point.

**PEP 659, the specialising adaptive interpreter (3.11).** Bytecode that
rewrites its own hot instructions into type-specialised variants behind guards,
with a deoptimisation path. No machine code at all. Officially: **"CPython 3.11
is an average of 25% faster than CPython 3.10"** on `pyperformance`, with the
honest caveat in the same document that "the overall speedup could be 10-60%",
that some benchmarks slowed slightly and others nearly doubled, and that
I/O-bound and C-extension-dominated code sees nothing. **This is the best
return-on-effort in the entire study: ~25% from making the interpreter smarter,
zero semantic change, zero user action.**

**PEP 744, the copy-and-patch JIT (3.13/3.14).** Real machine code, stitched
from stencils generated at *build* time by LLVM — so LLVM is a build dependency,
not a shipped one, and the runtime addition is small. That is the only
machine-code technique whose binary cost is compatible with Oro's identity.
(The technique is Xu and Kjolstad, **OOPSLA 2021**, not PLDI.)

And the measured result at launch was **"about as fast as the existing
specializing interpreter on most platforms"** — i.e. roughly zero. What's New in
3.13: "performance improvements are modest", shipped **disabled by default**.
An independent measurement found 0.98× on Fibonacci — *slower* — and 1.21× on
bubble sort. What's New in 3.14 gives no JIT speedup figure at all. As of a
March 2026 post, after a redesign of the trace-recording frontend, the JIT is at
**+11-12% on macOS AArch64 and +5-6% on x86-64 Linux** against the tail-calling
interpreter. It is still not default-on.

**The cautionary tale that belongs in any document full of benchmark numbers.**
CPython 3.14's tail-call interpreter was announced at roughly 10-15%. Nelson
Elhage spent about a month establishing that the gain came mostly from
**accidentally working around an unfixed regression in LLVM 19**. Measured
against GCC, clang-18, or LLVM 19 with the right flags, the real benefit is
**1-5%**. *Interpreter performance claims are routinely off by 3× because of
compiler and measurement artefacts.* This study's own numbers should be read
with that in mind, which is why §1 states the machine, the compiler versions and
the method.

**The programme, cumulatively.** Mark Shannon's plan was ~5× over four releases,
with Microsoft funding a roughly six-engineer team including Guido van Rossum.
Achieved: ~25% (3.11), ~5% (3.12), "modest" (3.13), ~3-5% (3.14) —
**compounding to roughly 1.4-1.5× against a 5× goal.** In May 2025 Microsoft
cancelled the funding and most of the team was laid off; continuation is a
volunteer working group meeting fortnightly.

**The lesson, twice over.** The cheap technique — specialisation, no machine
code — delivered five times what the expensive one did. And a corporate-funded
team of the world's foremost experts, on the world's most-used dynamic language,
got ~1.45× in four years and then lost its funding. That should calibrate any
estimate of what a small team gets from a from-scratch native compiler.

### 5.9 Ruby YJIT — the gap between the benchmark and production

Lazy basic-block versioning, written in Rust, deployed by Shopify at 75M+
requests/minute. The numbers:

| | speedup |
|---|---|
| railsbench (synthetic) | +38% over the 3.2 interpreter, +57% cumulative vs 3.1.3 |
| Liquid rendering (synthetic) | +39% |
| **Shopify Storefront Renderer, real production traffic** | **+5-15% end-to-end request time** |

**That gap — 38-57% on the benchmark, 5-15% in production — is the number this
study most needs.** A funded team of five to seven engineers, a production JIT
for a mainstream dynamic language, deployed at enormous scale, and the
real-workload win is in the teens of percent, because real workloads are not
benchmarks.

### 5.10 The counter-examples: Starlark and Wren

**Starlark** is the Python-like language that deliberately refused to get fast by
compiling, and got fast by *removing capability*: **no recursion** ("it is a
dynamic error for a function to call itself"), all loops over finite sequences,
**not Turing-complete by design**, hermetic and deterministic with no I/O, and
**freezing** — "immediately after execution of a Starlark module, all values in
its top-level environment are frozen", so the module can be shared across
threads without locks. Neither Bazel nor Buck2 ever compiled it to native code.
Bazel got its speed from caching an evaluation DAG; Buck2 moved the graph and
execution engine into Rust and left Starlark for declarative rule logic, then
shipped builds 2× faster than its predecessor.

Oro has already taken half of this medicine — no `eval`, closed classes, a
closed dunder set. The Starlark lesson is that the other half, *removing
capability rather than adding a compiler*, is a legitimate and cheap answer.

**Wren** is the calibration point for "how fast is a small, well-written
bytecode VM already". **Under 4,000 semicolons of C**, NaN tagging, one
allocation per instance, copy-down inheritance so dispatch is "adding a few
pointers", computed gotos. In 2015 it beat CPython comfortably; a 2023 re-run
shows CPython 3.11 having closed most of the gap and **winning on binary-trees**.

And Bob Nystrom's own optimisation data, from *Crafting Interpreters*, is the
single most on-point finding in this section for Oro: replacing a modulo with a
bitmask **in the hash table** took his VM from 3,192 to 6,249 batches per ten
seconds — **~2×** — and dropped `tableGet()` from 72% to 35% of execution time.
By contrast, **NaN-boxing the value representation was "roughly 10% faster
across the board"**, which he explicitly calls "not a huge improvement".

**Data-structure work beat value-representation work by a factor of twenty.**
Oro's attribute access is a SipHash `std::collections::HashMap` probe plus an
MRO walk plus an allocation, with no cache anywhere (§3.2). That is the
hash-table finding, sitting unclaimed.

### 5.11 What this means for a bytecode VM written in Rust specifically

Three findings that appear in none of the case studies and bear directly on
Oro's implementation.

**Rust has neither computed gotos nor guaranteed tail calls on stable**, so the
dispatch loop is a `match`, which lowers to one indirect branch per instruction
rather than one per handler. **How much that costs is less than folklore
suggests**: measurements across five CPUs found that on modern Intel (Haswell
and later) the dispatch method is "not a significant performance
differentiator", though older ARM and AMD parts showed up to 20%.

**Tail-call dispatch, where available, is worth much more.** A nightly-Rust
tail-call interpreter using the unstable `become` keyword measured, against the
same `match`-based VM:

| | ARM64 fib | ARM64 mandel | x86-64 fib | x86-64 mandel |
|---|---|---|---|---|
| match-based VM | 2.41 ms | 125 ms | 4.70 ms | 264 ms |
| **tail-call** | **1.19 ms** | **76 ms** | **3.23 ms** | **175 ms** |
| hand-written assembly | 1.32 ms | 87 ms | 1.84 ms | 168 ms |

**1.5-2× over match-based dispatch, in safe Rust, and it beat hand-written
assembly on ARM64.** `become` is unstable, has a Rust project goal with most
work planned for 2026 and **stabilisation targeted 2027**. For Oro this is a
1.5-2× dispatch win, requiring no language change, no dependency, and no binary
growth — arriving on its own schedule. It belongs on the roadmap in §6.5 with a
date next to it.

**And writing the VM in Rust buys safety, not speed.** RustPython is *slower*
than CPython. Every operation still pays type lookup, attribute resolution and
method dispatch regardless of the host language — which is the same point §1.2
made from the other end when Rust and Go tied at 0.95 and 1.06 µs.

### 5.12 The table

| system | technique | measured on real code | cost | what it forced |
|---|---|---|---|---|
| **Crystal** | whole-program union inference + LLVM AOT | ~10× Ruby median; **~1.5× with a DB query**; **1.0× on sockets** | 2011→1.0 2021; $2.5M; 450 contributors; no incremental builds after 15 yrs | no `eval`/`send`/`define_method`; ivar annotations; conservative GC; 25 s-7 min builds |
| **Codon** | localized HM inference + LLVM AOT | **5-10× on real genomics apps** (10-100× micro) | MIT research group | no heterogeneous collections, no bignums, no reflection, no metaclasses |
| Nim | static types + C backend | 1.4-11.6× Python (micro only) | 2005→1.0 2019, ~1 BDFL | be statically typed; need a C compiler |
| Julia | JIT + type specialisation | ~C on type-stable code; **1.2× where Python calls BLAS** | 10+ yrs, large team | type stability; 173 MB sysimage; static path forbids dynamic dispatch |
| **PyPy** | meta-tracing JIT, unmodified language | **3-4.3× CPython** | 23 yrs, ~690k LOC, ~€2M | warmup (rules out short processes); moving GC; C-extension breakage; NumPy dropped it |
| Cython (unannotated) | Python → C, dynamic ops kept | **1.35×** | mature | a C toolchain |
| mypyc | annotations → C, restricted classes | **4× on mypy** (already fully annotated); **1.5-2× on Black after 2 yrs** | years, small team | no monkey-patching; single inheritance; genexprs silently become lists |
| LuaJIT | trace JIT + asm interpreter + NaN boxing | ~25× Lua, ~3× slower than C | ~10 yrs, ~1 person | froze after he left; GC never finished; unmaintainable |
| CPython PEP 659 | specialising interpreter | **~1.25×** | funded team, 1 release | **nothing** |
| CPython copy-and-patch | stencil JIT | **~0% at launch; +5-12% in 2026** | funded team, 3 yrs | nothing (LLVM at build time only) |
| Ruby YJIT | basic-block versioning | **+5-15% in production** (+38-57% on benchmarks) | 5-7 engineers, years | nothing |
| Starlark | **remove capability instead** | n/a — never compiled | small | no recursion; frozen modules; not Turing-complete |

Read the last two columns together. **The techniques that changed the language
bought multiples. The techniques that left the language alone bought tens of
percent. And the one technique that bought a multiple without changing the
language — PyPy's — took twenty-three years, 690,000 lines, and reached 3-4.3×.**

---
## 6. The options

Each option below carries four costs, and the fourth is the one that decides
this: expected speedup, engineering cost, effect on the user-facing language,
and effect on the identity — *one small static binary, no dependencies,
learnable in an afternoon, frozen and finishable*. That last is not decoration.
A 2.77 MB single file with a frozen surface **is** the product.

For scale throughout: the entire Oro implementation is **18,986 lines of Rust**
excluding tests. `libLLVM.so` on this machine is **118 MB** (system LLVM 18)
and **199 MB** (the one rustc ships). The `oro` binary is **2.77 MB**.

### 6.1 AOT-compile the dynamic semantics as-is

**Speedup: 2× (range 1.5-3×).** Derived in §4.

**Engineering.** The code generator is the smaller half. The larger half is
everything the bytecode currently gets for free:

- **Closing the world.** `import` must become a top-level-only, statically
  resolved construct, or the binary must ship a code generator to compile
  modules found at run time. There is no third option.
- **Call binding.** Module-level `def` names are rebindable and the rebinding is
  visible to compiled callers (§3.1). Either every call site gets a guard, or
  the language forbids rebinding a `def` name.
- **Lowering exceptions.** The block stack, the five job stacks truncated at
  unwind, `break`/`continue` through `finally`, all without Rust's unwinder
  (the crate is `panic = "abort"`).
- **Lowering suspension.** Six park sites, plus every `import`, plus every
  generator, each needing a resumable state. Either keep an explicit frame —
  giving back much of the win — or CPS-transform every function that can
  transitively reach a park, which is the single hardest pass in the project.
- **A second backend, forever.** x86-64 and aarch64 are both shipped targets.

**Estimate: 1.5-3 engineer-years for the first backend and a correct runtime,
then a permanent maintenance obligation roughly equal to the current VM's.**
That is 1-2× the entire existing implementation, for 2×.

**Identity: this is where it dies.** Crystal's arrangement — the *compiler*
links LLVM, the *produced binaries* do not, confirmed three ways in §5.1 — does
not transfer, because `oro` is one binary that is both, and `oro script.oro` is
the entire user interface. Compiling Oro means one of:

| approach | binary cost | other cost |
|---|---|---|
| link LLVM | +40-80 MB static, ~15-30× the product | slow compiles, a huge dependency |
| link Cranelift | +10-15 MB (**estimate**) | ~4-6× the product; a real dependency |
| emit C | +~0 | requires a C toolchain on the user's machine — "no dependencies" gone |
| hand-written template codegen | +0.5-1 MB | 1-3k lines of backend **per architecture**, forever |
| split into `oroc` + a small runtime | n/a | `oro script.oro` stops being a thing you type |

Only the last two keep the binary small, and both trade away something the
thesis names explicitly. **Verdict: the worst ratio on this list. No.**

### 6.2 Whole-program type inference, Crystal-style, with no annotations

**Speedup: 3-6× on server code, more on numeric (estimate).** Less than people
expect on the workload that matters, because §1.2 showed that workload is
allocation- and hash-bound: Rust and Go tie on it at 0.95 and 1.06 µs. Perfect
type information does not remove an allocation.

**What it runs into in Oro specifically.** Every one of these is a language
change, made to a language that is about to freeze:

- **Heterogeneous containers.** `[1, "a", null]` is legal and `dict` values are
  untyped. Inference yields a union, and a union is boxed, so the container
  path — which is most of a server — keeps its boxes.
- **`null` inhabits everything.** Every inferred type is `T?` unless proven
  otherwise. This is precisely Crystal's situation, and Crystal's answer is to
  make the *programmer* narrow it with nil-checks. That is a user-facing
  obligation, and it is the main thing people complain about.
- **Late-bound globals** (§3.1) defeat monomorphisation at every call site.
- **Runtime `import`** defeats whole-program analysis outright.
- **Identity semantics** (§3.1) forbid unboxing eighteen of twenty-six variants
  even when their type is known exactly. Knowing a value is a `Str` does not
  license copying it, because `is` can tell.

**Identity: whole-program inference means whole-program compilation on every
build.** `oro script.oro` today is a lexer, a parser, a two-pass compiler and
`go`. Under this model it is a global constraint solve. Crystal's compile times
are the single most-cited complaint about Crystal, and they are structural —
whole-program inference does not decompose into incremental units easily,
which is why Crystal still has no incremental compilation after a decade.
Oro is a language you run directly on a file. **Verdict: no. This is not
"compiling Oro", it is writing a different language and calling it Oro.**

### 6.3 Optional / gradual annotations on hot paths

**Speedup: 2-4× on annotated code (estimate, anchored on mypyc).**

There is one genuinely interesting fact here: **the syntax already exists and
already does nothing.** `def f(a: int) -> str` parses today and the annotation
is discarded (§2), so `f("not an int")` runs happily. Whatever else happens,
that should not survive to 1.0 — either the annotations mean something or they
should be rejected, because a type annotation that is silently false is worse
than no annotation.

**Engineering.** A type checker is not a small program. Beyond it: a
specialising code generator for annotated functions, a calling convention for
the typed/untyped boundary, and runtime checks at that boundary (or an unsound
system, which is its own decision). And the restrictions come as a package —
mypyc's compiled classes stop being monkey-patchable, which Oro's classes
already are not, but the *instances* are, and that would have to go too.

**Identity: this is the largest surface increase of any option on the list.**
A gradual type system is not a feature; it is a second language layered on the
first. "One way to do each thing" becomes two ways to write every function, and
because one of them is faster, in practice everyone writes the annotated one and
Oro has a type system it did not choose to have. "Learnable in an afternoon"
does not survive it. **Verdict: no, and specifically no to it as a performance
measure.** If Oro ever wants types it should want them for *correctness*, argue
that case on its own, and accept the speed as a side effect.

### 6.4 A tracing or specialising JIT

Three quite different things get called this, and they have wildly different
ratios.

**6.4a — A specialising, adaptive *interpreter* (CPython's PEP 659).** Not a
JIT at all: quickened bytecode that rewrites hot instructions into
type-specialised variants with a guard and a deoptimisation path. No machine
code, no new dependency, no binary bloat. CPython measured about **25%** from
it. For Oro the same technique applied to `BinAdd` (skipping the `__add__`
probe and the overflow path when both operands have been `Int`), `LoadAttr`
(caching an instance's field-map hit against a class identity) and `Call` would
plausibly do better than 25%, because Oro's unspecialised paths are worse than
CPython's were. **This is the single best idea in this section, it belongs in
§6.5, and it costs no language change at all.**

**6.4b — Copy-and-patch (CPython 3.13/3.14).** Pre-compiled machine-code
stencils, generated at *build* time by LLVM and patched at run time — so LLVM
is a build dependency, not a shipped one, and the runtime addition is a few
hundred KB of stencils per architecture. It is the only machine-code technique
whose binary cost is compatible with Oro's identity. And CPython's honest
measured gain from it is roughly **0-5%**, sometimes negative. It is a
foundation for future work rather than a win today. **Verdict: not now.**

**6.4c — A real tracing JIT (PyPy, LuaJIT).** This is where the large multiples
live, and the price is set out in §5. Two Oro-specific objections on top of the
generic ones. First, **a tracing JIT wants a tracing garbage collector** —
bump-allocation in a nursery is a large fraction of where the win comes from —
and Oro's deterministic-destruction promise (§3.3) forbids one. Second, a JIT
is megabytes of runtime, and a warmup cost that a request handler serving
thousands of requests would amortise but a script run once would not. Oro is
both of those programs. **Verdict: no.**

### 6.5 Stay interpreted, and keep optimising the VM

**Speedup: 1.5-2.5× on server code (estimate). Cost: weeks to a few months.
Language change: none. Binary growth: tens of KB.**

`bench/RESULTS.md` already contains most of the plan, ranked, with estimates —
and every item on it is aimed at exactly the operations §1.2 identified as
Oro's weak spot:

1. **`LoadMethod`/`CallMethod`.** `obj.m(x)` allocates an `Rc<BoundMethod>` at
   `LoadAttr` purely to hand a pair two instructions forward. Measured here: a
   method call costs **1.9× a plain call with the same body** (§1.6). Estimated
   10-15% on `oo` and `chain`; it should be worth more than that on HTTP
   parsing, which is almost entirely `b.find`, `b.strip`, `s.lower`, `d.get`.
2. **An inline cache for `LoadAttr` on instances.** Today there is *no* cache on
   attribute access anywhere, and an instance field read is a SipHash probe
   (74 ns) while a method access is a probe, a miss, an MRO walk and an
   allocation (124 ns). This is the largest single structural gap between Oro
   and CPython.
3. **Superinstructions**, with the post-codegen relocation pass they need.

To which this study adds four more, all found while measuring:

4. **Stop storing line and column on every instruction.** The fetch does two
   unconditional `u32` stores into the task on *every* instruction
   (`src/vm/mod.rs:728-761`). Step 5 of `RESULTS.md` concluded these were free —
   but it tested that by replacing them with something strictly worse (a
   dependent `Option<Rc<..>>` load and a pointer compare). The alternative worth
   measuring is a side table: `pc → (line, col)` resolved only when a
   diagnostic is actually built, with nothing written on the hot path at all.
5. **Skip the dunder probe on arithmetic when neither operand is an
   `Instance`.** `Op::BinAdd` calls `instance_method(&a, "__add__")` before
   reaching `arith::binary` (`src/vm/mod.rs:883-899`). A tag check first is
   free.
6. **`FxHash` for attribute maps.** Attribute names come from the code object,
   not from the network, so the DoS argument for SipHash does not apply to
   `Instance::fields`, `Class::members` or `Module::members`. This is typically
   worth 2-3× on the hash itself.
7. **`HKey::Str` clones the whole string to build a dict lookup key**
   (`src/value.rs`, noted as item 6 in `RESULTS.md`'s own list). `d["name"]`
   allocates on every lookup. `dictops` uses integer keys and never sees it;
   `std/http.oro` uses string keys everywhere.

Plus **6.4a**: specialisation for `BinAdd`, `LoadAttr` and `Call`.

And one item that arrives on someone else's schedule and should be planned for
rather than pursued:

8. **Tail-call dispatch, when Rust's `become` stabilises.** Rust has neither
   computed gotos nor guaranteed tail calls on stable, so Oro's dispatch is a
   `match` — one indirect branch per instruction rather than one per handler.
   Measured on nightly, a `become`-based tail-call interpreter is **1.5-2×
   faster than the equivalent `match`-based VM, in safe Rust, and beat
   hand-written assembly on ARM64** (§5.11). Stabilisation is targeted for 2027.
   No language change, no dependency, no binary growth. Worth designing the
   dispatch loop so that the switch is a mechanical change when it lands.

Two of these deserve emphasis because the external evidence points at them
unusually hard. **Item 2 is the Wren finding**: Bob Nystrom measured that
replacing a modulo with a bitmask *in his VM's hash table* was worth ~2× and
dropped `tableGet()` from 72% to 35% of execution time, while NaN-boxing the
value representation — the glamorous change — was "roughly 10% across the
board" (§5.10). Oro's attribute access is a SipHash `std::HashMap` probe plus an
MRO walk plus an allocation, with no cache anywhere. That is the same
unclaimed win, in the same place. **And item 6.4a is the CPython finding**:
PEP 659 delivered ~25% with no semantic change, which is five times what
CPython's actual JIT delivered.

The target to aim at is not a multiple, it is a name: **CPython.** On the
workload that matters Oro is 3.6× behind an interpreter that is not doing
anything Oro is forbidden from doing. Closing that is a well-understood
engineering programme with a published playbook, no language change, no
dependency, and no risk to the identity. **Verdict: do this.**

### 6.6 Restrict the language to make inference tractable

Homogeneous containers, non-nullable by default, no rebinding, static imports.

**Speedup: the largest available — 5-15× is reachable if the semantics are
Nim's or Crystal's rather than Python's.**

And it is a different language. Not "Oro with a stricter mode": every existing
Oro program that puts two types in a list stops compiling, `null` stops being
the answer to `d.get(missing)`, and — decisively — **the CPython differential
corpus stops working**, because CPython's answers are the answers of a language
with heterogeneous containers and a universal `None`. `corpus/core/` is Oro's
only independent check on its own correctness; §"The corpus" of the README is
clear that Oro's self-generated fixtures "can never catch Oro being *wrong* from
the start". Giving up the oracle to gain a multiple on a microbenchmark is the
worst trade in this document.

**Verdict: no, and it should be named for what it is.** If the owner wants a
statically typed compiled language with Python-ish syntax, that language exists
and is called Crystal or Nim, and neither took less than a decade.

### 6.7 The option that is not on the list, and should be first

**Move the remaining per-byte work into Rust, exactly as
`docs/stdlib-server-design.md` §5 already requires.**

The rule is already written: *anything that touches every byte goes in Rust;
anything that touches every request goes in Oro.* Three places violate it, and
they were found by measurement, not by reading:

| violation | measured cost | fix | expected |
|---|---|---|---|
| `std/json.oro` — a char-at-a-time parser and encoder in Oro | **2,547 µs to parse 1.7 KB; 1,589 µs to emit it** | JSON primitives in Rust, or the whole module | **60-115×** |
| `http._is_token` / `_is_field_value` / `_is_target` — per-byte validation in Oro | **~23 µs of a 157 µs request (41% of `read_request`)** | a generic byte-class validating primitive — the `bytes.scan` the design doc already names | **1.7× on `read_request`** |
| `http._http_date` recomputed per response | **5.8 µs/request** | cache at one-second granularity, as Go does | **~4% of the request** |

**Cost: days to weeks. Language change: none — a `bytes` primitive is a
`bytes` primitive. Binary growth: perhaps 100 KB. Identity: untouched.**

And this is not a workaround; it is what the fast Python web stacks actually
did. `orjson`, `pydantic-core` and Granian are Rust extensions. Nobody made
CPython fast; they moved the hot paths out of it. Oro has the same option and a
much better version of it, because Oro *owns* the Rust side and does not have to
ship an extension mechanism to use it.

The one caution: the design doc is right that the primitive must stay
**generic**. A `bytes.scan(class_table, limit)` earns its place on its own. An
`http.parse_request` does not, and freezing HTTP/1.1's edge cases into the
runtime is exactly the mistake the document forbids. JSON is the harder call —
it is a wire format, and a frozen one, which is the argument *for* putting it in
Rust; but it is also a whole module rather than a primitive. The honest middle
is a small set of scanning primitives (`scan`, a UTF-8-and-escape-aware string
scanner) with the structure and policy staying in Oro, and to accept that if
that lands JSON at 5× rather than 60×, 5× is still ten times what compiling
would give.

### 6.8 Finish the reactor

Not a compilation option, but §7 makes it the most valuable item on the whole
list, so it belongs here. Green threads landed (M3a); the mio reactor (M3b) has
not. Until it does, an Oro server is one connection at a time. The measured
industry evidence in §7 says the concurrency model outweighs the execution model
by roughly an order of magnitude for database-backed workloads.

---

## 7. The server question: does any of it matter?

The owner's goal is a good server, not a good microbenchmark. This section is
where the two diverge, and they diverge more sharply than the framing of the
question allows for.

### 7.1 The model

A handler that does a 5 ms database query — a perfectly ordinary indexed
lookup over a network — and returns a JSON object. Per-request CPU from §1's
measurements; the compiled rows are the estimates from §4 and §6.

| option | CPU/req | latency | language overhead | rps/core (CPU-bound) |
|---|---|---|---|---|
| Oro today, hello-world | 109 µs | 5.109 ms | **2.1%** | 9,200 |
| **Oro today, JSON API (1.7 KB out)** | **1,746 µs** | **6.746 ms** | **25.9%** | **570** |
| Oro, §6.7 done (json in Rust) | 158 µs | 5.158 ms | 3.1% | 6,300 |
| Oro, §6.7 + §6.5 done | 115 µs | 5.115 ms | 2.2% | 8,700 |
| Oro, + AOT compiled (§6.1, 2×) | 58 µs | 5.058 ms | 1.1% | 17,200 |
| Oro, + whole-program inference (§6.2, 3×) | 38 µs | 5.038 ms | 0.8% | 26,300 |
| Go `net/http` + `encoding/json` | 13 µs | 5.013 ms | 0.3% | 77,500 |

Read the "language overhead" column, and then read the row that is bold.

**Every compilation option in this study moves a number between 3.1% and 0.8%.**
The entire span of "compile Oro", from doing nothing to rewriting it as Crystal,
is **2.3 percentage points of request latency**. Two engineer-years buys 2%.

**`std/json.oro` moves the same number from 25.9% to 3.1%.** One module, days
of work, no language change, and it is worth ten times the largest compilation
option — because it is the only line in the table where Oro is doing something
genuinely, structurally wrong rather than merely doing the right thing slowly.

### 7.2 The evidence that this is the real shape of it, not a modelling artefact

Two independent runs of TechEmpower's own raw results (requests/sec; hardware
Xeon Gold 6330, 56 cores):

| framework | json | db (1 query) | query @20 |
|---|---:|---:|---:|
| Go fasthttp | 1,623,223 | 638,704 | **31,484** |
| Go gin | 343,891 | 202,229 | 16,229 |
| Rust actix-web (pg deadpool) | — | 103,907 | 3,924 |
| **Python FastAPI / uvicorn** | 340,983 | 156,872 | **36,274** |
| **Python Granian** | 985,986 | 234,825 | **72,405** |
| Python Django (sync WSGI) | 30,192 | 24,472 | 3,137 |

*Source: TechEmpower raw results JSON, run of 2025-09-25; the same shape appears
in the 2023-10-11 run underlying Round 22, so this is reproducible, not a
fluke.*

On the pure-JSON row, Go beats FastAPI by 4.8×. On one database query, 4.1×.
**At twenty sequential queries, FastAPI beats Go's fastest framework** —
36,274 against 31,484 — and Granian, a Rust HTTP server driving interpreted
Python handlers, more than doubles it.

That is not a claim that Python is faster than Go. It is the same arithmetic as
§7.1: once the request is dominated by waiting, the execution speed of the
handler stops being the variable, and what is left is how well the runtime
overlaps the waiting.

Which is exactly what the Django row shows. Django is 10-25× behind at *every*
query count, and it is the same CPython that runs FastAPI. The difference is
sync-blocking workers against async I/O. **The concurrency model outweighs the
execution model by about an order of magnitude on database-backed work.**

And it is corroborated by a study whose entire purpose was to show the
opposite. The ZHAW evaluation of Crystal against Ruby, C and Go (§5.1) found
Ruby 10× slower at the median and 26-211× slower on the arithmetic benchmarks —
and on its **TCP socket** benchmark measured C at 0.892, Go at 0.968, Crystal at
1.000 and **Ruby at 0.999**. The authors' own conclusion: *"the performance of
TCP sockets were almost the same for all languages, even Ruby. This shows that
when using sockets the bottleneck is the operating system itself."*

The same collapse at the web level: Kemal (compiled Crystal) against Sinatra
(interpreted Ruby) is cited at roughly 20× on hello-world and **about 1.5× once
a PostgreSQL query with a join is in the loop.** A compiled language, against an
interpreted one, with a database in the request: 1.5×.

Corroborated in production, in the direction that matters: Uber's rewrite of
Schemaless from Python (Flask/uWSGI) to Go reported ~85% lower median latency
and >85% lower CPU — and attributed it to goroutines replacing uWSGI's
one-thread-per-process model, not to Go executing faster.

Oro has already made the right choice here — green threads, `spawn`, channels,
a cooperative scheduler — and it has not finished it. **`M3b`, the mio reactor,
is worth more to a real server than every option in §6 combined**, and it is
already on the plan.

### 7.3 Where the two goals genuinely do diverge

None of the above says throughput is free. It says latency is insensitive and
throughput is not. Read the last column of §7.1 again:

- Oro today, realistic handler: **570 req/s per core.** That is bad, and it is
  `json`.
- Oro with §6.7 and §6.5 done: **~8,700 req/s per core.**
- Go: **~77,500 req/s per core**, and Go gets twelve cores where Oro gets one,
  because there is no parallelism inside one Oro VM by design.

So the per-machine gap is roughly **9× from the language and another
~6-12× from the threading model** — the second factor being softer than a naive
multiplication by twelve, since real servers are syscall-bound long before they
are core-bound. Call it two orders of magnitude, and note that only the first
order is a compiler's business. Scaling Oro means processes, and processes mean
no shared `dict` cache, which is a real cost the README's "sharing them is free"
claim does not price.

**That is the honest divergence.** For a service where the database is the
bottleneck — which is most services — Oro after §6.5 and §6.7 is fine, and
compiling changes 2.2% into 1.1%. For a service that is CPU-bound in the
handler, or that must serve six-figure requests per second from one box, Oro is
not the tool and no version of this study makes it one.

---
## 8. Recommendation

**Do not compile Oro. Not now, and — on the evidence here — not later either.**

The case is not that it is impossible. §2 shows Oro is unusually well set up for
it: no `eval`, no attribute hooks, closed classes, single inheritance, a
seventeen-name dunder set, static slot resolution already done. Very few dynamic
languages arrive at this question in that good a shape, and the owner's instinct
that Oro is compilable is correct.

The case is that it is aimed at the wrong number. Four findings, in the order
they should change the plan:

**1. The stdlib is losing more performance than the interpreter is.**
`std/json.oro` costs 2.5 ms to parse 1.7 KB — 115× CPython, which has a C
accelerator. It is a character-at-a-time parser written in Oro, with its cursor
in an instance attribute, which measurement shows is itself a 2× penalty over a
local. `http`'s per-byte validators are 41% of `read_request`. `docs/stdlib-
server-design.md` §5 wrote the rule these break and wrote the falsification test
they fail. **Fix this first. It is days of work and it is worth more than every
compilation option in this document combined.**

**2. Oro is 3.6× behind CPython on the workload that matters, and CPython is
still interpreting.** The 1.9× geometric mean does not describe server code;
server code is calls and attribute access, which is Oro's worst quadrant. There
is no attribute inline cache anywhere in the system, `obj.m(x)` allocates a
bound method it immediately destructures, every instruction writes a line and a
column, and every `+` probes for `__add__` first. `bench/RESULTS.md` already
ranks the first three fixes. **This is the second thing to do, it needs no
language change, and it should be aimed at beating CPython rather than at a
multiple.**

**3. Compiling as-is buys 2×, and the literature is unusually unanimous about
why.** Dispatch — the thing compilation removes — is **14.2%** of CPython's
measured runtime. Cython, compiling unannotated Python, reports **1.35×**.
OCaml's native compiler beats OCaml's own bytecode interpreter by 6× on average,
and OCaml is statically typed. mypyc gets 4× on a codebase that was already
100% annotated, and 1.5-2× on Black **after more than two years of integration
work**. Ruby's production JIT buys 5-15% on real Rails traffic against 38-57%
on its own benchmarks. PyPy, twenty-three years and 690,000 lines in, is at
3-4.3× — and NumPy dropped support for it in February 2026.

The large multiples in this field come from **type specialisation**, and type
specialisation requires deleting dynamism. Codon — an MIT compiler for a
Python-like language, the closest thing to this proposal that exists — reaches
5-10× on real applications by forbidding heterogeneous collections, runtime
reflection, metaclasses and arbitrary-precision integers. Oro has two of those
four locked into its architecture.

**4. For an I/O-bound server, none of it is the variable.** With a 5 ms query,
Oro's hello-world overhead is 2.1% of latency and compiling takes it to 1.1%.
TechEmpower's own raw data shows FastAPI *beating* Go's fastest framework at
twenty queries per request, and Django — the same CPython — losing by 10-25×
at every query count because it blocks instead of overlapping. **The reactor is
worth an order of magnitude more than the compiler.**

### The order of work

1. **`std/json.oro` → Rust scanning primitives.** Expect 5-60× on JSON,
   depending on how much stays in Oro. Days to two weeks.
2. **A generic byte-class validating primitive** (`bytes.scan`, per §5 of
   `stdlib-server-design.md`), and cache the HTTP date. ~1.25× on the whole request. Days.
3. **M3b, the reactor.** The largest throughput item on any list here.
4. **The `RESULTS.md` optimisation list, plus §6.5's four additions and
   PEP 659-style specialisation for `BinAdd`/`LoadAttr`/`Call`.** Target: beat
   CPython on `bench/progs/oo` and on a head-parse benchmark that should be
   added to the suite. 1.5-2.5×, weeks to months.
5. **Plan the dispatch loop for `become`.** Rust's guaranteed tail calls are
   targeted to stabilise in 2027 and are worth a measured 1.5-2× on dispatch in
   safe Rust (§5.11). Nothing to do now except not paint the loop into a corner.
6. **Then re-measure, and re-ask this question.** If after all of the above Oro
   is still 5× behind CPython on server code, something structural is wrong and
   compilation deserves a second hearing. If it is at or ahead of CPython, the
   question is answered.

### Which goal has to give, and by how much

The brief asks this directly, so: **the throughput goal has to give, and it has
to give by about 20×.**

*Go-class throughput* means ~77,000 req/s per core on a realistic handler and
twelve cores on this machine. Oro's realistic ceiling, after everything in this
document that does not damage the identity, is **~8,700 req/s on one core** —
about 9× behind per core and about 100× behind per machine, the second factor
being the deliberate no-parallelism-in-one-VM decision rather than the
interpreter.

Closing the 9× would take, at minimum, whole-program type inference with
non-nullable types and homogeneous containers, an LLVM or Cranelift backend, and
a garbage collector to replace the deterministic destruction the README
promises. That is Crystal, and Crystal's bill is on the record: **June 2011 to
1.0 in March 2021, production multithreading in July 2026 — fifteen years — with
about $2.5M of declared sponsorship, thirteen core maintainers, 450
contributors, and still no incremental compilation.** It makes `oro script.oro`
a compile step measured in tens of seconds. It breaks the CPython oracle that is
Oro's only independent correctness check. And — on the evidence of §1.2, where
Rust and Go tie because the workload is allocation-bound, and of §5.1, where
Crystal's own advocates measured it at parity with Ruby on TCP sockets and 1.5×
on a database-backed web request — **it would still not reach Go on the
benchmark that motivated the question.**

*One small static binary, no dependencies, learnable in an afternoon, frozen and
finishable* is achievable, is nearly achieved, and is the more distinctive of
the two goals. Nobody needs another fast language. A finished one is rarer.

The deciding question, put plainly, is: **is the target "fast enough that the
database is the bottleneck", or "fast enough to win a benchmark"?** For the
first, Oro is two ordinary work-items away. For the second, it cannot get there
as itself, and it should stop trying.

---

## 9. What could not be settled without building something

Five things. Each has a specific experiment attached, because "uncertain" is
only useful with a next step.

**1. Whether AOT compiling Oro really buys 2×.** The figure in §4 is a
decomposition estimate, cross-checked against external precedent — it is not a
measurement of Oro. **The experiment that settles it is small:** hand-translate
one function — `_HeadParser.headers`, or `_is_field_value` — into Rust that
manipulates the same `Value`s through the same runtime helpers (`get_attr`,
`Value::clone`, the `OroDict` API), with no type specialisation and no
shortcuts, and time it against the interpreted original. That is a day or two of
work and it is the honest ceiling of §6.1. **If it comes in under 3×, the case
for compiling is closed on measurement rather than on argument.** If it comes in
at 5× or more, this document's central estimate is wrong and §6.1 deserves a
second look.

**2. How much refcount traffic escape analysis can actually remove**, given that
identity is observable for eighteen of twenty-six variants (§3.1). The 30-50%
figure in §3.2 is a guess. It matters because it is the one place where
compilation has a large, uniquely-compiler-shaped win available.

**3. How much of the 3.6× gap to CPython is recoverable.** §6.5 estimates
1.5-2.5×. The items are individually plausible and have been individually
estimated, but `RESULTS.md` is itself a record of confident estimates that
measured as noise (steps 5, 6 and 12 were all reverted). The programme should be
run the way that file already runs it — one change, best-of-N, keep the
regressions in the log.

**4. Whether the six park sites can be lowered without a full CPS transform.**
This is the difference between §6.1 costing 1.5 engineer-years and costing 3.
It cannot be answered by reading; it needs a sketch of the transform against
`serve_conn`, which parks on a channel inside a `try` inside a loop.

**5. Cranelift's real cost to the binary.** The 10-15 MB in §6.1 is an estimate
from wasmtime's shape, not a measurement. If it is closer to 5 MB the
hand-written-backend row in that table changes character. It is one afternoon
with `cargo bloat` to find out, and worth doing before anyone argues §6.1 again
from first principles.

Two things are stated in this document with less confidence than the rest and
should be read that way: the estimated CPU costs in §7.1's compiled rows (they
propagate §4's estimate), and the claim that a `bytes.scan` primitive gets
`read_request` to ~34 µs (it assumes the validators drop to near-zero, which
they will not quite).

One thing this study did *not* examine, and which the next one should: **memory**.
A 16-byte `Value` with an `Rc` per string, no interning, no small-string
optimisation, and no cycle collector is a very different memory profile from
Go's, and per-connection memory is what actually caps a server's concurrency.
Nothing here measured it.

### And a note on trusting any of these numbers

CPython 3.14's tail-call interpreter was announced at roughly 10-15%. Nelson
Elhage spent about a month establishing that most of the gain came from
**accidentally working around an unfixed regression in LLVM 19**; measured
against GCC, clang-18, or LLVM 19 with the right flags, the real benefit is
1-5%. Published interpreter-performance claims are routinely off by 3× because
of compiler and measurement artefacts.

That is why §1 states the machine, the compiler versions, the iteration counts
and the best-of-N method, and why every figure in it was re-measured here rather
than quoted. It is also why the external figures in §5 are given with their
sources and, where sources disagree, as ranges. Three things in §5 are
explicitly *not* established in the public record and should not be repeated as
if they were: **V8's team size and total person-years** (no credible source
exists), **LuaJIT's person-years and hand-written-assembly line count** (only a
conference slide deck), and **any industry statistic for what fraction of a web
request is spent in the database** (APM vendors ship the UI and publish no
aggregate). The last of those is why §7 models a 5 ms query explicitly rather
than citing a percentage.

---

## Sources for §5

Primary and near-primary, in the order the sections use them.

- Ismail & Suh, *Quantitative Overhead Analysis for Python*, IISWC 2018 —
  the 14.2%-dispatch decomposition.
- Ertl & Gregg, *The Structure and Performance of Efficient Interpreters*,
  JILP 5 (2003) — the OCaml native-vs-bytecode bound and the branch-prediction
  results.
- Crystal: `crystal-lang/crystal` issues #1824, #2390, #4864, #5653, #10568,
  #11387, #11805; PR #14748; `crystal-lang.org` static-linking guide, team and
  sponsors pages; the 1.0 announcement (March 2021) and the execution-contexts
  post (July 2026); kojix2's compile profile; Ganz & Spielberger, *Performance
  evaluation of Crystal*, ZHAW 2021.
- Codon: Shajii et al., *Codon: A Compiler for High-Performance Pythonic
  Applications and DSLs*, CC 2023.
- Julia: `JuliaLang/Microbenchmarks`; the 1.9 release-highlights post (TTFX
  table); LWN's coverage of `juliac`.
- PyPy: `pypy.org` front page and performance page; `doc.pypy.org` FAQ and GC
  documentation; the June 2026 benchmarking post; the 2018 `cpyext` post;
  Barrett, Bolz-Tereick, Killick, Mount & Tratt, *Virtual Machine Warmup Blows
  Hot and Cold*, OOPSLA 2017; `numpy/numpy` issue #30416 and PR #30764.
- Cython: the project's own `cythonize` quickstart and pure-mode documentation.
- mypyc: *Mypy 0.700 Released: Up To 4x Faster* (April 2019); mypyc's
  `differences_from_python` and `native_classes` documentation; the Black
  integration write-up.
- LuaJIT: `luajit.org` status and DynASM pages; `LuaJIT/LuaJIT` issues #37, #38,
  #563; Tarantool's NYI list; OpenResty's GC64 post.
- CPython: *What's New* for 3.11, 3.13 and 3.14; PEP 659; PEP 744; Xu &
  Kjolstad, *Copy-and-Patch Compilation*, OOPSLA 2021; `blog.python.org`,
  *Python 3.15's JIT is now back on track* (March 2026); Nelson Elhage on the
  tail-call interpreter; LWN and Discourse coverage of the May 2025 disbanding.
- YJIT: Shopify Engineering, *Ruby YJIT is production ready*; Rails at Scale on
  3.3's production numbers.
- Starlark: the Starlark specification; Bazel's Skyframe documentation; Buck2's
  value-representation documentation.
- Wren: `wren.io/performance`; Muxup's 2023 re-run; Nystrom, *Crafting
  Interpreters*, optimisation chapter.
- Rust dispatch: pliniker's VM-dispatch measurements across five CPUs; Matt
  Keeter's `become` tail-call interpreter measurements; the Trifecta Tech
  Foundation project goal for guaranteed tail calls.
- §7's throughput table: TechEmpower's own raw results JSON, runs of 2023-10-11
  and 2025-09-25.
