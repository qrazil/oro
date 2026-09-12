# Survey: Oro-level `def`s in `std/` under the positional-only / keyword-only rule

Repo `oro` at `9019829` (`feat/sane-defaults`). Read-only. Release build, `./target/release/oro`.

**Rule.** A parameter with no default can only be passed by position. A parameter with a default can only be passed by name. `*args`/`**kwargs` are unaffected, and `self` is not an argument.

## How the counts were made

- **Parser.** Scripts: `census.py`, `census2.py`, `analyze.py` and `nonoro.py`, all in this directory.
  - Each `.oro` file is first masked. Comments and string contents are blanked; the code inside f-string `{...}` is kept.
  - Each call's `(` is matched with nesting respected, and the argument list is split at top-level commas.
  - An argument is a **keyword** only if it matches `^\s*ident\s*=(?![=>])`. So `req => ...`, `a == b` and `(a, b) => ...` are positional. There are 66 resolved calls with a lambda argument, and all of them were classified correctly, for example `examples/client.oro:64`.
- **Coverage.** 131 `.oro` files and 1821 resolved calls. The results were compared against a raw grep for `io.buffer(`, `io.read(`, `http.fetch(`, `json.stringify(` and `.header(`. The only hits the resolver skips are text inside strings or comments, and the Rust-side `_io.buffer(b)` and `_json.stringify(...)`.
  - One real gap was found and fixed: a method called on an expression, like `parse(_TRAILER).header("X-Sum", "-")` or `K().m(...)`.
- **Resolution.**
  - `http.X(` / `io.X(` / `json.X(` resolve to the module-level def. Inside `std/http.oro` a bare `X(` resolves the same way.
  - A class call resolves to its `__init__`, following base classes.
  - `spawn(f, ...)` is resolved as a call to `f` with the remaining arguments; it forwards keywords since 5bb7f8a. Nothing in the tree aliases std modules with `import x as y`. No fixture subclasses a std class, and no std call uses `*`/`**` unpacking.
- **Ambiguous `obj.name(...)` calls.**
  - `.header(` could be `Request.header` or `Response.header`. The two signatures are identical, so the call is counted once for both. Receivers: `r` 8, `req` 6, `resp` 6, an expression 1.
  - `.fail(`: all 25 calls are `self.fail` inside `_HeadParser`, `_ChunkedReader` or their `_Response*` subclasses, whose signatures are identical.
  - Other std method names share a name with builtins: `.read`, `.text`, `.json`, `.bytes`, `.close`, and `.add` (one bench `acc.add` call is a set). None of these std methods has a defaulted parameter, and **no call anywhere passes one of their required parameters by name**, so a wrong attribution cannot create a violation.
  - Fixture methods are attributed only within the file that defines them. The single violating fixture method call was checked by eye.
- **Rule (b) for std.** Zero sites. No required parameter of any std def is passed by name, in any directory.
- **Behaviour today, checked by running `probe.oro`.** Every form is accepted: a defaulted parameter by position, a required one by name, `spawn(f, 1, c=7)`, and `f(1, **d)`. Lambdas cannot take defaults at all (`(x, y=1) => x` is a parse error), so the rule only concerns `def`.
- **Spot-checked by eye, 14 sites, all classified correctly:**
  - `examples/client.oro:64`, the lambda handler plus a positional `ready`
  - `corpus/divergence/62_http_client_socket.oro:73`
  - `corpus/divergence/60_http_serve.oro:70`, which is positional (a)
  - `corpus/divergence/60_http_serve.oro:108`, all named, so clean
  - `corpus/divergence/46_http_serve.oro:113`, a lambda wrapping `Response(200, {}, b"")`
  - `examples/client.oro:24`
  - `corpus/divergence/44_http_body.oro:75`
  - `corpus/divergence/61_http_client.oro:100` and `:157`
  - `corpus/divergence/46_http_serve.oro:70`
  - `corpus/core/06_functions.oro:14`
  - `tests/programs/varargs.oro:21`
  - `corpus/divergence/61_http_client.oro:227`
  - `corpus/divergence/47_spawn.oro:86`, a call inside `() => ...`
