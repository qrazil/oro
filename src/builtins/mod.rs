//! Builtin functions and type methods.
//!
//! Builtins are plain `fn(Vec<Value>) -> VResult<Value>` pointers wrapped in a
//! [`Value::Builtin`]; the VM invokes them natively and, crucially, none of them
//! ever calls back into Oro, so the single flat interpreter loop is never
//! re-entered. Type methods (string and a few container helpers) are dispatched
//! by name through [`call_method`].

pub mod pct;

use std::cell::RefCell;
use std::fmt::Write as _;
use std::rc::Rc;

use crate::bigint::BigInt;
use crate::exc::{
    attribute_error, index_error, key_error, runtime_error, type_error, value_error, VErr,
};
use crate::value::{
    Builtin, OroDict, OroList, OroStr, OroTuple, RangeVal, TypeTag, VResult, Value,
};

/// The body of a builtin the VM dispatches itself. Unreachable through the
/// interpreter, which checks the name first; it exists so every global has the
/// same `Value::Builtin` shape.
fn bi_vm_dispatched(_args: Vec<Value>) -> VResult<Value> {
    Err(runtime_error("internal: this builtin is dispatched by the VM"))
}

/// Look up a global name. Oro's only globals are the builtins.
pub fn lookup(name: &str) -> Option<Value> {
    let f: fn(Vec<Value>) -> VResult<Value> = match name {
        "print" => bi_print,
        "len" => bi_len,
        "type" => bi_type,
        "abs" => bi_abs,
        "min" => bi_min,
        "max" => bi_max,
        "sum" => bi_sum,
        "sorted" => bi_sorted,
        "repr" => bi_repr,
        "open" => bi_open,
        "set" => bi_set,
        "enumerate" => bi_enumerate,
        "zip" => bi_zip,
        "any" => bi_any,
        "all" => bi_all,
        "round" => bi_round,
        "chr" => bi_chr,
        "ord" => bi_ord,
        // The three concurrency builtins. They are named here so `LoadGlobal`
        // resolves them like any other global, but none of them ever runs as a
        // native function: the VM intercepts all three by name in `invoke`,
        // because `spawn` builds a stack segment the VM owns, `chan` has to be
        // dispatched alongside it, and `yield_now` can only be answered with a
        // `Step`. See `crate::vm::sched`.
        "spawn" => bi_vm_dispatched,
        "chan" => bi_vm_dispatched,
        "yield_now" => bi_vm_dispatched,
        _ => return None,
    };
    Some(Value::Builtin(Rc::new(Builtin { name: intern(name), func: f })))
}

/// Map a builtin name to its `'static` spelling for the [`Builtin`] struct.
fn intern(name: &str) -> &'static str {
    match name {
        "print" => "print",
        "len" => "len",
        "type" => "type",
        "abs" => "abs",
        "min" => "min",
        "max" => "max",
        "sum" => "sum",
        "sorted" => "sorted",
        "repr" => "repr",
        "open" => "open",
        "set" => "set",
        "enumerate" => "enumerate",
        "zip" => "zip",
        "any" => "any",
        "all" => "all",
        "round" => "round",
        "chr" => "chr",
        "ord" => "ord",
        "spawn" => "spawn",
        "chan" => "chan",
        "yield_now" => "yield_now",
        _ => "builtin",
    }
}

// --- Argument helpers -------------------------------------------------------

pub(crate) fn exactly(args: &[Value], n: usize, who: &str) -> VResult<()> {
    if args.len() != n {
        Err(type_error(format!("{who}() takes {n} argument(s) but {} were given", args.len())))
    } else {
        Ok(())
    }
}

/// Reject arguments past the `n`th. A method that accepts an argument it then
/// ignores answers confidently and wrongly, which is worse than refusing.
fn at_most(args: &[Value], n: usize, who: &str) -> VResult<()> {
    if args.len() > n {
        Err(type_error(format!(
            "{who}() takes at most {n} argument(s) but {} were given",
            args.len()
        )))
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
        other => return Err(type_error(format!(
            "object of type '{}' has no len()",
            other.type_name()
        ))),
    };
    Ok(Value::Int(n as i64))
}

fn bi_repr(args: Vec<Value>) -> VResult<Value> {
    exactly(&args, 1, "repr")?;
    Ok(Value::str(args[0].repr()))
}

fn bi_set(_args: Vec<Value>) -> VResult<Value> {
    Err(runtime_error("set() is not supported in Oro — sets are cut. Use a dict for membership \
         (`{k: True}`, then `k in d`), or dedup with a loop that skips keys already in a dict; \
         a Set data structure may return in the stdlib."
        ))
}

fn bi_open(args: Vec<Value>) -> VResult<Value> {
    use crate::stream::OroStream;
    let (path, mode) = match args.as_slice() {
        [Value::Str(p)] => (p.s.clone(), "r".to_string()),
        [Value::Str(p), Value::Str(m)] => (p.s.clone(), m.s.clone()),
        [_] | [_, _] => return Err(type_error("open() arguments must be strings")),
        _ => return Err(type_error("open() takes 1 or 2 arguments")),
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
            return Err(value_error(format!(
                "invalid file mode '{mode}' — open() has no 'b' suffix because there is no text \
                 mode to contrast with: every stream in Oro is bytes. Use '{}'.",
                &mode[..1]
            )))
        }
        other => return Err(value_error(format!(
            "invalid file mode '{other}' (use 'r', 'w', or 'a')"
        ))),
    };
    Ok(Value::Stream(Rc::new(stream)))
}

fn bi_type(args: Vec<Value>) -> VResult<Value> {
    exactly(&args, 1, "type")?;
    match &args[0] {
        // The type of a user instance is its class object.
        Value::Instance(i) => Ok(Value::Class(i.class.clone())),
        // ...and of anything else, the tag the type keyword denotes — the same
        // value, so `type(x) == str` is true for the same reason
        // `type(p) == Point` is.
        other => Ok(Value::Type(other.type_tag())),
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
        other => Err(type_error(format!("bad operand type for abs(): '{}'", other.type_name()))),
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
        0 => return Err(value_error(format!("{who}() expected at least 1 argument"))),
        1 => crate::vm::iterate_to_vec(&args[0])?,
        _ => args,
    };
    let mut it = items.into_iter();
    let mut best =
        it.next().ok_or_else(|| value_error(format!("{who}() arg is an empty sequence")))?;
    for v in it {
        if ord_or_defer(&v, &best, if want == std::cmp::Ordering::Less { "<" } else { ">" })?
            == want
        {
            best = v;
        }
    }
    Ok(best)
}

fn bi_sum(args: Vec<Value>) -> VResult<Value> {
    let (iterable, start) = match args.as_slice() {
        [it] => (it, Value::Int(0)),
        [it, start] => (it, start.clone()),
        _ => return Err(type_error("sum() takes 1 or 2 arguments")),
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
        _ => return Err(type_error("sorted() takes exactly 1 argument")),
    };
    let shape = sorted_shape_of(iterable);
    let mut items = crate::vm::iterate_to_vec(iterable)?;
    sort_values(&mut items)?;
    rebuild(
        match shape {
            SortedShape::List => Shape::List,
            SortedShape::Tuple => Shape::Tuple,
            SortedShape::Dict => Shape::Dict,
        },
        items,
    )
}

/// The shape `sorted(x)` rebuilds — for the native path and, through
/// [`crate::vm::sorted_shape`], for the frame-driven ones (`key=`, `reverse=`,
/// a user `__lt__`) too, so the two cannot answer different types for one call.
#[derive(Clone, Copy)]
pub enum SortedShape {
    List,
    Tuple,
    Dict,
}

/// Sorting *reorders*, and the collection protocol's stated rule is that an
/// operation which selects or reorders preserves the receiver's type. `.sorted()`
/// has always obeyed it; the builtin hardcoded a list, so `sorted((3, 1, 2))`
/// and `(3, 1, 2).sorted()` answered different types for the same word. The
/// method is the spelling that survives the builtin/method line (a builtin takes
/// scalars, a collection method takes a collection), so the builtin moves.
///
/// A tuple and a dict are the two shapes there are to keep. A list is already a
/// list, and a range, a generator, a str and a bytes have no literal to rebuild.
/// A dict is here because iterating one yields its `(key, value)` pairs: what
/// `sorted` reordered *is* a sequence of entries, so a dict is what it rebuilds
/// into, exactly as `d.sorted()` has always answered. While a dict was walked as
/// its keys there was no dict to rebuild and a list was the honest answer; that
/// stopped being true when the iterator changed.
pub fn sorted_shape_of(v: &Value) -> SortedShape {
    match v {
        Value::Tuple(_) => SortedShape::Tuple,
        Value::Dict(_) => SortedShape::Dict,
        _ => SortedShape::List,
    }
}

