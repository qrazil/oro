//! Builtin functions and type methods.
//!
//! Builtins are plain `fn(Vec<Value>) -> VResult<Value>` pointers wrapped in a
//! [`Value::Builtin`]; the VM invokes them natively and, crucially, none of them
//! ever calls back into Oro, so the single flat interpreter loop is never
//! re-entered. Type methods (string and a few container helpers) are dispatched
//! by name through [`call_method`].

use std::cell::RefCell;
use std::fmt::Write as _;
use std::rc::Rc;

use crate::bigint::BigInt;
use crate::value::{Builtin, OroDict, RangeVal, VResult, Value};

/// Look up a global name. Oro's only globals are the builtins.
pub fn lookup(name: &str) -> Option<Value> {
    let f: fn(Vec<Value>) -> VResult<Value> = match name {
        "print" => bi_print,
        "len" => bi_len,
        "range" => bi_range,
        "str" => bi_str,
        "bytes" => bi_bytes,
        "int" => bi_int,
        "float" => bi_float,
        "bool" => bi_bool,
        "type" => bi_type,
        "abs" => bi_abs,
        "min" => bi_min,
        "max" => bi_max,
        "sum" => bi_sum,
        "sorted" => bi_sorted,
        "isinstance" => bi_isinstance,
        "repr" => bi_repr,
        "open" => bi_open,
        "set" => bi_set,
        "list" => bi_list,
        "dict" => bi_dict,
        "enumerate" => bi_enumerate,
        "zip" => bi_zip,
        "any" => bi_any,
        "all" => bi_all,
        "round" => bi_round,
        "chr" => bi_chr,
        "ord" => bi_ord,
        _ => return None,
    };
    Some(Value::Builtin(Rc::new(Builtin { name: intern(name), func: f })))
}

/// Map a builtin name to its `'static` spelling for the [`Builtin`] struct.
fn intern(name: &str) -> &'static str {
    match name {
        "print" => "print",
        "len" => "len",
        "range" => "range",
        "str" => "str",
        "bytes" => "bytes",
        "int" => "int",
        "float" => "float",
        "bool" => "bool",
        "type" => "type",
        "abs" => "abs",
        "min" => "min",
        "max" => "max",
        "sum" => "sum",
        "sorted" => "sorted",
        "isinstance" => "isinstance",
        "repr" => "repr",
        "open" => "open",
        "set" => "set",
        "list" => "list",
        "dict" => "dict",
        "enumerate" => "enumerate",
        "zip" => "zip",
        "any" => "any",
        "all" => "all",
        "round" => "round",
        "chr" => "chr",
        "ord" => "ord",
        _ => "builtin",
    }
}

// --- Argument helpers -------------------------------------------------------

fn exactly(args: &[Value], n: usize, who: &str) -> VResult<()> {
    if args.len() != n {
        Err(format!("{who}() takes {n} argument(s) but {} were given", args.len()))
    } else {
        Ok(())
    }
}

// --- Builtins ---------------------------------------------------------------

fn bi_print(args: Vec<Value>) -> VResult<Value> {
    let mut out = String::new();
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&a.display());
    }
    println!("{out}");
    Ok(Value::None)
}

fn bi_len(args: Vec<Value>) -> VResult<Value> {
    exactly(&args, 1, "len")?;
    let n = match &args[0] {
        Value::Str(s) => s.char_len(),
        Value::Bytes(b) => b.len(),
        Value::List(l) => l.borrow().len(),
        Value::Tuple(t) => t.len(),
        Value::Dict(d) => d.borrow().len(),
        Value::Range(r) => r.len(),
        other => return Err(format!("object of type '{}' has no len()", other.type_name())),
    };
    Ok(Value::Int(n as i64))
}

fn bi_range(args: Vec<Value>) -> VResult<Value> {
    let ints: Vec<i64> = args.iter().map(as_i64).collect::<VResult<_>>()?;
    let (start, stop, step) = match ints.as_slice() {
        [stop] => (0, *stop, 1),
        [start, stop] => (*start, *stop, 1),
        [start, stop, step] => (*start, *stop, *step),
        _ => return Err("range() takes 1 to 3 integer arguments".to_string()),
    };
    if step == 0 {
        return Err("range() step argument must not be zero".to_string());
    }
    Ok(Value::Range(Rc::new(RangeVal { start, stop, step })))
}

fn bi_str(_args: Vec<Value>) -> VResult<Value> {
    Err(type_name_is_not_callable("str", "to_str", "\"\""))
}

fn bi_bytes(_args: Vec<Value>) -> VResult<Value> {
    Err(type_name_is_not_callable("bytes", "to_bytes", "b\"\""))
}

fn bi_repr(args: Vec<Value>) -> VResult<Value> {
    exactly(&args, 1, "repr")?;
    Ok(Value::str(args[0].repr()))
}

fn bi_set(_args: Vec<Value>) -> VResult<Value> {
    Err("set() is not supported in Oro — sets are cut. Use a dict for membership \
         (`{k: True}`, then `k in d`), or dedup with a loop that skips keys already in a dict; \
         a Set data structure may return in the stdlib."
        .to_string())
}

fn bi_open(args: Vec<Value>) -> VResult<Value> {
    use crate::stream::OroStream;
    let (path, mode) = match args.as_slice() {
        [Value::Str(p)] => (p.s.clone(), "r".to_string()),
        [Value::Str(p), Value::Str(m)] => (p.s.clone(), m.s.clone()),
        [_] | [_, _] => return Err("open() arguments must be strings".to_string()),
        _ => return Err("open() takes 1 or 2 arguments".to_string()),
    };
    let io_err = |e: std::io::Error| crate::vm::modules::io_err(&e, &path);
    let stream = match mode.as_str() {
        "r" => OroStream::open_read(&path).map_err(io_err)?,
        "w" => OroStream::open_write(&path).map_err(io_err)?,
        "a" => OroStream::open_append(&path).map_err(io_err)?,
        // Every mode is bytes, so the `b` contrasts with nothing: it would be a
        // letter meaning "not the other kind" in a language that has no other
        // kind. The error names the replacement rather than quietly accepting
        // it (see `docs/stdlib-server-design.md` §2).
        "rb" | "wb" | "ab" => {
            return Err(format!(
                "invalid file mode '{mode}' — open() has no 'b' suffix because there is no text \
                 mode to contrast with: every stream in Oro is bytes. Use '{}'.",
                &mode[..1]
            ))
        }
        other => return Err(format!("invalid file mode '{other}' (use 'r', 'w', or 'a')")),
    };
    Ok(Value::Stream(Rc::new(stream)))
}

fn bi_int(_args: Vec<Value>) -> VResult<Value> {
    Err(type_name_is_not_callable("int", "to_int", "0"))
}

fn bi_float(_args: Vec<Value>) -> VResult<Value> {
    Err(type_name_is_not_callable("float", "to_float", "0.0"))
}

fn bi_bool(_args: Vec<Value>) -> VResult<Value> {
    Err(type_name_is_not_callable("bool", "to_bool", "False"))
}

fn bi_type(args: Vec<Value>) -> VResult<Value> {
    exactly(&args, 1, "type")?;
    match &args[0] {
        // The type of a user instance is its class object.
        Value::Instance(i) => Ok(Value::Class(i.class.clone())),
        other => Ok(Value::str(format!("<class '{}'>", other.type_name()))),
    }
}

