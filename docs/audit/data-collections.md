# Data types and collections: which spelling is right, and does the tree use it

*A situation-by-situation audit of construction, access, conversion, strings and
bytes, dicts, and the collection protocol. Analysis only — nothing here has been
changed.*

The question is not the one
[`docs/one-way-audit.md`](../one-way-audit.md) asked. That document asked "are
these two things redundant?" and cut the ones that were. Several spellings can
legitimately coexist and still fail a stricter test:

> For any given situation, one spelling is right. Find the situations, name the
> right spelling, and check whether the tree uses it.

The standard is the owner's: **all code written for a given task by any
developer or AI should look the same.** Where two competent people would write
the same job differently, that is a finding — even if both spellings deserve to
exist. A language that has one right answer per situation and does not say which
it is has not finished the job the freeze is supposed to end.

**Method.** Every claim of behaviour below was produced by running it on
`./target/release/oro` (0.2.0), not by reading the parser. Every claim about
usage is a `grep` over `std/`, `examples/`, `bench/` and `corpus/` — 113 `.oro`
files. Every recommendation is written out at a real call site in
`std/http.oro` before it is made, which is the test `rsplit` failed: a cut that
is correct on paper and produces index arithmetic at the call site is a wrong
cut.

**Snapshot.** Measured at `f84d654`, with `std/http.oro` at 2,388 lines. Two
other agents are editing `src/`, `std/` and `examples/` concurrently, so line
numbers may have drifted; every site is quoted so it can be found by text.

**Out of scope, because they are settled.** `is`, chained assignment, `zfill`,
`rsplit`, `index`, `lstrip`/`rstrip`, `isinstance`, `dict.items()`, `str.join`,
the `sum`/`sorted`/`any`/`all`/`enumerate`/`zip` builtins, `__hash__`, type
annotations, `min`/`max` narrowed to scalars, dict iteration yielding pairs,
type names as keywords, chain fusion. Where this document touches one of those
it is to build on it, not to reopen it.

---

## 0. The situations, ranked

| # | Situation | Right spelling | Tree | Confidence |
|---|---|---|---|---|
| 1 | Transform a collection of **pairs** into a list | *does not exist* — the chain cannot destructure | 18 `for` loops, 0 chains | high |
| 2 | Remove a known prefix or suffix | `s.rm_prefix(p)` / `s.rm_suffix(p)` | **0 uses**; 5 longhand sites in `std/http.oro` | high |
| 3 | Build a list of integers over a range, filtered | `range(a, b).filter(p)` | 0 uses; 3 while-counter sites | high |
| 4 | Split off one field at a separator | `s.split(sep, 1)` when both halves are wanted | 0 uses; 6 find+offset sites | medium |
| 5 | Ask whether a dict has a key | `k in d` | 2 uses vs 7 of `d.get(k) == null` | high |
| 6 | Prefix / suffix / first / last / reverse | slice for `str`/`bytes`, protocol for collections | tree already agrees | high |
| 7 | Is this sequence empty | `len(x) == 0`; `x == b""` means *EOF* only | 1 site crosses the line | high |
| 8 | Does this string start with X | `s.startswith(p)` | 10 right, 2 longhand | high |
| 9 | Write a slice bound that is the default | omit it | 11 `[0:n]`, 0 `[:n]`, 2 `[0:len(x)-k]` | medium |
| 10 | Copy a collection | `xs.to_list()` / `d.to_dict()` | 0 uses either way | medium |
| 11 | Reach an element that may be absent | `d.get(k)` vs `d[k]` — **both right** | correct throughout | high |
| 12 | Cross the `str`/`bytes` boundary | `f"…".to_bytes()` | 10 uses, no competitor | high (earned) |

Sections 11 onward record what is earned and why, the gaps that turned up while
measuring, and what I could not settle.

---

## 1. The chain cannot read a pair, and that is why `std/http.oro` has no chains

This is the most important finding in the document, and it reframes the result
the previous audit reported as a discipline failure.

> `std/http.oro` — 2,388 lines, the largest program in the language — contains
> **zero** uses of `.map`, `.filter`, `.flat_map`, `.reduce`, `.any`, `.all`,
> `.group_by`, `.partition`, `.take`, `.drop`, `.first` or `.last`.

Re-measured at `f84d654`: still zero. `std/io.oro` and `std/json.oro`: also
zero. `examples/`: zero. Every chain in the tree is in `bench/` or in the four
`corpus/` files that exist to test the protocol.

The previous audit read this as "chains are a headline feature and the standard
library does not use them… it should be fixed by using them". That is half the
story. The other half is mechanical, and it is this:

```python
for k, v in d:                    # works — the `for` statement destructures
    ...
d.map((k, v) => …)                # works — but must answer with a pair
d.to_list().map((k, v) => …)      # RuntimeError: <lambda>() missing required argument: 'v'
xs.enumerate().map((i, x) => …)   # RuntimeError: <lambda>() missing required argument: 'x'
a.zip(b).filter((x, y) => …)      # RuntimeError: <lambda>() missing required argument: 'y'
```

**A `for` statement unpacks a tuple element into named parts. A chain callback
does not.** The one exception is a dict receiver, where the callback is handed
two arguments — and that exception is why the hole is invisible from the README,
which shows `d.filter((k, v) => v > 1)` and never shows a chain over
`enumerate`, `zip` or `group_by` output.

