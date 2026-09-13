# The Oro reference

Everything Oro has, with the exact spelling of every call.

Oro is a frozen subset of Python: a lexer, a Pratt parser, a bytecode compiler
and a heap-framed VM with green threads, all in Rust, in one binary. Its thesis
is **one way to do each thing**, so a great deal of Python has been cut — and
almost everything that was cut *raises a message naming its replacement*. This
document is the map of what is left, written for two readers at once: a person
learning the language, and a model writing it.

**The interpreter is the authority.** Every signature and every example below
was run against `target/release/oro` at commit `5b2a96b`. Where older prose in
`README.md` or `docs/` disagrees, this document follows the binary.

Three habits will save you most of the trouble:

1. **Read [the argument rule](#2-the-argument-rule) first.** It governs every
   signature in the language: no default means positional-only, a default means
   keyword-only.
2. **When in doubt, run it.** The errors are written to teach: they name the
   spelling you should have used, usually with your own call rewritten.
3. **Don't reach for a Python spelling.** If Python has it and this document
   does not list it, it is almost certainly refused on purpose — see
   [Cut, and what to write instead](#6-cut-and-what-to-write-instead).

---

## Contents

1. [Running Oro](#1-running-oro) — the binary, the installer, the repo's scripts
2. [The argument rule](#2-the-argument-rule) — positional-only vs keyword-only
3. [Index of every callable](#3-index-of-every-callable) — one line each
4. [The language in one page](#4-the-language-in-one-page) — what differs from Python
   - [4.1 Literals and names](#41-literals-and-names)
   - [4.2 Operators: the whole set](#42-operators-the-whole-set)
   - [4.3 Lambdas, and chains instead of comprehensions](#43-lambdas-and-chains-instead-of-comprehensions)
   - [4.4 Type names are keywords, and are not callable](#44-type-names-are-keywords-and-are-not-callable)
   - [4.5 f-strings](#45-f-strings)
   - [4.6 Functions, defaults, and `apply`](#46-functions-defaults-and-apply)
   - [4.7 Blocks, scope and lifetime](#47-blocks-scope-and-lifetime)
   - [4.8 Classes and the dunder set](#48-classes-and-the-dunder-set)
   - [4.9 Exceptions, `finally` and `raise`](#49-exceptions-finally-and-raise)
   - [4.10 `match`](#410-match)
   - [4.11 Generators](#411-generators)
   - [4.12 Green threads](#412-green-threads)
   - [4.13 Modules and imports](#413-modules-and-imports)
5. [The callable surface](#5-the-callable-surface)
   - [5.1 Free builtins](#51-free-builtins)
   - [5.2 Conversions, on every value](#52-conversions-on-every-value)
   - [5.3 `str`](#53-str)
   - [5.4 `bytes`](#54-bytes)
   - [5.5 Numbers](#55-numbers)
   - [5.6 `list`](#56-list)
   - [5.7 `tuple`](#57-tuple)
   - [5.8 `dict`](#58-dict)
   - [5.9 `range`](#59-range)
   - [5.10 The collection protocol](#510-the-collection-protocol)
   - [5.11 Generators as values](#511-generators-as-values)
   - [5.12 `Pattern` and `Match`](#512-pattern-and-match)
   - [5.13 Streams: `File`, `Buffer`, `TcpStream`, `TcpListener`](#513-streams-file-buffer-tcpstream-tcplistener)
   - [5.14 `Task` and `Channel`](#514-task-and-channel)
   - [5.15 Modules](#515-modules)
   - [5.16 `std/http`](#516-stdhttp)
6. [Cut, and what to write instead](#6-cut-and-what-to-write-instead)
7. [Exceptions](#7-exceptions)
8. [For an AI agent working in this repo](#8-for-an-ai-agent-working-in-this-repo)

---

## 1. Running Oro

### The `oro` binary

There is one binary and one subcommand. Read `src/main.rs` if you need to be
sure; this is what it does.

```text
oro <file.oro> [args...]      compile and run a program
oro --version                 print "oro 0.2.0"
oro -V                        the same
oro --tokens <file.oro>       dump the token stream and exit
oro --ast <file.oro>          dump the parsed AST and exit
oro fmt <file.oro>            print the canonically formatted source
oro fmt --write <file.oro>    rewrite the file in place
oro fmt -w <file.oro>         the same
oro fmt --check <file.oro>    exit 1 if the file is not already canonical
```

That is the whole command line. There is no REPL, no `-c`, no `-m`, no
`-O`, no import path flag and no `fmt` over a directory or a list of files —
`oro fmt` takes exactly one path, so formatting a tree is a shell loop.

**Arguments after the script go to the program**, in `sys.argv`, with
`sys.argv[0]` set to the script path — Python's convention. Only the *first*
argument is inspected for `--version`/`--tokens`/`--ast`/`fmt`, so
`oro app.oro --version` passes `--version` to `app.oro`.

**Exit codes**, which are worth knowing because scripts depend on them:

| code | when |
|---|---|
| 0 | the program ran to completion |
| 1 | a lex, parse or compile error; an uncaught exception; an unjoined task failed |
| 2..255 | `sys.exit(n)` with that code |
| 64 | usage error (`EX_USAGE`): no file, an unknown flag, a bad `fmt` argument |
| 66 | the file could not be read (`EX_NOINPUT`) |
| 1 | `oro fmt --check` on a file that is not canonically formatted |

A diagnostic from the front end is prefixed with the file (`app.oro:3:7: …`); a
runtime error names its own file, because the fault may come from an imported
module. An uncaught exception in a *task* nobody joined prints
`task failed: app.oro:2:5: KeyError: 'u'` and sets the exit status to 1, while
the rest of the program keeps running.

### `install.sh`

The installer in the repo root is POSIX `sh` and does one job: detect the
platform, download the matching release archive from GitHub, **verify its
SHA256 against the published `SHA256SUMS`** (never skipped), unpack it, install
the binary, and run `oro --version` to prove it works.

```text
curl -fsSL https://raw.githubusercontent.com/OWNER/oro/main/install.sh | sh

ORO_VERSION=v0.2.0        pin a release instead of taking the latest
ORO_INSTALL_DIR=/opt/bin  install here instead of ~/.local/bin
```

It supports Linux (musl) and macOS on `x86_64` and `aarch64`, uses `curl` or
`wget`, never edits your shell rc files (it prints the `export PATH=…` line for
you to add), and falls back to `/usr/local/bin` with `sudo` only when
`~/.local/bin` is not writable. `GITHUB_OWNER` in its SUBSTITUTIONS block must
be set before publishing.

### The repo's own scripts

| script | what it is for | when to run it |
|---|---|---|
| `./corpus/run.sh [oro-binary]` | Runs every `corpus/core/*.oro` and `corpus/divergence/*.oro` against its `.expected` file and diffs. Prints `pass N fail N`, then reports `corpus/known-failing/` separately — those never fail the build, and one that starts passing is flagged for promotion. | After any change to the language. This is the gate: **fail must be 0.** |
| `./corpus/oracle.sh` | Regenerates the `.expected` files for `core/` and `known-failing/` **by running the programs under CPython 3**, translating `true`/`false`/`null` to Python's spellings on the way in and back on the way out. `divergence/` is excluded — those baselines are reviewed by hand. Needs `python3`. | Only when you have deliberately changed behaviour that CPython also defines. It rewrites tracked files, so check `git status` afterwards: a clean tree means Oro still agrees with CPython. |
| `./bench/run.sh` | Best-of-3 wall-clock benchmarks against CPython, and it **checks that both produce the same output** before reporting a time. `-n 5` for best-of-5, `--no-python` to skip the CPython column, `--md` for a Markdown table, `--oro path` to measure another binary, or name benchmarks to run a subset (`./bench/run.sh fib loop`). | When changing something on a hot path. |

`cargo test` runs the Rust unit and integration tests, including
`tests/fmt_test.rs`, which formats **every `.oro` file in the repository** and
requires it to be canonical, idempotent, and semantics-preserving. If you add an
`.oro` file anywhere in the tree, run `oro fmt --write` on it or that test
fails.

`examples/` holds eight runnable programs, each one screen with a header saying
how to run it: `classes.oro`, `client.oro` (an HTTP client that starts its own
server), `cli.oro` (argv, usage, exit codes), `concurrency.oro` (a worker pool),
`files.oro`, `json_api.oro`, `server.oro`, `text.oro` (the string surface).

---

## 2. The argument rule

This is the single most important thing to know, because it decides the shape of
every call in the language.

> **A parameter with no default is positional-only.**
> **A parameter with a default is keyword-only.**
> The `=` in a signature is the whole calling convention.

It applies everywhere — Oro `def`s, lambdas, builtins, methods, the native
modules, and the Oro-written standard library. There is no third category and no
syntax for one.

```oro
def connect(host, port, timeout=30, retries=3):
    return [host, port, timeout, retries]


print(connect("localhost", 8080))
print(connect("localhost", 8080, timeout=5))
print(connect("localhost", 8080, retries=1, timeout=5))
```

Both halves are enforced, and each refusal rewrites your call for you:

```oro
def connect(host, port, timeout=30):
    return [host, port, timeout]


try:
    connect("localhost", 8080, 5)
except TypeError as e:
    print(f"{e}")
try:
    connect(host="localhost", port=8080)
except TypeError as e:
    print(f"{e}")
```

```text
connect() takes 2 positional arguments but 3 were given — write
    `connect('localhost', 8080, timeout=5)`: a parameter with a default is
    passed by name
connect() got 'host' by name, but 'host' has no default and is passed by
    position — write `connect('localhost', 8080)`
```

### `null` is not a way to say "omitted"

A keyword whose default is *not* `null` refuses an explicit `null`. This closes
the second spelling `f(x=null)` would otherwise be for `f()`.

```oro
try:
    "abc".find("b", start=null)
except TypeError as e:
    print(f"{e}")
print("abc".find("b"))
```

```text
find(): start= must be int, not null — null does not mean "omitted"; leave
    start= out for the default
```

Where `null` genuinely *is* the default, passing it is fine and means that
value: `d.get(k, default=null)`, `http.Response(body=null)`,
`http.fetch(headers=null)`, and any `def f(x=null)` of your own.

```oro
print({"a": 1}.get("zz", default=null))


def f(x=null):
    return x


print(f(x=null), f())
```

### Defaults are evaluated on each call

Python's mutable-default trap is not in Oro. A default is an expression
evaluated per call that omits it, so it can also read earlier parameters and
enclosing locals.

```oro
def collect(x=[]):
    x.append(1)
    return x


print(collect(), collect())


def span(start, stop=start):
    return [start, stop]


print(span(2))
```

Constant defaults (numbers, strings, bools, `null`) are precomputed, which is an
optimisation you cannot observe.

### Variadic by nature, and how to forward

There is **no `*args`, no `**kwargs`, and no `*`/`**` at a call site** — all four
are parse errors naming the replacement. A function that takes any number of
values takes a list; one that takes named options takes a dict; and to forward
either into a call there is one builtin:

```oro
def area(w, h, scale=1):
    return w * h * scale


dims = [3, 4]
opts = {"scale": 10}
print(apply(area, args=dims), apply(area, args=dims, kwargs=opts))
```

Because `args` binds by position it can only fill required parameters, and
because `kwargs` binds by name it can only fill defaulted ones — the rule holds
through `apply` too. A handful of builtins are variadic *natively*, because
being variadic is what they are: `print`, `min`, `max`, `xs.zip(...)`, `spawn`,
`os.path.join`, and `http.Router().add` chaining.
---

## 3. Index of every callable

One line each, so a signature can be found without reading the document. A
parameter written `name=` is keyword-only; a bare one is positional-only. Section
numbers link to the full entry.

### Free builtins ([5.1](#51-free-builtins))

```text
print(*values, sep=" ", end="\n")   write a line; str only, no file=/flush=
len(x)                              length of str/bytes/list/tuple/dict/range/__len__
type(x)                             the type, for `type(x) == str`
abs(x)                              magnitude
min(a, b, ...) / max(a, b, ...)     extreme of two or more scalars (not of one iterable)
round(x, ndigits=)                  int without ndigits, float with it; ties to even
repr(x)                             the repr, running __repr__
chr(n) / ord(c)                     codepoint <-> one-character str
open(path, mode="r")                a File; "r"/"w"/"a", all bytes
apply(f, args=[], kwargs={})        forward a list by position and a dict by name
spawn(f, ...)                       start a task, returns a Task
chan(cap=0)                         a Channel; 0 is a rendezvous
yield_now()                         hand the CPU to the next ready task
set(...)                            always raises: sets are cut
```

### Conversions, on every value ([5.2](#52-conversions-on-every-value))

```text
x.to_str()      display form; bytes decodes as strict UTF-8
x.to_bytes()    str encodes UTF-8; a list/tuple of ints becomes octets
x.to_int(base=10)   parse or truncate; base= only applies to a str
x.to_float()    parse or widen
x.to_bool()     truthiness
x.to_list()     elements; a dict gives (key, value) pairs
x.to_dict()     from 2-element pairs
```

### `str` ([5.3](#53-str)) — and the same fifteen on `bytes` ([5.4](#54-bytes))

```text
s.upper() / s.lower()                       case-folded copy
s.strip(chars=, side="both")                trim a character SET; side= replaces lstrip/rstrip
s.split(sep=, maxsplit=-1, side="left")     no sep= splits on whitespace runs; side="right" is rsplit
s.find(sub, start=, end=, reverse=false)    index or -1; reverse=true is rfind
s.count(sub, start=, end=)                  non-overlapping occurrences
s.startswith(affix, start=, end=)           one affix, not a tuple
s.endswith(affix, start=, end=)             as above
s.replace(old, new, count=-1)               count= caps from the left
s.rm_prefix(affix) / s.rm_suffix(affix)     remove a LITERAL affix once
s.is_digit() / is_alpha() / is_alnum() / is_space()   whole-sequence, false when empty
b.hex()                                     bytes only: lowercase hex
b.scan(allowed)                             bytes only: leading run inside a byte set
```

### `list` ([5.6](#56-list)), `dict` ([5.8](#58-dict)), `range` ([5.9](#59-range))

```text
xs.append(x) / xs.extend(iterable)      -> null (a list's element mutators stay)
xs.pop(index=-1)                        remove and return; index is keyword-only
d.get(key, default=null)                fallback is keyword-only
d.pop(key) / d.pop(key, default=v)      raises KeyError / returns v
d.keys() / d.values()                   plain lists, not views
range(end, start=0, step=1)             the positional argument is the END
```

### The collection protocol ([5.10](#510-the-collection-protocol)) — list, tuple, dict, range, generator

```text
xs.len()                       element count
xs.sum(start=0)                fold with +
xs.min() / xs.max()            extreme element
xs.first() / xs.last()         end element; IndexError when empty
xs.reverse()                   a new reversed collection
xs.unique()                    first occurrences
xs.take(n) / xs.drop(n)        prefix / rest; n required
xs.chunk(n)                    list of n-sized lists
xs.flatten()                   one level, -> list
xs.zip(other, ...)             n-way, truncated to the shortest
xs.join(sep)                   str or bytes, from the separator's type
xs.map(f) / xs.filter(p)       type-preserving
xs.flat_map(f)                 -> list
xs.sort(f, reverse=false)   the only keyed sort; reverse= is stable descending
xs.group_by(f)                 -> dict of key -> list
xs.partition(p)                -> (matching, rest)
xs.find(p)                     first match or null; short-circuits
xs.any(p) / xs.all(p)          bool; predicate required; short-circuit
xs.count(p)                    how many satisfy p
xs.min_by(f) / xs.max_by(f)    element with the extreme key
xs.unique_by(f)                first per key
xs.take_while(p) / xs.drop_while(p)   leading run / its complement
xs.reduce(init, f)             f(acc, item), sequential
```

### `Pattern` and `Match` ([5.12](#512-pattern-and-match))

```text
p.search(s) / p.fullmatch(s)   Match or null
p.findall(s)                   list of str
p.finditer(s)                  list of Match (eager)
p.split(s) / p.sub(repl, s)    list of str / str
m.group(n)                     group text; n is required, 0 is the whole match
m.start(n) / m.end(n)          character offsets, -1 for a non-participating group
```

### Streams ([5.13](#513-streams-file-buffer-tcpstream-tcplistener))

```text
s.read(n)                    1..n bytes, b"" at EOF; n >= 1
s.write(b)                   writes all of b or raises; bytes only
s.read_until(delim, limit)   through the delimiter; ValueError past the limit
s.close()                    also happens when the last reference drops
buf.bytes()                  Buffer: written and not yet read
conn.shutdown_write()        half-close: FIN, keep reading
conn.set_timeout(secs)       scheduler deadline, null clears; expiry is TimeoutError
conn.set_nodelay(on)         bool required
ln.accept()                  next connection, as a TcpStream
conn.peer / conn.local / ln.local    address strings
```

### `Task` and `Channel` ([5.14](#514-task-and-channel))

```text
t.join()                     wait; returns the value or re-raises the exception
ch.send(v) / ch.recv()       block the task; ChannelClosed after close
ch.close()                   idempotent shutdown signal
for _, x in ch               until closed and drained
```

### Modules ([5.15](#515-modules))

```text
io.read(r)                          everything to EOF
io.read(r, fixed_size=n)            exactly n bytes, or EOFError
io.copy(dst, src)                   bytes copied
io.buffer(b)                        an in-memory Reader+Writer; b required

json.parse(text)                    str in, Oro values out
json.stringify(value, indent=null)  compact by default; str keys only

net.listen(addr, reuseport=false)   a TcpListener
net.dial(addr)                      a TcpStream

os.environ                          dict snapshot
os.getcwd()                         str
os.listdir(path)                    list of names; path required
os.remove(path) / os.mkdir(path)    -> null
os.path.exists/isfile/isdir(p)      bool
os.path.join(*parts)                POSIX join
os.path.basename(p) / dirname(p)    str
os.path.splitext(p)                 (root, ext)

proc.run(args, cwd=, env=, timeout=, check=true, quiet=false)
                                    a Completed: .returncode .ok .truncated .stdout .stderr

re.search/fullmatch(pattern, s)     Match or null
re.findall(pattern, s)              list of str
re.finditer(pattern, s)             list of Match
re.sub(pattern, repl, s)            str; no count
re.split(pattern, s)                list of str; no maxsplit
re.compile(pattern)                 a Pattern; no flags
re.match(...)                       always raises: use search or ^

sys.argv / sys.platform             list of str / "oro"
sys.exit(code)                      raises SystemExit; code required
sys.stdout / sys.stderr / sys.stdin File streams on fd 1/2/0, bytes only

time.time() / time.monotonic()      when / how long
time.sleep(secs)                    parks the calling task
```

### `std/http` ([5.16](#516-stdhttp))

```text
http.serve(addr, handler, ready=null, max_conns=512, max_requests=1000, timeout=30, drain=5.0)
                                            -> tally dict: accepted/refused/drained/forced
http.serve_conn(conn, handler, max_requests=1000, watch=null)   -> requests served
http.read_request(r)                        -> Request, or null at a clean EOF
http.write_response(w, req, resp, keep_alive=false)             -> null, one write
http.should_keep_alive(req, resp)           -> bool
http.Request(method, path, query, version="1.1", headers=null, body=null)
req.header(name, default=null) / req.text() / req.json()
http.Response(status, headers=null, body=b"", version="1.1", reason=null)
resp.header(name, default=null) / resp.bytes() / resp.text() / resp.json()
resp.close() / resp.reason_phrase()
http.text(s, status=200) / http.json_response(v, status=200)    -> Response
http.Router() / r.add(method, path, handler) / r.dispatch(req)
r.handler_for(method, req) / r.allowed(path)
http.BadRequest(message, status=400) / http.BadResponse(message)
http.fetch(method, url, headers=null, body=null, timeout=30, max_body=33554432, params=null)
http.stream(method, url, headers=null, body=null, timeout=30, params=null)
http.write_request(w, method, target, headers, body=b"")        headers required
http.read_response(r, method)                                   method required
http.parse_url(url)                         -> Url: .scheme .host .port .path .query .target
u.authority() / u.host_header()
http.quote(s, safe="/") / http.quote_plus(s, safe="")
http.unquote(s, plus=false) / http.encode_query(params)
```
---

## 4. The language in one page

Everything in this section is a place where writing Python out of habit produces
an error. The semantics that are *not* listed here are Python's.

### 4.1 Literals and names

```oro
print(1, 1.5, .5, 1e10, 1_000, 0xff, 0o17, 0b101)
print("text", b"octets", rb"raw\d", r"raw\d")
print(true, false, null)
print([1, 2], (1, 2), (1,), {"k": 1}, {})
```

- Both quote characters lex, and `oro fmt` normalises a string to double quotes.
  `r"…"` keeps a regex readable and `rb"…"` is its bytes form.
- **`true` / `false` / `null`**, lowercase. `True`, `False` and `None` are
  rejected by the lexer, naming the replacement. `repr` and `str` print the
  lowercase forms everywhere, including inside containers and in JSON, and
  `type(null)` is `<class 'null'>` (not `NoneType` — that name pointed at a word
  the language does not have).
- **Numeric literals are CPython's grammar exactly**, which is stricter than it
  looks: `01` is refused, `1__0` and `1_` are errors, and a literal may not run
  into a name (`123abc` is an error).
- **`bytes` is a separate type, not a flavour of `str`.** `b[i]` is an `int`,
  iteration yields `int`s, `len` counts octets. Cross over explicitly with
  `s.to_bytes()` (UTF-8, cannot fail) and `b.to_str()` (UTF-8, strict — invalid
  input raises `ValueError` rather than substituting replacement characters).
- **No sets.** `{1, 2}` and `set()` are both errors pointing at a dict or a
  list. Tuples stay: they are the only hashable composite.
- **`_` is a discard, not a name.** In an assignment or `for` target it binds
  nothing — `for _, v in xs` keeps the value, `a, _ = pair` drops the second
  component, `for _, _ in xs` repeats it with no duplicate-binding error, and
  `_ = f()` runs `f` for its effect and drops the result. Reading `_` back is a
  compile error (`` `_` is a discard … cannot be read ``), so a discarded value
  cannot be picked up again by accident. (`case _` in a `match` is the unrelated
  wildcard pattern, and still matches anything.)
- **Indentation is spaces.** A tab in leading whitespace is a hard error, not a
  width-8 guess.

### 4.2 Operators: the whole set

This table exists because the gaps surprise people. What is listed works; what
is not listed is a parse error.

| category | Oro has | Oro does not have |
|---|---|---|
| arithmetic | `+` `-` `*` `/` `//` `%` `**`, unary `-` `+` | — |
| comparison | `==` `!=` `<` `<=` `>` `>=`, chained (`1 < 2 < 3`) | `is`, `is not` |
| logical | `and` `or` `not` — **operands and result are `bool`** | truthiness; value-returning `or` |
| membership | `in`, `not in` | — |
| assignment | `=`, `+=` `-=` `*=` `/=`, tuple unpacking | `//=` `%=` `**=`, chained `a = b = c`, walrus `:=` |
| bitwise | *nothing* | `&` `\|` `^` `~` `<<` `>>` |
| conditional | *nothing* | `a if c else b` |
| indexing | `x[i]`, `x[i:j]`, `x[i:j:k]`, negative indices | — |

```oro
print(7 // 2, -7 // 2, 7 % 3, 2 ** 100, 10 / 4)
print(1 == 1.0, 1 == true, 1 < 2 < 3, 1 != 2, [1] == [1])
print(1 in [1, 2], "b" not in "ac", "k" in {"k": 1}, 3 in range(5))
n = 1
n += 2
n *= 3
print(n)
xs = [1, 2, 3, 4]
print(xs[-1], xs[1:3], xs[::2], xs[::-1])
a, b = 1, 2
a, b = b, a
print(a, b)
```

Three consequences worth stating plainly:

- **There are no bitwise operators at all**, so flag arithmetic has to be
  written with `//`, `%` and `*`, or kept in a dict of bools. `~`, `^` and `&`
  are not even lexed.
- **There is no conditional expression.** `x = "big" if n > 40 else "small"` is a
  parse error; write an `if`/`else` statement. This also means an f-string field
  cannot contain one.
- **`==` is the identity test too.** A function, generator, class, instance,
  module, stream, pattern, match, task or channel compares equal only to itself,
  which is why `is` could be cut. `len == len` is true (builtins compare by
  name) and `a.m == a.m` is true (bound methods compare by receiver and
  function).
- **There is no truthiness.** `if`, `while`, `and`, `or` and `not` take a
  `bool` and nothing else; a non-bool is a `TypeError` that names the test the
  caller meant (`if xs:` → `use len(xs) != 0`, `if n:` → `use n != 0`, `if x:`
  on a maybe-null value → `use x != null`). `and`/`or` are therefore pure
  boolean operators returning a `bool`, not CPython's value-returning versions,
  so `name or "anon"` is a fault rather than a silent fallback — the falsy-value
  trap where a legitimate `""`, `0` or `[]` is replaced cannot be written. A
  fallback is `d.get(k, default=…)` or an explicit `if x == null`; the migration
  table in [§6](#6-cut-and-what-to-write-instead) lists the rewrites.

### 4.3 Lambdas, and chains instead of comprehensions

A lambda is `params => expression`. Its parameters are plain names: no defaults,
no annotations, and the body is one expression. Anything more is a `def`.

```oro
double = x => x * 2
add = (a, b) => a + b
zero = () => 0
print(double(4), add(1, 2), zero())
```

**There are no comprehensions** — list, dict, set or generator. A comprehension
reads inside-out and stops composing after two steps; a chain reads in the order
it runs and stays flat:

```oro
xs = [1, 2, 3, 4, 5]
print(xs.filter(x => x > 1).map(x => x * x).filter(x => x < 30))
```

A callback with **two or more parameters destructures its element** — the same
way a nested `for` target (`for _, (a, b) in xs`) destructures its value slot —
and a dict's element is its `(key, value)` pair. **A chain is value-only: there
is no index parameter and no chain spelling for an index at all.** If you need
the index, that is one of the reasons to write a `for i, x in xs` loop instead
of a chain.

```oro
d = {"a": 1, "b": 2}
print(d.filter((k, v) => v > 1))
print([("x", 1), ("y", 2)].map((name, n) => f"{name}={n}"))
```

What counts is the parameters *without* a default; a defaulted one never counts
(it is keyword-only), a bound method's `self` does not, and `reduce` counts the
ones after the accumulator. A chain of `map`/`filter` runs in **one pass** over
the receiver, and `first()`, `take(n)`, `find`, `any` and `all` stop that pass
early. See [5.10](#510-the-collection-protocol) for the whole protocol.

### 4.4 Type names are keywords, and are not callable

`bool int float str bytes list tuple dict range File Buffer TcpStream
TcpListener Pattern Match Task Channel` are **keywords**. Each *is* the type, so
`type(x) == str` is the one way to ask what something is, and it works
identically for a user class.

```oro
print(type(1) == int, type("a") == str, type(b"") == bytes, type([]) == list)


class Point:
    def __init__(self, x):
        self.x = x


print(type(Point(1)) == Point)
```

Two things follow:

- **A type name cannot be a variable, parameter, function, class, import or
  `except … as` name.** `dict = {}` is a compile error naming the reason. It
  stays legal as a *member*: `def bytes(self)` in a class and `w.bytes()` are
  fine, and `{"list": 1}` is a string key.
- **A type name is not callable.** Conversion is a method on the value, and
  construction is a literal. `range(end, start=0, step=1)` is the one exception,
  because a range has no literal syntax.

```oro
n = 5
print("42".to_int(), n.to_str(), [[1, 2]].to_dict(), "ff".to_int(base=16))
try:
    int("42")
except TypeError as e:
    print(f"{e}")
```

```text
int() is not callable in Oro — write `x.to_int()` to convert a value, or the
    literal `0` to build an empty one.
```

There is no `isinstance`: `type(x) == C` is the type test, and the one thing
`isinstance` could do that `==` cannot — test a *subclass* — is what `except`
does.

### 4.5 f-strings

The full CPython 3.12 format mini-language is there:
`[[fill]align][sign][#][0][width][grouping][.precision][type]` over
`s d f e g x o b %` and the uppercase variants, plus `!r` / `!s` / `!a` and
nested specs.

```oro
x = 3.14159
n = 42
s = "hi"
print(f"{x:.2f} {n:05d} {n:,} {s:>5}|{s:<5}|{s:^5}|")
print(f"{n:x} {n:o} {n:b} {n:+d} {0.5:.0%} {x:.{n // 21}f}")
print(f"{s!r} {n!s} {[1, 2]} {{literal}}")
print(f"{n > 40} {s.upper()} {n + 1} {s.find('i')}")
```

Two restrictions:

- **A lambda cannot appear inside an f-string field.** `f"{xs.map(x => x)}"` is
  rejected with a message telling you to bind it to a name first. (f-string
  fields are parsed at code generation, so the scope pass never sees the lambda
  the emitter makes.)
- **No conditional expression inside a field**, because the language has none.

`f"{x}"` is also the replacement for `str(x)`: it runs `__str__`, and it still
parses as CPython, which keeps such programs inside the differential corpus.

### 4.6 Functions, defaults, and `apply`

See [the argument rule](#2-the-argument-rule), which is the whole calling
convention. In short: no default means positional-only, a default means
keyword-only, `f(x=null)` is refused where the default is not `null`, defaults
are evaluated per call, and `apply(f, args=[], kwargs={})` is how a list or a
dict is forwarded into a call.

### 4.7 Blocks, scope and lifetime

**`if`, `for`, `while` and `try` are real scopes** — names bound inside do not
leak out. This is the one divergence most likely to bite when porting Python.

```oro
last = null
for i, _ in range(3):
    last = i
print(last)
```

Without the first line, `print(last)` is a `NameError`. Since a name is created
by assigning to it, carrying a result out of a block means binding it
beforehand — choose the placeholder so a skipped block is loud, not silent.

**Every `for` binds an `(index, value)` pair.** There is one loop form;
destructuring selects what you want (`for i, v`, `for _, v`, `for i, _`), a
single binding (`for x in xs`) is an error, and a tuple element is a nested
pattern in the value slot (`for _, (a, b) in pairs`). What the index *is*
depends on the iterable:

| iterable | index | value |
|---|---|---|
| `list`, `tuple`, `str`, `bytes`, `range` | position (`0, 1, 2, …`) | the element |
| `dict` | the **key** | the value |
| generator | a 0-based counter | the yielded value |

There is no `enumerate` and no `range(len(xs))`: the index is built into the
loop. A **chain has no index at all** — it is value-only, so needing the index
is one of the reasons to write a loop rather than a chain.
`corpus/divergence/80_for_pairs.oro` pins the index for each type.

**`while` is for a condition, not a counter.** A `while name < bound` whose body
steps `name` by an integer constant is a hand-rolled `range` and a compile error
pointing at `for i, _ in range(…)`. A step by a runtime value
(`got = got + len(chunk)`), a non-`<`/`>` condition (the EOF drain
`while chunk != b""`), a compound condition, and `while true` are genuine
conditions and compile. This is the counter half of the loop rule, enforced;
the collection half (`for` over `while i < len(xs)`) rests on the convention.

- **`global`** mutates module-level state from a function. **There is no
  `nonlocal`**, and a nested function cannot rebind an enclosing local (reading
  one works, and an assignment makes a new local).
- **No `with`.** Reference counting plus block scope means a stream closes when
  its last reference dies, deterministically. `close()` exists for when end of
  scope is too late.
- Reference cycles are not collected (there is no cycle collector).

```oro
count = 0


def bump():
    global count
    count = count + 1


bump()
bump()
print(count)
```

### 4.8 Classes and the dunder set

Single inheritance, `__init__`, instance attributes, methods, class-level
attributes and `super()`. The dunder set is fixed: `__str__`, `__repr__`,
`__eq__`, `__len__`, the arithmetic dunders (`__add__` … `__pow__`) and the
comparisons (`__lt__`, `__gt__`, `__le__`, `__ge__`).

```oro
class Shape:
    kind = "shape"

    def __init__(self, n):
        self.n = n

    def __str__(self):
        return f"Shape({self.n})"

    def __repr__(self):
        return f"Shape(n={self.n})"

    def __eq__(self, other):
        return self.n == other.n

    def __lt__(self, other):
        return self.n < other.n

    def __len__(self):
        return self.n

    def __add__(self, other):
        return Shape(self.n + other.n)


class Square(Shape):
    def __init__(self, n):
        super().__init__(n)
        self.kind = "square"


a = Shape(2)
b = Square(3)
print(f"{a}", repr(b), a == Shape(2), a < b, len(b), (a + b).n, b.kind, Shape.kind)
```

Refused, each with its own message: `__new__`, `__getattr__`/`__getattribute__`,
`__setattr__`/`__delattr__`, `__slots__`, `__hash__`, multiple inheritance,
`metaclass=`, decorators (so no `@classmethod`/`@staticmethod` — write an
ordinary function beside the class), and type annotations.

**`__hash__` is permanently absent**, and that is a rule rather than an
omission: an instance of a class that defines `__eq__` is *not* a dict key —
key by the value it compares by, `d[(self.row, self.col)]`. An instance of a
class without `__eq__` is a key, by identity.

```oro
class Plain:
    pass


p = Plain()
print({p: 1}[p])


class Valued:
    def __eq__(self, other):
        return true


try:
    d = {Valued(): 1}
except TypeError as e:
    print(f"{e}")
```

### 4.9 Exceptions, `finally` and `raise`

`try` / `except E` / `except E as e` / `finally`, `raise E("msg")`, bare `raise`
to re-raise, and user classes via `class MyError(Exception)`. The hierarchy is
CPython's for the subset Oro has, and `except` matches by inheritance — see
[section 7](#7-exceptions).

```oro
def risky(n):
    try:
        if n == 0:
            raise ValueError("zero")
        return 10 // n
    except ValueError as e:
        return f"caught {e}"
    finally:
        print("cleanup runs either way")


print(risky(0), risky(5))
```

The restrictions, all compile errors:

- **No bare `except:`** — catch a named type.
- **No `try/except/else`** — put the else code after the `try`, or inside it.
- **No `raise X from Y`.**
- **`raise` needs an instance**, not the class: `raise ValueError()`, not
  `raise ValueError`.
- **No `return`, `break` or `continue` whose nearest enclosing block is a
  `finally`** — each would discard an exception in flight. They are fine inside
  the `try` and still run the `finally` on the way out.

```text
`return` inside `finally` would discard an exception in flight — move it after
    the `try` statement
```

- **No `assert`** (its behaviour would depend on an interpreter flag) and no
  `del` (block scope plus `d.pop(k)` cover it).

### 4.10 `match`

`match` is a **value switch**, not structural pattern matching. Allowed patterns
are literals, dotted names, and `_`. There is no fall-through, and when every
case is a literal it compiles to a jump table.

```oro
class Cmd:
    QUIT = "quit"


def handle(v):
    match v:
        case 1:
            return "one"
        case "a":
            return "letter a"
        case Cmd.QUIT:
            return "quitting"
        case _:
            return "other"


print(handle(1), handle("a"), handle("quit"), handle(9))
```

Refused: bare capture names (`case QUIT:` — in Python this silently rebinds and
matches everything), or-patterns (`case a | b:`), sequence, mapping and class
patterns, guards (`case x if c:`) and as-patterns.

### 4.11 Generators

A `def` (or a method) containing `yield` produces a generator. Generators drive
`for` lazily, feed the whole collection protocol, and never grow the native
stack.

```oro
def counter(n):
    for i, _ in range(n):
        yield i


def evens(n):
    for _, x in counter(n):
        if x % 2 == 0:
            yield x


print(counter(5).to_list(), evens(10).to_list())
print(counter(5).sum(), counter(5).map(x => x * 2).filter(x => x > 2).to_list())
```

A generator is single-use, and it has **no methods of its own**: there is no
`.send()`, no `.next()`, no `.close()` and no `.throw()`. `StopIteration`
exists as a class but exhaustion is what `for` ends on. Starting a chain or
calling a native collection method on a generator **drains it eagerly**, so an
infinite generator hangs there; it stays lazy in a `for` loop.

### 4.12 Green threads

Three names, and the values they hand back.

```oro
def work(n):
    return n * 2


t = spawn(work, 21)
print(t.join())

ch = chan(cap=2)
ch.send("a")
ch.close()
for _, msg in ch:
    print("got", msg)
print(yield_now())
```

- `spawn(f, …)` starts `f(…)` as a task and returns a `Task`; `t.join()` waits
  and returns what `f` returned, or re-raises what it raised. It needs a
  function defined in Oro, and not a generator function.
- `chan()` is a rendezvous, `chan(cap=n)` a buffer of `n`. `send`, `recv`,
  `close`, and `for x in ch` until closed and drained.
- `yield_now()` hands the CPU to the next ready task and answers `null`.

**Scheduling is cooperative**: a task yields at a channel operation, a socket
call, `time.sleep` or `yield_now()`, and nowhere else — so a CPU-bound handler
starves its peers. **There is no parallelism inside one VM**, which is why there
is no `select` and no locks: a shutdown flag is a variable and a cache is a
`dict`. **An uncaught exception kills only its task** — re-raised in whoever
joins it, or printed when the last handle drops (and the process exits 1).
Scaling past one core means N processes over `net.listen(addr, reuseport=true)`,
each with its own heap.

### 4.13 Modules and imports

```oro
import os
import json as j

print(os.path.basename("/x/y.txt"), os.getcwd().startswith("/"), j.stringify({"a": 1}))
```

`import a.b.c` and `import x as y` only. **No `from X import Y`** and no
`import *`.

A **multi-segment import must use `as`** to say what the name binds to, because
Python would bind the first segment and Oro binds the last:
`import os.path` is a compile error telling you to write
`import os.path as path`. For the built-in modules that spelling then fails at
run time (`ModuleNotFoundError: No module named 'os.path'`) — only the top-level
names are registered — so **reach a submodule through its parent**:
`import os`, then `os.path.basename(...)`. The built-in modules are `sys`, `os` (with `os.path`), `time`, `re`,
`proc`, `net`, and the Oro-written `io`, `json` and `http` shipped as source
inside the binary. User modules resolve against the script's directory, run
once, and are cached; `sys.path` cannot be changed and there is no other search
path. A missing module is `ModuleNotFoundError`, and the underscored internals
(`_io`, `_json`, `_pct`) are resolvable only from inside a stdlib module — from
user code they are `ModuleNotFoundError` too.
---

## 5. The callable surface

How to read an entry: the signature is exactly what the interpreter accepts, so
a parameter shown as `name=` is keyword-only and one shown bare is
positional-only. **CPython trap** marks a name CPython also has with different
behaviour — those are the ones most likely to be written wrongly.

### 5.1 Free builtins

These are all of them. There are no other global functions.

| signature | returns | raises | note |
|---|---|---|---|
| `print(*values, sep=" ", end="\n")` | `null` | `TypeError` for any other keyword | Takes `str`; `file=` and `flush=` are refused. Unbuffered, like every writer. |
| `len(x)` | `int` | `TypeError` if `x` has no length | `str`, `bytes`, `list`, `tuple`, `dict`, `range`, and a class with `__len__`. Not a `Channel`. |
| `type(x)` | the type (a `Type`, or the class for an instance) | — | `type(x) == str` is the type test. |
| `abs(x)` | `int` / `float` | `TypeError` on a non-number | |
| `min(a, b, …)` | the smallest | `TypeError` on one argument, `ValueError` on none | **CPython trap**: `min(xs)` over one iterable is cut — that is `xs.min()`. No `key=`. |
| `max(a, b, …)` | the largest | as `min` | as `min` |
| `round(x, ndigits=)` | `int` with no `ndigits`, `float` with it | `TypeError` on a non-number | Banker's rounding, as CPython. `round(2.5)` is `2`; `round(2.5, ndigits=0)` is `2.0`. |
| `repr(x)` | `str` | — | Runs `__repr__`; cycle-safe on containers. |
| `chr(n)` | one-character `str` | `ValueError` out of range | |
| `ord(c)` | `int` | `TypeError` unless `c` is one character | |
| `open(path, mode="r")` | `File` | `ValueError` on a bad mode, `FileNotFoundError`, `PermissionError` | `"r"`, `"w"`, `"a"`, all **bytes**. `"rb"` is refused by name. |
| `apply(f, args=[], kwargs={})` | whatever `f` returns | `TypeError` on a non-list/non-dict, or if the binding breaks the argument rule | The replacement for `f(*xs)` / `f(**d)`. |
| `spawn(f, …)` | `Task` | `TypeError` unless `f` is a non-generator Oro function | Arguments after `f` are bound by the rule, keywords included. |
| `chan(cap=0)` | `Channel` | `TypeError` on a positional argument, `ValueError` if negative | |
| `yield_now()` | `null` | — | A no-op when nothing else is ready. |
| `set(...)` | — | always `RuntimeError` | Exists only to say sets are cut. |

```oro
print(len("abc"), type(1.5), abs(-3), min(3, 1, 2), max(3, 1, 2))
print(round(2.5), round(2.675, ndigits=2), chr(65), ord("A"))
print(repr("a'b"), repr([1, "x"]), repr(b"\x00"))
print(1, 2, sep="-", end="!\n")
```

### 5.2 Conversions, on every value

Seven methods, available on every value, and the only conversions in the
language. None takes an argument except `to_int`.

| signature | returns | raises |
|---|---|---|
| `x.to_str()` | `str` | `ValueError` on `bytes` that is not UTF-8 |
| `x.to_bytes()` | `bytes` | `ValueError` if a list/tuple element is not an int in `0..256`; `TypeError` on other types |
| `x.to_int(base=10)` | `int` (bignum if large) | `ValueError` on a bad literal; `TypeError` if `base=` is given to a number |
| `x.to_float()` | `float` | `ValueError` on a bad literal |
| `x.to_bool()` | `bool` | — |
| `x.to_list()` | `list` | `TypeError` if not iterable |
| `x.to_dict()` | `dict` | `TypeError`/`ValueError` unless the elements are 2-element pairs |

```oro
n = 5
x = 2.9
print("42".to_int(), "ff".to_int(base=16), "0x1f".to_int(base=16), "777".to_int(base=8))
print("3.5".to_float(), n.to_str(), x.to_int(), "".to_bool(), [1].to_bool())
print(b"hi".to_str(), "hi".to_bytes(), [104, 105].to_bytes())
print("abc".to_list(), b"ab".to_list(), {"a": 1}.to_list(), [("k", 1)].to_dict())
try:
    n.to_int(base=16)
except TypeError as e:
    print(f"{e}")
```

- `to_str()` on `bytes` **decodes** (strict UTF-8); on anything else it is the
  value's display form. `to_bytes()` on `str` encodes UTF-8 and cannot fail;
  on a list or tuple of ints it builds octets, which is the only way to
  construct arbitrary bytes from computed values.
- `to_int()` truncates a float toward zero, and accepts a sign, `0x`/`0o`/`0b`
  when it agrees with `base=`, underscores and surrounding whitespace.
- `to_list()` on a `str` gives its characters, on `bytes` its ints, on a dict its
  `(key, value)` pairs. It is **the bridge** that puts a `str` or `bytes` into
  the collection protocol.
- `to_dict()` on a dict copies it.

### 5.3 `str`

Fifteen methods, and the same fifteen exist on `bytes`. A `str` is **not** a
collection: it has no `map`, no `sum`, no `len()` method — `len(s)` and
`s.to_list()` are the ways in.

| signature | returns | raises | note |
|---|---|---|---|
| `s.upper()` / `s.lower()` | `str` | `TypeError` on any argument | Full Unicode case folding. |
| `s.strip(chars=, side="both")` | `str` | `ValueError` on a bad `side`; `TypeError` on a non-str `chars` | **CPython trap**: `chars` is a character *set*, and `side=` replaces `lstrip`/`rstrip`. `"ping.png".strip(chars=".png")` is `"pi"`. |
| `s.split(sep=, maxsplit=-1, side="left")` | `list[str]` | `ValueError` on an empty `sep` or a bad `side` | **Two algorithms**: with no `sep=`, runs of whitespace separate and the ends are dropped; with one, every occurrence of that literal separates. `side="right"` is what `rsplit` did. |
| `s.find(sub, start=, end=, reverse=false)` | `int`, or `-1` | `TypeError` unless `reverse=` is a bool | Character indices. `reverse=true` replaces `rfind`; there is no `index`. |
| `s.count(sub, start=, end=)` | `int` | — | Non-overlapping, over the same window `find` searches. An empty needle counts `len + 1`. |
| `s.startswith(affix, start=, end=)` | `bool` | — | One affix, not a tuple of them. |
| `s.endswith(affix, start=, end=)` | `bool` | — | as above |
| `s.replace(old, new, count=-1)` | `str` | — | `count=` caps replacements from the left. |
| `s.rm_prefix(affix)` | `str` | `TypeError` on a non-str | A *literal* prefix, removed once if present — what people reach for `strip` and get a set. |
| `s.rm_suffix(affix)` | `str` | as above | |
| `s.is_digit()` / `s.is_alpha()` / `s.is_alnum()` / `s.is_space()` | `bool` | — | Whole-sequence, and `false` for the empty string, as CPython's. |

```oro
s = "Hello World"
print(s.upper(), s.lower(), s.find("o"), s.find("o", reverse=true), s.find("z"))
print(s.count("o"), s.count("o", start=5), s.startswith("He"), s.endswith("ld"))
print(s.replace("o", "0"), s.replace("o", "0", count=1))
print("  x  ".strip(), "xyaxy".strip(chars="xy"), f"[{' a '.strip(side='left')}]")
print("a,b,c".split(sep=","), "a b  c".split(), "a,b,c".split(sep=",", maxsplit=1))
print("a=b=c".split(sep="=", maxsplit=1, side="right"))
print("pre_x".rm_prefix("pre_"), "x.txt".rm_suffix(".txt"))
print("12".is_digit(), "ab".is_alpha(), "a1".is_alnum(), " ".is_space(), "".is_digit())
print(len(s), s[0], s[1:3], s[::-1], "abc" in s.lower(), ["a", "b"].join("-"))
```

`side=` takes `"both"`, `"left"` or `"right"` on `strip`, and `"left"` or
`"right"` on `split` (a split has no "both"). A bad value is a `ValueError` that
names the alternatives. `side=` without `maxsplit=` is deliberately a no-op
rather than an error.

There is no `join` on `str`: the sequence is the receiver, `xs.join(sep)`.
Also absent, each raising with its replacement: `lstrip`, `rstrip`, `rsplit`,
`rfind`, `index`, `zfill`, `format`, `title`, `center`, `ljust`, `rjust`,
`splitlines`, `encode`, `%` formatting.

### 5.4 `bytes`

The same fifteen names as `str`, with the same meanings over octets, plus two of
its own. Case folding is ASCII-only. Arguments are `bytes`, never `str`.

| signature | returns | raises | note |
|---|---|---|---|
| the fifteen `str` methods | as `str`, but `bytes` | as `str` | `b.strip(chars=b"xy")`, `b.split(sep=b",")`, `b.find(b"x")`, … |
| `b.hex()` | `str` | `TypeError` on any argument | Lowercase, no separator. |
| `b.scan(allowed)` | `int` | `TypeError` on a non-bytes | How many bytes at the front of `b` are all in `allowed`. `b.scan(a) == len(b)` asks "is every byte in this class"; with the delimiters excluded from `allowed` it is a multi-delimiter find. |

```oro
b = b"Hello"
print(b.upper(), b.lower(), b.hex(), b.find(b"l"), b.count(b"l"))
print(b.startswith(b"He"), b" a b ".strip(), b"a,b".split(sep=b","), b" a b ".split())
print(b.scan(b"Helo"), b.scan(b"xyz"), b.replace(b"l", b"L", count=1))
print(len(b), b[0], b[1:3], b"el" in b, [b"a", b"b"].join(b"-"))
for _, x in b"ab":
    print(x)
```

### 5.5 Numbers

`int` and `float`. Integers are inline `i64` promoting to arbitrary precision on
overflow — you never silently wrap. Numbers have **no methods of their own**
beyond the conversions in [5.2](#52-conversions-on-every-value); arithmetic is
operators, and `abs`, `min`, `max` and `round` are free builtins.

```oro
n = 5
x = 2.9
big = 2 ** 70
print(big, big + 1, big // 3, type(big))
print(7 // 2, -7 // 2, 7 % 3, -7 % 3, 10 / 4, 2 ** 0.5 > 1.41)
print(1.0, 0.1, 1e20, 1e-07, -0.0, 1 / 3)
print(x.to_int(), x.to_float(), n.to_str(), true.to_int())
print(round(1250, ndigits=-2), round(2.675, ndigits=2))
```

- Division by zero is `ZeroDivisionError` for both `//` and `/`.
- A `bool` is an `int` everywhere: `[10, 20][true]` is `20`.
- Comparing a number to a `str` is a `TypeError`, as in CPython.
- Float repr is CPython's shortest-round-trip form.
### 5.6 `list`

A list has three methods of its own — the element mutators — and the whole
collection protocol on top. The mutators answer `null`, as CPython's do; they
add and remove single elements, which is different from a *transform* like
`sort` or `reverse` (those return a new collection, below).

| signature | returns | raises | note |
|---|---|---|---|
| `xs.append(x)` | `null` | `TypeError` on the wrong arity | |
| `xs.extend(iterable)` | `null` | `TypeError` if not iterable | |
| `xs.pop(index=-1)` | the element | `IndexError` on an empty list or a bad index | **CPython trap**: the index is keyword-only, so that a positional argument to `pop` only ever means a dict key. |

```oro
xs = [3, 1, 2]
xs.append(4)
xs.extend([5])
print(xs, xs.pop(), xs.pop(index=0), xs)
xs = xs.sort(x => x)     # sort returns a new list — rebind it
print(xs)
xs = xs.reverse()        # reverse likewise
print(xs)
```

Absent: `insert`, `remove`, `index`, `clear`, `copy`, and `count(value)` —
`xs.count(p)` takes a *predicate* (see 5.10). There is **no in-place `sort` or
`reverse`**: `xs.sort(f)` and `xs.reverse()` return a new collection (5.10), so
the aliasing bug where a shared list is reordered under another name cannot be
written. `sorted`, `sort_in_place` and `reversed` each raise, naming the new
spelling.

### 5.7 `tuple`

A tuple has **no methods of its own**: it is the collection protocol plus the
conversions. It is immutable, it is the only hashable composite, and operations
that select or reorder give a tuple back.

```oro
t = (3, 1, 2)
print(t.len(), t.sum(), t.first(), t.sort(x => x), t.map(x => x * 2))
print(t[0], t[0:2], len(t), 1 in t, t + (9,), (1,) * 2)
d = {(1, "a"): "v"}
print(d[(1, "a")])
```

### 5.8 `dict`

Four methods of its own, plus the protocol. **Iterating a dict yields its
`(key, value)` pairs**, which is why there is no `.items()`.

| signature | returns | raises | note |
|---|---|---|---|
| `d.get(key, default=null)` | the value, or the default | `TypeError` on an unhashable key | **CPython trap**: `default=` is keyword-only. `default=null` is a real value here. |
| `d.pop(key)` | the value | `KeyError` if absent | |
| `d.pop(key, default=v)` | the value, or `v` | — | Giving `default=` at all is what turns the raise off. |
| `d.keys()` | `list` | `TypeError` on any argument | **CPython trap**: a list, not a view — indexable, and not live. |
| `d.values()` | `list` | as above | as above |

```oro
d = {"a": 1, "b": 2}
print(d.get("a"), d.get("z"), d.get("z", default=0), d.keys(), d.values())
print(d.pop("a"), d)
print({"a": 1}.pop("zz", default="fallback"))
for k, v in {"k": 1}:
    print(k, v)
print({"a": 1, "b": 2}.filter((k, v) => v > 1), {"a": 1}.map((k, v) => (k, v * 2)))
print(d.keys().sort(k => k), d.values().sum(), d.to_list(), "b" in d, len(d))
```

- **`k in d` tests keys**, always — membership is a hash lookup, not a walk.
- Insertion order is preserved, by keys, values and iteration alike.
- `d.map(f)` must answer a `(key, value)` pair, because a dict rebuilds into a
  dict; `d.filter(p)` passes the dict's own pairs through.
- `d.sum()` adds the *pairs* and is therefore a `TypeError` —
  `d.values().sum()` is what that reaches for.
- A dict iterator walks a snapshot taken when the loop starts.
- Absent: `items`, `update`, `setdefault`, `clear`, `copy`, `fromkeys`.

### 5.9 `range`

`range(end, start=0, step=1)` — the one type keyword that is callable, because a
range has no literal. **CPython trap**: the single positional argument is always
the *end*, and the other two bounds are named, so `range(2, 10)` is an error
that rewrites itself as `range(10, start=2)`.

```oro
print(range(5).to_list(), range(10, start=2).to_list(), range(10, start=2, step=3).to_list())
print(range(0, start=3, step=-1).to_list(), len(range(10, start=2)), 3 in range(5))
print(range(3) == range(3), range(3).map(x => x * 2), range(3).sum())
try:
    range(2, 10)
except TypeError as e:
    print(f"{e}")
```

`step=0` is a `ValueError`. A range is lazy in `len` and `in`, and the
protocol's reshaping operations give a list, since there is no range to rebuild.

### 5.10 The collection protocol

One uniform surface on **`list`, `tuple`, `dict`, `range` and generators**. It is
what replaced comprehensions, and it composes in reading order.

Two rules govern all of it:

- **Operations that select or reorder preserve the receiver's type**
  (`map`, `filter`, `sort`, `unique`, `unique_by`, `reverse`, `take`, `drop`,
  `take_while`, `drop_while`); operations that reshape the data give a `list`
  (`flatten`, `chunk`, `zip`), a `dict` (`group_by`) or a tuple of two
  (`partition`). A range or a generator has no shape to keep, so it gives a list.
- **A callback with two or more parameters destructures its element** the way a
  nested `for` target destructures its value slot; one parameter takes the
  element whole. There is no `enumerate` and **no index in a chain** — a chain
  is value-only; when you need the index you write a `for i, x in xs` loop.

#### Without a callback

| signature | returns | raises |
|---|---|---|
| `xs.len()` | `int` | `TypeError` on any argument |
| `xs.sum(start=0)` | the sum | `TypeError` on non-addable elements |
| `xs.min()` / `xs.max()` | an element | `ValueError` on an empty receiver |
| `xs.first()` / `xs.last()` | an element | `IndexError` on an empty receiver |
| `xs.reverse()` | same type | — |
| `xs.unique()` | same type | `TypeError` on an unhashable element |
| `xs.take(n)` / `xs.drop(n)` | same type | `TypeError` if `n` is missing or not an int, `ValueError` if negative |
| `xs.chunk(n)` | `list[list]` | `TypeError` as above, `ValueError` if `n < 1` |
| `xs.flatten()` | `list` | `TypeError` if an element is not iterable |
| `xs.zip(other, …)` | `list[tuple]` | `TypeError` on a non-iterable | 
| `xs.join(sep)` | `str` or `bytes` (the separator's type) | `TypeError` on a mismatched element |

```oro
xs = [3, 1, 2]
print(xs.len(), xs.sum(), xs.sum(start=10), xs.min(), xs.max(), xs.first(), xs.last())
print(xs.reverse(), xs.unique(), xs.take(2), xs.drop(1), xs.chunk(2))
print([[1, 2], [3]].flatten(), xs.zip([9, 8]), xs.zip([9, 8], [7, 6]))
print(["a", "b"].join(", "))
```

`xs.zip(a, b)` is a three-way zip truncated to the shortest — not a two-way one
with `b` discarded.

#### With a callback

| signature | returns | raises |
|---|---|---|
| `xs.map(f)` | same type | whatever `f` raises |
| `xs.filter(p)` | same type | |
| `xs.flat_map(f)` | `list` | `TypeError` if a result is not iterable |
| `xs.sort(f, reverse=false)` | same type | `TypeError` on incomparable keys |
| `xs.group_by(f)` | `dict` | `TypeError` on an unhashable key |
| `xs.partition(p)` | `(matching, rest)` | |
| `xs.find(p)` | an element, or `null` | |
| `xs.any(p)` / `xs.all(p)` | `bool` | `TypeError` if `p` is missing or not callable |
| `xs.count(p)` | `int` | as above |
| `xs.min_by(f)` / `xs.max_by(f)` | an element | `ValueError` on an empty receiver |
| `xs.unique_by(f)` | same type | `TypeError` on an unhashable key |
| `xs.take_while(p)` / `xs.drop_while(p)` | same type | |
| `xs.reduce(init, f)` | the accumulator | whatever `f` raises |

```oro
xs = [3, 1, 2]
print(xs.map(x => x * 2), xs.filter(x => x > 1), xs.flat_map(x => [x, x]))
print(xs.sort(x => x), xs.sort(x => x, reverse=true), xs.unique_by(x => x % 2))
print(xs.group_by(x => x % 2), xs.partition(x => x > 1))
print(xs.find(x => x > 1), xs.any(x => x > 2), xs.all(x => x > 0), xs.count(x => x > 1))
print(xs.min_by(x => -x), xs.max_by(x => -x))
print(xs.take_while(x => x > 0), xs.drop_while(x => x > 2))
print(xs.reduce(0, (acc, v) => acc + v), {"a": 1, "b": 2}.reduce(0, (acc, k, v) => acc + v))
```

**Every callback is required.** `xs.any()` for truthiness is
`xs.any(x => x)`, and `[0, 1, 2, ""].count()` read as a length and was not one.
`find`, `any` and `all` short-circuit, and the stop reaches back through a fused
chain.

**Keyed sorting is only `sort`, and it returns a new collection.** There is no
`sorted()`, no `key=`, and no in-place sort: `xs.sort(x => x)` is the elements'
own order, `reverse=true` is a *stable* descending sort (which `xs.sort(f).reverse()`
is not — reversing flips the ties too), and `reverse()` likewise returns a new
collection. `sort` returning a copy rather than mutating is a **silent
behavioural difference from Python's `list.sort`** — see the migration note in
[§6](#6-cut-and-what-to-write-instead).

**A chain runs in one pass.** Runs of `map`/`filter` between barriers are fused,
so no intermediate collection is built for them; `sort`, `reverse`,
`unique`, `unique_by`, `chunk`, `flatten`, `zip`, `group_by`,
`partition`, `min_by`, `max_by` and `take_while` are barriers that materialise.
The receiver is copied before the first callback runs, so a chain never sees its
own source change. The one observable ordering: `xs.map(f).map(g)` runs
`f(x0), g(y0), f(x1), g(y1)` — the order a `for` loop would.

### 5.11 Generators as values

A generator has **no methods of its own** — no `send`, `next`, `close` or
`throw`. What it has is the collection protocol and the conversions, and both
*drain* it.

```oro
def nums():
    for i, _ in range(5):
        yield i


print(nums().to_list(), nums().sum(), nums().max(), nums().len())
print(nums().sort(x => x), nums().take(2), nums().zip([9, 8]))
print(nums().filter(x => x % 2 == 0).sum(), nums().map(x => x * 2).take(3))
g = nums()
print(g.to_list(), g.to_list())
```

- A generator is **single-use**: draining it leaves it empty, as in CPython.
- `to_str()` and `to_bool()` ask about the *generator*, not its elements, so they
  do not drain it: an empty generator is truthy and prints as `<generator>`.
- Native methods cannot re-enter the interpreter, so the VM drains a generator
  receiver a frame at a time and retries the call — the native stack never grows
  with it, but an **infinite generator hangs** anywhere outside a `for` loop.

### 5.12 `Pattern` and `Match`

Regular expressions come from the `regex` crate's Thompson NFA: matching is
**guaranteed linear time**, so no pattern can ReDoS an Oro program. The price is
that **backreferences (`\1`) and lookaround (`(?=…)`, `(?<=…)`) are not
supported** — do it in two passes, matching candidates with `finditer` and
verifying in Oro.

`re.compile(pattern)` gives a `Pattern`; the module-level functions take the
pattern as a string and are otherwise identical. See
[5.15](#515-modules) for the module. **No flags argument anywhere** — write
flags inline, as `(?i)` — and **no `pos`, `endpos`, `count` or `maxsplit`**:
each extra positional is a `TypeError` that says so.

| signature | returns | raises |
|---|---|---|
| `p.search(s)` | `Match`, or `null` | `TypeError` on an extra argument or a non-str |
| `p.fullmatch(s)` | `Match`, or `null` | as above |
| `p.findall(s)` | `list[str]` | as above |
| `p.finditer(s)` | `list[Match]` — eager | as above |
| `p.split(s)` | `list[str]` | as above |
| `p.sub(repl, s)` | `str` | as above |
| `m.group(n)` | `str`, or `null` for a group that did not participate | `TypeError` if `n` is omitted, `IndexError` if negative or out of range |
| `m.start(n)` / `m.end(n)` | `int` character offset, `-1` for a non-participating group | as above |

```oro
import re

m = re.search(r"(\d+)-(\d+)", "ab 12-34 cd")
print(m.group(0), m.group(1), m.start(0), m.end(0))
p = re.compile(r"a(b)")
print(p.search("zab").group(1), p.findall("abab"), p.sub("X", "ab"), p.split("1ab2"))
print(re.search(r"z", "ab") == null, len(re.finditer(r"\d", "a1b2")))
for _, hit in re.finditer(r"(\w+)@(\w+)", "a@b c@d"):
    print(hit.group(1), hit.group(2))
try:
    m.group()
except TypeError as e:
    print(f"{e}")
```

`m.group(0)` is the whole match — the index is **required**, which is both
unambiguous and still valid CPython.
### 5.13 Streams: `File`, `Buffer`, `TcpStream`, `TcpListener`

The io protocol is **two methods and a naming convention**, not a declared type:

```text
read(n)   -> bytes    # 1..n bytes; b"" at EOF; MAY return fewer than n
write(b)  -> null     # writes all of b, or raises
```

There is no `class Reader`, nothing to inherit from and no registration: any
object with a `read` of that shape is a Reader, which is why `io.read` and
`io.copy` work on a class you wrote this afternoon. Everything is **bytes** in
both directions — no stream anywhere accepts a `str`. Every reader buffers
internally (8 KiB, allocated on first read), and **there is no `flush()`
anywhere**, because every writer is unbuffered.

| method | on | returns | raises |
|---|---|---|---|
| `s.read(n)` | `File`, `Buffer`, `TcpStream` | `bytes`, `b""` at EOF | `ValueError` if `n < 1`, on a closed stream, or on one with no read side |
| `s.write(b)` | `File`, `Buffer`, `TcpStream` | `null` | `TypeError` on a `str`; `ValueError` on a reader or a closed stream; `BrokenPipeError` |
| `s.read_until(delim, limit)` | `File`, `Buffer`, `TcpStream` | `bytes` up to **and including** `delim` | `ValueError` if `limit` bytes arrive without it, on an empty `delim`, or `limit < 1` |
| `s.close()` | all four | `null` | — |
| `b.bytes()` | `Buffer` | everything written and not yet read | — |
| `c.shutdown_write()` | `TcpStream` | `null` | `ValueError` on a non-socket |
| `c.set_timeout(secs)` | `TcpStream` | `null` | `ValueError` on a non-socket or a non-positive number; `null` clears it |
| `c.set_nodelay(on)` | `TcpStream` | `null` | `TypeError` on a non-bool |
| `ln.accept()` | `TcpListener` | `TcpStream` | `ValueError` on a closed listener; `OSError` family |
| `c.peer` / `c.local` | `TcpStream` | `str` address | — |
| `ln.local` | `TcpListener` | `str` address | — |

```oro
import io
import os

path = "/tmp/oro_reference_demo.txt"
f = open(path, mode="w")
f.write(b"line1\nline2\n")
f.close()
print(io.read(open(path)))
r = open(path)
print(r.read(5), r.read_until(b"\n", 100))
r.close()
b = io.buffer(b"abcdef")
print(b.read(2), b.bytes(), io.read(b))
w = io.buffer(b"")
w.write(b"xy")
print(w.bytes(), repr(w), type(w) == Buffer)
os.remove(path)
```

- **`read(n)` may return fewer than `n` bytes without being at EOF** — `n` is a
  maximum, and the answer is "what has arrived". EOF is `b""`, not an exception.
  Code that needs exactly `n` bytes calls `io.read(r, fixed_size=n)`.
- `read(0)` is a `ValueError` rather than a second thing `b""` can mean, and
  `read()` with no argument is refused, pointing at `io.read(r)`.
- **A `TcpListener` is not a stream of bytes**: it has `accept`, `close` and
  `local`, and reading it is a `ValueError`.
- **No `seek`, no `tell`, no read-write mode, no line iterator, no text mode and
  no `encoding=`.** Lines are
  `io.read(f).to_str().strip(chars="\n", side="right").split(sep="\n")`.
- `sys.stdout`, `sys.stderr` and `sys.stdin` are real `File` streams on fd
  1/2/0, and take `bytes`: `sys.stdout.write("hi")` is a `TypeError`, while
  `print("hi")` and `sys.stdout.write(b"hi")` both work and interleave in
  program order.
- **Every socket call parks the green thread, not the VM** — `accept`, `read`,
  `write` and `net.dial` are ordinary blocking-looking calls over non-blocking
  sockets. There is no `async`, no `await`, and no second colour of function.
  `set_timeout` is a scheduler deadline covering a whole operation, and its
  expiry raises `TimeoutError`.

```oro
import net

ln = net.listen("127.0.0.1:0")


def client(addr):
    c = net.dial(addr)
    c.write(b"ping")
    c.shutdown_write()
    print("client got", c.read(4))
    c.close()


spawn(client, ln.local)
conn = ln.accept()
conn.set_timeout(5)
conn.set_nodelay(true)
print("server got", conn.read(4), conn.peer.startswith("127.0.0.1:"))
conn.write(b"pong")
conn.close()
ln.close()
```

### 5.14 `Task` and `Channel`

Neither type is constructible: `spawn` and `chan` are the only ways to get one,
and there is no `Task(...)` or `Channel(...)`.

| signature | returns | raises |
|---|---|---|
| `t.join()` | what the task's function returned | whatever the task raised, re-raised in the joiner (a second `join` re-raises the same one) |
| `ch.send(v)` | `null` | `ChannelClosed` on a closed channel |
| `ch.recv()` | the next value | `ChannelClosed` when the channel is closed **and** drained |
| `ch.close()` | `null` | — |
| `for x in ch` | iterates until closed and drained | — |

```oro
def work(n):
    return n * 2


t = spawn(work, 21)
print(type(t) == Task, t.join(), t.join())

ch = chan(cap=2)
ch.send(1)
ch.send(2)
print(repr(ch), ch.recv(), ch.recv())
ch.close()
print(repr(ch))
try:
    ch.send(3)
except ChannelClosed as e:
    print(f"{e}")


def failing():
    raise ValueError("in task")


try:
    spawn(failing).join()
except ValueError as e:
    print("join re-raises", f"{e}")
```

- A `Channel` is an ordinary value: store it in a dict, capture it in a closure,
  send it down another channel.
- `len(ch)` is **not** supported (`TypeError`), despite a source comment saying
  otherwise.
- An unjoined failed task prints `task failed: file:line:col: Class: message`
  when its last handle drops, and the process exits 1.

### 5.15 Modules

#### `io` — written in Oro

| signature | returns | raises |
|---|---|---|
| `io.read(r)` | `bytes`, everything until EOF | whatever the stream raises |
| `io.read(r, fixed_size=n)` | exactly `n` bytes | `EOFError` if the stream ends short; `ValueError` if `n < 0` |
| `io.copy(dst, src)` | `int`, bytes copied | whatever either stream raises |
| `io.buffer(b)` | `Buffer` | `TypeError` if `b` is omitted or not `bytes` |

```oro
import io

print(io.read(io.buffer(b"whole stream")), io.read(io.buffer(b""), fixed_size=0))
r = io.buffer(b"0123456789")
print(io.read(r, fixed_size=4), io.read(r, fixed_size=6), io.read(r))
print(io.copy(io.buffer(b""), io.buffer(b"zz")), io.buffer(b"").bytes())
try:
    io.read(io.buffer(b"ab"), fixed_size=5)
except EOFError as e:
    print(f"{e}")
```

Three functions and a constructor, and that is the module. There is deliberately
no `io.write` (`w.write(b)` already writes everything or raises), no `io.lines`,
no `io.read_text` and no `io.write_text`. `io.read(r)` holds the whole stream in
memory, with no size cap: the bounded reads are `r.read(n)` and
`r.read_until(delim, limit)`.

#### `json` — written in Oro, codec in Rust

| signature | returns | raises |
|---|---|---|
| `json.parse(text)` | `dict`/`list`/`str`/`int`/`float`/`bool`/`null` | `TypeError` on `bytes`; `ValueError` naming the character offset |
| `json.stringify(value, indent=null)` | `str` | `TypeError` on a non-`str` dict key or an unencodable value |

```oro
import json

print(json.parse('{"a": [1, 2.5, true, null]}'))
print(json.stringify({"a": 1, "b": [true, null]}))
print(json.stringify({"a": 1}, indent=2))
try:
    json.parse("{")
except ValueError as e:
    print(f"{e}")
```

`indent=null` is the compact form with **no incidental whitespace at all** — not
even CPython's `", "` — and an int switches to pretty output. `parse` takes
`str`, not octets: the decode is a step the program takes, `b.to_str()`. Dict
keys must be `str` rather than being silently stringified. Nesting is capped at
10 000 levels.

#### `net` — TCP, in Rust

| signature | returns | raises |
|---|---|---|
| `net.listen(addr, reuseport=false)` | `TcpListener` | `OSError` (a port in use), `TypeError` on a positional or non-bool `reuseport` |
| `net.dial(addr)` | `TcpStream` | `ConnectionRefusedError`, `TimeoutError`, `OSError` family |

Two constructors and two objects, and that is the module — everything else a
socket can do is a method on the stream (see 5.13). **Addresses are strings**,
`"host:port"`, with Go's bracket form for IPv6 (`"[::1]:8080"`); `":0"` binds
any free port and `ln.local` tells you which. There is no `Address` type, no
UDP, no Unix sockets and no TLS. `reuseport=true` is how N processes share one
port.

#### `os` and `os.path` — in Rust

| signature | returns | raises |
|---|---|---|
| `os.environ` | `dict` (a snapshot, not live) | — |
| `os.getcwd()` | `str` | `OSError` family |
| `os.listdir(path)` | `list[str]` | `FileNotFoundError`, `PermissionError` |
| `os.remove(path)` | `null` | `FileNotFoundError`, `PermissionError` |
| `os.mkdir(path)` | `null` | `OSError` if it exists, `PermissionError` |
| `os.path.exists(p)` / `isfile(p)` / `isdir(p)` | `bool` | `TypeError` on a non-str |
| `os.path.join(*parts)` | `str` | `TypeError` on a non-str |
| `os.path.basename(p)` / `dirname(p)` | `str` | as above |
| `os.path.splitext(p)` | `(root, ext)` | as above |

```oro
import os

print(os.path.join("a", "b", "c"), os.path.join("/x", "/y"))
print(os.path.basename("/x/y.txt"), os.path.dirname("/x/y.txt"), os.path.splitext("/x/y.txt"))
print(os.path.exists("/"), os.path.isfile("/"), os.path.isdir("/"))
print(os.getcwd().startswith("/"), type(os.environ) == dict)
d = "/tmp/oro_reference_dir"
if not os.path.exists(d):
    os.mkdir(d)
print(os.listdir(d), os.path.isdir(d))
```

**CPython trap**: `os.listdir(path)` requires its argument (CPython defaults to
`"."`). `os.path.join` follows POSIX — an absolute later component resets the
path. `os.mkdir` on a directory that exists is an `OSError`, and there is
nothing to undo it: the module has no `rmdir`, and `os.remove` is files only.
Also absent: `os.makedirs`, `rename`, `stat`, `walk`, `getenv`, `getpid` and
`system`. `os.environ` is a snapshot taken at import, so writing to it changes
nothing outside the program.

#### `proc` — one function

`proc.run(args, cwd=, env=, timeout=, check=true, quiet=false)` returns a
`Completed` with `.returncode`, `.ok`, `.truncated`, `.stdout` and `.stderr`.

| raises | when |
|---|---|
| `CommandError` | a nonzero exit, unless `check=false` |
| `TypeError` | a bare string, a list whose program contains whitespace, a second positional, or `capture_output=`/`text=` |
| `FileNotFoundError` / `PermissionError` | the program is missing or not executable |
| `TimeoutError` | `timeout=` elapsed |

```oro
import proc

r = proc.run(["echo", "hi"], quiet=true)
print(r.returncode, r.ok, r.stdout, r.stderr, r.truncated, type(r))
print(proc.run(["sh", "-c", "exit 3"], check=false, quiet=true).returncode)
print(proc.run(["pwd"], cwd="/tmp", quiet=true).stdout)
print(proc.run(["sh", "-c", "echo $FOO"], env={"FOO": "bar"}, quiet=true).stdout)
try:
    proc.run(["sh", "-c", "exit 3"], quiet=true)
except CommandError as e:
    print(f"{e}")
```

`args` is **always a list of separate strings** that go straight to `execve`:
there is no `shell=True`, so injection is impossible by construction (want a
shell? `["sh", "-c", cmd]`, and own it). Output is **both** streamed live and
captured — `quiet=true` drops the live tee — and `.stdout`/`.stderr` are
`bytes`, capped at 64 MiB retained per stream, with `.truncated` saying so.
`check=true` is the default, unlike CPython's `subprocess.run`, which is part of
why the module has a different name.

#### `re` — in Rust

| signature | returns | raises |
|---|---|---|
| `re.search(pattern, s)` | `Match`, or `null` | `TypeError` on an extra argument (no `flags`) |
| `re.fullmatch(pattern, s)` | `Match`, or `null` | as above |
| `re.findall(pattern, s)` | `list[str]` | as above |
| `re.finditer(pattern, s)` | `list[Match]` — eager | as above |
| `re.sub(pattern, repl, s)` | `str` | `TypeError` on a 4th argument (no `count`) |
| `re.split(pattern, s)` | `list[str]` | `TypeError` on a 3rd argument (no `maxsplit`) |
| `re.compile(pattern)` | `Pattern` | `ValueError` on a bad pattern; `TypeError` on a 2nd argument |
| `re.match(...)` | — | always `RuntimeError`: it anchors at the start, which is almost never meant |

See [5.12](#512-pattern-and-match) for `Pattern` and `Match`, and for what the
engine does not support.

#### `sys`

| name | is | note |
|---|---|---|
| `sys.argv` | `list[str]` | `argv[0]` is the script path |
| `sys.exit(code)` | raises `SystemExit` | **CPython trap**: the code is required. Catchable; uncaught it sets the process status |
| `sys.platform` | `"oro"` | |
| `sys.stdout` / `sys.stderr` / `sys.stdin` | `File` streams on fd 1/2/0 | unbuffered, take `bytes` |

```oro
import sys

print(sys.platform, type(sys.stdout) == File, sys.argv[0].endswith(".oro"))
sys.stdout.write(b"straight to fd 1\n")
try:
    sys.exit(0)
except SystemExit as e:
    print("SystemExit carries the code:", f"{e}")
```

#### `time`

| signature | returns | note |
|---|---|---|
| `time.time()` | `float` epoch seconds | for **when**; can jump or go backwards |
| `time.monotonic()` | `float` seconds from a fixed point | for **how long**; only increases |
| `time.sleep(secs)` | `null` | suspends the **calling task**, not the process |

```oro
import time

start = time.monotonic()
time.sleep(0.01)
print(time.monotonic() > start, time.time() > 1000000)
```

There is no `datetime`: calendar handling is a library to be written in Oro on
top of `time`.
### 5.16 `std/http`

HTTP/1.1 in both directions, **written entirely in Oro** on top of `io` and the
`bytes` methods — there is no HTTP-specific Rust primitive anywhere. Read
`std/http.oro` when you need the policy; it is meant to be read and changed.

#### The server

| signature | returns | raises |
|---|---|---|
| `http.serve(addr, handler, ready=null, max_conns=512, max_requests=1000, timeout=30, drain=5.0)` | `dict` tally: `accepted`, `refused`, `drained`, `forced` | `OSError` family from the bind |
| `http.serve_conn(conn, handler, max_requests=1000, watch=null)` | `int`, requests served | whatever the connection raises |
| `http.read_request(r)` | `Request`, or `null` at a clean EOF | `BadRequest` on a malformed head |
| `http.write_response(w, req, resp, keep_alive=false)` | `null` — head and sized body in **one** `write` | `ValueError` on an invalid header |
| `http.should_keep_alive(req, resp)` | `bool` | — |

`handler` is `req => Response`. A handler that raises becomes a 500 for that
request and nothing more; the server keeps running and logs the class.

```oro
import http

ready = chan()


def handler(req):
    if req.path == "/json":
        return http.json_response({"path": req.path, "q": req.query})
    if req.path == "/boom":
        raise ValueError("handler failure")
    return http.text("hello\n")


def run():
    return http.serve("127.0.0.1:0", handler, ready=ready, max_conns=8, timeout=5)


server = spawn(run)
ln = ready.recv()
addr = ln.local
print(http.fetch("GET", f"http://{addr}/").text())
print(http.fetch("GET", f"http://{addr}/json", params={"q": "a&b"}).text())
print("a raising handler ->", http.fetch("GET", f"http://{addr}/boom").status)
ln.close()
tally = server.join()
print("tally:", tally.keys().sort(k => k), tally["accepted"] > 0)
```

**Shutdown is closing the listener, and nothing else.** `ready=` is a channel
`serve` sends the bound listener down before accepting anything — which is both
how you learn the port when you bound `":0"` and how another task stops the
server. `ln.close()` wakes the parked `accept()`, every connection is told this
is its last round, idle keep-alive connections close at once, in-flight requests
get a bounded `drain`, and `serve` returns its tally.

#### `Request` and `Response`

| signature | returns / holds |
|---|---|
| `http.Request(method, path, query, version="1.1", headers=null, body=null)` | fields `.method`, `.path`, `.query` (a dict), `.version`, `.headers` (a dict, lowercased keys), `.body` (**always a Reader**), `.params` (a dict, filled by `Router`) |
| `req.header(name, default=null)` | the header, case-insensitively |
| `req.text()` | the body as `str` — consumes it |
| `req.json()` | the body parsed — consumes it |
| `http.Response(status, headers=null, body=b"", version="1.1", reason=null)` | fields `.status`, `.headers`, `.body` (**bytes or a Reader**), `.version`, `.reason` |
| `resp.header(name, default=null)` | as `req.header` |
| `resp.bytes()` / `resp.text()` / `resp.json()` | the body, however it is framed |
| `resp.close()` | releases the connection a streamed response holds; idempotent and safe on any response |
| `resp.reason_phrase()` | the peer's reason, or the one from the status table |
| `http.text(s, status=200)` | a `text/plain; charset=utf-8` `Response` |
| `http.json_response(v, status=200)` | an `application/json` `Response` |
| `http.BadRequest(message, status=400)` | an exception a parser or handler raises |
| `http.BadResponse(message)` | the client-side twin |

`status` is **required** on a `Response`, and a request body is **always a
Reader** (an empty `Buffer` when there is none), so a handler never branches on
its shape. A `Response` body that is `bytes` gets a `Content-Length`; one that is
a Reader gets `Transfer-Encoding: chunked` — that choice is the framing.

```oro
import http
import io

req = http.read_request(io.buffer(b"GET /a?x=1 HTTP/1.1\r\nHost: h\r\n\r\n"))
print(req.method, req.path, req.query, req.version, req.header("host"), req.header("nope", default="d"))
resp = http.Response(200, headers={"content-type": "text/plain"}, body=b"hi")
print(resp.status, resp.header("content-type"), resp.bytes(), resp.text(), resp.reason_phrase())
w = io.buffer(b"")
http.write_response(w, req, resp, keep_alive=false)
print(w.bytes().split(sep=b"\r\n")[0])
print(http.text("ok").status, http.json_response({"a": 1}).text())
body = http.read_request(io.buffer(b"POST /x HTTP/1.1\r\nHost: h\r\nContent-Length: 3\r\n\r\nabc"))
print(body.text(), http.read_request(io.buffer(b"")))
```

#### `Router`

| signature | returns |
|---|---|
| `http.Router()` | an empty router |
| `r.add(method, path, handler)` | the router, so `add` chains |
| `r.dispatch(req)` | the handler's `Response`, or a 404, or a 405 carrying `Allow` |
| `r.handler_for(method, req)` | the handler or `null`, binding `req.params` on a match |
| `r.allowed(path)` | the sorted list of methods for that path |

A `:name` segment binds one non-empty segment into `req.params`. A `HEAD` is
served by the `GET` route with the body suppressed on the way out.

```oro
import http
import io

routes = http.Router().add("GET", "/health", req => http.text("ok\n")).add("GET", "/users/:id", req => http.text(f"user={req.params['id']}"))
print(routes.dispatch(http.read_request(io.buffer(b"GET /health HTTP/1.1\r\nHost: h\r\n\r\n"))).text())
print(routes.dispatch(http.read_request(io.buffer(b"GET /users/7 HTTP/1.1\r\nHost: h\r\n\r\n"))).text())
print(routes.dispatch(http.read_request(io.buffer(b"GET /nope HTTP/1.1\r\nHost: h\r\n\r\n"))).status)
print(routes.allowed("/health"))
```

#### The client

| signature | returns | raises |
|---|---|---|
| `http.fetch(method, url, headers=null, body=null, timeout=30, max_body=33554432, params=null)` | `Response` with the body **in memory** | `BadResponse`, `ValueError` on a bad URL, `ConnectionRefusedError`, `TimeoutError` |
| `http.stream(method, url, headers=null, body=null, timeout=30, params=null)` | `Response` whose `.body` is a **live Reader** — the caller must `close()` it | as `fetch` |
| `http.write_request(w, method, target, headers, body=b"")` | `null` | `ValueError` without a `Host` header |
| `http.read_response(r, method)` | `Response` | `BadResponse` on a malformed head |
| `http.parse_url(url)` | `Url` with `.scheme`, `.host`, `.port`, `.path`, `.query`, `.target`, plus `.authority()` and `.host_header()` | `ValueError` on `https://` (there is no TLS) or a malformed URL |

`headers` on `write_request` and `method` on `read_response` are **required**,
and each because its old default was a bug: the header default was dead (a
request with no `Host` is refused anyway) and the method default was harmful — a
`HEAD` reply read as a `GET` misreads the next message on the connection as this
one's body.

```oro
import http
import io

w = io.buffer(b"")
http.write_request(w, "GET", "/p", {"host": "h"})
print(w.bytes().split(sep=b"\r\n")[0])
resp = http.read_response(io.buffer(b"HTTP/1.1 204 No Content\r\n\r\n"), "GET")
print(resp.status, resp.reason_phrase(), resp.bytes())
u = http.parse_url("http://h:81/p?q=1")
print(u.scheme, u.host, u.port, u.path, u.query, u.target, u.authority(), u.host_header())
try:
    http.parse_url("https://h/")
except ValueError as e:
    print(f"{e}")
```

There is no `http.get`/`post`/`put` (the method is an argument), no session
object and no pool — one call is one connection, saying `Connection: close` —
and deliberately no redirects, cookies, authentication or retries, because each
is a policy the caller owns and `resp.header("location")` is right there.

#### Building a URL

| signature | returns | raises |
|---|---|---|
| `http.quote(s, safe="/")` | `%HH` escaped, space as `%20`; a `str` in gives a `str`, `bytes` gives `bytes` | `ValueError` on a non-ASCII `safe` |
| `http.quote_plus(s, safe="")` | space as `+`, `+` as `%2B` | as above |
| `http.unquote(s, plus=false)` | the decoded value | `ValueError` on a malformed escape |
| `http.encode_query(params)` | `"a=1&b=2"` from a dict or a list of pairs; a list value expands into repeated keys | `TypeError` on an unencodable value |

```oro
import http

print(http.quote("a/b c"), http.quote("a/b c", safe=""), http.quote_plus("a b+c"))
print(http.unquote("a%20b"), http.unquote("a+b", plus=true))
print(http.encode_query({"q": "a&b", "page": 2}), http.encode_query({"t": ["x", "y"]}))
try:
    http.unquote("a%zz")
except ValueError as e:
    print(f"{e}")
```

These are `urllib.parse`'s names and behaviour, checked against it over every
octet, with three deliberate differences: a malformed escape **raises** rather
than passing through, a non-ASCII `safe=` raises, and `urlencode` is
`encode_query` and expands a list value into repeated keys rather than
stringifying the list. `unquote(quote(b, safe="")) == b` holds for arbitrary
bytes.

Deliberately absent from the module: `Expect: 100-continue` (answered 417),
HTTP/2, HTTP/3, WebSocket upgrade, multipart parsing, static file serving,
compression, cookies, sessions, and TLS.
---

## 6. Cut, and what to write instead

Almost every removal **raises a message naming its replacement**, and the
messages below are the interpreter's own, quoted from a run. If you find
yourself reaching for a Python spelling, look here first.

> **The one silent difference.** Every other cut here is loud — a `NameError`,
> an `AttributeError`, a `TypeError` at the call. The exception is `sort` and
> `reverse`: Python's `list.sort()` and `list.reverse()` **mutate in place and
> return `None`**, and Oro's `xs.sort(f)` / `xs.reverse()` **return a new
> collection and do not touch the receiver**. `xs.sort(f)` as a bare statement
> therefore does nothing; you must rebind, `xs = xs.sort(f)`. Code ported from
> Python compiles and runs, and quietly does something different — the one place
> in this table where reading it is the only way to catch it. In exchange, the
> aliasing bug where `b = a; a.sort()` reorders what `b` sees cannot be written.

The contrast, line for line (both start from `a = [3, 1, 2]` with `b = a`):

| you write | CPython | Oro |
|---|---|---|
| `a.sort()` (as a statement) | `a` becomes `[1, 2, 3]`, returns `None` | `AttributeError` — `sort` needs a key: `a.sort(x => x)` |
| `a.sort(x => x)` (as a statement) | — (`key=` is keyword-only there) | returns `[1, 2, 3]`; **`a` is still `[3, 1, 2]`** — the result was dropped |
| `a = a.sort(x => x)` | — | `a` is `[1, 2, 3]`; **`b` is still `[3, 1, 2]`** (not aliased) |
| `sorted(a)` | `[1, 2, 3]`, `a` untouched | `AttributeError` — use `a.sort(x => x)` |
| `a.reverse()` (as a statement) | `a` becomes `[2, 1, 3]`, returns `None` | returns `[2, 1, 3]`; **`a` unchanged** |

The takeaway a Python reader needs: **rebind.** `a = a.sort(f)`, `a = a.reverse()`.
`corpus/divergence/83_sort_not_in_place.oro` runs every row above.

### Builtins and functions

| Python | Oro | why |
|---|---|---|
| `sorted(xs)` | `xs.sort(x => x)` | A builtin takes scalars; a collection method takes a collection |
| `sorted(xs, key=f, reverse=true)` | `xs.sort(f, reverse=true)` | The key is the operand, so it is positional |
| `xs.sort(key=f)` (Python's in-place) | `xs = xs.sort(f)` | **Silent behaviour change:** Oro's `sort` returns a new list and does *not* mutate — rebind it. There is no in-place sort. |
| `xs.reverse()` (Python's in-place) | `xs = xs.reverse()` | Likewise a new collection, not a mutation |
| `sum(xs)` | `xs.sum()` | as `sorted` |
| `any(xs)` / `all(xs)` | `xs.any(x => x)` / `xs.all(x => x)` | The predicate is required; truthiness is spelled out |
| `enumerate(xs)` / `xs.enumerate()` | `for i, x in xs` | Gone: every `for` yields `(index, value)`. A chain has no index — needing one is a reason to use a loop |
| `zip(a, b)` | `a.zip(b)` | as `sorted`; takes any number of further sequences, eager |
| `min(xs)` / `max(xs)` | `xs.min()` / `xs.max()` | `min(a, b)` over two or more values is unchanged |
| `sorted("ba")` / `min(b"ba")` | `"ba".to_list().sort(x => x)` | A `str`/`bytes` is not a collection; `to_list()` is the bridge |
| `xs.count(v)` | `xs.count(x => x == v)` | The collection `count` takes a predicate |
| `int(s)` / `str(x)` / `float(s)` / `bool(x)` | `s.to_int()` / `f"{x}"` or `x.to_str()` / `s.to_float()` / `x.to_bool()` | Type names are not callable; conversion is a method that chains |
| `int(s, 16)` | `s.to_int(base=16)` | as above |
| `list(xs)` / `dict(pairs)` | `xs.to_list()` / `pairs.to_dict()` | as above |
| `list()` / `dict()` / `str()` / `int()` | `[]` / `{}` / `""` / `0` | Literals build; type names convert |
| `set()` / `{1, 2}` | `{1: true, 2: true}` with `k in d`, or a list | Sets are cut; only set algebra is a real gap |
| `isinstance(x, str)` | `type(x) == str` | A type name *is* the type |
| `isinstance(e, Base)` | `except Base:` | `except` is the one place a subclass test is the right question |
| `iter(xs)` / `next(g)` | `for i, x in xs` | Not defined; iteration is the `for` loop and the protocol |
| `hash(x)` / `input()` / `eval` / `exec` | *(absent)* | No hashing surface, no REPL-style input, no runtime code growth |
| `f(*args)` / `f(**kwargs)` | `apply(f, args=xs, kwargs=d)` | One spelling, and the argument rule still applies |
| `def f(*args)` / `def f(**kw)` | `def f(items)` / `def f(opts)` — a list or a dict | as above |

### `str` and `bytes`

| Python | Oro | message |
|---|---|---|
| `s.lstrip(c)` / `s.rstrip(c)` | `s.strip(chars=c, side="left")` / `side="right"` | ``lstrip` is not in Oro — use `strip(side="left")`` |
| `s.rsplit(sep, n)` | `s.split(sep=…, maxsplit=n, side="right")` | ``rsplit` is not in Oro — use `split(sep=…, maxsplit=…, side="right")`` |
| `s.rfind(sub)` | `s.find(sub, reverse=true)` | ``rfind` is not in Oro — use `find(sub, reverse=true)`` |
| `s.index(sub)` | `s.find(sub)` | ``index` is not in Oro — use `find(sub)`, which answers -1 rather than raising`` |
| `s.zfill(n)` | `f"{n:05d}"`, `f"{s:0>5}"`, `f"{s:0>{w}}"` | ``zfill` is not in Oro — a format spec pads` |
| `sep.join(xs)` | `xs.join(sep)` | ``str.join` is not in Oro — the separator is the argument and the sequence the receiver`` |
| `s.removeprefix(p)` / `removesuffix(p)` | `s.rm_prefix(p)` / `s.rm_suffix(p)` | ``removeprefix` is spelled `rm_prefix` in Oro` |
| `s.isdigit()` / `isalpha()` / `isalnum()` / `isspace()` | `s.is_digit()` / `is_alpha()` / `is_alnum()` / `is_space()` | ``isdigit` is spelled `is_digit` in Oro` |
| `s.find(sub, 2, 5)` | `s.find(sub, start=2, end=5)` | the window is keyword-only (the error rewrites your call) |
| `s.replace(a, b, 2)` | `s.replace(a, b, count=2)` | the count is keyword-only |
| `s.split(",")` | `s.split(sep=",")` | the separator is keyword-only |
| `s.split(None, 1)` | `s.split(maxsplit=1)` | there is no magic `null` separator |
| `s.strip("xy")` | `s.strip(chars="xy")` | the character set is keyword-only |
| `len(s)` as `s.len()` | `len(s)` | ``len` is a builtin in Oro and not a method on `str`/`bytes`` |
| `"abc".map(f)` | `"abc".to_list().map(f)` | a `str` is not a collection |
| `s.format(...)` / `"%d" % n` / `s.title()` / `s.center()` / `s.splitlines()` / `s.encode()` | f-strings, or the methods in [5.3](#53-str) | not present |

### Collections

| Python | Oro | why |
|---|---|---|
| `d.items()` | `for k, v in d`, or `d.to_list()` | Iterating a dict already yields its pairs |
| `d.get(k, 0)` | `d.get(k, default=0)` | The fallback is keyword-only |
| `d.pop(k, 0)` | `d.pop(k, default=0)` | as above; without `default=` a missing key raises `KeyError` |
| `d.keys()` as a view | `d.keys()` is a **list** | Indexable, not live, and the whole protocol works on it |
| `d.update(...)` / `setdefault` / `clear` / `copy` | a loop, or `d[k] = v` | not present |
| `xs.pop(0)` | `xs.pop(index=0)` | On a list the index is named, so a positional `pop` argument always means a dict key |
| `xs.insert(i, v)` / `xs.remove(v)` / `xs.index(v)` | a loop, or rebuild with the protocol | not present |
| `[f(x) for x in xs if p(x)]` | `xs.filter(p).map(f)` | Chains read in the order they run |
| `{k: v for ...}` / `{x for ...}` / `(x for ...)` | a loop, or a chain plus `to_dict()` | Comprehensions are cut |
| `range(2, 10)` / `range(0, 10, 3)` | `range(10, start=2)` / `range(10, step=3)` | The single positional argument is always the end |

### Statements and syntax

| Python | Oro |
|---|---|
| `lambda x: x * 2` | `x => x * 2` |
| `for x in xs:` | `for _, x in xs:` — every `for` binds an `(index, value)` pair |
| `for i, x in enumerate(xs):` | `for i, x in xs:` — the index is built in |
| `for i in range(n):` | `for i, _ in range(n):` — the position is the index |
| `for a, b in pairs:` (a list of tuples) | `for _, (a, b) in pairs:` — the element is nested in the value slot |
| `for k, v in d.items():` | `for k, v in d:` — a dict's index *is* its key |
| `if xs:` / `while xs:` (a collection) | `if len(xs) != 0:` — no truthiness |
| `if n:` (a number) | `if n != 0:` |
| `if x:` (a maybe-null value) | `if x != null:` |
| `name = d.get(k) or "anon"` (falsy fallback) | `name = d.get(k, default="anon")`, or an explicit `if name == null` — `or` takes bools |
| `x and y` / `x or y` returning an operand | both operands are `bool`, the result is a `bool` |
| `x is y` / `x is not y` | `x == y` / `x != y` |
| `True` / `False` / `None` | `true` / `false` / `null` |
| `a if c else b` | an `if`/`else` statement |
| `a & b`, `a \| b`, `a ^ b`, `~a`, `a << b`, `a >> b` | *(no bitwise operators at all)* |
| `n //= 2`, `n %= 2`, `n **= 2` | `n = n // 2`, … (only `+=` `-=` `*=` `/=` exist) |
| `obj.n += 1` | `obj.n = obj.n + 1` (an attribute is not an augmented-assignment target) |
| `a = b = c` | `a, b = c, c` |
| `(n := f())` | an assignment statement |
| `with open(p) as f:` | `f = open(p)` — it closes at end of scope |
| `from x import y` / `import *` | `import x` |
| `del d[k]` / `del x` | `d.pop(k)`; block scope drops names |
| `assert cond` | `if not cond: raise ...` |
| `try: … except: …` | `except SomeError:` — bare `except` is refused |
| `try: … except: … else: …` | put the else code after the `try` |
| `raise X from Y` / `raise X` | `raise X("msg")` |
| `return`/`break`/`continue` inside `finally` | move it after the `try` |
| `def f(a: int) -> int` / `x: int = 5` | no annotations |
| `@decorator` | write the wrapping explicitly |
| `class C(A, B)` / `metaclass=` / `__slots__` / `__new__` / `__getattr__` / `__setattr__` / `__hash__` | single inheritance and the fixed dunder set |
| `nonlocal x` | `global`, or hold the state on an object |
| `async def` / `await` | ordinary functions; `spawn` and channels |
| `match x: case a | b:` / `case [a, b]:` / `case x if c:` | separate `case` clauses; `match` is a value switch |
| a tab in leading whitespace | spaces |
| `a = 1; b = 2` / `if x: y` | one statement per line |

### Modules

| Python | Oro |
|---|---|
| `import subprocess` | `import proc` — `run(args)` captures always, `check=true` by default |
| `subprocess.run(a, shell=True)` | `proc.run(["sh", "-c", cmd])`, explicitly |
| `subprocess.run(a, capture_output=True, text=True)` | `proc.run(a)`, then `.stdout.to_str()` |
| `re.match(p, s)` | `re.search(p, s)`, or anchor with `^` |
| `re.sub(p, r, s, count)` / `flags=` / `pos` / `maxsplit` | not implemented, and refused rather than ignored; write flags inline as `(?i)` |
| `open(p, "rb")` / `open(p, "r").read()` | `open(p)` / `io.read(open(p))` — every mode is bytes |
| `f.readline()` / `f.readlines()` / `for line in f` | `io.read(f).to_str().strip(chars="\n", side="right").split(sep="\n")` |
| `f.flush()` | nothing — writers are unbuffered |
| `f.seek()` / `f.tell()` | absent (deferred, not cut) |
| `sys.exit()` | `sys.exit(0)` — the code is required |
| `os.listdir()` | `os.listdir(path)` — the path is required |
| `json.dumps(x, indent=2)` | `json.stringify(x, indent=2)`; `json.loads` is `json.parse` |
| `datetime` | `time.time()` / `time.monotonic()`; a calendar library is future work |
| `threading` / `asyncio` / `queue` | `spawn`, `chan`, `yield_now` |
| `socket` | `net.listen` / `net.dial`, and the stream methods |
| `requests` / `http.client` / `http.server` | `http.fetch` / `http.serve` |
---

## 7. Exceptions

### The hierarchy

Built once per VM as real classes, so `raise`, `except`, and
`class MyError(Exception)` all go through ordinary class machinery. `except`
matches by inheritance, and the shape is CPython's for the subset Oro has.

```text
BaseException
├── SystemExit                     sys.exit(code)
└── Exception
    ├── ImportError
    │   └── ModuleNotFoundError
    ├── ValueError
    ├── TypeError
    ├── LookupError
    │   ├── KeyError
    │   └── IndexError
    ├── AttributeError
    ├── NameError
    ├── ArithmeticError
    │   └── ZeroDivisionError
    ├── RuntimeError
    │   ├── RecursionError
    │   └── NotImplementedError
    ├── StopIteration
    ├── EOFError
    ├── CommandError                a nonzero exit from proc.run
    ├── ChannelClosed               send/recv on a closed channel
    └── OSError
        ├── FileNotFoundError
        ├── PermissionError
        ├── TimeoutError
        └── ConnectionError
            ├── ConnectionRefusedError
            ├── ConnectionResetError
            ├── ConnectionAbortedError
            └── BrokenPipeError
```

Every one of those names is a global you can `raise` and `except`. `CommandError`
and `ChannelClosed` are Oro's own; everything else is CPython's, in CPython's
position — so `except OSError` catches the whole network and filesystem family,
and `except LookupError` catches both a bad key and a bad index.

```oro
try:
    {}["k"]
except LookupError as e:
    print("caught at width:", type(e), f"{e}")


class ConfigError(Exception):
    pass


try:
    raise ConfigError("no port")
except ConfigError as e:
    print(type(e), f"{e}")
except Exception as e:
    print("not reached")
```

**A fault's class is named where the fault is raised**, not guessed later from
its message. This matters and is worth knowing why: the class used to be
recovered by substring-matching the English of the message, five thousand lines
away — and those messages interpolate values the *program* chose, so
`"timed out".to_int()` raised `TimeoutError` and a dict miss on a key that read
`"No such file or directory"` raised `FileNotFoundError`. Both are now what they
always were:

```oro
print("--- the class is a property of the operation, never of the data")
for _, text in ["timed out", "No such file or directory", "division by zero"]:
    try:
        {"a": 1}[text]
    except KeyError as e:
        print("KeyError", f"{e}")
    try:
        text.to_int()
    except ValueError as e:
        print("ValueError", f"{e}")
```

The one place a class is *derived* rather than written down is an OS error, and
it comes from the `ErrorKind` the kernel reported — never from `strerror`'s
text.

### What raises what

| operation | raises |
|---|---|
| a missing dict key (`d[k]`, `d.pop(k)`) | `KeyError` |
| an index out of range; `first()`/`last()` on empty; `pop` from an empty list | `IndexError` |
| a missing attribute or method; a cut method naming its replacement | `AttributeError` |
| an undefined name; a block-scoped name read outside its block | `NameError` |
| `//`, `%` or `/` by zero | `ZeroDivisionError` |
| a wrong argument count, a wrong type, a keyword that breaks the argument rule, `null` where a default is not `null` | `TypeError` |
| a bad literal (`"abc".to_int()`), non-UTF-8 `b.to_str()`, `min()`/`max()` on empty, `chunk(0)`, a bad `side=`, `read(0)`, `read_until` over its limit, a bad file mode, reading a writer, using a closed stream, a malformed JSON document or URL | `ValueError` |
| an unhashable key (`list`, `dict`, or an instance of a class defining `__eq__`) | `TypeError` |
| runaway recursion | `RecursionError` |
| `io.read(r, fixed_size=n)` on a stream that ends short | `EOFError` |
| `proc.run` with a nonzero exit (unless `check=false`) | `CommandError` |
| a missing or non-executable program; a missing file or directory | `FileNotFoundError` / `PermissionError` |
| `proc.run(timeout=)` elapsing; a socket deadline from `set_timeout` | `TimeoutError` |
| a refused connect, a reset peer, a write to a departed peer, a local abort | `ConnectionRefusedError` / `ConnectionResetError` / `BrokenPipeError` / `ConnectionAbortedError` |
| a bind to a port already in use | `OSError` |
| `send`/`recv` on a closed channel | `ChannelClosed` |
| `sys.exit(code)` | `SystemExit` |
| an import that does not resolve (including an underscored stdlib internal) | `ModuleNotFoundError` |
| `set()`, `re.match`, an unrouted internal invariant | `RuntimeError` |
| a malformed HTTP request head or a rejected header | `http.BadRequest` (an `Exception`) |
| a malformed HTTP response head | `http.BadResponse` |

```oro
def show(label, f):
    try:
        f()
        print(label, "-> no error")
    except Exception as e:
        print(label, "->", type(e))


show("missing key", () => {}["k"])
show("bad index", () => [1][9])
show("divide by zero", () => 1 // 0)
show("bad literal", () => "abc".to_int())
show("wrong type", () => 1 + "a")
show("missing method", () => [1].nope())
show("unhashable key", () => {}.get([1]))
show("missing file", () => open("/nonexistent/x"))
```

Diagnostics carry `file:line:column`, and a runtime error names the file it
happened in — which may be a module rather than the script you ran. An uncaught
exception exits 1; `sys.exit(n)` exits `n`; a failed task nobody joined prints
`task failed: …` and sets the status to 1 without stopping the other tasks.
---

## 8. For an AI agent working in this repo

You are working on a language whose defining property is that it refuses second
spellings. That changes how to work here.

**1. The one-way rule is the design, not a style preference.** Before adding a
name, a keyword or a parameter, check whether the capability already has a
spelling. If it does, the answer is almost always "no" — and if a second
spelling really is wanted, the removal of the first one is part of the same
change. Every cut in this language is paired with an error message that names its
replacement; a removal that leaves a bare `AttributeError` behind is an
unfinished removal.

**2. Never assume a Python signature. Run it.** This surface moved a great deal
in the last two dozen commits, and the fastest way to be wrong is to write what
CPython accepts. One-liners are cheap:

```text
printf 'print("a,b".split(sep=","))\n' > /tmp/p.oro && ./target/release/oro /tmp/p.oro
```

The errors are written to teach: an argument-rule violation usually prints *your
own call rewritten correctly*, so reading the message is faster than reading the
source. When you do read the source, the signature lives in
`src/builtins/mod.rs` (free builtins, `str`/`bytes`/number/collection natives,
`Match`), `src/vm/mod.rs` (the callback-taking collection protocol, `print`,
`proc.run`, `apply`, method dispatch), `src/vm/sched.rs` (`spawn`, `chan`, the
stream calls that park), `src/vm/modules.rs` (`sys`, `os`, `time`, `re`, `net`,
and the underscored internals), `src/stream.rs` (the stream types), and
`std/*.oro` (`io`, `json`, `http`, which are Oro).

**3. `oro fmt` is the formatter, and it is not optional.** No options, one
output. `tests/fmt_test.rs` formats **every `.oro` file in the repository** and
requires it to already be canonical, idempotent and semantics-preserving, so any
`.oro` file you add or touch needs `oro fmt --write` before you commit. It also
refuses to run on a comment it cannot place unambiguously, which is a real
failure mode when you edit inside a multi-line bracketed expression. Prefer
keeping examples inside Markdown, where the formatter does not reach.

**4. The corpus is the gate, and the oracle is how behaviour changes.** The
workflow for a language change is:

```text
cargo build --release
./corpus/run.sh            # must print "fail 0"
cargo test                 # includes fmt_test.rs
```

If you *deliberately* changed behaviour that CPython also defines, add or edit a
program in `corpus/core/` and regenerate its expectation with
`./corpus/oracle.sh`, which runs it under **CPython** — then read `git diff` on
the `.expected` files, because that diff is the behaviour change, stated in
CPython's own output. A clean tree after `oracle.sh` means Oro still agrees with
CPython.

Where a new spelling is not valid Python at all, the program goes in
`corpus/divergence/` — and if it diverges only in *spelling*, write a
`.twin.py` beside it (the same program in CPython's names) and make the twin's
output the `.expected`, so the oracle still stands behind it.
`corpus/known-failing/` is for correct Python that Oro gets wrong: it never fails
the build, and a case that starts passing is flagged for promotion. Keep the two
apart — `divergence/` is a design record and `known-failing/` is a bug list.

**5. Write the error message as carefully as the code.** The messages are a
documented surface: they are what a reader (or a model) sees instead of this
document, they appear verbatim in `.expected` files, and several of them are the
only place a design decision is recorded at the point of use. When you change
one, `./corpus/run.sh` will tell you which expectations move.

**6. Habits that pay off in this language specifically.**

- Reach for a chain before a loop, and for `for i, _ in range(n)` before a `while`
  with a counter — a `while` is for a condition that is not a count.
- Bind a name before a block if you need its value after it (`try` is a scope
  too).
- Remember that `read(n)` may return fewer than `n` bytes; use
  `io.read(r, fixed_size=n)` when the count is part of the protocol.
- A CPU-bound task starves its peers: the scheduler is cooperative, and the
  yield points are channel operations, socket calls, `time.sleep` and
  `yield_now()`.
- Generators are lazy in a `for` loop and eager everywhere else.
- `null` is a value, not "absent": passing `x=null` to a keyword whose default
  is not `null` is refused on purpose.