fn bi_isinstance(args: Vec<Value>) -> VResult<Value> {
    exactly(&args, 2, "isinstance")?;
    let obj = &args[0];
    let ok = match &args[1] {
        Value::Class(cls) => match obj {
            Value::Instance(i) => crate::value::Class::is_subclass(&i.class, cls),
            _ => false,
        },
        // A builtin type name (e.g. `int`, `str`) matches by type name.
        Value::Builtin(b) => builtin_type_matches(b.name, obj),
        other => {
            return Err(format!(
                "isinstance() arg 2 must be a class, not '{}'",
                other.type_name()
            ))
        }
    };
    Ok(Value::Bool(ok))
}

/// Whether `obj` matches a builtin type-constructor name used as `isinstance`'s
/// second argument (`isinstance(x, int)`).
fn builtin_type_matches(name: &str, obj: &Value) -> bool {
    match name {
        "int" => matches!(obj, Value::Int(_) | Value::Big(_) | Value::Bool(_)),
        "float" => matches!(obj, Value::Float(_)),
        "bool" => matches!(obj, Value::Bool(_)),
        "str" => matches!(obj, Value::Str(_)),
        "bytes" => matches!(obj, Value::Bytes(_)),
        _ => false,
    }
}

fn bi_abs(args: Vec<Value>) -> VResult<Value> {
    exactly(&args, 1, "abs")?;
    match &args[0] {
        Value::Bool(b) => Ok(Value::Int(*b as i64)),
        Value::Int(i) => Ok(match i.checked_abs() {
            Some(v) => Value::Int(v),
            None => Value::from_bigint(BigInt::from_i64(*i).abs()),
        }),
        Value::Big(b) => Ok(Value::from_bigint(b.abs())),
        Value::Float(f) => Ok(Value::Float(f.abs())),
        other => Err(format!("bad operand type for abs(): '{}'", other.type_name())),
    }
}

fn bi_min(args: Vec<Value>) -> VResult<Value> {
    fold_extreme(args, "min", std::cmp::Ordering::Less)
}

fn bi_max(args: Vec<Value>) -> VResult<Value> {
    fold_extreme(args, "max", std::cmp::Ordering::Greater)
}

/// Shared core of `min`/`max`. With one argument it ranges over an iterable;
/// with several it ranges over the arguments themselves.
fn fold_extreme(args: Vec<Value>, who: &str, want: std::cmp::Ordering) -> VResult<Value> {
    let items = match args.len() {
        0 => return Err(format!("{who}() expected at least 1 argument")),
        1 => crate::vm::iterate_to_vec(&args[0])?,
        _ => args,
    };
    let mut it = items.into_iter();
    let mut best = it.next().ok_or_else(|| format!("{who}() arg is an empty sequence"))?;
    for v in it {
        if v.compare(&best)? == want {
            best = v;
        }
    }
    Ok(best)
}

fn bi_sum(args: Vec<Value>) -> VResult<Value> {
    let (iterable, start) = match args.as_slice() {
        [it] => (it, Value::Int(0)),
        [it, start] => (it, start.clone()),
        _ => return Err("sum() takes 1 or 2 arguments".to_string()),
    };
    let mut acc = start;
    for v in crate::vm::iterate_to_vec(iterable)? {
        acc = crate::vm::add_values(&acc, &v)?;
    }
    Ok(acc)
}

fn bi_sorted(args: Vec<Value>) -> VResult<Value> {
    let iterable = match args.as_slice() {
        [it] => it,
        _ => return Err("sorted() takes exactly 1 argument".to_string()),
    };
    let mut items = crate::vm::iterate_to_vec(iterable)?;
    sort_values(&mut items)?;
    Ok(Value::List(Rc::new(RefCell::new(items))))
}

/// Stable sort of `items` by the matching entry in `keys` (the classic
/// decorate-sort-undecorate). `reverse` inverts the *comparator* rather than
/// reversing the result: `sort_by` is stable, so equal keys keep their original
/// order in both directions — which is what CPython guarantees. Reversing the
/// sorted output instead would flip ties and break that.
pub fn sort_by_keys(items: Vec<Value>, keys: &[Value], reverse: bool) -> VResult<Vec<Value>> {
    let mut idx: Vec<usize> = (0..items.len()).collect();
    let mut err: Option<String> = None;
    idx.sort_by(|&a, &b| {
        if err.is_some() {
            return std::cmp::Ordering::Equal;
        }
        let (lhs, rhs) = if reverse { (b, a) } else { (a, b) };
        match keys[lhs].compare(&keys[rhs]) {
            Ok(o) => o,
            Err(e) => {
                err = Some(e);
                std::cmp::Ordering::Equal
            }
        }
    });
    if let Some(e) = err {
        return Err(e);
    }
    let mut slots: Vec<Option<Value>> = items.into_iter().map(Some).collect();
    Ok(idx.into_iter().map(|i| slots[i].take().expect("index used once")).collect())
}