### The census that makes it a situation and not a curiosity

186 `for` statements in the tree. **18 of them destructure a tuple target**, and
7 of those 18 are in `std/http.oro`:

```
std/http.oro   for name, value in headers        (_check_headers)
std/http.oro   for k, v in h                     (write_response — one append)
std/http.oro   for m, pat, fn in self.patterns   (Router.handler_for)
std/http.oro   for m, pat, fn in self.patterns   (Router.allowed)
std/http.oro   for k, v in params                (encode_query)
std/http.oro   for k, v in h                     (write_request — one append)
std/http.oro   for name, value in headers        (_request_headers)
```

The rest are the language's own protocol outputs being consumed by a statement
because nothing else can consume them. Including, in the file whose whole job is
to demonstrate the protocol:

```python
# corpus/divergence/35_collection_protocol.oro:39
by_region = orders.group_by(o => o["region"])
for region, rows in by_region:
    print(region, rows.map(r => r["total"]).sum())
```

`group_by` is a chain step. Its result cannot start another chain without
falling back to `p[0]` / `p[1]`. So the demonstration of the protocol ends in a
`for` loop.

### Five spellings, none of them right

"Turn a dict's entries into a list of transformed values" — the job at
`write_response` and `write_request`, twice each, and the single most common
shape in the module:

```python
# 1. what the tree does (std/http.oro, in write_response and write_request)
parts = [f"HTTP/1.1 {resp.status} {reason}\r\n".to_bytes()]
for k, v in h:
    parts.append(f"{k}: {v}\r\n".to_bytes())

# 2. flat_map — destructures, and is the only chain form that works today
h.flat_map((k, v) => [f"{k}: {v}\r\n".to_bytes()])

# 3. reduce — destructures, and is quadratic
h.reduce([], (acc, k, v) => acc + [f"{k}: {v}\r\n".to_bytes()])

# 4. to_list, then index arithmetic
h.to_list().map(p => f"{p[0]}: {p[1]}\r\n".to_bytes())

# 5. keys, then a second lookup per element
h.keys().map(k => f"{k}: {h[k]}\r\n".to_bytes())
```

All five run. (Verified; `flat_map` over a dict really does hand the callback
two arguments and really does answer a list, which is not documented anywhere.)
Spelling 2 wraps every element in a one-element list to lie its way past a
type-preservation rule. Spelling 3 rebuilds the accumulator per element.
Spelling 4 is the `p[0]`/`p[1]` the `rsplit` lesson exists to forbid. Spelling 5
hashes every key twice and breaks if a value is `null`.

**A situation with five spellings and no right one is worse than a situation
with two.** The stdlib author, faced with this, wrote the `for` loop — and was
right to.

### The verdict

**A multi-parameter lambda in the collection protocol should destructure its
element exactly the way the `for` statement already does.**

That is one rule, it replaces a special case, and it costs nothing at any
existing call site:

- `d.map((k, v) => …)`, `d.filter((k, v) => …)` and the rest stop being a
  dict-only exception and become an instance of the general rule — a dict's
  element *is* a pair, which the README already says in as many words.
- `xs.enumerate().map((i, x) => …)`, `a.zip(b).filter((x, y) => …)`,
  `d.to_list().map((k, v) => …)` and `orders.group_by(f).map((k, rows) => …)`
  all begin to work, so the protocol's own outputs can feed its own inputs.
- A single-parameter lambda still receives the whole tuple, so `p => p[0]` is
  unaffected and nothing in the tree changes meaning.
- `for` already unpacks any two-element sequence — a tuple, a list, even a
  two-character string (`for a, b in ["xy"]` binds `"x"` and `"y"`). The rule to
  write is "the same unpacking `for` performs", not a new one.

**The ergonomics test**, at the real call site:

```python
# today — std/http.oro, write_response
parts = [f"HTTP/1.1 {resp.status} {reason}\r\n".to_bytes()]
_check_headers(h)
for k, v in h:
    parts.append(f"{k}: {v}\r\n".to_bytes())
parts.append(b"\r\n")

# after
_check_headers(h)
parts = [f"HTTP/1.1 {resp.status} {reason}\r\n".to_bytes()]
         + h.map((k, v) => f"{k}: {v}\r\n".to_bytes()).to_list()
         + [b"\r\n"]
```

Honest assessment: that particular site is a *draw*. The loop is fine; the chain
is not obviously better, because the job is "three pieces concatenated" and a
`for` loop expresses the middle piece as well as a `map` does. Which is exactly
why this recommendation is not "rewrite `std/http.oro` in chains". It is that
**the chain must be available**, because today a developer reaching for it is
told their lambda is missing an argument and has to discover that the protocol
has a hole. Four of `std/http.oro`'s 16 `for` loops are blocked by that hole,
and the remaining twelve are mostly loops the previous audit already ruled
correct (statements per element, early exit with a side effect, two
accumulators, pure side effects with no chain form at all).

### What this does to the "zero chains" result

It downgrades it, substantially, and I think correctly.