- **Per-site lists:** `std_violations_a.txt` for std and `fixture_violations.txt` for fixtures.

Count format: **pos/named/omitted**, split by directory. `ex` is `examples/`. `bench/` and `tests/programs/` make no std calls with defaulted parameters, except `json.stringify` in bench.

## Table 1: every std defaulted parameter (42 rows, covering 46 def-parameters)

`priv` marks a private `_helper`. Hot path means the parameter runs per request or per element, on the server or on each outbound client call.

| function | param | default | calls pos/named/omitted (by dir) | hot path? | rec | reason |
|---|---|---|---|---|---|---|
| `json.stringify(value, indent=)` | indent | `null` | std 0/0/1 · ex 0/1/3 · corpus 0/4/18 · bench 0/1/2 | per request in `json_response`, always omitted | **named** | A formatting variant (`indent=2`), and it's already spelled that way everywhere. Wraps Rust `_json.stringify(value, indent)`. |
| `io.read(r, n=)` | n | `null` | std 1/0/3 · ex 0/0/3 · corpus 13/0/10 | per chunk (`_ChunkedReader.terminator`: `io.read(self.r, 2)`) | **split** | "To EOF" and "exactly n" have different failure contracts (EOFError on a short stream vs never) and different memory profiles; `null` is only a sentinel for "no count". A split keeps the per-chunk call positional. Fallback: named `n=`. The EOF path wraps Rust `_io.read_all`. |
| `io.buffer(b=)` | b | `b""` | std 4/0/0 · ex 1/0/1 · corpus 33/0/20 | per bodiless request (`_body_reader`), per bodiless or HEAD client response, `_answer` | **required** | The initial contents are the point of a Buffer, and `io.buffer(b"")` spells out the empty case. Named would turn `b` into public surface, and the name says nothing. Wraps Rust `_io.buffer`. |
| `BadRequest(message, status=)` | status | `400` | std 4/0/6 · corpus 1/0/0 | no, error path | **named** | A variant: `status=431`. Matches its own `__repr__` (`status=…`) and `text(status=)`. |
| `Request.header` / `Response.header(name, default=)` (2 defs) | default | `null` | std 0/0/2 · ex 1/0/3 · corpus 3/0/12 | `req.header("connection")` per request, always omitted | **named** | Rarely passed, and meaningless without a name. Thin wrapper over builtin `dict.get(key, default)`, so it should follow that decision (see "needs a decision"). |
| `Response(status=, …)` | status | `200` | std 5/0/0 · ex 1/0/0 · corpus 9/0/0 | yes: every `text()`, `json_response()`, 404/405, and each client response | **required** | Every construction states one; `Response(404, …)` is obvious. Keeps the per-request constructor call positional. |
| `Response(…, headers=, …)` | headers | `null` | std 5/0/0 · ex 1/0/0 · corpus 7/0/2 | yes, same | **named** | Headers and body are independently optional (a 204 has neither, a redirect has headers and no body). Required would force `{}` and `b""` fillers. Measured cost: about +0.6 µs per request together with body=. |
| `Response(…, body=, …)` | body | `b""` | std 5/0/0 · ex 1/0/0 · corpus 7/0/2 | yes, same | **named** | Same as headers. |
| `Response(…, version=, …)` | version | `"1.1"` | std 1/0/4 · ex 0/0/1 · corpus 0/0/9 | per client response only | **named** | Only the proxy direction (`_read_one_response`) sets it. |
| `Response(…, reason=)` | reason | `null` | std 1/0/4 · ex 0/0/1 · corpus 0/0/9 | per client response only | **named** | Same as version. |
| `text(s, status=)` | status | `200` | std 1/0/0 · ex 0/0/4 · corpus 1/0/13 | `text()` runs per request; status is almost always omitted | **named** | A variant of "a text reply": `http.text("gone", status=410)`. |
| `json_response(v, status=)` | status | `200` | ex 2/0/2 · corpus 2/0/1 | per request | **named** | Same as `text`. |
| `_percent_decode(b, plus_is_space, fail=, where=)` priv | fail | `_fail_request` | std 5/0/0 | **yes**: path, plus 2 per query pair | **required** | Every caller passes it. Mechanically named would cost about +0.57 µs per call. Matches `_path_and_query(target, fail, where)`, which already takes both as required. |
| `_percent_decode` priv | where | `"the request target"` | std 5/0/0 | **yes** | **required** | Same. |
| `_parse_query(qs, fail=, where=)` priv | fail | `_fail_request` | std 1/0/0 | yes, per request with a query | **required** | Its only caller passes both. |
| `_parse_query` priv | where | `"the request target"` | std 1/0/0 | yes | **required** | Same. |
| `_content_length(s, fail=)` priv | fail | `_fail_request` | std 0/1/1 | yes: per request with a body, and per client response | **required** | Two callers, one per direction. The same collaborator argument as the rest of the `fail` family. |
| `_HeadParser.fail` / `_ChunkedReader.fail` / `_ResponseParser.fail` / `_ResponseChunkedReader.fail(msg, status=)` priv (4 defs) | status | `400` | std 3/0/22 | no, error path | **named** | `self.fail("…", status=431)` matches `BadRequest(status=)`. The two `_Response*` overrides ignore `status` but must keep it so they accept the inherited calls. |
| `_check_headers(headers, kind=)` priv | kind | `"response"` | std 0/0/2 | per request (called twice) | **reshape** | Drop the parameter. Both callers check response headers, and the request side calls `_check_header` directly. |
| `_check_header(name, value, kind=)` priv | kind | `"response"` | std 1/1/0 | **yes**, per header, about 5 per request | **required** | Every call supplies it. Named would add about 0.43 µs × about 5 per request. |
| `serve_conn(conn, handler, max_requests=, watch=)` | max_requests | `_MAX_REQUESTS_PER_CONN` | std 1/0/0 · corpus 0/0/15 | per connection, not per request | **named** | A tuning number; the same name as on `serve`. |
| `serve_conn` | watch | `null` | std 0/1/0 · corpus 0/0/15 | per connection | **named** | A collaborator that is rarely passed; already named. |
| `serve(addr, handler, ready=, …)` | ready | `null` | ex 2/0/0 · corpus 2/2/0 | no | **named** | `ready=ch`. Works through `spawn` since 5bb7f8a (verified). |
| `serve` | max_conns | `_MAX_CONNS` | ex 0/0/2 · corpus 0/2/2 | no | **named** | Tuning. |
| `serve` | max_requests | `_MAX_REQUESTS_PER_CONN` | ex 0/0/2 · corpus 0/1/3 | no | **named** | Tuning. |
| `serve` | timeout | `_CONN_TIMEOUT` | ex 0/0/2 · corpus 0/0/4 | no | **named** | Tuning. |
| `serve` | drain | `_DRAIN_SECONDS` | ex 0/0/2 · corpus 0/0/4 | no | **named** | Tuning. |
| `quote(s, safe=)` | safe | `"/"` | corpus 0/17/2 | no | **named** | A variant; already spelled that way. Wraps Rust `_pct.encode`. |
| `quote_plus(s, safe=)` | safe | `""` | std 0/0/3 · corpus 0/2/8 | per element in `encode_query`, always omitted | **named** | Same as `quote`. |
| `unquote(s, plus=)` | plus | `false` | corpus 0/4/23 | no | **named** | A variant; already spelled that way. Wraps Rust `_pct.decode`. |
| `write_request(w, method, target, headers=, body=)` | headers | `null` | std 1/0/0 · corpus 6/0/0 | per outbound request | **required** | The default is dead: with it, the call **always raises** "a request needs a Host header" (verified). Every call passes headers. |
| `write_request` | body | `b""` | std 1/0/0 · corpus 5/0/1 | per outbound request | **named** | Truly optional, and `body=` matches `fetch`. |
| `read_response(r, method=)` | method | `"GET"` | std 1/0/0 · corpus 7/0/0 | per outbound response | **required** | Its own comment says the method "is not optional information". A HEAD reply read with the default misreads the next message as the body (`b'HTTP/'`, verified). |
| `fetch(method, url, headers=, …)` | headers | `null` | ex 1/0/5 · corpus 2/0/18 | per outbound call; negligible next to a connect | **named** | Optional; unlabelled it reads as a bare dict. |
| `fetch` | body | `null` | ex 1/0/5 · corpus 1/0/19 | same | **named** | `body=` |
| `fetch` | timeout | `_CLIENT_TIMEOUT` | ex 0/0/6 · corpus 0/3/17 | same | **named** | Tuning. |
| `fetch` | max_body | `_MAX_BODY` | ex 0/0/6 · corpus 0/1/19 | same | **named** | Tuning. |
| `fetch` | params | `null` | ex 0/0/6 · corpus 0/3/17 | same | **named** | Already named. |
| `stream(method, url, headers=, …)` | headers | `null` | std 1/0/0 · ex 0/0/1 · corpus 0/0/2 | same | **named** | Same as `fetch`. The one positional call is `fetch`'s own forward. |
| `stream` | body | `null` | std 1/0/0 · ex 0/0/1 · corpus 0/0/2 | same | **named** | Same. |
| `stream` | timeout | `_CLIENT_TIMEOUT` | std 1/0/0 · ex 0/0/1 · corpus 0/0/2 | same | **named** | Same. |
| `stream` | params | `null` | std 1/0/0 · ex 0/0/1 · corpus 0/0/2 | same | **named** | Same. |

