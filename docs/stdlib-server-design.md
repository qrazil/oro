# Bytes, I/O, concurrency and networking

*Requirements and design for the layer that lets Oro serve HTTP. Status:
proposal. Nothing here is implemented.*

Oro today can compute. It cannot talk to anything. `open()` is UTF-8 text,
`sys.stdout` is the *string* `'<stdout>'`, there is no byte type, no socket, and
no way to do two things at once. This document specifies the layer that fixes
that, and it tries to do so in the way the README demands: pick one spelling,
state the reason, name what the choice costs, and then stop.

The target is concrete. A working HTTP server, with the HTTP itself written in
Oro on top of a small set of Rust primitives, and with the primitive set small
enough that it can be frozen at 1.0 without regret.

Four decisions are already settled and are not re-argued here, only built on:

- **Green threads, not `async`/`await`.** No function coloring. `spawn(f)` and
  channels; I/O looks blocking and parks the Oro task instead of the thread.
- **tokio is the engine and is invisible in the language.** One VM per OS
  thread, `current_thread`, nothing shared between threads. Every `Value` is
  `Rc`/`RefCell` — there is not one `Arc` in the codebase — so values are
  `!Send`. Scale out with `SO_REUSEPORT` across N single-threaded VMs.
- **Errors are exceptions**, using the existing
  `BaseException`→`Exception`→`OSError`/`ValueError` hierarchy.
- **Go's `io` philosophy**: one-method interfaces that everything composes
  through. In a dynamic language that is a naming convention and costs nothing —
  but it has to be committed to before the stdlib exists, or you get Python's
  zoo of mutually incompatible file-likes.

---

## 1. Bytes: two types, not one

### The fork

Oro has no byte type. Adding one is unavoidable — a socket delivers octets, and
pretending otherwise is how you get a server that mangles a JPEG. The question
is whether it is a *second* type or whether `str` becomes it.

**Model (a) — Python.** `str` is a sequence of Unicode scalars, `bytes` is a
sequence of octets, and the two convert explicitly. `len("é") == 1`.

**Model (b) — Go.** One string type that *is* immutable bytes. `len()` counts
bytes, indexing yields a byte, decoding UTF-8 is opt-in. `len("é") == 2`.
Strictly fewer types; the socket/string boundary disappears entirely.

Model (b) is the one the thesis appears to want. Fewer types, C as the north
star, no encode/decode ceremony, and it deletes a whole category of "which one
is this?" bugs. It deserves a serious hearing, and the case for it is stronger
than the case against it usually gets credit for. Here is the case, and then
here is why it loses anyway.

### The case for Go's model

1. **One type is genuinely simpler.** Every stdlib function that today takes a
   `str` would keep taking a `str`. `sock.read(n)` returns a string; it goes
   straight into `re.search`, into a dict key, into an f-string. No `.to_str()`
   at the boundary, ever, because there is no boundary.
2. **The `is_ascii` fast-path flag becomes unnecessary.** `OroStr` carries a
   precomputed ASCII flag precisely because Oro chose character indexing;
   under (b) indexing is O(1) unconditionally and that machinery deletes itself.
3. **Oro's corpus would not notice.** This is worth stating precisely, because
   it is the strongest empirical argument for (b) and it is true: of the 64
   files in `corpus/core/`, non-ASCII text appears in **comments only** — em
   dashes and ellipses in seven files. Not one corpus program calls `len()`,
   indexes, slices, or iterates a non-ASCII string. Switching to byte semantics
   today would not change a single `.expected` byte.

### Why it loses

**The decisive argument is the README's own surviving invariant.** From the
compatibility section:

> where syntax is identical, behaviour must be identical. Diverging loudly (a
> new name, a new spelling) is fine; the same code silently meaning two
> different things is not.

Model (b) is the *exact* thing that rule forbids. `len(s)`, `s[i]`, `s[i:j]`
and `for c in s` all keep their spelling and change their meaning, silently,
for any string that is not pure ASCII. There is no error, no migration message,
no new name — a program that counted characters now counts octets and only
notices when a user types an accent. That is the failure mode Oro rejects `case
QUIT:` over, rejects bare multi-segment `import a.b.c` over, and rejects
`capture_output=True` without `text=True` over. Applying the rule consistently
settles the question.

Model (a) obeys the rule by construction: `bytes` is a **new spelling**
(`b"..."`) for a new meaning. Loud divergence, which the README explicitly
blesses.

Three further costs, in descending order of how much they should worry you:

- **The oracle is lost for a region, permanently.** The corpus argument above
  cuts both ways. Today's corpus would not break — but under (b) the corpus can
  never *grow* a Unicode string test, because CPython would be permanently
  wrong about `len`, indexing and slicing for the rest of Oro's life. The
  README calls `corpus/core/` an *independent* check and says every correctness
  bug ever found lived in the computational core. Under (a), the entire new
  surface stays oracled: CPython has `bytes`, with the same literals, the same
  indexing rule, the same methods, and the same binary file semantics, so
  byte-level tests are generated by `corpus/oracle.sh` like everything else —
  with one exception, the file *mode* spelling, which §2 explains and pays for.
  Model (a) *adds* to the oracle's coverage; model (b) subtracts from it. For a
  project whose only independent correctness check is that oracle, this is not
  close.
- **The refactor is enormous and lands in the worst possible place.**
  `OroStr.s` is a Rust `String`, i.e. UTF-8 validity is enforced by the type
  system throughout the interpreter. Model (b) makes it a `Vec<u8>` with no
  validity invariant, and that touches every string operation, `format.rs`, the
  f-string `{n:c}` spec, `repr`/`display`, `HKey`, the lexer's own string
  handling, and the `regex` integration (which would move to `regex::bytes`,
  a different API with different semantics for `.` and `\w`). It would happen
  concurrently with the interpreter performance effort. Weeks of deep churn
  with a very high blast radius, in the layer whose correctness is the whole
  reason anyone would trust Oro.
- **It does not actually escape the wart it is sold as escaping.** Go's `s[i]`
  yields a `byte`, i.e. an integer — the very `b[0] → int` behaviour that gets
  held against Python. To avoid it you would have `"é"[0]` return a one-byte
  string that is not valid UTF-8, which then flows into `print`, into dict keys,
  into f-strings, into `re`. Model (b) does not remove the wart; it relocates it
  from a separate type into the type everything else uses.

### Decision

**Model (a). Two types: `str` and `bytes`. No third.**

And be honest about the bill: two types instead of one, `.to_str()` /
`.to_bytes()` at every boundary between the wire and the program, and an
indexing rule that differs from `str`'s. Model (b) would have been genuinely
smaller and would have made `net` and `str` the same world. It loses on one
thing, but that thing — a silent semantic change under an unchanged spelling,
and the loss of the oracle over it — is the thing Oro is most committed to not
doing.

### The `bytes` type

**Literal.** `b"..."`, with `\xNN`, `\n`, `\r`, `\t`, `\\`, `\"`, `\0` escapes
and no `\u`. Raw form `rb"..."` for symmetry with `r"..."`. A non-ASCII source
character inside a `b"..."` literal is a **compile error** naming `\xNN` or
`"…".to_bytes()` — which is also exactly what CPython does, so the oracle
covers the rejection too.

**Immutable, like `str`.** Slicing, concatenation and repetition produce new
values.

**Operators and protocol:**

| Spelling | Result | Note |
|---|---|---|
| `len(b)` | `int` | number of octets |
| `b[i]` | `int` (0–255) | negative indexes as elsewhere |
| `b[i:j]` | `bytes` | |
| `b1 + b2` | `bytes` | |
| `b * n` | `bytes` | |
| `b1 in b2` | `bool` | subsequence, not membership of an int |
| `==`, `<`, `>` … | `bool` | lexicographic, like `str` |
| `for x in b` | `int`s | |
| dict key | yes | `HKey::Bytes(Vec<u8>)` |
| truthiness | empty is falsy | |

Yes, `b[0]` is an `int` while `b[0:1]` is `bytes`. This is called a wart; it is
not one. `str` is a sequence of characters and Oro has no character type, so a
one-character `str` is the only thing `s[i]` can be. `bytes` is a sequence of
*numbers*, and a number is what `b[i]` should be. The asymmetry is the two
types telling the truth about what they contain. It also happens to be what
makes byte-level code in Oro affordable — `if line[i] == 58` compares two
unboxed integers, where a one-byte-string result would allocate an `Rc<OroStr>`
per index on an interpreter that is already 1.7×–5.7× slower than CPython.

**Method set.** Deliberately smaller than `str`'s:

```
b.find(sub)              -> int, -1 if absent
b.split(sep, maxsplit=?) -> list of bytes
b.strip() .lstrip() .rstrip()   -> bytes   (ASCII whitespace only)
b.startswith(p) .endswith(s)    -> bool
b.replace(old, new)             -> bytes
b.to_str()                      -> str     (UTF-8 decode, strict)
b.hex()                         -> str     (lowercase, no separator)
```

Plus `xs.join(b"")` from the existing collection protocol, where `xs` is a list
of `bytes`. A list mixing `str` and `bytes` is a `TypeError` — `join` never
guesses a conversion, the same rule `json.stringify` applies to dict keys.

Deliberately **absent**, each for a reason:

- **`.upper()` / `.lower()`.** Case is a text concept. Lowercasing an HTTP
  header name happens after it is decoded to `str`, where `str.lower()` already
  lives. *Deciding question: if profiling shows header-name decoding before
  dispatch is hot, an ASCII-only `bytes.lower()` is the cheapest fix. Measure
  first.*
- **The collection protocol** (`.map`, `.filter`, chains). `bytes` is a
  sequence for `len`/index/slice/`in`/iteration but is not a chain receiver.
  Every chain step over bytes would materialise a list of boxed ints — always
  the wrong tool for the byte-level work it would be reached for — and the
  type-preservation rule cannot hold (a `map` over `bytes` can return anything).
  The operations people actually want on bytes are `split`, `find` and slicing,
  and those are here.