Of `std/http.oro`'s 16 `for` loops, by my reading: **three** are a chain written
longhand today (`_has_token` is `.any`; `Router.allowed`'s first loop is
`.filter().map()`; `encode_query`'s inner loop is a `flat_map`), **four** are
blocked by the destructuring hole, **and nine are correct as loops** — side
effects over `live.values()`, a find-with-a-side-effect, a four-statement hex
accumulation, a dict build with a `raise` inside it.

Three out of sixteen is a style lapse. Zero out of sixteen looked like a
repudiation of the feature. It is mostly neither: it is a protocol with a hole
in the shape of the data this module actually holds.

The 82 `while` loops with a manual counter are a separate matter and are
[§7b of the previous audit](../one-way-audit.md); §3 below shows what three of
them look like once construction is spelled properly.

---

## 2. Remove a known prefix or suffix — the method exists and nothing uses it

`rm_prefix` and `rm_suffix` are two of the fifteen names on the string surface.
The README gives them a paragraph of justification:

> `rm_prefix`/`rm_suffix` remove a *literal* affix and exist precisely because
> `strip(chars)` gets mistaken for one.

**Census: five occurrences in the entire tree, all in
`corpus/divergence/51_string_surface.oro`, the file that exists to test the
string surface.** Zero uses in `std/`, `examples/`, `bench/`, and zero in every
other corpus file. Meanwhile `std/http.oro` does the job five times, longhand,
with a hard-coded offset:

```python
# std/http.oro — _split_target
if target.startswith(b"http://"):
    target = _authority_path(target[7:])
elif target.startswith(b"https://"):
    target = _authority_path(target[8:])

# std/http.oro — _HeadParser.request_line (and again in _ResponseParser)
if not version.startswith(b"HTTP/"):
    self.fail("unrecognised protocol in the request line")
v = version[5:].to_str()

# std/http.oro — _ChunkedReader.chunk_size
if not line.endswith(b"\r\n"):
    self.fail("chunk size line was not terminated")
head = line[0:len(line) - 2]

# std/http.oro — _unbracket
if host.startswith(b"[") and host.endswith(b"]"):
    host = host[1:len(host) - 1]
```

Four literal magic numbers — `7`, `8`, `5`, `2` — each of which is
`len` of the affix on the line above it, and each of which is wrong the moment
someone edits the affix and not the number. `parse_url`'s `rest = b[sep + 3:]`
after `b.find(b"://")` is the fifth, and the worst: the `3` is not even next to
the string it measures.

### The right spelling, and the one judgement call inside it

```python
v = version.rm_prefix(b"HTTP/").to_str()
head = line.rm_suffix(b"\r\n")
host = host.rm_prefix(b"[").rm_suffix(b"]")
```

All three verified. `rm_prefix` returns the receiver unchanged when the affix is
absent, which is why the `startswith` guard **stays** wherever absence is an
error or selects a different branch:

```python
if not version.startswith(b"HTTP/"):
    self.fail("unrecognised protocol in the request line")
v = version.rm_prefix(b"HTTP/").to_str()      # guard kept; magic 5 gone
```

and **goes** where it was only computing the offset:

```python
host = host.rm_prefix(b"[").rm_suffix(b"]")   # _unbracket: guard and both
                                              # magic numbers gone
```

Careful: `_unbracket`'s guard is `startswith("[") and endswith("]")`, an
*and* — a value of `b"[::1"` keeps its bracket under both spellings, so the
rewrite is faithful. It would not be for an *or*.

**Rule to write down: an affix you can name is removed by name.** A slice whose
bound is the length of a literal on an adjacent line is that removal written
longhand, and it is the removal with an off-by-one bug waiting in it.

This is the highest-value-per-line finding in the document: five sites, five
magic numbers, one method already in the frozen surface, zero cost.

---

## 3. Build a list of integers over a range — construction, and where it went wrong

Three times in its first 220 lines, `std/http.oro` builds a byte class:

```python
_TOKEN_DELIMS = '"(),/:;<=>?@[\\]{}'
_tchars = []
_tb = 33
while _tb < 127:
    if f"{_tb:c}" not in _TOKEN_DELIMS:
        _tchars.append(_tb)
    _tb = _tb + 1
_TOKEN_OK = _tchars.to_bytes()
```

Six lines, three module-level names that outlive the loop (`_tchars`, `_tb`, and
the class itself), a hand-maintained induction variable, and an accumulator.
Repeated for `_FIELD_VALUE_OK` and `_TARGET_OK`, eighteen lines in all.

The right spelling is one line each:

```python
_TOKEN_OK = range(33, 127).filter(b => f"{b:c}" not in _TOKEN_DELIMS).to_bytes()
_TARGET_OK = range(33, 256).filter(b => b != 127).to_bytes()
_FIELD_VALUE_OK = ([9] + range(32, 256).filter(b => b != 127).to_list()).to_bytes()
```

Verified byte-for-byte equal to what the loops produce (77, 224 and 222 octets).
No temporary names survive, no counter to get wrong, and the predicate sits next
to the range it filters instead of three lines below it.

Two things worth knowing that are not in the README:

- `range(a, b)` and `range(a, b, step)` both exist. The README documents only
  `range(n)` — "a range has no literal to build it with" — and never shows the
  two- and three-argument forms, which is very likely part of why the stdlib
  reaches for a counter.
- A chain over a `range` answers a **list** (a range has no literal to rebuild
  into), so `.to_bytes()` works on the result. But `range(65, 68).to_bytes()`
  raises `'range' object has no conversion to bytes` — a range converts to bytes
  only by passing through `filter`, `map` or `.to_list()` first. That is an
  inconsistency, not a design: see §13.

**The general rule, which is the construction half of the loop rule the previous
audit drew:** a collection built by appending to an accumulator inside a loop is
a chain written longhand, and where the source of the loop is a count, the
source of the chain is `range`. The previous audit's §7b measured 67 manual
counter loops and proved `for … in range(…)` is 35% *faster* than the counter it
replaces; this is the same site set seen from the other end, where the answer is
not a `for` loop at all.

---

## 4. Split off one field at a separator

`std/http.oro` reaches for `find` and then slices around the index at six sites:

```python
c = line.find(b":")
if c <= 0:
    self.fail("malformed header line")
raw_name = line[0:c]
raw_value = line[c + 1:]                 # header line

e = pair.find(b"=")                      # query pair
q = target.find(b"?")                    # path and query
sep = b.find(b"://")                     # scheme
sp = key.find(" ")                       # route key
colon = authority.find(b":", reverse=true)  # host and port
```

`split(sep, maxsplit)` is the other spelling, and the README already litigated
this *against itself* when it reinstated `rsplit`'s capability:

> the replacement was still wrong, because `find` hands back an *index* and
> leaves the caller to write `s[:i]`, `s[i + 1:]` and the `-1` check — three
> chances to be off by one where there had been none.

That is a verbatim description of the six sites above, written in the README as
the reason a design was reversed, in a language whose largest program does it six
times.

### The verdict, and where I am less sure than elsewhere

I do not think "always split" survives the ergonomics test. Written out:

```python
# std/http.oro — _path_and_query, today
q = target.find(b"?")
if q < 0:
    return _percent_decode(target, false, fail, where).to_str(), {}
path = _percent_decode(target[0:q], false, fail, where).to_str()
return path, _parse_query(target[q + 1:], fail, where)

# after
parts = target.split(b"?", 1)
if len(parts) == 1:
    return _percent_decode(parts[0], false, fail, where).to_str(), {}
path = _percent_decode(parts[0], false, fail, where).to_str()
return path, _parse_query(parts[1], fail, where)
```

A `q < 0` test became a `len(parts) == 1` test and `parts[0]`/`parts[1]` are not
better names than `target[0:q]`. That is a draw, and I will not recommend churn
for a draw.

The line that *does* hold, and that resolves all six sites:

> **Use `split(sep, 1)` when the separator's own length would otherwise appear
> as a number in the slice. Use `find` when you want the position itself, when
> two separators compete for the same field, or when the separator is one
> character.**

Applied:

- **`sep = b.find(b"://")` … `rest = b[sep + 3:]`** — clear win for `split`. The
  `3` is `len(b"://")` written as a literal, five lines away from the string it
  measures, in the function that validates a URL. `b.split(b"://", 1)` deletes
  it.
- **`c = line.find(b":")` … `line[c + 1:]`** — win for `split`, and it is
  *safer*. `line.split(b":", 1)` gives `[name, value]` or `[line]`, so the
  `c <= 0` test (which is doing double duty: "no colon" *and* "empty name")
  becomes `len(parts) != 2`, with the empty-name case already caught by
  `_is_token(raw_name)`, which requires `len(b) > 0`. One test stops meaning two
  things.
- **`q = target.find(b"?")`, `e = pair.find(b"=")`, `sp = key.find(" ")`** —
  single-character separators, `+ 1` arithmetic, a draw. Leave them.
- **`_split_authority`** — `find(b"/")` and `find(b"?")` race for the earlier
  position. No split expresses that. `find` is correct and stays.
- **`_split_host_port`** — `find(b"]", reverse=true)` then a relative
  `find(b":")` inside the remainder. `find` is correct and stays; this is the
  function that justifies `reverse=` existing.

So: two rewrites, four confirmations. Confidence medium — the rule is right, but
it is a rule about *when a length becomes a literal*, which is narrower and less
memorable than I would like.

---

## 5. Ask whether a dict has a key

Four spellings, and they are not all the same question:

```python
k in d                       # is there an entry for k
d.get(k) == null             # is there an entry for k whose value is not null
d.get(k, default)            # the value, or a stand-in
d[k]                         # the value, or KeyError
```

`k in d` is a hash lookup that answers a `bool`. `d.get(k) == null` answers the
same `bool` **only when `null` is not a legal value in this dict** — and the
reader has to prove that to themselves, at every site, from context the line
does not carry.

**The tree splits 7–2 in favour of the one that needs the proof.**

```python
# std/http.oro — the presence tests written as a null comparison
if version == "1.1" and headers.get("host") == null:      # read_request
if h.get("date") == null:                                  # write_response
if h.get("host") == null:                                  # write_request
if h.get("host") == null:                                  # _client_headers
if h.get("user-agent") == null:                            # _client_headers
if h.get("connection") == null:                            # _client_headers
existing = out.get(name)                                   # _HeadParser.headers
if existing == null:

# std/http.oro — the presence tests written as membership
elif name in _SINGLE_VALUED:
if key in h:
```

Every one of the seven is on a header dict whose values are always `str`, so
every one is correct today. That is the problem: they are correct *by a fact
about the data*, and the two that use `in` are correct by construction.

**Verdict: `k in d` is the presence test.** `d.get(k)` is the *retrieval* that
tolerates absence, and the `== null` that follows it is a test of the value, not
of the key. Where the code goes on to use the value — `existing = out.get(name)`
then `existing + ", " + value` — `get` is exactly right and should stay. Where it
does not, as in all four `_client_headers`/`write_*` sites, `in` is the spelling:

```python
# today
if h.get("host") == null:
    h["host"] = u.host_header()

# after
if "host" not in h:
    h["host"] = u.host_header()
```

Shorter, one lookup instead of a lookup and a comparison, and it stops depending
on a property of the values to be a correct question about the keys.

This matters more in Oro than in Python for a reason the previous audit
identified from the other direction: Oro chose sentinel returns, so `null` is a
load-bearing value in this language and a dict that legitimately holds one is not
exotic. A `d.get(k) == null` presence test is the dict version of `if s.find(x):`
— right until the data changes underneath it, and silent when it goes wrong.

**Not a redundancy, and I want to be explicit about it:** `d[k]` and `d.get(k)`
are *not* two spellings of one job, and neither is `d.get(k, default)` a third.
`d[k]` demands, `d.get(k)` asks, `d.get(k, default)` asks with a stand-in ready.
The previous audit's §8 rule — a lookup that can legitimately find nothing
answers with a value; an operation that was asked to do something and could not,
raises — governs all three and is right. The tree obeys it perfectly: there is
not one `except KeyError` in `std/`, `examples/` or `bench/`, and every
`d.get(k)` call in `std/` is on a dict whose keys are genuinely optional.

### A note on `d.get(k) or default`

`and`/`or` return an operand, so `d.get(k) or "default"` is available and the
previous audit called it "the language's idiom for a fallback". **It is used zero
times in `std/`, `examples/` and `bench/`.** Good. It is wrong whenever the
stored value can be falsy — `0`, `""`, `b""`, `false` — and `d.get(k, default)`
is both shorter and exactly right. I would state in the README that
`d.get(k, default)` is the fallback spelling and `or` is not, because the tree
has already decided this unanimously without saying so.

---

## 6. Prefix, suffix, first, last, reverse — the slice operator versus the protocol

Six jobs are spelled twice, and this is the largest *unremarked* overlap in the
language:

| job | slice | collection protocol |
|---|---|---|
| first `n` | `xs[:n]` | `xs.take(n)` |
| all but first `n` | `xs[n:]` | `xs.drop(n)` |
| reverse | `xs[::-1]` | `xs.reversed()` |
| first element | `xs[0]` | `xs.first()` |
| last element | `xs[-1]` | `xs.last()` |
| every `k`th | `xs[::k]` | — |

On a list and on a tuple these agree in every respect I could find, including
type preservation (`(1,2,3)[:2]` is `(1, 2)`, as `.take(2)` is) and
out-of-range behaviour (`xs.take(99)` and `xs[:99]` both answer the whole thing;
`xs.drop(99)` and `xs[99:]` both answer empty).

They differ at the edges, and the difference is exactly complementary:

```python
"hello"[:2]        # 'he'
"hello".take(2)    # AttributeError: 'str' object has no attribute 'take'
{"a":1}.take(1)    # {'a': 1}
{"a":1}[0:1]       # TypeError: 'dict' object is not sliceable
range(5).take(2)   # [0, 1]
range(5)[1:3]      # TypeError: 'range' object is not sliceable
```

**`str` and `bytes` are outside the collection protocol. `dict`, `range` and
generators are outside the slice.** Only `list` and `tuple` are in both, and
that is where the redundancy lives.

### The verdict, which the tree already follows

> **Slice when you hold an index. Use the protocol when you hold a count.**
> On `str` and `bytes` the slice is the only spelling; on a dict, a range or a
> generator the protocol is.

`std/http.oro` slices 11 times and calls `.take`/`.drop`/`.first`/`.last` zero
times — and in every one of the 11 the bound came out of a `find`:

```python
raw_name = line[0:c]              # c = line.find(b":")
path = target[0:q]                # q = target.find(b"?")
scheme = b[0:sep]                 # sep = b.find(b"://")
```

Those are indices, on `bytes`, in a byte-oriented parser. The protocol has no
form for them and should not grow one. The tree is right, and this is a case
where two spellings both earn their place and the line between them just needs
writing down.

**`xs[::-1]` is the exception, and it should be named as never-preferred.** A
step slice on a list or tuple is `.reversed()` or a `filter` written as
punctuation. It appears twice in the tree, both in
`corpus/core/35_bytes.oro`, which is an exhaustive slice-semantics test on
`bytes` — where it is the only spelling and is therefore correct. No program
uses `[::-1]` or `[::k]` on a list. The README should say that the step form
exists for `str` and `bytes` and that `.reversed()` is the spelling everywhere
else, before somebody discovers `[::-1]` and starts a second tradition.

### One difference that is a bug, not a design

```python
[][0]         # IndexError: list index out of range
[].first()    # RuntimeError: first() on an empty sequence
[].last()     # RuntimeError: last() on an empty sequence
```

Two spellings of one job that raise two different classes, one of which is not
catchable at the width a caller would reach for. `RuntimeError` is what
`d.map((k, v) => v)` also raises where `TypeError` is what it describes — the
previous audit flagged that one. This is the same fault: `first()` and `last()`
on an empty sequence are an index out of range, and `IndexError` is the class.
Small, mechanical, and it should go before the freeze, because after the freeze
`except IndexError` around a `.first()` is a promise the language broke.

---

## 7. Is this sequence empty

```python
len(x) == 0 / len(x) > 0    # 15 sites in std/
x == b"" / x != b""         # 11 sites in std/
x == ""                     #  1 site in std/
not x                       #  0 sites in std/ on a non-bool
```

This looks like three spellings of one test. It is two spellings of **two
different tests**, and the split is real and worth defending.

Every one of the eleven `b""` comparisons in `std/` is an **EOF test**:

```python
chunk = body.read(_STREAM_CHUNK)
while chunk != b"":
```

The io protocol makes `b""` the EOF sentinel by fiat — *"EOF is an empty return,
not an exception"* — so `chunk == b""` is asking "did the stream end", and
`len(chunk) == 0` would be asking the same question in a spelling that does not
say so. That is a genuine second meaning attached to a genuine second spelling,
and the tree gets it right every time it uses it.

`len(x) == 0` is the emptiness test on a value that is merely a value:

```python
if len(pair) == 0:            # a field between two '&'s in a query string
if len(head) == 0:            # a chunk-size line with nothing on it
if len(authority) == 0:       # a URL with no host
if len(digits) == 0 or digits.scan(_DIGIT_OK) != len(digits):
```

**The one site that crosses the line** is `_with_params`:

```python
qs = encode_query(params)
if qs == "":
    return u
```

`qs` is a `str` that `encode_query` built out of a list; there is no stream and
no EOF anywhere near it. It should be `if len(qs) == 0:`, which is what the file
writes everywhere else for exactly this. Trivial as a bug — it cannot go wrong —
and worth fixing precisely because it cannot: it is the site that would teach a
reader the wrong rule.

**Truthiness stays cut by usage, and the previous audit's §10 argument holds.**
Worth adding one line to it that this section supplies: the reason `if not xs:`
is a bad spelling in Oro specifically is that Oro made `b""` mean *end of
stream*, so `if not chunk:` reads as "if the chunk is empty" and means "if the
stream ended", and those are two different sentences about the same bytes.

---

## 8. Does this string start with X

```python
s.startswith(p)        # 10 sites in std/
s[0] == c              # 2 sites
s[0:n] == p            # 0 sites
s.find(p) == 0         # 0 sites
```

`startswith` wins on usage and is obviously right: it is one call, it handles the
empty receiver (`"".startswith(":")` is `false`, verified), and it takes the
optional `start`/`end` window the corpus tests exhaustively.

The two longhand sites are both guarded, and the guard is the tell:

```python
# std/http.oro — _match
if len(p) > 0 and p[0] == ":":

# std/http.oro — _HeadParser.headers
if line[0] == _SPACE or line[0] == _TAB:
```

The first is `if p.startswith(":"):` — one call, no guard, and the guard was
there *because* `p[0]` raises on an empty string. A spelling that needs a
length check in front of it to be safe is the wrong spelling for a test that has
a safe one.

The second is different and I would leave it. `line` is `bytes`, so `line[0]` is
an `int` and `_SPACE`/`_TAB` are int constants; the alternative is
`line.startswith(b" ") or line.startswith(b"\t")`, which is two calls where the
loop above guarantees `len(line) > 0`. Call it a draw, leaning to `startswith`
for consistency with the other ten.

**The rule:** ask a question about the front of a sequence with the method whose
name is that question. Reach for `s[0]` only when you want the element itself —
on `bytes`, where the element is an `int` and the comparison is to an int.

---

## 9. Redundant slice bounds

```python
rest[0:q], b"/" + rest[q:]        # std/http.oro, one statement, both spellings
```

`std/` writes `x[0:n]` **11 times and `x[:n]` zero times**, while
writing `x[n:]` and never `x[n:len(x)]`. It omits the redundant upper bound and
writes the redundant lower one, sometimes in the same expression.

And twice it computes a bound the language would have computed:

```python
head = line[0:len(line) - 2]       # = line[:-2], and really = line.rm_suffix(b"\r\n")
host = host[1:len(host) - 1]       # = host[1:-1], and really rm_prefix/rm_suffix
```

**Verdict: omit a bound that is the default, and index from the end with a
negative index rather than with `len(x) - k`.** A written bound is a claim the
reader has to check; `len(line) - 2` is a subtraction the reader has to perform
and the author could have got wrong. Negative indices work (`[1,2,3][-1]` is
`3`, `b[-5:]` is tested in the corpus) and are oracled against CPython.

Two caveats, and they are why my confidence is medium rather than high:

- **`x[:-0]` is empty, not everything.** A negative index computed at run time
  is a trap where a literal is not. The rule should be "negative literal, yes;
  negative expression, no".
- Both of the two sites above are better served by §2's `rm_prefix`/`rm_suffix`
  than by either slice spelling, so the negative-index rule is the fallback and
  not the headline.

The `[0:n]` versus `[:n]` half I am confident about and would settle: **omit
it**. Eleven sites, mechanical, and it makes `rest[0:q], b"/" + rest[q:]` stop
contradicting itself inside one `return`.

---

## 10. Copy a collection

Three spellings, all verified to produce an independent object:

```python
b = a.to_list()    # a cast that is a copy
b = a[:]           # a slice that is a copy
b = a + []         # a concatenation that is a copy
d2 = d.to_dict()   # the dict form of the first
```

**Zero uses of any of them in `std/`, `examples/` or `bench/`** — the stdlib
never copies a collection. When it needs a snapshot it takes one for free,
because `.values()` and `.keys()` already answer fresh lists:

```python
# std/http.oro — _drain_live
rest = live.values()          # a snapshot; the loop below closes connections,
for entry in rest:            # which removes entries from `live`
    entry.conn.close()
```

That is the right use of an eager view and is a good argument for the eagerness
divergence, which the README currently defends only on repr grounds.

**Verdict: `xs.to_list()` and `d.to_dict()` are the copy spelling**, and the
other two should be named as not-it. The reasoning is the README's own:
conversion is a method, it chains, and it reads. `a[:]` is a copy that looks like
a slice of everything, which is a Python idiom rather than an argument; `a + []`
is a copy that looks like a concatenation with nothing.

**But flag the collision honestly.** `xs.to_list()` where `xs` is already a list
is *both* an identity cast and a copy, and the call site cannot say which was
meant. `d.to_dict()` likewise. That is one name doing two jobs, and it is the
only place in the `to_` family where the no-op form is load-bearing rather than
merely tolerated. I do not have a better answer than documenting it —
`.copy()` would be a sixteenth name and a second spelling of a cast — but a
sentence in the README saying "`to_list()` on a list is a copy, and is how you
take one" turns an accident into a decision.

---

## 11. Conversions, and where two casts reach the same result

The `to_` family is `to_str`, `to_bytes`, `to_int`, `to_float`, `to_bool`,
`to_list`, `to_dict`. I looked for pairs that reach the same result and found
fewer than expected, which is a good sign.

**Genuinely overlapping, in one narrow case:** `x.to_str()`, `f"{x}"` and
`repr(x)` all answer `'5'` for the integer `5`. The previous audit's §11 settled
`to_str` versus f-string (a conversion versus a template) and I agree; the part
it did not say is that they **diverge on a `str`**, where `s.to_str()` is a no-op
and `f"{s}"` is also a no-op but `repr(s)` adds quotes — so the three are only
interchangeable on numbers, which is where they are most likely to be confused.
The line that resolves it: `f"{x}"` when the result is going into a string,
`x.to_str()` when the result is a value in its own right, `repr(x)` only for
diagnostics.

**Not overlapping, and I checked because they look like they should:**

```python
"hi".to_bytes()          # b'hi'  — UTF-8 encode
[104, 105].to_bytes()    # b'hi'  — octets from a list of ints
(5).to_bytes()           # TypeError: 'int' object has no conversion to bytes
"ab".to_list()           # ['a', 'b']  — characters
b"ab".to_list()          # [97, 98]    — octets
```

Two different jobs reaching one type, from two source types that cannot be
confused. Correct, and `.to_list()` disagreeing with itself across `str` and
`bytes` is the two-type split working as designed rather than an inconsistency.

**The `str`/`bytes` boundary is the best-behaved surface in the tree.** One
spelling, used ten times, with no competitor anywhere:

```python
parts.append(f"{k}: {v}\r\n".to_bytes())
size = f"{len(chunk):x}\r\n".to_bytes()
sys.stderr.write(f"http: handler raised {type(e)}: {e}\n".to_bytes())
```

Format with an f-string, convert once at the write. No site in `std/` builds
bytes by concatenating `b"…"` literals with formatted numbers, and no site
converts twice (`.to_str().to_bytes()` appears nowhere outside a corpus test of
the round trip). If the README wants a worked example of "one way to do each
thing" that the stdlib actually honours, this is it.

**Building bytes incrementally** is equally clean: `parts = [...]`,
`parts.append(...)`, `parts.join(b"")`, at seven sites in `std/http.oro` and two
in `std/io.oro`, exactly as the design doc prescribes when it explains why there
is no `flush()`. The one wobble is `[size, chunk, b"\r\n"].join(b"")` in
`_write_chunked`, where three known values are concatenated through the
list-and-join idiom rather than with `+`; `size + chunk + b"\r\n"` is the
spelling used at `b"/" + rest[q:]` eight hundred lines earlier. Minor, but it is
two spellings of "concatenate three things I already have", and the rule the
previous audit drew for strings applies unchanged: **`+` joins pieces you
already have; `join` ends a chain or closes an accumulator.**

---

## 12. Where two spellings both earn their place

Recorded so these are not reopened.

**`d[k]`, `d.get(k)` and `d.get(k, default)`.** Three questions, not three
spellings — demand, ask, ask-with-a-stand-in. §5.

**The slice and the collection protocol.** Complementary at the edges
(`str`/`bytes` have only the slice; `dict`/`range`/generators have only the
protocol) and redundant only on `list` and `tuple`, where the index/count rule
settles it. §6.

**`x == b""` and `len(x) == 0`.** EOF and emptiness are different questions and
the io protocol made `b""` the word for the first. §7.

**`find` and `in`.** The previous audit's §12 covered
`find(x) >= 0` as containment written longhand; that stands, and there are ten
such sites in `std/http.oro` (`name.find("\r") >= 0`, `path.find(":") < 0`,
`target.find(b"#") >= 0`, `host.find(":") >= 0`, …). What is worth adding is that
`in` on `bytes` is a **substring** test while iterating `bytes` yields **ints**,
so `97 in b"abc"` raises `'in <bytes>' requires bytes as left operand` while
`for x in b"abc"` binds `97`. One type, two answers to "what is an element". It
is pinned in `corpus/divergence/41_bytes_membership.oro` and it is defensible —
substring search is what `in` is for on a text-shaped type — but it is the
sharpest edge in the two-type design and the README does not mention it.

**`b.scan(set)` and a per-character loop.** `digits.scan(_DIGIT_OK) != len(digits)`
is the right spelling for "is every octet of this field in a class", it is one
call into Rust, and `std/http.oro` uses it four times. The `for ch in s` version
at `_content_length` is the wrong spelling of the same question — but it is on a
`str`, and `str` has no `scan` and no `.all`, so the loop is the only spelling
available. See §13.

**`for` and the chain.** The previous audit's §9 line is right and this document
does not disturb it: a chain is an eager expression, a `for` is a lazy statement
that can run statements, exit early with a side effect, and feed two
accumulators. §1 adds the missing clause — that the chain must be *able* to read
a pair before "if the loop body is a single append of an expression, it is a
chain written longhand" is a fair rule to hold anyone to.

---

## 13. Gaps found while measuring

Not two-spellings problems. Each is a place where the right spelling does not
exist, which is how a second-best spelling becomes a habit.

1. **`str` and `bytes` are outside the collection protocol entirely.** No `map`,
   `filter`, `any`, `all`, `first`, `take`. `"abc".to_list()` is the only bridge,
   it allocates, and `b"abc".to_list()` changes the element type on the way
   through. This is the direct cause of `_content_length`'s hand-rolled digit
   loop, which the previous audit counted as one of three spellings of "is this
   all digits" without noticing that two of the three were forced. A
   `str.scan(chars)` twin of `bytes.scan` would close the specific case; opening
   the whole protocol to `str` would not, because a chain over a `str` has no
   shape to rebuild into.

2. **`range(a, b).to_bytes()` raises, but `range(a, b).filter(p).to_bytes()`
   works** — because `filter` already answered a list. The conversion should
   accept a range directly, or the error should name `.to_list()`. §3.

3. **`[].first()` and `[].last()` raise `RuntimeError` where `[][0]` raises
   `IndexError`.** §6.

4. **There is no side-effect-per-element form**, and there should not be one —
   `for` is it. Worth saying out loud in the README next to the chain rule,
   because `_check_headers` (a loop whose body is one call and which accumulates
   nothing) reads like a `map` that forgot to be one, and the answer is that it
   is correctly a statement.

5. **`flat_map` over a dict hands its callback two arguments and answers a
   list**, which is the only working chain form for a dict-to-list transform and
   is documented nowhere. Whether it survives §1's fix is a design call — under
   the destructuring rule it becomes redundant with `d.map(…).to_list()` — but
   it should not stay undocumented either way.

---

## 14. If only three things happen

1. **Make a multi-parameter lambda destructure its element the way `for`
   does** (§1). It closes a five-spelling situation to one, makes the protocol's
   own outputs (`enumerate`, `zip`, `group_by`, `to_list`) chainable, turns the
   dict callback from a special case into an instance of a rule, and changes the
   meaning of nothing in the tree. It is also the honest answer to "why does the
   standard library not use chains", which is not, on inspection, mostly about
   discipline.

2. **Use `rm_prefix` / `rm_suffix`** (§2). Five sites in `std/http.oro`, five
   magic numbers, two methods already in the frozen fifteen-name surface and
   used **zero** times outside the file that tests them. Nothing is cut, nothing
   is added, and four literal offsets that are silently coupled to string
   literals on adjacent lines go away.

3. **Write down the two rules the tree half-follows, and fix the sites that
   break them** (§5, §6, §9): `k in d` is the presence test; slice when you hold
   an index and use the protocol when you hold a count; omit a slice bound that
   is the default. Eleven `[0:n]`, four `d.get(k) == null` presence tests, one
   `qs == ""`. All mechanical, and together they are most of what a reader of
   `std/http.oro` currently has to reverse-engineer from forty call sites.

---

## Confidence, stated plainly

**High:** §1 (the destructuring hole is mechanical and verified), §2 (a census
of five versus zero is not a judgement call), §5, §6, §7, §8.

**Medium:** §3 (the rewrite is verified byte-for-byte, but three sites is a
small sample on which to hang a construction rule), §4 (the rule is right and
narrower than I would like), §9 (the negative-index half has a real trap in it),
§10 (the copy verdict is a preference, and the tree offers no evidence either
way because it never copies).

**Where I could not settle it:** whether `flat_map`'s dict arity survives §1, and
whether `str` should get a `scan`. Both are small; both want a decision from
whoever owns the string surface rather than from a census.