**Totals.**
- **Recommendations:** 30 named, 10 required, 1 split, 1 reshape. Private rows: 8.
- **Rule (a):** 161 arguments to defaulted parameters are passed by position today. By directory: std 51, examples 11, corpus 99, bench 0, tests 0.
  - Under these recommendations, 81 of them become legal as written because the parameter becomes required. The other 80 need `name=`, and the 14 `io.read(r, n)` calls move to the split name.
  - Making a parameter required also changes calls that currently omit it or name it:
    - the 21 bare `io.buffer()` calls become `io.buffer(b"")`
    - the omitted and named `_content_length` calls (one each) pass `fail` by position
    - the one named `_check_header` call passes `kind` by position
- **Rule (b):** 0 std sites.

**Non-`.oro` call sites that would need rewriting** (module-qualified std calls only; method calls not counted):

| where | violating / total std calls |
|---|---|
| `README.md` | 4 / 34 |
| tracked `docs/` (`one-way-audit.md`, `stdlib-server-design.md`) | 13 / 49 |
| untracked `docs/audit/` | 41 / 120 |
| `src/**/*.rs` | 13 / 38 |
| `std/` comments | 3 |

- The `src/**/*.rs` sites are mostly Oro programs embedded in `src/vm/tests.rs` (9), plus `src/stream.rs` (3) and `src/net/tests.rs` (1). They run under `cargo test`.
- The `std/` comment sites are `http.oro:828`, `:2153`, and the two-line `fetch` example at `:2218`.

