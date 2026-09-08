//! The Oro runtime value model.
//!
//! This implements architecture points 2, 4 and 5:
//!
//! * **Reference counting via `Rc`, not `Arc`** (point 2). Oro is single
//!   threaded; cloning a [`Value`] is a refcount bump, which is exactly Python's
//!   assignment semantics. Mutable containers add `RefCell` for interior
//!   mutability.
//! * **Integers are inline `i64`, promoted to a heap [`BigInt`] only on
//!   overflow** (point 4). The invariant maintained everywhere is that a
//!   [`Value::Big`] never holds a value that fits in `i64`; use
//!   [`Value::from_bigint`] to preserve it.
//! * **Strings carry an `is_ascii` flag computed once at creation** (point 5),
//!   so indexing and slicing are O(1) for the common ASCII case.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::rc::Rc;

use crate::bigint::BigInt;
use crate::compiler::CodeObject;

/// A short alias for the fallible results produced by value operations and
/// builtins. The message is a bare string; the VM decorates it with the source
/// position of the faulting instruction.
pub type VResult<T> = Result<T, String>;

/// A first-class Oro value.
///
/// All heap-backed variants hold an `Rc`, so `clone` is always cheap and models
/// Python reference semantics.
#[derive(Clone)]
pub enum Value {
    None,
    Bool(bool),
    /// An integer that fits in `i64` (the overwhelmingly common case).
    Int(i64),
    /// An integer too large for `i64`. Never holds an `i64`-representable value.
    Big(Rc<BigInt>),
    Float(f64),
    Str(Rc<OroStr>),
    List(Rc<RefCell<Vec<Value>>>),
    Tuple(Rc<Vec<Value>>),
    Dict(Rc<RefCell<OroDict>>),
    Range(Rc<RangeVal>),
    /// A live iterator produced by `GetIter`.
    Iter(Rc<RefCell<IterState>>),
    Func(Rc<Function>),
    Builtin(Rc<Builtin>),
    Method(Rc<BoundMethod>),
    Class(Rc<Class>),
    Instance(Rc<Instance>),
    Super(Rc<SuperProxy>),
    /// A module namespace (`sys`, `os`, `os.path`) — attribute access reads its
    /// members.
    Module(Rc<Module>),
    /// An open text file, refcounted so it closes deterministically when the
    /// last reference is dropped (Oro's answer to `with`).
    File(Rc<RefCell<OroFile>>),
    /// A generator: a suspended function activation, advanced by iteration. The
    /// concrete state lives in the VM (it holds a `Frame`), so this is an opaque
    /// handle here.
    Generator(Rc<RefCell<GenBox>>),
    /// A compiled regular expression (`re.compile`), backed by the linear-time
    /// `regex` engine.
    Regex(Rc<OroRegex>),
    /// A regex match, with group texts and their char-offset spans.
    Match(Rc<OroMatch>),
    /// Internal sentinel for a local/cell slot that has not been assigned yet.
    /// Never reachable by user code: reading it raises a clean runtime error.
    Unbound,
}

/// A UTF-8 string with a precomputed ASCII flag (architecture point 5).
pub struct OroStr {
    pub s: String,
    /// True when every byte is ASCII, so byte index == char index and slicing
    /// is O(1). Computed once at construction.
    pub is_ascii: bool,
}

impl OroStr {
    pub fn new(s: String) -> Rc<OroStr> {
        let is_ascii = s.is_ascii();
        Rc::new(OroStr { s, is_ascii })
    }

    /// The number of Unicode scalar values, O(1) for ASCII.
    pub fn char_len(&self) -> usize {
        if self.is_ascii {
            self.s.len()
        } else {
            self.s.chars().count()
        }
    }

    /// The `i`-th character as an owned `String`, O(1) for ASCII.
    pub fn char_at(&self, i: usize) -> Option<String> {
        if self.is_ascii {
            self.s.get(i..i + 1).map(|c| c.to_string())
        } else {
            self.s.chars().nth(i).map(|c| c.to_string())
        }
    }
}

/// A lazy integer range (`range(...)`), never materialised as a list.
pub struct RangeVal {
    pub start: i64,
    pub stop: i64,
    pub step: i64,
}

