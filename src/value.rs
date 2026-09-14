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
use crate::exc::type_error;
use crate::stream::OroStream;

/// A short alias for the fallible results produced by value operations and
/// builtins. The error names the exception class it raises and carries the
/// message; the VM decorates it with the source position of the faulting
/// instruction. See [`crate::exc`] for why the class is named here rather than
/// guessed from the message later.
pub type VResult<T> = Result<T, crate::exc::VErr>;

/// Iterative teardown for nested containers.
///
/// Freeing a value is naturally recursive — a list's drop glue drops its
/// elements, each of which may be a list — so a structure nested *n* deep costs
/// *n* Rust frames to free. That is a stack overflow, and it is an
/// `abort()`: not a panic, not an Oro exception, nothing a server can catch.
/// It is reachable from any deeply nested value, whether it was built by a loop
/// in Oro or parsed from a socket by `json`.
///
/// A depth limit does not fix this, it only postpones it: the safe limit
/// depends on the frame size the optimizer happened to produce, so a number
/// that survives `--release` can abort in a debug build, and a Rust upgrade or
/// a change to `Value`'s layout can move it either way. The limit `json` keeps
/// is a bound on *memory amplification*, which is a real thing to bound; it was
/// never a sound bound on stack depth and is not asked to be one.
///
/// So teardown is a worklist instead of a call stack. The four container
/// payloads — [`OroList`], [`OroTuple`], [`OroDict`] and [`Instance`] — each
/// have a `Drop` that moves their children into a thread-local queue and leaves
/// an empty container for the drop glue to free, so the glue has nothing to
/// recurse into. The outermost drop drains the queue in a loop; every drop
/// inside that loop sees `DRAINING` set, contributes its own children and
/// returns immediately. Stack depth is constant in the nesting depth —
/// genuinely, not by arithmetic on a frame size.
///
/// The hook is on the *payloads* and not on `Value`, and that is a measured
/// decision rather than a stylistic one: a `Drop for Value` costs every value
/// drop in the program, integers included, and measured **+7.7% on `loop`** and
/// **+4.3% on `fib`** — benchmarks with no container in them at all. Hooking
/// the payloads means only a program that frees a container pays anything, and
/// what it pays is one queue push per child that could itself nest.
///
/// The four hooks cover every chain a value graph can form, including
/// alternating ones, because the queue takes *everything that is not provably a
/// leaf* — a closure, a generator, a bound method — and not merely the four
/// container kinds. A list of closures each capturing a list would otherwise
/// still recurse, since the closure would be freed where it was found and reach
/// the next list from there.
///
/// **What this does not cover**, so the guarantee is not overclaimed. Hashing
/// and comparing a nested `tuple` are still recursive (`HKey::from_value`,
/// `try_equals`), so a tuple nested deeply enough overflows when it is used as
/// a dict key, not when it is freed. That is a smaller hazard — it needs a
/// program to *build* such a key rather than merely to receive data — and it is
/// a separate change. `json` cannot reach it: JSON has no tuples and its keys
/// are strings.
///
/// The other residue is a chain with no container anywhere in it — a closure
/// capturing a closure capturing a closure, a thousand deep. There is no owned
/// type on that path to hang a hook on (the cell is an `Rc<RefCell<Value>>`),
/// and nothing arriving over a socket can build one, so it is left.
mod teardown {
    use super::Value;
    use std::cell::{Cell, RefCell};

    thread_local! {
        /// Containers whose parent has already been freed, waiting to be freed
        /// themselves.
        static PENDING: RefCell<Vec<Value>> = const { RefCell::new(Vec::new()) };
        /// Whether a drain is already running further up this stack.
        static DRAINING: Cell<bool> = const { Cell::new(false) };
    }

    /// Is this a value that provably holds no other value?
    ///
    /// The list is deliberately by *inclusion*, not by exclusion. Queueing only
    /// the four containers would still recurse on a chain that alternates with
    /// something else — a list of closures each capturing a list — because the
    /// closure would be dropped inline and reach the next list from there.
    /// Anything that might hold a `Value`, however indirectly, goes on the
    /// queue; only the leaves are freed where they are found.
    ///
    /// The leaves are what a list is normally full of, which is the point: a
    /// list of a million integers never touches the queue.
    #[inline]
    fn is_leaf(v: &Value) -> bool {
        matches!(
            v,
            Value::None
                | Value::Bool(_)
                | Value::Int(_)
                | Value::Big(_)
                | Value::Float(_)
                | Value::Str(_)
                | Value::Bytes(_)
                | Value::Range(_)
                | Value::Builtin(_)
                | Value::Unbound
        )
    }

    /// Free `children`, without recursing for any of them that might nest.
    ///
    /// Takes an iterator rather than a `Vec` so that a dict's entries and an
    /// instance's fields go straight from their own storage into the queue: a
    /// `collect()` here would be a heap allocation for every dict and every
    /// instance the program frees, which `oo` and `exc` do hundreds of
    /// thousands of times. `&mut dyn` rather than a generic, so there is one
    /// copy of this function and not four.
    #[inline(never)]
    pub(super) fn release(children: &mut dyn Iterator<Item = Value>) {
        // Fast path: a container whose children are *all* leaves — a record of
        // strings and numbers, a list of integers — is freed exactly where it
        // is found, and never touches the queue or the thread-local at all.
        // That is most containers in most programs, and nearly all of the ones
        // near the fringe of a parsed document, so it is the case worth having.
        let first_nester = loop {
            match children.next() {
                None => return,
                Some(child) if is_leaf(&child) => drop(child),
                Some(child) => break child,
            }
        };
        // Something here can nest, so the rest goes through the queue — one
        // thread-local access for the whole container, not one per child.
        PENDING.with(|p| {
            let mut queue = p.borrow_mut();
            queue.push(first_nester);
            for child in children {
                if is_leaf(&child) {
                    // Freed with the queue still borrowed, and that is sound
                    // precisely because it is a leaf: it owns no `Value`, so
                    // its drop cannot re-enter this module and find the
                    // `RefCell` already taken.
                    drop(child);
                } else {
                    queue.push(child);
                }
            }
        });
        drain();
    }

    #[inline(never)]
    fn drain() {
        // Already draining further up: this call has done its job by queueing,
        // and returning now is what keeps the stack flat.
        if DRAINING.with(|d| d.replace(true)) {
            return;
        }
        // Reset through a guard, so an unwind out of a drop (which should not
        // happen, but "should not" is not "cannot") does not leave the flag set
        // and every later teardown recursive again.
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                DRAINING.with(|d| d.set(false));
            }
        }
        let _guard = Guard;
        // Take the whole queue at once and free it outside the borrow. What
        // those frees enqueue lands in the now-empty `PENDING` and is picked up
        // by the next swap, so this costs one thread-local access per *level*
        // of the structure rather than one per node. (Measured against the
        // pop-one-at-a-time version this is a small win, not a decisive one:
        // what `json` actually pays for is moving container children through
        // the queue at all, and no arrangement of the queue changes that.)
        //
        // `level` keeps its capacity across iterations, so a deep teardown
        // allocates once rather than per level.
        let mut level: Vec<Value> = Vec::new();
        loop {
            PENDING.with(|p| std::mem::swap(&mut *p.borrow_mut(), &mut level));
            if level.is_empty() {
                return;
            }
            // Each of these re-enters `release`, which queues its own children
            // and returns — one frame, not one per level.
            level.clear();
        }
    }
}

/// How often [`Value::truthy`] was asked, in test builds only.
///
/// Not a profiler: a *shape* meter. Deciding whether a collection terminal's
/// answer is settled by scanning the callback results collected so far is a
/// `truthy` per entry, so counting the calls measures how the collection
/// driver's work grows with the length of its input without timing anything.
/// Counting the *operation* rather than the scan is the point: a rescan
/// reintroduced anywhere in the driver is caught whether or not whoever wrote
/// it thought to instrument it. Read by
/// `vm::tests::a_short_circuit_terminal_does_not_rescan_its_results`.
#[cfg(test)]
pub(crate) mod truthy_count {
    use std::cell::Cell;

    thread_local! {
        static CALLS: Cell<u64> = const { Cell::new(0) };
    }

    /// Count one call.
    pub(crate) fn bump() {
        CALLS.with(|c| c.set(c.get() + 1));
    }

