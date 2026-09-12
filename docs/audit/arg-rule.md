# The argument rule: a decision for every optional parameter

**The rule (approved 2026-09-11).** A parameter with no default can only be passed by position. A parameter with a default can only be passed by name. `*args`/`**kwargs` are unaffected, and a method's receiver is not an argument. The rule covers builtins, native modules and `std/`. No new syntax: the `=` in a signature is the only marker.

**Direction.** "Cost maybe high but consistent code is priceless." Call-site counts are given as information and were never used to choose.

Sources, each checked by running code at `9019829`, are in `arg-rule/`:
- `survey-native.md`: free builtins, and the `str`/`bytes`/number/collection methods.
- `survey-vm.md`: VM-dispatched callables, the modules, `spawn`/`chan`, and streams.
- `survey-std.md`: every `def` in `std/*.oro`, plus the call-site census.

---

## Settled: follows from the rule, no decision needed

### Becomes keyword-only (named)

| where | parameters |
|---|---|
| str/bytes `find`, `count`, `startswith`, `endswith` | `start=`, `end=` |
| str/bytes `replace` | `count=` |
| str/bytes `strip` | `chars=` |
| str/bytes `split` | `maxsplit=` (so `split(null, 1)` placeholders disappear) |
| `x.to_int` | `base=` |
| `xs.sum`, `xs.enumerate` | `start=` |
| `range` | `step=` |
| `d.get` | `default=`, and so `Request.header` / `Response.header(default=)` |
| `json.stringify` | `indent=` |
| `http.serve` | `ready=`, `max_conns=`, `max_requests=`, `timeout=`, `drain=` |
| `http.serve_conn` | `max_requests=`, `watch=` |
| `http.fetch` | `headers=`, `body=`, `timeout=`, `max_body=`, `params=` |
| `http.stream` | `headers=`, `body=`, `timeout=`, `params=` |
| `http.quote` / `quote_plus` | `safe=` |
| `http.unquote` | `plus=` |
| `http.text`, `json_response`, `BadRequest`, the parsers' `fail` | `status=` |
| `http.Response` | `headers=`, `body=`, `version=`, `reason=` |
| `http.write_request` | `body=` |

### Already conforming, no change
- `sorted` / `sort`: `key=`, `reverse=`
- `print`: `sep=`, `end=`
- `strip` / `split`: `side=`
- `find`: `reverse=`
- `net.listen`: `reuseport=`
- `proc.run`: `cwd=`, `env=`, `timeout=`, `check=`, `quiet=`

### Becomes required (positional, default dropped)
- **`m.group(n)`, `m.start(n)`, `m.end(n)`.** The number means itself, the call runs once per match, and CPython rejects `group=`.
- **`sys.exit(code)`.**
- **Private std hot-path helpers:**
  - `_percent_decode(…, fail, where)`
  - `_parse_query(…, fail, where)`
  - `_content_length(s, fail)`
  - `_check_header(name, value, kind)`

  Every caller passes these. Naming them would add about 7 µs to each request (about 10%). Keeping them required costs nothing.
- **`write_request(…, headers)`.** The default was dead: using it always raises "a request needs a Host header".
- **`read_response(r, method)`.** The default was harmful: a HEAD reply read with it misreads the next message as the body.

### Reshape
- **`_check_headers`:** drop `kind`, since both callers check response headers.

### Enforcement details
- **An explicit `null` no longer means "omitted".** It is rejected for keyword parameters whose default is not `null`, so `f(x=null)` can't become a second spelling of `f()`. Nine parameters accept that today. `null` stays a real value where it is the default: `d.get(default=)`, `Response(body=)`, `fetch(headers=)`.
- **Error messages that teach the old spellings get reworded.** These are the `rsplit` and `enumerate` removal messages.
- **Fixture fix:** `53_yield_now`'s four `spawn(counting, …, n=3)` calls go back to positional, because `n` has no default.

