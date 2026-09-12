# Native builtins: optional-parameter survey (`src/builtins/mod.rs`)

Scope: the free builtins in `lookup`, `call_type` (`range`), the `to_*` casts, the native collection methods (`seq_native_method`, `list_method`, `dict_method`), `str_method`, `bytes_method`, and the `Match` methods that live in the same file. Surveyed at `feat/sane-defaults` `9019829`. Every behaviour below was run on `target/release/oro`, and every CPython claim was run on `python3` 3.12.10.

**Counts.**
- **pos** = passed positionally (the rule forces every one of these to change). **pos `null`** = an explicit `null` passed positionally to mean "omitted" (see note 1).
- Dirs: `std`, `ex` = examples, `corpus` (core + divergence + known-failing), `tests` (`tests/programs/*.oro` plus the Oro in `tests/*.rs` string literals), `bench`, `rs-tests` (the Oro in `src/**/tests.rs` string literals).
- `bench` has **no positional use of any optional parameter in scope**. Its 5 relevant sites all omit the parameter.
- Excluded from the counts: `corpus/core/39_method_arity.oro`, whose every call is a rejection probe, and error-probe sites that pass too many args or a bogus keyword (11 sites).
- Sites were found by a static scan. Receiver types came from a runtime trace of all 128 `.oro` programs, run on an instrumented copy of the interpreter built in scratch. 90 sites that were never executed, or are embedded in Rust, were typed by literal-type heuristics (75) or by hand (13), and 2 by weak heuristic. Collection `find(p)` and `count(p)` callbacks, regex `split`, and user methods are not counted.