impl RangeVal {
    pub fn len(&self) -> usize {
        if self.step > 0 && self.stop > self.start {
            (((self.stop - self.start) as i128 + self.step as i128 - 1) / self.step as i128) as usize
        } else if self.step < 0 && self.stop < self.start {
            (((self.start - self.stop) as i128 + (-self.step as i128) - 1) / (-self.step as i128))
                as usize
        } else {
            0
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The state backing a live iterator. Each `ForIter` step advances it.
pub enum IterState {
    Range { cur: i64, stop: i64, step: i64 },
    /// Iterates by index, remembering the original length so a size change
    /// during iteration is reported as a clean error rather than silently
    /// skipping or panicking.
    List { list: Rc<RefCell<Vec<Value>>>, idx: usize, orig_len: usize },
    Tuple { tuple: Rc<Vec<Value>>, idx: usize },
    Str { chars: Vec<String>, idx: usize },
    /// Dict/set iteration works over a snapshot taken at `GetIter` time.
    Snapshot { items: Vec<Value>, idx: usize },
    /// Line-by-line iteration over an open file (`for line in f`).
    File { file: Rc<RefCell<OroFile>> },
}

/// A compiled Oro function together with its captured environment.
pub struct Function {
    pub code: Rc<CodeObject>,
    /// Default values for the trailing defaulted parameters, evaluated once when
    /// the `def` executes (Python semantics).
    pub defaults: Vec<Value>,
    /// Captured cells, one per entry in `code.freevars`, shared with the scope
    /// that defined this function.
    pub freevars: Vec<Rc<RefCell<Value>>>,
}

/// A native builtin function.
pub struct Builtin {
    pub name: &'static str,
    pub func: fn(Vec<Value>) -> VResult<Value>,
}

/// A method bound to a receiver. Either a native builtin method (dispatched by
/// name in `crate::builtins`, e.g. `"a,b".split` or `xs.append`) or an Oro
/// method defined on a user class.
pub struct BoundMethod {
    pub receiver: Value,
    pub kind: MethodKind,
}

#[derive(Clone)]
pub enum MethodKind {
    /// A builtin method, dispatched by name.
    Native(Rc<str>),
    /// An Oro method: `func` is called with `receiver` as its first (`self`)
    /// argument. `defclass` is the class the method is defined in, so that
    /// `super()` inside it searches from `defclass`'s base.
    User { func: Rc<Function>, defclass: Rc<Class> },
}

/// A user-defined class (single inheritance only).
pub struct Class {
    pub name: Rc<str>,
    pub base: Option<Rc<Class>>,
    /// Methods and class-level attributes, by name.
    pub members: RefCell<HashMap<String, Value>>,
    /// True when this class descends from `BaseException`. Such instances get
    /// native message storage/rendering and are what `raise`/`except` operate on.
    pub is_exception: bool,
}

impl Class {
    /// Find `name` in this class or its base chain, returning the member and the
    /// class it was found in (the latter fixes `super()`'s search origin).
    pub fn find(class: &Rc<Class>, name: &str) -> Option<(Value, Rc<Class>)> {
        let mut cur = Some(class.clone());
        while let Some(c) = cur {
            if let Some(v) = c.members.borrow().get(name) {
                return Some((v.clone(), c.clone()));
            }
            cur = c.base.clone();
        }
        None
    }

    /// True when `class` is `other` or a subclass of it (used by `isinstance`).
    pub fn is_subclass(class: &Rc<Class>, other: &Rc<Class>) -> bool {
        let mut cur = Some(class.clone());
        while let Some(c) = cur {
            if Rc::ptr_eq(&c, other) {
                return true;
            }
            cur = c.base.clone();
        }
        false
    }
}

/// An instance of a user class. Instance attributes live in `fields`.
pub struct Instance {
    pub class: Rc<Class>,
    pub fields: RefCell<HashMap<String, Value>>,
}

/// The state of a generator. Its suspended activation record is a VM `Frame`,
/// stored opaquely here (the VM downcasts it) so `value` need not know the
/// frame layout. `done` is set when the generator is exhausted.
pub struct GenBox {
    pub done: bool,
    /// The suspended `Frame`, or `None` while the generator is running or done.
    /// `yield` is statement-only in Oro, so resuming just continues the frame —
    /// no sent value to inject.
    pub frame: Option<Box<dyn std::any::Any>>,
}

/// A compiled regular expression.
pub struct OroRegex {
    pub re: regex::Regex,
    pub pattern: String,
}

/// A single regex match. Group 0 is the whole match; the rest are captures.
/// Each group is `None` if it did not participate, else its `(start, end)` as
/// character offsets plus the matched text.
pub struct OroMatch {
    pub groups: Vec<Option<(usize, usize, String)>>,
}

/// A module namespace: a fixed set of named members (functions, sub-modules,
/// or data like `sys.argv`).
pub struct Module {
    pub name: Rc<str>,
    pub members: RefCell<HashMap<String, Value>>,
}

/// An open text file. Reading and writing go through buffered handles; dropping
/// the last `Rc` flushes and closes it (see the `with`-free file lifetime).
pub struct OroFile {
    pub path: String,
    pub reader: Option<std::io::BufReader<std::fs::File>>,
    pub writer: Option<std::io::BufWriter<std::fs::File>>,
    pub closed: bool,
}

/// The proxy returned by `super()`: attribute access searches the method
/// resolution order starting *after* the defining class, but binds to the
/// original instance.
pub struct SuperProxy {
    /// Where to begin the search — the defining class's base.
    pub start: Option<Rc<Class>>,
    pub instance: Value,
}

/// An insertion-ordered dictionary. Order is preserved for iteration and repr,
/// matching modern Python.
#[derive(Default)]
pub struct OroDict {
    index: HashMap<HKey, usize>,
    /// `(key, value)` pairs in insertion order. The key `Value` is retained for
    /// iteration and repr.
    entries: Vec<(Value, Value)>,
}

impl OroDict {
    pub fn new() -> OroDict {
        OroDict::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn insert(&mut self, key: Value, value: Value) -> VResult<()> {
        let hk = HKey::from_value(&key)?;
        if let Some(&pos) = self.index.get(&hk) {
            self.entries[pos].1 = value;
        } else {
            self.index.insert(hk, self.entries.len());
            self.entries.push((key, value));
        }
        Ok(())
    }

    pub fn get(&self, key: &Value) -> VResult<Option<Value>> {
        let hk = HKey::from_value(key)?;
        Ok(self.index.get(&hk).map(|&pos| self.entries[pos].1.clone()))
    }

    pub fn contains(&self, key: &Value) -> VResult<bool> {
        HKey::from_value(key).map(|hk| self.index.contains_key(&hk))
    }

    pub fn keys(&self) -> Vec<Value> {
        self.entries.iter().map(|(k, _)| k.clone()).collect()
    }

    pub fn values(&self) -> Vec<Value> {
        self.entries.iter().map(|(_, v)| v.clone()).collect()
    }

    pub fn items(&self) -> &[(Value, Value)] {
        &self.entries
    }
}

/// A hashable projection of a [`Value`], used as a dict/set key.
///
/// Numeric keys are normalised so that `True`, `1` and `1.0` collide, matching
/// Python (`{1: "a", True: "b", 1.0: "c"}` has a single entry). Unhashable
/// values (list, dict, set, function, ...) produce an error.
#[derive(Clone, PartialEq, Eq, Hash)]
enum HKey {
    None,
    Int(i64),
    Big(BigInt),
    /// Only non-integral floats reach here (integral ones normalise to `Int`).
    Float(u64),
    Str(String),
    Tuple(Vec<HKey>),
}

impl HKey {
    fn from_value(v: &Value) -> VResult<HKey> {
        Ok(match v {
            Value::None => HKey::None,
            Value::Bool(b) => HKey::Int(if *b { 1 } else { 0 }),
            Value::Int(i) => HKey::Int(*i),
            Value::Big(b) => HKey::Big((**b).clone()),
            Value::Float(f) => {
                if f.is_finite() && f.fract() == 0.0 && *f >= i64::MIN as f64 && *f <= i64::MAX as f64
                {
                    HKey::Int(*f as i64)
                } else {
                    HKey::Float(f.to_bits())
                }
            }
            Value::Str(s) => HKey::Str(s.s.clone()),
            Value::Tuple(items) => {
                let mut parts = Vec::with_capacity(items.len());
                for it in items.iter() {
                    parts.push(HKey::from_value(it)?);
                }
                HKey::Tuple(parts)
            }
            other => {
                return Err(format!("unhashable type: '{}'", other.type_name()));
            }
        })
    }
}

impl Value {
    /// Build a string value, computing the ASCII flag once.
    pub fn str(s: impl Into<String>) -> Value {
        Value::Str(OroStr::new(s.into()))
    }

    /// Build an integer value from a `BigInt`, demoting to inline `Int` when it
    /// fits so the "`Big` is always out of `i64` range" invariant is preserved.
    pub fn from_bigint(b: BigInt) -> Value {
        match b.to_i64() {
            Some(i) => Value::Int(i),
            None => Value::Big(Rc::new(b)),
        }
    }

    /// Python truthiness (architecture point 7): `0`, `0.0`, `""`, `[]`, `{}`,
    /// `()`, empty set, `None`, `False` are falsy.
    pub fn truthy(&self) -> bool {
        match self {
            Value::None => false,
            Value::Bool(b) => *b,
            Value::Int(i) => *i != 0,
            Value::Big(_) => true, // never zero by invariant
            Value::Float(f) => *f != 0.0,
            Value::Str(s) => !s.s.is_empty(),
            Value::List(l) => !l.borrow().is_empty(),
            Value::Tuple(t) => !t.is_empty(),
            Value::Dict(d) => !d.borrow().is_empty(),
            Value::Range(r) => !r.is_empty(),
            Value::Iter(_) | Value::Func(_) | Value::Builtin(_) | Value::Method(_) => true,
            Value::Class(_) | Value::Super(_) => true,
            Value::Module(_) | Value::File(_) | Value::Generator(_) => true,
            Value::Regex(_) | Value::Match(_) => true,
            // An instance is truthy unless its class defines a falsy __len__;
            // the VM overrides this when a __len__/__bool__ dunder is present.
            Value::Instance(_) => true,
            Value::Unbound => false,
        }
    }

    /// The Python-style type name, as returned by `type(x)`.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::None => "NoneType",
            Value::Bool(_) => "bool",
            Value::Int(_) | Value::Big(_) => "int",
            Value::Float(_) => "float",
            Value::Str(_) => "str",
            Value::List(_) => "list",
            Value::Tuple(_) => "tuple",
            Value::Dict(_) => "dict",
            Value::Range(_) => "range",
            Value::Iter(_) => "iterator",
            Value::Func(_) => "function",
            Value::Builtin(_) => "builtin_function",
            Value::Method(_) => "method",
            Value::Class(_) => "type",
            Value::Instance(_) => "object",
            Value::Super(_) => "super",
            Value::Module(_) => "module",
            Value::File(_) => "file",
            Value::Generator(_) => "generator",
            Value::Regex(_) => "Pattern",
            Value::Match(_) => "Match",
            Value::Unbound => "unbound",
        }
    }

    /// Like [`type_name`](Self::type_name), but returns the actual class name for
    /// a user instance (`Point` rather than the generic `object`). Used where a
    /// diagnostic should match CPython, e.g. attribute errors.
    pub fn type_label(&self) -> String {
        match self {
            Value::Instance(i) => i.class.name.to_string(),
            Value::Class(c) => c.name.to_string(),
            other => other.type_name().to_string(),
        }
    }

    /// The `str()` form, used by `print` and string conversion. Containers show
    /// the repr of their elements.
    pub fn display(&self) -> String {
        match self {
            Value::Str(s) => s.s.clone(),
            // An exception's str() is its message (its single arg, or the args
            // tuple). A custom __str__ is handled by the VM before this point.
            Value::Instance(i) if i.class.is_exception => exception_message(i),
            _ => self.repr(),
        }
    }

    /// The `repr()` form: strings are quoted, everything else matches `str()`.
    pub fn repr(&self) -> String {
        match self {
            Value::None => "None".to_string(),
            Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Big(b) => b.to_string(),
            Value::Float(f) => format_float(*f),
            Value::Str(s) => repr_str(&s.s),
            Value::List(l) => {
                let mut out = String::from("[");
                for (i, v) in l.borrow().iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&v.repr());
                }
                out.push(']');
                out
            }
            Value::Tuple(t) => {
                let mut out = String::from("(");
                for (i, v) in t.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&v.repr());
                }
                // A one-element tuple prints as `(x,)`.
                if t.len() == 1 {
                    out.push(',');
                }
                out.push(')');
                out
            }
            Value::Dict(d) => {
                let d = d.borrow();
                let mut out = String::from("{");
                for (i, (k, v)) in d.items().iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    let _ = write!(out, "{}: {}", k.repr(), v.repr());
                }
                out.push('}');
                out
            }
            Value::Range(r) => {
                if r.step == 1 {
                    format!("range({}, {})", r.start, r.stop)
                } else {
                    format!("range({}, {}, {})", r.start, r.stop, r.step)
                }
            }
            Value::Iter(_) => "<iterator>".to_string(),
            Value::Func(f) => format!("<function {}>", f.code.name),
            Value::Builtin(b) => format!("<builtin {}>", b.name),
            Value::Method(_) => "<bound method>".to_string(),
            Value::Class(c) => format!("<class '{}'>", c.name),
            // An exception reprs as `Name(arg, ...)`, matching CPython.
            Value::Instance(i) if i.class.is_exception => exception_repr(i),
            // Default form only; a __repr__/__str__ dunder is applied by the VM
            // before this fallback is reached.
            Value::Instance(i) => format!("<{} object>", i.class.name),
            Value::Super(_) => "<super>".to_string(),
            Value::Generator(_) => "<generator>".to_string(),
            Value::Regex(r) => format!("re.compile({})", repr_str(&r.pattern)),
            Value::Match(m) => {
                let whole = m.groups.first().and_then(|g| g.as_ref());
                match whole {
                    Some((s, e, text)) => {
                        format!("<re.Match span=({s}, {e}), match={}>", repr_str(text))
                    }
                    None => "<re.Match>".to_string(),
                }
            }
            Value::Module(m) => format!("<module '{}'>", m.name),
            Value::File(f) => {
                let f = f.borrow();
                let mode = if f.writer.is_some() { "w" } else { "r" };
                format!("<file '{}' mode '{}'>", f.path, mode)
            }
            Value::Unbound => "<unbound>".to_string(),
        }
    }

    /// Structural equality as used by `==`, `!=`, `in`, and membership. Numbers
    /// compare across `bool`/`int`/`float`; unlike types are simply unequal.
    pub fn equals(&self, other: &Value) -> bool {
        if let (Some(a), Some(b)) = (self.as_number(), other.as_number()) {
            return a.equals(&b);
        }
        match (self, other) {
            (Value::None, Value::None) => true,
            (Value::Str(a), Value::Str(b)) => a.s == b.s,
            (Value::List(a), Value::List(b)) => seq_eq(&a.borrow(), &b.borrow()),
            (Value::Tuple(a), Value::Tuple(b)) => seq_eq(a, b),
            (Value::Dict(a), Value::Dict(b)) => {
                let (a, b) = (a.borrow(), b.borrow());
                a.len() == b.len()
                    && a.items().iter().all(|(k, v)| {
                        b.get(k).ok().flatten().map(|bv| bv.equals(v)).unwrap_or(false)
                    })
            }
            // Default identity equality; a __eq__ dunder, when present, is
            // dispatched by the VM before this fallback is used.
            (Value::Instance(a), Value::Instance(b)) => Rc::ptr_eq(a, b),
            (Value::Class(a), Value::Class(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }

    /// Ordering for `<`, `<=`, `>`, `>=`. Numbers order across numeric types;
    /// strings and equal-typed sequences order lexicographically. Anything else
    /// is a `TypeError`.
    pub fn compare(&self, other: &Value) -> VResult<std::cmp::Ordering> {
        if let (Some(a), Some(b)) = (self.as_number(), other.as_number()) {
            return a.compare(&b);
        }
        match (self, other) {
            (Value::Str(a), Value::Str(b)) => Ok(a.s.cmp(&b.s)),
            (Value::List(a), Value::List(b)) => seq_cmp(&a.borrow(), &b.borrow()),
            (Value::Tuple(a), Value::Tuple(b)) => seq_cmp(a, b),
            _ => Err(format!(
                "'<' not supported between instances of '{}' and '{}'",
                self.type_name(),
                other.type_name()
            )),
        }
    }

    /// A numeric projection for arithmetic and comparison, or `None` for
    /// non-numbers. `bool` participates as `0`/`1`.
    pub fn as_number(&self) -> Option<Number> {
        match self {
            Value::Bool(b) => Some(Number::Int(if *b { 1 } else { 0 })),
            Value::Int(i) => Some(Number::Int(*i)),
            Value::Big(b) => Some(Number::Big((**b).clone())),
            Value::Float(f) => Some(Number::Float(*f)),
            _ => None,
        }
    }
}

/// The constructor arguments stored on an exception instance (empty if none).
pub fn exception_args(inst: &Instance) -> Vec<Value> {
    match inst.fields.borrow().get("args") {
        Some(Value::Tuple(t)) => t.to_vec(),
        _ => Vec::new(),
    }
}

/// An exception's `str()`: no args → ""; one arg → that arg's str; several → the
/// args tuple's repr. Matches CPython's `BaseException.__str__`.
pub fn exception_message(inst: &Instance) -> String {
    let args = exception_args(inst);
    match args.as_slice() {
        [] => String::new(),
        [one] => one.display(),
        many => {
            let parts: Vec<String> = many.iter().map(|v| v.repr()).collect();
            format!("({})", parts.join(", "))
        }
    }
}

/// An exception's `repr()`: `Name(arg_repr, ...)`.
pub fn exception_repr(inst: &Instance) -> String {
    let args = exception_args(inst);
    let parts: Vec<String> = args.iter().map(|v| v.repr()).collect();
    format!("{}({})", inst.class.name, parts.join(", "))
}

fn seq_eq(a: &[Value], b: &[Value]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.equals(y))
}

fn seq_cmp(a: &[Value], b: &[Value]) -> VResult<std::cmp::Ordering> {
    for (x, y) in a.iter().zip(b.iter()) {
        if !x.equals(y) {
            return x.compare(y);
        }
    }
    Ok(a.len().cmp(&b.len()))
}

/// A number lifted out of a [`Value`] for arithmetic. The `bool`/`int` split is
/// erased here — `bool` arrives as `Int`.
#[derive(Clone)]
pub enum Number {
    Int(i64),
    Big(BigInt),
    Float(f64),
}

impl Number {
    /// The integer as a `BigInt`. Only valid for the integer variants.
    pub fn to_bigint(&self) -> BigInt {
        match self {
            Number::Int(i) => BigInt::from_i64(*i),
            Number::Big(b) => b.clone(),
            Number::Float(_) => unreachable!("to_bigint on a float"),
        }
    }

    pub fn to_f64(&self) -> f64 {
        match self {
            Number::Int(i) => *i as f64,
            Number::Big(b) => b.to_f64(),
            Number::Float(f) => *f,
        }
    }

    pub fn is_float(&self) -> bool {
        matches!(self, Number::Float(_))
    }

    fn equals(&self, other: &Number) -> bool {
        if self.is_float() || other.is_float() {
            self.to_f64() == other.to_f64()
        } else {
            self.to_bigint() == other.to_bigint()
        }
    }

    fn compare(&self, other: &Number) -> VResult<std::cmp::Ordering> {
        if self.is_float() || other.is_float() {
            self.to_f64()
                .partial_cmp(&other.to_f64())
                .ok_or_else(|| "cannot compare with nan".to_string())
        } else {
            Ok(self.to_bigint().cmp(&other.to_bigint()))
        }
    }
}

/// Format an `f64` the way Oro prints floats: integral finite values get a
/// trailing `.0`, otherwise Rust's shortest round-tripping form is used.
fn format_float(f: f64) -> String {
    if f.is_nan() {
        return "nan".to_string();
    }
    if f.is_infinite() {
        return if f < 0.0 { "-inf" } else { "inf" }.to_string();
    }
    if f == 0.0 {
        // Preserves the sign of negative zero, as CPython's repr does.
        return format!("{f:.1}");
    }
    // CPython switches to exponent form when the decimal exponent is < -4 or
    // >= 16; Rust's `{}` never does, so drive the choice off `{:e}`, which
    // already gives the shortest round-tripping digits.
    let sci = format!("{f:e}");
    let (mantissa, exp) = match sci.split_once('e') {
        Some((m, e)) => (m, e.parse::<i32>().unwrap_or(0)),
        None => (sci.as_str(), 0),
    };
    if !(-4..16).contains(&exp) {
        let sign = if exp < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{:02}", exp.abs());
    }
    if f.fract() == 0.0 {
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
}

/// Produce a single-quoted Python-style repr of a string, escaping specials.
fn repr_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out.push('\'');
    out
}