    /// Read the counter and zero it, so each measurement stands alone. The
    /// counter is thread-local, so tests running in parallel cannot see each
    /// other's counts.
    pub(crate) fn take() -> u64 {
        CALLS.with(|c| c.replace(0))
    }
}

/// The payload of a `list`: a `Vec<Value>` behind a `RefCell`, in a type of our
/// own so that [`teardown`] has somewhere to hook.
///
/// It exists for exactly that reason. `Drop` cannot be implemented for
/// `RefCell<Vec<Value>>` — it is not ours — and implementing it for `Value`
/// instead taxes *every* value drop in the program, including integers, which
/// measured **+7.7% on `loop`** and +4.3% on `fib`: the hook has to sit where
/// only containers pay for it. It derefs to the `RefCell`, so `l.borrow()`,
/// `l.borrow_mut().push(..)` and everything else read exactly as before.
#[derive(Default)]
pub struct OroList(RefCell<Vec<Value>>);

// A test-only tally of how many `list` payloads have been allocated — the
// observable for the fusion invariant: a fused chain builds no intermediate
// collection, so `xs.map(f).filter(p).reduce(…)` allocates exactly the source
// and nothing per stage. See `vm::tests::a_fused_chain_allocates_no_intermediate`.
// Thread-local, not a global atomic: the test suite runs in parallel and the VM
// allocates on the calling test's own thread, so a per-thread tally sees exactly
// one test's `list` allocations and no other's.
#[cfg(test)]
thread_local! {
    pub static LIST_ALLOCS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl OroList {
    pub fn new(items: Vec<Value>) -> Rc<OroList> {
        #[cfg(test)]
        LIST_ALLOCS.with(|c| c.set(c.get() + 1));
        Rc::new(OroList(RefCell::new(items)))
    }
}

impl std::ops::Deref for OroList {
    type Target = RefCell<Vec<Value>>;
    fn deref(&self) -> &RefCell<Vec<Value>> {
        &self.0
    }
}

impl Drop for OroList {
    #[inline(never)]
    fn drop(&mut self) {
        teardown::release(&mut std::mem::take(self.0.get_mut()).into_iter());
    }
}

/// The payload of a `tuple`. Same reasoning as [`OroList`], without the
/// `RefCell` — a tuple is immutable.
#[derive(Default)]
pub struct OroTuple(Vec<Value>);

impl OroTuple {
    pub fn new(items: Vec<Value>) -> Rc<OroTuple> {
        Rc::new(OroTuple(items))
    }
}

impl std::ops::Deref for OroTuple {
    type Target = Vec<Value>;
    fn deref(&self) -> &Vec<Value> {
        &self.0
    }
}

impl Drop for OroTuple {
    #[inline(never)]
    fn drop(&mut self) {
        teardown::release(&mut std::mem::take(&mut self.0).into_iter());
    }
}

impl Drop for OroDict {
    #[inline(never)]
    fn drop(&mut self) {
        // Both halves of every entry: a key is a `Value` too, and a tuple key
        // can nest as deeply as anything else.
        teardown::release(&mut self.take_entries().into_iter().flat_map(|(k, v)| [k, v]));
    }
}

impl Drop for Instance {
    #[inline(never)]
    fn drop(&mut self) {
        teardown::release(&mut std::mem::take(self.fields.get_mut()).into_values());
    }
}

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
    /// An immutable string of octets (`b"..."`). A second type rather than a
    /// reinterpretation of `Str`: `str` is a sequence of characters, `bytes` a
    /// sequence of numbers, and each tells the truth about what it holds (see
    /// `docs/stdlib-server-design.md` §1).
    Bytes(Rc<Vec<u8>>),
    List(Rc<OroList>),
    Tuple(Rc<OroTuple>),
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
    /// An open byte stream — a `File` from `open()`, or a `Buffer` — refcounted
    /// so it closes deterministically when the last reference is dropped
    /// (Oro's answer to `with`). See `crate::stream`.
    Stream(Rc<OroStream>),
    /// A generator: a suspended function activation, advanced by iteration. The
    /// concrete state lives in the VM (it holds a `Frame`), so this is an opaque
    /// handle here.
    Generator(Rc<RefCell<GenBox>>),
    /// A compiled regular expression (`re.compile`), backed by the linear-time
    /// `regex` engine.
    Regex(Rc<OroRegex>),
    /// A regex match, with group texts and their char-offset spans.
    Match(Rc<OroMatch>),
    /// A green thread's handle, as returned by `spawn`. The stack segment it
    /// names lives in the scheduler; this is only the outcome cell and the
    /// joiner list. See `crate::task`.
    Task(Rc<crate::task::TaskHandle>),
    /// A channel, as returned by `chan`. See `crate::task`.
    Channel(Rc<crate::task::Channel>),
    /// A spawned child process's handle, as returned by `proc.spawn`. Holds the
    /// live child and its three pipe streams; the reaping and the kill-on-drop
    /// are the only parts that must be Rust. See `crate::process`.
    Proc(Rc<crate::process::Proc>),
    /// Internal sentinel for a local/cell slot that has not been assigned yet.
    /// Never reachable by user code: reading it raises a clean runtime error.
    Unbound,
    /// A builtin type, as named by one of the type keywords (`str`, `int`,
    /// `File`, ...) and as answered by `type(x)`.
    ///
    /// A user type is a [`Value::Class`] and a builtin type is this, because a
    /// builtin type has no members, no base and no identity beyond its name —
    /// a whole `Rc<Class>` to carry one byte of information would put an
    /// allocation and a refcount bump on `type(x)`. The two behave alike where
    /// it is observable: both print `<class 'N'>`, both compare by identity,
    /// and `type(x)` answers one or the other.
    Type(TypeTag),
}

/// One of Oro's builtin types.
///
/// Field-less, so [`Value`] stays 16 bytes (`value::tests::value_is_two_words`).
/// The names live in [`TypeTag::name`], which is the only type-name table in
/// the tree — [`Value::type_name`] reads it too.
///
/// Not every tag is reachable from Oro source: the keywords bind the types a
/// program can construct and would test (see `KEYWORD_TYPES`), while
/// `Function`, `Generator`, `Module`, `Method`, `Iterator`, `BuiltinFunction`,
/// `Object`, `Super`, `Type`, `Null` and `Unbound` exist so `type(x)` can still
/// *name* those values. They are printed, not written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum TypeTag {
    Null,
    Bool,
    Int,
    Float,
    Str,
    Bytes,
    List,
    Tuple,
    Dict,
    Range,
    Iterator,
    Function,
    BuiltinFunction,
    Method,
    Type,
    Object,
    Super,
    Module,
    File,
    Buffer,
    TcpStream,
    TcpListener,
    Generator,
    Pattern,
    Match,
    Task,
    Channel,
    Proc,
    Pipe,
    Unbound,
}

impl TypeTag {
    /// The type's name — the word between the quotes in `<class 'str'>`.
    ///
    /// `null`'s type is `null`, not CPython's `NoneType`. That name was a
    /// leftover: `None` is not a spelling this language has any more, so a type
    /// named after it pointed at nothing the reader could write. This is a
    /// deliberate divergence and the only one in the table — every other name
    /// here is CPython's, so `corpus/core/` still oracles the whole of the rest
    /// and only the one line moved to `corpus/divergence/`.
    pub fn name(self) -> &'static str {
        match self {
            TypeTag::Null => "null",
            TypeTag::Bool => "bool",
            TypeTag::Int => "int",
            TypeTag::Float => "float",
            TypeTag::Str => "str",
            TypeTag::Bytes => "bytes",
            TypeTag::List => "list",
            TypeTag::Tuple => "tuple",
            TypeTag::Dict => "dict",
            TypeTag::Range => "range",
            TypeTag::Iterator => "iterator",
            TypeTag::Function => "function",
            TypeTag::BuiltinFunction => "builtin_function",
            TypeTag::Method => "method",
            TypeTag::Type => "type",
            TypeTag::Object => "object",
            TypeTag::Super => "super",
            TypeTag::Module => "module",
            TypeTag::File => "File",
            TypeTag::Buffer => "Buffer",
            TypeTag::TcpStream => "TcpStream",
            TypeTag::TcpListener => "TcpListener",
            TypeTag::Generator => "generator",
            TypeTag::Pattern => "Pattern",
            TypeTag::Match => "Match",
            TypeTag::Task => "Task",
            TypeTag::Channel => "Channel",
            TypeTag::Proc => "Proc",
            TypeTag::Pipe => "Pipe",
            TypeTag::Unbound => "unbound",
        }
    }
}