- **`.decode(encoding)` / an `encoding=` argument anywhere.** UTF-8 is the one
  encoding. Others are a library, written in Oro, later. Latin-1 in particular
  is four lines.

### No `bytearray`, and no growable buffer type

The obvious next move is a mutable growable byte buffer. Do not make it.

Oro already has a frozen idiom for building a string incrementally, and
`std/json.oro` uses it on every code path: append to a list, `join` at the end.

```python
out = []
out.append(chunk)
out.append(b"\r\n")
return out.join(b"")
```

The same idiom builds bytes, with the same performance characteristics, using
zero new types. A `bytearray` would be a second spelling for something the
language can already do — precisely the tax the thesis rejects.

The one place a genuinely mutable window is required is a buffered reader, which
needs to consume a prefix without copying the tail on every read. That window
lives inside every Rust-backed reader (§2), in Rust, and Oro code never sees
it — there is no reader type to construct, no buffer size to pass, and no way to
observe it. So the requirement is real, and it is satisfied without exposing a
mutable byte type at all.

### Conversions, and what invalid UTF-8 raises

```python
s.to_bytes()   # str -> bytes, UTF-8. Cannot fail.
b.to_str()     # bytes -> str, UTF-8, strict.
```

`b.to_str()` on invalid UTF-8 raises **`ValueError`** — a plain one, not a new
class — and so does every other place in the language where bytes become `str`.

An earlier draft gave it a dedicated `UnicodeDecodeError` under `ValueError`,
arguing that a server must be able to tell "the client sent garbage — answer
400" from "my code has a bug — answer 500", and that catching `ValueError`
broadly to get the first would swallow the second. The argument is sound about
the *distinction* and wrong about the *mechanism*. A server draws that line by
where it catches, not by what it catches: the decode of a header value happens
inside the head parser, so a `try` around the parse answers 400 and a `try`
around the handler answers 500, and the two never overlap (§6). A dedicated
class buys the ability to be sloppy about scope, at the cost of one more name
frozen at 1.0 forever.

It also buys nothing from the oracle, which is the tiebreak. CPython's
`UnicodeDecodeError` is a subclass of `ValueError`, so a corpus test that writes
`except ValueError` passes identically under CPython and under Oro — the
hierarchy stays compatible without Oro having to own the extra class. The cost,
named: a program that wants to distinguish decode failure from any other
`ValueError` by type alone cannot, and must arrange its `try` blocks instead.

`int.to_bytes()` is **not** provided. CPython's version needs a length and a
byte order, which is a binary-packing feature, and nothing in HTTP/1.1 needs it.
*Deciding question: the first binary protocol Oro is asked to speak decides
whether this becomes `struct`-shaped module written in Oro or a pair of
primitives. It is additive either way, so leaving it out now costs nothing.*

### Corpus impact

Nearly every item above is directly oracle-testable, because CPython has it with
identical semantics: literals and escapes, the `b[0]`/`b[0:1]` split,
`find`/`split`/`strip`/`startswith`/`replace`, ordering, hashing, and the
rejection of non-ASCII characters in a `b"..."` literal. That is one new core
file, `bytes_basics`, generated by `corpus/oracle.sh` like the rest.

Two things cannot go there. `b"".join(xs)` is spelled `xs.join(b"")` in Oro, so
joins go to `corpus/divergence/` as usual. And file I/O follows them, because
`open(p, "r")` no longer means the same thing to CPython and to Oro — that is a
consequence of §2's mode decision, and §2 pays for it there rather than
pretending the cost lands here.

---

## 2. The io protocol

`sys.stdout` is currently the string `'<stdout>'`. There is no stream protocol
to stay compatible with. This is a free hand, and it will not come again.

### The two interfaces

```
Reader:   read(n)    -> bytes     # 1..n bytes, or b"" at EOF
Writer:   write(b)   -> None      # writes all of b, or raises
```

That is the entire protocol. It is a **naming convention**, not a declared
type — there is no `class Reader`, nothing to inherit from, and no registration.
Any Oro object with a `read` method of that shape is a Reader; any Rust-backed
stream that exposes one is too. In a dynamic language this costs exactly
nothing, which is the whole reason to commit to it now rather than discover it
later.

**`read` returns at most `n`, and may return fewer without being at EOF.** A
short read is semantically meaningful: it means "this is what has arrived". Code
that needs exactly `n` bytes calls `io.read(r, n)`.

**`write` writes everything or raises.** Go returns a count and requires the
caller to handle short writes; Oro does not, and it is the concurrency model
that earns this. A short write happens because the socket buffer filled; under
green threads the runtime simply parks the task and finishes the write when the
socket is writable again. There is no reason to export that to the user, so
`write` has no return value and every call site loses a branch it would always
have got wrong.

**Both directions are bytes, and only bytes.** `read` returns `bytes`; `write`
takes `bytes` and nothing else. No stream anywhere in the language accepts a
`str`, in either direction, ever. Two consequences follow, and both are the kind
of thing people expect to work, so they are stated rather than discovered:

- **`print` and streams are separate worlds.** `print` is unchanged and still
  takes `str`. There is no `sys.stdout.write("hi")` — that is a `TypeError`, and
  the spellings are `print("hi")` or `sys.stdout.write(b"hi")`. Both reach fd 1
  in program order; they just do not accept the same argument. This is a real
  edge to trip over, and it is the price of having exactly one stream protocol
  rather than one for text and one for bytes.
- **An HTTP response is bytes the whole way down**, from the handler's return
  value to the socket. There is no point in the stack where an accidental encode
  can happen, because there is nothing that would accept a `str` to encode it.

The bill is `.to_bytes()` at the point where a program's text becomes output.
That is §1's two-type tax, paid once at a visible boundary instead of smeared
across a family of stream types that each guess differently.

### EOF is an empty return, not an exception

Reading past the end of a stream returns `b""`. It does not raise.

Errors in Oro are exceptions, and it is tempting to be consistent and raise at
EOF too. Three reasons not to:

1. **EOF is not a fault.** Every stream ends. The exception hierarchy is for
   things that went wrong, and the normal termination of the most common loop in
   systems programming is not one of them.
2. **The cost model is different from Go's.** Go returns `io.EOF` *as a value*,
   which is cheap to compare and idiomatic because Go returns errors anyway.
   Oro's exceptions unwind block and frame stacks. Copying Go's shape here would
   copy the form without the reason, and would put a `try`/`except` — with no
   `with` and no `try/except/else` to soften it — around every read loop in the
   language.
3. **It is oracle-checkable.** CPython's `read(n)` returns `b""` at EOF, so a
   corpus test can pin it.

The one hole this leaves is `read(0)`, which would return `b""` and look like
EOF. Closed by fiat: **`n` must be ≥ 1; `read(0)` raises `ValueError`.**

There is no `read()` with no argument meaning "read everything". That is a
second behaviour under one name, the interface is supposed to have exactly one
method, and an unbounded read is a memory footgun on a server. Reading a whole
stream is `io.read(r)`, a free function, in Oro, built on `read(n)`.

### `open()` returns a byte stream, and text mode is cut

This is the most contentious call in the document, so here it is plainly.

```python
f = open("data.bin", "r")     # Reader
g = open("out.bin", "w")      # Writer
h = open("log.bin", "a")      # Writer, appending
```

Three modes, the letters everyone already knows, and every one of them is
**bytes**. There is no `"rb"`, no text mode, no `encoding=`, and no second file
shape anywhere in the language.

**Style note, because it governs every example from here down:** `open()` is
assigned to a variable and the variable is used. Chaining straight off the call
— `io.read(open(p, "r"))` — works, and is shorter, and hides the one thing that
matters most about a stream in this language: its lifetime. Oro closes a file
when the last reference to it drops, and Oro has block scope on `if`, `for` and
`while`, so *where the variable lives* is *when the file is open*. A name makes
that visible in the source. An anonymous temporary turns it into a rule you have
to have read this paragraph to know.

An earlier draft of this document rejected `open(p, "r")` outright and required
`"rb"`, on the grounds that keeping a spelling while changing its meaning is the
exact silent change that §1's rule forbids. That argument was wrong here, and it
is worth being precise about why, because the rule itself is not in doubt — it
is the best rule in the README.

**The rule earns its force from the failure mode, not from the spelling.** Under
Go's `len(s)`, the failure is silent, on a user's data, in production, months
later, for the one customer with an accent in their name. Nothing raises. The
program is simply wrong, and the oracle can never be asked about it again. A
`open(p, "r")` that returns bytes fails on the first line of the first run: the
old code does `data.split("\n")` and gets a `TypeError` for passing a `str` to a
`bytes` method, at the top of the file, in development, every single time. Those
are not the same kind of change. Silent-change is a class of *bug*, not a
category of edit, and this edit is not in it.

The migration, besides, is one note in a changelog. Oro is 0.2.0 and has no
users; the entire population that must read that note is the corpus and whoever
is reading this.

Which leaves the `b`. With text mode gone, `"rb"` contrasts with nothing — it is
a letter meaning "not the other kind" in a language that has no other kind,
typed in every `open` call for the rest of the language's life. Vestigial
syntax is exactly what the thesis exists to refuse. So the `b` goes. The cut is
the same cut; it happens by respelling three modes rather than by rejecting
them, and the result is one fewer character in the common case instead of one
more.

The text conveniences do not come back in another form either. Whole-file text
is a compose of two things that already exist:

```python
f = open(path, "r")
s = io.read(f).to_str()          # whole file, as text
```

```python
f = open(path, "w")
f.write(s.to_bytes())            # text, written
```

There is **no text stream type, no text wrapper, and no line iterator**. A file
is a Reader. That is the only shape it has.