/// The error a native ordering answers with when it meets an operand whose
/// ordering only a user `__lt__` can decide.
///
/// It is a backstop, not a path: the VM checks every ordering entry point
/// (`sorted`, `sort`, `min`, `max`, and their chain spellings) for such an
/// operand *before* calling native code, and runs the resumable comparison
/// itself instead. Seeing this message means an entry point was missed — which
/// is worth a loud internal error, because the alternative for a `__lt__` that
/// native code cannot call is ordering by address.
pub const ORD_NEEDS_VM: &str = "internal: ordering needs the VM (unrouted __lt__)";

/// [`Value::try_compare`] with "the VM must decide" turned into the backstop
/// error above, for the native orderings that have no way to suspend.
pub fn ord_or_defer(a: &Value, b: &Value, sym: &'static str) -> VResult<std::cmp::Ordering> {
    a.try_compare(b, sym)?.ok_or_else(|| runtime_error(ORD_NEEDS_VM))
}

/// Stable sort of `items` by the matching entry in `keys` (the classic
/// decorate-sort-undecorate). `reverse` inverts the *comparator* rather than
/// reversing the result: `sort_by` is stable, so equal keys keep their original
/// order in both directions — which is what CPython guarantees. Reversing the
/// sorted output instead would flip ties and break that.
pub fn sort_by_keys(items: Vec<Value>, keys: &[Value], reverse: bool) -> VResult<Vec<Value>> {
    let mut idx: Vec<usize> = (0..items.len()).collect();
    let mut err: Option<VErr> = None;
    idx.sort_by(|&a, &b| {
        if err.is_some() {
            return std::cmp::Ordering::Equal;
        }
        let (lhs, rhs) = if reverse { (b, a) } else { (a, b) };
        match ord_or_defer(&keys[lhs], &keys[rhs], "<") {
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
    let mut err: Option<VErr> = None;
    items.sort_by(|a, b| {
        if err.is_some() {
            return std::cmp::Ordering::Equal;
        }
        match ord_or_defer(a, b, "<") {
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
        other => Err(type_error(format!("expected an integer, got '{}'", other.type_name()))),
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
        None => Err(value_error(format!("invalid literal for int(): '{s}'"))),
    }
}

// --- Methods ----------------------------------------------------------------

/// Whether `name` is a valid method of `recv`'s type (drives attribute access).
/// `int(s, base)`. Accepts an optional sign, the `0x`/`0o`/`0b` prefix when it
/// agrees with `base`, and underscore separators — matching CPython.
fn parse_int_base(s: &str, base: i64) -> VResult<Value> {
    if base != 0 && !(2..=36).contains(&base) {
        return Err(value_error("int() base must be >= 2 and <= 36, or 0"));
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
        return Err(value_error(format!("invalid literal for int() with base {base}: '{s}'")));
    }
    match i64::from_str_radix(&cleaned, base as u32) {
        Ok(n) => Ok(Value::Int(if neg { -n } else { n })),
        Err(_) => Err(value_error(format!("invalid literal for int() with base {base}: '{s}'"))),
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
fn type_name_is_not_callable(who: &str, method: &str, literal: &str) -> VErr {
    type_error(format!(
        "{who}() is not callable in Oro — write `x.{method}()` to convert a value, or the \
         literal `{literal}` to build an empty one."
    ))
}

/// Calling a type keyword: `range(3)`, and the refusals.
///
/// `range` is the one builtin type with a constructor, because a range has no
/// literal syntax to build it with. Every other type does — `""`, `0`, `[]`,
/// `{}` — or is a handle something else hands you, so a constructor would be a
/// second way to write a thing that already has one. The conversions are
/// methods (`x.to_int()`), which is where a conversion belongs: it reads left
/// to right and it is one spelling, not two.
pub fn call_type(t: TypeTag, args: Vec<Value>) -> VResult<Value> {
    match t {
        TypeTag::Range => {
            let ints: Vec<i64> = args.iter().map(as_i64).collect::<VResult<_>>()?;
            let (start, stop, step) = match ints.as_slice() {
                [stop] => (0, *stop, 1),
                [start, stop] => (*start, *stop, 1),
                [start, stop, step] => (*start, *stop, *step),
                _ => return Err(type_error("range() takes 1 to 3 integer arguments")),
            };
            if step == 0 {
                return Err(value_error("range() step argument must not be zero"));
            }
            Ok(Value::Range(Rc::new(RangeVal { start, stop, step })))
        }
        TypeTag::Str => Err(type_name_is_not_callable("str", "to_str", "\"\"")),
        TypeTag::Bytes => Err(type_name_is_not_callable("bytes", "to_bytes", "b\"\"")),
        TypeTag::Int => Err(type_name_is_not_callable("int", "to_int", "0")),
        TypeTag::Float => Err(type_name_is_not_callable("float", "to_float", "0.0")),
        TypeTag::Bool => Err(type_name_is_not_callable("bool", "to_bool", "false")),
        TypeTag::List => Err(type_name_is_not_callable("list", "to_list", "[]")),
        TypeTag::Dict => Err(type_name_is_not_callable("dict", "to_dict", "{}")),
        // No `to_tuple` to point at: a tuple is a literal, and the one
        // conversion that would want a constructor (`list` to `tuple`) has no
        // caller in the tree. The message names what does exist.
        TypeTag::Tuple => Err(type_error(
            "tuple() is not callable in Oro — write the literal `()` for an \
             empty tuple, or `(x,)` for a one-element one.",
        )),
        // The handle types. Each is produced by exactly one thing, and naming
        // it is the whole point of the keyword; it is not a constructor.
        other => Err(type_error(format!(
            "{}() is not callable in Oro — it is a type name, which is what `type(x)` \
             answers with, not a constructor.",
            other.name()
        ))),
    }
}

/// `enumerate(it, start=0)`. Eager: returns a list of `(index, value)` tuples
/// rather than a lazy iterator, the same choice `dict.keys()` already makes.
fn bi_enumerate(args: Vec<Value>) -> VResult<Value> {
    let (it, start) = match args.as_slice() {
        [it] => (it, 0),
        [it, s] => (it, as_i64(s)?),
        _ => return Err(type_error("enumerate() takes 1 or 2 arguments")),
    };
    let items = crate::vm::iterate_to_vec(it)?;
    let mut out = Vec::with_capacity(items.len());
    for (i, v) in items.into_iter().enumerate() {
        out.push(Value::Tuple(OroTuple::new(vec![Value::Int(start + i as i64), v])));
    }
    Ok(Value::List(OroList::new(out)))
}

/// `zip(a, b, ...)`, truncating to the shortest input. Eager, like `enumerate`.
fn bi_zip(args: Vec<Value>) -> VResult<Value> {
    let mut cols = Vec::with_capacity(args.len());
    for a in &args {
        cols.push(crate::vm::iterate_to_vec(a)?);
    }
    Ok(zip_cols(cols))
}

/// The one implementation of zip, shared by the builtin and `xs.zip(...)`.
///
/// It lives here, and both spellings route through it, because the two used to
/// have a body each: the method's read `args.first()` and dropped every
/// sequence after it, so `[1, 2].zip([3, 4], [5, 6])` answered a two-way zip
/// with no error while `zip([1, 2], [3, 4], [5, 6])` answered the three-way one.
/// One operation with two bodies is how that happens; one body is how it stops.
fn zip_cols(cols: Vec<Vec<Value>>) -> Value {
    let n = cols.iter().map(|c| c.len()).min().unwrap_or(0);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let row: Vec<Value> = cols.iter().map(|c| c[i].clone()).collect();
        out.push(Value::Tuple(OroTuple::new(row)));
    }
    Value::List(OroList::new(out))
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
        _ => return Err(type_error("round() takes 1 or 2 arguments")),
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
            return Err(type_error(format!(
                "type '{}' doesn't define __round__ method",
                other.type_name()
            )))
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
        .ok_or_else(|| value_error("chr() arg not in range(0x110000)"))?;
    Ok(Value::str(cp.to_string()))
}

fn bi_ord(args: Vec<Value>) -> VResult<Value> {
    exactly(&args, 1, "ord")?;
    let s = match &args[0] {
        Value::Str(s) => s.s.clone(),
        other => {
            return Err(type_error(format!(
                "ord() expected a character, but got '{}'",
                other.type_name()
            )))
        }
    };
    let mut it = s.chars();
    match (it.next(), it.next()) {
        (Some(c), None) => Ok(Value::Int(c as i64)),
        _ => Err(type_error(format!(
            "ord() expected a character, but string of length {} found",
            s.chars().count()
        ))),
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

/// The sixteen names on `str` — and, plus `hex`, on `bytes`. One list, so the
/// two types can never drift apart.
pub fn is_str_method(name: &str) -> bool {
    matches!(
        name,
        "strip"
            | "split"
            | "find"
            | "count"
            | "startswith"
            | "endswith"
            | "rm_prefix"
            | "rm_suffix"
            | "upper"
            | "lower"
            | "join"
            | "replace"
            | "is_digit"
            | "is_alpha"
            | "is_alnum"
            | "is_space"
    )
}

/// A `str`/`bytes` method that used to exist and no longer does. Removals name
/// their replacement — silently answering "no such attribute" would leave the
/// reader to guess what happened to it.
pub fn cut_method_message(recv: &Value, name: &str) -> Option<&'static str> {
    // `.items()` went when iterating a dict started yielding the pair itself:
    // the method's whole job was to undo a choice — iterate keys — that the
    // language no longer makes. It is the one Python habit likely to be typed
    // out of muscle memory, so it is named rather than left to fail as a bare
    // "no such attribute".
    if matches!(recv, Value::Dict(_)) {
        return (name == "items").then_some(
            "`dict.items()` is not in Oro — iterating a dict already yields its (key, value) \
             pairs, so write `for k, v in d`; `d.to_list()` is the list of pairs",
        );
    }
    if !matches!(recv, Value::Str(_) | Value::Bytes(_)) {
        return None;
    }
    Some(match name {
        "lstrip" => "`lstrip` is not in Oro — use `strip(side=\"left\")`",
        "rstrip" => "`rstrip` is not in Oro — use `strip(side=\"right\")`",
        "rsplit" => {
            "`rsplit` is not in Oro — use `split(sep, maxsplit, side=\"right\")`"
        }
        "rfind" => "`rfind` is not in Oro — use `find(sub, reverse=true)`",
        "index" => "`index` is not in Oro — use `find(sub)`, which answers -1 rather than raising",
        "zfill" => {
            "`zfill` is not in Oro — a format spec pads: f\"{n:05d}\", f\"{s:0>5}\", \
             or f\"{s:0>{width}}\" for a width computed at run time"
        }
        "removeprefix" => "`removeprefix` is spelled `rm_prefix` in Oro",
        "removesuffix" => "`removesuffix` is spelled `rm_suffix` in Oro",
        "isdigit" => "`isdigit` is spelled `is_digit` in Oro",
        "isalpha" => "`isalpha` is spelled `is_alpha` in Oro",
        "isalnum" => "`isalnum` is spelled `is_alnum` in Oro",
        "isspace" => "`isspace` is spelled `is_space` in Oro",
        _ => return None,
    })
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
        Value::Str(_) => is_str_method(name),
        // The same sixteen names `str` carries, plus `hex` and `scan`, which
        // only bytes needs.
        Value::Bytes(_) => is_str_method(name) || matches!(name, "hex" | "scan"),
        Value::List(_) => {
            matches!(name, "append" | "pop" | "extend" | "sort" | "reverse" | "map" | "filter")
        }
        // map/filter are type-preserving, so every collection carries them.
        Value::Tuple(_) | Value::Range(_) | Value::Generator(_) => {
            matches!(name, "map" | "filter")
        }
        Value::Dict(_) => {
            matches!(name, "get" | "pop" | "keys" | "values" | "map" | "filter")
        }
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
            // A socket is a Reader and a Writer with the same four methods a
            // file has — that is the io protocol doing its job — plus the
            // three things only a socket can do.
            crate::stream::StreamKind::TcpStream { .. } => matches!(
                name,
                "read"
                    | "write"
                    | "read_until"
                    | "close"
                    | "shutdown_write"
                    | "set_timeout"
                    | "set_nodelay"
            ),
            // A listener is not a stream of bytes: no `read`, no `write`.
            crate::stream::StreamKind::TcpListener { .. } => matches!(name, "accept" | "close"),
        },
        Value::Regex(_) => matches!(
            name,
            "search" | "findall" | "finditer" | "fullmatch" | "sub" | "split"
        ),
        Value::Match(_) => matches!(name, "group" | "start" | "end"),
        _ => false,
    }
}

/// The only native methods that take a keyword: `strip(side=…)`,
/// `split(…, side=…)` and `find(…, reverse=…)`, on `str` and `bytes`.
/// Everything else refuses one, so a misplaced keyword is an error rather than
/// a silently discarded argument.
fn takes_kwargs(recv: &Value, name: &str) -> bool {
    matches!(recv, Value::Str(_) | Value::Bytes(_)) && matches!(name, "strip" | "split" | "find")
}

/// Dispatch a bound method call.
pub fn call_method(
    recv: &Value,
    name: &str,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> VResult<Value> {
    if !kwargs.is_empty() && !takes_kwargs(recv, name) {
        return Err(type_error(format!("{name}() takes no keyword arguments")));
    }
    match recv {
        _ if is_cast_method(name) => cast_method(recv, name, args),
        _ if is_collection(recv) && is_seq_native(name) => seq_native_method(recv, name, args),
        Value::Str(_) => str_method(recv, name, args, &kwargs),
        Value::Bytes(b) => bytes_method(b, name, args, &kwargs),
        Value::List(l) => list_method(l, name, args),
        Value::Dict(d) => dict_method(d, name, args),
        Value::Stream(s) => stream_method(s, name, args),
        Value::Regex(r) => regex_method(r, name, args),
        Value::Match(m) => match_method(m, name, args),
        other => Err(attribute_error(format!(
            "'{}' object has no method '{}'",
            other.type_name(),
            name
        ))),
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
        _ => Err(attribute_error(format!("'Pattern' object has no method '{name}'"))),
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
        [Value::Int(_)] => return Err(index_error("group index must be non-negative")),
        _ => return Err(type_error(format!("{name}() takes an optional group index"))),
    };
    match name {
        "group" => rx::group(m, n),
        "start" => rx::start(m, n),
        "end" => rx::end(m, n),
        _ => Err(attribute_error(format!("'Match' object has no method '{name}'"))),
    }
}

fn stream_method(
    s: &Rc<crate::stream::OroStream>,
    name: &str,
    args: Vec<Value>,
) -> VResult<Value> {
    match name {
        // The five that park, or wake a parked task, live in `crate::vm::sched`
        // instead: a native method must answer with a `Value`, and the whole
        // content of those is the `Step` they answer with. Spelled here rather
        // than left to fall through, so a routing mistake says what it is.
        "read" | "write" | "read_until" | "accept" | "close" => Err(runtime_error(format!(
            "internal: {name}() on a stream must be dispatched by the VM (it can park)"
        ))),
        "bytes" => {
            exactly(&args, 0, "bytes")?;
            Ok(Value::bytes(s.bytes()?))
        }
        // A half-close, and not the same thing as `close()`: it sends FIN and
        // keeps the read side, which is how a client says "that is the whole
        // request" on a connection it still expects an answer on.
        "shutdown_write" => {
            exactly(&args, 0, "shutdown_write")?;
            s.shutdown_write()?;
            Ok(Value::None)
        }
        "set_timeout" => {
            let secs = match args.as_slice() {
                [Value::None] => None,
                [Value::Int(n)] => Some(*n as f64),
                [Value::Float(f)] => Some(*f),
                [other] => {
                    return Err(type_error(format!(
                        "set_timeout() argument must be a number of seconds or None, not '{}'",
                        other.type_name()
                    )))
                }
                _ => return Err(type_error("set_timeout() takes one argument")),
            };
            s.set_timeout(secs)?;
            Ok(Value::None)
        }
        "set_nodelay" => {
            let on = match args.as_slice() {
                [Value::Bool(b)] => *b,
                [other] => {
                    return Err(type_error(format!(
                        "set_nodelay() argument must be bool, not '{}'",
                        other.type_name()
                    )))
                }
                _ => return Err(type_error("set_nodelay() takes one argument")),
            };
            s.set_nodelay(on)?;
            Ok(Value::None)
        }
        _ => Err(attribute_error(format!("'{}' object has no method '{name}'", s.kind.type_name()))),
    }
}

/// The type name of argument `i`, for a diagnostic. `null` when absent, so a
/// missing argument reads the same way a wrong one does.
pub(crate) fn type_of(args: &[Value], i: usize) -> &'static str {
    args.get(i).map(|v| v.type_name()).unwrap_or_else(|| Value::None.type_name())
}

/// Optional integer argument (Python's `maxsplit`, `width`, ... style).
/// Absent or `None` yields `default`.
fn opt_int_arg(args: &[Value], i: usize, who: &str, default: i64) -> VResult<i64> {
    match args.get(i) {
        None | Some(Value::None) => Ok(default),
        Some(Value::Int(n)) => Ok(*n),
        Some(Value::Bool(b)) => Ok(*b as i64),
        Some(other) => Err(type_error(format!(
            "{who}() argument must be int, not '{}'",
            other.type_name()
        ))),
    }
}

/// CPython's `ADJUST_INDICES`: fold a pair of Python slice bounds into offsets
/// over a sequence of `len` items. A negative bound counts from the end and
/// floors at 0, and `end` is capped at `len` — but `start` is deliberately
/// *not* capped, so a start past the end leaves a negative-width window. That
/// asymmetry is what makes `"abc".find("", 3)` 3 and `"abc".find("", 99)` -1.
fn adjust_indices(start: i64, end: i64, len: i64) -> (i64, i64) {
    let end = if end > len {
        len
    } else if end < 0 {
        end.saturating_add(len).max(0)
    } else {
        end
    };
    let start = if start < 0 { start.saturating_add(len).max(0) } else { start };
    (start, end)
}

/// The `[start, end)` window that `find`/`startswith`/`endswith` search, read
/// from the optional arguments beginning at `i`. `None` means the window has
/// negative width, in which case nothing matches — not even an empty needle.
fn search_window(args: &[Value], i: usize, who: &str, len: i64) -> VResult<Option<(i64, i64)>> {
    let start = opt_int_arg(args, i, who, 0)?;
    let end = opt_int_arg(args, i + 1, who, i64::MAX)?;
    let (start, end) = adjust_indices(start, end, len);
    Ok(if end < start { None } else { Some((start, end)) })
}

/// Byte offsets of characters `start` and `end`, both already clamped to
/// `0..=char_len`. O(1) for an ASCII string — the `is_ascii` flag on
/// [`OroStr`] exists for exactly this — and one pass otherwise.
fn char_window_bytes(os: &OroStr, start: usize, end: usize) -> (usize, usize) {
    if os.is_ascii {
        return (start, end);
    }
    let (mut b0, mut b1) = (os.s.len(), os.s.len());
    for (n, (b, _)) in os.s.char_indices().enumerate() {
        if n == start {
            b0 = b;
        }
        if n == end {
            b1 = b;
            break;
        }
    }
    (b0, b1)
}

/// `str.find` restricted to the character window `[start, end)`. The result is
/// a *character* index, as CPython's is, or -1.
fn str_find_in(os: &OroStr, needle: &str, start: usize, end: usize) -> i64 {
    let (b0, b1) = char_window_bytes(os, start, end);
    match os.s[b0..b1].find(needle) {
        Some(off) if os.is_ascii => (b0 + off) as i64,
        Some(off) => (start + os.s[b0..b0 + off].chars().count()) as i64,
        None => -1,
    }
}

/// `str.find(sub, ..., reverse=true)`, restricted to the character window
/// `[start, end)`: the *last* occurrence, as a character index, or -1. The twin
/// of [`str_find_in`], and the reason there is no `rfind`.
fn str_rfind_in(os: &OroStr, needle: &str, start: usize, end: usize) -> i64 {
    let (b0, b1) = char_window_bytes(os, start, end);
    match os.s[b0..b1].rfind(needle) {
        Some(off) if os.is_ascii => (b0 + off) as i64,
        Some(off) => (start + os.s[b0..b0 + off].chars().count()) as i64,
        None => -1,
    }
}

/// Which end(s) an operation works from — the `side=` keyword, shared by
/// `strip` and `split`, and the whole reason `lstrip`/`rstrip`/`rsplit` are
/// gone. `strip` takes all three values; `split` takes two, because a split
/// has no "both".
#[derive(Clone, Copy, PartialEq)]
enum Side {
    Both,
    Left,
    Right,
}

impl Side {
    fn cuts_left(self) -> bool {
        self != Side::Right
    }
    fn cuts_right(self) -> bool {
        self != Side::Left
    }
}

/// The one keyword `strip` takes. Keyword-only, and validated by name: a value
/// outside the three is a `ValueError` that *names the three*, because a strip
/// that silently did nothing is exactly the class of bug this language exists
/// to refuse.
fn strip_side(kwargs: &[(String, Value)]) -> VResult<Side> {
    let mut side = Side::Both;
    for (k, v) in kwargs {
        if k != "side" {
            return Err(type_error(format!("strip() got an unexpected keyword argument '{k}'")));
        }
        let Value::Str(s) = v else {
            return Err(type_error(format!("strip(): side must be str, not '{}'", v.type_name())));
        };
        side = match &*s.s {
            "both" => Side::Both,
            "left" => Side::Left,
            "right" => Side::Right,
            other => {
                return Err(value_error(format!(
                    "strip(): side must be \"both\", \"left\" or \"right\", not {}",
                    crate::value::repr_str(other)
                )))
            }
        };
    }
    Ok(side)
}

/// The one keyword `split` takes: which end `maxsplit` counts its splits from.
/// `"right"` is what `rsplit` did.
///
/// Two values, not `strip`'s three, and the error names two: a split from
/// "both" ends is not a thing that has a meaning, so accepting the word would
/// be answering a question that was not asked.
///
/// **`side` with no `maxsplit` is a no-op, deliberately, not an error.** With
/// the splits unlimited both ends produce the same list — CPython's `rsplit`
/// behaves the same way — so nothing is silently wrong. It could not be an
/// error honestly anyway: "`side` had no effect" is a property of the *data*
/// (`maxsplit` at or above the number of separators), not of the call, so a
/// check could only fire on the syntactic absence of the argument. That would
/// reject `split(sep, side="right")` while waving through `split(sep, -1,
/// side="right")` and `split(sep, 99, side="right")`, which are equally inert.
/// A rule that catches one of its three cases is worse than no rule.
fn split_side(kwargs: &[(String, Value)]) -> VResult<Side> {
    let mut side = Side::Left;
    for (k, v) in kwargs {
        if k != "side" {
            return Err(type_error(format!("split() got an unexpected keyword argument '{k}'")));
        }
        let Value::Str(s) = v else {
            return Err(type_error(format!("split(): side must be str, not '{}'", v.type_name())));
        };
        side = match &*s.s {
            "left" => Side::Left,
            "right" => Side::Right,
            other => {
                return Err(value_error(format!(
                    "split(): side must be \"left\" or \"right\", not {}",
                    crate::value::repr_str(other)
                )))
            }
        };
    }
    Ok(side)
}

/// `strip`'s trim, with the end(s) chosen by `side`.
fn trim_with(s: &str, side: Side, hit: impl Fn(char) -> bool + Copy) -> &str {
    let s = if side.cuts_left() { s.trim_start_matches(hit) } else { s };
    if side.cuts_right() {
        s.trim_end_matches(hit)
    } else {
        s
    }
}

/// The one keyword `find` takes: `reverse=true` asks for the last occurrence
/// rather than the first. Spelled the way `sorted(reverse=…)` already is.
fn find_reverse(kwargs: &[(String, Value)]) -> VResult<bool> {
    let mut reverse = false;
    for (k, v) in kwargs {
        if k != "reverse" {
            return Err(type_error(format!("find() got an unexpected keyword argument '{k}'")));
        }
        reverse = v.truthy();
    }
    Ok(reverse)
}

/// Non-overlapping occurrences of `sub` in an already-windowed slice,
/// CPython's `count`: an empty needle sits between every pair of characters and
/// at both ends, so it is found `len + 1` times — of the *window*, which is why
/// the caller narrows the slice before getting here rather than passing bounds
/// in.
fn count_sub(hay: &str, sub: &str) -> i64 {
    if sub.is_empty() {
        return hay.chars().count() as i64 + 1;
    }
    hay.matches(sub).count() as i64
}

/// [`count_sub`] over octets.
fn count_bytes(hay: &[u8], sub: &[u8]) -> i64 {
    if sub.is_empty() {
        return hay.len() as i64 + 1;
    }
    let (mut i, mut n) = (0usize, 0i64);
    while i + sub.len() <= hay.len() {
        if hay[i..i + sub.len()] == *sub {
            n += 1;
            i += sub.len();
        } else {
            i += 1;
        }
    }
    n
}

/// The four `is_*` predicates, for `str`. Whole-sequence semantics, matching
/// CPython's `isdigit`/`isalpha`/`isalnum`/`isspace` — including that the empty
/// string is `false` for all four, which is the part people get wrong.
///
/// `is_space` is exact (see [`is_py_space`]). The other three read the Unicode
/// properties Rust's standard library exposes — `Alphabetic` and `Numeric` —
/// where CPython tests the stricter general categories `L` and `N`. Checked
/// against CPython over every code point: they agree on all of ASCII, and on
/// the letters and digits of every script; they differ on 12,194 code points,
/// all of them combining marks (`Other_Alphabetic`), non-decimal numerals
/// (`Ⅷ`, `½`), or characters newer than the host CPython's tables. Every one of
/// those differences is in the same direction — Oro says `true` where CPython
/// says `false`, never the reverse — because these are supersets, not a
/// different answer. Closing the gap needs a Unicode general-category table,
/// which is a dependency (or a copy of one that goes stale) for a case no
/// scripting program has.
fn str_is_class(s: &str, name: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    s.chars().all(|c| match name {
        "is_digit" => c.is_numeric(),
        "is_alpha" => c.is_alphabetic(),
        "is_alnum" => c.is_alphanumeric(),
        _ => is_py_space(c),
    })
}

/// The same four for `bytes`, where an octet is not a character: ASCII only,
/// which is exactly what CPython's `bytes.isdigit` and friends do.
fn bytes_is_class(b: &[u8], name: &str) -> bool {
    if b.is_empty() {
        return false;
    }
    b.iter().all(|&x| match name {
        "is_digit" => x.is_ascii_digit(),
        "is_alpha" => x.is_ascii_alphabetic(),
        "is_alnum" => x.is_ascii_alphanumeric(),
        _ => is_bytes_space(x),
    })
}

/// CPython's `tailmatch`, shared by `startswith` and `endswith` for both `str`
/// and `bytes`: the window shrinks by the affix's length first, so a window too
/// small to hold the affix fails *before* an empty affix is considered true.
/// `at_front` picks which end of the window the comparison happens at.
fn tail_window(start: i64, end: i64, affix_len: i64, at_front: bool) -> Option<i64> {
    let end = end - affix_len;
    if end < start {
        return None;
    }
    Some(if at_front { start } else { end })
}

fn str_arg(args: &[Value], i: usize, who: &str) -> VResult<String> {
    match args.get(i) {
        Some(Value::Str(s)) => Ok(s.s.clone()),
        Some(other) => Err(type_error(format!(
            "{who}() argument must be str, not '{}'",
            other.type_name()
        ))),
        None => Err(type_error(format!("{who}() missing a required argument"))),
    }
}

/// `str.split` on an explicit separator, honouring `maxsplit`
/// (negative = unlimited) and the end it counts from.
///
/// With `maxsplit` unlimited the two sides give the same list, so `side` only
/// reaches the branch below when it can change the answer.
fn split_sep_n(s: &str, sep: &str, maxsplit: i64, side: Side) -> Vec<String> {
    if maxsplit < 0 {
        return s.split(sep).map(|p| p.to_string()).collect();
    }
    // maxsplit splits => maxsplit + 1 pieces.
    let n = (maxsplit as usize).saturating_add(1);
    if side == Side::Right {
        // `rsplitn` walks from the right and yields right to left, so the
        // pieces come back in reverse source order.
        let mut parts: Vec<String> = s.rsplitn(n, sep).map(|p| p.to_string()).collect();
        parts.reverse();
        return parts;
    }
    s.splitn(n, sep).map(|p| p.to_string()).collect()
}

/// The characters CPython calls whitespace: Unicode `White_Space`, plus the
/// four ASCII separators U+001C..U+001F that `str.isspace()` counts and Rust's
/// `char::is_whitespace` does not. Checked against CPython over every code
/// point; those four are the only difference, in either direction.
fn is_py_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}')
}

/// `s.split()` with no limit, over [`is_py_space`] rather than Rust's slightly
/// smaller whitespace set.
fn split_whitespace_all(s: &str) -> Vec<String> {
    s.split(is_py_space).filter(|p| !p.is_empty()).map(|p| p.to_string()).collect()
}

/// `str.split(null, maxsplit)`: runs of whitespace separate, leading and
/// trailing whitespace is discarded, and once `maxsplit` splits are made the
/// remainder is returned verbatim (interior whitespace and all) — from
/// whichever end `side` names.
fn split_whitespace_n(s: &str, maxsplit: i64, side: Side) -> Vec<String> {
    if maxsplit < 0 {
        return split_whitespace_all(s);
    }
    if side == Side::Right {
        return split_whitespace_n_right(s, maxsplit);
    }
    let limit = maxsplit as usize;
    let chars: Vec<char> = s.chars().collect();
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0usize;
    while parts.len() < limit {
        while i < chars.len() && is_py_space(chars[i]) {
            i += 1;
        }
        if i >= chars.len() {
            return parts;
        }
        let start = i;
        while i < chars.len() && !is_py_space(chars[i]) {
            i += 1;
        }
        parts.push(chars[start..i].iter().collect());
    }
    // Remainder: drop only the whitespace that separated it from the last field.
    while i < chars.len() && is_py_space(chars[i]) {
        i += 1;
    }
    if i < chars.len() {
        parts.push(chars[i..].iter().collect());
    }
    parts
}

/// [`split_whitespace_n`] scanning from the right — what `rsplit(null, n)`
/// does. Written as its own walk rather than a direction flag on the one
/// above: the two loops differ in every bound, and a shared body would be a
/// worse thing to read than a mirror of a short one.
fn split_whitespace_n_right(s: &str, maxsplit: i64) -> Vec<String> {
    let limit = maxsplit as usize;
    let chars: Vec<char> = s.chars().collect();
    let mut parts: Vec<String> = Vec::new();
    let mut i = chars.len();
    while parts.len() < limit {
        while i > 0 && is_py_space(chars[i - 1]) {
            i -= 1;
        }
        if i == 0 {
            parts.reverse();
            return parts;
        }
        let end = i;
        while i > 0 && !is_py_space(chars[i - 1]) {
            i -= 1;
        }
        parts.push(chars[i..end].iter().collect());
    }
    // Remainder: drop only the whitespace that separated it from the last field.
    while i > 0 && is_py_space(chars[i - 1]) {
        i -= 1;
    }
    if i > 0 {
        parts.push(chars[..i].iter().collect());
    }
    parts.reverse();
    parts
}

/// The conversion methods, available on every value: `to_str`, `to_int`,
/// `to_float`, `to_bool`, `to_list`, `to_dict`. Instances and containers whose
/// `to_str` must run an Oro dunder are intercepted by the VM before reaching
/// here, since a native method can never re-enter the interpreter.
/// A sequence of ints as octets. Out of range or not an int is a `ValueError`
/// naming the offending element: a `bytes` built from a number that did not fit
/// in a byte would be silently wrong data, which is the failure mode this
/// language exists to refuse.
fn ints_to_bytes(items: &[Value]) -> VResult<Value> {
    let mut out = Vec::with_capacity(items.len());
    for (i, v) in items.iter().enumerate() {
        match v {
            Value::Int(n) if (0..=255).contains(n) => out.push(*n as u8),
            // A bool is an int everywhere else in the language — `xs[True]`,
            // `sum([True, True])` — so it is one here too.
            Value::Bool(b) => out.push(*b as u8),
            Value::Int(n) => {
                return Err(value_error(format!(
                    "to_bytes(): item {i} must be in range(0, 256), got {n}"
                )))
            }
            other => {
                return Err(value_error(format!(
                    "to_bytes(): item {i} must be an int, not '{}'",
                    other.type_name()
                )))
            }
        }
    }
    Ok(Value::bytes(out))
}

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
                        Err(value_error(format!(
                            "bytes could not be decoded as UTF-8: invalid byte 0x{:02x} at \
                             position {pos}",
                            b[pos]
                        )))
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
                // `bytes` is a sequence of ints, and this is the only way to
                // *construct* one from ints. `chr(200).to_bytes()` yields the
                // two octets of U+00C8 in UTF-8, not one octet 200, so without
                // this there is no way to build `b"\xc8"` from a computed
                // value at all — which makes every binary protocol
                // unwriteable.
                Value::List(l) => ints_to_bytes(&l.borrow()),
                Value::Tuple(t) => ints_to_bytes(t),
                other => Err(type_error(format!(
                    "'{}' object has no conversion to bytes",
                    other.type_name()
                ))),
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
                    .map_err(|_| value_error(format!(
                        "could not convert string to float: '{}'",
                        s.s
                    ))),
                other => Err(type_error(format!(
                    "'{}' object has no conversion to float",
                    other.type_name()
                ))),
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
                other => Err(type_error(format!(
                    "'{}' object has no conversion to int",
                    other.type_name()
                ))),
            }
        }
        "to_list" => {
            exactly(&args, 0, "to_list")?;
            let items = crate::vm::iterate_to_vec(recv)?;
            Ok(Value::List(OroList::new(items)))
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
                        return Err(type_error(format!(
                            "to_dict() requires (key, value) pairs, found '{}'",
                            other.type_name()
                        )))
                    }
                };
                if parts.len() != 2 {
                    return Err(value_error(format!(
                        "to_dict() requires 2-element pairs, found one of length {}",
                        parts.len()
                    )));
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

/// Whether a native method reads its receiver *as a sequence*, and therefore
/// needs a generator receiver drained into a list before it can run.
///
/// Native code cannot re-enter the interpreter, so it cannot resume a
/// generator; the VM drains one a frame at a time and retries the call. That
/// already happened for a generator handed to a builtin, and for the
/// callback-driven half of the collection protocol (`g().map(f)`), but not for
/// the native half — which is why `g().to_list()` used to surface an internal
/// invariant message and `g().sum()` claimed a generator had no such method,
/// both of which the README promises work.
///
/// `to_str` and `to_bool` are deliberately not here: they ask about the
/// generator, not about its elements, and draining would make an empty
/// generator falsy and print a list where `<generator>` is the honest answer.
pub fn drains_generator_receiver(name: &str) -> bool {
    matches!(name, "to_list" | "to_dict") || is_seq_native(name)
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
                .map(|(k, v)| Value::Tuple(OroTuple::new(vec![k.clone(), v.clone()])))
                .collect(),
        ),
        Value::Range(_) => (Shape::List, crate::vm::iterate_to_vec(recv)?),
        other => {
            return Err(attribute_error(format!(
                "'{}' object has no method '{who}'",
                other.type_name()
            )))
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
        Shape::List => Value::List(OroList::new(items)),
        Shape::Tuple => Value::Tuple(OroTuple::new(items)),
        Shape::Dict => {
            let mut d = OroDict::new();
            for entry in items {
                let pair = match &entry {
                    Value::Tuple(t) => t.as_slice().to_vec(),
                    Value::List(l) => l.borrow().clone(),
                    other => {
                        return Err(type_error(format!(
                            "rebuilding a dict needs (key, value) pairs, not '{}'",
                            other.type_name()
                        )))
                    }
                };
                if pair.len() != 2 {
                    return Err(value_error(format!(
                        "rebuilding a dict needs 2-element pairs, got {}",
                        pair.len()
                    )));
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
                None => Err(index_error(format!("{name}() on an empty sequence"))),
            }
        }
        "sum" => {
            exactly(&args, 0, "sum")?;
            bi_sum(vec![rebuild(Shape::List, items)?])
        }
        "min" | "max" => {
            exactly(&args, 0, name)?;
            if items.is_empty() {
                return Err(value_error(format!("{name}() arg is an empty sequence")));
            }
            let mut best = items[0].clone();
            for v in &items[1..] {
                let ord = ord_or_defer(v, &best, if name == "min" { "<" } else { ">" })?;
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
                return Err(value_error(format!("{name}() needs a count >= 0")));
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
            Ok(Value::List(OroList::new(out)))
        }
        "chunk" => {
            let n = opt_int_arg(&args, 0, "chunk", 0)?;
            if n <= 0 {
                return Err(value_error("chunk() needs a size >= 1"));
            }
            let out: Vec<Value> = items
                .chunks(n as usize)
                .map(|c| Value::List(OroList::new(c.to_vec())))
                .collect();
            Ok(Value::List(OroList::new(out)))
        }
        "zip" => {
            // `a.zip(b, c)` is `zip(a, b, c)` — every sequence, truncated to
            // the shortest — and not `zip(a, b)` with `c` thrown away.
            let mut cols = Vec::with_capacity(args.len() + 1);
            cols.push(items);
            for a in &args {
                cols.push(crate::vm::iterate_to_vec(a)?);
            }
            Ok(zip_cols(cols))
        }
        "enumerate" => {
            // `enumerate` takes a start and nothing else. Without this an
            // `xs.enumerate(1, 9)` answered confidently, having read neither
            // the 9 nor the reader's mind.
            at_most(&args, 1, "enumerate")?;
            let start = opt_int_arg(&args, 0, "enumerate", 0)?;
            let out: Vec<Value> = items
                .into_iter()
                .enumerate()
                .map(|(i, v)| Value::Tuple(OroTuple::new(vec![Value::Int(start + i as i64), v])))
                .collect();
            Ok(Value::List(OroList::new(out)))
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
                            return Err(type_error(format!(
                                "join() requires bytes elements, found '{}'",
                                other.type_name()
                            )))
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
                        return Err(type_error(format!(
                            "join() requires str elements, found '{}'",
                            other.type_name()
                        )))
                    }
                }
            }
            Ok(Value::str(pieces.join(&sep)))
        }
        _ => unreachable!("not a native sequence method"),
    }
}