## Table 2: required parameters that look like options

| function | param | calls | why it looks like an option | suggestion |
|---|---|---|---|---|
| `write_response(w, req, resp, keep_alive)` | keep_alive | 13 (std 2, corpus 11, all 11 bare `true`/`false`) | A bare boolean. `http.write_response(w, req, resp, true)` doesn't say what `true` is, and the rule would lock that in. | Needs a decision: keep it positional (it carries a decision `serve_conn` computed), or default it to `false`, the safe answer, which makes it keyword-only (`keep_alive=true`). |
| `Request(method, path, query, version, headers, body)` | version, headers, body (all six are positional) | 6 (std 2, corpus 4). Every corpus call writes `"1.1"`, `{}`, `io.buffer(b"")`. | Six positional-only arguments, three of them near-constant fillers. | Needs a decision: default `version="1.1"`, `headers=null`, `body=null` (keyword-only), or keep it a parser-facing record. |
| `_percent_decode(b, plus_is_space, …)` priv | plus_is_space | 5 std, all literal | A bare boolean. | Keep positional: private, on the per-element hot path, and the two callers are the path (`false`) and the query (`true`). Public `unquote` already names it `plus=`. |
| `Url(scheme, host, port, path, query, target)` | all six | 2 (std only) | Six positional arguments. | Leave: only std constructs it. |
| `_conn_task(handler, live, entry, max_requests)` priv | max_requests | 1 (the spawn in `serve`) | A tuning number. | Leave: a private pass-through of `serve`'s keyword. |
| `_drain_live(live, tally, drain)` priv | drain | 2 | A tuning number. | Leave: private pass-through. |
| `_read_capped(r, limit)` priv | limit | 1 | A tuning number or `null`. | Leave: private pass-through of `max_body`. |

