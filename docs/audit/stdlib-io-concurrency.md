# One spelling per situation: `io`, `http`, `json`, `net` and concurrency

*An audit of the standard library, I/O and concurrency before the freeze.
Analysis only — nothing here has been changed.*

`docs/one-way-audit.md` asked whether two spellings are **redundant**, and cut
the ones that were. This asks a sharper question, and it is the one that
actually matters for a stdlib:

> Several spellings can legitimately coexist. But for any given situation, one
> of them is right. Find the situations, name the right spelling for each, and
> check whether the tree uses it.

The standard is the owner's: *all code written for a given task by any developer
or AI should look the same.* It binds hardest here, because the standard library
is what people copy. A rule that `std/http.oro` breaks is a rule that will be
broken by everyone who reads `std/http.oro` to learn how this language is
written — and `std/http.oro` is 2,388 lines, the largest program in the
language, and the only extended example anyone has.

Every behavioural claim below was checked by running it against
`./target/release/oro` at 0.2.0. Every timing is that binary, this machine,
20,000 iterations in a `while` loop, reported per call. Timings are for
*ranking*, not for a datasheet: they are consistent across runs to within a few
percent, and every ratio quoted is large enough that the noise does not reach
it.

Already settled elsewhere and not re-argued: `is`, chained assignment,
`isinstance`, `dict.items()`, `str.join`, the six collection builtins,
`__hash__`, type annotations, dict iteration yielding pairs, chain fusion, the
DNS helper thread, `SO_REUSEPORT` existing. Where a finding overlaps
`one-way-audit.md`, it is cited and extended rather than restated.

---

## 0. The recommendations, ranked

| # | Situation | The right spelling | Does the tree use it? | Confidence |
|---|---|---|---|---|
| 1 | Naming an argument past the third in a stdlib call | keyword — and **the language cannot express keyword-only, and `spawn` takes no keywords at all** | **no** — `http.serve(addr, h, one, 8, 2)`, `http.fetch(..., null, null, 0.2)` | high |
| 2 | A short plain-text error body | `text(msg, status)` | **no** — `Router` 404/405 use `Response(...)` and ship **no `content-type`** | high |
| 3 | Reading a whole stream with a ceiling | *nothing in `io` does this* — two private loops in `std/http.oro` | n/a — the gap is the finding | high |
| 4 | Writing something `http` can parse from | `read(n)` **and** `read_until(d, limit)` — a second, unnamed tier | not stated anywhere; README gives both framings | high |
| 5 | Spending a message body | `resp.bytes()` / `.text()` / `.json()` — but **`Request` has no `bytes()`** | **no** — `io.read(req.body)` at four corpus sites | high |
| 6 | Parsing an integer off the wire | `b.scan(SET) == len(b)` then `to_int(base)` | **one of three sites** — the other two hand-roll, 1.7×–2.7× slower | high |
| 7 | Generating the `Date` header | a one-second memo | **no** — recomputed per response, 5.6 µs, 23% of `write_response` | high |
| 8 | Serving on N cores | `serve(addr, handler, reuseport=true)` | **impossible** — `serve` has no such argument; scale-out drops to raw `net` | high |
| 9 | Coordinating tasks | channel to *hand over*, shared dict/field to *observe*, `join` for lifetime | **yes**, exactly — and the safety rests on an invariant nothing marks | high |
| 10 | Handing a CPU turn to a peer | `yield_now()` | **yes** — `time.sleep(0)` appears once, as an oracled return-value test | high |
| 11 | Turning a value into text for another program | `json.stringify(v)` | yes — but Oro's `repr` is one character class away from JSON | medium |
| 12 | Deciding free function vs method for a new stdlib name | **there is no stated rule** | n/a — §12 proposes one | high |
| 13 | Marking something private in a stdlib module | `_` — which is **enforced for `_io`/`_json`/`_pct` and advisory for everything else** | mixed; two `Router` methods are public by accident | medium |
| 14 | Setting a socket option | constructor keyword if it must precede `bind`; method otherwise | yes — but `set_nodelay` has **zero callers in the whole tree** and is **missing from the freeze list** | high |

Sections 15 and 16 record what was checked and found *right*, because in an
audit whose standard is "one way", a confirmed rule is worth as much as a broken
one.

---

## 1. Reading: five spellings, five situations, and one situation with no spelling

The five spellings, and what each guarantees:

| Spelling | Guarantee | Cost |
|---|---|---|
| `r.read(n)` | 1..n bytes, `b""` at EOF. One syscall's worth. | you must loop |
| `io.read(r, n)` | exactly `n`, or `EOFError` | holds `n` bytes |
| `io.read(r)` | everything to EOF | holds the whole stream, **no ceiling** |
| `r.read_until(d, limit)` | up to and including `d`, `ValueError` at `limit` | Rust readers only (§2 below) |
| `io.copy(dst, src)` | everything, `src`→`dst`, returns the count | holds one 64 KiB chunk |

These are not five spellings of one job. `one-way-audit.md` §13 already settled
`io.read(r, n)` versus `r.read(n)` — *"one is a syscall's worth and the other is
a loop with an `EOFError`"* — and that is right. The tree proves the split is
real rather than rhetorical at a place worth recording, because it is exactly
the sort of asymmetry an audit is tempted to "fix":

```python
io.buffer(b"abc").read(0)      # ValueError: read() size must be at least 1, not 0
io.read(io.buffer(b"abc"), 0)  # b''
```

Same number, two answers, and **both are correct**. `read(0)` on the raw
primitive would be indistinguishable from EOF, so §2 of the design doc closed it
by fiat. `io.read(r, 0)` is the whole-job function being asked for exactly zero
bytes, which is `b""` without touching the stream and can never be confused with
anything. `corpus/divergence/40_io.oro:40` pins it deliberately. Do not
"reconcile" these.

So the map by situation:

| Situation | Right spelling |
|---|---|
| a whole file | `f = open(p, "r")` then `io.read(f)` |
| a whole file, as text | `io.read(f).to_str()` |
| a body of known length | `io.read(body, n)` |
| a line, or a header block | `r.read_until(b"\n", limit)` |
| pump one stream into another | `io.copy(dst, src)` |
| **a whole stream with a ceiling** | **nothing** |

### 1a. The missing one, and the argument it falsifies

`std/http.oro` needs the last row twice, and writes it out by hand twice:

```python
# std/http.oro:2376 — _read_capped, for `fetch`'s max_body
def _read_capped(r, limit):
    if limit == null:
        return io.read(r)
    parts = []
    got = 0
    chunk = r.read(_STREAM_CHUNK)
    while chunk != b"":
        got = got + len(chunk)
        if got > limit:
            raise BadResponse(f"the response body is larger than max_body ...")
        parts.append(chunk)
        chunk = r.read(_STREAM_CHUNK)
    return parts.join(b"")

# std/http.oro:1145 — _drain_loop, for an unread request body
def _drain_loop(body):
    left = _MAX_DRAIN
    chunk = body.read(left)
    while chunk != b"":
        left = left - len(chunk)
        if left < 1:
            return false
        chunk = body.read(left)
    return true
```

Two hand-rolled read loops in one file, both doing "read everything, stop at a
ceiling", differing only in whether they keep the bytes and whether they raise
or answer a sentinel. (The raise/sentinel difference is *correct* and
`one-way-audit.md` §8 explains why: `_drain`'s caller is sentinel-driven and
`fetch`'s caller wants the exception. The duplicated loop is the problem, not
the two conventions.)

This falsifies a stated argument, which is the strongest kind of finding this
project recognises. `docs/stdlib-server-design.md` §2 and §7 item 5 declined a
cap on `io.read(r)` on a *structural* ground:

> the unbounded-input risk is not there; it is on sockets, and socket reads go
> through `io.read(r, n)` with an `n` the server chose, or through
> `read_until(delim, limit)` with a limit the server chose.