### Arity bugs fixed along the way
Every one of these silently ignores an argument. Each becomes a TypeError.
- **`re.sub(p, r, s, 1)` returns `'bbb'` where CPython gives `'baa'`.** The `re` functions and pattern methods ignore extra positionals: `pos`, `count`, `maxsplit`, `flags`.
- **`proc.run(cmd, "/tmp")`** ignores its second positional.
- **`take` / `drop` / `chunk`** ignore extra arguments. A missing count is a ValueError instead of a TypeError.
- **`to_int`** ignores extra arguments, and ignores `base` on int and float values.
- **`find(reverse=)`** accepts any truthy value; it will require a bool.

---

## Needs a decision

Each item lists the options, with my recommendation first.

**Collection protocol**
1. **`xs.any(p)` / `all(p)` / `count(p)`, where omitting `p` tests truthiness.**
   - **Required (recommended):** truthiness is spelled `xs.any(x => x)`. `[0, 1, 2, ""].count()` is 2, which reads like a length and isn't one.
   - **Split:** separate names for the truthiness forms.
2. **`xs.sorted(key=f)` vs `xs.sort_by(f)`, which give identical results.**
   - **Keep `sort_by(f, reverse=)` for keyed sorts (recommended).** The function is the operand, and the rule makes operands positional, like `min_by`, `max_by`, `group_by` and `unique_by`. `xs.sorted()` keeps only `reverse=`. `list.sort(key=)` stays, because it's in place and valid CPython.
   - **Keep `sorted(key=)`** and cut `sort_by`.
   - **Keep both:** two spellings.

**Builtins**

3. **`range`: `range(n)` treats its first argument as stop; `range(a, b)` treats it as start.**
   - **A (recommended):** always `range(0, n)`. The first argument always means start, and it's valid CPython.
   - **B:** keep `range(n)` and add a second name for the two-bound form.
   - **C:** `range(n, start=a)`. This reads stop-first, and CPython rejects it.
4. **`split`: `s.split()` (whitespace) and `s.split(",")` (separator) are different algorithms.**
   - **Split (recommended):** `s.split(sep)` with `sep` required, plus `s.fields(maxsplit=, side=)` for whitespace. Go's `strings.Fields` is the precedent. `split(null, 1)` becomes `fields(maxsplit=1)`.
   - **Named:** `s.split(sep=",")`.
   - **Keep `split(null)`** as the whitespace spelling.
5. **`d.pop(k, default)`: omitting the default means "raise KeyError", which no value can express.**
   - **Split (recommended):** `d.pop(k)` raises, and `d.pop_or(k, default)` does not.
   - **Named `default=`:** its absence silently changes behaviour.
6. **`round(x, ndigits)`: omitting it returns an int; `ndigits=0` returns a float.**
   - **Named `ndigits=` (recommended):** it's a precision (a tuning number), and CPython does exactly this.
   - **Split into two names.**
7. **`open(path, mode)`: passed at 19 of 22 sites.**
   - **Named `mode="w"` (recommended):** `"w"`/`"a"` are mode codes, like `side=`.
   - **Required:** always `open(p, "r")`.
8. **`xs.pop(index)` on lists: there are zero call sites.**
   - **Named `index=` (recommended):** then a positional argument to `.pop` always means a dict key.
   - **Split:** `pop()` and `pop_at(i)`.

**Concurrency and io**

9. **`chan(capacity)`.**
   - **Named `chan(cap=8)` (recommended):** capacity is a tuning number, and the repr already prints `cap=`.
   - **Required:** always `chan(0)`.
   - **Split:** a separate buffered constructor.
10. **`io.buffer(b=b"")`.**
    - **Required (recommended):** `io.buffer(b"")` spells the empty case.
    - **Named:** this would make the parameter name public, so `b` would have to be renamed.
11. **`io.read(r, n=null)`: reading to EOF and reading exactly n fail and use memory differently.**
    - **Split (recommended):** `io.read(r, n)` with `n` required, plus `io.read_all(r)`. This also keeps the per-chunk call positional.
    - **Named `n=`.**

**http**

12. **`Response(status, …)`.**
    - **Required (recommended):** `Response(404, body=…)`, since every construction states a status.
    - **Named:** `status=404`, to match `text(…, status=)`.
