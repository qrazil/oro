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
| method | best-of-3 wall clock |

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
| `chain` | the collection protocol (`.filter`/`.map`/`.reduce` with `=>`) — oro-only, CPython twin in `chain.py` |

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
