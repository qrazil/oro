//! `_pct`: the percent-codec behind `std/http.oro`'s URL encoding.
//!
//! ## Why this is in Rust, and why it is only *half* of percent-encoding
//!
//! `docs/stdlib-server-design.md` §5 draws the line: a per-byte loop goes in
//! Rust, and anything carrying policy stays in Oro. Percent-encoding is both,
//! and the two halves separate cleanly:
//!
//! * **The form** — `%` followed by two hex digits, uppercase on the way out
//!   and either case on the way in, over UTF-8 octets — is RFC 3986 and is
//!   frozen the way JSON's grammar is frozen. It is also the per-byte loop.
//!   That is this file.
//! * **The policy** — *which* bytes have to be escaped — is different for a
//!   path segment, a query value, a form body and a fragment, and getting it
//!   wrong is how an encoder produces a URL that works until somebody types a
//!   `+` or a `/`. Every one of those sets lives in `std/http.oro`, in Oro,
//!   where it can be read and argued with. Nothing here knows that a URL
//!   exists: `encode` is handed the safe set, exactly as `bytes.scan` is
//!   handed the allowed set.
//!
//! The number that sent it here. A pure-Oro encoder measured **0.27–0.55 µs
//! per byte** across the realistic shapes (a 200-byte query value: 91–112 µs
//! per call for the per-byte spelling, 54 µs for the fastest `bytes.scan`
//! composition) against CPython's `urllib.parse.quote` at 1.5–2.5 µs *total*.
//! That is 20–70×, and — the part that decides it — a single 200-byte query
//! value cost more than `read_request` spends parsing an entire 430-byte
//! request head (76.4 µs, §5). Decoding was the same story from the other
//! side and was already on the server's per-request path: 55 µs for a
//! 175-byte query string, 100× CPython's `unquote_plus`.
//!
//! ## Underscored
//!
//! Like `_io` and `_json`, `_pct` resolves only from inside a stdlib module
//! body, so it is not part of the language's surface and is not frozen at
//! 1.0. `http.quote` / `http.unquote` are the surface; this is the loop.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::value::{Builtin, Module, VResult, Value};

/// Build the `_pct` module. Called by `crate::vm::modules::build`.
pub fn build() -> Value {
    let mut map: HashMap<Rc<str>, Value> = HashMap::new();
    map.insert(Rc::from("encode"), builtin("_pct.encode", encode));
    map.insert(Rc::from("decode"), builtin("_pct.decode", decode));
    Value::Module(Rc::new(Module { name: Rc::from("_pct"), members: RefCell::new(map) }))
}

fn builtin(name: &'static str, func: fn(Vec<Value>) -> VResult<Value>) -> Value {
    Value::Builtin(Rc::new(Builtin { name, func }))
}

/// A 256-bit membership table, built once per call from a `bytes` argument —
/// the same shape `bytes.scan` builds, and for the same reason: one pass over
/// the (short) set, then one shift and one test per byte of the subject.
fn membership(set: &[u8]) -> [u64; 4] {
    let mut t = [0u64; 4];
    for &c in set {
        t[(c >> 6) as usize] |= 1u64 << (c & 63);
    }
    t
}

fn has(t: &[u64; 4], c: u8) -> bool {
    t[(c >> 6) as usize] & (1u64 << (c & 63)) != 0
}