/// The type keywords: the names a program may write to denote a builtin type.
///
/// A name in this table is **not a variable**. It cannot be assigned, declared
/// `global`, bound by `def`/`class`/`for`/`import`/`except ... as`, or taken as
/// a parameter — the compiler rejects all of those by name, so `dict = {}`
/// cannot quietly shadow the type the way it used to. What a type keyword
/// denotes is a [`Value::Type`], which is exactly what `type(x)` answers, so
/// `type(x) == str` is true and there is one spelling of a type test.
///
/// The line this table draws: a keyword exists for every type a program can
/// *construct* — the nine with literal or constructor syntax, and the eight
/// runtime handles the stdlib and user code dispatch on. The remaining tags
/// (`function`, `generator`, `module`, `method`, ...) are lowercase English
/// words that would break far more programs as reserved names than a type test
/// against them would ever serve; `type(x)` still prints them.
pub const KEYWORD_TYPES: &[(&str, TypeTag)] = &[
    ("bool", TypeTag::Bool),
    ("int", TypeTag::Int),
    ("float", TypeTag::Float),
    ("str", TypeTag::Str),
    ("bytes", TypeTag::Bytes),
    ("list", TypeTag::List),
    ("tuple", TypeTag::Tuple),
    ("dict", TypeTag::Dict),
    ("range", TypeTag::Range),
    ("File", TypeTag::File),
    ("Buffer", TypeTag::Buffer),
    ("TcpStream", TypeTag::TcpStream),
    ("TcpListener", TypeTag::TcpListener),
    ("Pattern", TypeTag::Pattern),
    ("Match", TypeTag::Match),
    ("Task", TypeTag::Task),
    ("Channel", TypeTag::Channel),
];

/// The [`TypeTag`] a type keyword denotes, or `None` if `name` is an ordinary
/// identifier. The compiler asks this at every name it binds and every name it
/// reads.
pub fn keyword_type(name: &str) -> Option<TypeTag> {
    // Linear over 17 entries, and only on the names that reach it: a `&str`
    // compare fails on the first byte for almost every identifier.
    KEYWORD_TYPES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, t)| *t)
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

    /// The characters in `start..end` (character indices, already clamped to
    /// `0 ..= char_len()` with `start <= end`), as a borrowed `&str`.
    ///
    /// O(1) for ASCII. For a string that is not, it walks to `end` once rather
    /// than collecting the whole thing into a `Vec<char>` — so a slice near the
    /// front of a large string costs what the slice costs, not what the string
    /// costs.
    pub fn byte_slice(&self, start: usize, end: usize) -> &str {
        if self.is_ascii {
            return &self.s[start..end];
        }
        let mut it = self.s.char_indices();
        let lo = match it.by_ref().nth(start) {
            Some((b, _)) => b,
            None => return "",
        };
        // `nth(k)` has already consumed `start + 1` characters, so the byte
        // offset of character `end` is `end - start - 1` further on.
        let hi = if end <= start {
            lo
        } else {
            match it.nth(end - start - 1) {
                Some((b, _)) => b,
                None => self.s.len(),
            }
        };
        &self.s[lo..hi]
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
            (((self.stop - self.start) as i128 + self.step as i128 - 1) / self.step as i128)
                as usize
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

    /// CPython's `range.__eq__`, which compares the *sequence* rather than the
    /// three fields: two ranges are equal when they have the same length and
    /// yield the same values, so `range(0) == range(2, 2, 7)` is true and
    /// `step` only matters once there are at least two elements to step
    /// between.
    fn equals(&self, other: &RangeVal) -> bool {
        let n = self.len();
        if n != other.len() {
            return false;
        }
        if n == 0 {
            return true;
        }
        if self.start != other.start {
            return false;
        }
        n == 1 || self.step == other.step
    }
}

/// The state backing a live iterator. Each `ForIter` step advances it.
pub enum IterState {
    /// `n` is the position counter — a `for` yields `(index, value)`, and a
    /// range's index is its position, which the raw `cur`/`step` do not give
    /// directly once `start` is not zero.
    Range {
        cur: i64,
        stop: i64,
        step: i64,
        n: i64,
    },
    /// Iterates by index, remembering the original length so a size change
    /// during iteration is reported as a clean error rather than silently
    /// skipping or panicking.
    List {
        list: Rc<OroList>,
        idx: usize,
        orig_len: usize,
    },
    Tuple {
        tuple: Rc<OroTuple>,
        idx: usize,
    },
    Str {
        chars: Vec<String>,
        idx: usize,
    },
    /// Iterating `bytes` yields the octets as `int`s, so no per-element
    /// allocation is needed — the source `Rc` is simply held and indexed.
    Bytes {
        bytes: Rc<Vec<u8>>,
        idx: usize,
    },
    /// Iterating a dict yields its `(key, value)` pairs — the same shape
    /// `map` and `filter` over a dict already answer with. The entries are
    /// snapshotted at `GetIter` time; the pair tuple itself is built per step,
    /// so a loop that unpacks and drops it never holds more than one.
    DictPairs {
        items: Vec<(Value, Value)>,
        idx: usize,
    },
}

/// A compiled Oro function together with its captured environment.
pub struct Function {
    pub code: Rc<CodeObject>,
    /// Captured cells, one per entry in `code.freevars`, shared with the scope
    /// that defined this function.
    ///
    /// There is no `defaults` field beside it. A default is not a value the
    /// `def` computed once and this function carries: a constant one lives on
    /// the code object, and any other is evaluated in the callee's frame on
    /// each call that omits it. See [`CodeObject::defaults`].
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

impl BoundMethod {
    /// CPython's `method.__eq__`: two bound methods are equal when they bind
    /// the same function to the same receiver.
    ///
    /// The subtlety worth knowing is that `a.m is a.m` is **false** — a fresh
    /// bound method is built on every attribute access, in CPython as here —
    /// while `a.m == a.m` is **true**. Identity would answer the first
    /// question when the reader asked the second, so this is the one reference
    /// type that is not compared by address.
    ///
    /// `None`, like everywhere else in this file, means the receivers are a
    /// pair only the VM can decide (see [`Value::try_equals`]).
    ///
    /// Out of line for the reason [`try_seq_eq`] is: it calls back into
    /// [`Value::eq_at`], and inlining it there would make that function
    /// self-recursive — which would cost it the inlining into every `==` in
    /// the program, for an arm reached only by comparing two bound methods.
    #[inline(never)]
    fn try_equals(&self, other: &BoundMethod, depth: u32) -> Option<bool> {
        if !self.kinds_equal(other) {
            return Some(false);
        }
        self.receiver.eq_at(&other.receiver, depth + 1)
    }

    /// The half of [`BoundMethod::try_equals`] that never needs the VM: same
    /// function, or same native name.
    fn kinds_equal(&self, other: &BoundMethod) -> bool {
        match (&self.kind, &other.kind) {
            (MethodKind::User { func: a, .. }, MethodKind::User { func: b, .. }) => {
                Rc::ptr_eq(a, b)
            }
            // A native method has no function object to point at; its name is
            // its identity, and the receiver's type fixes what the name means.
            (MethodKind::Native(a), MethodKind::Native(b)) => a == b,
            _ => false,
        }
    }

    /// The [`HKey`] half of [`BoundMethod::equals`] — the same two components,
    /// so equal methods hash alike. Unhashable exactly when the receiver is:
    /// `[].append` is no more a dict key than `[]` is, which is CPython's rule
    /// too.
    fn hkey(&self) -> VResult<HKey> {
        let recv = HKey::from_value(&self.receiver)?;
        let func = match &self.kind {
            MethodKind::User { func, .. } => HKey::Id(Rc::as_ptr(func) as *const () as usize),
            MethodKind::Native(name) => HKey::Str(StrKey(OroStr::new(name.to_string()))),
        };
        Ok(HKey::Method(Box::new(recv), Box::new(func)))
    }
}

#[derive(Clone)]
pub enum MethodKind {
    /// A builtin method, dispatched by name.
    Native(Rc<str>),
    /// An Oro method: `func` is called with `receiver` as its first (`self`)
    /// argument. `defclass` is the class the method is defined in, so that
    /// `super()` inside it searches from `defclass`'s base.
    User {
        func: Rc<Function>,
        defclass: Rc<Class>,
    },
}

/// A user-defined class (single inheritance only).
pub struct Class {
    pub name: Rc<str>,
    pub base: Option<Rc<Class>>,
    /// Methods and class-level attributes, by name. A [`Fields`] for the same
    /// reasons an instance's attributes are one, and on a hotter path than
    /// theirs: `obj.m()` reaches the class table only *after* missing in the
    /// instance, so every method call in the program pays for this lookup.
    pub members: RefCell<Fields>,
    /// True when this class descends from `BaseException`. Such instances get
    /// native message storage/rendering and are what `raise`/`except` operate on.
    pub is_exception: bool,
}

impl Class {
    /// Find `name` in this class or its base chain, returning the member and the
    /// class it was found in (the latter fixes `super()`'s search origin).
    ///
    /// Walks the chain by reference: each base is owned by its subclass, so the
    /// search needs no refcount traffic at all, and only the class it actually
    /// finds the member in is cloned.
    pub fn find(class: &Rc<Class>, name: &str) -> Option<(Value, Rc<Class>)> {
        let mut cur = class;
        loop {
            if let Some(v) = cur.members.borrow().get(name) {
                return Some((v.clone(), cur.clone()));
            }
            cur = cur.base.as_ref()?;
        }
    }

