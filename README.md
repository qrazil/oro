# Oro

Oro (named for the *ouroboros*) is a small, Python-inspired scripting language
implemented in Rust and run on a bytecode virtual machine.

> **Status:** early scaffolding. Only the lexer is implemented.

## Design at a glance

These decisions are locked:

- **Python-like syntax**, indentation-based blocks, **no braces**.
- **Tabs are rejected outright** in leading whitespace — a hard error. This
  deliberately designs out the tab/space indentation-ambiguity bug class.
- File extension: `.oro`.
- **Bytecode VM** (not tree-walking, not JIT).
- **Reference counting via `Rc`** — the runtime is single-threaded (no `Arc`).
- **Heap-allocated call frames**: the interpreter never recurses in Rust to
  call a script-level function.
- **Static, slot-based name resolution** via a two-pass compiler.
- Integers are `i64` inline and **promote to bignum on overflow**.
- Strings are UTF-8 bytes plus an `is_ascii` flag for O(1) ASCII indexing.
- Goal: a single self-contained binary with **zero runtime dependencies**.

### Language subset (frozen)

Classes with single inheritance, exceptions (`try`/`except`/`finally`/`raise`),
generators/`yield`, f-strings, `if`/`elif`/`else`, `for`/`while`, lists/dicts/
tuples/sets, Python truthiness, and operator overloading via a fixed dunder set.

**Deliberately cut:** walrus, `match`, comprehensions, decorators, metaclasses,
`__getattr__`, `eval`, `exec`, `globals()`, multiple inheritance, `with`, bare
`except:`, `from x import y`, `import *`.

## Building

```sh
cargo build          # builds the `oro` binary
cargo test           # runs the test suite
cargo run -- file.oro  # lex a file and print its token stream
```

## Layout

- `src/main.rs` — the `oro` CLI (currently a lexer harness).
- `src/lib.rs` — the `oro_lang` library crate.
- `src/lexer/` — hand-written lexer with an INDENT/DEDENT engine.

## License

Licensed under either of MIT or Apache-2.0 at your option.