**Corpus impact, and the one place this decision genuinely costs something.**
`corpus/core/22_files.oro` exercises text `open`/`read`/`readline`/`readlines`/
iteration against CPython, and it is rewritten in byte mode. But `corpus/core/`
works by running the same source file under both CPython and Oro, and
`open(p, "r")` no longer means the same thing to both — so **file I/O leaves
`corpus/core/` and lands in `corpus/divergence/`**, reviewed by hand instead of
generated. Had `"rb"` been kept, the file test would have stayed automatically
oracled.

That is a real bill, and §1's decisive argument was an oracle argument, so it
deserves a straight answer rather than a wave. What is lost is the *mode string*,
not the semantics: `read(n)` returning at most `n`, `b""` at EOF, `write`, and
every `bytes` method exercised on the way through are all identical to CPython's
`open(p, "rb")`, and all of them stay oracled by `bytes_basics` in
`corpus/core/`. The divergence file is a CPython program with one letter
changed, so its baseline can be regenerated by running the `"rb"` twin under
CPython and diffing — a manual oracle rather than an automatic one, which is
weaker, and is the honest description of what the `b` was buying.

**Settled: `"r"`, and on a principle worth stating once here because it will
come up again.** The oracle is a *check* on Oro's correctness, not an authority
over Oro's design. The moment it starts dictating syntax it has stopped being a
test and become a specification — and the specification it would impose is
CPython's, mistakes included. A mode letter meaning "not the other kind", in a
language that has no other kind, is one of those mistakes. Oro pays a
manual-review cost on one file rather than carry Python's vestigial syntax for
the rest of its life.

This cuts only one way, and the limit matters: it licenses declining a
*spelling* CPython would impose, never declining the *answer* CPython gives.
Where Oro and CPython run the same program, CPython is still right and Oro is
still wrong when they disagree.

*Deciding question, if this is to be reopened: is a text file object worth
having two file shapes forever? The answer here is still no. It is no longer the
closest call in the document, either — cutting `io.read_text` below is, and that
is a helper rather than a type.*

#### No `"rw"`, and no `seek`/`tell`, in M0–M6

A deliberate deferral, recorded with its reasoning so it is not later mistaken
for an oversight.

Read-write without random access is nearly useless: you can append, or you can
reopen and re-read, and both of those already have a mode. So `"rw"` is not
really a request for a mode — it is a request for `seek` and `tell`. That is a
genuine building block, in the strict sense this project uses the phrase: by the
C standard's reckoning it is one of the small set of things you cannot write
around, and a database file, an index, or any random-access format cannot be
written without it. **It should exist before the 1.0 freeze.**

It is also completely orthogonal to serving HTTP, and it has a cost this design
now has to respect: every reader buffers internally (below), so every seek must
invalidate that buffer, and getting that wrong produces stale reads that look
like data corruption. That is a small amount of Rust and a real correctness
trap, and it has no business landing in the same release as the scheduler.

When it does land, it comes with `seek`/`tell` and a spelling chosen fresh —
**not** C's `r+` / `w+`, which are famous for being unmemorable in the one way
that matters: `r+` does not truncate and `w+` does, and nothing in the syntax
tells you which is which.

### The concrete stream types

Three Rust-backed types, and that is all of them:

| Type | Made by | Reader | Writer | Extra |
|---|---|---|---|---|
| `File` | `open(path, mode)` | yes (`"r"`) | yes (`"w"`/`"a"`) | `read_until`, `close()` |
| `TcpStream` | `net.dial`, `listener.accept()` | yes | yes | `read_until`, `close()`, and §4 |
| `Buffer` | `io.buffer(b=b"")` | yes | yes | `read_until`, `bytes()` |

Down from five. `BufReader` and `BufWriter` are gone, for the two reasons that
follow this table.

`Buffer` survives a round of cutting that removed two of its neighbours, and it
is worth saying why, because it looks like the convenience of the three and is
in fact the load-bearing one. **It is the only way to get a Reader you can feed
literal bytes to**, and that is what lets the entire HTTP layer be written,
tested and fed adversarial inputs before a socket exists (§8). Nothing else in
the language can stand in for it: a list of `bytes` has no `read(n)`, no
`read_until` and no read position. Writing it as an Oro class instead would mean
writing a second `read_until` in Oro, and then the language has two of them with
two sets of edge cases — precisely the "zoo of mutually incompatible file-likes"
that the one-protocol decision exists to prevent.

**`read_until` is a method on every Rust-backed reader** — all three of them.

```python
head = conn.read_until(b"\r\n\r\n", 65536)   # includes the delimiter
```

It has to be a method rather than a free function because it must see inside the
reader's buffer: written over `read(n)` it would either read a byte at a time,
or over-read past the delimiter with nowhere to put the excess. The previous
draft made it a method on buffered types only. Now that every reader buffers, it
is a method on *every* reader, which is strictly simpler: there is no longer a
category of stream that lacks it, and no constructor call standing between a
socket and the ability to read a line. This is not interface growth — the
*interface* is still one method. `read_until` is a method on the concrete types,
the way `bufio.Reader` has `ReadSlice` in Go.

An Oro class that implements the protocol still supplies `read(n)` and nothing
else, and that is fine as long as nothing tries to read a delimited record from
one. It is also the reason `io.read` and `io.copy` are written against `read(n)`
alone: `_LimitReader` and `_ChunkedReader` in §6 are Readers by that definition
and by no other, and every free function has to keep working on them.

`read_until` raises `ValueError` if the limit is reached before the delimiter is
found — the required behaviour for an HTTP server that must not let a client
send an unbounded header block.

### Every reader buffers, and nothing in Oro can see it

`BufReader` and `io.buf_reader` are cut. Buffering is not opt-in; it is what a
reader is.

The argument is short. `read_until` requires a buffer, and HTTP header parsing
requires `read_until`. On a raw socket without buffering, finding `\r\n\r\n`
means one `read(1)` syscall per byte of the request head — two orders of
magnitude of overhead for the most common operation the target program
performs. So every real program would open its connection and immediately wrap
it, and the failure mode for forgetting is not an error but a server that is
mysteriously, catastrophically slow. An API whose correct use is "always, right
away" is not an API; it is a default someone forgot to set.

Go makes buffering opt-in, and Go is right to, for a reason that does not
transfer: `net.Conn` is deliberately a thin, unbuffered handle on a file
descriptor, because Go expects you to build `bufio.Reader`, `bufio.Scanner`,
`http.Transport` and your own framing on top of the same primitive. Oro's whole
0.2 divergence from Python was argued on **sane defaults**: the divergence table
is a list of places where the old default was wrong more often than it was
right, and was therefore changed rather than documented. Buffering by default is
the same call, made in the place where getting it wrong costs the most.

**The buffer is allocated lazily, on first read, not at construction.** This is
a small implementation rule with a large operational consequence and it should
be built in from the beginning rather than discovered under load. 8 KiB per
stream is nothing when the streams are files; 10,000 idle keep-alive connections
is 80 MB of buffers holding nothing, on a server whose entire selling point is
that a parked task costs a few hundred bytes. An idle connection should cost
what an idle task costs.

This is necessarily a Rust-side detail, and that is the point: Oro code has no
handle on the buffer, no way to size it, and no way to observe whether it has
been allocated yet. Exposing any of that would be reintroducing `BufReader`
under a new name.

### Writers are unbuffered, and `flush()` does not exist

`BufWriter`, `io.buf_writer` and `flush()` are all cut — `flush` from the
language entirely, not just from the writers.

A buffered writer needs a `flush()`, and `flush()` is one of the great silent
bugs: the program is correct, the tests pass, the last response of every
connection is truncated, and nothing anywhere raises. It is an API whose only
purpose is to create a discipline that every caller must remember forever, and
whose only failure mode is silence. Oro removed `with` because refcounting made
it unnecessary; keeping `flush` would be adding back a ritual of the same shape,
with worse consequences for forgetting it.

The thing `BufWriter` was for — one syscall per response instead of one per
header — is already an idiom this language has, and it is the same idiom §1 uses
instead of `bytearray`:

```python
parts = []
parts.append(head)
parts.append(body)
w.write(parts.join(b""))       # one call, one syscall
```

`std/json.oro` already builds every string it produces this way. Batching in the
program is more explicit than batching in the runtime, it is visible at the call
site, and it cannot be forgotten halfway — there is no state to leave dirty. The
cost is that a program which really does want to dribble out many small writes
now issues many small syscalls, and the fix for that is a source change rather
than a constructor. That is a fair trade against a class of bug that produces
truncated output with no error.

**Closing is not part of the protocol.** There is no `Closer` interface, because
Oro's refcounting already closes a stream deterministically when its last
reference drops — the same property that removed the need for `with`. Concrete
types still expose `close()` for the cases where "end of scope" is too late, and
`TcpStream` additionally exposes `shutdown_write()` for a half-close. With
writers unbuffered, a dropped writer has nothing pending, so there is no
drop-flush and nothing to lose by never calling `close()` at all.

### Free functions: three of them

```python
io.read(r, n=None)     # everything until EOF, or exactly n (EOFError if short)
io.copy(dst, src)      # -> int, bytes copied, until src EOF
io.buffer(b=b"")       # an in-memory Reader + Writer
```

That is the entire `io` module. `io.copy` is a chunk-at-a-time loop over the
protocol, written in Oro in `std/io.oro`, never byte at a time; so is `io.read`,
apart from one Rust fast path that the memory subsection below explains and
pays for. `io.buffer` is the constructor for the Rust `Buffer` type and lives
here because a stream constructor belongs in the stream module. `io.copy` is the payoff of
the naming convention: it works on a file, a socket, a `Buffer`, or an Oro class
someone wrote this afternoon, and it never learned about any of them.

An earlier draft of this section listed six, with `io.buf_reader` and
`io.buf_writer` beside them in the types table. Every cut, with its reason:

**`io.read_all` and `io.read_exact` are one function, not two.** The capability
had to survive the merge, because it is not a convenience. An HTTP body on a
keep-alive connection is exactly `Content-Length` bytes: reading to EOF is not
available (the connection does not close, that is the point of keep-alive), and
`r.read(n)` may legally return fewer. Without a count form, every body read in
every program hand-rolls a loop, and a hand-rolled read loop is the bug that
works in testing against localhost and fails under load when a request spans two
packets. So the count form stays; it just stays as an argument rather than as a
name. The resulting mental model is worth stating outright, because the whole
short-read hazard reduces to it:

> **`r.read(n)` is the raw primitive — one syscall's worth, "what has arrived".
> `io.read(...)` does the whole job, and always returns what was asked for.**

`io.read(r)` with no count reads until EOF. `io.read(r, n)` returns exactly `n`
bytes or raises `EOFError` (a new class directly under `Exception`, CPython's
placement) if the stream ends first.

**`io.write` was never added.** It would be a second spelling for `w.write(b)`,
which already writes all of `b` or raises — there is no partial-write case for a
free function to paper over. That leaves a visible asymmetry: `io.read` exists
and `io.write` does not. The asymmetry is real, it is documented here, and it is
not going to be fixed, because it is not an inconsistency — it is the two
directions being genuinely different. Reading everything requires a loop.
Writing everything is already guaranteed by the protocol.

**`io.lines` — cut.** It is a compose of primitives that already exist:

```python
lines = io.read(f).to_str().rstrip("\n").split("\n")
```

The trailing-empty papercut is real (`"a\nb\n".split("\n")` is
`['a', 'b', '']`, which is why the `rstrip` is there) — and it is
CPython-identical, so the oracle covers it, which is the strongest thing that
can be said for any behaviour in this language. Oro has no `splitlines`, and
this is not the moment to add one: it would arrive owing an answer about `\r\n`,
about `\v`, `\f` and `\x1c`, and about `keepends`, and CPython's answers there
are a list of Unicode line breaks nobody remembers. The cost of the cut is that
a program reading a large file line by line now holds the whole file in memory
first, where a generator would not. Streaming line-at-a-time over an arbitrary
Reader is the thing that is genuinely harder now, and if it turns out to matter,
it comes back as a generator over the protocol — additively, with no change to
anything here.

**`io.read_text` / `io.write_text` — cut.** Each was one line that is still one
line without it:

```python
s = io.read(f).to_str()          # was io.read_text(path)
f.write(s.to_bytes())            # was io.write_text(path, s)
```

No loop, no repeated logic, nothing subtle to get wrong — which is the bar a
stdlib helper has to clear, and neither clears it. The deciding argument is
consistency: Oro programmers write `.to_str()` and `.to_bytes()` at every other
boundary in the language, so a pair of functions that exist only to hide those
two calls at one specific boundary is a special case, not a convenience.

The honest cost, since this was close: reading a whole file as text is the most
common thing anyone does in a scripting language, and it just got about fifteen
characters longer. Worse, with the file assigned to a
variable — which the style note above asks for — it is two lines where it used
to be one:

```python
f = open(path, "r")
s = io.read(f).to_str()
```

That is the entire case for keeping them, and it is not a weak one. It was
judged insufficient against two more names in a frozen module.

#### Whole-file reads and memory

The optimisation goes in `io.read(r)` itself rather than in a text helper,
which is a second reason not to have the helper: put it here and *every*
whole-file read benefits, including the ones inside `io.copy`-shaped code that
nobody thought of as a file read.

**When the Reader is a `File`, `stat` it and allocate the result exactly once.**
Growing a buffer chunk by chunk doubles capacity as it goes, so at the moment of
the last doubling the process holds both the old and the new allocation: the
transient peak is roughly 1.5–2× the final size. A 4 GB file momentarily wants
8 GB, and that transient is what actually kills the process — not the file.
Pre-sizing takes the peak to 1×.

Frame this correctly, because it is easy to oversell: **this is a 2× reduction
in peak memory, not immunity from running out of it.** A 10 GB file still does
not fit in 8 GB of RAM, and `io.read` should not pretend otherwise. For sockets
and pipes, where the size is not knowable in advance, it falls back to chunked
growth and the 2× transient comes back.

**This costs `io.read` its purity, and that is worth stating rather than
hiding.** Pre-sizing cannot be done in Oro: `bytes` is immutable and there is no
`bytearray` (§1), so the Oro spelling of "read everything" is append-to-a-list
and `join`, which holds the chunks and the joined result at the same time — 2×,
exactly what the optimisation exists to avoid. So the whole-stream read of a
Rust-backed stream happens in Rust. §5's rule already says this without needing
an exception carved for it: assembling a multi-gigabyte result touches every
byte, and anything that touches every byte goes in Rust.

The wrinkle is that `io.read` must still work on an Oro class that implements
`read(n)` — `_LimitReader` in §6 is exactly that — and a Rust builtin cannot
call back into the interpreter (§3). So `io.read` stays in `std/io.oro` and
opens with a type check: one of the three Rust stream types goes to the Rust
path, anything else takes the Oro chunk loop and its 2× transient. A type check
in a language that avoids them is a small smell; `std/http.oro` already contains
the same shape for `Response.body`, and three lines of dispatch is the honest
price of the whole-file case being fast and the general case being possible.

**There is no size cap, and the reason is structural rather than a shrug.**
Whole-file reads take a path, so they touch local files that the programmer
named. The unbounded-input risk is not there; it is on sockets, and socket reads
go through `io.read(r, n)` with an `n` the server chose, or through
`read_until(delim, limit)` with a limit the server chose. A surprising default
limit would fire on the legitimate case (the 200 MB log file you meant to read)
and never on the dangerous one. One honest line of documentation — *this holds
the entire stream in memory* — is worth more than a magic number that has to be
turned off.

### `sys.stdout` / `sys.stderr` / `sys.stdin`

They stop being strings and become real streams: `sys.stdout` and `sys.stderr`
are Writers on fd 1 and 2, `sys.stdin` is a Reader on fd 0.

They are **unbuffered**, like every other writer in the language, which
preserves the documented behaviour that Oro flushes as it goes (`python3 -u`
semantics) and now does so because there is nothing to flush rather than because
something flushes eagerly. `print` continues to write through the same handle,
so `print(...)` and `sys.stdout.write(b"...")` interleave in program order.
`print` itself is unchanged and still takes `str`.

### The knock-on removals

Three things in the language today predate this protocol and cannot survive it.
They are listed together because each is a small break with a corpus
consequence, and the migration note should carry all of them at once.

- **`File.readline`, `File.readlines`, and line iteration over a file.** All
  three are the text file object, which no longer exists. The replacement is
  `io.read(f).to_str().rstrip("\n").split("\n")`, per `io.lines` above.
  `corpus/core/22_files.oro` is rewritten in byte mode, and moves to
  `corpus/divergence/` with it (§2).
- **`sys.stdout` / `sys.stderr` / `sys.stdin` as string placeholders.** They
  become the real fd-backed streams above.
- **`proc.run(...).stdout` and `.stderr` become `bytes`.** They are `str` today
  — `src/vm/mod.rs` builds them with `Value::str(...)` — and that was the right
  call when there was no byte type and no alternative. There is one now, and
  consistency wins: a subprocess emits octets, the encoding of those octets is a
  property of the child process and not of Oro, and a wrong guess about it is a
  real bug that surfaces as mojibake in a log or a crash in a parser. `.to_str()`
  at the call site is the same one call every other boundary in this document
  asks for. The cost is that the common case — `proc.run(["git", "rev-parse",
  "HEAD"]).stdout.strip()` — gains a `.to_str()`, and that
  `corpus/divergence/28_proc.oro` breaks and needs its baseline regenerated.
  Worth noting that this moves *toward* CPython, whose `subprocess` also
  returns bytes unless asked otherwise — Oro simply has no `text=` to ask with,
  because there is one spelling and it is `.to_str()`.

---

## 3. Green threads

### The surface

```python
t = spawn(handle, conn)        # start a task, returns a Task handle
t.join()                       # wait; returns the function's value

ch = chan()                    # unbuffered: send blocks until a receiver takes it
ch = chan(64)                  # buffered: send blocks only when full
ch.send(v)
v = ch.recv()
ch.close()
for msg in ch:                 # iterates until the channel is closed and drained
    ...
```

Six names total: `spawn`, `chan`, `send`, `recv`, `close`, `join`.

**`spawn(f, *args)`** starts `f(*args)` as a task, immediately, and returns a
`Task`. It is a builtin, not a module member, because it is a control-flow
construct — the same reason `print` and `len` are builtins.

**Both buffered and unbuffered channels, from one constructor.** `chan()` is a
rendezvous, `chan(n)` has capacity `n`. This is one spelling with a parameter,
not two functions, exactly as `json.stringify(value, indent=None)` covers both
compact and pretty output with one name.

**A closed channel.** `send` on a closed channel raises `ChannelClosed` (a new
class under `Exception`, alongside `CommandError`). `recv` on a closed *and
drained* channel raises `ChannelClosed` too; a closed channel with buffered
items still hands them out first. Iterating a channel with `for` stops cleanly
at that point instead of raising, which makes the producer/consumer shape read
the way a `for` over a generator already does.

### No `select`

Go needs `select` because a goroutine otherwise cannot wait on two channels.
Oro largely does not, and the reason is worth stating because it is the best
property of the whole model:

**Inside one VM there is no parallelism, so there are no data races, so tasks
can share plain mutable state with no locks and no channels.** A shutdown flag
is a module-level variable. A connection counter is an `int`. A cache is a
`dict`. Channels in Oro are for *structure* — handing work between stages — not
for safety, because safety is free.

That removes the main thing `select` is reached for in a server (wait for work,
or for a shutdown signal). Per-connection tasks plus read timeouts plus a shared
shutdown flag cover the rest.

