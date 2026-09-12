# Survey: optional parameters on the VM / native-module surface

Tree: `oro` @ `feat/sane-defaults` `9019829`, release build. Every "accepted today" cell and every
behaviour note below was confirmed by running `./target/release/oro` on a probe (probes and raw
output: `cases/`, `cases{1,2,3,4}.oro.txt`, `run_cases.py`). CPython column: `python3` 3.12.10
(`cpython_checks.py`).

**The rule.** A parameter with no default is positional-only. A parameter with a default is keyword-only.

**Direction from the user.** Migration size is never a reason to prefer a recommendation. Counts are given for planning only.

**Counting.** The counts come from `count_sites.py`, with the raw data in `sites.json`. It scans `.oro` files, skipping Oro
comments and plain string literals but keeping f-string contents. It also scans the string literals of Rust tests: `src/**/tests.rs`
and `tests/*.rs`. The inline `#[cfg(test)]` modules in `fmt.rs`, `format.rs`, `bigint.rs`,
`net/reuseport.rs` and `sched.rs` embed no Oro calls. I read every site by hand and made three corrections:
- I removed the `str`/`bytes.count(sub)` sites from `xs.count`. They belong to the sibling's rows.
- I removed an f-string prose hit at `std/io.oro:83`.
- I removed an assert message at `tests/programs_run.rs:158`.