/// Stable sort by Oro's `<` ordering. Because [`Value::compare`] is fallible
/// (unorderable pairs are a `TypeError`), we capture the first error and, once
/// tripped, treat every remaining comparison as `Equal` so the sort finishes
/// quickly before we surface the error.
fn sort_values(items: &mut [Value]) -> VResult<()> {
    let mut err: Option<String> = None;
    items.sort_by(|a, b| {
        if err.is_some() {
            return std::cmp::Ordering::Equal;
        }
        match a.compare(b) {
            Ok(ord) => ord,
            Err(e) => {
                err = Some(e);
                std::cmp::Ordering::Equal
            }
        }
    });
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

// --- Numeric conversion helpers ---------------------------------------------

fn as_i64(v: &Value) -> VResult<i64> {
    match v {
        Value::Bool(b) => Ok(*b as i64),
        Value::Int(i) => Ok(*i),
        other => Err(format!("expected an integer, got '{}'", other.type_name())),
    }
}

fn float_to_int(f: f64) -> Value {
    // Truncate toward zero. Large magnitudes go through BigInt via the decimal
    // form so we never silently saturate.
    if f.is_finite() && f >= i64::MIN as f64 && f <= i64::MAX as f64 {
        Value::Int(f.trunc() as i64)
    } else if f.is_finite() {
        let s = format!("{:.0}", f.trunc());
        let neg = s.starts_with('-');
        let digits = s.trim_start_matches('-');
        match BigInt::parse_decimal(digits) {
            Some(b) => Value::from_bigint(if neg { b.neg() } else { b }),
            None => Value::Int(0),
        }
    } else {
        Value::Int(0)
    }
}

fn parse_int_str(s: &str) -> VResult<Value> {
    let t = s.trim();
    if let Ok(i) = t.parse::<i64>() {
        return Ok(Value::Int(i));
    }
    let (neg, digits) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    match BigInt::parse_decimal(digits) {
        Some(b) => Ok(Value::from_bigint(if neg { b.neg() } else { b })),
        None => Err(format!("invalid literal for int(): '{s}'")),
    }
}

// --- Methods ----------------------------------------------------------------

/// Whether `name` is a valid method of `recv`'s type (drives attribute access).
/// `int(s, base)`. Accepts an optional sign, the `0x`/`0o`/`0b` prefix when it
/// agrees with `base`, and underscore separators — matching CPython.
fn parse_int_base(s: &str, base: i64) -> VResult<Value> {
    if base != 0 && !(2..=36).contains(&base) {
        return Err("int() base must be >= 2 and <= 36, or 0".to_string());
    }
    let t = s.trim();
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let lower = t.to_ascii_lowercase();
    let (base, digits) = match (base, lower.get(..2)) {
        (0, Some("0x")) | (16, Some("0x")) => (16, &t[2..]),
        (0, Some("0o")) | (8, Some("0o")) => (8, &t[2..]),
        (0, Some("0b")) | (2, Some("0b")) => (2, &t[2..]),
        (0, _) => (10, t),
        (b, _) => (b, t),
    };
    let cleaned: String = digits.chars().filter(|c| *c != '_').collect();
    if cleaned.is_empty() {
        return Err(format!("invalid literal for int() with base {base}: '{s}'"));
    }
    match i64::from_str_radix(&cleaned, base as u32) {
        Ok(n) => Ok(Value::Int(if neg { -n } else { n })),
        Err(_) => Err(format!("invalid literal for int() with base {base}: '{s}'")),
    }
}

/// Type names convert; they do not construct. `list(xs)` casts an iterable to a
/// list, but `list()` — a second, wordier spelling of `[]` — is an error, and so
/// is `str()` for `""`, `int()` for `0`, and the rest. One spelling per thing:
/// literals build, type names convert.
/// Conversion is spelled as a method on the value being converted —
/// `xs.to_list()`, `"42".to_int()` — not as a call on a type name. One spelling
/// per thing, and it chains: `xs.filter(p).map(f).to_list()` reads in the order
/// it runs, where `list(xs.filter(p).map(f))` makes you read outward again.
///
/// It also removes a whole error class by construction: a conversion needs
/// something to convert, so there is no zero-argument form to confuse with
/// building an empty collection. For that, write the literal.
fn type_name_is_not_callable(who: &str, method: &str, literal: &str) -> String {
    format!(
        "{who}() is not callable in Oro — write `x.{method}()` to convert a value, or the \
         literal `{literal}` to build an empty one."
    )
}

fn bi_list(_args: Vec<Value>) -> VResult<Value> {
    Err(type_name_is_not_callable("list", "to_list", "[]"))
}

fn bi_dict(_args: Vec<Value>) -> VResult<Value> {
    Err(type_name_is_not_callable("dict", "to_dict", "{}"))
}

/// `enumerate(it, start=0)`. Eager: returns a list of `(index, value)` tuples
/// rather than a lazy iterator, the same choice `dict.keys()` already makes.
fn bi_enumerate(args: Vec<Value>) -> VResult<Value> {
    let (it, start) = match args.as_slice() {
        [it] => (it, 0),
        [it, s] => (it, as_i64(s)?),
        _ => return Err("enumerate() takes 1 or 2 arguments".to_string()),
    };
    let items = crate::vm::iterate_to_vec(it)?;
    let mut out = Vec::with_capacity(items.len());
    for (i, v) in items.into_iter().enumerate() {
        out.push(Value::Tuple(Rc::new(vec![Value::Int(start + i as i64), v])));
    }
    Ok(Value::List(Rc::new(RefCell::new(out))))
}

/// `zip(a, b, ...)`, truncating to the shortest input. Eager, like `enumerate`.
fn bi_zip(args: Vec<Value>) -> VResult<Value> {
    if args.is_empty() {
        return Ok(Value::List(Rc::new(RefCell::new(Vec::new()))));
    }
    let mut cols = Vec::with_capacity(args.len());
    for a in &args {
        cols.push(crate::vm::iterate_to_vec(a)?);
    }
    let n = cols.iter().map(|c| c.len()).min().unwrap_or(0);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let row: Vec<Value> = cols.iter().map(|c| c[i].clone()).collect();
        out.push(Value::Tuple(Rc::new(row)));
    }
    Ok(Value::List(Rc::new(RefCell::new(out))))
}