§7 item 5 then hedges the same argument — *"it quietly assumes the path is
trusted"* — and names the server-serving-uploads case. But the case that
actually arrived is not that one. It is the **client**: `fetch` reads a stream
whose length is chosen by a remote service, and neither escape hatch applies
(the length is not known in advance, so `io.read(r, n)` is unavailable; the body
is not delimited, so `read_until` is unavailable). The module's own answer was
to write the loop privately and document `max_body` as a parameter of `fetch`.

**Verdict: add a `limit=` to `io.read`**, spelled with the word the language
already uses for a ceiling that raises:

```python
io.read(r, n=null, limit=null)     # exactly n; or everything, up to `limit`
```

`limit` raises `ValueError` when exceeded, exactly as `read_until(d, limit)`
does, and `std/http.oro` translates that into `BadResponse` in the one place
that cares — the same division of labour `_pct.decode` already uses (the codec
answers, Oro decides which exception the answer becomes).

The ergonomics test, at the real call site:

```python
resp.body = _read_capped(resp.body, max_body)   # today: a 14-line private helper
resp.body = io.read(resp.body, limit=max_body)  # after
```

and the drain, which does not want the bytes but is bounded by `_MAX_DRAIN`
(65,536 — exactly one chunk, so holding them costs nothing):

```python
try:
    io.read(body, limit=_MAX_DRAIN)
    return true
except ValueError as e:
    return false
```

The cost, named: `io` goes from three names to three names and four arguments,
and `docs/stdlib-server-design.md` §7 is right that *"a module this small is
only defensible if it stays this small"*. Against that: the alternative is that
every program which reads an untrusted stream hand-rolls this loop, and the
hand-rolled version is the one that gets the `got > limit` check on the wrong
side of the append.

*Confidence: high on the gap, medium on `limit=` being the best spelling.* The
honest competitor is `io.copy(dst, src, limit=null)`, which fixes the drain more
naturally and the `fetch` case less naturally. I prefer `io.read` because the
situation people describe is "read this body, but not more than", and the noun
in that sentence is the read.

### 1b. Whole-file text, and the style note nobody broke

The design doc asks that `open()` be bound to a name rather than chained
(`io.read(open(p, "r"))`), because in a refcounted language *where the variable
lives is when the file is open*. The tree obeys this: of the 20 `open()` calls
in `.oro` files, exactly two chain off the call, and both are error probes in
`corpus/divergence/38_streams.oro:80` and `:85` where the stream is meant to die
immediately. That rule is in good shape.

One small thing nobody decided: **`open(p)` works, with no mode.**

```python
f = open(p)     # -> a Reader. corpus/divergence/31_file_scope.oro:16 and :22
```

The freeze list says *"`open(path, mode)` with `"r"` / `"w"` / `"a"`"*, the
README writes the two-argument form everywhere, and the design doc's entire
argument about mode letters assumes one is always given. A defaulted `"r"` is a
fourth spelling that arrived without an argument for it, used twice in the tree,
in a call that freezes at 1.0. Either decide it (it is defensible — "read" is
the overwhelmingly common case and Python defaults it too) or remove it. Do not
freeze it by accident.

---

## 2. The io protocol has two tiers and says it has one

This is the most load-bearing item on the freeze list — §7 calls it *"the
highest-stakes item"* — so it is worth being exact about what it actually is.

The protocol as stated, in the README, in `std/io.oro`'s header comment, and in
design doc §2:

```
read(n)   -> bytes      # 1..n bytes; b"" at EOF; MAY return fewer
write(b)  -> null       # writes all of b, or raises
```

*"That is the entire protocol… Any object with a `read` method of that shape is
a Reader."*

It is not the entire protocol, and a six-line class demonstrates it:

```python
class PlainReader:                      # a Reader by the stated definition
    def __init__(self, b):
        self.b = io.buffer(b)
    def read(self, n):
        return self.b.read(n)

io.read(PlainReader(b"abc"))            # 3      — works
io.copy(io.buffer(), PlainReader(b"abc"))  # 3   — works
http.read_request(PlainReader(HEAD))    # AttributeError: 'PlainReader' object
                                        # has no attribute 'read_until'
```

There are two tiers, and every consumer in the tree picks one:

| Tier | Methods | Who requires it |
|---|---|---|
| **Reader** | `read(n)` | `io.read`, `io.copy`, `_LimitReader`'s source, `_read_capped`, `_drain_loop`, `_write_chunked` |
| **Framed reader** | `read(n)` + `read_until(d, limit)` | `http.read_request`, `http.read_response`, `_ChunkedReader`, `serve_conn` |