So: **`select` is not shipped.** It is additive and can arrive later if a real
program needs it, in which case it should be a function — `select([a, b])`
returning `(index, value)` — because Oro cannot afford new statement syntax.
*Deciding question: is there a real program in the first year that must wait on
two independent sources at once? If not, this stays cut.*

### An uncaught exception in a task

Neither Go's answer nor Python's is right here.

- Go kills the process on an unrecovered panic in any goroutine. Every Go HTTP
  server therefore wraps every handler in `recover()` — the ecosystem works
  around the language.
- Python's threads print a traceback and vanish, and the process exits 0, so a
  dead worker is silent.

Oro's rule:

1. The exception is **stored on the `Task`** and the task dies. Other tasks and
   the program keep running.
2. If the task is joined — then or later — the exception is **re-raised in the
   joiner**. The joiner owns it; nothing is printed.
3. If the `Task` handle is dropped without ever being joined, the exception is
   **printed to stderr** at that moment, with the same rendering an uncaught
   top-level exception gets, and the process exit code is set to 1.

Rule 3 uses Oro's deterministic refcount drop, which is exactly the property
that removed `with`. Nothing is ever silently swallowed, and nothing is
double-reported.

This is the single most important property of a server runtime: a `KeyError` in
one request handler must return 500 for that request and must not take down the
other 9,999 connections.

### When the program is finished

**When the main task returns *and* every spawned task has finished.** There is
an implicit join-all at exit.

Go's "main returns and everything dies" loses the last lines a logger goroutine
was writing, and is a reliable source of "why did my work not happen". A server's
main never returns anyway, so this costs nothing in the target use case. The
escape hatch already exists: `sys.exit(code)` terminates immediately, killing
every task, and is the one way to abandon in-flight work on purpose.

### Shutdown and cancellation

There is no `task.cancel()` and no way to interrupt a task from outside.
Injecting an exception at an arbitrary suspension point is how you get code that
cannot maintain an invariant across an `await` — the thing that makes Python's
`asyncio.CancelledError` genuinely hard to write correctly.

Shutdown is cooperative and explicit, which in this model is easy:

```python
# std/http.oro, sketched
_shutting_down = False

def shutdown():
    global _shutting_down
    _shutting_down = True
    _listener.close()          # a parked accept() raises OSError

def serve(addr, handler):
    ln = net.listen(addr, reuseport=True)
    while not _shutting_down:
        conn = ln.accept()
        spawn(_serve_conn, conn, handler)
```

Closing the listener wakes the parked `accept()` with an exception; the loop
exits; the implicit join-all at exit drains the in-flight connections; each
connection loop checks `_shutting_down` before serving another keep-alive
request. A deadline on that drain is a `time.sleep` in a task plus `sys.exit`,
which needs no new API.

### Cooperative, not preemptive

**Tasks yield only at I/O, channel operations, and `time.sleep`.** A CPU-bound
task starves every other task in its VM until it finishes.

Say it plainly: yes, that means a handler with an accidental `while True:` hangs
that VM's other connections. Three reasons that is the right trade here, and one
reason it is not permanent:

1. **The deployment model already bounds the blast radius.** The architecture is
   N single-threaded VMs behind `SO_REUSEPORT`. A stuck task removes 1/N of
   capacity — exactly what a blocked OS thread does in a thread-per-core server,
   which is the standard the industry already accepts.
2. **Preemption costs the interpreter, and the interpreter is the problem.** A
   safepoint check on every instruction costs on the hottest path in the system,
   and the interpreter is already 1.7×–5.7× slower than CPython with a separate
   effort under way to fix that. Spending budget there, to fix a
   self-inflicted problem, is the wrong direction.
3. **Preemption without parallelism buys only latency fairness.** It does not
   buy throughput. Within one VM the total work is the same either way.
4. **It is not a freeze commitment.** Preemption is an implementation property,
   not a surface — there is no API that says "cooperative". If it is ever
   needed, a check on *backward jumps only* (loop back-edges) is nearly free,
   is where every runaway loop necessarily passes, and can be added in a point
   release without changing one line of user code. Choosing cooperative now is
   reversible; that is what makes it an easy choice.

No `yield_now()`. It is a second spelling for cooperation whose only purpose is
to work around a limitation we may delete.

### What `spawn` costs, concretely

The VM's flat design is what makes this cheap, and it is worth being precise
about how cheap.

A generator today is one suspended `Frame` moved into a `GenBox`
(`frame: Option<Box<dyn Any>>`) — the frame's `locals`, `cells`, `free` and
`stack` `Vec`s, boxed. Suspension is a `Vec::pop` and resumption a `Vec::push`.
Nothing is copied and the native stack is never involved.

A task needs the same trick applied to a *stack segment* rather than a single
frame, because a task parked in `conn.read(n)` may be twenty Oro calls deep.
That is the one real difference from generators — `Op::Yield` pops exactly one
frame, and a generator can only suspend its own top frame.

The work, therefore, is not the scheduler. It is a refactor: the state that is
today VM-global and is really per-execution has to move out of `Vm` into a
`Task`:

```
frames, line, col, prints, str_jobs, sort_jobs, seq_jobs, mat_jobs,
gen_stack, handling, finally_why
```

Everything left on `Vm` (`excs`, `argv`, `module_cache`, `import_root`,
`importing`, `proc_class`, `exit_code`) is genuinely process-wide and stays
shared — including the module cache, so `import` happens once per VM and all
tasks see the same module objects. `MAX_FRAMES` becomes a per-task limit.

Once that refactor exists, a task is:

- one `Task` allocation,
- one `Vec<Frame>` (empty `Vec`s do not allocate in Rust, so the eleven hoisted
  stacks are free until used),
- the initial `Frame`'s `locals` and `stack` vectors.

Call it three to five small allocations and a few hundred bytes, versus an OS
thread's `clone(2)` plus an 8 MiB stack reservation. No stack copying ever,
because frames are already heap cells — this is the property the README's
architecture section promised would pay for generators, collecting a second
dividend.

The realistic ceiling is memory: tens of thousands of parked tasks per VM is
routine, and a connection's cost is dominated by its 8 KiB read buffer
rather than by the task — which is exactly why that buffer is allocated lazily
on first read (§2). An accepted-but-silent connection costs what an idle task
costs.

### The one hard constraint on implementation

A task can only suspend when the VM is **between instructions with no Rust frame
in the middle**. That is already guaranteed — builtins are
`fn(Vec<Value>) -> VResult<Value>` and can never re-enter the interpreter, which
is why generator arguments have to be drained by `MatJob` and the call retried.

But it also means **an I/O primitive cannot be a plain `Builtin`.** A `Builtin`
must return a `Value`; a blocking read must instead say "park this task". I/O
primitives are therefore VM-dispatched, the way `proc.run` already is, and the
interpreter's `Step` enum grows one variant:

```rust
enum Step {
    Next,
    Done(Value),
    Raise(Value),
    Park(Wait),     // suspend the current task until `Wait` is ready
}
```

### tokio is the reactor; the VM is the scheduler

This framing matters, because "tokio runs the tasks" does not work: `tokio::spawn`
requires `Send`, and every Oro `Value` is `Rc`.

Instead:

- The **VM** owns the ready queue and decides which task runs next. Its run loop
  is unchanged and stays synchronous.
- **tokio** provides only readiness and timers: `TcpStream`/`TcpListener`
  registration with epoll/kqueue, and `sleep`.
- A parked task holds a boxed readiness future in a `FuturesUnordered`. When the
  ready queue empties, the VM calls `rt.block_on(parked.next())` to learn which
  task woke, moves it to the ready queue, and resumes the loop.

Nothing crosses a thread boundary, so `!Send` values are never a problem, and
`LocalSet`/`spawn_local` are not needed either.

**The dependency cost, named rather than waved at.** Oro's README boasts one
dependency, and adding tokio is a second and much larger exception to that. It
should be feature-gated to `net`, `time`, `rt` and `io-util`, kept out of the
`macros` and `rt-multi-thread` features that are useless here, and the release
binary size delta measured and recorded in the README rather than discovered by
a user. It must remain static-musl-linkable; that is a release-gate check, not
an assumption.

---

## 4. `net`

```python
import net

ln = net.listen("0.0.0.0:8080", reuseport=True)
conn = ln.accept()               # parks; -> TcpStream
ln.close()

conn = net.dial("example.com:80")
conn.read(4096)                  # Reader
conn.write(b"...")               # Writer
conn.peer                        # "203.0.113.7:54321"
conn.local                       # "10.0.0.2:8080"
conn.set_timeout(30)             # seconds; applies to read and write
conn.set_nodelay(True)
conn.shutdown_write()
conn.close()
```

That is the whole module: two constructors, two objects.

**Addresses are strings.** `"host:port"`, with Go's bracket form for IPv6
(`"[::1]:8080"`). No `Address` type. A type would buy parsing that is rarely
needed, and when it is, `addr.rsplit(":", 1)` covers it with methods that already
exist. Go made this call and it has aged well.

**A `TcpStream` *is* a Reader and a Writer**, with exactly the §2 semantics:
`read(n)` returns 1..n bytes or `b""` when the peer has closed, `write(b)`
writes all of `b` or raises. Every `io` free function therefore works on a socket
with no adaptation, and `io.copy(conn_out, conn_in)` is a working TCP proxy.

**Timeouts are one knob.** `set_timeout(seconds)` sets a deadline applied to both
directions; `set_timeout(None)` clears it. Separate read and write deadlines are
two knobs where servers set both to the same value. Expiry raises `TimeoutError`,
which already exists under `OSError`. A listener has no timeout; use a task and a
shutdown flag.

**Blocking DNS is a trap and must be handled.** `std::net::ToSocketAddrs` resolves
synchronously and would freeze the entire VM — every task, not just the caller —
for the duration of a slow lookup. `net.dial` with a hostname must go through
tokio's `lookup_host`, which uses tokio's blocking pool. This is easy to get
wrong and easy to not notice in testing against `127.0.0.1`, so it belongs in the
implementation checklist explicitly.