Not an option, but worth noting: under the rule, the argument order of `io.copy(dst, src)` can never be labelled at a call site (7 calls).

## Hot path: measured cost

Source: `bench.oro`, 3 runs, 300k iterations each, ns per call including about 50 ns of loop overhead.

| call shape | positional | with keywords | delta |
|---|---|---|---|
| 4 args, 2 of them named (`_percent_decode` shape) | 182–208 | 744–795 | about +570 ns |
| 3 args, 1 named (`_check_header` shape) | 179–188 | 608–637 | about +430 ns |
| the same call with the parameter made required | 181–196 | — | ±0, so required costs nothing extra |
| `Response(404, h, body)` → `(404, headers=, body=)` → `(status=, headers=, body=)` | 898–931 | 1528–1580 / 1630–1724 | +630 / +750 ns |
| `http.text(s, 404)` → `(s, status=404)` | 1374–1412 | 1733–1779 | +360 ns |
| `io.buffer(b"")` → `(b=b"")` | 389–401 | 679–701 | +290 ns |
| one whole request through `serve_conn` (GET, 3 query pairs, 4 headers) | 76–79 µs | | |

What the rule adds to that request, estimated from the measured per-call deltas and the per-request call counts in the code. The embedded std (`include_str!`) can't be swapped for a modified copy without editing std and rebuilding, so this is an estimate, not a timing.

| call | calls per request | rule applied mechanically | with these recommendations |
|---|---|---|---|
| `_percent_decode(…, fail=, where=)` | 7 | +4.0 µs | 0 |
| `_parse_query(…, fail=, where=)` | 1 | +0.6 µs | 0 |
| `_check_header(…, kind=)` | 5 | +2.1 µs | 0 |
| `Response(…)` inside `text()` | 1 | +0.75 µs | +0.63 µs |
| `io.buffer(b=b"")` for the bodiless request | 1 | +0.3 µs | 0 |
| **total** | | **about +7.7 µs, about 10%** | **about +0.6 µs, about 0.8%** |

Other hot or semi-hot defaulted parameters:
- `io.read(self.r, 2)` runs once per chunk: +430 ns per chunk if named, 0 if split.
- `serve_conn(max_requests=)` runs once per connection.
- Client-side `stream`, `write_request`, `read_response` and `Response(version=, reason=)` run once per outbound call, which is negligible next to a TCP connect.
- `Request.header(default=)`, `text(status=)`, `quote_plus(safe=)`, `json.stringify(indent=)` and `_check_headers(kind=)` run per request or per element, but are omitted on the hot path, so they cost nothing.