fn bi_any(args: Vec<Value>) -> VResult<Value> {
    exactly(&args, 1, "any")?;
    for v in crate::vm::iterate_to_vec(&args[0])? {
        if v.truthy() {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

fn bi_all(args: Vec<Value>) -> VResult<Value> {
    exactly(&args, 1, "all")?;
    for v in crate::vm::iterate_to_vec(&args[0])? {
        if !v.truthy() {
            return Ok(Value::Bool(false));
        }
    }
    Ok(Value::Bool(true))
}

/// `round(x)` -> int, `round(x, n)` -> float. Uses banker's rounding (ties to
/// even) exactly as CPython does: `round(0.5)` is 0 and `round(2.5)` is 2.
fn bi_round(args: Vec<Value>) -> VResult<Value> {
    let (v, ndigits) = match args.as_slice() {
        [v] => (v, None),
        [v, n] => (v, Some(as_i64(n)?)),
        _ => return Err("round() takes 1 or 2 arguments".to_string()),
    };
    let x = match v {
        Value::Int(n) => {
            // Rounding an int is the identity at ndigits >= 0; a negative
            // ndigits rounds to that power of ten and stays an int.
            match ndigits {
                Some(d) if d < 0 => {
                    let factor = 10i64.pow((-d).min(18) as u32);
                    let scaled = *n as f64 / factor as f64;
                    return Ok(Value::Int(round_half_even(scaled) as i64 * factor));
                }
                _ => return Ok(v.clone()),
            }
        }
        Value::Big(_) => return Ok(v.clone()),
        Value::Bool(b) => return Ok(Value::Int(*b as i64)),
        Value::Float(f) => *f,
        other => {
            return Err(format!(
                "type '{}' doesn't define __round__ method",
                other.type_name()
            ))
        }
    };
    match ndigits {
        None => Ok(Value::Int(round_half_even(x) as i64)),
        // Scaling by 10^n and rounding gives the wrong answer whenever the
        // scaled product is not exactly representable (the classic
        // round(2.675, 2) case). Rust's float formatter rounds the *exact*
        // binary value half-to-even, which is precisely CPython's rule, so
        // format-and-reparse rather than doing the arithmetic ourselves.
        Some(n) if n >= 0 => {
            if !x.is_finite() {
                return Ok(Value::Float(x));
            }
            let digits = n.min(17) as usize;
            let text = format!("{x:.digits$}");
            Ok(Value::Float(text.parse::<f64>().unwrap_or(x)))
        }
        Some(n) => {
            // Negative ndigits rounds to a power of ten left of the point.
            let factor = 10f64.powi((-n) as i32);
            let scaled = x / factor;
            if !scaled.is_finite() {
                return Ok(Value::Float(x));
            }
            Ok(Value::Float(round_half_even(scaled) * factor))
        }
    }
}

/// `chr(n)` / `ord(c)` — the codepoint pair. Until now the only way across this
/// boundary was the f-string `{n:c}` spec, which goes one way only; writing the
/// `json` module in Oro made the gap obvious.
fn bi_chr(args: Vec<Value>) -> VResult<Value> {
    exactly(&args, 1, "chr")?;
    let n = as_i64(&args[0])?;
    let cp = u32::try_from(n)
        .ok()
        .and_then(char::from_u32)
        .ok_or_else(|| "chr() arg not in range(0x110000)".to_string())?;
    Ok(Value::str(cp.to_string()))
}

fn bi_ord(args: Vec<Value>) -> VResult<Value> {
    exactly(&args, 1, "ord")?;
    let s = match &args[0] {
        Value::Str(s) => s.s.clone(),
        other => {
            return Err(format!(
                "ord() expected a character, but got '{}'",
                other.type_name()
            ))
        }
    };
    let mut it = s.chars();
    match (it.next(), it.next()) {
        (Some(c), None) => Ok(Value::Int(c as i64)),
        _ => Err(format!(
            "ord() expected a character, but string of length {} found",
            s.chars().count()
        )),
    }
}

/// Round to nearest, ties to even — the rule CPython's `round` follows.
fn round_half_even(x: f64) -> f64 {
    let r = x.round();
    if (x - x.trunc()).abs() == 0.5 && r % 2.0 != 0.0 {
        r - x.signum()
    } else {
        r
    }
}

/// The types the collection protocol applies to.
pub fn is_collection(v: &Value) -> bool {
    matches!(
        v,
        Value::List(_) | Value::Tuple(_) | Value::Dict(_) | Value::Range(_) | Value::Generator(_)
    )
}

pub fn method_exists(recv: &Value, name: &str) -> bool {
    if is_cast_method(name) {
        return true;
    }
    // The collection protocol is uniform across every collection type. Names in
    // it can collide with a per-type method (`str.find`, `str.join`), so this
    // only claims them for collections and otherwise falls through.
    if is_collection(recv) && (is_seq_native(name) || crate::vm::is_seq_op(name)) {
        return true;
    }
    match recv {
        Value::Str(_) => matches!(
            name,
            "split" | "rsplit" | "join" | "strip" | "lstrip" | "rstrip" | "upper"
                | "lower" | "replace" | "startswith" | "endswith" | "find" | "zfill"
        ),
        // The same names `str` carries — every one of them exists on CPython's
        // `bytes` with the same meaning — plus `hex`, which only bytes needs.
        Value::Bytes(_) => matches!(
            name,
            "split" | "rsplit" | "join" | "strip" | "lstrip" | "rstrip" | "upper"
                | "lower" | "replace" | "startswith" | "endswith" | "find" | "zfill"
                | "hex"
        ),
        Value::List(_) => {
            matches!(name, "append" | "pop" | "extend" | "sort" | "reverse" | "map" | "filter")
        }
        // map/filter are type-preserving, so every collection carries them.
        Value::Tuple(_) | Value::Range(_) | Value::Generator(_) => {
            matches!(name, "map" | "filter")
        }
        Value::Dict(_) => matches!(name, "get" | "keys" | "values" | "items" | "map" | "filter"),
        // The protocol is `read`/`write`; `read_until` and `close` are methods
        // on the concrete type, the way `bufio.Reader` has `ReadSlice` in Go.
        // There is no `flush` and no line iterator.
        Value::Stream(s) => match s.kind {
            crate::stream::StreamKind::Buffer => {
                matches!(name, "read" | "write" | "read_until" | "close" | "bytes")
            }
            crate::stream::StreamKind::File { .. } => {
                matches!(name, "read" | "write" | "read_until" | "close")
            }
        },
        Value::Regex(_) => matches!(
            name,
            "search" | "findall" | "finditer" | "fullmatch" | "sub" | "split"
        ),
        Value::Match(_) => matches!(name, "group" | "start" | "end"),
        _ => false,
    }
}

/// Dispatch a bound method call.
pub fn call_method(recv: &Value, name: &str, args: Vec<Value>) -> VResult<Value> {
    match recv {
        _ if is_cast_method(name) => cast_method(recv, name, args),
        _ if is_collection(recv) && is_seq_native(name) => seq_native_method(recv, name, args),
        Value::Str(_) => str_method(recv, name, args),
        Value::Bytes(b) => bytes_method(b, name, args),
        Value::List(l) => list_method(l, name, args),
        Value::Dict(d) => dict_method(d, name, args),
        Value::Stream(s) => stream_method(s, name, args),
        Value::Regex(r) => regex_method(r, name, args),
        Value::Match(m) => match_method(m, name, args),
        other => Err(format!("'{}' object has no method '{}'", other.type_name(), name)),
    }
}

fn regex_method(
    r: &Rc<crate::value::OroRegex>,
    name: &str,
    args: Vec<Value>,
) -> VResult<Value> {
    use crate::regexutil as rx;
    match name {
        "search" => Ok(rx::search(&r.re, &str_arg(&args, 0, "search")?)),
        "fullmatch" => Ok(rx::fullmatch(&r.re, &str_arg(&args, 0, "fullmatch")?)),
        "findall" => Ok(rx::findall(&r.re, &str_arg(&args, 0, "findall")?)),
        "finditer" => Ok(rx::finditer(&r.re, &str_arg(&args, 0, "finditer")?)),
        "split" => Ok(rx::split(&r.re, &str_arg(&args, 0, "split")?)),
        "sub" => {
            let repl = str_arg(&args, 0, "sub")?;
            Ok(rx::sub(&r.re, &repl, &str_arg(&args, 1, "sub")?))
        }
        _ => Err(format!("'Pattern' object has no method '{name}'")),
    }
}

fn match_method(
    m: &Rc<crate::value::OroMatch>,
    name: &str,
    args: Vec<Value>,
) -> VResult<Value> {
    use crate::regexutil as rx;
    // The group index defaults to 0 (the whole match).
    let n = match args.as_slice() {
        [] => 0usize,
        [Value::Int(i)] if *i >= 0 => *i as usize,
        [Value::Int(_)] => return Err("group index must be non-negative".to_string()),
        _ => return Err(format!("{name}() takes an optional group index")),
    };
    match name {
        "group" => rx::group(m, n),
        "start" => rx::start(m, n),
        "end" => rx::end(m, n),
        _ => Err(format!("'Match' object has no method '{name}'")),
    }
}

fn stream_method(
    s: &Rc<crate::stream::OroStream>,
    name: &str,
    args: Vec<Value>,
) -> VResult<Value> {
    match name {
        "read" => {
            let n = match args.as_slice() {
                [Value::Int(n)] => *n,
                // `read()` with no argument would be a second behaviour under
                // one name, and an unbounded read is a memory footgun on a
                // server. Reading a whole stream is `io.read(r)`.
                [] => {
                    return Err("read() takes a size — use io.read(r) to read a whole stream"
                        .to_string())
                }
                _ => {
                    return Err(format!(
                        "read() size argument must be int, not '{}'",
                        type_of(&args, 0)
                    ))
                }
            };
            Ok(Value::bytes(s.read(n)?))
        }
        "write" => {
            // Bytes only, in both directions, everywhere in the language. The
            // spellings for text are `print("hi")` and `w.write(s.to_bytes())`.
            let b = bytes_arg(&args, 0, "write")?;
            s.write(&b)?;
            Ok(Value::None)
        }
        "read_until" => {
            let delim = bytes_arg(&args, 0, "read_until")?;
            let limit = match args.get(1) {
                Some(Value::Int(n)) => *n,
                // The limit is required, not defaulted: it is the thing that
                // stops a client sending an unbounded header block, and a
                // default would be a number nobody chose.
                None => return Err("read_until() takes a delimiter and a limit".to_string()),
                Some(_) => {
                    return Err(format!(
                        "read_until() limit argument must be int, not '{}'",
                        type_of(&args, 1)
                    ))
                }
            };
            exactly(&args, 2, "read_until")?;
            Ok(Value::bytes(s.read_until(&delim, limit)?))
        }
        "bytes" => {
            exactly(&args, 0, "bytes")?;
            Ok(Value::bytes(s.bytes()?))
        }
        "close" => {
            exactly(&args, 0, "close")?;
            s.close()?;
            Ok(Value::None)
        }
        _ => Err(format!("'{}' object has no method '{name}'", s.kind.type_name())),
    }
}

/// The type name of argument `i`, for a diagnostic. `NoneType` when absent, so
/// a missing argument reads the same way a wrong one does.
fn type_of(args: &[Value], i: usize) -> &'static str {
    args.get(i).map(|v| v.type_name()).unwrap_or("NoneType")
}

/// Optional integer argument (Python's `maxsplit`, `width`, ... style).
/// Absent or `None` yields `default`.
fn opt_int_arg(args: &[Value], i: usize, who: &str, default: i64) -> VResult<i64> {
    match args.get(i) {
        None | Some(Value::None) => Ok(default),
        Some(Value::Int(n)) => Ok(*n),
        Some(Value::Bool(b)) => Ok(*b as i64),
        Some(other) => Err(format!(
            "{who}() argument must be int, not '{}'",
            other.type_name()
        )),
    }
}

fn str_arg(args: &[Value], i: usize, who: &str) -> VResult<String> {
    match args.get(i) {
        Some(Value::Str(s)) => Ok(s.s.clone()),
        Some(other) => Err(format!("{who}() argument must be str, not '{}'", other.type_name())),
        None => Err(format!("{who}() missing a required argument")),
    }
}

/// `str.split`/`rsplit` on an explicit separator, honouring `maxsplit`
/// (negative = unlimited). `rsplit` consumes separators from the right, so the
/// *unsplit* remainder ends up in the first element.
fn split_sep_n(s: &str, sep: &str, maxsplit: i64, from_right: bool) -> Vec<String> {
    if maxsplit < 0 {
        return s.split(sep).map(|p| p.to_string()).collect();
    }
    // maxsplit splits => maxsplit + 1 pieces.
    let n = (maxsplit as usize).saturating_add(1);
    if from_right {
        let mut parts: Vec<String> = s.rsplitn(n, sep).map(|p| p.to_string()).collect();
        parts.reverse();
        parts
    } else {
        s.splitn(n, sep).map(|p| p.to_string()).collect()
    }
}

/// `str.split(None, maxsplit)`: runs of whitespace separate, leading/trailing
/// whitespace is discarded, and once `maxsplit` splits are made the remainder is
/// returned verbatim (interior whitespace and all).
fn split_whitespace_n(s: &str, maxsplit: i64, from_right: bool) -> Vec<String> {
    if maxsplit < 0 {
        return s.split_whitespace().map(|p| p.to_string()).collect();
    }
    let limit = maxsplit as usize;
    let chars: Vec<char> = s.chars().collect();
    let mut parts: Vec<String> = Vec::new();

    if !from_right {
        let mut i = 0usize;
        while parts.len() < limit {
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            if i >= chars.len() {
                return parts;
            }
            let start = i;
            while i < chars.len() && !chars[i].is_whitespace() {
                i += 1;
            }
            parts.push(chars[start..i].iter().collect());
        }
        // Remainder: drop only the whitespace that separated it from the last field.
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i < chars.len() {
            parts.push(chars[i..].iter().collect());
        }
        parts
    } else {
        let mut i = chars.len();
        while parts.len() < limit {
            while i > 0 && chars[i - 1].is_whitespace() {
                i -= 1;
            }
            if i == 0 {
                parts.reverse();
                return parts;
            }
            let end = i;
            while i > 0 && !chars[i - 1].is_whitespace() {
                i -= 1;
            }
            parts.push(chars[i..end].iter().collect());
        }
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        if i > 0 {
            parts.push(chars[..i].iter().collect());
        }
        parts.reverse();
        parts
    }
}

/// The conversion methods, available on every value: `to_str`, `to_int`,
/// `to_float`, `to_bool`, `to_list`, `to_dict`. Instances and containers whose
/// `to_str` must run an Oro dunder are intercepted by the VM before reaching
/// here, since a native method can never re-enter the interpreter.
pub fn is_cast_method(name: &str) -> bool {
    matches!(
        name,
        "to_str" | "to_bytes" | "to_int" | "to_float" | "to_bool" | "to_list" | "to_dict"
    )
}

fn cast_method(recv: &Value, name: &str, args: Vec<Value>) -> VResult<Value> {
    match name {
        "to_str" => {
            exactly(&args, 0, "to_str")?;
            // Decoding is the whole point of the boundary, so `bytes` decodes
            // rather than showing its repr: strict UTF-8, no replacement
            // characters, because silently corrupting a body is worse than
            // refusing it. CPython raises `UnicodeDecodeError`, a `ValueError`
            // subclass, so `except ValueError` catches this the same way.
            if let Value::Bytes(b) = recv {
                return match String::from_utf8((**b).clone()) {
                    Ok(s) => Ok(Value::str(s)),
                    Err(e) => {
                        let pos = e.utf8_error().valid_up_to();
                        Err(format!(
                            "bytes could not be decoded as UTF-8: invalid byte 0x{:02x} at \
                             position {pos}",
                            b[pos]
                        ))
                    }
                };
            }
            Ok(Value::str(recv.display()))
        }
        "to_bytes" => {
            exactly(&args, 0, "to_bytes")?;
            match recv {
                // UTF-8 is the one encoding, so this cannot fail and takes no
                // `encoding=` argument. Others are a library, written in Oro.
                Value::Str(s) => Ok(Value::bytes(s.s.as_bytes())),
                Value::Bytes(_) => Ok(recv.clone()),
                other => Err(format!(
                    "'{}' object has no conversion to bytes",
                    other.type_name()
                )),
            }
        }
        "to_bool" => {
            exactly(&args, 0, "to_bool")?;
            Ok(Value::Bool(recv.truthy()))
        }
        "to_float" => {
            exactly(&args, 0, "to_float")?;
            match recv {
                Value::Bool(b) => Ok(Value::Float(*b as i64 as f64)),
                Value::Int(i) => Ok(Value::Float(*i as f64)),
                Value::Big(b) => Ok(Value::Float(b.to_f64())),
                Value::Float(_) => Ok(recv.clone()),
                Value::Str(s) => s
                    .s
                    .trim()
                    .parse::<f64>()
                    .map(Value::Float)
                    .map_err(|_| format!("could not convert string to float: '{}'", s.s)),
                other => Err(format!(
                    "'{}' object has no conversion to float",
                    other.type_name()
                )),
            }
        }
        "to_int" => {
            // `to_int(base)` for strings, mirroring CPython's int(s, base).
            let base = opt_int_arg(&args, 0, "to_int", 10)?;
            match recv {
                Value::Bool(b) => Ok(Value::Int(*b as i64)),
                Value::Int(_) | Value::Big(_) => Ok(recv.clone()),
                Value::Float(f) => Ok(float_to_int(*f)),
                Value::Str(s) => {
                    if base == 10 && args.is_empty() {
                        parse_int_str(&s.s)
                    } else {
                        parse_int_base(&s.s, base)
                    }
                }
                other => Err(format!(
                    "'{}' object has no conversion to int",
                    other.type_name()
                )),
            }
        }
        "to_list" => {
            exactly(&args, 0, "to_list")?;
            let items = crate::vm::iterate_to_vec(recv)?;
            Ok(Value::List(Rc::new(RefCell::new(items))))
        }
        "to_dict" => {
            exactly(&args, 0, "to_dict")?;
            let mut d = OroDict::new();
            if let Value::Dict(src) = recv {
                for (k, v) in src.borrow().items() {
                    d.insert(k.clone(), v.clone())?;
                }
                return Ok(Value::Dict(Rc::new(RefCell::new(d))));
            }
            for pair in crate::vm::iterate_to_vec(recv)? {
                let parts = match &pair {
                    Value::Tuple(t) => t.as_slice().to_vec(),
                    Value::List(l) => l.borrow().clone(),
                    other => {
                        return Err(format!(
                            "to_dict() requires (key, value) pairs, found '{}'",
                            other.type_name()
                        ))
                    }
                };
                if parts.len() != 2 {
                    return Err(format!(
                        "to_dict() requires 2-element pairs, found one of length {}",
                        parts.len()
                    ));
                }
                d.insert(parts[0].clone(), parts[1].clone())?;
            }
            Ok(Value::Dict(Rc::new(RefCell::new(d))))
        }
        _ => unreachable!("not a cast method"),
    }
}

/// Collection methods that need no callback, so they can run natively. The
/// callback-taking half (`map`, `filter`, `reduce`, `group_by`, …) is driven by
/// the VM instead, since those run Oro code per element.
pub fn is_seq_native(name: &str) -> bool {
    matches!(
        name,
        "sum" | "min" | "max" | "unique" | "take" | "drop" | "first" | "last"
            | "flatten" | "chunk" | "zip" | "join" | "reversed" | "sorted" | "enumerate" | "len"
    )
}

/// The elements of a collection, plus how to put one back together. A dict's
/// elements are its `(key, value)` pairs, so selecting and reordering a dict
/// gives back a dict.
fn seq_parts(recv: &Value, who: &str) -> VResult<(Shape, Vec<Value>)> {
    Ok(match recv {
        Value::List(l) => (Shape::List, l.borrow().clone()),
        Value::Tuple(t) => (Shape::Tuple, t.as_slice().to_vec()),
        Value::Dict(d) => (
            Shape::Dict,
            d.borrow()
                .items()
                .iter()
                .map(|(k, v)| Value::Tuple(Rc::new(vec![k.clone(), v.clone()])))
                .collect(),
        ),
        Value::Range(_) => (Shape::List, crate::vm::iterate_to_vec(recv)?),
        other => {
            return Err(format!(
                "'{}' object has no method '{who}'",
                other.type_name()
            ))
        }
    })
}

#[derive(Clone, Copy, PartialEq)]
enum Shape {
    List,
    Tuple,
    Dict,
}

fn rebuild(shape: Shape, items: Vec<Value>) -> VResult<Value> {
    Ok(match shape {
        Shape::List => Value::List(Rc::new(RefCell::new(items))),
        Shape::Tuple => Value::Tuple(Rc::new(items)),
        Shape::Dict => {
            let mut d = OroDict::new();
            for entry in items {
                let pair = match &entry {
                    Value::Tuple(t) => t.as_slice().to_vec(),
                    Value::List(l) => l.borrow().clone(),
                    other => {
                        return Err(format!(
                            "rebuilding a dict needs (key, value) pairs, not '{}'",
                            other.type_name()
                        ))
                    }
                };
                if pair.len() != 2 {
                    return Err(format!(
                        "rebuilding a dict needs 2-element pairs, got {}",
                        pair.len()
                    ));
                }
                d.insert(pair[0].clone(), pair[1].clone())?;
            }
            Value::Dict(Rc::new(RefCell::new(d)))
        }
    })
}

fn seq_native_method(recv: &Value, name: &str, args: Vec<Value>) -> VResult<Value> {
    let (shape, items) = seq_parts(recv, name)?;
    match name {
        "len" => {
            exactly(&args, 0, "len")?;
            Ok(Value::Int(items.len() as i64))
        }
        "first" | "last" => {
            exactly(&args, 0, name)?;
            let pick = if name == "first" { items.first() } else { items.last() };
            match pick {
                Some(v) => Ok(v.clone()),
                None => Err(format!("{name}() on an empty sequence")),
            }
        }
        "sum" => {
            exactly(&args, 0, "sum")?;
            bi_sum(vec![rebuild(Shape::List, items)?])
        }
        "min" | "max" => {
            exactly(&args, 0, name)?;
            if items.is_empty() {
                return Err(format!("{name}() arg is an empty sequence"));
            }
            let mut best = items[0].clone();
            for v in &items[1..] {
                let ord = v.compare(&best)?;
                let take = if name == "min" {
                    ord == std::cmp::Ordering::Less
                } else {
                    ord == std::cmp::Ordering::Greater
                };
                if take {
                    best = v.clone();
                }
            }
            Ok(best)
        }
        "sorted" => {
            exactly(&args, 0, "sorted")?;
            let mut out = items;
            sort_values(&mut out)?;
            rebuild(shape, out)
        }
        "reversed" => {
            exactly(&args, 0, "reversed")?;
            let mut out = items;
            out.reverse();
            rebuild(shape, out)
        }
        "unique" => {
            exactly(&args, 0, "unique")?;
            let mut seen = OroDict::new();
            let mut out = Vec::new();
            for v in items {
                if !seen.contains(&v)? {
                    seen.insert(v.clone(), Value::Bool(true))?;
                    out.push(v);
                }
            }
            rebuild(shape, out)
        }
        "take" | "drop" => {
            let n = opt_int_arg(&args, 0, name, -1)?;
            if n < 0 {
                return Err(format!("{name}() needs a count >= 0"));
            }
            let n = (n as usize).min(items.len());
            let out = if name == "take" {
                items[..n].to_vec()
            } else {
                items[n..].to_vec()
            };
            rebuild(shape, out)
        }
        "flatten" => {
            exactly(&args, 0, "flatten")?;
            let mut out = Vec::new();
            for v in &items {
                out.extend(crate::vm::iterate_to_vec(v)?);
            }
            Ok(Value::List(Rc::new(RefCell::new(out))))
        }
        "chunk" => {
            let n = opt_int_arg(&args, 0, "chunk", 0)?;
            if n <= 0 {
                return Err("chunk() needs a size >= 1".to_string());
            }
            let out: Vec<Value> = items
                .chunks(n as usize)
                .map(|c| Value::List(Rc::new(RefCell::new(c.to_vec()))))
                .collect();
            Ok(Value::List(Rc::new(RefCell::new(out))))
        }
        "zip" => {
            let other = crate::vm::iterate_to_vec(
                args.first().ok_or("zip() missing its second sequence")?,
            )?;
            let out: Vec<Value> = items
                .iter()
                .zip(other.iter())
                .map(|(a, b)| Value::Tuple(Rc::new(vec![a.clone(), b.clone()])))
                .collect();
            Ok(Value::List(Rc::new(RefCell::new(out))))
        }
        "enumerate" => {
            let start = opt_int_arg(&args, 0, "enumerate", 0)?;
            let out: Vec<Value> = items
                .into_iter()
                .enumerate()
                .map(|(i, v)| Value::Tuple(Rc::new(vec![Value::Int(start + i as i64), v])))
                .collect();
            Ok(Value::List(Rc::new(RefCell::new(out))))
        }
        "join" => {
            // `xs.join(", ")` rather than `", ".join(xs)`: the separator is the
            // detail, the sequence is the subject, and this way it ends a chain
            // instead of forcing the reader back to the front of the line.
            // The separator's type decides the result's, and the elements
            // must match it: `join` never guesses a conversion, the same rule
            // `json.stringify` applies to dict keys.
            if let Some(Value::Bytes(sep)) = args.first() {
                let mut out: Vec<u8> = Vec::new();
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        out.extend_from_slice(sep);
                    }
                    match it {
                        Value::Bytes(p) => out.extend_from_slice(p),
                        other => {
                            return Err(format!(
                                "join() requires bytes elements, found '{}'",
                                other.type_name()
                            ))
                        }
                    }
                }
                return Ok(Value::bytes(out));
            }
            let sep = str_arg(&args, 0, "join")?;
            let mut pieces = Vec::with_capacity(items.len());
            for it in items {
                match it {
                    Value::Str(p) => pieces.push(p.s.clone()),
                    other => {
                        return Err(format!(
                            "join() requires str elements, found '{}'",
                            other.type_name()
                        ))
                    }
                }
            }
            Ok(Value::str(pieces.join(&sep)))
        }
        _ => unreachable!("not a native sequence method"),
    }
}