**Error mapping**, using CPython's class names throughout so the hierarchy stays
one hierarchy:

| Condition | Exception |
|---|---|
| connect refused | `ConnectionRefusedError` |
| peer reset | `ConnectionResetError` |
| write to a closed peer | `BrokenPipeError` |
| local abort | `ConnectionAbortedError` |
| read/write deadline | `TimeoutError` |
| `io.read(r, n)` short | `EOFError` |
| bind to a used port, and everything else | `OSError` with the errno-shaped message `modules::io_err` already produces |

New classes: `ConnectionError` under `OSError`, with `ConnectionRefusedError`,
`ConnectionResetError`, `ConnectionAbortedError` and `BrokenPipeError` under it —
CPython's exact shape.

**Not in `net`, on purpose:**

- **UDP.** Not a stream, so it does not satisfy the io protocol and would need
  its own `send_to`/`recv_from` surface. Nothing in scope needs it. Adding it
  later is additive.
- **Unix domain sockets.** Same shape as TCP, genuinely useful for a sidecar,
  and cheap — but it is one more thing to freeze for a use case not yet on the
  table. `net.listen("unix:/run/oro.sock")` is the spelling if it returns.
- **TLS.** Out of scope, and it is the one item here that will force a large
  dependency (`rustls` plus a certificate store). The design keeps the seam
  clean: a TLS stream is just another Reader/Writer, so when it arrives,
  `std/http.oro` does not change by one line. That is worth designing for even
  while declining to build it.

---

## 5. The Rust/Oro boundary

The rule, in one line:

> **Anything that touches every byte goes in Rust. Anything that touches every
> request goes in Oro.**

The arithmetic behind it. The interpreter is 1.7×–5.7× slower than CPython. A
byte-at-a-time loop in Oro costs on the order of ten VM instructions per byte —
index, compare, branch, increment — so parsing a 300-byte request head
byte-by-byte is a few thousand instructions before any work happens. Doing the
same scan with `bytes.find` or `read_until` is one call into Rust. That is the
difference between a toy and a server.

But per-*request* work in Oro is fine. Splitting a head into eight header lines
and slicing each at its colon is roughly three Oro-level operations per line —
about two dozen per request. Routing is a dict lookup. Building a `Response` is
an object allocation. At those counts the interpreter's constant factor is
irrelevant next to the syscalls.

### The split

**Rust:**

| Piece | Why |
|---|---|
| `bytes` and its methods | `find`, `split`, slicing and comparison are the per-byte loops |
| `File`, `TcpStream`, `TcpListener` | syscalls |
| `read_until` on every reader | the scan for `\r\n\r\n` is per-byte, and must see inside the buffer |
| every reader's internal buffer | one syscall per 8 KiB instead of one per byte (§2) |
| `Buffer` | trivial, and it must be a Reader/Writer peer of the real ones |
| whole-stream read of a Rust stream | `stat` + one allocation, and it touches every byte (§2) |
| scheduler, `spawn`, channels, parking | the interpreter loop |
| `sys.stdout`/`stderr`/`stdin` | fds |

**Oro (`std/`):**

| Piece | Why |
|---|---|
| `io.copy`, and `io.read`'s general path | chunk-at-a-time loops over the protocol |
| the entire HTTP layer | per-request, not per-byte |
| `Request`, `Response`, headers, routing, keep-alive policy, chunked framing | policy |
| status codes, date formatting, URL decoding | tables and small loops |

### There is no Rust `http` module, and there should never be one

The interesting conclusion is that **no HTTP-specific Rust primitive is
needed**. Head parsing decomposes entirely into generic operations that earn
their place independently:

```python
head = conn.read_until(b"\r\n\r\n", 65536) # Rust: per-byte scan
lines = head.split(b"\r\n")                # Rust: per-byte scan
# then, per line, in Oro:
i = line.find(b":")                        # Rust: per-byte scan
name = line[0:i].to_str().lower()
value = line[i + 1:].strip().to_str()
```

Three Rust calls that are all justified as `bytes`/`io` building blocks, and
everything else in Oro. Chunked encoding is the same story: `read_until(b"\r\n")`
for the size line, `to_int(16)` to parse it, `io.read(conn, n)` for the chunk —
per chunk, not per byte.

So the boundary holds without a single protocol-aware primitive, and that is the
outcome to defend. **If HTTP parsing turns out to be too slow, the fix is a
faster generic primitive or a faster interpreter — never an `http` builtin.**
Baking a protocol into the language freezes a wire format into a runtime that
promises to freeze, and HTTP/1.1's edge cases are not the sort of thing to be
stuck with.

**The falsifiable version**, so this is not just an assertion: profile a
hello-world request. If Oro-level head parsing is more than ~30% of per-request
CPU, revisit — and the first thing to reach for is one more *generic* primitive
(a multi-delimiter `bytes.scan`), not `http.parse_request`.

---

## 6. The HTTP layer, in Oro

`std/http.oro`, in the style of `std/json.oro`: a state-holding class for the
parser (because Oro cannot rebind a captured variable from a nested function),
plain functions for everything else, no cleverness.

### Types

```python
class Request:
    def __init__(self, method, path, query, version, headers, body):
        self.method = method       # str, "GET"
        self.path = path           # str, percent-decoded, no query
        self.query = query         # dict of str -> str
        self.version = version     # str, "1.1"
        self.headers = headers     # dict, lowercased str -> str
        self.body = body           # Reader, always present, possibly empty
        self.params = {}           # filled by the router

    def header(self, name, default=None):
        return self.headers.get(name.lower(), default)

    def text(self):
        return io.read(self.body).to_str()

    def json(self):
        return json.parse(self.text())


class Response:
    def __init__(self, status=200, headers=None, body=b""):
        self.status = status
        self.headers = {}          # dict, str -> str
        if headers != None:
            self.headers = headers
        self.body = body           # bytes, or a Reader for a streamed body


def text(s, status=200):
    return Response(status, {"content-type": "text/plain; charset=utf-8"}, s.to_bytes())


def json_response(v, status=200):
    return Response(status, {"content-type": "application/json"}, json.stringify(v).to_bytes())
```

`body` is **always a Reader**, never `None` and never sometimes-bytes. A GET with
no body gets an empty `Buffer`. One shape means handlers never branch on it, and
`req.text()` is one line instead of three.

`Response.body` is bytes *or* a Reader, and that choice is what selects the
framing: bytes gets `Content-Length`, a Reader gets `Transfer-Encoding: chunked`.
That is the only place the two-shape rule is worth its cost, because it is the
distinction the protocol itself makes.

### The parser

```python
_MAX_HEAD = 65536
_MAX_HEADERS = 100

class _HeadParser:
    def __init__(self, raw):
        self.lines = raw.split(b"\r\n")
        self.n = len(self.lines)

    def fail(self, msg):
        raise BadRequest(msg)

    def request_line(self):
        parts = self.lines[0].split(b" ")
        if len(parts) != 3:
            self.fail("malformed request line")
        method = parts[0].to_str()
        target = parts[1].to_str()
        version = parts[2]
        if not version.startswith(b"HTTP/"):
            self.fail("unknown protocol")
        return (method, target, version[5:].to_str())

    def headers(self):
        out = {}
        i = 1
        while i < self.n and len(self.lines[i]) > 0:
            if i > _MAX_HEADERS:
                self.fail("too many headers")
            line = self.lines[i]
            c = line.find(b":")
            if c <= 0:
                self.fail("malformed header")
            name = line[0:c].to_str().lower()
            value = line[c + 1:].strip().to_str()
            existing = out.get(name)
            if existing == None:
                out[name] = value
            else:
                out[name] = existing + ", " + value
            i = i + 1
        return out


def read_request(r):
    raw = r.read_until(b"\r\n\r\n", _MAX_HEAD)
    p = _HeadParser(raw)
    method, target, version = p.request_line()
    headers = p.headers()
    path, query = _split_target(target)
    body = _body_reader(r, headers)
    return Request(method, path, query, version, headers, body)
```

`BadRequest(Exception)` is defined in `std/http.oro` and caught by the connection
loop, which answers 400 and closes. A `ValueError` from any `.to_str()` above is
caught in the same place and treated identically — a client that sends non-UTF-8
in a header gets a 400, not a traceback.

That catch has to be **narrow**, and this is the one place §1's `ValueError`
decision costs something concrete. `except ValueError` around `read_request`
only is correct: everything inside it is parsing attacker-controlled bytes, so
every `ValueError` it can raise really is a bad request — a failed decode, a
`read_until` that hit its limit, a `to_int` on a malformed chunk size. The same
`except` wrapped around the *handler* would turn a genuine bug in application
code into a 400, which is a misleading answer to a well-formed request. Scope is
the mechanism; keep it tight.

Header values are decoded as **UTF-8, strictly**. RFC 7230 says ISO-8859-1
historically; modern practice is ASCII or UTF-8 and every real server rejects the
rest. Rejecting is the safe direction.

**Obsolete line folding (a header continued on an indented line) is not
supported** and is a 400. RFC 7230 deprecated it, it is a request-smuggling
vector, and supporting it costs parser complexity forever.

### Body framing

```python
def _body_reader(r, headers):
    te = headers.get("transfer-encoding")
    if te != None and te.lower().endswith("chunked"):
        return _ChunkedReader(r)
    n = headers.get("content-length")
    if n == None:
        return io.buffer(b"")
    return _LimitReader(r, n.to_int())
```

Both are Oro classes with a single `read(n)` method, which is all it takes to be
a Reader:

```python
class _LimitReader:
    def __init__(self, r, remaining):
        self.r = r
        self.remaining = remaining

    def read(self, n):
        if self.remaining == 0:
            return b""
        if n > self.remaining:
            n = self.remaining
        chunk = self.r.read(n)
        self.remaining = self.remaining - len(chunk)
        return chunk
```

