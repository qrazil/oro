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
        "clamp" => bi_clamp,
        "repr" => bi_repr,
        "open" => bi_open,
        "set" => bi_set,
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
        // `apply(f, args=…, kwargs=…)` is the whole of what `f(*xs)` and
        // `f(**d)` used to spell, and it calls `f` — which only the VM can do,
        // since a call is a frame and a native answers with a value. So it is
        // named here and intercepted there, like the three above.
        "apply" => bi_vm_dispatched,
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
        "clamp" => "clamp",
        "repr" => "repr",
        "open" => "open",
        "set" => "set",
        "round" => "round",
        "chr" => "chr",
        "ord" => "ord",
        "spawn" => "spawn",
        "chan" => "chan",
        "yield_now" => "yield_now",
        "apply" => "apply",
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

/// The keyword arguments a native takes, bound by name. Under the argument
/// rule every parameter with a default is keyword-only, so this is where a
/// native reads one: a name it does not take is a TypeError, and so is a name
/// given twice, rather than either being silently dropped.
fn bind_kwargs<'a, const N: usize>(
    who: &str,
    kwargs: &'a [(String, Value)],
    names: [&str; N],
) -> VResult<[Option<&'a Value>; N]> {
    let mut out = [None; N];
    for (k, v) in kwargs {
        let Some(i) = names.iter().position(|n| n == k) else {
            return Err(type_error(format!("{who}() got an unexpected keyword argument '{k}'")));
        };
        if out[i].is_some() {
            return Err(type_error(format!(
                "{who}() got multiple values for keyword argument '{k}'"
            )));
        }
        out[i] = Some(v);
    }
    Ok(out)
}

/// An explicit `null` for a keyword whose default is not `null`. It used to
/// mean "omitted", which made `f(x=null)` a second spelling of `f()`; the
/// refusal names the one spelling that is left.
pub(crate) fn null_is_not_omitted(who: &str, name: &str, want: &str) -> VErr {
    type_error(format!(
        "{who}(): {name}= must be {want}, not null — null does not mean \"omitted\"; \
         leave {name}= out for the default"
    ))
}

/// A keyword-only integer: `default` when omitted, and never `null`.
fn kw_int(who: &str, name: &str, v: Option<&Value>, default: i64) -> VResult<i64> {
    match v {
        None => Ok(default),
        Some(Value::Int(n)) => Ok(*n),
        Some(Value::Bool(b)) => Ok(*b as i64),
        Some(Value::None) => Err(null_is_not_omitted(who, name, "int")),
        Some(other) => Err(type_error(format!(
            "{who}(): {name}= must be int, not '{}'",
            other.type_name()
        ))),
    }
}

/// Refuse positional arguments past the `n` a native takes, and say where the
/// rest went. Each of these was a valid positional spelling until the
/// argument rule made defaulted parameters keyword-only, so a bare arity error
/// would leave the reader to rediscover the keyword; `fix` says it, usually as
/// the reader's own call rewritten (see [`respell`]).
fn positional_at_most(
    args: &[Value],
    n: usize,
    who: &str,
    fix: impl FnOnce() -> String,
) -> VResult<()> {
    if args.len() <= n {
        return Ok(());
    }
    let takes = match n {
        0 => "no positional arguments".to_string(),
        1 => "1 positional argument".to_string(),
        _ => format!("{n} positional arguments"),
    };
    let given = args.len();
    let verb = if given == 1 { "was" } else { "were" };
    Err(type_error(format!("{who}() takes {takes} but {given} {verb} given — {}", fix())))
}

/// How an argument appears in a rewritten call: a scalar as its literal, so the
/// fix can be pasted, and anything else as `…`.
pub(crate) fn lit(v: &Value) -> String {
    match v {
        Value::None | Value::Bool(_) | Value::Int(_) | Value::Float(_) => v.repr(),
        Value::Str(s) if s.char_len() <= 24 => v.repr(),
        Value::Bytes(b) if b.len() <= 24 => v.repr(),
        _ => "…".to_string(),
    }
}

/// A call rewritten into the argument rule's shape: `fixed` stay positional,
/// and each of `named` becomes `name=value` — or is dropped, where the old
/// positional spelling passed `null` to mean "omitted".
pub(crate) fn respell(who: &str, fixed: &[&Value], named: &[(&str, &Value)]) -> String {
    let parts: Vec<String> = fixed
        .iter()
        .map(|v| lit(v))
        .chain(
            named
                .iter()
                .filter(|(_, v)| !matches!(v, Value::None))
                .map(|(n, v)| format!("{n}={}", lit(v))),
        )
        .collect();
    format!("`{who}({})`", parts.join(", "))
}

/// `find(sub, 2, 5)` as `find(sub, start=2, end=5)`: the window `find`,
/// `count`, `startswith` and `endswith` share.
fn window_spelling(who: &str, args: &[Value]) -> String {
    match args {
        [sub, rest @ ..] if rest.len() <= 2 => {
            let named: Vec<(&str, &Value)> = ["start", "end"].into_iter().zip(rest).collect();
            format!("write {}: the window is keyword-only", respell(who, &[sub], &named))
        }
        _ => format!("write `{who}(sub, start=…, end=…)`"),
    }
}

/// `replace(old, new, 2)` as `replace(old, new, count=2)`.
fn replace_spelling(args: &[Value]) -> String {
    match args {
        [old, new, count] => format!(
            "write {}: the count is keyword-only",
            respell("replace", &[old, new], &[("count", count)])
        ),
        _ => "write `replace(old, new, count=…)`".to_string(),
    }
}

/// `strip("xy")` as `strip(chars="xy")`.
fn strip_spelling(args: &[Value]) -> String {
    match args {
        [chars] => format!(
            "write {}: the character set is keyword-only",
            respell("strip", &[], &[("chars", chars)])
        ),
        _ => "write `strip(chars=…, side=…)`".to_string(),
    }
}