fn str_method(recv: &Value, name: &str, args: Vec<Value>) -> VResult<Value> {
    let s = match recv {
        Value::Str(s) => s.s.clone(),
        _ => unreachable!(),
    };
    match name {
        "upper" => Ok(Value::str(s.to_uppercase())),
        "lower" => Ok(Value::str(s.to_lowercase())),
        "strip" => Ok(Value::str(s.trim().to_string())),
        "lstrip" => Ok(Value::str(s.trim_start().to_string())),
        "rstrip" => Ok(Value::str(s.trim_end().to_string())),
        "startswith" => Ok(Value::Bool(s.starts_with(&str_arg(&args, 0, "startswith")?))),
        "endswith" => Ok(Value::Bool(s.ends_with(&str_arg(&args, 0, "endswith")?))),
        "find" => {
            let needle = str_arg(&args, 0, "find")?;
            match s.find(&needle) {
                Some(byte) => Ok(Value::Int(s[..byte].chars().count() as i64)),
                None => Ok(Value::Int(-1)),
            }
        }
        "replace" => {
            let from = str_arg(&args, 0, "replace")?;
            let to = str_arg(&args, 1, "replace")?;
            Ok(Value::str(s.replace(&from, &to)))
        }
        "split" | "rsplit" => {
            // maxsplit < 0 (the default) means "no limit"; maxsplit == n caps the
            // number of *splits*, so at most n + 1 pieces come back.
            let maxsplit = opt_int_arg(&args, 1, name, -1)?;
            let from_right = name == "rsplit";
            let parts: Vec<String> = match args.first() {
                None | Some(Value::None) => split_whitespace_n(&s, maxsplit, from_right),
                Some(Value::Str(sep)) => {
                    if sep.s.is_empty() {
                        return Err("empty separator".to_string());
                    }
                    split_sep_n(&s, &sep.s, maxsplit, from_right)
                }
                Some(other) => {
                    return Err(format!(
                        "{name}() separator must be str, not '{}'",
                        other.type_name()
                    ))
                }
            };
            let parts = parts.into_iter().map(Value::str).collect::<Vec<_>>();
            Ok(Value::List(Rc::new(RefCell::new(parts))))
        }
        "zfill" => {
            let width = opt_int_arg(&args, 0, "zfill", 0)?;
            let len = s.chars().count() as i64;
            if len >= width {
                return Ok(Value::str(s));
            }
            let pad = "0".repeat((width - len) as usize);
            // A leading sign stays in front of the padding: "-7".zfill(4) -> "-007".
            let mut it = s.chars();
            match it.next() {
                Some(c) if c == '-' || c == '+' => {
                    Ok(Value::str(format!("{c}{pad}{}", it.as_str())))
                }
                _ => Ok(Value::str(format!("{pad}{s}"))),
            }
        }
        "join" => {
            let items = crate::vm::iterate_to_vec(args.first().ok_or("join() missing argument")?)?;
            let mut pieces = Vec::with_capacity(items.len());
            for it in items {
                match it {
                    Value::Str(p) => pieces.push(p.s.clone()),
                    other => {
                        return Err(format!(
                            "join() requires str elements, found '{}'",
                            other.type_name()
                        ))
                    }
                }
            }
            Ok(Value::str(pieces.join(&s)))
        }
        _ => Err(format!("'str' object has no method '{name}'")),
    }
}