Where std calls into Rust (the builtins agents' scope):
- `_io.buffer`, `_io.read_all`, `_json.parse`, `_json.stringify(value, indent)`
- `_pct.encode(b, safe)`, `_pct.decode(b, plus)`
- `r.read_until(delim, limit)`, `bytes.scan`, `find(…, reverse=true)`, `split(b" ", 2)`
- `net.listen`, `net.dial`, `conn.set_timeout`, `dict.get(k, default)`

## Test and fixture defs (aggregate)

- **Defs:** 379 fixture defs across `examples/`, `corpus/`, `tests/programs/` and `bench/progs/`. 10 of them have a defaulted parameter.
- **Calls:** 1063 resolved calls to fixture defs (examples 51, corpus 972, tests 18, bench 22). 63 of them go to the 10 defaulted defs.
- **Affected by the rule: 18 call sites, 19 arguments.**
  - **Rule (a), a defaulted parameter passed by position:** 8 sites, 9 arguments (corpus 7 sites, tests 1).
  - **Rule (b), a required parameter passed by name:** 10 sites, 10 arguments (all in corpus).
- **Deliberate binding or refusal tests (10 of the 18):**
  - `corpus/core/72_keyword_binding.oro`: 6 sites. It tests keyword binding itself and needs rewriting against the new semantics.
  - `corpus/divergence/47_spawn.oro:86-87`: 4 sites inside `refusal(() => …)`. They stay errors, but the expected messages may change.
- **Ordinary calls (8 of the 18):**
  - `corpus/core/06_functions.oro:14`
  - `corpus/divergence/61_http_client.oro:227-228`
  - `tests/programs/varargs.oro:21`
  - `corpus/divergence/53_yield_now.oro:26,27,42,43`: `spawn(counting, log, "a", n=3)`, where `n` **has no default**. a2b47b3 wrote these to name a tuning argument; the new rule makes them illegal again.
- **`**` unpacking that binds a required parameter by name:** 3 more sites:
  - `72_keyword_binding.oro:27`: `f(**{"a": 7, "b": 8})`
  - `72_keyword_binding.oro:58`, a refusal test
  - `47_spawn.oro:69`: `spawn(configure, **{"name": "q", "retries": 3})`
  - Also `72_keyword_binding.oro:26`: `f(*[1, 2], …)` feeds defaulted `b` from `*`.

## Needs a decision

1. **`Response`'s `status`.** Required (`http.Response(404, body=…)`) or kept keyword-only (`status=404`) to match `text(…, status=)`, `json_response(…, status=)` and `BadRequest(…, status=)`? I recommend required. The per-request cost difference is only about 0.1 µs, so this is about consistency against the fact that every construction states a status.
2. **`io.buffer`.** Required (`io.buffer(b"")` replaces the bare `io.buffer()`), or named? Named makes the parameter's name public surface, and `b` would need renaming (for example `data=`). I recommend required.
3. **`io.read(r, n=null)`.** Split into to-EOF and exactly-n names (their failure and memory contracts differ, and the split keeps the per-chunk call positional), or named `n=`? A split adds a name to the frozen `io` surface.
4. **`Request.header` / `Response.header(default=)`.** These wrap `dict.get(key, default)`, so they should follow whatever the builtins agent decides for `dict.get`.
5. **`write_response(…, keep_alive)`.** Lock in a bare positional boolean, or default it to `false` so it becomes keyword-only?
6. **`http.Request(...)`.** Six positional-only arguments with filler values (`"1.1"`, `{}`, `io.buffer(b"")`). Should it gain defaults and keyword-only arguments?
7. **`**mapping` and `*seq` at a call site.** Rule 1 says a required parameter cannot be passed by name. Does that also forbid binding one from a `**` mapping (3 fixture sites)? Rule 2 says a defaulted parameter cannot be passed by position; does that forbid feeding one from `*`? The rule only settles `*args`/`**kwargs` as *parameters*.
8. **`53_yield_now` (fixture).** a2b47b3 named `n=3` on a parameter with no default. Revert those four calls to positional, or give `n` a default?

Settled, with no decision needed:
- `write_request(headers=)` and `read_response(method=)` should drop their defaults. Running them showed the defaults are dead or harmful.
- The earlier audit's objection to a keyword-only rule ("`serve` would be unspawnable") no longer applies: `spawn` forwards keywords, verified by `spawn(f, 1, c=7)`.