The second tier is supplied by exactly three types, all of them Rust: `File`,
`TcpStream`, `Buffer`. Nothing in Oro can join it without writing a second
`read_until` — which is precisely the outcome design doc §2 says the
one-protocol decision exists to prevent (*"then the language has two of them
with two sets of edge cases"*).

The tree already knows this and works around it silently. Both corpus test
doubles that feed the HTTP layer define a forwarding `read_until`:

```python
# corpus/divergence/44_http_body.oro:34, and 46_http_serve.oro:32
def read_until(self, delim, limit):
    return self.b.read_until(delim, limit)
```

And the documentation contains both framings, four hundred lines apart in one
file. README's io-protocol section: *"has the same two methods, and there are
only two"*. README's `open` entry, further down: *"The stream has `read(n)`,
`write(b)`, `read_until(delim, limit)` and `close()`"*. `std/http.oro`'s own
header says `serve_conn` *"is served by handing it anything that is a Reader and
a Writer"*, which is false.

**Verdict: name the second tier, do not widen the first.** Widening is the
tempting move and it is wrong for the reason §2 already gave. What is missing is
two sentences, in the README next to the protocol and in `std/http.oro`'s
header:

> `read(n)` is the whole protocol, and `io.read`/`io.copy` need nothing more. A
> *framed* reader additionally has `read_until(delim, limit)`, which cannot be
> written over `read(n)` — it has to see inside the reader's buffer — so only
> the three Rust stream types are framed readers. Anything that parses a
> delimited protocol (`http.read_request`, `http.read_response`) needs a framed
> reader. If you have bytes and need one, that is what `io.buffer()` is for.

That last sentence is the ergonomics answer, and it is a good one: the situation
"I want to feed the HTTP parser something I made up" has a one-call spelling
already, and it is the spelling all 67 `io.buffer(...)` uses in the tree take.
Nobody should
be hand-writing tier two. The two corpus doubles that do are doing it to
intercept `read`, not because they wanted a `read_until`.

*This is also the one item where I would re-check the freeze list.* §7 freezes
`read_until` as *"a method on every reader"*. As shipped it is a method on every
**Rust** reader, and the difference is the whole of this section.

---

## 3. `Request` and `Response` disagree about how to spend a body

`Response` has three accessors and a comment claiming `Request` has the same
three:

```python
# std/http.oro:438
    # The three ways to spend the body, and they are the same three
    # `Request` has, for the same reason: a caller should not have to know
    # whether it is holding bytes or a Reader to read it once.
    def bytes(self):        # :441
        if type(self.body) == bytes:
            return self.body
        return io.read(self.body)
    def text(self):         # :446
    def json(self):         # :449
```

`Request` (`std/http.oro:375`) has `text()` at :391 and `json()` at :394, and
**no `bytes()`**. The comment is wrong about the file it is in.

So the situation "get this message's body as bytes" has two spellings, and which
one is right depends on which of two near-identical classes you are holding:

```python
resp.bytes()          # a Response
io.read(req.body)     # a Request — because there is nothing else
```

The tree splits accordingly, and it is not a clean split. `io.read(req.body)`
appears at `corpus/divergence/43_http_request.oro:53`,
`44_http_body.oro:49` and `:83`. But `io.read(resp.body)` — where `resp.bytes()`
exists and the module advertises it — appears at
`corpus/divergence/61_http_client.oro:187` and `:211`. The `io.read` habit
learned on `Request` leaks onto `Response` in the corpus file whose whole job is
to demonstrate the client.

**Verdict: `Request` gets `bytes()`, three lines, and the comment becomes
true.** It is not a wrapper for `io.read` — it is the same dispatch `Response`
does, and a request body *can* be bytes in one direction: `write_request(w,
method, target, headers, body)` takes bytes or a Reader, so a `Request` built by
a program (as at `corpus/divergence/61_http_client.oro:156`) can hold either.
The rule underneath is the one §12 below states: the object that knows which of
two shapes it is holding is the object that should answer the question.

Then the rule for callers, which nothing states:

> `msg.bytes()` / `.text()` / `.json()` for a message body. `io.read(r)` is for
> a stream you are holding directly, not for a body attached to a message.

### 3a. And `bytes()` means two things

`Buffer.bytes()` and `Response.bytes()` share a name and do not share a meaning:

```python
q = io.buffer(b"hello")
q.bytes()    # b'hello'   — what is written and not yet read. Does NOT consume.
q.read(5)    # b'hello'
q.bytes()    # b''

resp.bytes() # the whole body, CONSUMING the underlying Reader if it is one
```

Two nouns spelled the same, on two types a program handles in the same
paragraph — `resp.body` is frequently a `Buffer`, so `resp.bytes()` and
`resp.body.bytes()` are both legal, both return bytes, and only one of them is
what anyone wanted.

This is small and I would not cut either name; `Buffer.bytes()` is the older
and better claim on it (it is the accessor for an in-memory byte store), and
`Response.bytes()` is the one that should move. **`Response.body_bytes()` is
ugly.** The honest alternative is to let `.text()` and `.json()` stand as the
two accessors anyone actually calls and spell the third `io.read(resp.body)` —
which is what §3 just argued against. I do not have a spelling I am happy with
here, and I am flagging the collision rather than resolving it. *Confidence:
high that the collision is real, low that I know the fix.*

---

## 4. The router's 404 has no `content-type`, because of a spelling choice

Three ways to build a `Response`, and the rule is clear and almost perfectly
followed:

| Spelling | Situation |
|---|---|
| `http.text(s, status=200)` | a plain-text body — sets `content-type: text/plain; charset=utf-8` |
| `http.json_response(v, status=200)` | a JSON body — sets `content-type: application/json` |
| `http.Response(status, headers, body, ...)` | everything else: a bodiless status, a streamed body, any other content type |

The two helpers exist because they set the one header you would otherwise
forget. That is the whole of their job and it is a good job. Outside `std/`, the
tree calls `http.text` 19 times and `http.json_response` 8 times, and reaches
for `http.Response(...)` 11 times — and every one of the 11 is a case the
helpers genuinely do not cover: `Response(204)`,
`Response(200, {"content-type": "text/plain"}, io.buffer(...))`,
`Response(200, {"x-echo": "a\r\nx-injected: yes"}, b"")`. Not one call site in
the corpus or the examples gets this wrong.

`std/http.oro` constructs a `Response` directly three times outside the two
helpers. One is right — `read_response` at `:2133` builds a response off the
wire, which is neither text nor JSON. The other two are the standard library
breaking its own rule:

```python
# std/http.oro:1529
            return Response(405, {"allow": allowed.join(", ")}, b"method not allowed\n")
# std/http.oro:1530
        return Response(404, {}, b"not found\n")
```

Those are plain-text bodies. `text()` is the spelling. The consequence is
visible on the wire — here is a router 404 and, for contrast, a parser error
built by `_error_response` (`std/http.oro:1156`), which *does* call `text()`:

```
HTTP/1.1 404 Not Found              HTTP/1.1 505 HTTP Version Not Supported
content-length: 10                  content-type: text/plain; charset=utf-8
connection: close                   content-length: 31
date: Fri, 11 Sep 2026 …            connection: close
                                    date: Fri, 11 Sep 2026 …
not found
                                    unsupported HTTP version '9.9'
```

The same server answers two errors in the same shape and labels one of them.
A browser shown the unlabelled one sniffs the body; RFC 9110 says a sender
*should* send `Content-Type` for any representation. This is not a style nit:
it is a wire-visible defect, it is in the one file everyone will copy, and the
cause is that two lines chose the constructor where the module's own helper was
right.

**Verdict: `std/http.oro:1529-1530` become `text(...)`.** The 405 keeps its
`allow` header, which means the helper needs it merged rather than replaced:

```python
resp = text("method not allowed\n", 405)
resp.headers["allow"] = allowed.join(", ")
return resp
```

Two lines instead of one, and an argument for a third helper shape
(`text(s, status, headers)`) that I would **decline** — `Response.headers` is a
plain dict and setting a key on it is the language's one way to set a header.
Adding a headers argument to `text` would make the same job spellable twice.

Then the rule, stated so the next person does not have to infer it from 29 call
sites:

> Build a response with `text()` or `json_response()` whenever the body is text
> or JSON. Reach for `Response(...)` only when you are choosing a content type,
> a body shape or a status the two helpers do not cover — and if you find
> yourself writing `{"content-type": "text/plain"}` by hand, you wanted `text()`.

---

## 5. There is no way to spell a keyword-only parameter, and the standard library needs one

This is the finding I would act on first, because it is the only one whose
window closes with the freeze.

### 5a. The rule already exists — for the Rust half only

`docs/stdlib-server-design.md` §4 argues `net.listen`'s `reuseport` into being
keyword-only, and the argument is exactly right:

> `net.listen(addr, true)` is a bare boolean whose meaning no reader can recover
> from the call site, and admitting both spellings would put two ways to do one
> thing into a language whose stated rule is that there is one.

It is enforced. `net.listen` is VM-dispatched Rust that hand-checks its kwargs
(`src/vm/modules.rs:273`), so there is no positional spelling to admit.

Now the same test, applied to the Oro half:

```python
# corpus/divergence/60_http_serve.oro:108
capped = spawn(http.serve, "127.0.0.1:0", handler, one, 8, 2)
# corpus/divergence/60_http_serve.oro:121
one_at_a_time = spawn(http.serve, "127.0.0.1:0", handler, full, 1)
# corpus/divergence/62_http_client_socket.oro:207
http.fetch("GET", rawbase + "/silent", null, null, 0.2)
# corpus/divergence/62_http_client_socket.oro:210
http.fetch("GET", base + "/big", null, null, _DEADLINE, 1024)
# corpus/divergence/62_http_client_socket.oro:174
http.fetch("GET", base + "/text", {"x-custom": "sent"}, null, _DEADLINE)
```

`8` is `max_conns` and `2` is `max_requests`. `0.2` is `timeout` and `1024` is
`max_body`, reached by padding two arguments with `null`. And 56 lines above the
last of those, in the same file, the *other* spelling:

```python
# corpus/divergence/62_http_client_socket.oro:151
http.fetch("GET", base + "/hello/ada", params={"q": "a&b", "n": 2})
```

One file, one function, both spellings, and the positional one is unreadable in
exactly the way §4 says a bare boolean is unreadable. This is not the corpus
being careless — it is the corpus writing the only thing that fits.

### 5b. Two things make it not fit

**Oro has no keyword-only marker.** `*args` and `**kwargs` both work; the bare
`*` separator does not:

```
def f(a, *, b=1): ...
  -> expected a parameter name after `*`, found `,`
```

So no Oro-level function — in the standard library or anywhere else — can
require that an argument be named. `http.serve`'s seven parameters,
`http.fetch`'s seven, `http.stream`'s six, `http.serve_conn`'s four and
`json.stringify`'s `indent` are all positionally callable, forever, unless this
changes before the freeze. **Adding `*` to an existing signature afterwards is a
breaking change**, which is what makes this urgent rather than merely desirable.

**`spawn` takes no keyword arguments at all.**

```rust
// src/vm/sched.rs, do_spawn
if !kwargs.is_empty() {
    return Ok(self.raise("TypeError", "spawn() takes no keyword arguments"));
}
```

and it forwards `Vec::new()` as the callee's kwargs. So even with a `*`
separator, `spawn(http.serve, addr, handler, ready, max_conns=8)` would not
parse the way it reads. `http.serve` is the one function in the standard library
whose entire purpose is to be spawned, and `spawn` cannot name any of its
arguments. That is why the 60_http_serve call sites look the way they do.

There is a second spelling available today, and the tree uses it exactly once,
by accident, for something else (`corpus/divergence/54_type_names.oro:58`):

```python
spawn(() => http.serve(addr, handler, ready, max_conns=8, max_requests=2))
```

Wrap the call in a lambda and the arguments become nameable again. It works and
it is what the corpus should be doing today. It is also a workaround whose
existence means "spawn a call" has two spellings with different capabilities —
which is the sin this document exists to find.

### 5c. Verdict

Two changes, both additive, both small, both due before the freeze:

1. **`spawn(f, *args, **kwargs)` forwards its keywords.** The plumbing is one
   line: `bind_call` already takes a kwargs vector and is being handed an empty
   one. Nothing about the `Park`/`Step` machinery changes. This is strictly
   additive — no existing call breaks.
2. **A keyword-only marker in `def`.** One token of grammar. Then the stdlib
   signatures that want it get it: `serve(addr, handler, *, ready=null,
   max_conns=…, max_requests=…, timeout=…, drain=…)`,
   `fetch(method, url, headers=null, body=null, *, timeout=…, max_body=…,
   params=…)`.

The ergonomics test, at the two worst call sites in the tree:

```python
spawn(http.serve, "127.0.0.1:0", handler, one, 8, 2)                       # today
spawn(http.serve, "127.0.0.1:0", handler, one, max_conns=8, max_requests=2) # after

http.fetch("GET", base + "/big", null, null, _DEADLINE, 1024)              # today
http.fetch("GET", base + "/big", timeout=_DEADLINE, max_body=1024)         # after
```

Two `null`s disappear from the second, which is the tell: an argument you have
to *skip* is an argument that should have been keyword-only.

**And a note on the seven parameters themselves**, since the brief asks. Seven
is not the problem and I would not cut any of them. `ready` is the shutdown
handle and the only way to learn a `:0` port; `max_conns`, `max_requests`,
`timeout` and `drain` are a deployment's four numbers, and every one of them is
a number an operator has a reason to change. What is wrong is that they are
positional, and the module's own docstring shows only the two-argument form —
so the first person who needs a third argument reads six lines of signature and
counts commas. One keyword marker makes seven parameters read as well as two.

---

## 6. `serve` versus owning the listener, and the scale-out path that skips the server

Two ways to write an Oro HTTP server, and the tree, the README and the design
doc all show both:

```python
# (a) std/http.oro:1374
http.serve(addr, handler)

# (b) README.md:1493 — the scale-out program
ln = net.listen("0.0.0.0:8080", reuseport=true)
while true:
    spawn(handle, ln.accept())
```

These are not two spellings of one job — (b) in the README is a raw-`net` demo
that does not speak HTTP — but they *are* the two answers to "run a server", and
the situation that picks between them is the wrong one. `serve` calls
`net.listen(addr)` at `std/http.oro:1375` with no `reuseport`, and has no
argument to pass one through. **`reuseport=true` appears in zero `.oro` files in
the repository.** The entire scale-out story — the one the README gives a
section, a systemd unit and a `for` loop to — is unreachable through the
standard library.

The README says so, to its credit (`README.md:1526`): *"`serve` calls
`net.listen` for you and has nowhere to pass `reuseport=` through"*. But look at
what the honesty costs. To serve HTTP on N cores today you drop from `serve` to
raw `net`, and you give up, all at once: the graceful drain, the connection cap,
the accept-error backoff, the idle/request timeout split, the 400/500 mapping,
the `task failed` suppression in `_conn_task`, and the shutdown tally. Nine
hundred lines of judgement, discarded to get one socket option.

The intermediate seam exists — `serve_conn(conn, handler, max_requests, watch)`
— but it is not a server: it has no registry, no drain and no accept loop, so
writing (b) on top of it means reimplementing `_Live`, `_conn_task` and
`_drain_live` by hand.

**Verdict: `serve` takes `reuseport=false`, keyword-only, and passes it
through.** One parameter, one line, and it makes the README's section true
rather than apologetic:

```python
http.serve(addr, handler, reuseport=true)     # N copies, one port
```

This is explicitly *not* `workers=N`. Design doc M6 already argued that one down
— a language runtime should not be starting processes — and that argument
stands. The change here is the opposite of a launcher: it is the one option that
lets the operating system's launcher work.

*Note the dependency:* keyword-only requires §5. Passed positionally as the
eighth argument, `reuseport` would be exactly the bare boolean §4 refused.

---

## 7. Concurrency: four mechanisms, four jobs, and an invariant nothing marks

The brief asks whether `spawn`+`join`, channels, or a shared dict is the right
coordination mechanism, and whether `serve`'s unlocked `live = {}` registry is
the right answer. `serve` uses **all four** mechanisms, and after reading it
closely the answer is that they are not competing — they are layered, each doing
one job that the others cannot:

| Mechanism | `serve`'s use | The job |
|---|---|---|
| channel | `ready.send(ln)` (`:1380`) | **hand over** one object across a task boundary, once |
| shared dict | `live[conn] = entry` (`:1429`), `live.pop` (`:1463`) | a **registry** two parties mutate and one counts |
| object field | `entry.busy`, `entry.stopping` | a per-connection **flag**, written by one side and read by the other |
| `task.join()` | `entry.task.join()` (`:1490`) | **lifetime** — wait for completion |

That gives the rule, and nothing in the tree or the docs states it:

> A **channel** when something is handed over and the sender is done with it. A
> **shared dict or a field** when something is observed — when the writer keeps
> it and the reader wants to know its current value. **`join`** when what you
> want is not a value but the fact that a task is finished.

The tree obeys this without exception, including in the corpus's concurrency
files. That is a genuine pass and worth recording as one.

### 7a. The shared dict is not a choice — it is what "no `select`" leaves

The interesting part is *why* the registry cannot be a channel, and the answer
is a decision made in a different section of a different document.

A channel-based drain is the obvious design: every `_conn_task` sends "done"
down a channel, and `_drain_live` receives `waiting` of them. It cannot be
written, because the drain is **bounded**:

```python
# std/http.oro:1480
    while len(live) > 0 and time.monotonic() < deadline:
        time.sleep(_DRAIN_TICK)
```

Channels have exactly three methods — `send`, `recv`, `close`
(`src/vm/mod.rs:5916`) — with no `try_recv` and no timeout, and there is no
`select`. So "wait for N completions, or five seconds, whichever comes first"
has **no channel spelling at all**. A shared dict polled against a
`time.monotonic()` deadline is not one option among several; it is the only
thing that can be written.

That is worth stating plainly because §3 of the design doc presents "no
`select`" and "a cache is a `dict`" as two independent observations, and they
are one decision seen twice. The deciding question §3 leaves open — *"is there a
real program in the first year that must wait on two independent sources at
once?"* — has an answer in this tree: `_drain_live` waits on completion **and** a
deadline, and it pays for the missing `select` with a 10 ms poll. I still think
**not shipping `select` is right** — the poll is five lines, the granularity is
a tunable constant, and `select` is additive later. But the cost is now
measurable in the standard library rather than hypothetical, and §3's entry
should say so.

### 7b. The cost of "safety is free" is an invariant with no syntax

§3's best line is that inside one VM there is no parallelism, so tasks share
plain mutable state with no locks. That is true about **data races**. It is not
true about **atomicity across a suspension**, and `std/http.oro` depends on the
difference in a comment:

```python
# std/http.oro:1456
    finally:
        # In a `finally` because the registry is what `serve`'s drain counts,
        # and a connection that leaves it behind is one a shutdown waits the
        # whole drain for. The last statement of the task, with no park point
        # after it, so under cooperative scheduling "gone from the registry"
        # and "the task has finished" are the same instant to every other
        # task in the VM.
        entry.conn.close()
        live.pop(entry.conn, null)
```

The correctness of `serve`'s shutdown accounting rests on there being **no park
point** between `live.pop` and the task's last instruction. Insert any call that
can park — a log line to a socket, a `time.sleep`, a channel send, a
`task.join`, or (less obviously) an `import`, which parks on the import
rendezvous — and the drain's tally silently goes wrong.

The park points are: I/O on a stream, `accept`, `net.dial`, channel `send`/
`recv`/iteration, `Task.join`, `time.sleep`, `yield_now`, and `import`. Nine
`Park` variants in `src/vm/sched.rs`. Design doc §3 lists four of those
categories; the README lists none. And the whole point of green threads here is
that **there is no function colouring** — no `await`, no marker, nothing at a
call site that says "this one can suspend". That is the feature. It is also why
this invariant cannot be seen by reading the code.

This is a real, named cost of the model that neither document states, and it is
not an argument against the model. It is an argument for one paragraph:

> There are no data races inside a VM, so shared mutable state needs no lock.
> What it does need is that any sequence which must be atomic contains no park
> point — Oro has no `await` to mark one, so the list is: stream I/O, `accept`,
> `net.dial`, channel `send`/`recv`/iteration, `Task.join`, `time.sleep`,
> `yield_now` and `import`. If a sequence of mutations must be seen together,
> keep it free of those.

Put it in the README next to the concurrency surface. It is the one thing a
programmer needs that the model's own simplicity hides.

### 7c. `yield_now()` versus `time.sleep(0)` — the tree gets this right

Both spellings exist and both hand over the CPU. They are not the same
mechanism:

| | mechanism | cost |
|---|---|---|
| `yield_now()` | `Park::Yield`, requeued at the back of the ready queue with its `null` already pushed (`src/vm/sched.rs:1032`) | a `push_back` and a `pop_front` |
| `time.sleep(0)` | a deadline of *now* armed in the reactor's timer heap (`src/vm/sched.rs:2017`) | a heap insert, a `poll` with a zero timeout, and a sweep |

`yield_now()` is right, for a reason beyond cost: a yielding task is still
*runnable*, so it can never deadlock and never depends on the reactor existing.
A `sleep(0)` task is *parked*, and only the reactor's sweep makes it runnable
again.

**The tree already uses the right one.** `yield_now()` appears 11 times
including the load-bearing one at `std/http.oro:1478`; `time.sleep(0)` appears
once, at `corpus/core/29_time.oro:20`, as an oracled test that it returns
`null`. Nothing in the tree uses `sleep(0)` as a yield. No change needed — one
README line ("to hand over the CPU, `yield_now()`; `time.sleep(0)` does it too,
through the timer list, and is the slower way to say it") would close it
permanently.

### 7d. `spawn` + `join` versus a channel for a result

One more pairing the brief implies. `t = spawn(f); v = t.join()` and
`ch = chan(); spawn(worker, ch); v = ch.recv()` both get a value out of a task.
`join` is right when the task produces **one** result and the joiner wants it;
a channel is right when the task produces a **stream** of them, or when the
receiver is not the spawner. The tree follows this: `corpus/divergence/47_spawn.oro`
uses `join` for single results, `48_channels.oro` uses channels for
producer/consumer, and `examples/client.oro:113` uses `tally = srv.join()` for
the one value `serve` returns. No disagreement found. Worth a sentence in the
README for the same reason as the rest.

---

## 8. Three spellings of "parse an integer off the wire", and the fastest is already in the file

`one-way-audit.md` §12 noticed that `std/http.oro` spells "is this all digits"
three ways because `str.is_digit()` follows Unicode and is therefore wrong for a
numeric-parse test. That is right, and it stops one step short of the finding.

The three sites:

```python
# std/http.oro:820 — _content_length, on a str
    for ch in s:
        if ch not in "0123456789":
            fail(f"malformed Content-Length '{s}'")
    return s.to_int()

# std/http.oro:948 — _ChunkedReader.chunk_size, on bytes, with a dict per byte
        size = 0
        for c in head:
            v = _HEX_VALUE.get(c)
            if v == null:
                self.fail("malformed chunk size")
            size = size * 16 + v

# std/http.oro:1966 — _port, on bytes, with the primitive that exists for this
def _port(digits, url):
    if len(digits) == 0 or digits.scan(_DIGIT_OK) != len(digits):
        raise ValueError(f"'{url}' has a port that is not a number")
    n = digits.to_str().to_int()
```

The third is right and the first two are wrong, and it is worth being precise
about *why* the first two exist, because the reason is good: `to_int(base)` is
far too lenient to parse a wire format.

```python
"+20".to_int(16)    #  32
"-20".to_int(16)    # -32
" 20 ".to_int(16)   #  32
"2_0".to_int(16)    #  32
"0x20".to_int(16)   #  32
```

A chunk size of `-1` or `0x20` accepted from the wire is a request-smuggling
primitive, so the guard is not optional. But `bytes.scan` is the guard —
that is what §5 of the design doc added it for — and it is one Rust call
instead of a per-byte Oro loop. Measured, per call:

| | today | `scan` + `to_int` | |
|---|---|---|---|
| `chunk_size(b"2000")` | 3.30 µs | **1.23 µs** | 2.7× |
| `_content_length("12345")` | 1.91 µs | **1.10 µs** | 1.7× |
| bare `b"2000".to_str().to_int(16)` | | 0.44 µs | |

and the strictness is identical — `-20`, ` 20`, `2_0`, `0x20` and `+20` are all
refused by both.

The ergonomics test: nothing changes at either call site. `self.chunk_size(line)`
and `_content_length(s)` keep their signatures; only the bodies change. That is
the cheapest kind of fix there is.

**Verdict: one spelling, applied at all three sites.**

> A number arriving off the wire is validated with `b.scan(SET) == len(b)` and
> then parsed with `to_int(base)`. Never `to_int` alone — it accepts a sign,
> whitespace, underscores and a `0x` prefix, none of which any wire format
> means. Never a per-byte loop — `scan` is the primitive that exists for this.

`chunk_size` also needs a `_HEX_OK` constant beside `_DIGIT_OK`, which is one
line, and `_content_length` needs a `.to_bytes()` because `scan` is bytes-only —
still 1.7× faster with the allocation included, and the freeze list is explicit
that `scan` is *"`bytes` only, not `str`"*.

*Two smaller notes in the same neighbourhood.* `_escape_fault`
(`std/http.oro:819`) is also a per-byte-ish loop and is **correct as written** —
its own comment explains that it runs only after `_pct.decode` has already
refused the input, so its cost falls on malformed requests and nobody else. And
design doc §6's sketch of `_ChunkedReader` says *"`to_int(16)`"* plainly; the
implementation that shipped hand-rolled the loop instead and nothing recorded
why. The `to_int` leniency above is the why, and it belongs in a comment at
`:948`.

---

## 9. The 30% threshold fired a third time, and the answer is not Rust

Design doc §5 has been wrong twice about where the Rust/Oro line falls — `json`
placed by analogy and shipping at 95× CPython, and percent-decoding sitting on
the server's per-request path at ~100×. The brief asks for a third. I profiled
for one and found something better: a place where the threshold fires and
**neither** side of §5's rule is the answer.

Per-request CPU in `std/http.oro`, on a realistic browser request head (eight
headers, a long `User-Agent`, a long `Accept`, a cookie, a two-parameter query
— 430 bytes):

| | per request |
|---|---|
| `read_request` | 62.7 µs |
| `write_response` (fresh `Response`, plain-text body) | 24.3 µs |
| of which `_http_date(time.time())` | **5.6 µs** |
| `time.time()` alone | 0.28 µs |
| `http.text("hello world\n")` alone | 1.72 µs |

**`_http_date` is 23% of the response write path**, and it is recomputed from
scratch for every single response (`std/http.oro:1074`, under
`if h.get("date") == null:`, which is true for essentially every response a
handler builds).

It is not a per-byte loop. It is about twenty integer divisions of Howard
Hinnant's era algorithm (`_civil_from_days`, `std/http.oro:992`) plus two
f-string formats, and by §5's rule — *anything that touches every request goes
in Oro* — it is exactly where it belongs. Moving it to Rust would be the wrong
fix and would be the third time §5 was misread.

The right fix is the one every production server uses, and it is five lines of
Oro:

```python
_date_second = -1
_date_value = ""

def _http_date_now():
    global _date_second, _date_value
    s = time.time().to_int()
    if s != _date_second:
        _date_second = s
        _date_value = _http_date(s)
    return _date_value
```

Measured: **5.61 µs → 0.49 µs**, an 11× reduction, removing 5.1 µs from every
response — about 6% of the ~87 µs the HTTP layer spends on a whole request.
`_http_date(t)` stays a pure function of its argument, so
`corpus/divergence/45_http_response.oro` and every test that pins a date keep
working unchanged; only `write_response`'s call site moves.

nginx and Go's `net/http` both cache the `Date` header to one-second
granularity, for this reason. The header is defined to one-second resolution, so
there is nothing to lose.

**The transferable finding is about §5's rule, not about dates.** The rule is a
two-way switch — Rust for per-byte, Oro for per-request — and it has no
vocabulary for *"compute it once"*. Both of §5's previous corrections moved code
across the line. This one does not move, and the rule as written offers no help
in spotting it: a per-request cost that is the same for every request in a
second is not a per-request cost at all. §5 should gain a third clause:

> Before moving a loop across the line, ask whether it needs to run. A cost that
> is per-request but whose *answer* is not per-request is a memo, and a memo is
> cheaper than either side of this rule.

*Confidence: high on the measurement and the fix, medium on ranking.* 5.1 µs of
~87 µs is real but it is not the 95× that moved `json`, and I would not want
this read as a third §5 failure of the same magnitude. It is a third *firing* of
the threshold with a different answer, which is more interesting and less
alarming.

### 9a. And a hazard the same measurement exposed

`write_response` **mutates the caller's `Response.headers` in place** — it
writes `content-length`, `connection` and `date` into the dict the handler
built. So a handler that returns a module-level constant serves a stale header
set forever:

```python
_OK = http.text("ok\n")           # a module-level Response
def handler(req):
    return _OK                     # every response carries the first one's date
```

Demonstrated: two `write_response` calls 1.1 seconds apart against one `_OK`
produce byte-identical `date:` headers. `content-length` and `connection` are
stale in the same way, and `connection` is the one that can break a keep-alive
connection outright.

This is not a redundancy, but it *is* the reason `text()` and `json_response()`
being fresh-per-call is the right spelling and a hoisted constant is the wrong
one — so it belongs with §4's rule. One sentence above `write_response`: *"this
fills in headers on the response it is given; build a fresh `Response` per
request rather than returning a shared one."*

---

## 10. `json`: one converter, and a lookalike that is closer here than in Python

`json.parse(text)` / `json.stringify(value, indent=null)` are the whole surface,
and `std/json.oro` is 69 lines of which four are code — two forwards into
`_json`. The `indent=` argument instead of a second `stringify_pretty` is the
canonical instance of the language's own rule (*a parameter changes what a
function does; a second name is for when it changes what the caller must do
next*), and nothing in the tree disagrees with it.

Asked whether anything else converts between Oro values and text, the honest
inventory is:

| Direction | Spellings |
|---|---|
| value → text | `json.stringify(v)`, `repr(v)` / `f"{v}"`, `x.to_str()`, `http.encode_query(d)`, `_http_date(t)` |
| text → value | `json.parse(s)`, `s.to_int()` / `.to_float()`, `http.parse_url(u)`, `http.unquote(s)` |

Only one pair overlaps, and Oro made it worse than Python did:

```python
d = {"ok": true, "n": 2, "who": "ada", "f": 1.5, "z": null, "xs": [1, 2]}
f"{d}"              # {'ok': true, 'n': 2, 'who': 'ada', 'f': 1.5, 'z': null, 'xs': [1, 2]}
json.stringify(d)   # {"ok":true,"n":2,"who":"ada","f":1.5,"z":null,"xs":[1,2]}
```

In CPython, `str(d)` gives you `True`/`None` and the mistake announces itself the
first time anything tries to parse it. In Oro, the lowercase-literal divergence
means a dict's `repr` differs from valid JSON by **exactly one character
class** — the quote. Everything else already matches: `true`, `false`, `null`,
the float rendering, the list syntax. The corpus leans on this
(`corpus/divergence/61_http_client.oro:158` asserts `f"{resp.json()}"` equals
`"201 {'ok': true}"`), which is correct for a test and is also the habit that
produces `resp.body = f"{d}".to_bytes()` in somebody's handler.

**Verdict: nothing to cut, one line to write.** They are different jobs — a
rendering for a human, an interchange format for a program — and the rule is:

> `json.stringify(v)` whenever anything other than a person will read the
> result. `repr(v)` and `f"{v}"` are renderings, not formats; in Oro they happen
> to be one quote character away from JSON, and that is a coincidence of the
> `true`/`null` literals, not a promise.

*One thing I expected to find and did not:* a second JSON-ish encoder somewhere
in `std/http.oro`. There is none — `json_response` calls `json.stringify` and
that is the only path. Good.

---

## 11. `net`: the knobs, the one nobody calls, and the address decision

`net` is two constructors and two objects, and after reading every call site I
would change nothing about its shape. Three observations.

### 11a. Constructor keyword or setter method — the rule is right and unstated

```python
net.listen(addr, reuseport=true)    # a keyword on the constructor
conn.set_timeout(30)                # a method afterwards
conn.set_nodelay(true)              # a method afterwards
```

Two ways to configure a socket, and which is right is fully determined:

> An option that must be set **between `socket(2)` and `bind(2)`** is a
> constructor keyword, because there is no moment afterwards in which to set it.
> Everything else is a method, because it can change during the socket's life
> and a constructor argument would freeze it.

`SO_REUSEPORT` is the first kind (design doc §4 documents the four syscalls and
the `unsafe` block this costs). `set_timeout` and `set_nodelay` are the second —
both are meaningfully changed mid-connection, and `serve` in fact calls
`conn.set_timeout` twice per request with two different values
(`std/http.oro:1424` and the idle/request split at `:1167`). The rule holds
across the whole module and nothing states it. One sentence in the README's
`net` entry, and the next socket option has an answer before anyone argues.

### 11b. `set_nodelay` has zero callers and is not on the freeze list

`set_nodelay` appears in `README.md:952`, in design doc §4's sketch, and in
`src/net/tests.rs`. It appears in **no `.oro` file anywhere in the repository** —
not in `std/http.oro`, not in `examples/`, not in a single corpus program.

It is also absent from design doc §7's frozen list, which names *"`net.listen` /
`net.dial` / `accept` / `peer` / `local` / `set_timeout` / `shutdown_write`"* and
stops. That looks like an omission rather than a decision: `shutdown_write` is
there and `set_nodelay` is not, and nothing anywhere says why.

The situation that wants it is real and is in this tree. `write_response` sends
head and body in one `write`, which is Nagle-safe, but `_write_chunked`
(`std/http.oro:1102`) writes one 8 KiB chunk at a time and then a five-byte
terminator — a small write immediately following a large one, which is the
textbook 40 ms delayed-ACK stall.

**Verdict: force the decision before the freeze.** Either `std/http.oro`'s
`serve` calls `conn.set_nodelay(true)` on accept (which is what every HTTP
server does, and which would make the knob's existence self-evident), or
`set_nodelay` comes out. A frozen method with no caller in its own standard
library is a name nobody chose.

*Confidence: high that the decision is owed, medium on which way it goes.* I did
not measure the chunked stall — doing it properly needs two machines, and on
loopback Nagle is invisible. I would default it on in `serve` and say so, on the
grounds that a server that batches its own writes has nothing left for Nagle to
coalesce.

### 11c. Addresses as strings: right, and now parsed twice

`"host:port"` with Go's bracket form is the right call and design doc §7 item 7
already records the risk (*"if IPv6, Unix sockets and TLS SNI all arrive, string
parsing shows up in three places"*). It has arrived in two:

- Rust: `one_addr` / `plan_dial` (`src/vm/modules.rs:302`, `src/net.rs`).
- Oro: `_split_host_port` (`std/http.oro:1940`), `_unbracket` (`:1957`),
  `_port` (`:1965`) and `_bracket` (`:1885`), which exists to *re-*assemble the
  string that `net.dial` will take apart again.

They agree — checked directly, `parse_url("http://[::1]:8080/x").authority()`
round-trips to `"[::1]:8080"`, which is what `net.dial` wants — but they agree
by hand, and nothing tests that they agree. `Url.authority()` (`std/http.oro:1863`)
un-parses a host and port back into the exact string form the Rust parser
expects, which is a contract written in two languages and enforced in neither.

I would **not** add an `Address` type. The string form is proven, the
alternative means two spellings forever, and the parse is genuinely rare. What
is cheap and missing is a corpus file that asserts the round trip across the
boundary for the awkward cases — bracketed IPv6, a bare host with no port, a
port of `0`, a host that is an IPv4 literal. That is a test, not an API.

---

## 12. Free function or method? There is no stated rule, and the freeze makes it permanent

The brief's central question, and the answer is: **nothing anywhere states a
rule.** There are three good per-case arguments and no general one:

- design doc §3: *"`spawn` is a builtin, not a module member, because it is a
  control-flow construct — the same reason `print` and `len` are builtins."*
- design doc §2: *"`read_until` has to be a method rather than a free function
  because it must see inside the reader's buffer."*
- `one-way-audit.md` §2: *"`to_str()` is a method because it has a receiver."*

Three cases, three different reasons, no principle. Meanwhile the surface that
freezes at 1.0 mixes them freely: `io.read(r, n)` free, `r.read(n)` method,
`io.copy(dst, src)` free, `r.read_until(d, l)` method, `json.parse(s)` free,
`s.to_int()` method, `http.fetch(...)` free, `resp.close()` method,
`http.write_response(w, req, resp, keep)` free, `resp.bytes()` method,
`spawn(f)` builtin, `t.join()` method.

Every one of those individually defensible; collectively, a coin flip for
anyone adding the next name. And after the freeze, `io.foo(x)` and `x.foo()` are
not interchangeable — choosing wrong is choosing forever.

**Here is the rule I would write.** It was derived by testing candidate
formulations against the whole existing surface and keeping the one that
explains all of it:

> 1. **A method** when the answer depends on state only the receiver has — its
>    file descriptor, its internal buffer, its own bytes, its position, its
>    liveness. `r.read(n)`, `r.read_until(d, l)`, `t.join()`, `resp.close()`,
>    `b.scan(set)`, `x.to_str()`.
> 2. **A free function in a module** when the operation is a loop or a policy
>    written over a *public protocol*, and would work identically on any
>    implementation of it. `io.read`, `io.copy`, `http.read_request`,
>    `http.write_response`, `http.should_keep_alive`. The test is: could this be
>    written by someone who did not author the receiver's type? If yes, it is a
>    free function.
> 3. **A free function in a module** for a format or a protocol that belongs to
>    neither side. `json.parse` is not a `str` method because a `str` is not a
>    JSON document, and `json.stringify` cannot be a method because it would
>    have to be one on every type.
> 4. **A free factory in a module** to construct a Rust-backed type, which
>    cannot be called by name: `io.buffer`, `net.listen`, `net.dial`, `open`,
>    `chan`. An Oro class is constructed by its own name — `http.Router()`,
>    `http.Response(...)` — and because Oro has no classmethods, an *alternative*
>    constructor is a free function beside it: `http.text`, `http.json_response`.
> 5. **A builtin** only for control flow: `spawn`, `yield_now`, `print`, `len`.

Every existing name in `io`, `json`, `net`, `http` and the concurrency surface
fits one of the five. That is the test a rule has to pass before it is worth
writing down.

**And it earns its keep immediately, because it diagnoses two things.**

*It says `Request` should have `bytes()`* (§3). The object that knows whether
its body is bytes or a Reader is the message; clause 1.

*It explains the `_fail_*` functions.* `one-way-audit.md` §12 noticed that
`std/http.oro` parameterises "which exception does this raise" two ways — as an
argument (`_percent_decode(b, plus, fail=_fail_request, where="…")`,
`_parse_query(qs, fail=…, where=…)`, `_content_length(s, fail=…)`, with three
`_fail_*` free functions at `:361`, `:365`, `:369`) and as an overridable method
(`_HeadParser.fail` at `:598`, `_ResponseParser.fail` at `:2058`,
`_ChunkedReader.fail`, `_LimitReader.truncated`). §12 called the method form
better and left it there. The rule says *why*: `fail=` and `where=` are
**receiver state carried as arguments**, and carrying receiver state as an
argument is the definition of a function that wanted to be a method. Fold
`_percent_decode`, `_parse_query` and `_content_length` onto the parser classes
and all three `_fail_*` free functions and both keyword arguments disappear
together.

*Where I am least sure:* clause 2 versus clause 1 for `http.write_response(w,
req, resp, keep)`. It could be `resp.write_to(w, req, keep)` under clause 1,
since a `Response` knows its own body shape. I keep it free because it is a
policy over the *Writer* protocol — it works on a socket, a `Buffer` or a class
someone wrote this afternoon, and it is the exact mirror of `read_request(r)`,
which has no receiver to be a method on. But it is the closest call in the rule,
and if the rule is adopted, that pair is where to test it first.

---

## 13. The underscore means two different things

The convention: an underscored built-in module is the Rust half of a stdlib
module, resolvable only from inside a stdlib module body. `_io`, `_json`, `_pct`.
Design doc §7 puts them under "deliberately left unfrozen" and the reason is
good — *"it lets the Rust/Oro line move, as it moved for `json`, without moving
anything a program can see."*

Three inconsistencies, in decreasing order of how much they matter.

**The same sigil is enforced in one place and advisory in the other.** Checked
directly:

```python
import _io                  # ModuleNotFoundError: No module named '_io'
http._http_date(0)          # 'Thu, 01 Jan 1970 00:00:00 GMT'
http._MAX_HEAD              # 65536
```

`_io` is genuinely unreachable. Every underscored name inside `std/http.oro` —
`_HeadParser`, `_ChunkedReader`, `_MAX_CONNS`, `_http_date`, `_percent_decode`,
all of them — is reachable from user code and therefore de facto public. Since
`std/http.oro` ships provisional at 1.0 and freezes one release later, that
means the module's private surface freezes with its public one unless something
says otherwise. This is the kind of thing that is free to state now and
expensive to walk back:

> A leading underscore on a *module* is enforced by the resolver. A leading
> underscore on a name *inside* a module is a promise from the author, not a
> restriction from the runtime: reaching for one is reaching past a stated
> boundary, and nothing in `std/` that has one is covered by the freeze.

**`_pct` has no `pct`.** `_io` sits under `io`, `_json` sits under `json`, and
`_pct` sits under… `http`, whose public spellings are `http.quote`,
`http.unquote` and `http.encode_query`. So the convention "`_X` is the private
half of `X`" is true twice and false once, and the next contributor adding a
Rust codec for, say, base64 gets two different answers about what to call it.
The convention that actually holds is weaker and should be stated as what it is:
*an underscored module is a private Rust helper for the standard library, named
for what it does, not for the module that imports it.*

**Two `Router` methods are public by accident.** `Router.handler_for`
(`std/http.oro:1532`) and `Router.allowed` (`:1545`) have no underscore and are
not in the module's own API list, which names only
`http.Router().add(method, path, handler)`. Contrast `serve_conn` and
`should_keep_alive`, which also shipped public but did so *deliberately* (design
doc §6 records the reason: a connection served over a `Buffer` is how the layer
is tested, and a test cannot call a private name) — and which **are** in the API
list. The list is the authority; two names escaped it. Underscore them, or add
them to the list and mean it.

---

## 14. Smaller things, with their line numbers

**`req => routes.dispatch(req)` is a lambda that does nothing.** It is how a
router reaches `serve` at every one of the three call sites in the tree —
`examples/server.oro:96`, `examples/client.oro:64`,
`corpus/divergence/62_http_client_socket.oro:73` — and it is what the module's
own docstring shows at `std/http.oro:1295`. A bound method is a first-class
value here. Verified by running it:

```python
http.serve_conn(c, r.dispatch)          # works; served 1 request
http.serve(addr, routes.dispatch)       # the spelling
http.serve(addr, logged(routes.dispatch))  # and with middleware
```

Three sites and a docstring, one spelling, and the shorter one is the one the
language's own first-class-functions argument predicts. Unanimity in the wrong
direction is worse than a split, because it is what a model will learn.

**`Router.add` chains and nothing chains it.** `one-way-audit.md` §12 already
called this and recommended dropping the `return self`. The evidence is stronger
than that section had: the one place in the tree that uses the chain,
`corpus/divergence/46_http_serve.oro:145`, is a **191-character single line**
holding three routes. The three files that ignore it —
`examples/server.oro:75-81`, `examples/client.oro:46-50`,
`corpus/divergence/62_http_client_socket.oro:64-70` — are all readable. That is
the ergonomics test returning a verdict: **drop `return self`** (`std/http.oro:1512`),
and delete the sentence in the module docstring that advertises it.

**`http.text(s, status)` and `Response(status, ...)` order status differently.**
The helpers put it second, the constructor first. Both are defensible in
isolation (the helper's subject is the body; a response *is* a status) and the
helpers agree with each other, so this is one boundary rather than three
spellings. I would leave it and note it in the docstring, because
`http.text(404, "nope")` is a plausible mistake whose error message will be
about `.to_bytes()` on an int.

**`Router`'s composite string key.** `self.exact[method + " " + path]`
(`std/http.oro:1508`) forces `allowed()` (`:1545`) to split the key back apart
with `key.find(" ")` and two slices. A nested `{path: {method: handler}}` would
not. This is outside the spelling question and I flag it only because
`one-way-audit.md` §11 suggested f-stringing that key — which would make the
expression prettier and leave the index arithmetic in `allowed()` exactly where
it is. If it is touched, touch the data structure, not the concatenation.

---

## 15. Checked, and found right

Recorded so these are not reopened.

**`fetch` versus `stream` holds, and the argument is the right one.** The
module's claim — *"the difference is lifetime rather than an option… a flag that
changes what a caller must do afterwards is a flag that will be missed"* —
survives inspection. `stream` returns with `resp.conn` set and the socket live;
`fetch` is six lines on top of it, and those six lines are `_read_capped` and a
`finally: resp.close()`. `examples/client.oro:96-102` shows the `try`/`finally`
a `stream` caller owes and `fetch` does not, and there is no keyword argument
that could express that obligation. The generalisation
`one-way-audit.md` §11 drew from it — *a parameter changes what a function does;
a second name is for when it changes what the caller must do next* — is the best
single rule in either audit, and it is worth promoting from a paragraph in an
audit to a line in the README.

**No `io.write`, and the asymmetry is not an inconsistency.** Reading everything
requires a loop; writing everything is already guaranteed by the protocol. Both
documents say so, and I could not find a call site that wanted one.

**`io.read(r, n)` versus `r.read(n)`** — settled by `one-way-audit.md` §13, and
§1 above adds the `read(0)` evidence that the split is principled rather than
historical.

**`chan()` / `chan(n)`, `json.stringify(indent=)`** — one name with a parameter,
which is the shape. `chan(0)` runs and is an explicit synonym, as design doc §7
item 10 asked.

**The four coordination mechanisms** (§7) — layered, not competing, and the tree
obeys the layering everywhere I looked.

**`yield_now()` over `time.sleep(0)`** (§7c) — the tree already uses the right
one, unanimously.

**No `http.get`/`post`/`put`** — the method is an argument because it is an
argument, and the same argument kills every future per-verb wrapper.

**`text` / `json_response` / `Response`** — the rule is clear and followed at
every one of the 38 call sites outside `std/`. The only two exceptions in the
tree are §4, and they are in the standard library.

**Addresses as strings** — right, and the only action is a corpus file (§11c).

---

## 16. What I expected to find and did not

Worth stating, because absences are evidence too.

**A second JSON encoder.** I expected `std/http.oro` to have grown a small
value-to-text path of its own for headers or for error bodies. It has not:
`json_response` calls `json.stringify` and that is the only route.

**A second percent-codec.** After `one-way-audit.md`'s history I expected an
inlined `%HH` loop somewhere on the response side. There is one Oro loop
(`_escape_fault`, `std/http.oro:819`) and it runs only on input already known to
be malformed, with a comment saying so.

**A `for c in b` loop left on the hot path.** §5 of the design doc moved three of
them to `bytes.scan` and I expected a fourth. The three grammar checks measure
1.15 µs on a 90-byte header value, which is `scan` doing its job. The two that
remain (§8) are wire-integer parsing, are small, and are not where I expected to
find them.

**A `flush()` or a buffered writer sneaking back.** Neither exists anywhere,
including in the corpus test doubles.

**A stream type implemented in Oro that breaks the protocol.** Every `def read`
in the tree is `read(self, n)`. Nothing anywhere defines a no-argument `read()`,
a `readline`, or a `write` that returns a count.

---

## 17. If only three things happen

1. **Make keyword arguments reachable** (§5): `spawn` forwards its kwargs, and
   `def` gains a keyword-only marker. Both additive, both small, and both
   impossible to add *after* the signatures freeze. Then fix the five call sites
   that pad with `null` and count commas, and give `serve` its `reuseport=`
   (§6), which is what makes the README's scale-out section true.

2. **Fix the two lines that ship a 404 with no `content-type`** (§4), and write
   down the `text`/`json_response`/`Response` rule beside them. It is a
   wire-visible defect in the file everyone copies, caused by nothing but a
   spelling choice, and the fix is two lines plus a sentence.

3. **Write down the free-function-versus-method rule** (§12). It costs a
   paragraph, it explains every name already shipped, and without it the next
   stdlib name is a coin flip that cannot be re-flipped. Adopting it also
   settles §3 (`Request.bytes()`) and `one-way-audit.md` §12's two-mechanisms
   problem for free.

And if a fourth: the `_http_date` memo (§9). Five lines, 11× on the function,
about 6% of per-request CPU, and a third clause for §5's rule that is worth more
than the microseconds.