13. **`write_response(w, req, resp, keep_alive)` has a bare `true`/`false` at 13 sites.**
    - **Give it the default `false` (recommended),** so callers write `keep_alive=true`.
    - **Keep it positional.**
14. **`http.Request(method, path, query, version, headers, body)`: six positional arguments, three of which are always the same filler values in tests.**
    - **Give `version="1.1"`, `headers=null` and `body=null` defaults (recommended),** so they're passed by name.
    - **Leave it** as a parser-facing record.

**Semantics**

15. **Unpacking at the call site.** Does the rule also govern arguments that arrive through `**mapping` or `*seq`?
    - **Yes (recommended):** a required parameter can't be bound from `**`, and a defaulted one can't be fed from `*`. So `spawn(configure, **{"name": "q"})` becomes an error when `name` is required. The rule is about how an argument binds, not how it's written. Three fixture sites are affected.
    - **No:** unpacking is exempt.

---

## Decisions so far (user, 2026-09-11)

Where a decision differs from the recommendation above, this list wins.

1. `any` / `all` / `count` stay, and each always takes a predicate. Truthiness is `xs.any(x => x)`.
2. Keyed sorting is only `xs.sort_by(f, reverse=)`. `xs.sorted()` keeps only `reverse=`. *Open:* whether in-place `list.sort` stays at all.
3. `range(end, start=0, step=1)`. `range(5)` still works, and `start=` and `step=` are keyword-only.
4. `s.split()` is the whitespace algorithm, and `s.split(sep=",")` overrides it with a literal separator. No magic default string.
5. `d.pop(k)` raises, and `d.pop(k, default=v)` doesn't. This uses the same word as `d.get(k, default=)`.
6. `round(x, ndigits=n)`.
7. `open(p, mode="w")`.
8. `xs.pop(index=i)`.
9. `chan(cap=n)`.
10. `io.buffer(b"")`, with the argument required.
11. `io.read(r)` reads to EOF, and `io.read(r, exactly=n)` reads exactly n bytes or raises EOFError. *(Name pending confirmation. `bytes=` is impossible: it's a type keyword, and those can't be parameter names.)*
12. `Response(status, …)`, with status required.
13. `write_response(…, keep_alive=false)`.
14. `http.Request` gets defaults for `version`, `headers` and `body`.
15. **No `*` or `**` anywhere in Oro code**, neither in definitions nor at call sites. Instead there is an ordinary builtin, `apply(f, args, kwargs)`, where `args` is a list bound by position and `kwargs` a dict bound by name, so the rule applies to both. Builtins like `print`, `min`, `max`, `zip` and `spawn` stay variadic natively.

---

## Consequences, under the recommendations

- **`corpus/core` programs that move to `corpus/divergence/` with a `.twin.py`,** because CPython rejects the new spelling:
  - `38_str_bytes_optional_args`
  - `43_identity_equality` (`step=`)
  - `35_bytes` (`get(default=)`)
  - `45_dict_pop`
  - `30_str_split_maxsplit` (`fields`)

  `27_re` stays in core and gains three `(0)`s.
- **Hot path:** about +0.6 µs per HTTP request (0.8%), versus +7.7 µs if every default were named mechanically. A keyword argument costs about 0.4–0.6 µs per call, which would be worth optimizing in the binder afterwards.
- **Migration size (information only):**
  - Oro code: about 160 std-function arguments and about 250 builtin arguments change spelling.
  - Rust tests with embedded Oro: 13 sites.
  - README: 4. Tracked docs: 13.

---

## FINAL DECISIONS (approved 2026-09-11; these supersede every list above)

**The rule.**
- A parameter with no default is positional-only, and one with a default is keyword-only. This applies to Oro `def`s, builtins, native modules and `std/`.
- Passing an explicit `null` to a keyword parameter whose default isn't `null` is rejected, so `f(x=null)` can't become a second spelling of `f()`.