| callable | param | omitted means | accepted today | calls pos / named / omitted (by dir) | CPython kw? | recommendation | reason |
|---|---|---|---|---|---|---|---|
| `open(path, mode)` | `mode` | `"r"` | positional only (`open(p, mode="r")` → "takes no keyword arguments") | pos **19** (ex 4, corpus 15); named **0**; omitted **3** (corpus 2, rs-tests 1) | yes | **named** `mode=` (see needs-a-decision) | It selects a mode, and `"w"` is a code word, not a self-evident value. It matches `side=` and `reverse=`. |
| `round(x, ndigits)` | `ndigits` | returns an **int** (`round(2.5)` → `2`). `round(2.5, 0)` → `2.0`, so omission is a sentinel, not `0`. `null` is rejected. | positional only | pos **11** (ex 2, corpus 9); named **0**; omitted **6** (corpus 6) | yes | **named** `ndigits=` (see needs-a-decision) | A precision is a tuning number, and `round(x, 2)` hides what the 2 is. |
| `range(start, stop, step)` | `start` | `range(n)`: the first argument is *stop* and start is 0. In the 2/3-arg forms the first argument is start. | positional only, overloaded by arity | 2/3-arg forms **23** (std 3, corpus 18, tests 2); named **0**; 1-arg `range(n)` **79** (std 2, ex 2, corpus 68, tests 1, rs-tests 6) | **no** (`range() takes no keyword arguments`) | **needs a decision** (A/B/C below) | The no-arg form is a different arity-shape, and the rule cannot express an optional *leading* parameter (criterion 3). |
| `range(start, stop, step)` | `step` | 1 | positional only | pos **12** (corpus 12); named **0**; omitted **90** (std 5, ex 2, corpus 74, tests 3, rs-tests 6) | **no** | **named** `step=` | A stride is a tuning number, as in `range(0, 10, step=3)`. |
| `x.to_int(base)` | `base` | 10 (plus `int()`-style sign, `_` and whitespace handling). `null` also means 10. The base is **silently ignored** on int and float receivers (`(5).to_int(16)` → 5). | positional only | pos **12** (corpus 12); named **0**; omitted **12** (std 4, ex 2, corpus 6) | yes (`int(s, base=16)`) | **named** `base=` | The radix selects a variant, as in `"ff".to_int(base=16)`. |
| `xs.sum(start)` | `start` | `0` (`sum(null)` is a TypeError because start is a real value) | positional only | pos **1** (corpus 1); named **0**; omitted **23** (ex 3, corpus 16, tests 2, rs-tests 2) | yes (`sum(xs, start=)`) | **named** `start=` | It is a rare initial accumulator, and `xs.sum([])` does not say that `[]` is the seed. |
| `xs.enumerate(start)` | `start` | 0 (`null` also means 0) | positional only | pos **3** (corpus 3); named **0**; omitted **12** (corpus 10, rs-tests 2) | yes | **named** `start=` | It is an offset, and `xs.enumerate(1)` misreads as a count. |
| `s.strip(chars, side=)` (str/bytes) | `chars` | whitespace: Unicode `White_Space` for str, the 6 ASCII octets for bytes. `null` means the same. | positional only (`chars=` → "unexpected keyword argument") | pos **20** (ex 5, corpus 11, rs-tests 4); named **0**; omitted **39** (std 3, ex 4, corpus 21, tests 1, bench 1, rs-tests 9) | **no** (`str.strip() takes no keyword arguments`) | **named** `chars=` | Two-thirds of calls omit it, so required fails. The name also surfaces that it is a character *set*, the footgun the source comments on. |
| `s.strip(chars, side=)` | `side` | `"both"` | keyword only (positional → "at most 1 argument") | pos **0**; named **28** (ex 5, corpus 14, rs-tests 9); omitted **31** (std 3, ex 4, corpus 18, tests 1, bench 1, rs-tests 4) | n/a (Oro-only) | **named** (no change) | It is a mode selector and already conforms. |
| `s.split(sep, maxsplit, side=)` (str/bytes) | `sep` | a whitespace split: runs collapse and the ends are dropped. This is a different algorithm from a separator split. `null` means the same. | positional only (`sep=` → "unexpected keyword argument") | pos **77** (std 8, ex 10, corpus 38, tests 10, rs-tests 11); pos `null` **17** (ex 4, corpus 9, rs-tests 4); named **0**; omitted **3** (corpus 2, rs-tests 1) | yes | **needs a decision** (see below) | `s.split()` and `s.split(",")` are two operations (criterion 3). |
| `s.split(sep, maxsplit, side=)` | `maxsplit` | -1, unlimited (`null` also) | positional only (`maxsplit=` → "unexpected keyword argument") | pos **43** (std 1, ex 7, corpus 22, rs-tests 13); named **0**; omitted **54** (std 7, ex 7, corpus 27, tests 10, rs-tests 3) | yes | **named** `maxsplit=` | It is a tuning number. It also deletes the `split(null, 1)` placeholder that exists only to reach it. |
| `s.split(sep, maxsplit, side=)` | `side` | `"left"` (without `maxsplit` it has no effect, by design) | keyword only | pos **0**; named **25** (ex 2, corpus 11, rs-tests 12); omitted **72** (std 8, ex 12, corpus 38, tests 10, rs-tests 4) | n/a | **named** (no change) | It is a mode selector and already conforms. |
| `s.find(sub, start, end, reverse=)` (str/bytes) | `start` | 0 (`null` also) | positional only (`start=` → "unexpected keyword argument") | pos **10** (std 1, corpus 8, rs-tests 1); named **0**; omitted **57** (std 23, ex 4, corpus 19, tests 1, bench 1, rs-tests 9) | **no** | **named** `start=` | It is a window bound, as in `b.find(b"%", start=i + 3)`. Naming both bounds lets `end=` be given alone without a `null` placeholder. |
| `s.find(...)` | `end` | the length (`null` also) | positional only | pos **4** (corpus 3, rs-tests 1); named **0**; omitted **63** (std 24, ex 4, corpus 24, tests 1, bench 1, rs-tests 9) | **no** | **named** `end=` | Same as `start`. |
| `s.find(...)` | `reverse` | false, meaning first occurrence. Any truthy value is accepted (`reverse="yes"` → last occurrence). | keyword only | pos **0**; named **16** (std 2, ex 2, corpus 7, rs-tests 5); omitted **51** (std 22, ex 2, corpus 20, tests 1, bench 1, rs-tests 5) | n/a | **named** (no change) | It is a flag and already conforms. It could require a bool. |
| `s.count(sub, start, end)` (str/bytes) | `start` | 0 (`null` also) | positional only (`start=` → "takes no keyword arguments") | pos **13** (corpus 4, rs-tests 9); named **0**; omitted **18** (ex 6, corpus 8, rs-tests 4) | **no** | **named** `start=` | Same window as `find`, so the two surfaces stay alike. |
| `s.count(...)` | `end` | the length (`null` also) | positional only | pos **4** (corpus 2, rs-tests 2); named **0**; omitted **27** (ex 6, corpus 10, rs-tests 11) | **no** | **named** `end=` | Same as `find`. |
| `s.startswith` / `s.endswith(affix, start, end)` (str/bytes) | `start` | 0 (`null` also) | positional only | pos **8** (corpus 8); named **0**; omitted **39** (std 12, ex 1, corpus 22, tests 2, rs-tests 2) | **no** | **named** `start=` | Same window as `find`. |
| `s.startswith` / `s.endswith(...)` | `end` | the length (`null` also) | positional only | pos **4** (corpus 4); named **0**; omitted **43** (std 12, ex 1, corpus 26, tests 2, rs-tests 2) | **no** | **named** `end=` | Same window as `find`. |
| `s.replace(old, new, count)` (str/bytes) | `count` | -1, meaning every occurrence (`null` also) | positional only | pos **2** (corpus 2); named **0**; omitted **10** (std 1, corpus 6, tests 1, bench 1, rs-tests 1) | **no** on 3.12 (`str.replace() takes no keyword arguments`). 3.13 added `count=` per its changelog; not run here, since only 3.10 and 3.12 are installed and the oracle uses 3.12. | **named** `count=` | It is a tuning number, and `replace("a", "b", 2)` hides what the 2 is. |
| `xs.pop(index)` (list) | `index` | the last element (`pop(null)` → TypeError) | positional only | **no call sites in the tree** (the only one is the arity probe `[1].pop(1, 2)`) | **no** (`list.pop() takes no keyword arguments`) | **named** `index=` (see needs-a-decision) | Then a positional argument to `.pop` always means a dict *key*. There are no call sites to test the ergonomics against. |
| `d.get(key, default)` | `default` | `null` (a real value) | positional only | pos **12** (std 3, ex 2, corpus 4, tests 2, rs-tests 1); named **0**; omitted **18** (std 16, corpus 2) | **no** (`dict.get() takes no keyword arguments`) | **named** `default=` | It is passed at 12 of 30 sites, so required would force `get(k, null)`. `default=0` states the role. |
| `d.pop(key, default)` | `default` | **raises `KeyError`**, which no value expresses. `pop(k, null)` answers `null`. | positional only | pos **1** (corpus 1); pos `null` **2** (std 1, corpus 1); named **0**; omitted **10** (corpus 10) | **no** (`dict.pop() takes no keyword arguments`) | **needs a decision** (see below) | With and without the default, the call either raises or doesn't. The "default" is a hidden sentinel. |
| `m.group` / `m.start` / `m.end(n)` (Match; the `re` module is the sibling's) | `n` | 0, the whole match | positional only (`group(n=1)` → "takes no keyword arguments") | pos **13** (corpus 11, rs-tests 2); named **0**; omitted **5** (corpus 3, rs-tests 2) | positional yes, keyword **no** (`Match.group() takes no keyword arguments`) | **required**, as in `m.group(0)` | A group number is obvious from the value, and it is passed at 13 of 18 sites. `m.start(0)` and `m.group(0)` were run in CPython and match. |

24 rows.

## Needs a decision

- **`range` `start`** (criterion 3). There are 79 one-arg `range(n)` sites and 23 two/three-arg sites.
  - **A (required):** always `range(start, stop)`, so `range(n)` becomes `range(0, n)`. It is one shape, and CPython accepts it: all 10 core files touched still match.
  - **B (split):** keep `range(n)` and give the two-bound form its own name (for example `span(a, b)`), so the first argument always means one thing.
  - **C (named):** `range(n, start=a)`. This reads stop-before-start, and CPython rejects it, so 24, 40 and 43 would move.
  - In every option, `step` becomes `step=`.
- **`split` `sep`** (criterion 3). There are 77 separator-form sites and 20 whitespace-form sites: 3 bare `split()`, 4 `split(null)`, and 13 `split(null, n)` that exist only to reach `maxsplit`.
  - **split:** `s.split(sep)` with sep required, plus `s.fields(maxsplit=, side=)` for the whitespace form. This is Go's `strings.Fields` precedent. 30, 35 and 38 would move out of core.
  - **named:** `s.split(sep=",")`. This names the obvious value at 77 sites. CPython accepts it, and `split(maxsplit=1)` covers the whitespace form.
  - **required:** keep `split(null)` as the whitespace spelling. It is one spelling, but the `null` is opaque.
- **`dict.pop` `default`.** Omitting it means "raise", so under **named** the default is a hidden sentinel and `default=null` differs from omission. The alternative is to **split** into `d.pop(k)`, which raises, and `d.pop_or(k, default)` (or `d.discard(k)` for the "remove if present" use at `std/http.oro:1453`). The counts are 10 omitted and 3 passed. Both options move `45_dict_pop`.
- **`round` `ndigits`.** Omitting it returns an int, while `ndigits=0` returns a float, so the default is a sentinel just like `dict.pop`'s. The choice is **named** `ndigits=` versus **split** into an int-rounding `round(x)` and a separately named n-digit form. The counts are 11 passed and 6 omitted.
- **`open` `mode`.** It is passed at 19 of 22 sites, which leans toward **required**. But it is a mode code (`"r"`/`"w"`/`"a"`), which leans toward **named**. CPython accepts both.
- **`list.pop` `index`.** The choice is **named** `index=` versus **split** into `xs.pop()` and `xs.pop_at(i)`. There are zero call sites, so ergonomics can't break the tie.

## Affected `corpus/core` files

To verify this, I rewrote each core file into the recommended spellings and ran it under CPython 3.12, with the oracle's `true`/`false`/`null` mapping, diffing against `.expected`. The harness baseline matches all 45 files. `10_deep` needs the oracle's `setrecursionlimit`.

**Must move to `corpus/divergence/` with a `.twin.py`, under the recommendations:**
- `38_str_bytes_optional_args.oro`: 21 edits (`start=`/`end=` on find, count, startswith and endswith; `count=` on replace; `chars=` on strip). CPython raises `TypeError: str.find() takes no keyword arguments`.
- `43_identity_equality.oro`: 9 edits of `range(a, b, step=c)`. CPython raises `TypeError: range() takes no keyword arguments`.
- `35_bytes.oro`: 1 edit, `counts.get(w, default=0)`. CPython raises `TypeError`. Its 2 `maxsplit=` edits are fine.
- `45_dict_pop.oro`: 2 edits, `d.pop(k, default=…)`. CPython raises `TypeError`. It also moves under the `pop_or` split.

**Still match CPython, so they stay in core:** `22_paths` (`mode=`), `27_re` (`m.start(0)`, `m.end(0)`, `m.group(0)`), `30_str_split_maxsplit` (`maxsplit=`; `split(null, n)` becomes `split(maxsplit=n)`), `32_round_and_float_repr` (`ndigits=`), and `39_method_arity` (rejection probes that stay `TypeError` either way).

**Conditional on the open decisions:**
- `range` option C adds `24_generator_features` and `40_repr_nonprintable` (43 already moves). Option A moves nothing.
- The `split`/`fields()` option adds `30_str_split_maxsplit` and `38` (38 already moves), and `35_bytes` (already moving).

## Notes

1. **`null` as omission is a second spelling.** Explicit `null` means "omitted" for find/count/startswith/endswith `start`/`end`, replace `count`, split `sep`/`maxsplit`, strip `chars`, to_int `base` and enumerate `start`. All were run, including `split(",", null)` and `find("b", 2, null)`. This is the `opt_int_arg` helper plus strip's and split's `None` arms. Once these are keyword-only, `f(x=null)` would be a second spelling of `f()`, so reject it. The `null` is a real value only for `dict.get`/`dict.pop` `default`. `round(x, null)`, `list.pop(null)` and `sum(null)` already reject it.
2. **Messages that teach positional spellings** need rewording. The `rsplit` removal message says "use `split(sep, maxsplit, side="right")`", and the `enumerate` removal message says "`xs.enumerate(1)` to start elsewhere". I ran both.
3. **Variable arity that is not an optional parameter.** All four are bugs; I ran each one.
   - `take`, `drop` and `chunk` ignore extra args: `[1, 2, 3].take(2, 99)` → `[1, 2]`, fused chain included. Omitting the count is a `ValueError`, not a default.
   - `to_int` ignores extras (`"10".to_int(16, 2)` → 16) and ignores `base` on int and float.
   - Regex `search`/`split`/etc. in `regex_method` ignore extras: `re.compile("b").search("abcb", 3)` → `span=(1, 2)`, where CPython gives `(3, 4)`; `.split("a,b,c", 1)` → the full split, where CPython gives `['a', 'b,c']`.
   - `find(reverse=)` accepts any truthy value.
4. **Already conforming:**
   - The no-default parameters (`len`, `type`, `abs`, `repr`, `chr`, `ord`, `join`, `rm_prefix`, `rm_suffix`, `scan`, `append`, `extend`) already refuse keywords (`chr(i=65)` → TypeError).
   - These are variadic and unaffected: `print(*args)`, `min(*args)`, `max(*args)` (no `key=`), and `xs.zip(*others)`.
5. **Names shared with the sibling surveys:**
   - `print` is in my lookup table, but the VM's `do_print` handles `sep=`/`end=`. They are already keyword-only, and `flush=` is rejected.
   - `xs.sorted`/`xs.sort` take `key=`/`reverse=` through the VM. They are already keyword-only: `sort(true)` → "takes no positional arguments".
   - `xs.min`/`xs.max`, and the `min`/`max` builtins, go through the VM for `__lt__`.
   - Collection `find(p)`/`count(p)` are VM seq-ops that share names with str/bytes `find`/`count`.
   - `spawn`, `chan` and `yield_now` are in `lookup`, but the VM dispatches them.
   - The regex methods live here, but the `re` module belongs to the sibling.