    /// Whether `name` is defined anywhere in this class or its bases, without
    /// producing the member. [`find`](Self::find) clones both the value and the
    /// class it came from; the equality path only wants the yes/no, and asks it
    /// on comparisons that are not going to dispatch anything.
    pub fn defines(class: &Rc<Class>, name: &str) -> bool {
        let mut cur = class;
        loop {
            if cur.members.borrow().contains_key(name) {
                return true;
            }
            match cur.base.as_ref() {
                Some(b) => cur = b,
                None => return false,
            }
        }
    }

    /// True when `class` is `other` or a subclass of it (used by `isinstance`).
    pub fn is_subclass(class: &Rc<Class>, other: &Rc<Class>) -> bool {
        let mut cur = class;
        loop {
            if Rc::ptr_eq(cur, other) {
                return true;
            }
            match &cur.base {
                Some(base) => cur = base,
                None => return false,
            }
        }
    }
}

/// An instance of a user class. Instance attributes live in `fields`.
pub struct Instance {
    pub class: Rc<Class>,
    /// Instance attributes, by name. See [`Fields`].
    pub fields: RefCell<Fields>,
}

/// An instance's attribute table: an association list, scanned linearly.
///
/// This was a `HashMap<Rc<str>, Value>`, and the map was the wrong shape for
/// what it holds. Instances carry a *handful* of attributes — the classes in
/// this repository's corpus average three, and a class with more than ten is
/// not idiomatic Oro — while the names are short identifiers, so hashing one
/// with SipHash costs more than comparing it against every entry there is.
/// Measured on a loop that keeps its cursor in `self.pos`, the swap is worth
/// roughly a quarter of the loop's total time.
///
/// Two properties make the linear scan safe as well as fast. Nothing anywhere
/// iterates an instance's fields — they are only ever `get` and `insert`, and
/// Oro has no `del obj.x` — so the order entries happen to sit in is not
/// observable, and an entry's index, once assigned, never moves. The first
/// fact is what allows a `Vec` at all; the second is what would allow a
/// per-call-site slot cache on top of it later.
///
/// The name comparison leads with the pointers because it usually wins: a
/// field is stored under the very `Rc<str>` the code object interned, so a
/// later read of the same name from the same code object compares equal
/// without touching the characters. A read from a *different* code object
/// (`__init__` stores `self.x`, `add` reads it) falls through to the ordinary
/// `str` comparison, which for a short identifier is a length check and one
/// word of `memcmp`.
#[derive(Default)]
pub struct Fields {
    entries: Vec<(Rc<str>, Value)>,
}

impl Fields {
    pub fn new() -> Fields {
        Fields {
            entries: Vec::new(),
        }
    }

    #[inline]
    pub fn get(&self, name: &str) -> Option<&Value> {
        for (k, v) in &self.entries {
            if same_name(k, name) {
                return Some(v);
            }
        }
        None
    }

    pub fn with_capacity(n: usize) -> Fields {
        Fields {
            entries: Vec::with_capacity(n),
        }
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    pub fn insert(&mut self, name: Rc<str>, value: Value) {
        for (k, v) in &mut self.entries {
            if same_name(k, &name) {
                *v = value;
                return;
            }
        }
        self.entries.push((name, value));
    }

    /// The values, consuming the map — the `HashMap` method this replaced, kept
    /// under the same name so the teardown call site reads the same for every
    /// container. The keys are `Rc<str>` and hold no `Value`, so dropping them
    /// here cannot reach another container and start recursing.
    pub fn into_values(self) -> impl Iterator<Item = Value> {
        self.entries.into_iter().map(|(_, v)| v)
    }
}

/// Whether an interned field name is the name being looked up. Pointer-equal
/// `Rc<str>`s are the same string by construction; anything else is decided by
/// the characters. See [`Fields`].
#[inline]
fn same_name(k: &Rc<str>, name: &str) -> bool {
    (std::ptr::eq(k.as_ptr(), name.as_ptr()) && k.len() == name.len()) || &**k == name
}

/// The state of a generator. Its suspended activation record is a VM `Frame`,
/// stored opaquely here (the VM downcasts it) so `value` need not know the
/// frame layout. `done` is set when the generator is exhausted.
pub struct GenBox {
    pub done: bool,
    /// The position counter for a `for` loop over this generator: a `for` yields
    /// `(index, value)`, and a generator's index is a plain 0-based counter (it
    /// has no positions to speak of, and may be infinite). Bumped once per value
    /// delivered to a `for` driver; untouched by materialisation into a chain.
    pub for_index: i64,
    /// The generator's frame slot: a box holding `Some(frame)` while it is
    /// suspended and `None` while it is running (its frame is then on some
    /// task's frame stack). `yield` is statement-only in Oro, so resuming just
    /// continues the frame — there is no sent value to inject.
    ///
    /// The concrete type inside is `Option<Frame>`, and the **box is allocated
    /// once per generator, not once per `yield`**: it used to hold
    /// `Box<Frame>`, so suspending allocated and resuming freed, a malloc/free
    /// pair per element produced. The outer `Option` is only ever `None` for a
    /// generator that has finished.
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
    pub members: RefCell<HashMap<Rc<str>, Value>>,
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
    // Keyed by a per-process-randomised `ahash` seed, not `std`'s SipHash: dict
    // keys arrive from untrusted input (JSON bodies, HTTP header names), so this
    // map is directly reachable by an anonymous client and must be fast *and*
    // collision-attack-resistant. `ahash`'s seed is drawn once per process from
    // the OS RNG (`runtime-rng`), so an attacker cannot precompute keys that all
    // land in one bucket. Insertion order — the observable order — lives in
    // `entries`, not here, so the randomised seed changes nothing a program sees.
    // See Cargo.toml and docs/hash-and-equality.md.
    index: HashMap<HKey, usize, ahash::RandomState>,
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

    /// Empty the dict, handing back its entries. Used by the iterative teardown
    /// in [`Value::drop`]; nothing else should need it, because a dict that is
    /// being emptied for any other reason wants `entries.clear()` *and* the
    /// index cleared with it.
    pub(crate) fn take_entries(&mut self) -> Vec<(Value, Value)> {
        self.index.clear();
        std::mem::take(&mut self.entries)
    }

    pub fn items(&self) -> &[(Value, Value)] {
        &self.entries
    }