/// `split(",", 1)` as `split(sep=",", maxsplit=1)`, and `split(null, 1)` as
/// `split(maxsplit=1)`.
fn split_spelling(args: &[Value]) -> String {
    match args {
        [_] | [_, _] => {
            let named: Vec<(&str, &Value)> = ["sep", "maxsplit"].into_iter().zip(args).collect();
            format!(
                "write {}: the separator is keyword-only, and `split()` with none splits on \
                 runs of whitespace",
                respell("split", &[], &named)
            )
        }
        _ => "write `split(sep=…, maxsplit=…, side=…)`".to_string(),
    }
}

/// `range(2, 10, 3)` as `range(10, start=2, step=3)`. A start of 0 is the
/// default, so `range(0, n)` comes back as plain `range(n)`.
fn range_spelling(args: &[Value]) -> String {
    match args {
        [start, end, step @ ..] if step.len() <= 1 => {
            let mut named: Vec<(&str, &Value)> = Vec::new();
            if !matches!(start, Value::Int(0)) {
                named.push(("start", start));
            }
            if let [step] = step {
                named.push(("step", step));
            }
            format!(
                "write {}: the one positional argument is the end, and the start and step are \
                 keyword-only",
                respell("range", &[end], &named)
            )
        }
        _ => "write `range(end, start=…, step=…)`".to_string(),
    }
}

/// A plain builtin called with keyword arguments. `round` and `open` are the
/// two whose parameters have defaults, so they are the two that take one.
pub fn call_builtin_kw(
    name: &str,
    args: Vec<Value>,
    kwargs: &[(String, Value)],
) -> VResult<Value> {
    match name {
        "round" => round_with(args, kwargs),
        "open" => open_with(args, kwargs),
        "clamp" => clamp_with(args, kwargs),
        _ => Err(type_error(format!("{name}() takes no keyword arguments"))),
    }
}

/// `clamp(v, min=lo, max=hi)` — bound `v` below by `lo`, above by `hi`, either
/// or both. It reads as what it means ("floor at zero", "cap at 100") where
/// `max(v, 0)` makes the reader translate "the maximum of these two" into a
/// bound, and it replaces the backwards-writable nesting `min(max(v, 0), 100)`.
/// At least one bound is required — clamping to nothing is a no-op and almost
/// certainly a mistake.
fn clamp_with(args: Vec<Value>, kwargs: &[(String, Value)]) -> VResult<Value> {
    exactly(&args, 1, "clamp")?;
    let [lo, hi] = bind_kwargs("clamp", kwargs, ["min", "max"])?;
    if lo.is_none() && hi.is_none() {
        return Err(type_error(
            "clamp() needs at least one of min= or max= — clamp(v, min=0), clamp(v, max=100), \
             or both"
                .to_string(),
        ));
    }
    let mut v = args.into_iter().next().expect("one positional");
    // Floor first, then cap. `try_compare` answers `None` only for a receiver
    // whose ordering needs a user `__lt__` and so the VM — clamp is native, so
    // it declines that cleanly rather than leaking the internal sentinel. Bounds
    // are numbers in every real use; the collection reductions keep `__lt__`.
    if let Some(lo) = lo {
        match v.try_compare(lo, "<")? {
            Some(std::cmp::Ordering::Less) => v = lo.clone(),
            Some(_) => {}
            None => return Err(clamp_unorderable(&v, lo)),
        }
    }
    if let Some(hi) = hi {
        match v.try_compare(hi, ">")? {
            Some(std::cmp::Ordering::Greater) => v = hi.clone(),
            Some(_) => {}
            None => return Err(clamp_unorderable(&v, hi)),
        }
    }
    Ok(v)
}

fn clamp_unorderable(a: &Value, b: &Value) -> VErr {
    type_error(format!(
        "clamp() needs values it can order with `<`, not '{}' and '{}'",
        a.type_name(),
        b.type_name()
    ))
}

/// `clamp(v)` with no bounds — the no-keyword path lands here. Always an error:
/// a clamp with neither bound is a no-op nobody means.
fn bi_clamp(_args: Vec<Value>) -> VResult<Value> {
    Err(type_error(
        "clamp() needs at least one of min= or max= — clamp(v, min=0), clamp(v, max=100), \
         or both"
            .to_string(),
    ))
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
    open_with(args, &[])
}