fn str_method(
    recv: &Value,
    name: &str,
    args: Vec<Value>,
    kwargs: &[(String, Value)],
) -> VResult<Value> {
    // Borrowed, not cloned: `find` and `replace` are on hot paths, and the
    // arms that hand the receiver straight back clone the `Rc` instead of the
    // characters.
    let os = match recv {
        Value::Str(s) => s,
        _ => unreachable!(),
    };
    let s: &str = &os.s;
    match name {
        "upper" => {
            exactly(&args, 0, "upper")?;
            Ok(Value::str(s.to_uppercase()))
        }
        "lower" => {
            exactly(&args, 0, "lower")?;
            Ok(Value::str(s.to_lowercase()))
        }
        "strip" => {
            at_most(&args, 1, name)?;
            let side = strip_side(kwargs)?;
            // `chars` is a *set* of characters to remove from the end(s), not a
            // prefix or a suffix: `"xyx".strip("xy")` is `""`. That is the
            // footgun `rm_prefix`/`rm_suffix` exist to answer. Omitted (or
            // null), whitespace is stripped instead.
            let trimmed = match args.first() {
                None | Some(Value::None) => trim_with(s, side, is_py_space),
                Some(Value::Str(set)) => trim_with(s, side, |c| set.s.contains(c)),
                Some(other) => {
                    return Err(type_error(format!(
                        "{name}() argument must be str, not '{}'",
                        other.type_name()
                    )))
                }
            };
            if trimmed.len() == s.len() {
                return Ok(Value::Str(os.clone()));
            }
            Ok(Value::str(trimmed.to_string()))
        }
        "rm_prefix" | "rm_suffix" => {
            exactly(&args, 1, name)?;
            let affix = str_arg(&args, 0, name)?;
            // A *literal* prefix or suffix, removed once if it is there. This
            // is the method people reach for `strip` and get a character set.
            let rest = if name == "rm_prefix" {
                s.strip_prefix(&affix)
            } else {
                s.strip_suffix(&affix)
            };
            match rest {
                Some(r) => Ok(Value::str(r.to_string())),
                None => Ok(Value::Str(os.clone())),
            }
        }
        "count" => {
            at_most(&args, 3, "count")?;
            let sub = str_arg(&args, 0, "count")?;
            let len = os.char_len() as i64;
            // The same window `find` searches, read the same way — `count` is
            // "how many times", `find` is "where", and asking them over
            // different regions of the same string would be the asymmetry this
            // surface exists to not have.
            let Some((start, end)) = search_window(&args, 1, "count", len)? else {
                return Ok(Value::Int(0));
            };
            let (b0, b1) = char_window_bytes(os, start as usize, end as usize);
            Ok(Value::Int(count_sub(&s[b0..b1], &sub)))
        }
        "is_digit" | "is_alpha" | "is_alnum" | "is_space" => {
            exactly(&args, 0, name)?;
            Ok(Value::Bool(str_is_class(s, name)))
        }
        "startswith" | "endswith" => {
            at_most(&args, 3, name)?;
            let affix = str_arg(&args, 0, name)?;
            let len = os.char_len() as i64;
            let Some((start, end)) = search_window(&args, 1, name, len)? else {
                return Ok(Value::Bool(false));
            };
            let alen = if os.is_ascii && affix.is_ascii() {
                affix.len() as i64
            } else {
                affix.chars().count() as i64
            };
            let Some(at) = tail_window(start, end, alen, name == "startswith") else {
                return Ok(Value::Bool(false));
            };
            if alen == 0 {
                return Ok(Value::Bool(true));
            }
            let (b0, b1) = char_window_bytes(os, at as usize, (at + alen) as usize);
            Ok(Value::Bool(s[b0..b1] == *affix))
        }
        "find" => {
            at_most(&args, 3, "find")?;
            let reverse = find_reverse(kwargs)?;
            let needle = str_arg(&args, 0, "find")?;
            let len = os.char_len() as i64;
            let Some((start, end)) = search_window(&args, 1, "find", len)? else {
                return Ok(Value::Int(-1));
            };
            let (start, end) = (start as usize, end as usize);
            Ok(Value::Int(if reverse {
                str_rfind_in(os, &needle, start, end)
            } else {
                str_find_in(os, &needle, start, end)
            }))
        }
        "replace" => {
            at_most(&args, 3, "replace")?;
            let from = str_arg(&args, 0, "replace")?;
            let to = str_arg(&args, 1, "replace")?;
            // A negative count means "every occurrence", which is the default.
            let count = opt_int_arg(&args, 2, "replace", -1)?;
            if count < 0 {
                return Ok(Value::str(s.replace(&from, &to)));
            }
            Ok(Value::str(s.replacen(&from, &to, count as usize)))
        }
        "split" => {
            at_most(&args, 2, name)?;
            // maxsplit < 0 (the default) means "no limit"; maxsplit == n caps the
            // number of *splits*, so at most n + 1 pieces come back. `side`
            // picks the end those n splits are counted from, and is what
            // `rsplit` used to be.
            let side = split_side(kwargs)?;
            let maxsplit = opt_int_arg(&args, 1, name, -1)?;
            let parts: Vec<String> = match args.first() {
                None | Some(Value::None) => split_whitespace_n(s, maxsplit, side),
                Some(Value::Str(sep)) => {
                    if sep.s.is_empty() {
                        return Err(value_error("empty separator"));
                    }
                    split_sep_n(s, &sep.s, maxsplit, side)
                }
                Some(other) => {
                    return Err(type_error(format!(
                        "{name}() separator must be str, not '{}'",
                        other.type_name()
                    )))
                }
            };
            let parts = parts.into_iter().map(Value::str).collect::<Vec<_>>();
            Ok(Value::List(OroList::new(parts)))
        }
        "join" => {
            exactly(&args, 1, "join")?;
            let items = crate::vm::iterate_to_vec(&args[0])?;
            let mut pieces = Vec::with_capacity(items.len());
            for it in items {
                match it {
                    Value::Str(p) => pieces.push(p.s.clone()),
                    other => {
                        return Err(type_error(format!(
                            "join() requires str elements, found '{}'",
                            other.type_name()
                        )))
                    }
                }
            }
            Ok(Value::str(pieces.join(s)))
        }
        _ => Err(attribute_error(format!("'str' object has no method '{name}'"))),
    }
}