fn bytes_arg(args: &[Value], i: usize, who: &str) -> VResult<Rc<Vec<u8>>> {
    match args.get(i) {
        Some(Value::Bytes(b)) => Ok(b.clone()),
        Some(other) => Err(format!("{who}() argument must be bytes, not '{}'", other.type_name())),
        None => Err(format!("{who}() missing a required argument")),
    }
}

/// The whitespace `bytes` recognises: space, tab, newline, vertical tab, form
/// feed and carriage return. Rust's `is_ascii_whitespace` leaves out the
/// vertical tab; CPython includes it, and this is oracle-checked.
fn is_bytes_space(x: u8) -> bool {
    matches!(x, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// The offset of `needle` in `hay`, or `None`. The empty needle is at 0, as it
/// is for `str`.
fn bytes_find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// `bytes.replace`. An empty `from` inserts `to` between every pair of octets
/// and at both ends, which is what `str.replace` does with an empty pattern.
fn bytes_replace(hay: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(hay.len());
    if from.is_empty() {
        for &x in hay {
            out.extend_from_slice(to);
            out.push(x);
        }
        out.extend_from_slice(to);
        return out;
    }
    let mut i = 0;
    while i < hay.len() {
        if hay[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    out
}

/// `bytes.split`/`rsplit` on an explicit separator — the byte-level twin of
/// [`split_sep_n`], with the same `maxsplit` rule (negative = unlimited).
fn split_sep_bytes(s: &[u8], sep: &[u8], maxsplit: i64, from_right: bool) -> Vec<Vec<u8>> {
    let limit = if maxsplit < 0 { usize::MAX } else { maxsplit as usize };
    let mut parts: Vec<Vec<u8>> = Vec::new();
    if !from_right {
        let (mut start, mut i) = (0usize, 0usize);
        while parts.len() < limit && i + sep.len() <= s.len() {
            if s[i..i + sep.len()] == *sep {
                parts.push(s[start..i].to_vec());
                i += sep.len();
                start = i;
            } else {
                i += 1;
            }
        }
        parts.push(s[start..].to_vec());
    } else {
        let (mut end, mut i) = (s.len(), s.len());
        while parts.len() < limit && i >= sep.len() {
            let j = i - sep.len();
            if s[j..i] == *sep {
                parts.push(s[i..end].to_vec());
                end = j;
                i = j;
            } else {
                i -= 1;
            }
        }
        parts.push(s[..end].to_vec());
        parts.reverse();
    }
    parts
}

/// `bytes.split(None, maxsplit)` — the byte-level twin of
/// [`split_whitespace_n`]: runs of whitespace separate, leading and trailing
/// whitespace is discarded, and the remainder after `maxsplit` splits comes
/// back verbatim.
fn split_space_bytes(s: &[u8], maxsplit: i64, from_right: bool) -> Vec<Vec<u8>> {
    let limit = if maxsplit < 0 { usize::MAX } else { maxsplit as usize };
    let mut parts: Vec<Vec<u8>> = Vec::new();
    if !from_right {
        let mut i = 0usize;
        while parts.len() < limit {
            while i < s.len() && is_bytes_space(s[i]) {
                i += 1;
            }
            if i >= s.len() {
                return parts;
            }
            let start = i;
            while i < s.len() && !is_bytes_space(s[i]) {
                i += 1;
            }
            parts.push(s[start..i].to_vec());
        }
        while i < s.len() && is_bytes_space(s[i]) {
            i += 1;
        }
        if i < s.len() {
            parts.push(s[i..].to_vec());
        }
        parts
    } else {
        let mut i = s.len();
        while parts.len() < limit {
            while i > 0 && is_bytes_space(s[i - 1]) {
                i -= 1;
            }
            if i == 0 {
                parts.reverse();
                return parts;
            }
            let end = i;
            while i > 0 && !is_bytes_space(s[i - 1]) {
                i -= 1;
            }
            parts.push(s[i..end].to_vec());
        }
        while i > 0 && is_bytes_space(s[i - 1]) {
            i -= 1;
        }
        if i > 0 {
            parts.push(s[..i].to_vec());
        }
        parts.reverse();
        parts
    }
}

/// The `bytes` methods. Deliberately the same names `str` carries — every one
/// exists on CPython's `bytes` with the same meaning, so the oracle covers the
/// whole set — plus `hex`, which has no `str` counterpart. Case folding is
/// ASCII-only: an octet is not a character, and there is no encoding here to
/// case a non-ASCII one under.
fn bytes_method(b: &Rc<Vec<u8>>, name: &str, args: Vec<Value>) -> VResult<Value> {
    match name {
        "upper" => Ok(Value::bytes(b.to_ascii_uppercase())),
        "lower" => Ok(Value::bytes(b.to_ascii_lowercase())),
        "strip" => {
            let start = b.iter().take_while(|&&x| is_bytes_space(x)).count();
            let end = b.iter().rev().take_while(|&&x| is_bytes_space(x)).count();
            Ok(Value::bytes(if start + end >= b.len() {
                Vec::new()
            } else {
                b[start..b.len() - end].to_vec()
            }))
        }
        "lstrip" => {
            let start = b.iter().take_while(|&&x| is_bytes_space(x)).count();
            Ok(Value::bytes(b[start..].to_vec()))
        }
        "rstrip" => {
            let end = b.iter().rev().take_while(|&&x| is_bytes_space(x)).count();
            Ok(Value::bytes(b[..b.len() - end].to_vec()))
        }
        "startswith" => Ok(Value::Bool(b.starts_with(&bytes_arg(&args, 0, "startswith")?))),
        "endswith" => Ok(Value::Bool(b.ends_with(&bytes_arg(&args, 0, "endswith")?))),
        "find" => {
            let needle = bytes_arg(&args, 0, "find")?;
            Ok(Value::Int(match bytes_find(b, &needle) {
                Some(i) => i as i64,
                None => -1,
            }))
        }
        "replace" => {
            let from = bytes_arg(&args, 0, "replace")?;
            let to = bytes_arg(&args, 1, "replace")?;
            Ok(Value::bytes(bytes_replace(b, &from, &to)))
        }
        "split" | "rsplit" => {
            let maxsplit = opt_int_arg(&args, 1, name, -1)?;
            let from_right = name == "rsplit";
            let parts: Vec<Vec<u8>> = match args.first() {
                None | Some(Value::None) => split_space_bytes(b, maxsplit, from_right),
                Some(Value::Bytes(sep)) => {
                    if sep.is_empty() {
                        return Err("empty separator".to_string());
                    }
                    split_sep_bytes(b, sep, maxsplit, from_right)
                }
                Some(other) => {
                    return Err(format!(
                        "{name}() separator must be bytes, not '{}'",
                        other.type_name()
                    ))
                }
            };
            let parts = parts.into_iter().map(Value::bytes).collect::<Vec<_>>();
            Ok(Value::List(Rc::new(RefCell::new(parts))))
        }
        "zfill" => {
            let width = opt_int_arg(&args, 0, "zfill", 0)?;
            let len = b.len() as i64;
            if len >= width {
                return Ok(Value::Bytes(b.clone()));
            }
            let pad = (width - len) as usize;
            let mut out = Vec::with_capacity(width as usize);
            // A leading sign stays in front of the padding: b"-7".zfill(4) is
            // b"-007".
            let rest = match b.first() {
                Some(&c) if c == b'-' || c == b'+' => {
                    out.push(c);
                    &b[1..]
                }
                _ => &b[..],
            };
            out.extend(std::iter::repeat_n(b'0', pad));
            out.extend_from_slice(rest);
            Ok(Value::bytes(out))
        }
        "join" => {
            let items = crate::vm::iterate_to_vec(args.first().ok_or("join() missing argument")?)?;
            let mut out: Vec<u8> = Vec::new();
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    out.extend_from_slice(b);
                }
                match it {
                    Value::Bytes(p) => out.extend_from_slice(p),
                    other => {
                        return Err(format!(
                            "join() requires bytes elements, found '{}'",
                            other.type_name()
                        ))
                    }
                }
            }
            Ok(Value::bytes(out))
        }
        "hex" => {
            exactly(&args, 0, "hex")?;
            let mut out = String::with_capacity(b.len() * 2);
            for &x in b.iter() {
                let _ = write!(out, "{x:02x}");
            }
            Ok(Value::str(out))
        }
        _ => Err(format!("'bytes' object has no method '{name}'")),
    }
}

fn list_method(l: &Rc<RefCell<Vec<Value>>>, name: &str, args: Vec<Value>) -> VResult<Value> {
    match name {
        "append" => {
            exactly(&args, 1, "append")?;
            l.borrow_mut().push(args.into_iter().next().unwrap());
            Ok(Value::None)
        }
        "extend" => {
            exactly(&args, 1, "extend")?;
            let items = crate::vm::iterate_to_vec(&args[0])?;
            l.borrow_mut().extend(items);
            Ok(Value::None)
        }
        "pop" => {
            let mut b = l.borrow_mut();
            let idx = match args.as_slice() {
                [] => {
                    if b.is_empty() {
                        return Err("pop from empty list".to_string());
                    }
                    b.len() - 1
                }
                [v] => {
                    let i = as_i64(v)?;
                    let adj = if i < 0 { i + b.len() as i64 } else { i };
                    if adj < 0 || adj as usize >= b.len() {
                        return Err("pop index out of range".to_string());
                    }
                    adj as usize
                }
                _ => return Err("pop() takes at most 1 argument".to_string()),
            };
            Ok(b.remove(idx))
        }
        "sort" => {
            exactly(&args, 0, "sort")?;
            // Sort a temporary snapshot so the list is never observed in a
            // half-ordered state (and to avoid holding the borrow across the
            // fallible comparisons).
            let mut items = l.borrow().clone();
            sort_values(&mut items)?;
            *l.borrow_mut() = items;
            Ok(Value::None)
        }
        "reverse" => {
            exactly(&args, 0, "reverse")?;
            l.borrow_mut().reverse();
            Ok(Value::None)
        }
        _ => Err(format!("'list' object has no method '{name}'")),
    }
}

fn dict_method(d: &Rc<RefCell<OroDict>>, name: &str, args: Vec<Value>) -> VResult<Value> {
    match name {
        "get" => {
            let (key, default) = match args.as_slice() {
                [k] => (k, Value::None),
                [k, def] => (k, def.clone()),
                _ => return Err("get() takes 1 or 2 arguments".to_string()),
            };
            Ok(d.borrow().get(key)?.unwrap_or(default))
        }
        "keys" => Ok(Value::List(Rc::new(RefCell::new(d.borrow().keys())))),
        "values" => Ok(Value::List(Rc::new(RefCell::new(d.borrow().values())))),
        "items" => {
            let items: Vec<Value> = d
                .borrow()
                .items()
                .iter()
                .map(|(k, v)| Value::Tuple(Rc::new(vec![k.clone(), v.clone()])))
                .collect();
            Ok(Value::List(Rc::new(RefCell::new(items))))
        }
        _ => Err(format!("'dict' object has no method '{name}'")),
    }
}