Dir keys: `std`, `ex`=examples, `core`/`div`=corpus/core, corpus/divergence (corpus/known-failing has
no hits), `tests`=tests/programs/*.oro, `bench`, `rs-src`=src/**/tests.rs, `rs-t`=tests/*.rs.
A dir that isn't listed has 0.

## Table (22 rows)

| callable | param | omitted means | accepted today | calls pos / named / omitted (by dir) | CPython kw? | recommendation | reason |
|---|---|---|---|---|---|---|---|
| `xs.any(p)` (vm `do_seq_op`) | `p` predicate | truthiness of each element | positional only; `p=` → TypeError; `null` → TypeError | pos 6 (div 5, rs-src 1) / named 0 / omitted 5 (div 5) | n/a: `any(iterable)` has no predicate and takes no kw | **needs decision**: required (lean) or split | no-arg vs predicate are two operations (criterion 3); no `std`/`ex` use of either form |
| `xs.all(p)` | `p` predicate | truthiness | positional only | pos 4 (div 3, rs-src 1) / 0 / omitted 5 (div 5) | n/a (as `any`) | **needs decision**: same as `any` | must move with `any` |
| `xs.count(p)` | `p` predicate | count of truthy elements | positional only; `count(1)` → TypeError "needs a function" | pos 8 (div 4, rs-src 4) / 0 / omitted 1 (div 1) | n/a: `list.count(x)` counts equal values, a different operation | **needs decision**: required (lean) or split | `xs.count()` reads as a length and is not one: `[0,1,2,""].count()` is 2 |
| `xs.enumerate(start)` (native seq method, listed in my scope) | `start` | 0 | positional only; `start=` → TypeError; `null` → 0 | pos 4 (div 4, one of them the `enumerate(1, 9)` error probe) / 0 / omitted 12 (div 10, rs-src 2) | **yes** (`enumerate(xs, start=1)`) | **named** `start=` | an offset/tuning number; `xs.enumerate(1)` does not say what the 1 is |
| `xs.sum(start)` (native seq method; **shared with builtins sibling**) | `start` | 0 | positional only; `start=` → TypeError | pos 1 (div 1) / 0 / omitted 23 (ex 3, div 16, tests 2, rs-src 2) | **yes** (`sum(xs, start=10)`) | **named** `start=` | a seed that tunes the fold, not the subject; matches CPython |
| `xs.sorted(key=)` (vm) | `key` | natural order | keyword only; positional → TypeError; `key=null` ok | pos 1 (div 1, `sorted(1)` error probe) / named 12 (div 11, rs-src 1) / omitted 64 (std 1, ex 2, div 54, bench 1, rs-src 6) | **yes**, keyword-only | **named** (no change) | already keyword-only. See decision **D3**: `xs.sort_by(f)` gives identical results |
| `xs.sorted(reverse=)` | `reverse` | ascending | keyword only | 0 / named 13 (div 10, rs-src 3) / omitted 63 | **yes**, keyword-only | **named** (no change) | a flag. It is a *stable* descending sort, which `.reversed()` is not: verified, the ties come out in a different order |
| `list.sort(key=)` (vm, in place) | `key` | natural order | keyword only; positional → TypeError | 0 / named 2 (div 2) / omitted 5 (core 1, div 4) | **yes**, keyword-only | **named** (no change) | as `sorted(key=)`; no in-place `sort_by` twin exists |
| `list.sort(reverse=)` | `reverse` | ascending | keyword only | 0 / named 3 (div 3) / omitted 4 (core 1, div 3) | **yes**, keyword-only | **named** (no change) | flag |
| `print(*args, sep=)` (vm `do_print`) | `sep` | `" "` | keyword only (`*args` absorbs positionals); `null` = default | 0 / named 4 (core 4) / omitted 1713 (ex 59, core 585, div 965, tests 40, bench 18, rs-src 3, rs-t 43) | **yes** | **named** (no change) | a mode; `*args` makes keyword the only possible spelling anyway |
| `print(*args, end=)` | `end` | `"\n"` | keyword only | 0 / named 5 (core 5) / omitted 1712 | **yes** | **named** (no change) | as `sep`. Hot path: `end=""` runs inside per-item loops, but it is already keyword today, so no regression |
| `chan(capacity)` (sched `do_chan`) | `capacity` | 0 = rendezvous | positional only; kw → TypeError; `null` → TypeError | pos 14 (div 6, rs-src 8), of which 6 buffer for real (2,3,1,2,1,4), 4 are explicit `chan(0)`, 4 are error probes / 0 / omitted 26 (ex 4, div 13, rs-src 8, rs-t 1) | n/a (analogue `queue.Queue(maxsize=8)` accepts kw) | **needs decision**: named `cap=` (lean), required, or split | rendezvous vs buffered is a real semantic pair (criterion 3); capacity is also a tuning number (criterion 2) |
| `m.group(n)` (Match method) | `n` group index | 0 (whole match) | positional only; kw → TypeError; `null` → TypeError | pos 11 (core 9, rs-src 2) / 0 / omitted 0 | **no**: "Match.group() takes no keyword arguments" | **required** | every call passes it; the number means itself; per-match hot path; `named` would push 2 core files to divergence |
| `m.start(n)` | `n` | 0 | positional only; kw → TypeError | pos 1 (core 1) / 0 / omitted 3 (core 2, rs-src 1) | **no** | **required** | same argument as `group`, and it keeps the three Match methods identical. `m.start(0)` is valid CPython |
| `m.end(n)` | `n` | 0 | positional only; kw → TypeError | pos 1 (core 1) / 0 / omitted 2 (core 1, rs-src 1) | **no** | **required** | as `start` |
| `sys.exit(code)` | `code` | 0 (also `null`) | positional only; `code=` → TypeError | pos 7 (ex 4, rs-t 3) / 0 / omitted 0 | **no**: "sys.exit() takes no keyword arguments" | **required** | every call passes it; the value is self-explanatory; CPython is positional-only |
| `net.listen(addr, reuseport=)` (modules `net_listen_kw`) | `reuseport` | false | keyword only; `listen(a, true)` → TypeError; non-bool → TypeError | 0 / named 4 (rs-t 4) / omitted 22 (std 1, div 6, rs-t 15) | n/a | **named** (no change) | a bare boolean flag; §4's chosen spelling |
| `proc.run(cmd, cwd=)` (vm `do_proc_run`) | `cwd` | inherit | keyword only | 0 / 0 / omitted 11 (div 6, rs-src 5) | **yes** (`subprocess.run(cwd=)`) | **named** (no change) | option |
| `proc.run(cmd, env=)` | `env` | inherit | keyword only | 0 / 0 / omitted 11 | **yes** | **named** (no change) | option |
| `proc.run(cmd, timeout=)` | `timeout` | none | keyword only | 0 / 0 / omitted 11 | **yes** | **named** (no change) | tuning number |
| `proc.run(cmd, check=)` | `check` | **true** (CPython's default is False) | keyword only | 0 / named 2 (div 1, rs-src 1) / omitted 9 | **yes** (default differs) | **named** (no change) | flag |
| `proc.run(cmd, quiet=)` | `quiet` | false (tee output live) | keyword only | 0 / named 9 (div 6, rs-src 3) / omitted 2 (rs-src 2) | **no** (Oro-only; CPython `Popen` rejects it) | **named** (no change) | flag |

Summary:
- 10 rows change spelling under the rule: `any`, `all`, `count`, `enumerate`, `sum`, `chan`, `group`, `start`, `end` and `sys.exit`. Each is a positional optional today.
- 12 rows are already keyword-only and need no change: `sorted`×2, `sort`×2, `print`×2, `net.listen`, and `proc.run`×5.

## Needs a decision

- **D1: `xs.any(p)` / `xs.all(p)` / `xs.count(p)`**. Decide the three together.
  - Usage:
    - The predicate is passed at 18 sites (any 6, all 4, count 8).
    - It is omitted at 11 (any 5, all 5, count 1).
    - Every omitted site is a protocol probe in `corpus/divergence` (33, 35, 65). None is in `std` or `examples`.
  - Option (a), *required*. Truthiness becomes `xs.any((x) => x)`, which I verified gives the same answers.
    - One operation, one spelling.
    - It removes the `count()` ≈ `len()` trap.
  - Option (b), *split*. Keep the nullary truthiness forms and give the predicate forms their own names, or the reverse: `any(p)` required plus `any_true()` / `all_true()` / `count_true()`.
    - Naming caveat: `_by` is already taken to mean "by key" (`sort_by`, `min_by`, `unique_by`, `group_by`). So `any_by(p)` would muddy that suffix.
  - Option (c), *named* `p=`, is rejected by criterion 2: the predicate is the operand, not a mode.
  - Lean: (a).
- **D2: `chan(capacity)`**. The rendezvous form is used at 26 sites. The buffered form really buffers at 6, and there are 4 explicit `chan(0)` plus 4 error probes.
  - (a) *named* `chan(cap=8)`. `chan()` stays rendezvous. `cap` is the word `repr` already prints (`<channel cap=0>`). Capacity is a tuning number, so criterion 2 points here.
  - (b) *required*. Every channel states its buffering (`chan(0)`); §3 already calls `chan(0)` "an explicit spelling of the default".
  - (c) *split*. `chan()` rendezvous plus a separately named buffered constructor; it treats the two as different synchronisation semantics.
  - Lean: (a). `chan(8)` does not say what 8 is, and `chan()` reads correctly for the common case.
- **D3: `xs.sorted(key=f)` vs `xs.sort_by(f)`**. This is not about an optional parameter, but it is a one-spelling conflict the rule exposes. I verified both give identical results on list, tuple and dict, including ties.
  - Options:
    - (a) Keep both. `key=` pairs with `reverse=` and with CPython-compatible `list.sort(key=)`.
    - (b) Reshape: key-sorting goes only through `sort_by(f)`, which would then need `reverse=` (it rejects it today). `sorted` keeps only `reverse=`. `list.sort(key=)` stays, because there is no in-place `sort_by`.
  - No feature is lost either way.
- **Close but not flagged.** For `m.start(n)` / `m.end(n)`, the argument is omitted 3 and 2 times out of 4 and 3 sites. So the "nearly every call passes it" test is weaker than it is for `group`. I still recommend *required*, for three reasons:
  - It keeps the three Match methods uniform.
  - `m.start(0)` is valid CPython, whereas `named` is rejected by CPython.
  - They sit on the same per-match path as `group`.

## Affected `corpus/core` files

- **Under the recommendations above, no `corpus/core` program has to move to `divergence/`.** Every new spelling is also valid CPython.
  - `corpus/core/27_re.oro` is the only file whose text changes, in 3 places: line 7 `m.start()`/`m.end()` and line 34 `mm.start()` become `(0)`. CPython accepts that.
  - `corpus/core/37_fstring_scope.oro` already writes `m.group(0)`, so it is unchanged.
- **If `m.group` / `m.start` / `m.end` went *named* instead**, both `27_re.oro` and `37_fstring_scope.oro` would have to move to `divergence/` with a `.twin.py`, because CPython rejects `group=`.
- Unchanged, and CPython-valid as keywords:
  - `34_print_sep_end.oro` and `46_exception_str.oro` use `sep=`/`end=`.
  - `08_lists.oro` has `.sort()`.
- Not mine: the `.count(...)` sites in `38_str_bytes_optional_args.oro` and `39_method_arity.oro` are `str`/`bytes.count(sub, start, end)`, the builtins sibling's rows.

## Hot paths

- **`m.group` / `m.start` / `m.end`**: called per match, typically once per input line. That is one more reason for *required* (positional) over *named*.
- **Stream methods, already positional and required** (no optional parameter): `read(n)`, `read_until(delim, limit)` and `write(b)` run per request; `set_timeout` runs per connection. Keep them positional.
- **`print(end=)`**: runs per line, but it is already keyword, so there is no change.
- **Collection callbacks**: `enumerate(start=)`, `sum(start=)`, `sorted(key=/reverse=)` and the `any`/`all`/`count` predicate each pay the keyword once per collection, not per element.

## Fixed-arity or variadic (no row; verified)

- **Unaffected variadics**:
  - `spawn(f, *args, **kwargs)`: kwargs are forwarded to `f`, and `spawn()` → TypeError.
  - `min(*args)` / `max(*args)`: 2 or more arguments, no kw. `min([1])` → TypeError pointing to `xs.min()`.
  - `print(*args)`.
  - `xs.zip(*others)`: `xs.zip()` → 1-tuples.
  - `os.path.join(*parts)`: `join()` → `''`, where CPython raises TypeError.
- **Collection methods, required positional**:
  - `reduce(init, f)`, `sort_by(f)`, `min_by(f)`, `max_by(f)`.
  - `take(n)`, `drop(n)`, `chunk(n)`. The count is effectively required, but omitting it or passing `null` gives a *ValueError* "needs a count >= 0" rather than an arity TypeError, because the implementation uses a sentinel default in `opt_int_arg`.
- **Collection methods, no arguments**: `first()`, `last()`, `xs.min()`, `xs.max()`, `xs.sorted()`.
- **Concurrency and time**:
  - `yield_now()`, `t.join()`, `ch.recv()`, `ch.close()` take no arguments; `ch.send(v)` takes one.
  - `time.sleep(secs)`, `time.time()`, `time.monotonic()`, `net.dial(addr)`.
- **Streams**:
  - `s.read(n)`: no nullary form. It says "use io.read(r)".
  - `s.read_until(delim, limit)`: the limit is deliberately required.
  - `s.write(b)`, `s.close()`, `s.accept()`, `s.bytes()`, `s.shutdown_write()`.
  - `s.set_timeout(secs|null)`: the argument is required, and `null` is a value meaning "no timeout", not a default.
  - `s.set_nodelay(bool)`.
- **`os`**: `os.getcwd()`, `os.listdir(path)` (CPython's `path` is optional), `os.remove`, `os.mkdir`, `os.path.exists`/`isfile`/`isdir`/`basename`/`dirname`/`splitext`.
- **Internal modules**: `_io.buffer(b)`, `_io.read_all(s)`, `_json.parse(s)`, `_json.stringify(v, indent)` and `_pct.encode`/`decode` cannot be imported from user code (ModuleNotFoundError). They are internal to `std/`.
- **Not defined**: `iter` and `next` → NameError. No other names are intercepted by string before the generic builtin path. The full intercept set in `Vm::invoke` / `invoke_native_method` is:
  - `spawn`, `chan`, `yield_now`, `time.sleep`, `net.dial`, `print`, `min`/`max`, `proc.run`, `net.listen`, `repr`, `len`.
  - Task and channel methods, and the stream I/O methods.
  - `to_str`, the SeqOps, `first`/`take` at the end of a chain, `sort`, `sorted`, and the method forms of `min`/`max`.

## Arity bugs found (the rule's enforcement will need these fixed)

- **`re.search` / `findall` / `finditer` / `fullmatch` / `split(pattern, s, ...)`** silently ignore extra positionals. `re.search("a", "xa", 99)` matches, and `re.findall("a", "aaa", 1, 2)` returns 3 matches.
- **`re.sub(p, r, s, 1)`** silently ignores CPython's `count`. It returns `'bbb'` where CPython returns `'baa'`: a silent wrong answer.
- **`re.compile("a", 2)`** ignores the flags argument.
- **Pattern methods** do the same. `p.search("xa", 5)` ignores `pos`, which in CPython would return None, and `p.sub("b", "aaa", 1)` ignores `count`.
- **`proc.run(["true"], "/tmp")`** accepts and ignores a second positional.

## Shared names (sibling scopes)

- **Builtins sibling**:
  - `str`/`bytes.count(sub, start, end)` vs `xs.count(p)`.
  - `xs.sum(start)` and `xs.enumerate(start)`, which live in `seq_native_method`.
  - `open(path, mode)`, whose `mode` is an optional positional.
  - The only native methods that take kwargs: `strip(side=)`, `split(side=)` and `find(reverse=)`.
- **std sibling**:
  - `io.read(r, n=null)` is the whole-stream counterpart of `s.read(n)`.
  - `io.buffer(b=b"")`.
  - `json.stringify(value, indent=null)` accepts `indent` positionally today (`json.stringify(x, 2)` works), while CPython's `json.dumps(x, 2)` is a TypeError.