    /// Remove `key` and answer its value, or `None` if it was not there.
    ///
    /// Insertion order survives the removal, which is the whole point: `d.pop`
    /// is the removal a language with no `del` offers, and an order-preserving
    /// dict that reorders itself when you take a key out of it would be a
    /// surprise nobody asked for. The entry is spliced out of `entries` and
    /// every index past it slides down one — O(n) in the dict's size, the price
    /// of a compact array with no tombstones, and the same shape as
    /// `list.pop(i)`, which shifts for the same reason.
    pub fn remove(&mut self, key: &Value) -> VResult<Option<Value>> {
        let hk = HKey::from_value(key)?;
        let Some(pos) = self.index.remove(&hk) else {
            return Ok(None);
        };
        let (_, value) = self.entries.remove(pos);
        for slot in self.index.values_mut() {
            if *slot > pos {
                *slot -= 1;
            }
        }
        Ok(Some(value))
    }
}

/// A hashable projection of a [`Value`], used as a dict key.
///
/// Numeric keys are normalised so that `True`, `1` and `1.0` collide, matching
/// Python (`{1: "a", True: "b", 1.0: "c"}` has a single entry). The mutable
/// containers — `list` and `dict` — are unhashable, as they are in CPython, and
/// so is an instance of a class that defines its own `__eq__`.
///
/// Everything else *is* a key, including the reference types, keyed by
/// [`Value::identity`]. That is what makes a registry keyed by connection, task
/// or handler writable at all — `docs/stdlib-server-design.md` §7 item 13.
/// A string dict key, shared with the string it came from rather than copied
/// out of it.
///
/// `HKey::Str` used to hold a `String`, so `d["name"]` allocated a copy of the
/// whole string to build a probe that was thrown away a moment later — on every
/// read and every write. An `Rc<OroStr>` is the same key for a refcount bump.
///
/// Equality and hashing are by **content**, which is what makes this a drop-in
/// for the `String` it replaces: two equal strings must land in the same bucket
/// however they were built, so the hash cannot involve the address. Equality
/// leads with a pointer comparison because a key stored from an interned name
/// is usually probed with the very same `Rc`, and then answers by content.
#[derive(Clone)]
struct StrKey(Rc<OroStr>);

impl PartialEq for StrKey {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0) || self.0.s == other.0.s
    }
}

impl Eq for StrKey {}