/// `_pct.encode(b, safe)` — every byte of `b` that is not in `safe` becomes
/// `%HH`, with **uppercase** hex.
///
/// Uppercase because RFC 3986 §6.2.2.1 says producers should normalise that
/// way and because it is what every reference encoder emits, `urllib`'s
/// included; both cases decode, so this is a convention rather than a
/// requirement, and matching the convention is what makes two encoders'
/// output comparable.
///
/// There is no unreserved set baked in here, deliberately. A caller who omits
/// `A-Za-z0-9-._~` from `safe` gets them escaped, which is legal (`%41` and
/// `A` are the same character) and is not this function's business to
/// prevent. `std/http.oro` puts them in every set it builds.
fn encode(args: Vec<Value>) -> VResult<Value> {
    let (b, safe) = match args.as_slice() {
        [Value::Bytes(b), Value::Bytes(safe)] => (b, safe),
        [_, _] => return Err("internal: _pct.encode takes two bytes".to_string()),
        _ => return Err("internal: _pct.encode takes two arguments".to_string()),
    };
    let table = membership(safe);
    // The common case is a component that needs no escaping at all, and the
    // result is then the argument. Checking first costs one pass and saves an
    // allocation and a copy; `bytes` is immutable, so handing back the same
    // `Rc` is the whole of "returning it unchanged".
    if b.iter().all(|&c| has(&table, c)) {
        return Ok(Value::Bytes(Rc::clone(b)));
    }
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out: Vec<u8> = Vec::with_capacity(b.len() + 8);
    for &c in b.iter() {
        if has(&table, c) {
            out.push(c);
        } else {
            out.push(b'%');
            out.push(HEX[(c >> 4) as usize]);
            out.push(HEX[(c & 15) as usize]);
        }
    }
    Ok(Value::bytes(out))
}

fn hex_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// `_pct.decode(b, plus_is_space)` — the inverse, and the exact inverse: for
/// any `safe` that excludes `%` (and excludes `+` when `plus_is_space`),
/// `decode(encode(b, safe)) == b` for arbitrary bytes.
///
/// **A malformed escape is refused, not passed through as a literal `%`.**
/// That is a deliberate divergence from `urllib.parse.unquote`, which leaves
/// `%zz` and a truncated `%4` alone; the reasoning is `std/http.oro`'s and
/// predates this file — guessing what a malformed escape meant is how two
/// intermediaries end up disagreeing about a path, which is a
/// request-smuggling primitive and not a leniency.
///
/// Refused here is `null`, and not a raise, because **which** exception it
/// becomes is policy and policy is Oro's: the same bad escape is a 400 on the
/// way in and a `ValueError` for a URL the program itself got wrong, and the
/// message names a different noun in each case. So this returns the bytes or
/// nothing, `std/http.oro`'s `_percent_decode` decides what that means, and
/// `_escape_fault` — which runs only on input already known to be bad — says
/// whether it was truncated or non-hex. A codec that raised would have had to
/// pick one of those answers for both callers.
///
/// `plus_is_space` is the caller's, for the reason it always was: in a query
/// or a form body `+` means a space, and in a path it means a `+`. Nothing
/// here can know which one it was handed.
fn decode(args: Vec<Value>) -> VResult<Value> {
    let (b, plus) = match args.as_slice() {
        [Value::Bytes(b), Value::Bool(plus)] => (b, *plus),
        [_, _] => return Err("internal: _pct.decode takes bytes and a bool".to_string()),
        _ => return Err("internal: _pct.decode takes two arguments".to_string()),
    };
    // Nothing to do is the overwhelmingly common case for a path, and it is
    // also the case the Oro version got wrong: its fast path was `no '%' and
    // not plus_is_space`, so every query string in the language walked itself
    // byte by byte even when it held neither a `%` nor a `+`.
    let first = b.iter().position(|&c| c == b'%' || (plus && c == b'+'));
    let Some(first) = first else {
        return Ok(Value::Bytes(Rc::clone(b)));
    };
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    out.extend_from_slice(&b[..first]);
    let mut i = first;
    while i < b.len() {
        let c = b[i];
        if c == b'%' {
            if i + 2 >= b.len() {
                return Ok(Value::None);
            }
            match (hex_value(b[i + 1]), hex_value(b[i + 2])) {
                (Some(hi), Some(lo)) => out.push(hi * 16 + lo),
                _ => return Ok(Value::None),
            }
            i += 3;
        } else if c == b'+' && plus {
            out.push(b' ');
            i += 1;
        } else {
            out.push(c);
            i += 1;
        }
    }
    Ok(Value::bytes(out))
}