pub(crate) fn bytes_arg(args: &[Value], i: usize, who: &str) -> VResult<Rc<Vec<u8>>> {
    match args.get(i) {
        Some(Value::Bytes(b)) => Ok(b.clone()),
        Some(other) => Err(type_error(format!(
            "{who}() argument must be bytes, not '{}'",
            other.type_name()
        ))),
        None => Err(type_error(format!("{who}() missing a required argument"))),
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

/// The offset of the *last* `needle` in `hay`, or `None` — what
/// `find(sub, reverse=true)` answers. The empty needle sits at the end.
fn bytes_rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(hay.len());
    }
    if needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).rposition(|w| w == needle)
}

/// `bytes.replace` with CPython's `count`: negative means every occurrence.
/// An empty `from` inserts `to` between every pair of octets and at both ends,
/// which is what `str.replace` does with an empty pattern — and `count` caps
/// those insertions from the left, so `b"abc".replace(b"", b"-", 2)` is
/// `b"-a-bc"`.
fn bytes_replace(hay: &[u8], from: &[u8], to: &[u8], count: i64) -> Vec<u8> {
    let limit = if count < 0 { usize::MAX } else { count as usize };
    let mut out = Vec::with_capacity(hay.len());
    let mut done = 0usize;
    if from.is_empty() {
        for &x in hay {
            if done < limit {
                out.extend_from_slice(to);
                done += 1;
            }
            out.push(x);
        }
        if done < limit {
            out.extend_from_slice(to);
        }
        return out;
    }
    let mut i = 0;
    while i < hay.len() {
        if done < limit && hay[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
            done += 1;
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    out
}

/// `bytes.split` on an explicit separator — the byte-level twin of
/// [`split_sep_n`], with the same `maxsplit` rule (negative = unlimited) and
/// the same `side`.
fn split_sep_bytes(s: &[u8], sep: &[u8], maxsplit: i64, side: Side) -> Vec<Vec<u8>> {
    let limit = if maxsplit < 0 { usize::MAX } else { maxsplit as usize };
    if side == Side::Right {
        return split_sep_bytes_right(s, sep, limit);
    }
    let mut parts: Vec<Vec<u8>> = Vec::new();
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
    parts
}

/// [`split_sep_bytes`] scanning from the right. The separator is non-empty
/// (`split` rejects an empty one before this is reached), so the window can
/// never be zero-width and the walk always terminates.
fn split_sep_bytes_right(s: &[u8], sep: &[u8], limit: usize) -> Vec<Vec<u8>> {
    let mut parts: Vec<Vec<u8>> = Vec::new();
    let (mut end, mut i) = (s.len(), s.len());
    while parts.len() < limit && i >= sep.len() {
        if s[i - sep.len()..i] == *sep {
            parts.push(s[i..end].to_vec());
            i -= sep.len();
            end = i;
        } else {
            i -= 1;
        }
    }
    parts.push(s[..end].to_vec());
    parts.reverse();
    parts
}

/// `bytes.split(null, maxsplit)` — the byte-level twin of
/// [`split_whitespace_n`]: runs of whitespace separate, leading and trailing
/// whitespace is discarded, and the remainder after `maxsplit` splits comes
/// back verbatim, from whichever end `side` names.
fn split_space_bytes(s: &[u8], maxsplit: i64, side: Side) -> Vec<Vec<u8>> {
    let limit = if maxsplit < 0 { usize::MAX } else { maxsplit as usize };
    if side == Side::Right {
        return split_space_bytes_right(s, limit);
    }
    let mut parts: Vec<Vec<u8>> = Vec::new();
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
}

/// [`split_space_bytes`] scanning from the right — the mirror of
/// [`split_whitespace_n_right`], over octets.
fn split_space_bytes_right(s: &[u8], limit: usize) -> Vec<Vec<u8>> {
    let mut parts: Vec<Vec<u8>> = Vec::new();
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

/// The `bytes` methods. Deliberately the same names `str` carries — every one
/// exists on CPython's `bytes` with the same meaning, so the oracle covers the
/// whole set — plus `hex`, which has no `str` counterpart. Case folding is
/// ASCII-only: an octet is not a character, and there is no encoding here to
/// case a non-ASCII one under.
fn bytes_method(
    b: &Rc<Vec<u8>>,
    name: &str,
    args: Vec<Value>,
    kwargs: &[(String, Value)],
) -> VResult<Value> {
    match name {
        // `b.scan(allowed)`: how many bytes at the front of `b` are all in
        // `allowed`. `len(b)` means every one of them was.
        //
        // The generic primitive `docs/stdlib-server-design.md` §5 said to reach
        // for when Oro-level parsing crossed its threshold — though not quite
        // the one it predicted. §5 guessed a multi-delimiter scan; what the
        // HTTP parser was actually spending its time on is the opposite
        // question, "is every byte of this field in the set the grammar
        // allows", which no `find` can answer. This spelling answers both: a
        // multi-delimiter find is `b.scan(everything_but_the_delimiters)`,
        // which returns the first delimiter's index.
        //
        // Nothing here knows what a header is. The set is the caller's, built
        // once from whatever grammar the caller is implementing.
        "scan" => {
            exactly(&args, 1, "scan")?;
            let Some(Value::Bytes(allowed)) = args.first() else {
                return Err(type_error(format!(
                    "scan() argument must be bytes, not '{}'",
                    args[0].type_name()
                )));
            };
            // A 256-bit membership table, built per call. It costs one pass
            // over `allowed` and then every byte of `b` is one shift and one
            // test — which is the whole point, because the Oro spelling costs
            // a dict lookup and a loop iteration per byte instead.
            let mut set = [0u64; 4];
            for &c in allowed.iter() {
                set[(c >> 6) as usize] |= 1u64 << (c & 63);
            }
            let n = b
                .iter()
                .take_while(|&&c| set[(c >> 6) as usize] & (1u64 << (c & 63)) != 0)
                .count();
            Ok(Value::Int(n as i64))
        }
        "upper" => {
            exactly(&args, 0, "upper")?;
            Ok(Value::bytes(b.to_ascii_uppercase()))
        }
        "lower" => {
            exactly(&args, 0, "lower")?;
            Ok(Value::bytes(b.to_ascii_lowercase()))
        }
        "strip" => {
            at_most(&args, 1, name)?;
            let side = strip_side(kwargs)?;
            // As for `str`, the argument is a *set* of octets to remove from
            // the end(s); omitted (or null), whitespace is removed instead.
            let cut: Box<dyn Fn(u8) -> bool> = match args.first() {
                None | Some(Value::None) => Box::new(is_bytes_space),
                Some(Value::Bytes(set)) => {
                    let set = set.clone();
                    Box::new(move |x| set.contains(&x))
                }
                Some(other) => {
                    return Err(type_error(format!(
                        "{name}() argument must be bytes, not '{}'",
                        other.type_name()
                    )))
                }
            };
            let lead = if side.cuts_left() {
                b.iter().take_while(|&&x| cut(x)).count()
            } else {
                0
            };
            if lead == b.len() {
                return Ok(Value::bytes(Vec::new()));
            }
            let trail = if side.cuts_right() {
                b[lead..].iter().rev().take_while(|&&x| cut(x)).count()
            } else {
                0
            };
            if lead == 0 && trail == 0 {
                return Ok(Value::Bytes(b.clone()));
            }
            Ok(Value::bytes(b[lead..b.len() - trail].to_vec()))
        }
        "rm_prefix" | "rm_suffix" => {
            exactly(&args, 1, name)?;
            let affix = bytes_arg(&args, 0, name)?;
            let rest = if name == "rm_prefix" {
                b.strip_prefix(&affix[..])
            } else {
                b.strip_suffix(&affix[..])
            };
            match rest {
                Some(r) => Ok(Value::bytes(r.to_vec())),
                None => Ok(Value::Bytes(b.clone())),
            }
        }
        "count" => {
            at_most(&args, 3, "count")?;
            let sub = bytes_arg(&args, 0, "count")?;
            let Some((start, end)) = search_window(&args, 1, "count", b.len() as i64)? else {
                return Ok(Value::Int(0));
            };
            Ok(Value::Int(count_bytes(&b[start as usize..end as usize], &sub)))
        }
        "is_digit" | "is_alpha" | "is_alnum" | "is_space" => {
            exactly(&args, 0, name)?;
            Ok(Value::Bool(bytes_is_class(b, name)))
        }
        "startswith" | "endswith" => {
            at_most(&args, 3, name)?;
            let affix = bytes_arg(&args, 0, name)?;
            let Some((start, end)) = search_window(&args, 1, name, b.len() as i64)? else {
                return Ok(Value::Bool(false));
            };
            let Some(at) = tail_window(start, end, affix.len() as i64, name == "startswith")
            else {
                return Ok(Value::Bool(false));
            };
            let at = at as usize;
            Ok(Value::Bool(b[at..at + affix.len()] == **affix))
        }
        "find" => {
            at_most(&args, 3, "find")?;
            let reverse = find_reverse(kwargs)?;
            let needle = bytes_arg(&args, 0, "find")?;
            let Some((start, end)) = search_window(&args, 1, "find", b.len() as i64)? else {
                return Ok(Value::Int(-1));
            };
            let (start, end) = (start as usize, end as usize);
            let hit = if reverse {
                bytes_rfind(&b[start..end], &needle)
            } else {
                bytes_find(&b[start..end], &needle)
            };
            Ok(Value::Int(match hit {
                Some(i) => (start + i) as i64,
                None => -1,
            }))
        }
        "replace" => {
            at_most(&args, 3, "replace")?;
            let from = bytes_arg(&args, 0, "replace")?;
            let to = bytes_arg(&args, 1, "replace")?;
            let count = opt_int_arg(&args, 2, "replace", -1)?;
            Ok(Value::bytes(bytes_replace(b, &from, &to, count)))
        }
        "split" => {
            at_most(&args, 2, name)?;
            let side = split_side(kwargs)?;
            let maxsplit = opt_int_arg(&args, 1, name, -1)?;
            let parts: Vec<Vec<u8>> = match args.first() {
                None | Some(Value::None) => split_space_bytes(b, maxsplit, side),
                Some(Value::Bytes(sep)) => {
                    if sep.is_empty() {
                        return Err(value_error("empty separator"));
                    }
                    split_sep_bytes(b, sep, maxsplit, side)
                }
                Some(other) => {
                    return Err(type_error(format!(
                        "{name}() separator must be bytes, not '{}'",
                        other.type_name()
                    )))
                }
            };
            let parts = parts.into_iter().map(Value::bytes).collect::<Vec<_>>();
            Ok(Value::List(OroList::new(parts)))
        }
        "join" => {
            exactly(&args, 1, "join")?;
            let items = crate::vm::iterate_to_vec(&args[0])?;
            let mut out: Vec<u8> = Vec::new();
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    out.extend_from_slice(b);
                }
                match it {
                    Value::Bytes(p) => out.extend_from_slice(p),
                    other => {
                        return Err(type_error(format!(
                            "join() requires bytes elements, found '{}'",
                            other.type_name()
                        )))
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
        _ => Err(attribute_error(format!("'bytes' object has no method '{name}'"))),
    }
}

fn list_method(l: &Rc<OroList>, name: &str, args: Vec<Value>) -> VResult<Value> {
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
                        return Err(index_error("pop from empty list"));
                    }
                    b.len() - 1
                }
                [v] => {
                    let i = as_i64(v)?;
                    let adj = if i < 0 { i + b.len() as i64 } else { i };
                    if adj < 0 || adj as usize >= b.len() {
                        return Err(index_error("pop index out of range"));
                    }
                    adj as usize
                }
                _ => return Err(type_error("pop() takes at most 1 argument")),
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
        _ => Err(attribute_error(format!("'list' object has no method '{name}'"))),
    }
}

fn dict_method(d: &Rc<RefCell<OroDict>>, name: &str, args: Vec<Value>) -> VResult<Value> {
    match name {
        "get" => {
            let (key, default) = match args.as_slice() {
                [k] => (k, Value::None),
                [k, def] => (k, def.clone()),
                _ => return Err(type_error("get() takes 1 or 2 arguments")),
            };
            Ok(d.borrow().get(key)?.unwrap_or(default))
        }
        // The removal, and the reason `del` could be cut: `d.pop(k)` raises
        // `KeyError` when the key is absent, `d.pop(k, default)` answers the
        // default. CPython's two arities exactly — the README has cited this
        // method as `del`'s replacement since before it existed.
        "pop" => {
            let (key, default) = match args.as_slice() {
                [k] => (k, None),
                [k, def] => (k, Some(def)),
                _ => return Err(type_error("pop() takes 1 or 2 arguments")),
            };
            match d.borrow_mut().remove(key)? {
                Some(v) => Ok(v),
                None => match default {
                    Some(def) => Ok(def.clone()),
                    None => Err(key_error(format!("key error: {}", key.repr()))),
                },
            }
        }
        "keys" => {
            exactly(&args, 0, "keys")?;
            Ok(Value::List(OroList::new(d.borrow().keys())))
        }
        "values" => {
            exactly(&args, 0, "values")?;
            Ok(Value::List(OroList::new(d.borrow().values())))
        }
        _ => Err(attribute_error(format!("'dict' object has no method '{name}'"))),
    }
}