impl std::hash::Hash for StrKey {
    #[inline]
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // Exactly what `String`'s own `Hash` does, so a dict built before this
        // change and one built after agree bucket for bucket.
        self.0.s.hash(state);
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum HKey {
    None,
    Int(i64),
    Big(BigInt),
    /// Only non-integral floats reach here (integral ones normalise to `Int`).
    Float(u64),
    Str(StrKey),
    Bytes(Vec<u8>),
    Tuple(Vec<HKey>),
    /// A `range`, by the sequence it denotes rather than its three fields:
    /// `(len, start, step)` with the last two zeroed exactly where
    /// [`RangeVal::equals`] stops looking at them, so equal ranges hash alike.
    ///
    /// Three plain integers rather than the `Option`s that read more honestly.
    /// The honest spelling made this the widest variant in the enum — 40 bytes
    /// against `Big`'s 32 — and paying for that in `HKey`'s layout is paying
    /// for it on every dict operation in every program. Measured at +3% on
    /// `bench/progs/dictops.oro`, which is 1M integer-keyed hashes and no
    /// ranges at all.
    Range(usize, i64, i64),
    /// A reference type, by the address of its heap cell — see
    /// [`Value::identity`].
    ///
    /// **Why a raw address is sound as a key.** An address identifies an object
    /// only while that object is alive, and every address that reaches a
    /// comparison here belongs to a live object: the probe is built from a
    /// `Value` the caller is holding, and a stored key is held by
    /// [`OroDict::entries`], which owns the key `Value` for as long as the
    /// entry exists. Two live objects never share an address, so a match here
    /// is identity and nothing else.
    Id(usize),
    /// A builtin, by name.
    ///
    /// Not by address, because there is no single address to use: the builtin
    /// cache is per call site, so `len` in two places is two `Rc<Builtin>`s
    /// wrapping the same function. The name is what CPython's `len is len`
    /// actually means here, and it is unique per builtin.
    Builtin(&'static str),
    /// A builtin type, by its tag. Keeps `{type(v): ...}` working, which is
    /// what `std/json.oro` dispatches on.
    Type(TypeTag),
    /// A bound method: the receiver's key and the function's. See
    /// [`BoundMethod::hkey`].
    Method(Box<HKey>, Box<HKey>),
}

impl HKey {
    fn from_value(v: &Value) -> VResult<HKey> {
        Ok(match v {
            Value::None => HKey::None,
            Value::Bool(b) => HKey::Int(if *b { 1 } else { 0 }),
            Value::Int(i) => HKey::Int(*i),
            Value::Big(b) => HKey::Big((**b).clone()),
            Value::Float(f) => {
                if f.is_finite()
                    && f.fract() == 0.0
                    && *f >= i64::MIN as f64
                    && *f <= i64::MAX as f64
                {
                    HKey::Int(*f as i64)
                } else {
                    HKey::Float(f.to_bits())
                }
            }
            Value::Str(s) => HKey::Str(StrKey(s.clone())),
            Value::Bytes(b) => HKey::Bytes((**b).clone()),
            Value::Type(t) => HKey::Type(*t),
            Value::Tuple(items) => {
                let mut parts = Vec::with_capacity(items.len());
                for it in items.iter() {
                    parts.push(HKey::from_value(it)?);
                }
                HKey::Tuple(parts)
            }
            // Everything else is a reference type or an error, and neither is
            // on any hot path. Out of line so that `from_value` — which runs on
            // every dict read and write in the program — stays the size it was.
            other => return hkey_cold(other),
        })
    }
}

/// The [`HKey::from_value`] arms that are not the common case: the reference
/// types, and the three things that are not keys.
#[inline(never)]
fn hkey_cold(v: &Value) -> VResult<HKey> {
    Ok(match v {
        Value::Range(r) => {
            let n = r.len();
            HKey::Range(
                n,
                if n > 0 { r.start } else { 0 },
                if n > 1 { r.step } else { 0 },
            )
        }
        Value::Builtin(b) => HKey::Builtin(b.name),
        Value::Method(m) => m.hkey()?,
        // An instance is keyed by identity, which is only honest while the
        // class has not taken equality over. There is no `__hash__` in Oro's
        // dunder set to restore it with — CPython's escape hatch — so a class
        // that decides its own equality is not a key, and says so rather than
        // silently keying by address and losing lookups.
        Value::Instance(i) => {
            if Class::find(&i.class, "__eq__").is_some() {
                return Err(type_error(format!(
                    "unhashable type: '{}' — it defines __eq__, so its identity is \
                     not what equality means for it; key by the value it compares by, \
                     e.g. counts[(\"a\", 1)]",
                    i.class.name
                )));
            }
            HKey::Id(Rc::as_ptr(i) as *const () as usize)
        }
        // The mutable containers, and the internal sentinel. `list` and `dict`
        // are unhashable in CPython for the reason that outlives every other
        // argument about it: a key that can change is a key that can be lost.
        Value::List(_) | Value::Dict(_) | Value::Unbound => {
            return Err(type_error(format!("unhashable type: '{}'", v.type_name())));
        }
        other => match other.identity() {
            Some(id) => HKey::Id(id),
            // Unreachable: every variant is either handled in `from_value` or
            // above, or is a reference type with an identity. Spelled as an
            // error rather than `unreachable!` because a panic in the dict path
            // would be a worse answer than a diagnostic.
            None => {
                return Err(type_error(format!(
                    "unhashable type: '{}'",
                    other.type_name()
                )))
            }
        },
    })
}

impl Value {
    /// Build a string value, computing the ASCII flag once.
    pub fn str(s: impl Into<String>) -> Value {
        Value::Str(OroStr::new(s.into()))
    }

    /// Build a byte-string value.
    pub fn bytes(b: impl Into<Vec<u8>>) -> Value {
        Value::Bytes(Rc::new(b.into()))
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
        #[cfg(test)]
        truthy_count::bump();
        match self {
            Value::None => false,
            Value::Bool(b) => *b,
            Value::Int(i) => *i != 0,
            Value::Big(_) => true, // never zero by invariant
            Value::Float(f) => *f != 0.0,
            Value::Str(s) => !s.s.is_empty(),
            Value::Bytes(b) => !b.is_empty(),
            Value::List(l) => !l.borrow().is_empty(),
            Value::Tuple(t) => !t.is_empty(),
            Value::Dict(d) => !d.borrow().is_empty(),
            Value::Range(r) => !r.is_empty(),
            Value::Iter(_) | Value::Func(_) | Value::Builtin(_) | Value::Method(_) => true,
            Value::Class(_) | Value::Super(_) => true,
            Value::Module(_) | Value::Stream(_) | Value::Generator(_) => true,
            Value::Regex(_) | Value::Match(_) => true,
            // A task handle and a channel are objects, not containers: neither
            // is ever falsy, and neither answers `len()` — a channel is not a
            // buffer you inspect but an endpoint you `send`/`recv`/`close`.
            Value::Task(_) | Value::Channel(_) => true,
            // A process handle is an object, not a container: always truthy,
            // and reaped through `wait()` rather than inspected with `len()`.
            Value::Proc(_) => true,
            // An instance is truthy unless its class defines a falsy __len__;
            // the VM overrides this when a __len__/__bool__ dunder is present.
            Value::Instance(_) => true,
            // A type is an object like any other, and objects are truthy.
            Value::Type(_) => true,
            Value::Unbound => false,
        }
    }

    /// The Python-style type name, as returned by `type(x)`.
    ///
    /// `null`'s type is `null`, not CPython's `NoneType`. That name was a
    /// leftover: `None` is not a spelling this language has any more, so a type
    /// named after it pointed at nothing the reader could write. This is a
    /// deliberate divergence and the only one in the table — every other name
    /// here is CPython's, so `corpus/core/` still oracles the whole of the rest
    /// and only the one line moved to `corpus/divergence/`.
    pub fn type_name(&self) -> &'static str {
        self.type_tag().name()
    }

    /// The value's type, as the tag `type(x)` answers with.
    ///
    /// This and [`TypeTag::name`] are the single table: there is no second list
    /// of type names anywhere, so a name and the tag that denotes it cannot
    /// drift apart.
    pub fn type_tag(&self) -> TypeTag {
        match self {
            Value::None => TypeTag::Null,
            Value::Bool(_) => TypeTag::Bool,
            Value::Int(_) | Value::Big(_) => TypeTag::Int,
            Value::Float(_) => TypeTag::Float,
            Value::Str(_) => TypeTag::Str,
            Value::Bytes(_) => TypeTag::Bytes,
            Value::List(_) => TypeTag::List,
            Value::Tuple(_) => TypeTag::Tuple,
            Value::Dict(_) => TypeTag::Dict,
            Value::Range(_) => TypeTag::Range,
            Value::Iter(_) => TypeTag::Iterator,
            Value::Func(_) => TypeTag::Function,
            Value::Builtin(_) => TypeTag::BuiltinFunction,
            Value::Method(_) => TypeTag::Method,
            // A class object's type is `type`, and so is a type tag's: `str`
            // and `Square` are the same kind of thing now, so they answer the
            // same thing.
            Value::Class(_) | Value::Type(_) => TypeTag::Type,
            Value::Instance(_) => TypeTag::Object,
            Value::Super(_) => TypeTag::Super,
            Value::Module(_) => TypeTag::Module,
            Value::Stream(s) => s.kind.type_tag(),
            Value::Generator(_) => TypeTag::Generator,
            Value::Regex(_) => TypeTag::Pattern,
            Value::Match(_) => TypeTag::Match,
            Value::Task(_) => TypeTag::Task,
            Value::Channel(_) => TypeTag::Channel,
            Value::Proc(_) => TypeTag::Proc,
            Value::Unbound => TypeTag::Unbound,
        }
    }

    /// Like [`type_name`](Self::type_name), but returns the actual class name for
    /// a user instance (`Point` rather than the generic `object`). Used where a
    /// diagnostic should match CPython, e.g. attribute errors.
    pub fn type_label(&self) -> String {
        match self {
            Value::Instance(i) => i.class.name.to_string(),
            // A type names itself, whether it is a user class or a builtin
            // one: `str.upper()` should say `str`, exactly as `Point.nope`
            // says `Point`.
            Value::Class(c) => c.name.to_string(),
            Value::Type(t) => t.name().to_string(),
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
            Value::None => "null".to_string(),
            Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Big(b) => b.to_string(),
            Value::Float(f) => format_float(*f),
            Value::Str(s) => repr_str(&s.s),
            Value::Bytes(b) => repr_bytes(b),
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
            // Spelled the way the language accepts it back: the end is the one
            // positional argument, and a start or step that is not the default
            // is named. CPython prints `range(2, 10, 3)`, which is no longer a
            // call Oro will take, and a repr that cannot be typed back in is
            // the inconsistency the argument rule exists to remove.
            Value::Range(r) => {
                let mut out = format!("range({}", r.stop);
                if r.start != 0 {
                    let _ = write!(out, ", start={}", r.start);
                }
                if r.step != 1 {
                    let _ = write!(out, ", step={}", r.step);
                }
                out.push(')');
                out
            }
            Value::Iter(_) => "<iterator>".to_string(),
            Value::Func(f) => format!("<function {}>", f.code.name),
            Value::Builtin(b) => format!("<builtin {}>", b.name),
            Value::Method(_) => "<bound method>".to_string(),
            Value::Class(c) => format!("<class '{}'>", c.name),
            // The same form a class object takes: `str` and `Square` are both
            // types, so they print alike.
            Value::Type(t) => format!("<class '{}'>", t.name()),
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
            Value::Stream(s) => s.repr(),
            Value::Task(t) => format!("<task {}>", t.id),
            Value::Channel(c) => c.repr(),
            Value::Proc(p) => p.repr(),
            Value::Unbound => "<unbound>".to_string(),
        }
    }

    /// The address of the heap cell behind a reference type, or `None` for a
    /// value type. Oro's `id()`, without the builtin.
    ///
    /// These are the types CPython compares and hashes by *identity* rather
    /// than by content, and the reason is the same for all of them: a function
    /// is not the same function because it has the same body, and a stream is
    /// not the same stream because it points at the same file. Two closures
    /// over one code object are two functions, which falls out of this for
    /// free — the `Rc<Function>`s differ even though the `Rc<CodeObject>`s do
    /// not.
    ///
    /// Nothing here pairs up variants, and it does not have to: two *live*
    /// values at one address are the same object, because an `Rc<Function>` and
    /// an `Rc<OroStream>` cannot occupy one address at one time. Every caller
    /// holds both values across the comparison, which is what makes "live"
    /// true of both.
    ///
    /// `Builtin` and `Method` are absent on purpose. A builtin has no single
    /// address — the builtin cache is per call site — so it is compared by
    /// name; a bound method is built fresh on every attribute access, so
    /// `a.m is a.m` is false in CPython too, and it is compared by
    /// (receiver, function) instead. See [`BoundMethod::equals`].
    pub fn identity(&self) -> Option<usize> {
        let p = match self {
            Value::Func(f) => Rc::as_ptr(f) as *const (),
            Value::Generator(g) => Rc::as_ptr(g) as *const (),
            Value::Class(c) => Rc::as_ptr(c) as *const (),
            Value::Instance(i) => Rc::as_ptr(i) as *const (),
            Value::Module(m) => Rc::as_ptr(m) as *const (),
            Value::Stream(s) => Rc::as_ptr(s) as *const (),
            Value::Task(t) => Rc::as_ptr(t) as *const (),
            Value::Channel(c) => Rc::as_ptr(c) as *const (),
            Value::Proc(p) => Rc::as_ptr(p) as *const (),
            Value::Regex(r) => Rc::as_ptr(r) as *const (),
            Value::Match(m) => Rc::as_ptr(m) as *const (),
            Value::Iter(i) => Rc::as_ptr(i) as *const (),
            Value::Super(s) => Rc::as_ptr(s) as *const (),
            _ => return None,
        };
        Some(p as usize)
    }

    /// Equality as used by `==`, `!=`, `in`, and membership — **as far as it
    /// can be decided without running Oro code**. `None` is not a third truth
    /// value: it means the answer depends on a user `__eq__`, which lives in a
    /// frame and can only be reached by the VM (see `Vm::begin_compare`).
    ///
    /// Numbers compare across `bool`/`int`/`float`; unlike types are simply
    /// unequal. Value types compare by content and reference types by identity,
    /// which is CPython's split and was not Oro's: before this, `f == f` was
    /// **false** for a function, a generator, a stream and a task, because
    /// there was no identity arm at all and everything fell through to
    /// `_ => false`. A function that is not equal to itself is a plain
    /// correctness bug, and it is also what stopped a connection registry from
    /// being a `dict` (`docs/stdlib-server-design.md` §7 item 13).
    #[inline]
    pub fn try_equals(&self, other: &Value) -> Option<bool> {
        self.eq_at(other, 0)
    }

    /// [`try_equals`](Self::try_equals) at a known nesting depth.
    ///
    /// The depth is carried so that a *cyclic* structure defers instead of
    /// recursing until the Rust stack dies — `x = []; x.append(x)` compared
    /// against a second such list used to be a hard crash. Only the container
    /// arms look at it, so the scalar path (which is every `==` in a program
    /// without containers on both sides) pays nothing for it.
    #[inline]
    fn eq_at(&self, other: &Value, depth: u32) -> Option<bool> {
        if let (Some(a), Some(b)) = (self.as_number(), other.as_number()) {
            return Some(a.equals(&b));
        }
        match (self, other) {
            (Value::None, Value::None) => Some(true),
            (Value::Str(a), Value::Str(b)) => Some(a.s == b.s),
            (Value::Bytes(a), Value::Bytes(b)) => Some(a == b),
            (Value::List(a), Value::List(b)) => {
                if Rc::ptr_eq(a, b) {
                    return Some(true);
                }
                try_seq_eq(&a.borrow(), &b.borrow(), depth)
            }
            (Value::Tuple(a), Value::Tuple(b)) => {
                if Rc::ptr_eq(a, b) {
                    return Some(true);
                }
                try_seq_eq(a, b, depth)
            }
            (Value::Dict(a), Value::Dict(b)) => {
                if Rc::ptr_eq(a, b) {
                    Some(true)
                } else {
                    try_dict_eq(a, b, depth)
                }
            }
            // A `range` is a sequence, and CPython compares it as one.
            (Value::Range(a), Value::Range(b)) => Some(a.equals(b)),
            // A builtin type is its tag, so two `str`s are one type however
            // they were reached — the name and `type(x)` alike.
            (Value::Type(a), Value::Type(b)) => Some(a == b),
            // The two reference types that are *not* their address.
            (Value::Builtin(a), Value::Builtin(b)) => Some(a.name == b.name),
            (Value::Method(a), Value::Method(b)) => a.try_equals(b, depth),
            // Identity for everything else that has one — except an instance
            // whose class defines `__eq__`, which is the whole point of this
            // returning an `Option`: that pair is handed back to the VM.
            // Unlike types have no identity in common and fall out as unequal.
            //
            // Out of line on purpose. `try_equals` is called on every `==`,
            // `!=`, `in` and dict comparison in the program, and it is small
            // enough to inline into those call sites; folding the instance
            // check and two `identity()` matches into its body would have cost
            // that, for arms that are reached only when both operands are
            // reference types.
            _ => identity_or_defer(self, other),
        }
    }

    /// Ordering for `<`, `<=`, `>`, `>=`, as far as native code can decide it.
    /// Numbers order across numeric types; strings and equal-typed sequences
    /// order lexicographically. Anything else is a `TypeError` — except an
    /// instance, which is `Ok(None)`: only the VM can run `__lt__`.
    ///
    /// `sym` is the operator the *program* wrote, carried only so the
    /// `TypeError` can name it. CPython names the real operator (`'>=' not
    /// supported between …`), and a comparison inside a sequence reports the
    /// operator the sequence was compared with, so it threads down too.
    #[inline]
    pub fn try_compare(
        &self,
        other: &Value,
        sym: &'static str,
    ) -> VResult<Option<std::cmp::Ordering>> {
        if let (Some(a), Some(b)) = (self.as_number(), other.as_number()) {
            return a.compare(&b).map(Some);
        }
        match (self, other) {
            (Value::Str(a), Value::Str(b)) => Ok(Some(a.s.cmp(&b.s))),
            (Value::Bytes(a), Value::Bytes(b)) => Ok(Some(a.cmp(b))),
            (Value::List(a), Value::List(b)) => try_seq_cmp(&a.borrow(), &b.borrow(), sym),
            (Value::Tuple(a), Value::Tuple(b)) => try_seq_cmp(a, b, sym),
            // An instance may define `__lt__`; the VM decides, and produces
            // this same message itself when the class defines nothing.
            (Value::Instance(_), _) | (_, Value::Instance(_)) => Ok(None),
            _ => Err(unorderable(sym, self, other)),
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

/// The field an internally-raised exception carries to record that its single
/// "argument" is an already-rendered *message*, not a constructor argument.
///
/// A runtime fault travels as a `String` — `key error: 'nope'` — and is turned
/// into an exception instance at the point it is raised, by which time the key
/// itself is gone and only its rendering survives. `KeyError` is the one class
/// whose `str()` reprs its argument, so without this flag that rendering would
/// be quoted a second time and `d["nope"]` would print `"'nope'"`.
///
/// The name is unreachable from Oro: attribute access needs an identifier and a
/// NUL is not one, the same trick the `sys.exit` sentinel uses.
pub const RENDERED_MESSAGE: &str = "\u{0}rendered";

/// Reserved field recording where an exception was first raised, as the tuple
/// `(source, line, col)`. Stamped once, the first time the exception unwinds,
/// and never overwritten — so an exception that passes through an `except` clause
/// it does not match, or a `finally`, is still reported at its raise site rather
/// than at the clause it travelled through. NUL-prefixed like
/// [`RENDERED_MESSAGE`], so no Oro attribute name can reach or collide with it.
pub const RAISE_ORIGIN: &str = "\u{0}origin";

/// Whether `class` is `KeyError` or descends from it — the one built-in
/// exception CPython gives a `__str__` of its own.
fn is_key_error(class: &Rc<Class>) -> bool {
    let mut cur = class;
    loop {
        if &*cur.name == "KeyError" {
            return true;
        }
        match cur.base.as_ref() {
            Some(b) => cur = b,
            None => return false,
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
/// args tuple's repr. Matches CPython's `BaseException.__str__` — and its one
/// override, `KeyError`'s.
///
/// `KeyError` is the only built-in exception in CPython that defines a `__str__`
/// of its own, and it reprs its single argument: `KeyError('user')` prints
/// `'user'`, not `user`. The quotes are load-bearing — they are what separates
/// a missing key `user` from a missing key `user ` with a trailing space — and
/// the rule stops at one argument, so `KeyError('a', 'b')` falls back to the
/// args tuple exactly as every other class does. Nothing else in the hierarchy
/// has such a rule; checked against CPython 3.12 across `ValueError`,
/// `IndexError`, `LookupError`, `AttributeError`, `NameError`, `TypeError` and
/// `StopIteration`, all of which are plain `str()`.
pub fn exception_message(inst: &Instance) -> String {
    // An internally-raised exception's argument is already the rendered
    // message, so the `KeyError` rule has been applied to it once already.
    let pre_rendered = inst.fields.borrow().contains_key(RENDERED_MESSAGE);
    let args = exception_args(inst);
    match args.as_slice() {
        [] => String::new(),
        [one] if !pre_rendered && is_key_error(&inst.class) => one.repr(),
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

/// How deep [`Value::eq_at`] will walk before handing the pair to the VM. The
/// VM's own limit (`Vm::CMP_DEPTH_LIMIT`) is what finally turns a cycle into a
/// `RecursionError`; this one only has to stop the *Rust* stack from being the
/// thing that notices, so it is small.
const MAX_EQ_DEPTH: u32 = 64;

/// The identity arm of [`Value::try_equals`], kept out of that function's body
/// so the hot path stays inlinable. See the call site.
///
/// This is where a user `__eq__` is *detected* — never called; calling it means
/// a frame, which only the VM can push. The check is deliberately blind to
/// whether the two are the same object: CPython's `==` operator has no identity
/// shortcut (`a == a` runs `__eq__`, and answers `false` if that is what it
/// says), and the places that *do* shortcut — `in`, and element comparison
/// inside a container — apply it in the VM, which is the only layer that knows
/// which of the two it is doing.
#[inline(never)]
fn identity_or_defer(a: &Value, b: &Value) -> Option<bool> {
    if defines_eq(a) || defines_eq(b) {
        return None;
    }
    Some(match (a.identity(), b.identity()) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    })
}

/// Does deciding equality for this operand need a user `__eq__`? Only an
/// instance can carry one.
pub fn defines_eq(v: &Value) -> bool {
    match v {
        Value::Instance(i) => Class::defines(&i.class, "__eq__"),
        _ => false,
    }
}

/// Out of line, and it has to be: if this were folded into [`Value::eq_at`]
/// that function would become directly recursive, and a recursive function
/// cannot be inlined into the `==` and `in` sites that call it — which is where
/// the whole fast path lives. The same is true of the two below.
#[inline(never)]
fn try_seq_eq(a: &[Value], b: &[Value], depth: u32) -> Option<bool> {
    if a.len() != b.len() {
        return Some(false);
    }
    if depth >= MAX_EQ_DEPTH {
        return a.is_empty().then_some(true);
    }
    for (x, y) in a.iter().zip(b.iter()) {
        match x.eq_at(y, depth + 1) {
            Some(true) => {}
            verdict => return verdict,
        }
    }
    Some(true)
}

#[inline(never)]
fn try_dict_eq(a: &Rc<RefCell<OroDict>>, b: &Rc<RefCell<OroDict>>, depth: u32) -> Option<bool> {
    if depth >= MAX_EQ_DEPTH {
        return None;
    }
    let (a, b) = (a.borrow(), b.borrow());
    if a.len() != b.len() {
        return Some(false);
    }
    for (k, v) in a.items() {
        // The keys are matched by `HKey` and never run user code; only the
        // values are compared with `==` (`docs/hash-and-equality.md`).
        match b.get(k).ok().flatten() {
            None => return Some(false),
            Some(bv) => match bv.eq_at(v, depth + 1) {
                Some(true) => {}
                verdict => return verdict,
            },
        }
    }
    Some(true)
}

#[inline(never)]
fn try_seq_cmp(a: &[Value], b: &[Value], sym: &'static str) -> VResult<Option<std::cmp::Ordering>> {
    for (x, y) in a.iter().zip(b.iter()) {
        match x.try_equals(y) {
            Some(true) => continue,
            Some(false) => return x.try_compare(y, sym),
            None => return Ok(None),
        }
    }
    Ok(Some(a.len().cmp(&b.len())))
}

/// The `TypeError` for a pair that has no ordering, naming the operator the
/// program actually wrote. Shared by the native path and the VM's, so the
/// message does not depend on which of them discovered the problem.
pub fn unorderable(sym: &str, a: &Value, b: &Value) -> crate::exc::VErr {
    // CPython names `type(v)`, which for an instance is its class and for a
    // class object is `type` — so this is `type_name` with the instance arm
    // filled in, not `type_label` (which would call `int` an `int`).
    fn ty(v: &Value) -> String {
        match v {
            Value::Instance(i) => i.class.name.to_string(),
            other => other.type_name().to_string(),
        }
    }
    crate::exc::type_error(format!(
        "'{sym}' not supported between instances of '{}' and '{}'",
        ty(a),
        ty(b)
    ))
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
                .ok_or_else(|| type_error("cannot compare with nan"))
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

/// Is `c` printable in the sense CPython's `repr` means it — i.e. not in
/// general category `Cc`, `Cf`, `Cs`, `Co`, `Cn`, `Zl`, `Zp` or `Zs`, with
/// U+0020 exempted and printable regardless?
///
/// std does not expose general category, but it does expose exactly this
/// predicate by a side door. Rust's own `is_printable` — the one behind
/// `char::escape_debug` — is generated from precisely that category list plus
/// the same space exemption, so `escape_debug` answers the question for us and
/// no table has to be carried here.
///
/// The side door is needed because `char::escape_debug` *also* escapes
/// grapheme-extended characters (combining accents, Indic matras), which
/// CPython prints raw. [`str::escape_debug`] applies that extra rule only to
/// the first character of the string — documented behaviour, not an accident —
/// so putting `c` in second position asks the printability question and
/// nothing else. The leading `a` is arbitrary; any printable ASCII would do.
///
/// The one place the two can disagree is Unicode versions. `Cn` means
/// *unassigned*, and what is unassigned shrinks with every release: a code
/// point CPython's tables have never heard of is `Cn` and gets escaped, while
/// newer tables know it as a letter and print it. Sweeping all 1,112,064
/// non-surrogate code points against CPython 3.12 (Unicode 15.0) from a build
/// against Rust's Unicode 17.0 found exactly 10,615 such points — every one of
/// them `Cn` under 15.0 — and no disagreement of any other kind: no character
/// escaped that CPython prints, and no escape spelled differently. Build the
/// two against the same Unicode version and the gap closes to nothing. That
/// residue is the price of not carrying a Unicode table in this repository,
/// and it is confined to characters that did not exist when CPython was built.
pub(crate) fn is_printable(c: char) -> bool {
    // ASCII is settled without asking: U+0020..U+007E is printable, and the
    // controls either side are `Cc`. Answering here also sidesteps the one
    // place `escape_debug` is not a printability oracle — it escapes `\'` and
    // `"` because it is quoting a literal, not judging the character.
    if c.is_ascii() {
        return (' '..='~').contains(&c);
    }
    // Two std predicates that settle most of the remaining traffic on their
    // own, and settle it exactly. `Alphabetic` is `L*` plus `Nl` plus
    // `Other_Alphabetic` (which lists only `Mn`/`Mc`/`So` characters) and
    // `Numeric` is `Nd`/`Nl`/`No`; none of the eight escaping categories can
    // be either — an unassigned code point has no properties at all — so a
    // `true` here is a proof of printability, not a guess. Between them they
    // cover CJK, Cyrillic, Greek and accented Latin, which is what a
    // non-ASCII string is usually made of, and skip the probe below.
    if c.is_alphanumeric() {
        return true;
    }
    // The mirror image: `White_Space` is a subset of `Cc` ∪ `Zs` ∪ `Zl` ∪
    // `Zp`, and its one printable member is U+0020, already returned above.
    if c.is_control() || c.is_whitespace() {
        return false;
    }
    let mut buf = [0u8; 5];
    buf[0] = b'a';
    let n = c.encode_utf8(&mut buf[1..]).len();
    let probe = std::str::from_utf8(&buf[..1 + n]).expect("ASCII byte then one char");
    let mut it = probe.escape_debug();
    it.next();
    it.next() == Some(c)
}

/// Write the `\xNN` / `\uNNNN` / `\UNNNNNNNN` escape CPython uses for a
/// character it will not print, choosing the shortest form that holds the code
/// point — the same widths, and the same lowercase hex.
pub(crate) fn push_unicode_escape(out: &mut String, c: char) {
    let n = c as u32;
    let _ = if n < 0x100 {
        write!(out, "\\x{n:02x}")
    } else if n < 0x1_0000 {
        write!(out, "\\u{n:04x}")
    } else {
        write!(out, "\\U{n:08x}")
    };
}

/// Produce a Python-style repr of a string, escaping specials. The quote flips
/// to `"` when the data holds a `'` and no `"`, so the common case never needs
/// an escaped quote — the same rule `repr_bytes` follows, and the one CPython
/// uses.
///
/// A repr exists to be read back, so every character CPython considers
/// unprintable is escaped, not just the ASCII controls: a raw U+0080 or a
/// no-break space in the output looks like nothing at all, and pasting it back
/// does not reliably reproduce the value. Printable non-ASCII (`é`, `日本語`)
/// still goes out raw, as it does in Python 3.
pub fn repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let qb = quote as u8;
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    // Copy in runs of plain ASCII rather than character by character: almost
    // every string is entirely such a run, and this way it costs one memcpy.
    let mut run = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if (b.is_ascii_graphic() && b != b'\\' && b != qb) || b == b' ' {
            i += 1;
            continue;
        }
        out.push_str(&s[run..i]);
        if b.is_ascii() {
            match b {
                b'\\' => out.push_str("\\\\"),
                b'\n' => out.push_str("\\n"),
                b'\t' => out.push_str("\\t"),
                b'\r' => out.push_str("\\r"),
                _ if b == qb => {
                    out.push('\\');
                    out.push(quote);
                }
                _ => push_unicode_escape(&mut out, b as char),
            }
            i += 1;
        } else {
            let c = s[i..].chars().next().expect("i is a char boundary");
            if is_printable(c) {
                out.push(c);
            } else {
                push_unicode_escape(&mut out, c);
            }
            i += c.len_utf8();
        }
        run = i;
    }
    out.push_str(&s[run..]);
    out.push(quote);
    out
}

/// Produce a single-quoted Python-style repr of a byte string, `b'...'`. Only
/// printable ASCII is emitted literally; every other octet becomes `\xNN`,
/// because a byte is a number and there is no character to show for it. The
/// quote flips to `"` when the data holds a `'` and no `"`, so the common case
/// never needs an escaped quote — the rule CPython uses.
fn repr_bytes(b: &[u8]) -> String {
    let quote = if b.contains(&b'\'') && !b.contains(&b'"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(b.len() + 3);
    out.push('b');
    out.push(quote);
    for &x in b {
        match x {
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\t' => out.push_str("\\t"),
            b'\r' => out.push_str("\\r"),
            x if x == quote as u8 => {
                out.push('\\');
                out.push(quote);
            }
            0x20..=0x7e => out.push(x as char),
            _ => {
                let _ = write!(out, "\\x{x:02x}");
            }
        }
    }
    out.push(quote);
    out
}

#[cfg(test)]
mod tests;