**Language core**
- **No `*` or `**` anywhere in Oro code**, neither in `def` parameters nor at call sites.
- **New builtin `apply(f, args=[], kwargs={})`.** The list binds by position, so it can only fill required parameters. The dict binds by name, so it can only fill defaulted ones.
- **Builtins that are variadic by nature stay variadic natively:** `print`, `min`, `max`, `xs.zip`, `spawn`, `os.path.join`.
- **Default values are evaluated at each call, not once at `def` time.** Today `def f(x=[])` shares one list across calls. Constant defaults, such as literal numbers, strings, booleans and `null`, may stay precomputed.

**Collections and sorting**
- **`xs.sorted()` is removed.** Sorting is `xs.sort_by(f, reverse=false)`, which returns a new collection, and natural order is `xs.sort_by(x => x)`.
- **In-place `list.sort(key=, reverse=)` is removed.** In its place is `xs.sort_in_place(f, reverse=false)`: lists only, it sorts the list's own storage without copying it first, and returns `null` like `append` and `extend`.
- **`xs.any(p)`, `xs.all(p)` and `xs.count(p)` always require a predicate.** Truthiness is `xs.any(x => x)`.
- **`xs.sum(start=0)` and `xs.enumerate(start=0)`.**
- **`take(n)`, `drop(n)` and `chunk(n)`:** the count is required, and extra arguments raise a TypeError.

**str / bytes**
- `find`, `count`, `startswith` and `endswith` take `start=` and `end=`.
- `replace` takes `count=`, and `strip` takes `chars=`.
- **`split`:** `s.split()` is the whitespace algorithm, which collapses runs and drops the ends. `s.split(sep=",")` splits on a literal separator. It also takes `maxsplit=`, and `side=` is unchanged.
- **`find(reverse=)` requires a bool.**
- The `rsplit` and `enumerate` removal messages are reworded to show the new spellings.

**Numbers, dict, list, files**
- **`x.to_int(base=10)`.** Extra arguments raise a TypeError, and `base=` on an int or float raises a TypeError instead of being ignored.
- **`range(end, start=0, step=1)`**, so `range(5)` still works.
- **`round(x, ndigits=)`.** Omitting it still returns an int.
- **`d.get(k, default=null)`.**
- **`d.pop(k)` raises KeyError; `d.pop(k, default=v)` returns `v` instead.**
- **`xs.pop(index=-1)`** on lists.
- **`open(path, mode="r")`.**

**Modules**
- **`chan(cap=0)`.**
- **`m.group(n)`, `m.start(n)` and `m.end(n)`:** `n` is required.
- **`sys.exit(code)`:** `code` is required.
- **Extra positional arguments to `re` functions and pattern methods raise a TypeError.** CPython's `count`, `pos`, `maxsplit` and `flags` are not implemented. Today they are silently ignored, and `re.sub(p, r, s, 1)` returns a wrong answer.
- **`proc.run` rejects a second positional argument.**

**std**
- **io:**
  - `io.buffer(b)` requires its argument, so `io.buffer(b"")` spells the empty buffer.
  - **`io.read(r)`** reads to EOF. **`io.read(r, fixed_size=n)`** reads exactly n bytes, or raises EOFError.
  - The stream method **`r.read(n)`** stays "up to n".
- **http:**
  - **`Response(status, headers=null, body=b"", version="1.1", reason=null)`:** `status` is required.
  - **`write_response(w, req, resp, keep_alive=false)`.**
  - **`Request(method, path, query, version="1.1", headers=null, body=null)`.**
  - **`write_request(w, method, target, headers, body=b"")`:** `headers` is required, because its old default was dead.
  - **`read_response(r, method)`:** `method` is required, because its old default was harmful.
  - **`_check_headers` loses `kind`.** The private hot-path helpers `_percent_decode`, `_parse_query`, `_content_length` and `_check_header` get all-required parameters.
  - **Everything else in std with a default becomes keyword-only, as in the Table 1 recommendations.**

**Corpus**
- A `corpus/core` program whose new spelling CPython rejects moves to `corpus/divergence/` with a `.twin.py`, so its `.expected` is still byte-for-byte CPython's output.