`_ChunkedReader` is the same shape with a small state machine: when its current
chunk is exhausted it calls `read_until(b"\r\n", 32)`, takes everything before
any `;` extension, `to_int(16)`, and then `io.read(self.r, n)` for exactly that
many bytes — the count form, because a chunk that spans two packets is the
normal case and not an edge case; size `0` reads and discards the trailer and
switches to EOF. **A `Transfer-Encoding` and a
`Content-Length` on the same request is a 400** — that ambiguity is the classic
request-smuggling vector and there is no reason to be lenient about it.

### Writing a response

```python
def write_response(w, req, resp, keep_alive):
    h = resp.headers
    streamed = type(resp.body) != "<class 'bytes'>"
    if streamed:
        h["transfer-encoding"] = "chunked"
    else:
        h["content-length"] = f"{len(resp.body)}"
    if not keep_alive:
        h["connection"] = "close"
    h["date"] = _http_date(time.time())

    parts = [f"HTTP/1.1 {resp.status} {_reason(resp.status)}\r\n".to_bytes()]
    for k, v in h.items():
        parts.append(f"{k}: {v}\r\n".to_bytes())
    parts.append(b"\r\n")

    if streamed:
        w.write(parts.join(b""))
        _write_chunked(w, resp.body)
    else:
        parts.append(resp.body)
        w.write(parts.join(b""))      # head and body: one call, one syscall
```

The head and a non-streamed body go out in **one `write`**, which is one
syscall — exactly what `BufWriter` used to buy, bought instead by the
list-append-and-`join` idiom the language already had (§2). The batching is
visible at the call site, there is no buffer left dirty if the function returns
early, and there is no `flush()` to forget after the last response of a
keep-alive connection, because there is no `flush()`.

A streamed body is genuinely more than one write — one per chunk — and that is
the honest cost of not knowing the length up front. It follows that a chunk
should be large enough to be worth a syscall (8 KiB, say), never a line at a
time.

### Keep-alive and the connection loop

```python
def _serve_conn(conn, handler):
    n = 0
    while n < _MAX_REQUESTS_PER_CONN:
        conn.set_timeout(_IDLE_TIMEOUT)
        req = _try_read(conn)               # None on clean EOF
        if req == None:
            return
        conn.set_timeout(_REQUEST_TIMEOUT)
        resp = _dispatch(handler, req)
        keep = _should_keep_alive(req, resp)
        write_response(conn, req, resp, keep)
        _drain(req.body)                    # unread body would desync the stream
        if not keep:
            return
        n = n + 1
```

There are no wrapper objects in that loop, and that is the visible payoff of
§2: `conn` buffers its reads by construction, so `read_until` works on it
directly, and `write_response` batches its own output, so there is nothing to
flush and nothing to close. The connection is the stream.

Rules, each of which is a bug if omitted:

- HTTP/1.1 keeps alive by default; HTTP/1.0 does not unless
  `Connection: keep-alive`. `Connection: close` from either side ends it.
- **An unread request body must be drained before the next request**, or the next
  parse reads the previous body as a request line. If the remainder is large
  (say over 64 KiB), close instead of drain — draining a 100 MB upload nobody
  read is a denial-of-service assist.
- A separate idle timeout (between requests) and request timeout (mid-request),
  because they mean different things.
- A cap on requests per connection, so a long-lived connection cannot pin a
  task's memory forever.
- Two catches, with two scopes, for the reason given under the parser above.
  `_try_read` wraps the parse and nothing else: `BadRequest` or `ValueError`
  from it means the client sent something malformed, so it answers 400 and
  closes, and it returns `None` only on a clean EOF. `_dispatch` wraps the
  handler call: any `Exception` → 500 with the traceback on stderr, and
  `BaseException` (i.e. `SystemExit`) re-raised.

### Routing

```python
class Router:
    def __init__(self):
        self.exact = {}       # "GET /health" -> handler
        self.patterns = []    # (method, [segments], handler)

    def add(self, method, path, handler):
        if path.find(":") < 0:
            self.exact[method + " " + path] = handler
        else:
            self.patterns.append((method, path.split("/"), handler))
        return self

    def dispatch(self, req):
        h = self.exact.get(req.method + " " + req.path)
        if h != None:
            return h(req)
        segs = req.path.split("/")
        for m, pat, fn in self.patterns:
            if m == req.method:
                params = _match(pat, segs)
                if params != None:
                    req.params = params
                    return fn(req)
        return Response(404, {}, b"not found")
```

Exact routes are an O(1) dict hit, which is where the overwhelming majority of
traffic lands. Parameterised routes (`/users/:id`) are a linear scan of a
segment matcher. No regex, no trie, no wildcard mounts, no middleware stack —
middleware in a language with first-class functions is `h => req => ...`, and it
does not need a framework to hold it.

`add` returns `self`, so routes chain in the reading order Oro prefers:

```python
r = http.Router().add("GET", "/health", ok).add("GET", "/users/:id", show)
http.serve("0.0.0.0:8080", req => r.dispatch(req))
```

### What is deliberately not in `http`

`Expect: 100-continue` (answer `417` and move on), HTTP/2, HTTP/3, WebSocket
upgrade, multipart parsing, cookie jars, sessions, static file serving,
compression, and a client. Several of those are worth having; none of them is
worth having *before* the server works, and each is expressible in Oro
afterwards on the primitives above.

---

## 7. Freeze implications

### Frozen at 1.0

These are the load-bearing spellings. Getting one wrong is expensive forever.

- **`bytes`**: the literal syntax, the `b[i]`→`int` / `b[i:j]`→`bytes` rule,
  ordering, hashability, and the method set in §1.
- **`str.to_bytes()` / `bytes.to_str()`**, UTF-8 only, no `encoding=`.
- **The io protocol**: `read(n)` returning ≤ n bytes with `b""` for EOF and
  `ValueError` for `n == 0`; `write(b)` writing all or raising, returning
  nothing; both sides `bytes`-only. This is the highest-stakes item on the list,
  because after 1.0 the names `read` and `write` are effectively reserved — no
  stdlib type, and by convention no user type, may ever give them another
  meaning.
- **`read_until(delim, limit)`** as a method on every reader, delimiter included
  in the result, `ValueError` at the limit.
- **The three names in `io`**: `io.read(r, n=None)`, `io.copy(dst, src)`,
  `io.buffer(b=b"")`. A module this small is only defensible if it stays this
  small; every addition after 1.0 is permanent.
- **`open(path, mode)`** with `"r"` / `"w"` / `"a"`, all three returning byte
  streams, and no `b` suffix. A later `"rw"` has to fit alongside these three
  spellings rather than replace them (§2).
- **`spawn`, `Task.join`, `chan`, `send`, `recv`, `close`**, channel iteration,
  and the uncaught-exception and program-termination rules.
- **`net.listen` / `net.dial` / `accept` / `peer` / `local` / `set_timeout` /
  `shutdown_write`**, and addresses as `"host:port"` strings.
- **The new exception classes**: `EOFError`, `ConnectionError` and its four
  children, `ChannelClosed`. Invalid UTF-8 raising a plain `ValueError` belongs
  here too — it is a decision about what *not* to freeze, and it is just as hard
  to undo.

### Deliberately left unfrozen

- **`std/http.oro` in its entirety**, shipping as provisional at 1.0 and freezing
  one release later. Router ergonomics are the single most-regretted API in every
  language that has one, and unlike the primitives below it, HTTP is not a
  building block — it is built *from* the blocks, and the blocks are what the
  freeze is for. Say so in the module docstring, loudly.
- **`select`** — not shipped, additive later (§3).
- **UDP, Unix sockets, TLS** — not shipped, additive later (§4).
- **`int.to_bytes` and binary packing** — waiting for a real binary protocol.
- **Preemption** — an implementation property with no surface, changeable at any
  time (§3).
- **Buffering** — that every reader buffers, how large the buffer is, and when it
  is allocated are Rust-side properties with no Oro-visible handle (§2). That is
  what makes them safe to change; it is also why exposing any of them later would
  be a new API rather than a tuning knob.
- **Buffer sizes and timeout defaults** — tunable, not API.
- **`"rw"`, `seek` and `tell`** — deferred out of M0–M6 deliberately, but unlike
  everything else on this list they are *intended*, and intended before 1.0
  rather than after it (§2). They are unfrozen because they are not designed
  yet, not because they are unwanted.

### What would be regretted

Stated as risks, not resolved:

1. **`read(n)` returning short reads.** It is correct and it is what every
    systems language does, and people will still write `conn.read(n)` where they
    meant `io.read(conn, n)` and ship a bug that only fires under load. Folding
    `read_exact` into `io.read` helps a little — there is one obvious answer now
    instead of a choice between two names — but it does not remove the hazard,
    it only names it. Mitigation is documentation, the
    raw-primitive-versus-whole-job framing in §2, and using `io.read(r, n)` in
    every example that reads a known length. This will still bite someone.
2. **Cutting text mode, and everything that stood in for it.** The most
    contentious area in the document, and review made the cut deeper rather
    than shallower: there is no text file object, and there is no `io.lines`,
    `io.read_text` or `io.write_text` either. Whole-file text went from one call
    to two lines. If a year of real programs shows that grating in scripts, the
    recovery is *differently named* helpers — never a text mode on `open`, which
    is now the one spelling that cannot come back, because it would be the
    second meaning for a letter that already has one.
3. **File I/O leaving `corpus/core/`.** Dropping the `b` moved the file test
    into `corpus/divergence/`, where it is a hand-reviewed baseline rather than
    a CPython-generated one (§2). The exposure is narrow — the mode letters and
    the shape of the file object, not `bytes` semantics, which stay fully
    oracled — but §1's decisive argument was an oracle argument, and this is the
    one place in the document where that argument was traded away instead of
    upheld. It was a knowing trade. It is also the first thing to re-examine if
    the reasoning in §2 ever starts to look motivated.
