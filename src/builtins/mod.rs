//! Builtin functions and type methods.
//!
//! Builtins are plain `fn(Vec<Value>) -> VResult<Value>` pointers wrapped in a
//! [`Value::Builtin`]; the VM invokes them natively and, crucially, none of them
//! ever calls back into Oro, so the single flat interpreter loop is never
//! re-entered. Type methods (string and a few container helpers) are dispatched
//! by name through [`call_method`].

use std::cell::RefCell;
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

fn bi_str(args: Vec<Value>) -> VResult<Value> {
    match args.as_slice() {
        [] => Ok(Value::str(String::new())),
        [v] => Ok(Value::str(v.display())),
        _ => Err("str() takes at most 1 argument".to_string()),
    }
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
    use std::io::{BufReader, BufWriter};
    let (path, mode) = match args.as_slice() {
        [Value::Str(p)] => (p.s.clone(), "r".to_string()),
        [Value::Str(p), Value::Str(m)] => (p.s.clone(), m.s.clone()),
        [_] | [_, _] => return Err("open() arguments must be strings".to_string()),
        _ => return Err("open() takes 1 or 2 arguments".to_string()),
    };
    let io_err = |e: &std::io::Error| crate::vm::modules::io_err(e, &path);
    let mut file = crate::value::OroFile { path: path.clone(), reader: None, writer: None, closed: false };
    match mode.as_str() {
        "r" => {
            let f = std::fs::File::open(&path).map_err(|e| io_err(&e))?;
            file.reader = Some(BufReader::new(f));
        }
        "w" => {
            let f = std::fs::File::create(&path).map_err(|e| io_err(&e))?;
            file.writer = Some(BufWriter::new(f));
        }
        "a" => {
            let f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| io_err(&e))?;
            file.writer = Some(BufWriter::new(f));
        }
        other => return Err(format!("invalid file mode '{other}' (use 'r', 'w', or 'a')")),
    }
    Ok(Value::File(Rc::new(RefCell::new(file))))
}

fn bi_int(args: Vec<Value>) -> VResult<Value> {
    match args.as_slice() {
        [] => Ok(Value::Int(0)),
        [v] => match v {
            Value::Bool(b) => Ok(Value::Int(*b as i64)),
            Value::Int(_) | Value::Big(_) => Ok(v.clone()),
            Value::Float(f) => Ok(float_to_int(*f)),
            Value::Str(s) => parse_int_str(&s.s),
            other => Err(format!("int() argument must be a number or string, not '{}'", other.type_name())),
        },
        _ => Err("int() takes at most 1 argument".to_string()),
    }
}

fn bi_float(args: Vec<Value>) -> VResult<Value> {
    match args.as_slice() {
        [] => Ok(Value::Float(0.0)),
        [v] => match v {
            Value::Bool(b) => Ok(Value::Float(*b as i64 as f64)),
            Value::Int(i) => Ok(Value::Float(*i as f64)),
            Value::Big(b) => Ok(Value::Float(b.to_f64())),
            Value::Float(_) => Ok(v.clone()),
            Value::Str(s) => s
                .s
                .trim()
                .parse::<f64>()
                .map(Value::Float)
                .map_err(|_| format!("could not convert string to float: '{}'", s.s)),
            other => Err(format!("float() argument must be a number or string, not '{}'", other.type_name())),
        },
        _ => Err("float() takes at most 1 argument".to_string()),
    }
}

fn bi_bool(args: Vec<Value>) -> VResult<Value> {
    match args.as_slice() {
        [] => Ok(Value::Bool(false)),
        [v] => Ok(Value::Bool(v.truthy())),
        _ => Err("bool() takes at most 1 argument".to_string()),
    }
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
pub fn method_exists(recv: &Value, name: &str) -> bool {
    match recv {
        Value::Str(_) => matches!(
            name,
            "split" | "join" | "strip" | "lstrip" | "rstrip" | "upper" | "lower"
                | "replace" | "startswith" | "endswith" | "find"
        ),
        Value::List(_) => matches!(name, "append" | "pop" | "extend" | "sort" | "reverse"),
        Value::Dict(_) => matches!(name, "get" | "keys" | "values" | "items"),
        Value::File(_) => matches!(
            name,
            "read" | "readline" | "readlines" | "write" | "close"
        ),
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
        Value::Str(_) => str_method(recv, name, args),
        Value::List(l) => list_method(l, name, args),
        Value::Dict(d) => dict_method(d, name, args),
        Value::File(f) => file_method(f, name, args),
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

fn file_method(
    f: &Rc<RefCell<crate::value::OroFile>>,
    name: &str,
    args: Vec<Value>,
) -> VResult<Value> {
    use std::io::{BufRead, Read, Write};
    let mut file = f.borrow_mut();
    if file.closed && name != "close" {
        return Err("I/O operation on closed file".to_string());
    }
    match name {
        "read" => {
            exactly(&args, 0, "read")?;
            let reader = file.reader.as_mut().ok_or("file not open for reading")?;
            let mut s = String::new();
            reader.read_to_string(&mut s).map_err(|e| e.to_string())?;
            Ok(Value::str(s))
        }
        "readline" => {
            exactly(&args, 0, "readline")?;
            let reader = file.reader.as_mut().ok_or("file not open for reading")?;
            let mut s = String::new();
            reader.read_line(&mut s).map_err(|e| e.to_string())?;
            Ok(Value::str(s))
        }
        "readlines" => {
            exactly(&args, 0, "readlines")?;
            let reader = file.reader.as_mut().ok_or("file not open for reading")?;
            let mut out = Vec::new();
            loop {
                let mut s = String::new();
                if reader.read_line(&mut s).map_err(|e| e.to_string())? == 0 {
                    break;
                }
                out.push(Value::str(s));
            }
            Ok(Value::List(Rc::new(RefCell::new(out))))
        }
        "write" => {
            let text = str_arg(&args, 0, "write")?;
            let writer = file.writer.as_mut().ok_or("file not open for writing")?;
            writer.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
            Ok(Value::Int(text.chars().count() as i64))
        }
        "close" => {
            exactly(&args, 0, "close")?;
            // Dropping the buffered handles flushes and closes them.
            file.reader = None;
            file.writer = None;
            file.closed = true;
            Ok(Value::None)
        }
        _ => Err(format!("'file' object has no method '{name}'")),
    }
}

fn str_arg(args: &[Value], i: usize, who: &str) -> VResult<String> {
    match args.get(i) {
        Some(Value::Str(s)) => Ok(s.s.clone()),
        Some(other) => Err(format!("{who}() argument must be str, not '{}'", other.type_name())),
        None => Err(format!("{who}() missing a required argument")),
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
        "split" => {
            let parts: Vec<Value> = match args.first() {
                None | Some(Value::None) => {
                    s.split_whitespace().map(Value::str).collect()
                }
                Some(Value::Str(sep)) => {
                    if sep.s.is_empty() {
                        return Err("empty separator".to_string());
                    }
                    s.split(&sep.s).map(Value::str).collect()
                }
                Some(other) => {
                    return Err(format!(
                        "split() separator must be str, not '{}'",
                        other.type_name()
                    ))
                }
            };
            Ok(Value::List(Rc::new(RefCell::new(parts))))
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