/// `open(path, mode="r")`. The mode is a code word (`"r"`, `"w"`, `"a"`),
/// which is the kind of argument that is named.
fn open_with(args: Vec<Value>, kwargs: &[(String, Value)]) -> VResult<Value> {
    use crate::stream::OroStream;
    positional_at_most(&args, 1, "open", || match args.as_slice() {
        [path, mode] => format!(
            "write {}: the mode is keyword-only",
            respell("open", &[path], &[("mode", mode)])
        ),
        _ => "write `open(path, mode=…)`".to_string(),
    })?;
    let [mode] = bind_kwargs("open", kwargs, ["mode"])?;
    let path = match args.as_slice() {
        [Value::Str(p)] => p.s.clone(),
        [_] => return Err(type_error("open() arguments must be strings")),
        _ => return Err(type_error("open() missing its required argument: the path")),
    };
    let mode = match mode {
        None => "r".to_string(),
        Some(Value::Str(m)) => m.s.clone(),
        Some(Value::None) => return Err(null_is_not_omitted("open", "mode", "str")),
        Some(other) => {
            return Err(type_error(format!(
                "open(): mode= must be str, not '{}'",
                other.type_name()
            )))
        }
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

/// The message for a bare `min`/`max` used as a scalar call. Both are now
/// **only** collection reductions (`xs.min()` / `xs.max()`); the two-argument
/// scalar forms were cut in favour of `clamp`, which reads as the bound it is
/// (`clamp(v, min=0)` rather than `max(v, 0)`). Kept as a named message because
/// the chain-terminal ordering path (`ord_callback`) refers to it.
pub fn extreme_arity_message(who: &str) -> String {
    format!(
        "`{who}()` is a collection reduction in Oro — write `xs.{who}()`. For a bound, \
         `clamp(v, min=…, max=…)` reads as what it means; for the extreme of two values, \
         `[a, b].{who}()`."
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
/// (`sort`, `min`, `max`, and their chain spellings) for such an
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
/// reversing the result: `sort` is stable, so equal keys keep their original
/// order in both directions — which is what CPython guarantees. Reversing the
/// sorted output instead would flip ties and break that.
pub fn sort_by_keys(items: Vec<Value>, keys: &[Value], reverse: bool) -> VResult<Vec<Value>> {
    let perm = sort_permutation(keys, reverse)?;
    let mut items = items;
    apply_permutation(&mut items, perm);
    Ok(items)
}

/// The stable order of `keys`, as a permutation: entry `k` is the index of the
/// key that belongs at position `k`. `reverse` inverts the comparator, as in
/// [`sort_by_keys`], so ties keep their input order in both directions.
pub fn sort_permutation(keys: &[Value], reverse: bool) -> VResult<Vec<usize>> {
    let mut idx: Vec<usize> = (0..keys.len()).collect();
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
    match err {
        Some(e) => Err(e),
        None => Ok(idx),
    }
}

/// Reorder `items` where they are, so that position `k` ends up holding what
/// was at `perm[k]` — the undecorate half of a decorate-sort-undecorate, done
/// without a second vector of values. It follows each cycle of the permutation
/// with swaps, so every element moves once and nothing is cloned; `perm` is
/// consumed as the record of which positions are already settled.
///
/// `perm` must be a permutation of `0..items.len()`, which is what
/// [`sort_permutation`] and the VM's merge sort both produce.
pub fn apply_permutation(items: &mut [Value], mut perm: Vec<usize>) {
    debug_assert_eq!(items.len(), perm.len());
    for start in 0..perm.len() {
        let mut k = start;
        loop {
            let from = perm[k];
            // A settled position points at itself. The cycle through `start`
            // is closed once it leads back there: the value `start` held has
            // been carried along by the swaps and is already in place.
            perm[k] = k;
            if from == start || from == k {
                break;
            }
            items.swap(k, from);
            k = from;
        }
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
pub fn call_type(t: TypeTag, args: Vec<Value>, kwargs: &[(String, Value)]) -> VResult<Value> {
    match t {
        // `range(end, start=0, step=1)`. The one positional argument is always
        // the end, so `range(5)` reads as it always has, and the bounds that
        // the two- and three-argument forms used to tell apart by arity are
        // named instead.
        TypeTag::Range => {
            positional_at_most(&args, 1, "range", || range_spelling(&args))?;
            let Some(end) = args.first() else {
                return Err(type_error(
                    "range() missing its required argument: the end — `range(end, start=0, step=1)`",
                ));
            };
            let [start, step] = bind_kwargs("range", kwargs, ["start", "step"])?;
            let stop = as_i64(end)?;
            let start = kw_int("range", "start", start, 0)?;
            let step = kw_int("range", "step", step, 1)?;
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

fn bi_round(args: Vec<Value>) -> VResult<Value> {
    round_with(args, &[])
}

/// `round(x)` -> int, `round(x, ndigits=n)` -> float. Uses banker's rounding
/// (ties to even) exactly as CPython does: `round(0.5)` is 0 and `round(2.5)`
/// is 2. Omitting `ndigits=` is not the same as `ndigits=0` — that one answers
/// a float, as CPython's does — so the omission is a real third case, and an
/// explicit `null` is not a way to spell it.
fn round_with(args: Vec<Value>, kwargs: &[(String, Value)]) -> VResult<Value> {
    positional_at_most(&args, 1, "round", || match args.as_slice() {
        [x, n] => format!(
            "write {}: the precision is keyword-only",
            respell("round", &[x], &[("ndigits", n)])
        ),
        _ => "write `round(x, ndigits=…)`".to_string(),
    })?;
    let [nd] = bind_kwargs("round", kwargs, ["ndigits"])?;
    let Some(v) = args.first() else {
        return Err(type_error("round() missing its required argument: the number"));
    };
    let ndigits = match nd {
        None => None,
        given => Some(kw_int("round", "ndigits", given, 0)?),
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

/// The fifteen names on `str` — and, plus `hex` and `scan`, on `bytes`. One
/// list, so the two types can never drift apart.
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
            | "replace"
            | "is_digit"
            | "is_alpha"
            | "is_alnum"
            | "is_space"
    )
}

/// A global that used to exist and no longer does.
///
/// **A builtin takes scalars; a collection method takes a collection.** That
/// one sentence is why these six are gone: each was the prefix spelling of a
/// method that does the same job on the same argument, usage across the tree
/// was split roughly down the middle, and the split had already let the two
/// halves *disagree* three times — `zip` silently dropping a sequence,
/// `sorted` answering the wrong type, `enumerate` ignoring an argument. Two
/// bodies for one operation is how that happens; one body is how it stops, and
/// one spelling is how it stays stopped.
///
/// `len` is the single exception and the README says why: it reaches `str` and
/// `bytes`, which are deliberately outside the collection protocol, and it is
/// the dispatch point for `__len__` on a user class. `min` and `max` are
/// narrowed rather than cut — the variadic *scalar* form `min(a, b)` has no
/// chain spelling — and their single-iterable form answers with the message
/// below.
pub fn cut_global_message(name: &str) -> Option<&'static str> {
    Some(match name {
        "sum" => {
            "`sum` is not defined in Oro — a builtin takes scalars and a collection method \
             takes a collection: use `xs.sum()`"
        }
        "sorted" => {
            "`sorted` is not defined in Oro — a builtin takes scalars and a collection \
             method takes a collection: use `xs.sort(x => x)`, or \
             `xs.sort(f, reverse=true)` with a key"
        }
        "any" => {
            "`any` is not defined in Oro — a builtin takes scalars and a collection method \
             takes a collection: use `xs.any(p)`, or `xs.any(x => x)` for truthiness"
        }
        "all" => {
            "`all` is not defined in Oro — a builtin takes scalars and a collection method \
             takes a collection: use `xs.all(p)`, or `xs.all(x => x)` for truthiness"
        }
        "enumerate" => {
            "`enumerate` is not in Oro — every `for` yields (index, value), so write \
             `for i, x in xs` (and `for _, x in xs` when the index is unused)"
        }
        "zip" => {
            "`zip` is not defined in Oro — a builtin takes scalars and a collection method \
             takes a collection: use `a.zip(b)`, which takes any number of further \
             sequences"
        }
        "min" | "max" => {
            "`min`/`max` are collection reductions in Oro — `xs.min()` / `xs.max()`. The \
             two-argument scalar form is cut: use `clamp(v, min=…, max=…)` for a bound (which \
             reads as the bound it is, where `max(v, 0)` reads backwards), or `[a, b].min()` \
             for the extreme of two values"
        }
        _ => return None,
    })
}

/// A `str`/`bytes` method that used to exist and no longer does. Removals name
/// their replacement — silently answering "no such attribute" would leave the
/// reader to guess what happened to it.
pub fn cut_method_message(recv: &Value, name: &str) -> Option<&'static str> {
    if let Some(msg) = cut_sort_message(recv, name) {
        return Some(msg);
    }
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
    // `enumerate` went the way `range(len(x))` did: every `for` now yields
    // (index, value), so a separate index-pairing step has no job.
    if is_collection(recv) && name == "enumerate" {
        return Some(
            "`enumerate` is not in Oro — every `for` yields (index, value), so write \
             `for i, x in xs` (and `for _, x in xs` when the index is unused)",
        );
    }
    if !matches!(recv, Value::Str(_) | Value::Bytes(_)) {
        return None;
    }
    if let Some(msg) = cut_str_method_message(recv, name) {
        return Some(msg);
    }
    // A collection-protocol name on a `str` or `bytes`. Neither type is in the
    // protocol — `"abc".map(f)` has always been an AttributeError — but the
    // `sorted`/`min`/`max`/`sum`/`any`/`all`/`enumerate`/`zip` builtins used to
    // reach both types through the *iteration* protocol, so the habit is real
    // and a bare "no such attribute" would leave the reader to invent the
    // bridge. `len` is the one of the nine that stayed a builtin, so it gets
    // its own line rather than being pointed at `to_list()`.
    if name == "len" {
        return Some(
            "`len` is a builtin in Oro and not a method on `str`/`bytes` — write `len(s)`",
        );
    }
    if is_seq_native(name) || crate::vm::is_seq_op(name) || name == "sorted" {
        return Some(if matches!(recv, Value::Str(_)) {
            "a `str` is not a collection in Oro — `s.to_list()` is the bridge into the \
             collection protocol, so write `s.to_list().sort(x => x)`"
        } else {
            "a `bytes` is not a collection in Oro — `b.to_list()` is the bridge into the \
             collection protocol, so write `b.to_list().sort(x => x)`"
        });
    }
    None
}

/// Sorting is one spelling now: `xs.sort(f, reverse=)`, which returns a new
/// collection. There is no in-place sort and no `sorted` builtin — every
/// collection operation returns a new collection, so the aliasing bug where
/// `b = a; a.sort()` silently reorders what `b` sees cannot be written. The
/// function is the operand, so it is the positional argument, as for `min_by`
/// and `group_by`. `reversed` is likewise `reverse` now, also a new collection.
fn cut_sort_message(recv: &Value, name: &str) -> Option<&'static str> {
    if !is_collection(recv) {
        return None;
    }
    Some(match name {
        "sorted" => {
            "`sorted` is not in Oro — sort with `xs.sort(f)`: `xs.sort(x => x)` for the \
             elements' own order, `xs.sort(f, reverse=true)` for a stable descending one"
        }
        "sort_by" => {
            "`sort_by` is spelled `sort` in Oro — `xs.sort(f)`, or `xs.sort(x => x)` for \
             the elements' own order"
        }
        "sort_in_place" => {
            "`sort_in_place` is not in Oro — every collection operation returns a new \
             collection, so sorting is `xs.sort(f)` and you rebind: `xs = xs.sort(f)`"
        }
        "reversed" => {
            "`reversed` is spelled `reverse` in Oro — `xs.reverse()`, which returns a new \
             collection (there is no in-place reverse)"
        }
        _ => return None,
    })
}

/// The `str`/`bytes` names that were removed by name, each pointing at the one
/// that replaced it.
fn cut_str_method_message(recv: &Value, name: &str) -> Option<&'static str> {
    Some(match name {
        "lstrip" => "`lstrip` is not in Oro — use `strip(side=\"left\")`",
        "rstrip" => "`rstrip` is not in Oro — use `strip(side=\"right\")`",
        "rsplit" => {
            "`rsplit` is not in Oro — use `split(sep=…, maxsplit=…, side=\"right\")`"
        }
        "rfind" => "`rfind` is not in Oro — use `find(sub, reverse=true)`",
        "index" => "`index` is not in Oro — use `find(sub)`, which answers -1 rather than raising",
        "zfill" => {
            "`zfill` is not in Oro — a format spec pads: f\"{n:05d}\", f\"{s:0>5}\", \
             or f\"{s:0>{width}}\" for a width computed at run time"
        }
        // `sep.join(xs)` and `xs.join(sep)` were byte-for-byte the same
        // operation with the same type rules, and the README already argued
        // for the second one — "the sequence is the subject and the separator
        // the detail" — against a spelling it still shipped. The collection
        // form is the half that chains, so it is the half that stays.
        "join" if matches!(recv, Value::Str(_)) => {
            "`str.join` is not in Oro — the separator is the argument and the sequence \
             the receiver: use `xs.join(sep)`"
        }
        "join" => {
            "`bytes.join` is not in Oro — the separator is the argument and the sequence \
             the receiver: use `xs.join(sep)`"
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
        // The same fifteen names `str` carries, plus `hex` and `scan`, which
        // only bytes needs.
        Value::Bytes(_) => is_str_method(name) || matches!(name, "hex" | "scan"),
        Value::List(_) => {
            matches!(name, "append" | "pop" | "extend" | "map" | "filter")
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

/// The native methods that take a keyword. Under the argument rule these are
/// exactly the ones with a defaulted parameter: on `str` and `bytes`, the
/// search window (`start=`, `end=`), `find(reverse=)`, `replace(count=)`,
/// `strip(chars=, side=)` and `split(sep=, maxsplit=, side=)`; `to_int(base=)`
/// on any value; `list.pop(index=)`; and `dict.get`/`dict.pop(default=)`.
/// Everything else refuses one, so a misplaced keyword is an error rather than
/// a silently discarded argument.
fn takes_kwargs(recv: &Value, name: &str) -> bool {
    match recv {
        _ if name == "to_int" => true,
        _ if is_collection(recv) && name == "sum" => true,
        Value::Str(_) | Value::Bytes(_) => matches!(
            name,
            "strip" | "split" | "find" | "count" | "startswith" | "endswith" | "replace"
        ),
        Value::List(_) => name == "pop",
        Value::Dict(_) => matches!(name, "get" | "pop"),
        _ => false,
    }
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
        _ if is_cast_method(name) => cast_method(recv, name, args, &kwargs),
        _ if is_collection(recv) && is_seq_native(name) => {
            seq_native_method(recv, name, args, &kwargs)
        }
        Value::Str(_) => str_method(recv, name, args, &kwargs),
        Value::Bytes(b) => bytes_method(b, name, args, &kwargs),
        Value::List(l) => list_method(l, name, args, &kwargs),
        Value::Dict(d) => dict_method(d, name, args, &kwargs),
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
    let (n, missing) = match name {
        "search" | "fullmatch" | "findall" | "finditer" => (1, rx::POS),
        "split" => (1, "CPython's maxsplit is not supported"),
        "sub" => (2, "CPython's count is not supported"),
        // Not a method at all: the last arm below says so.
        _ => (usize::MAX, ""),
    };
    rx::no_extra(&args, n, "Pattern", name, missing)?;
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
    // The group index is required, so `m.group(0)` says "the whole match".
    let n = match args.as_slice() {
        [Value::Int(i)] if *i >= 0 => *i as usize,
        [Value::Int(_)] => return Err(index_error("group index must be non-negative")),
        [] => {
            return Err(type_error(format!(
                "{name}() missing its group index — m.{name}(0) is the whole match"
            )))
        }
        _ => return Err(type_error(format!("{name}() takes one group index, an int"))),
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

/// The `[start, end)` window that `find`/`count`/`startswith`/`endswith`
/// search, read from their `start=` and `end=` keywords. `None` means the
/// window has negative width, in which case nothing matches — not even an
/// empty needle.
fn search_window(
    start: Option<&Value>,
    end: Option<&Value>,
    who: &str,
    len: i64,
) -> VResult<Option<(i64, i64)>> {
    let start = kw_int(who, "start", start, 0)?;
    let end = kw_int(who, "end", end, i64::MAX)?;
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

/// `strip`'s `side=`. Validated by name: a value
/// outside the three is a `ValueError` that *names the three*, because a strip
/// that silently did nothing is exactly the class of bug this language exists
/// to refuse.
fn strip_side(side: Option<&Value>) -> VResult<Side> {
    let Some(v) = side else {
        return Ok(Side::Both);
    };
    let Value::Str(s) = v else {
        return Err(type_error(format!("strip(): side must be str, not '{}'", v.type_name())));
    };
    Ok(match &*s.s {
        "both" => Side::Both,
        "left" => Side::Left,
        "right" => Side::Right,
        other => {
            return Err(value_error(format!(
                "strip(): side must be \"both\", \"left\" or \"right\", not {}",
                crate::value::repr_str(other)
            )))
        }
    })
}

/// `split`'s `side=`: which end `maxsplit=` counts its splits from.
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
/// reject `split(sep=s, side="right")` while waving through `split(sep=s,
/// maxsplit=-1, side="right")` and `split(sep=s, maxsplit=99, side="right")`,
/// which are equally inert.
/// A rule that catches one of its three cases is worse than no rule.
fn split_side(side: Option<&Value>) -> VResult<Side> {
    let Some(v) = side else {
        return Ok(Side::Left);
    };
    let Value::Str(s) = v else {
        return Err(type_error(format!("split(): side must be str, not '{}'", v.type_name())));
    };
    Ok(match &*s.s {
        "left" => Side::Left,
        "right" => Side::Right,
        other => {
            return Err(value_error(format!(
                "split(): side must be \"left\" or \"right\", not {}",
                crate::value::repr_str(other)
            )))
        }
    })
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

/// `find`'s `reverse=`: `true` asks for the last occurrence
/// rather than the first. Spelled the way `sorted(reverse=…)` already is.
/// A bool and nothing else: any truthy value used to count, so
/// `reverse="no"` searched from the end.
fn find_reverse(reverse: Option<&Value>) -> VResult<bool> {
    match reverse {
        None => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(Value::None) => Err(null_is_not_omitted("find", "reverse", "bool")),
        Some(other) => Err(type_error(format!(
            "find(): reverse= must be bool, not '{}'",
            other.type_name()
        ))),
    }
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

/// `str.split(maxsplit=n)`: runs of whitespace separate, leading and
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

fn cast_method(
    recv: &Value,
    name: &str,
    args: Vec<Value>,
    kwargs: &[(String, Value)],
) -> VResult<Value> {
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
                // A range is a sequence of ints too, so it builds bytes the same
                // way a list of those ints does: `range(68, start=65).to_bytes()`
                // is `[65, 66, 67].to_bytes()` is `b'ABC'`. Materialize it first,
                // exactly as `to_list` does, rather than refusing the receiver.
                Value::Range(_) => ints_to_bytes(&crate::vm::iterate_to_vec(recv)?),
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
            // `to_int(base=16)` for strings, mirroring CPython's
            // `int(s, base=16)`. It takes nothing else: extra arguments used to
            // be read past, so `"10".to_int(16, 2)` answered 16.
            positional_at_most(&args, 0, "to_int", || match args.as_slice() {
                [base] => format!(
                    "write {}: the base is keyword-only",
                    respell("to_int", &[], &[("base", base)])
                ),
                _ => "write `to_int(base=…)`".to_string(),
            })?;
            let [base] = bind_kwargs("to_int", kwargs, ["base"])?;
            let base = match base {
                None => None,
                given => Some(kw_int("to_int", "base", given, 10)?),
            };
            match recv {
                // A number has no digits to read in a base, so a base given to
                // one is a mistake — which used to be quietly ignored, so that
                // `(5).to_int(base=16)` answered 5. CPython refuses it too.
                Value::Bool(_) | Value::Int(_) | Value::Big(_) | Value::Float(_)
                    if base.is_some() =>
                {
                    Err(type_error(format!(
                        "to_int(): base= only applies to a str — a '{}' has no digits to read \
                         in a base",
                        recv.type_name()
                    )))
                }
                Value::Bool(b) => Ok(Value::Int(*b as i64)),
                Value::Int(_) | Value::Big(_) => Ok(recv.clone()),
                Value::Float(f) => Ok(float_to_int(*f)),
                Value::Str(s) => match base {
                    None => parse_int_str(&s.s),
                    Some(base) => parse_int_base(&s.s, base),
                },
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
            | "flatten" | "chunk" | "zip" | "join" | "reverse" | "len"
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

/// `start=`, the one option `sum` and `enumerate` take. It is keyword-only: a
/// positional argument is the old spelling, and `xs.enumerate(1)` read like a
/// count. `start=null` is refused rather than read as "leave it out", because
/// leaving it out is how that is said.
fn start_kwarg<'a>(
    args: &[Value],
    kwargs: &'a [(String, Value)],
    who: &str,
) -> VResult<Option<&'a Value>> {
    if !args.is_empty() {
        return Err(type_error(format!(
            "{who}() takes no positional arguments — the start is named: `xs.{who}(start=n)`"
        )));
    }
    let mut start = None;
    for (k, v) in kwargs {
        match (k.as_str(), v) {
            ("start", Value::None) => {
                return Err(type_error(format!(
                    "{who}() start must not be null — leave `start=` out for the default of 0"
                )))
            }
            ("start", v) => start = Some(v),
            (other, _) => {
                return Err(type_error(format!(
                    "{who}() got an unexpected keyword argument '{other}'"
                )))
            }
        }
    }
    Ok(start)
}

/// The count `take`, `drop` and `chunk` require: exactly one argument, and an
/// int. There is no default to fall back on, so a missing count is an arity
/// error rather than a bad value, and a second argument is refused rather than
/// ignored. A `bool` is not a count; it is refused here, as it always was by
/// the fused `take(n)` that ends a chain, so the two paths cannot disagree.
fn count_arg(args: &[Value], who: &str) -> VResult<i64> {
    exactly(args, 1, who)?;
    match &args[0] {
        Value::Int(n) => Ok(*n),
        other => Err(type_error(format!(
            "{who}() argument must be int, not '{}'",
            other.type_name()
        ))),
    }
}

fn seq_native_method(
    recv: &Value,
    name: &str,
    args: Vec<Value>,
    kwargs: &[(String, Value)],
) -> VResult<Value> {
    let (shape, items) = seq_parts(recv, name)?;
    match name {
        "len" => {
            exactly(&args, 0, "len")?;
            Ok(Value::Int(items.len() as i64))
        }
        "first" | "last" => {
            exactly(&args, 0, name)?;
            // `first()`/`last()` **ask**: an empty receiver answers `null`, the
            // same way `d.get(k)` answers `null` for a missing key. Indexing
            // (`xs[0]`/`xs[-1]`) **demands** and raises `IndexError` on an empty
            // receiver, the way `d[k]` raises `KeyError`. The two spellings
            // answer different questions — "is there one?" versus "give me the
            // one that must be there" — so neither is a rename of the other.
            // A receiver whose first/last element genuinely *is* `null` is
            // indistinguishable from an empty one here, exactly as it is for
            // `get`; that is the price of the asking form and it is the same
            // price in both places.
            let pick = if name == "first" { items.first() } else { items.last() };
            Ok(pick.cloned().unwrap_or(Value::None))
        }
        "sum" => {
            // The whole of what the `sum` builtin used to be, now that the
            // builtin is cut: a fold with `+`, starting at 0, so an empty
            // sequence sums to 0 and a list of strings is a TypeError rather
            // than a concatenation.
            let start = start_kwarg(&args, kwargs, "sum")?;
            let mut acc = start.cloned().unwrap_or(Value::Int(0));
            for v in items {
                acc = crate::vm::add_values(&acc, &v)?;
            }
            Ok(acc)
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
        "reverse" => {
            exactly(&args, 0, "reverse")?;
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
            let n = count_arg(&args, name)?;
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
            let n = count_arg(&args, "chunk")?;
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
        "join" => {
            // The only join in the language: `", ".join(xs)` is cut, because
            // the separator is the detail and the sequence is the subject, and
            // this way the call ends a chain instead of forcing the reader back
            // to the front of the line.
            // The separator's type decides the result's, and the elements
            // must match it: `join` never guesses a conversion, the same rule
            // `json.stringify` applies to dict keys.
            //
            // One argument, and only one. `xs.join(sep, 2)` used to answer
            // confidently having ignored the 2 — the same drift `enumerate`
            // had, found when `str.join`'s arity probes moved over here and
            // this side turned out not to have any.
            exactly(&args, 1, "join")?;
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
            positional_at_most(&args, 0, name, || strip_spelling(&args))?;
            let [chars, side] = bind_kwargs(name, kwargs, ["chars", "side"])?;
            let side = strip_side(side)?;
            // `chars=` is a *set* of characters to remove from the end(s), not
            // a prefix or a suffix: `"xyx".strip(chars="xy")` is `""`. That is
            // the footgun `rm_prefix`/`rm_suffix` exist to answer. Omitted,
            // whitespace is stripped instead.
            let trimmed = match chars {
                None => trim_with(s, side, is_py_space),
                Some(Value::Str(set)) => trim_with(s, side, |c| set.s.contains(c)),
                Some(Value::None) => return Err(null_is_not_omitted(name, "chars", "str")),
                Some(other) => {
                    return Err(type_error(format!(
                        "{name}(): chars= must be str, not '{}'",
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
            positional_at_most(&args, 1, "count", || window_spelling("count", &args))?;
            let [start, end] = bind_kwargs("count", kwargs, ["start", "end"])?;
            let sub = str_arg(&args, 0, "count")?;
            let len = os.char_len() as i64;
            // The same window `find` searches, read the same way — `count` is
            // "how many times", `find` is "where", and asking them over
            // different regions of the same string would be the asymmetry this
            // surface exists to not have.
            let Some((start, end)) = search_window(start, end, "count", len)? else {
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
            positional_at_most(&args, 1, name, || window_spelling(name, &args))?;
            let [start, end] = bind_kwargs(name, kwargs, ["start", "end"])?;
            let affix = str_arg(&args, 0, name)?;
            let len = os.char_len() as i64;
            let Some((start, end)) = search_window(start, end, name, len)? else {
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
            positional_at_most(&args, 1, "find", || window_spelling("find", &args))?;
            let [start, end, reverse] = bind_kwargs("find", kwargs, ["start", "end", "reverse"])?;
            let reverse = find_reverse(reverse)?;
            let needle = str_arg(&args, 0, "find")?;
            let len = os.char_len() as i64;
            let Some((start, end)) = search_window(start, end, "find", len)? else {
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
            positional_at_most(&args, 2, "replace", || replace_spelling(&args))?;
            let [count] = bind_kwargs("replace", kwargs, ["count"])?;
            let from = str_arg(&args, 0, "replace")?;
            let to = str_arg(&args, 1, "replace")?;
            // A negative count means "every occurrence", which is the default.
            let count = kw_int("replace", "count", count, -1)?;
            if count < 0 {
                return Ok(Value::str(s.replace(&from, &to)));
            }
            Ok(Value::str(s.replacen(&from, &to, count as usize)))
        }
        "split" => {
            positional_at_most(&args, 0, name, || split_spelling(&args))?;
            let [sep, maxsplit, side] = bind_kwargs(name, kwargs, ["sep", "maxsplit", "side"])?;
            // maxsplit < 0 (the default) means "no limit"; maxsplit == n caps the
            // number of *splits*, so at most n + 1 pieces come back. `side`
            // picks the end those n splits are counted from, and is what
            // `rsplit` used to be.
            let side = split_side(side)?;
            let maxsplit = kw_int(name, "maxsplit", maxsplit, -1)?;
            // Two algorithms, and the keyword is what picks one: with no `sep=`
            // runs of whitespace separate and the ends are dropped; with one,
            // every occurrence of that literal separates. No value of `sep`
            // means "whitespace", so `null` is not one either.
            let parts: Vec<String> = match sep {
                None => split_whitespace_n(s, maxsplit, side),
                Some(Value::Str(sep)) => {
                    if sep.s.is_empty() {
                        return Err(value_error("empty separator"));
                    }
                    split_sep_n(s, &sep.s, maxsplit, side)
                }
                Some(Value::None) => return Err(null_is_not_omitted(name, "sep", "str")),
                Some(other) => {
                    return Err(type_error(format!(
                        "{name}(): sep= must be str, not '{}'",
                        other.type_name()
                    )))
                }
            };
            let parts = parts.into_iter().map(Value::str).collect::<Vec<_>>();
            Ok(Value::List(OroList::new(parts)))
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

/// `bytes.split(maxsplit=n)` — the byte-level twin of
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
            positional_at_most(&args, 0, name, || strip_spelling(&args))?;
            let [chars, side] = bind_kwargs(name, kwargs, ["chars", "side"])?;
            let side = strip_side(side)?;
            // As for `str`, `chars=` is a *set* of octets to remove from the
            // end(s); omitted, whitespace is removed instead.
            let cut: Box<dyn Fn(u8) -> bool> = match chars {
                None => Box::new(is_bytes_space),
                Some(Value::Bytes(set)) => {
                    let set = set.clone();
                    Box::new(move |x| set.contains(&x))
                }
                Some(Value::None) => return Err(null_is_not_omitted(name, "chars", "bytes")),
                Some(other) => {
                    return Err(type_error(format!(
                        "{name}(): chars= must be bytes, not '{}'",
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
            positional_at_most(&args, 1, "count", || window_spelling("count", &args))?;
            let [start, end] = bind_kwargs("count", kwargs, ["start", "end"])?;
            let sub = bytes_arg(&args, 0, "count")?;
            let Some((start, end)) = search_window(start, end, "count", b.len() as i64)? else {
                return Ok(Value::Int(0));
            };
            Ok(Value::Int(count_bytes(&b[start as usize..end as usize], &sub)))
        }
        "is_digit" | "is_alpha" | "is_alnum" | "is_space" => {
            exactly(&args, 0, name)?;
            Ok(Value::Bool(bytes_is_class(b, name)))
        }
        "startswith" | "endswith" => {
            positional_at_most(&args, 1, name, || window_spelling(name, &args))?;
            let [start, end] = bind_kwargs(name, kwargs, ["start", "end"])?;
            let affix = bytes_arg(&args, 0, name)?;
            let Some((start, end)) = search_window(start, end, name, b.len() as i64)? else {
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
            positional_at_most(&args, 1, "find", || window_spelling("find", &args))?;
            let [start, end, reverse] = bind_kwargs("find", kwargs, ["start", "end", "reverse"])?;
            let reverse = find_reverse(reverse)?;
            let needle = bytes_arg(&args, 0, "find")?;
            let Some((start, end)) = search_window(start, end, "find", b.len() as i64)? else {
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
            positional_at_most(&args, 2, "replace", || replace_spelling(&args))?;
            let [count] = bind_kwargs("replace", kwargs, ["count"])?;
            let from = bytes_arg(&args, 0, "replace")?;
            let to = bytes_arg(&args, 1, "replace")?;
            let count = kw_int("replace", "count", count, -1)?;
            Ok(Value::bytes(bytes_replace(b, &from, &to, count)))
        }
        "split" => {
            positional_at_most(&args, 0, name, || split_spelling(&args))?;
            let [sep, maxsplit, side] = bind_kwargs(name, kwargs, ["sep", "maxsplit", "side"])?;
            let side = split_side(side)?;
            let maxsplit = kw_int(name, "maxsplit", maxsplit, -1)?;
            let parts: Vec<Vec<u8>> = match sep {
                None => split_space_bytes(b, maxsplit, side),
                Some(Value::Bytes(sep)) => {
                    if sep.is_empty() {
                        return Err(value_error("empty separator"));
                    }
                    split_sep_bytes(b, sep, maxsplit, side)
                }
                Some(Value::None) => return Err(null_is_not_omitted(name, "sep", "bytes")),
                Some(other) => {
                    return Err(type_error(format!(
                        "{name}(): sep= must be bytes, not '{}'",
                        other.type_name()
                    )))
                }
            };
            let parts = parts.into_iter().map(Value::bytes).collect::<Vec<_>>();
            Ok(Value::List(OroList::new(parts)))
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

fn list_method(
    l: &Rc<OroList>,
    name: &str,
    args: Vec<Value>,
    kwargs: &[(String, Value)],
) -> VResult<Value> {
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
        // `xs.pop(index=-1)`. The index is named so that a positional
        // argument to `pop` only ever means one thing — a dict key.
        "pop" => {
            positional_at_most(&args, 0, "pop", || match args.as_slice() {
                [i] => format!(
                    "write {}: on a list the index is keyword-only, so that a positional \
                     argument to `pop` only ever means a dict key",
                    respell("pop", &[], &[("index", i)])
                ),
                _ => "write `pop(index=…)`".to_string(),
            })?;
            let [index] = bind_kwargs("pop", kwargs, ["index"])?;
            let i = kw_int("pop", "index", index, -1)?;
            let mut b = l.borrow_mut();
            // CPython checks for an empty list before it looks at the index.
            if b.is_empty() {
                return Err(index_error("pop from empty list"));
            }
            let adj = if i < 0 { i + b.len() as i64 } else { i };
            if adj < 0 || adj as usize >= b.len() {
                return Err(index_error("pop index out of range"));
            }
            Ok(b.remove(adj as usize))
        }
        _ => Err(attribute_error(format!("'list' object has no method '{name}'"))),
    }
}

fn dict_method(
    d: &Rc<RefCell<OroDict>>,
    name: &str,
    args: Vec<Value>,
    kwargs: &[(String, Value)],
) -> VResult<Value> {
    match name {
        // `d.get(k, default=null)`. Here `null` is the default, so an explicit
        // `default=null` is simply that value, not a refusal.
        "get" => {
            positional_at_most(&args, 1, "get", || match args.as_slice() {
                [k, def] => format!(
                    "write `get({}, default={})`: the fallback is keyword-only",
                    lit(k),
                    lit(def)
                ),
                _ => "write `get(key, default=…)`".to_string(),
            })?;
            let [default] = bind_kwargs("get", kwargs, ["default"])?;
            let Some(key) = args.first() else {
                return Err(type_error("get() missing its required argument: the key"));
            };
            Ok(d.borrow().get(key)?.unwrap_or_else(|| default.cloned().unwrap_or(Value::None)))
        }
        // The removal, and the reason `del` could be cut: `d.pop(k)` raises
        // `KeyError` when the key is absent, `d.pop(k, default=v)` answers `v`.
        // Giving `default=` at all is what turns the raise off, so
        // `default=null` is a real default that answers `null` — the one
        // keyword in this file where an explicit `null` is a value.
        "pop" => {
            positional_at_most(&args, 1, "pop", || match args.as_slice() {
                [k, def] => format!(
                    "write `pop({}, default={})`: the fallback is keyword-only, and without it \
                     a missing key raises KeyError",
                    lit(k),
                    lit(def)
                ),
                _ => "write `pop(key)`, or `pop(key, default=…)`".to_string(),
            })?;
            let [default] = bind_kwargs("pop", kwargs, ["default"])?;
            let Some(key) = args.first() else {
                return Err(type_error("pop() missing its required argument: the key"));
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