4. **Unbuffered writers, with no way to opt in.** `list.append` + `join` is
    right for a response, which is assembled in one place at one time. It is
    worse for a program that emits many small writes from many places — a
    logger, a line-oriented protocol — which now pays a syscall per write with
    no wrapper to reach for. If that turns out to be common, a buffered writer
    comes back, and `flush()` comes back with it, because the two cannot be
    separated. That is the whole trade: not having it is slow, having it
    truncates output silently when someone forgets.
5. **No size cap on `io.read(r)`.** The structural argument in §2 is sound —
    whole-file reads take a path the programmer chose — but it quietly assumes
    the path is trusted, and "a path the programmer chose" and "a path an
    attacker supplied" look identical at the call site in a program that serves
    uploads. The mitigation is a documentation line rather than a limit, on
    purpose, and a limit added later would be a breaking change to a function
    that shipped without one.
6. **Deferring `seek`/`tell` past `open`'s freeze.** The mode strings freeze at
    1.0 and random access is not designed yet, so a genuine building block will
    have to fit a spelling chosen without it in view. That ordering is
    backwards, and it is accepted only because `"rw"` drags buffer invalidation
    into a release that already contains the scheduler.
7. **Addresses as strings.** Cheap and Go-proven, but if IPv6, Unix sockets and
    TLS SNI all arrive, string parsing shows up in three places and an `Address`
    type starts to look right. Adding one later means two spellings forever.
8. **`Task.join()` semantics.** "Re-raise in the joiner, or print on drop" is
    the right rule and it is subtle. If it proves confusing, the fallback —
    always print, and have `join` return a result object — is worse. Keep the
    rule; document it with an example.
9. **No `select`.** If cutting it turns out to be wrong, the function form can
    be added without breaking anything. This is still the safest bet on the
    list.
10. **`chan()` versus `chan(0)`.** Unbuffered-by-default is Go's choice and is
    right (it forces you to think about backpressure), but `chan(0)` should be
    an explicit synonym rather than an error, so the reader never has to
    remember which one the bare call means.

---

## 8. Implementation plan

Ordered by dependency. The earliest milestone that serves a real HTTP request is
**M4**, and it does so *without* the `http` module.

### M0 — `bytes`

`Value::Bytes(Rc<Vec<u8>>)`, the literal in lexer and parser, the operators,
`HKey::Bytes`, `truthy`, `repr`/`display` matching CPython, the method set, and
`to_bytes`/`to_str` with `ValueError` on invalid UTF-8. Corpus: `bytes_basics`
in `corpus/core/`, generated by CPython.

No I/O, no concurrency. Self-contained, and the only thing everything else needs.

### M1 — the io protocol, synchronous

`open(path, "r"|"w"|"a")` returning byte streams, with text mode and the `b`
suffix both gone; `File` and `Buffer`; internal read buffering with lazy
allocation and `read_until` on every reader; unbuffered writers and no `flush`
anywhere; `sys.stdout`/`stderr`/`stdin` as real fd-backed streams; `std/io.oro`
with exactly `read` and `copy`, plus `io.buffer` re-exported from the Rust type.

The knock-on removals land here too: `File.readline`/`readlines`/iteration go,
`proc.run`'s `.stdout`/`.stderr` become `bytes`, and both corpus files that
depended on the old shapes are rewritten — `corpus/core/22_files.oro` in byte
mode and moved to `corpus/divergence/` (§2), `corpus/divergence/28_proc.oro`
with a regenerated baseline.

Still single-task and genuinely blocking.

**This milestone unlocks the largest parallel track in the plan** — see below.

### M2 — the `Task` refactor (no new surface)

Hoist the eleven per-execution fields out of `Vm` into a `Task`; `Vm` holds
`tasks`, a ready queue, and a current-task index; exactly one task exists.
`MAX_FRAMES` becomes per-task. Add `Step::Park`, unused.

**Zero user-visible change**, so the entire existing test suite and corpus is the
acceptance criterion. This is the riskiest change in the project and it lands
with no new API to debug alongside it. That is the point of separating it.

### M3 — the scheduler

tokio as the reactor (`net`, `time`, `rt`, `io-util` features only); the ready
queue and the parked-future set; `spawn`; `Task.join` with the drop-reports rule;
`chan`; channel iteration; `time.sleep` parking instead of
`std::thread::sleep`. Record the binary-size delta and verify the static musl
build still links.

### M4 — `net`, and the first serving demo

`net.listen`/`dial`, `TcpListener.accept`, `TcpStream` as Reader/Writer, timeouts,
error mapping, `SO_REUSEPORT`, non-blocking DNS.

**The demo is here**, roughly thirty lines of Oro on `net` + `io` alone:

```python
import io
import net

BODY = b"hello\n"
RESP = b"HTTP/1.1 200 OK\r\ncontent-length: 6\r\nconnection: close\r\n\r\n" + BODY

def handle(conn):
    conn.read_until(b"\r\n\r\n", 8192)
    conn.write(RESP)

ln = net.listen("0.0.0.0:8080", reuseport=True)
while True:
    spawn(handle, ln.accept())
```

It is a real HTTP response to a real browser, over a real socket, with a task per
connection — and it proves every hard part (bytes, protocol, scheduler, reactor,
parking) before any protocol code exists. Load-test it here: this is where
"tokio as reactor, VM as scheduler" is either right or is not.

### M5 — `std/http.oro`

`Request`/`Response`, the head parser, `_LimitReader`/`_ChunkedReader`, response
writing, chunked output, keep-alive, the error-to-status mapping. Replace the M4
demo with `http.serve`.

### M6 — routing, scale-out, shutdown

`Router`; the multi-VM launcher that forks N processes (or N OS threads, each
with its own `Vm`) sharing a `SO_REUSEPORT` listener; graceful shutdown via the
flag plus listener close; `http.serve(addr, handler, workers=N)`.

### What can be built in parallel

- **M0 and M2 are fully independent.** `bytes` touches `value.rs`, the lexer and
  the builtins; the `Task` refactor touches `vm/mod.rs` only. Two people, no
  collisions, start on day one.
- **After M1, the entire HTTP layer can be written and tested — with no sockets
  and no concurrency.** A `Buffer` is a Reader, so a canned request feeds the
  parser, and a `Buffer` is a Writer, so the response can be asserted byte for
  byte. That means M5's parser, chunked framing, header handling and response
  writing all proceed in parallel with M3 and M4, and arrive already tested
  against adversarial inputs (truncated heads, absurd `Content-Length`, folded
  headers, both framing headers at once) that are painful to produce over a real
  socket. This is the single biggest scheduling win available and it is a direct
  dividend of the one-protocol decision in §2.
- **`std/io.oro`'s two free functions** are pure Oro over the protocol and can
  be written against `Buffer` as soon as M1's Rust types exist.
- **The corpus work for M0 and M1** is CPython-generated and can be written
  before the Rust side compiles.

### Testing, and an honest gap

`bytes` itself is fully oracled — CPython has every literal, operator and method
in §1 with identical semantics, so `corpus/core/` gets real independent coverage
of the new value type. File I/O is a step weaker: the semantics are CPython's
binary-file semantics exactly, but the mode spelling is not, so the test is a
reviewed baseline in `corpus/divergence/` whose expected output can be checked
by hand against a CPython twin that says `"rb"` (§2).

**Networking and concurrency cannot be oracled.** CPython could in principle run
a socket test, but the timing, the scheduler and the task semantics are Oro's own
and there is no independent authority to check them against. They fall into the
corpus's known blind spot: `corpus/divergence/` baselines that are reviewed by
hand, which can catch a regression but cannot catch being wrong from the start.
The README already names this weakness and it applies here with full force.

Compensate deliberately, because a reviewed baseline is not enough for a
scheduler:

- **Rust integration tests** that drive a real loopback socket and assert on the
  wire bytes.
- **A conformance run against an external client** — `curl`, `ab`, and at least
  one HTTP/1.1 test suite — because the authority for HTTP is the RFC and other
  implementations, not a `.expected` file we wrote.
- **A load test in CI** with a fixed request count, asserting on completion and
  on no task leaking, which is the only way scheduler bugs surface reliably.
- **Adversarial parser fixtures** run through `Buffer`, in `corpus/divergence/`,
  reviewed as diffs.

---

## Summary of the surface

Everything this document proposes to add to the frozen language:

**Builtins:** `spawn(f, *args)`, `chan(n=0)`. `open(path, mode)` keeps its
three mode letters and returns a byte stream from all of them.

**Types:** `bytes`. And, not user-constructible: `File`, `TcpStream`,
`TcpListener`, `Buffer`, `Task`, `Channel`.

**Methods on streams:** `read(n)`, `write(b)`, `read_until(delim, limit)`,
`close()`, and `shutdown_write()` on `TcpStream` only.

**Modules:** `io` — `read(r, n=None)`, `copy(dst, src)`, `buffer(b=b"")`, and
nothing else; `net` (Rust); `http` (Oro, provisional).

**Exceptions:** `EOFError`, `ConnectionError` + `ConnectionRefusedError` /
`ConnectionResetError` / `ConnectionAbortedError` / `BrokenPipeError`,
`ChannelClosed`. Invalid UTF-8 raises a plain `ValueError`.

**Removed:** text mode on `open` and the `b` mode suffix; `File.readline`,
`File.readlines` and line iteration over a file; `sys.stdout`/`stderr`/`stdin`
as strings; `flush()`, from the whole language; `str` results from `proc.run`.

**Considered and not shipped:** `bytearray`, `BufReader`/`BufWriter`,
`io.lines`, `io.read_text`, `io.write_text`, `io.write`, `"rw"`, `seek`/`tell`,
`select`, UDP, Unix sockets, TLS.

Two new builtins, one new value type, three modules, seven exception classes,
three functions in `io`. For a language that gains networking, byte handling,
streams and concurrency, that is close to the floor — which is the only defence
any of it has.
