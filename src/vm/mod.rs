//! The Oro virtual machine: one flat interpreter loop over a stack of heap
//! [`Frame`]s (architecture point 1).
//!
//! **Calling an Oro function never recurses in Rust.** A call pushes a new
//! [`Frame`] onto the frame stack and the same loop keeps turning; `Return`
//! pops the frame and hands the value back to the caller's operand stack. This
//! is what makes 5000-deep recursion (and, later, generators/coroutines)
//! possible without growing the native stack.
//!
//! That frame stack, and every piece of interpreter state that hangs off it,
//! lives on a [`Task`] rather than on [`Vm`] — the split between *what is
//! running right now* and *what is true of this process*. `spawn` makes a
//! second one, and [`sched`] is the loop above this one that decides which of
//! them is current; switching is a `std::mem::replace` of `Vm::task`.
//!
//! The interpreter loop itself does **not** know the scheduler exists. There is
//! no safepoint check on the dispatch path: a task stops only by producing
//! [`Step::Park`] from the instruction it is executing, which is what
//! "cooperative, not preemptive" means concretely and what keeps the hot path
//! exactly as fast as it was before there was a scheduler at all.

pub mod arith;
mod exceptions;
pub mod modules;
pub mod sched;
mod stdlib;

use std::cell::RefCell;
use std::rc::Rc;

use crate::ast::CmpOp;
use crate::exc::{
    attribute_error, command_error, index_error, key_error, name_error, recursion_error,
    runtime_error, timeout_error, type_error, value_error, Exc, VErr,
};
use crate::compiler::{CaptureSource, ClassSpec, CodeObject, Op, VarTarget};
use crate::task::{TaskHandle, TaskId};
use crate::value::{
    BoundMethod, Class, Fields, Function, Instance, IterState, MethodKind, OroDict, OroList,
    OroTuple, RangeVal, SuperProxy, VResult, Value,
};
use std::collections::{HashMap, VecDeque};
use std::cell::Cell;

/// A runtime error carrying the full source position of the faulting
/// instruction — **file, line and column**, not line and column alone.
///
/// The file is here rather than prepended by the CLI because the CLI only ever
/// knew one file: the script it was handed. A frame running inside an imported
/// module — a user's `helpers.oro`, or `http` from inside the binary — reports
/// a line from *that* file, and pairing it with the script's path produced a
/// location that pointed at nothing while looking exactly like one that did.
///
/// **This struct must stay 40 bytes.** It is the `E` of the
/// `Result<Step, VmError>` that [`Vm::step`] returns on *every*
/// instruction, through a hidden return pointer that is written and read back
/// once per dispatch (see [`Vm::run_slice`]). An `Rc<str>` next to the two
/// `usize`s that were here takes it to 56 bytes and the `Result` to 64, and
/// that alone is **+6.3% across the suite, +10% on `loop`** — measured by
/// building exactly this change and nothing else. The two fields that paid for
/// the file instead: `message` is built once and never appended to, so it is a
/// `Box<str>` rather than a `String`, and a line and a column are `u32` in the
/// span table and `u32` on the task, so they are `u32` here too instead of
/// being widened at this boundary and narrowed back at the next.
/// `vm::tests` asserts the size.
///
/// This cost and the inlining one in [`Vm::err_source`] are independent, and
/// each hid the other: fixing only the width measured no better than the naive
/// version, and fixing only the inlining measured no better either. Both, and
/// it drops from +7.3% to +1.6%. Neither is safe to undo on the grounds that
/// undoing it "made no difference" — that is precisely what each one does
/// while the other is still wrong.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeError {
    /// The exception class this fault raises, named by the code that detected
    /// it. One byte, and it is what replaced `classify_error` — a substring
    /// table that guessed the class back out of `message`, and so let a value
    /// the program (or, through `std/http.oro`, a client) chose decide which
    /// exception a fault became. See [`crate::exc`].
    pub class: Exc,
    pub message: Box<str>,
    /// The file the faulting frame was compiled from. See [`CodeObject::source`].
    pub source: Rc<str>,
    pub line: u32,
    pub col: u32,
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}: {}", self.source, self.line, self.col, self.message)
    }
}

impl std::error::Error for RuntimeError {}

/// The error half of every *internal* fallible VM signature, and the reason it
/// is a `Box`.
///
/// `Vm::step` returns `Result<Step, VmError>` once per instruction, through a
/// hidden return pointer that is written and read back each time. `Step` is 24
/// bytes; `RuntimeError` is 40 (`{Box<str>, Rc<str>, u32, u32}`), so an inline
/// error made that return value 48 — twice the width of the half that carries
/// the actual result, to describe a condition that arises on well under one
/// instruction in a million. Boxed, the pair fits in 24 bytes: the `Result`
/// discriminant rides in `Step`'s own spare tag values and the error costs one
/// pointer.
///
/// The allocation this adds is paid only when a diagnostic is actually built,
/// which is already the `#[cold]` path (see [`Vm::err`]). The public surface
/// (`vm::run`, `vm::run_main`) still hands back a plain `RuntimeError`; the
/// unboxing happens once, at the boundary, per program.
pub(crate) type VmError = Box<RuntimeError>;

/// A guard against runaway recursion. Chosen far above the required 5000 so it
/// only ever trips on genuine infinite recursion, turning an eventual OOM into a
/// clean error.
const MAX_FRAMES: usize = 200_000;

/// How many retired [`Frame`]s to keep for reuse.
///
/// Calls and returns are a stack discipline, so the steady state of any
/// program needs only a handful of recycled frames; the cap is what stops a
/// deeply recursive program from parking 200_000 frames' worth of buffers in
/// the pool after it unwinds.
const FRAME_POOL_MAX: usize = 128;

/// A single activation record. Everything a running function needs lives here,
/// on the heap, in the `frames` vector — never on the Rust call stack.
/// What `apply(f, args=…, kwargs=…)` forwards, once checked: the callee, the
/// list bound by position, and the dict bound by name.
type ApplyOperands = (Value, Vec<Value>, Vec<(String, Value)>);

struct Frame {
    code: Rc<CodeObject>,
    pc: usize,
    /// Plain local slots, sized once by the compiler's pre-pass.
    locals: Vec<Value>,
    /// This frame's own captured cells (shared with inner closures).
    cells: Vec<Rc<RefCell<Value>>>,
    /// Cells captured from enclosing frames.
    free: Vec<Rc<RefCell<Value>>>,
    /// The operand stack.
    stack: Vec<Value>,
    /// What to do with this frame's return value when it returns. Non-`Normal`
    /// only for frames the VM sets up itself (dunder dispatch), never for
    /// ordinary Oro calls.
    ret_action: ReturnAction,
    /// When this frame is a method body: the class it is defined in and the
    /// receiver, so `super()` can search from the base and rebind to `self`.
    super_ctx: Option<(Rc<Class>, Value)>,
    /// Active exception-handling blocks (try/except and try/finally), innermost
    /// on top. Consulted when an exception unwinds through this frame.
    blocks: Vec<Block>,
}

/// A try block registered on a frame while its body runs.
struct Block {
    kind: BlockKind,
    /// Where to jump when this block catches an unwinding exception.
    target: usize,
    /// Operand-stack depth to restore before handling.
    stack_len: usize,
    /// Depths of the VM's in-flight job stacks when this block was entered.
    /// An exception abandons any job started *inside* the block, and those have
    /// to be discarded or they accumulate forever; jobs that were already
    /// running when the block was entered (an enclosing `map`, say) must
    /// survive, which is why this is a depth and not a blanket clear.
    jobs: JobDepths,
}

/// A snapshot of every in-flight job stack, used to unwind them alongside the
/// operand stack. See [`Block::jobs`].
///
/// `u32`, not `usize`: this is copied into every [`Block`], so every `try` and
/// every loop in the program carries one. Seven `usize` would be 56 bytes —
/// more than the five it replaced — and seven `u32` is 28, which is less. A
/// job stack cannot reach 2^32 entries: each job owns heap data and the frame
/// stack itself is capped at `MAX_FRAMES`.
#[derive(Clone, Copy, Default)]
struct JobDepths {
    prints: u32,
    str_jobs: u32,
    seq_jobs: u32,
    mat_jobs: u32,
    cmp_jobs: u32,
    ord_jobs: u32,
}

enum BlockKind {
    /// A try...except: routes here with the exception pushed onto `handling`.
    Except,
    /// A try...finally: routes here with the exception pushed onto the operand
    /// stack so `EndFinally` can re-raise it after the finally body runs.
    Finally,
    /// A loop: `break`/`continue` unwind to it (running enclosing finallys).
    /// `Block.target` is the after-loop target; `cont` is the continue point.
    Loop { cont: usize },
}

/// What the VM does with a frame's return value — the mechanism that lets a
/// native operation (an operator, `str()`, `print()`) invoke an Oro dunder
/// without the interpreter recursing in Rust.
enum ReturnAction {
    /// Push the value onto the caller's operand stack (an ordinary call).
    Normal,
    /// Discard the value; an instance was already left on the caller's stack.
    /// Used for `__init__`, which must return `None`.
    DropForInit,
    /// Feed the returned string into the active `print` job and continue it.
    DrivePrint,
    /// Feed the returned `__repr__` string into the active container-stringify
    /// job and continue it.
    DriveStr,
    /// Push the boolean negation of the return value's truthiness. Used for
    /// `!=` when a class defines `__eq__` but not `__ne__`.
    NegateBool,
    /// Apply an f-string format spec to the returned (string) value, then push.
    FormatSpec(String),
    /// Feed the returned key into the active sort job and continue it.
    /// Feed the returned value into the active map/filter job and continue it.
    DriveSeq,
    /// Feed the returned comparison dunder's value into the active deep
    /// comparison and continue it (`in`, container equality, nested ordering).
    DriveCmp,
    /// A module body finished: capture its namespace into a module value, cache
    /// it under the dotted path, and push it as the import result.
    BuildModule(Rc<str>),
}

/// Why a `finally` body is running — decides what happens after it (see
/// `EndFinally`). This is how a `return` or an exception is threaded *through* a
/// finally so the cleanup still runs.
enum Why {
    Normal,
    Raise(Value),
    Return(Value),
    Break,
    Continue,
}

/// How the interpreter loop should proceed after one instruction.
enum Step {
    /// Advance to the next instruction.
    Next,
    /// The running task's outermost frame returned. For the main task that is
    /// the end of the program; for a spawned one it is that task's outcome.
    Done(Value),
    /// Raise this exception value (unwind the block/frame stack).
    Raise(Value),
    /// Suspend the running task until the [`Park`](sched::Park) reason is
    /// settled. The scheduler files the task away and runs somebody else.
    ///
    /// The payload is boxed for one reason: `Step` is the return value of
    /// `Vm::step`, which runs on every instruction, and a `Park` inline would
    /// widen `Result<Step, VmError>` on the hottest path in the system to
    /// buy nothing — parking is rare, so it can afford an allocation.
    ///
    /// **`Park` carries owned values only, and neither it nor `Step` has a
    /// lifetime parameter.** That is what makes `src/net.rs`'s recorded hazard
    /// — a `RefCell` borrow held across a suspend, which is a `BorrowMutError`
    /// *panic* rather than a catchable exception — unexpressible instead of
    /// merely discouraged. See `sched`'s module docs.
    Park(Box<sched::Park>),
}

/// A `print(...)` call in progress: some arguments still need `__str__`.
struct PrintJob {
    rendered: Vec<String>,
    remaining: Vec<Value>,
    next: usize,
    /// `print(sep=…)` — inserted between arguments. Defaults to a space.
    sep: String,
    /// `print(end=…)` — written after the last argument. Defaults to a newline.
    end: String,
}

/// What is driving a suspended generator frame: a `for` loop (which wants each
/// value on its stack and a jump target on exhaustion), or a materialisation job
/// collecting every value into a list for a native builtin.
#[derive(Clone, Copy)]
enum GenDriver {
    ForLoop(usize),
    Materialize,
}

/// A native builtin was called with a generator argument. Builtins run in Rust
/// and can never re-enter the interpreter, so the generator cannot be drained
/// inside one — it is drained here, a frame at a time, and the call is retried
/// once every generator argument has become a list.
struct MatJob {
    callee: Value,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
    /// Index of the argument currently being drained.
    idx: usize,
    items: Vec<Value>,
    /// When set, `args[0]` is the method receiver rather than an argument, and
    /// the bound method is rebuilt around the drained value on retry.
    receiver_in_args: bool,
}

/// Which sequence adapter is running.
#[derive(Clone, Copy, PartialEq)]
enum SeqOp {
    /// Rebuild the collection from each callback result.
    Map,
    /// Keep the elements whose callback result is truthy.
    Filter,
    /// Like `Map`, but each result must itself be a sequence, concatenated.
    FlatMap,
    /// Reorder by the callback's value (stable).
    SortBy,
    /// Bucket elements into a dict keyed by the callback's value.
    GroupBy,
    /// Split into `(matching, rest)`.
    Partition,
    /// Keep the first element whose callback is truthy, else None. Short-circuits.
    Find,
    /// Whether any / every callback result is truthy. Both short-circuit.
    Any,
    All,
    /// How many callback results are truthy.
    Count,
    /// The element with the smallest / largest callback value.
    MinBy,
    MaxBy,
    /// Drop elements whose callback value has already been seen.
    UniqueBy,
    /// Longest leading run whose callback is truthy, and its complement.
    TakeWhile,
    DropWhile,
    /// Thread an accumulator through: `f(acc, item)`. Sequential, so it cannot
    /// batch its callbacks like the others.
    Reduce,
    /// Not a method: the terminal a fused chain gets when the step that ends it
    /// is one of the callback-less natives (`first()`, `take(n)`). It has no
    /// callback of its own, so every element that survives the stages is its
    /// own result and the collection is rebuilt from them.
    Collect,
}

impl SeqOp {
    fn name(self) -> &'static str {
        match self {
            SeqOp::Map => "map",
            SeqOp::Filter => "filter",
            SeqOp::FlatMap => "flat_map",
            SeqOp::SortBy => "sort",
            SeqOp::GroupBy => "group_by",
            SeqOp::Partition => "partition",
            SeqOp::Find => "find",
            SeqOp::Any => "any",
            SeqOp::All => "all",
            SeqOp::Count => "count",
            SeqOp::MinBy => "min_by",
            SeqOp::MaxBy => "max_by",
            SeqOp::UniqueBy => "unique_by",
            SeqOp::TakeWhile => "take_while",
            SeqOp::DropWhile => "drop_while",
            SeqOp::Reduce => "reduce",
            SeqOp::Collect => "take",
        }
    }

    fn from_name(name: &str) -> Option<SeqOp> {
        Some(match name {
            "map" => SeqOp::Map,
            "filter" => SeqOp::Filter,
            "flat_map" => SeqOp::FlatMap,
            "sort" => SeqOp::SortBy,
            "group_by" => SeqOp::GroupBy,
            "partition" => SeqOp::Partition,
            "find" => SeqOp::Find,
            "any" => SeqOp::Any,
            "all" => SeqOp::All,
            "count" => SeqOp::Count,
            "min_by" => SeqOp::MinBy,
            "max_by" => SeqOp::MaxBy,
            "unique_by" => SeqOp::UniqueBy,
            "take_while" => SeqOp::TakeWhile,
            "drop_while" => SeqOp::DropWhile,
            "reduce" => SeqOp::Reduce,
            _ => return None,
        })
    }

    /// Operations that select or reorder existing elements keep the receiver's
    /// type; ones that change the shape of the data produce a list.
    fn preserves_shape(self) -> bool {
        matches!(
            self,
            SeqOp::Map | SeqOp::Filter | SeqOp::SortBy | SeqOp::UniqueBy
                | SeqOp::TakeWhile | SeqOp::DropWhile | SeqOp::Collect
        )
    }

    /// Whether the answer is built out of the elements the operation was called
    /// on, or only out of what its callback said about them.
    ///
    /// It matters to a fused chain and to nothing else. Unfused, the elements
    /// are the receiver's snapshot and are there whether anyone reads them; a
    /// fused terminal has to be *handed* each element that survives the stages,
    /// and for `map`, `flat_map`, `any`, `all`, `count` and `reduce` that would
    /// be a second full-length vector built only to be thrown away.
    ///
    /// A `limit` is only ever set on a `Collect`, which is on this side of the
    /// line, so counting elements as they arrive is still the right stop test.
    fn keeps_items(self) -> bool {
        !matches!(
            self,
            SeqOp::Map | SeqOp::FlatMap | SeqOp::Any | SeqOp::All | SeqOp::Count | SeqOp::Reduce
        )
    }
}

/// One fused upstream step of a collection chain.
///
/// `xs.filter(p).map(f).filter(q)` is one [`SeqJob`] whose `stages` are the two
/// steps before the last; each element walks the whole pipeline before the next
/// element is read, so the source is read once, the callbacks run in the order
/// a `for` loop would run them, and exactly one collection is built.
#[derive(Clone)]
struct Stage {
    kind: StageKind,
    /// Where the step that contributed this stage is written. A diagnostic the
    /// step itself raises — its callback being a generator function — has to
    /// name that call, and by the time a fused stage runs, the position the VM
    /// is holding belongs to whatever callback returned last.
    line: u32,
    col: u32,
    /// How this stage's callback takes its element, exactly as
    /// [`SeqJob::spread`] does for the terminal step. Settled once, when the
    /// stage is made.
    spread: Spread,
}

/// What a fused stage does with its callback's answer. Only the steps that
/// produce their output one element at a time, in order, with no view of the
/// whole input can be one of these — see [`crate::compiler::chain_defers`] for
/// the ones that cannot and why.
#[derive(Clone)]
enum StageKind {
    /// Replace the element with the callback's value.
    Map(Value),
    /// Keep the element when the callback is truthy, drop it otherwise.
    Filter(Value),
}

/// What the callback result now arriving belongs to.
#[derive(Clone, Copy)]
enum SeqResume {
    /// The terminal step's callback: the value is one of `results`.
    Terminal,
    /// Stage `n`'s callback, deciding about the element held in [`SeqJob::held`].
    Stage(usize),
}

/// A chain step the VM was given permission to defer (by
/// [`crate::compiler::CHAIN_HINT`]) and did, waiting for the step that will run
/// it. It lives only across the handful of instructions between one
/// `CallMethod` and the next — a `LoadMethod` and the inert pushes of the next
/// step's arguments — which is what makes the bookkeeping below sufficient.
struct PendingChain {
    /// The receiver the stages were deferred against, and the value that was
    /// pushed back in place of the step's result. The flushing step must be
    /// called on *this* value, checked by identity, or it is not ours.
    source: Value,
    stages: Vec<Stage>,
    /// The shape an element has coming *out* of the last stage.
    shape: SeqShape,
    /// How deep the frame stack was. A chain is one expression and never spans
    /// frames, so a pending chain whose frame is gone is garbage.
    frame_depth: usize,
}

/// The collection an adapter was called on, and therefore the collection it
/// rebuilds. `map`/`filter` are type-preserving: a tuple stays a tuple and a
/// dict stays a dict, so a chain never silently changes the shape of the data
/// running through it.
#[derive(Clone, Copy)]
enum SeqShape {
    List,
    Tuple,
    Dict,
}

/// How a collection callback is handed each element.
///
/// The rule is the `for` statement's: a callback with two or more parameters
/// destructures its element exactly as `for a, b in xs` does — any element
/// `for` can unpack, and a length mismatch is `for`'s own `ValueError` — and a
/// callback with one takes the element whole. So the protocol's own
/// pair-makers (`enumerate`, `zip`, a dict's `to_list()`, `group_by`) feed its
/// callbacks, and a dict receiver is one instance of the rule rather than an
/// exception to it. [`declared_spread`] says which parameters count.
///
/// Settled once per step, never per element: it is read on every element of
/// every stage, and reading it must not mean walking a parameter list.
#[derive(Clone, Copy)]
enum Spread {
    /// One argument: the element itself.
    Whole,
    /// Exactly this many arguments, always two or more, unpacked from the
    /// element by [`unpack_exact`].
    Unpack(u32),
}

/// An in-flight `.map(f)` / `.filter(p)`. Like [`SeqJob`], the callback is Oro
/// code and must run in a frame, so elements are processed one at a time and the
/// collection is rebuilt once the last result lands.
struct SeqJob {
    op: SeqOp,
    shape: SeqShape,
    /// The elements the terminal step sees. For a dict, each is the
    /// `(key, value)` pair.
    ///
    /// With no fused stages this *is* the receiver's snapshot, walked in place
    /// by `next` — which is what keeps a lone `xs.map(f)` exactly as cheap as
    /// it was before fusion existed. With stages it starts empty and grows as
    /// elements fall out of the pipeline.
    items: Vec<Value>,
    results: Vec<Value>,
    next: usize,
    /// `None` only for [`SeqOp::Collect`], where each element stands in for its
    /// own callback result.
    func: Option<Value>,
    /// How `func` takes each element (see [`Spread`]), settled when the job is
    /// made.
    spread: Spread,
    /// Fused upstream steps, in order. Empty for an unfused single step.
    stages: Vec<Stage>,
    /// The receiver's snapshot when `stages` is non-empty (see `items`).
    src: Vec<Value>,
    /// Values still on their way down the stages, as `(stage index, value)`.
    /// A stack rather than a field because one element can be in flight at a
    /// stage boundary while the job is suspended in a callback.
    work: Vec<(usize, Value)>,
    /// The element a suspended `Filter` stage is deciding about.
    held: Value,
    resume: SeqResume,
    /// Stop once this many elements have reached the terminal. `usize::MAX`
    /// unless a `take(n)` or `first()` ended the chain — the short circuit that
    /// makes `xs.map(f).first()` call `f` once instead of once per element.
    limit: usize,
    /// `first()`: answer with the single element rather than a collection.
    pick_first: bool,
    /// `drop_while`: set once its predicate has first answered false. From then
    /// on every remaining element is kept **without calling the predicate** —
    /// dropping stops at the first false, so the predicate must not run past it.
    drop_done: bool,
    /// `sort(f, reverse=true)`: a stable descending sort. Read by no other
    /// step.
    reverse: bool,
    /// Where the step that ends the chain is written. Only a `first()` on an
    /// empty result reads it, and only because that diagnostic is raised after
    /// the last callback has returned, by which time `task.line`/`col` name the
    /// callback's `return` rather than the call.
    line: u32,
    col: u32,
}

/// Whether two values are the *same* collection — one allocation, not two equal
/// ones. A deferred pipeline belongs to the receiver it was deferred against
/// and to no other, and for a chain over a list `==` is far too weak a test.
fn same_collection(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::List(x), Value::List(y)) => Rc::ptr_eq(x, y),
        (Value::Tuple(x), Value::Tuple(y)) => Rc::ptr_eq(x, y),
        (Value::Dict(x), Value::Dict(y)) => Rc::ptr_eq(x, y),
        (Value::Range(x), Value::Range(y)) => Rc::ptr_eq(x, y),
        _ => false,
    }
}

/// `for a, b in xs`'s unpacking of one element into exactly `n` values: any
/// element `for` can iterate, and a length mismatch is its `ValueError`, word
/// for word. [`Op::UnpackSequence`] and a destructuring collection callback
/// both call this, so the two cannot drift apart.
///
/// Out of line, like [`declared_spread`], for the reason [`Vm::chain_tail`]
/// is: inlined, the two grew the callback driver enough to slow code they do
/// not touch — the receiver snapshot every chain step takes — by 8% on
/// `fusesc`, measured A/B.
#[inline(never)]
fn unpack_exact(seq: &Value, n: usize) -> VResult<Vec<Value>> {
    let items = match seq {
        // The common case — a dict's pair, an `enumerate` or `zip` row — read
        // straight off the tuple rather than through an iterator.
        Value::Tuple(t) => t.as_slice().to_vec(),
        other => iterate_to_vec(other)?,
    };
    if items.len() == n {
        return Ok(items);
    }
    Err(value_error(if items.len() < n {
        format!("not enough values to unpack (expected {n}, got {})", items.len())
    } else {
        format!("too many values to unpack (expected {n})")
    }))
}

/// A condition or boolean operand that is not a `bool`. Oro has no truthiness:
/// `if`, `while`, `and`, `or` and `not` take a `bool` and nothing else, so a
/// non-bool here is a fault that names the explicit test the caller meant. The
/// suggested spelling follows the value's type — emptiness for a collection,
/// `!= 0` for a number, `!= null` for anything that might be absent.
fn not_bool(v: &Value, ctx: &str) -> VErr {
    let suggestion = match v {
        Value::Str(_) | Value::Bytes(_) | Value::List(_) | Value::Tuple(_)
        | Value::Dict(_) | Value::Range(_) => "len(x) != 0",
        Value::Int(_) | Value::Big(_) | Value::Float(_) => "x != 0",
        _ => "x != null",
    };
    type_error(format!(
        "{ctx} must be a bool, not '{}' — Oro has no truthiness, so write the test: `{}`",
        v.type_label(),
        suggestion
    ))
}

/// How many arguments an Oro callback asks for its element to be spread over,
/// or `None` for a native callable, which declares no parameters to count.
///
/// What counts is the positional parameters **without a default**, after the
/// `leading` ones the caller fills itself: the accumulator `reduce` threads,
/// and a bound method's own `self`. A defaulted parameter never counts — it is
/// keyword-only, so a callback cannot be handed one at all — which is why
/// `def f(x, scale=2)` still takes its element whole.
#[inline(never)]
fn declared_spread(func: &Value, leading: usize) -> Option<Spread> {
    let (f, leading) = match func {
        Value::Func(f) => (f, leading),
        Value::Method(m) => match &m.kind {
            MethodKind::User { func, .. } => (func, leading + 1),
            MethodKind::Native(_) => return None,
        },
        _ => return None,
    };
    let asked = f
        .code
        .params
        .iter()
        .filter(|p| !p.has_default)
        .count()
        .saturating_sub(leading);
    Some(if asked >= 2 { Spread::Unpack(asked as u32) } else { Spread::Whole })
}

/// [`declared_spread`] for a collection step over elements of `shape`. A
/// native callable has nothing to count, so it is handed a dict's pair as two
/// arguments and any other element whole.
fn seq_spread(func: &Value, shape: SeqShape, leading: usize) -> Spread {
    declared_spread(func, leading).unwrap_or(match shape {
        SeqShape::Dict => Spread::Unpack(2),
        SeqShape::List | SeqShape::Tuple => Spread::Whole,
    })
}

/// Rendering a container to a string, where some elements are instances whose
/// `__repr__` must run (via a frame). Phase 1 collected those instances; phase 2
/// runs each and fills `results`; phase 3 rebuilds the string splicing them in.
struct StrJob {
    value: Value,
    instances: Vec<Value>,
    results: Vec<String>,
    next: usize,
    cont: StrCont,
}

/// What to do with a finished [`StrJob`] string.
enum StrCont {
    /// Push it (result of `str`/`repr` of a container).
    Push,
    /// Feed it into the active print job and continue printing.
    Print,
    /// Apply an f-string format spec, then push.
    FormatSpec(String),
}

/// How deep a comparison may nest before Oro calls it a cycle. CPython answers
/// the same shape of program with `RecursionError: maximum recursion depth
/// exceeded in comparison`, and so does this — the number is not the same one
/// CPython uses, but the behaviour it is there to produce is.
const CMP_DEPTH_LIMIT: usize = 600;

/// A comparison that native code could not finish on its own — because some
/// pair inside it is an instance whose class defines `__eq__`, `__lt__` or a
/// sibling, and calling one of those means running Oro code.
///
/// The VM never recurses in Rust (module docs), so the "recursion" of a deep
/// comparison is this explicit `levels` stack instead: each level is one
/// suspended `a == b` that is waiting on the answer to a smaller one. That is
/// the same trade [`SeqJob`] makes — the difference is only that
/// what suspends here is an operator rather than a call.
///
/// The fast path never builds one of these. [`Value::try_equals`] and
/// [`Value::try_compare`] answer natively for every pair with no user dunder
/// under it, and a job is created only when one of them says `None`.
struct CmpJob {
    levels: Vec<CmpLevel>,
    cont: CmpCont,
}

/// One suspended level of a deep comparison.
///
/// Every level answers a `bool`, and every level's *questions* are single
/// pairs, which is what keeps the machine flat: a level either asks about one
/// more pair or announces its answer.
enum CmpLevel {
    /// Waiting on a comparison dunder's return value.
    ///
    /// There is no negated form of this. `!=` is normalised to `==` plus one
    /// negation of the *whole* answer before the machine starts, and nothing
    /// inside a container ever consults `__ne__` — CPython compares elements
    /// with `Py_EQ` and negates at the end, and so does this.
    Dunder,
    /// Two sequences, element by element. For `==` every pair must match; for
    /// an ordering the first pair that does *not* match decides the whole
    /// answer by `op`, which is CPython's `list_richcompare` exactly.
    Seq { a: Vec<Value>, b: Vec<Value>, i: usize, op: CmpOp, deciding: bool },
    /// Two dicts' values, paired up by key. The keys were matched by `HKey`
    /// before this level existed and never dispatch `__eq__` — that is the
    /// decision `docs/hash-and-equality.md` argues, and this honours it.
    Vals { a: Vec<Value>, b: Vec<Value>, i: usize },
    /// `item in items`: scan for an element equal to `item`.
    Contains { items: Vec<Value>, item: Value, i: usize },
}

/// What a finished [`CmpJob`] answer is for.
enum CmpCont {
    /// The result of an operator: push it, negated for `!=` / `not in`.
    Push { negate: bool },
    /// The `<` an ordering job asked for: hand it back to [`OrdJob`].
    Order,
}

/// What one step of the pair evaluator did.
enum PairStep {
    /// The pair is decided.
    Done(bool),
    /// It reduces to this other pair (a bound method to its receivers).
    Ask(Value, Value, CmpOp),
    /// A level was pushed; advance it.
    Pushed,
    /// A dunder frame was pushed; the interpreter loop takes over.
    Dispatched,
}

/// What advancing one [`CmpLevel`] did.
enum LevelStep {
    /// The level is decided.
    Done(bool),
    /// It wants this pair compared.
    Ask(Value, Value, CmpOp),
    /// It settled an element without asking anything (the identity shortcut);
    /// advance it again.
    Retry,
}

/// Where the comparison machine goes next: down into a pair, or back up with an
/// answer. One `enum` rather than two functions calling each other, because
/// "calling each other" is the Rust recursion this whole design exists to
/// avoid.
enum CmpNext {
    Ask(Value, Value, CmpOp),
    Give(bool),
}

/// An ordering — `sorted`, `list.sort`, `min`, `max` — whose `<` is a user
/// `__lt__`, and which therefore cannot be a call to `slice::sort_by`.
///
/// Native ordering stays native: [`Vm::begin_order`] scans the elements once
/// for an instance and only builds one of these when it finds one, so a sort of
/// ints or strings runs exactly the code it ran before.
struct OrdJob {
    /// What is being ordered, and what shape the answer takes.
    kind: OrdKind,
    /// Where the answer goes once it has that shape.
    cont: OrdCont,
    /// The values being ordered. For a keyed sort these are the *keys*; `items`
    /// carries what they decorate — except for an in-place sort, whose elements
    /// carried in the job. Read by no in-place path any more.
    keys: Vec<Value>,
    items: Vec<Value>,
    reverse: bool,
    state: OrdState,
}

/// What a finished ordering's value is for — the same shape as [`StrCont`],
/// and for the same reason: an ordering is not always something the program
/// wrote at the top level. `xs.map(sorted)` runs one per element from inside a
/// [`SeqJob`], and `sorted(xs, key=min)` runs one per key from inside a
/// a chain step, and neither wants its answer on the operand stack.
enum OrdCont {
    /// An ordering the program wrote: push it.
    Push,
    /// A collection callback's result: record it and carry on with the chain.
    Seq,
}

/// Which ordering an [`OrdJob`] is carrying out, and where its answer goes.
enum OrdKind {
    /// `xs.sort(f)`: push a new collection of this shape.
    Sort(SeqShape),
    /// `min` / `max` / `min_by` / `max_by`: push the winning element. `who`
    /// is the spelling the program used, so the empty-sequence message names
    /// the call that was actually written.
    Extreme { want_min: bool, who: &'static str },
}

/// The resumable half of an [`OrdJob`].
///
/// The sort is a bottom-up merge sort rather than a call into `slice::sort_by`,
/// for the same reason the comparison machine above is a stack: the comparator
/// can suspend, and a Rust sort has nowhere to suspend to. Bottom-up is chosen
/// because its whole state is five indices — no recursion to make explicit —
/// and because it is stable, which is what `sorted` promises and what makes
/// `reverse=true` invert the comparator instead of the result.
enum OrdState {
    /// A merge sort in progress. It permutes *indices*, not values, so one
    /// permutation reorders the keys and the items it decorates together —
    /// the classic decorate-sort-undecorate, which is what the native
    /// `sort_by_keys` this replaces does too.
    Merge {
        src: Vec<usize>,
        dst: Vec<usize>,
        width: usize,
        lo: usize,
        mid: usize,
        hi: usize,
        i: usize,
        j: usize,
    },
    /// A linear min/max fold: `best` is the index of the winner so far, `next`
    /// the candidate being weighed.
    Fold { best: usize, next: usize },
}

/// One unit of execution: everything that describes *what is running right
/// now*, as opposed to what is true of the whole process.
///
/// The split exists so that a second execution is structurally expressible.
/// A generator suspends a single [`Frame`]; a green thread has to suspend a
/// whole *stack segment* — the frame stack plus every piece of interpreter
/// state that hangs off it (the source position a diagnostic will name, the
/// in-flight `print`/stringify/sort/sequence/materialise jobs, the exception
/// being handled, why each `finally` is running, which generators are being
/// driven). All of that lived directly on [`Vm`], which is exactly what made
/// "one execution per VM" a structural property rather than a choice.
///
/// The VM owns its current task **by value** (`Vm::task`), not behind a
/// pointer. Reaching a per-execution field is still one constant offset from
/// `self` — the offsets simply moved — so the hot dispatch path pays nothing.
/// Switching tasks is a `std::mem::swap` of this struct with a parked one; a
/// `Box<Task>` would have put a dependent load on every frame access instead.
struct Task {
    /// The frame stack. Never the Rust call stack — see the module docs.
    frames: Vec<Frame>,
    /// Source position of the instruction currently executing, which is what
    /// every diagnostic this task raises will name. Per-execution because two
    /// tasks are at two different places in the program.
    line: u32,
    col: u32,
    /// Stack of in-flight `print` calls whose instance args are being rendered
    /// through `__str__`. A `__str__` that itself prints nests cleanly.
    prints: Vec<PrintJob>,
    /// Stack of in-flight container-stringify jobs (see [`StrJob`]).
    str_jobs: Vec<StrJob>,
    seq_jobs: Vec<SeqJob>,
    mat_jobs: Vec<MatJob>,
    /// Stack of in-flight deep comparisons (see [`CmpJob`]). A `__eq__` that
    /// itself compares containers nests cleanly, which is why it is a stack.
    cmp_jobs: Vec<CmpJob>,
    /// Stack of in-flight orderings — sort, min, max — whose `<` is a user
    /// `__lt__` (see [`OrdJob`]).
    ord_jobs: Vec<OrdJob>,
    /// Exceptions currently being handled (top = innermost), for bare `raise`.
    handling: Vec<Value>,
    /// Why each in-flight `finally` body is running, so `EndFinally` can resume
    /// the exception or `return` that was suspended to run the cleanup.
    finally_why: Vec<Why>,
    /// Generators currently being advanced (innermost on top), with the
    /// `ForIter` target to jump to when each is exhausted. Pushed on resume,
    /// popped on `yield`/exhaustion.
    gen_stack: Vec<(Rc<RefCell<crate::value::GenBox>>, GenDriver)>,
    /// The locals of this task's outermost frame, captured when it returns —
    /// i.e. what the execution left behind when it ran out of stack segment.
    /// For the main task that is the module namespace, which is what the VM
    /// unit tests inspect. Written exactly once per task.
    last_locals: Vec<Value>,
    /// This task's identity, unique within the VM and never reused. The main
    /// task is 0; `spawn` allocates upwards from 1.
    id: TaskId,
    /// The `Task` value `spawn` handed back, or `None` for the main task —
    /// which nothing can `join`, which is why its outcome is the program's.
    /// The scheduler keeps this reference for as long as the task lives, so a
    /// discarded handle cannot fire its drop report before the task has run.
    handle: Option<Rc<TaskHandle>>,
    /// An exception a *different* task raised in this one (a joined failure, a
    /// closed channel, a failed import). Unwinding walks `Vm::task`, so it
    /// cannot happen from the waker's side; it rides here until the scheduler
    /// makes this task current. Checked once per switch, never per instruction.
    pending_raise: Option<Value>,
}

impl Task {
    /// A task with nothing running in it yet. Every field is an empty `Vec`,
    /// which does not allocate, so a task costs one struct until it runs.
    fn new() -> Task {
        Task {
            frames: Vec::new(),
            line: 0,
            col: 0,
            prints: Vec::new(),
            str_jobs: Vec::new(),
            seq_jobs: Vec::new(),
            mat_jobs: Vec::new(),
            cmp_jobs: Vec::new(),
            ord_jobs: Vec::new(),
            handling: Vec::new(),
            finally_why: Vec::new(),
            gen_stack: Vec::new(),
            last_locals: Vec::new(),
            id: 0,
            handle: None,
            pending_raise: None,
        }
    }
}

/// The virtual machine.
pub struct Vm {
    /// The execution currently in flight. See [`Task`].
    task: Task,
    /// Chain steps deferred by [`crate::compiler::CHAIN_HINT`] and not yet run.
    ///
    /// On the VM rather than on the [`Task`], because a pending chain lives
    /// only between one `CallMethod` and the next — a `LoadMethod` and the
    /// inert pushes of the next step's arguments — and nothing in that gap can
    /// yield. It is therefore empty at every point a task can be switched at,
    /// and putting it here keeps `Task`'s layout, which the dispatch loop reads
    /// on every instruction, exactly as it was.
    chains: Vec<PendingChain>,
    /// Retired frames, kept for their buffer capacity. See [`Vm::take_frame`].
    ///
    /// Process-wide on purpose: buffers a finished task hands back are exactly
    /// the shape the next task's first call wants, and a per-task pool would
    /// re-`malloc` them once per task.
    frame_pool: Vec<Frame>,
    /// The built-in exception classes, by name (shared identity for the run).
    excs: HashMap<&'static str, Rc<Class>>,
    /// Program arguments, exposed as `sys.argv`.
    argv: Vec<String>,
    /// Set when `sys.exit(code)` runs; becomes the process exit status.
    exit_code: Option<i32>,
    /// Directory user modules are resolved against — the single search-path
    /// rule (the main script's directory). No runtime mutation.
    ///
    /// Empty when the script is in the working directory, rather than `"."`.
    /// The two resolve identically (`fs` treats `helpers.oro` and
    /// `./helpers.oro` the same), but this path is now also *shown*: it is what
    /// a diagnostic from inside `helpers.oro` names, and `./helpers.oro` is
    /// not how anyone writes that file's name.
    import_root: std::path::PathBuf,
    /// What to call the file when a diagnostic is built with no frame left to
    /// ask. Set from the top-level module's own code object, so it is the
    /// script path the user typed. The one case that reaches it is the
    /// implicit join-all's deadlock report, raised after the main task has
    /// already been retired and its frames released.
    main_source: Rc<str>,
    /// Imported user modules by dotted path (module identity), run once.
    module_cache: HashMap<String, Value>,
    /// Module bodies that are currently running, and **which task** is running
    /// each. The owning task matters: a path this task is already initialising
    /// is a genuine cycle, but the same path in *another* task is only a
    /// rendezvous to wait on. Keying by path alone reported the second task's
    /// perfectly ordinary import as `circular import detected`.
    importing: HashMap<String, TaskId>,
    /// Tasks parked waiting for someone else's in-flight module body, by path.
    import_waiters: HashMap<String, Vec<TaskId>>,
    /// The class of `proc.run`'s result (a `Completed`).
    proc_class: Rc<Class>,
    /// Ids handed out by `spawn`. Monotonic, so an id in a waiter queue can
    /// never be mistaken for a later task's.
    next_task_id: TaskId,
    /// Tasks that can run now, oldest first. Holds whole stack segments rather
    /// than ids: nothing ever needs to find a *ready* task by identity, and a
    /// side table would be one more thing to keep in step.
    ready: VecDeque<Task>,
    /// Tasks that cannot run until something settles, by id.
    parked: HashMap<TaskId, sched::Parked>,
    /// Set when an unjoined failed task's last handle is dropped (§3 rule 3),
    /// which turns the process exit code into 1. Shared with every handle
    /// rather than global, because a `Vm` is a value and the tests build many.
    failure_flag: Rc<Cell<bool>>,
    /// The mio reactor: readiness for parked I/O, and the deadline list.
    ///
    /// Lazy where it counts — a program that never opens a socket and never
    /// sleeps never creates an epoll fd or an event buffer (see
    /// [`sched::Reactor`]) — and boxed where *that* counts. Inline, the struct
    /// put 128 bytes between `Vm`'s hot fields and its cold ones and cost the
    /// dispatch-bound benchmarks about 5%, which is a real number for a change
    /// that does nothing on those programs. One 128-byte allocation per `Vm`,
    /// once, buys it back.
    reactor: Box<sched::Reactor>,
    /// Distinguishes one park of a task from the next, so a deadline armed for
    /// an operation that has since finished can be recognised and dropped.
    park_seq: u64,
    /// Task switches since the program started, for the periodic non-blocking
    /// reactor sweep. Only ever incremented while something is registered.
    tick: u64,
}

/// Add two values with the VM's numeric/sequence `+` semantics. Exposed for
/// the `sum` builtin so it need not reimplement the numeric tower.
pub fn add_values(a: &Value, b: &Value) -> VResult<Value> {
    arith::binary(&Op::BinAdd, a, b)
}

/// Run a compiled module to completion, returning its (ignored) result. Used by
/// tests; `sys.argv` is empty.
pub fn run(code: Rc<CodeObject>) -> Result<Value, RuntimeError> {
    let mut vm = Vm::new(Vec::new());
    vm.push_module_frame(code);
    vm.run_loop().map_err(|e| *e)
}

/// Run a program for the `oro` binary, returning the process exit code (0 on
/// normal completion, or the argument of `sys.exit`). `argv[0]` is the script.
pub fn run_main(code: Rc<CodeObject>, argv: Vec<String>) -> Result<i32, RuntimeError> {
    let mut vm = Vm::new(argv);
    // User modules resolve against the main script's directory (argv[0]).
    if let Some(script) = vm.argv.first() {
        if let Some(dir) = std::path::Path::new(script).parent() {
            if !dir.as_os_str().is_empty() {
                vm.import_root = dir.to_path_buf();
            }
        }
    }
    vm.push_module_frame(code);
    let outcome = vm.run_loop();
    // Deterministic drop is the whole mechanism behind §3 rule 3, so the
    // program's last locals are released *here*, before the exit code is read:
    // a `Task` handle the program was still holding reports its unjoined
    // failure now, not after the status has already been decided.
    vm.task = Task::new();
    vm.ready.clear();
    vm.parked.clear();
    let failed_task = vm.failure_flag.get();
    match outcome {
        // An explicit `sys.exit(code)` is a deliberate act and outranks the
        // failure flag; without one, a task that died unjoined makes the run a
        // failure even though the program itself finished.
        Ok(_) => Ok(vm.exit_code.unwrap_or(i32::from(failed_task))),
        // A `sys.exit` surfaces as an uncaught SystemExit; honour its code
        // rather than reporting it as an error.
        Err(e) => match vm.exit_code {
            Some(code) => Ok(code),
            None => Err(*e),
        },
    }
}

impl Vm {
    fn new(argv: Vec<String>) -> Vm {
        Vm {
            task: Task::new(),
            chains: Vec::new(),
            frame_pool: Vec::new(),
            excs: exceptions::build_registry(),
            argv,
            exit_code: None,
            import_root: std::path::PathBuf::new(),
            main_source: Rc::from("<unknown>"),
            module_cache: HashMap::new(),
            importing: HashMap::new(),
            import_waiters: HashMap::new(),
            proc_class: Rc::new(Class {
                name: Rc::from("Completed"),
                base: None,
                members: RefCell::new(Fields::new()),
                is_exception: false,
            }),
            next_task_id: 0,
            ready: VecDeque::new(),
            parked: HashMap::new(),
            failure_flag: Rc::new(Cell::new(false)),
            reactor: Box::default(),
            park_seq: 0,
            tick: 0,
        }
    }

    /// A frame ready to run `code`, reusing a retired frame's buffers when one
    /// is available.
    ///
    /// Two heap allocations per call — the `locals` vector, sized by the
    /// compiler's pre-pass, and the operand `stack`'s first growth — were the
    /// dominant cost on the call path, and a call-heavy program does nothing
    /// but pay them. A returning frame's buffers are exactly the shape the next
    /// call wants, so they are kept and refilled rather than freed and
    /// re-`malloc`'d microseconds later.
    fn take_frame(&mut self, code: Rc<CodeObject>, free: &[Rc<RefCell<Value>>]) -> Frame {
        let nlocals = code.nlocals;
        let ncells = code.ncells;
        match self.frame_pool.pop() {
            // `recycle` emptied every buffer already; only capacity was kept.
            Some(mut f) => {
                f.code = code;
                f.pc = 0;
                f.locals.resize(nlocals, Value::Unbound);
                f.cells.extend((0..ncells).map(|_| Rc::new(RefCell::new(Value::Unbound))));
                f.free.extend_from_slice(free);
                f
            }
            None => Frame {
                locals: vec![Value::Unbound; nlocals],
                cells: (0..ncells).map(|_| Rc::new(RefCell::new(Value::Unbound))).collect(),
                free: free.to_vec(),
                stack: Vec::new(),
                pc: 0,
                code,
                ret_action: ReturnAction::Normal,
                super_ctx: None,
                blocks: Vec::new(),
            },
        }
    }

    /// Retire a finished frame into the pool.
    ///
    /// The buffers are emptied *here*, when the frame dies, rather than lazily
    /// on reuse: pooling is a memory optimisation and must not extend the
    /// lifetime of a single value the frame was holding.
    fn recycle(&mut self, mut frame: Frame) {
        if self.frame_pool.len() >= FRAME_POOL_MAX {
            return;
        }
        frame.locals.clear();
        frame.cells.clear();
        frame.free.clear();
        frame.stack.clear();
        frame.blocks.clear();
        frame.super_ctx = None;
        frame.ret_action = ReturnAction::Normal;
        self.frame_pool.push(frame);
    }

    fn push_module_frame(&mut self, code: Rc<CodeObject>) {
        self.main_source = code.source.clone();
        let frame = Frame {
            locals: vec![Value::Unbound; code.nlocals],
            cells: (0..code.ncells).map(|_| Rc::new(RefCell::new(Value::Unbound))).collect(),
            free: Vec::new(),
            stack: Vec::new(),
            pc: 0,
            code,
            ret_action: ReturnAction::Normal,
            super_ctx: None,
            blocks: Vec::new(),
        };
        self.task.frames.push(frame);
    }
}

impl Vm {
    // --- Operand-stack helpers (always the top frame) ------------------------

    fn top(&mut self) -> &mut Frame {
        self.task.frames.last_mut().expect("no active frame")
    }

    fn push(&mut self, v: Value) {
        self.top().stack.push(v);
    }

    fn pop(&mut self) -> Value {
        self.top().stack.pop().expect("operand stack underflow")
    }

    fn popn(&mut self, n: usize) -> Vec<Value> {
        let stack = &mut self.top().stack;
        stack.split_off(stack.len() - n)
    }

    /// The file the running frame was compiled from — the other half of the
    /// location `self.task.line`/`col` already hold.
    ///
    /// A load through `frame.code`, an `Rc` clone, and it happens **only when a
    /// diagnostic is being built**. Nothing about it is on the dispatch path:
    /// the per-instruction fetch still writes exactly the two `u32`s it always
    /// did, and reads nothing new.
    ///
    /// # The attributes are load-bearing
    ///
    /// `#[cold]` and `#[inline(never)]`, here and on [`Vm::err`], are worth
    /// **4.7% across the benchmark suite** and were measured, not assumed.
    ///
    /// "Off the hot path" is not the same as "not in the hot function".
    /// `Vm::err` is called from something like a hundred places inside
    /// `Vm::step`, which is the largest function in the system and the one the
    /// dispatch loop calls per instruction. Left inlinable, three extra
    /// instructions in `err` become three hundred inside `step`, and `step` is
    /// already at the size where growing it changes what LLVM will do with the
    /// loop around it: the naive version of this change measured **+7.3%** with
    /// nothing added to any instruction's execution. Out of line, each site is
    /// a call it never makes, and the cost falls to ~1.5%.
    ///
    /// The converse was measured too and is not an invitation: pushing
    /// `unwind`, `uncaught_error` and `error_to_exception` out of line as well
    /// took it back to **+5.6%**, and hoisting the `err_source()` call out of
    /// `unwind` into `run_slice` — the loop itself — cost **+12.6%**. The rule
    /// this leaves is narrow and worth keeping: the *error constructors* stay
    /// out of line, and everything else stays where it was.
    #[cold]
    #[inline(never)]
    fn err_source(&self) -> Rc<str> {
        match self.task.frames.last() {
            Some(f) => f.code.source.clone(),
            None => self.main_source.clone(),
        }
    }

    /// Build a diagnostic at the running instruction. See [`Vm::err_source`]
    /// for why this is `#[cold]` and out of line.
    #[cold]
    #[inline(never)]
    fn err(&self, e: VErr) -> VmError {
        Box::new(RuntimeError {
            class: e.class,
            message: e.message.into_boxed_str(),
            source: self.err_source(),
            line: self.task.line,
            col: self.task.col,
        })
    }

    fn wrap<T>(&self, r: VResult<T>) -> Result<T, VmError> {
        r.map_err(|e| self.err(e))
    }

    /// The list at the top of the stack (left in place), for the incremental
    /// call-argument assembly ops.
    fn expect_list_tos(&mut self, who: &str) -> Result<Rc<OroList>, VmError> {
        match self.top().stack.last() {
            Some(Value::List(l)) => Ok(l.clone()),
            _ => Err(self.err(runtime_error(format!("internal: {who} on non-list")))),
        }
    }

    fn expect_dict_tos(&mut self, who: &str) -> Result<Rc<RefCell<OroDict>>, VmError> {
        match self.top().stack.last() {
            Some(Value::Dict(d)) => Ok(d.clone()),
            _ => Err(self.err(runtime_error(format!("internal: {who} on non-dict")))),
        }
    }

    // --- The interpreter loop ------------------------------------------------

    /// Run the current task until it parks, returns, or dies — and no further.
    ///
    /// This is the old `run_loop` with two changes and no third: `Step::Park`
    /// gets an arm, and the two terminal paths report a *task* outcome instead
    /// of a process one. There is deliberately no scheduler check inside the
    /// loop; cooperative scheduling means the only way out is a `Step` the
    /// running instruction produced, which is what keeps the dispatch path
    /// exactly as fast as M2 left it (§3, "Cooperative, not preemptive").
    fn run_slice(&mut self) -> sched::Slice {
        // An exception another task raised in this one (a joined failure, a
        // channel closing, a module body that died) lands *between*
        // instructions, never inside one — the same guarantee that lets a task
        // suspend at all.
        if let Some(exc) = self.task.pending_raise.take() {
            if let Some((uncaught, source)) = self.unwind(exc) {
                let err = self.uncaught_error(&uncaught, source);
                return sched::Slice::Failed(uncaught, err);
            }
        }
        loop {
            // Fetch, advance, release — in a single borrow of the frame stack.
            // `Op` is a `Copy` word, so reading the instruction out costs a
            // register move, and the borrow can end before `step` runs (which
            // it must: executing a call or a return restructures `frames`).
            let op = {
                let frame = self.task.frames.last_mut().expect("no active frame");
                let pc = frame.pc;
                frame.pc = pc + 1;
                let (l, c) = frame.code.spans[pc];
                self.task.line = l;
                self.task.col = c;
                frame.code.ops[pc]
            };

            // The instructions a loop body is made of, executed here rather
            // than through `step`.
            //
            // `step` is one enormous match that returns
            // `Result<Step, VmError>` — 48 bytes, written through a
            // hidden return pointer and read back — and it is far too large
            // for LLVM to inline into this loop. Every `i = i + 1` therefore
            // paid a call, a 48-byte store and a 48-byte load to move one
            // integer between a slot and the operand stack. These arms are
            // the same semantics with none of that protocol: the ones that
            // always apply end in `continue`, and the ones that only apply to
            // a shape (two `Int`s, a `Bool` condition, a bound local) leave
            // the operand stack **untouched** when the shape is wrong and fall
            // out of the match, so `step` below runs exactly as it always did
            // and produces exactly the value or the diagnostic it always did.
            // Nothing here may be the only place a case is handled.
            match op {
                Op::LoadFast(slot) => {
                    let frame = self.task.frames.last_mut().expect("no active frame");
                    let v = &frame.locals[slot as usize];
                    if !matches!(v, Value::Unbound) {
                        let v = v.clone();
                        frame.stack.push(v);
                        continue;
                    }
                }
                Op::StoreFast(slot) => {
                    let frame = self.task.frames.last_mut().expect("no active frame");
                    let v = frame.stack.pop().expect("operand stack underflow");
                    frame.locals[slot as usize] = v;
                    continue;
                }
                Op::LoadConst(i) => {
                    let frame = self.task.frames.last_mut().expect("no active frame");
                    let v = frame.code.consts[i as usize].clone();
                    frame.stack.push(v);
                    continue;
                }
                Op::Jump(target) => {
                    self.task.frames.last_mut().expect("no active frame").pc = target as usize;
                    continue;
                }
                Op::BinAdd | Op::BinSub | Op::BinMul => {
                    let frame = self.task.frames.last_mut().expect("no active frame");
                    let n = frame.stack.len();
                    if n >= 2 {
                        if let (Value::Int(a), Value::Int(b)) =
                            (&frame.stack[n - 2], &frame.stack[n - 1])
                        {
                            // `checked_*`: an overflow promotes to `Big`, which
                            // is `arith`'s job, so it falls through untouched.
                            let r = match op {
                                Op::BinAdd => a.checked_add(*b),
                                Op::BinSub => a.checked_sub(*b),
                                _ => a.checked_mul(*b),
                            };
                            if let Some(r) = r {
                                frame.stack.truncate(n - 1);
                                frame.stack[n - 2] = Value::Int(r);
                                continue;
                            }
                        }
                    }
                }
                Op::Compare(cmp) => {
                    let frame = self.task.frames.last_mut().expect("no active frame");
                    let n = frame.stack.len();
                    if n >= 2 {
                        if let (Value::Int(a), Value::Int(b)) =
                            (&frame.stack[n - 2], &frame.stack[n - 1])
                        {
                            // `in`/`not in` on two integers is not a
                            // comparison at all, so it declines here rather
                            // than in an arm guard: a guard on one arm of this
                            // match costs the whole match its jump table.
                            let r = match cmp {
                                CmpOp::Eq => Some(a == b),
                                CmpOp::NotEq => Some(a != b),
                                CmpOp::Lt => Some(a < b),
                                CmpOp::Gt => Some(a > b),
                                CmpOp::LtEq => Some(a <= b),
                                CmpOp::GtEq => Some(a >= b),
                                CmpOp::In | CmpOp::NotIn => None,
                            };
                            if let Some(r) = r {
                                frame.stack.truncate(n - 1);
                                frame.stack[n - 2] = Value::Bool(r);
                                continue;
                            }
                        }
                    }
                }
                Op::PopJumpIfFalse(target) | Op::PopJumpIfTrue(target) => {
                    let frame = self.task.frames.last_mut().expect("no active frame");
                    if let Some(Value::Bool(b)) = frame.stack.last() {
                        let want = matches!(op, Op::PopJumpIfTrue(_));
                        let take = *b == want;
                        frame.stack.pop();
                        if take {
                            frame.pc = target as usize;
                        }
                        continue;
                    }
                }
                // The ordinary call and the ordinary return, which between
                // them are the whole cost of `fib`. Both conditions are
                // exactly the ones the general paths test for themselves —
                // `fast_call_target` decides the call, and a return is plain
                // when there is no `finally` to run on the way out, an outer
                // frame to return into, and nothing for the VM to do with the
                // value but push it. Anything else falls through to `step`.
                Op::Call(n) => {
                    if self.task.frames.len() < MAX_FRAMES {
                        if let Some(func) = self.fast_call_target(n as usize) {
                            self.call_fast_unchecked(func, n as usize);
                            continue;
                        }
                    }
                }
                Op::Return => {
                    let plain = self.task.frames.len() > 1 && {
                        let frame = self.task.frames.last().expect("no active frame");
                        !frame.code.is_generator
                            && frame.blocks.is_empty()
                            && matches!(frame.ret_action, ReturnAction::Normal)
                    };
                    if plain {
                        let mut frame = self.task.frames.pop().expect("return with no frame");
                        let value = frame.stack.pop().expect("operand stack underflow");
                        self.recycle(frame);
                        self.task
                            .frames
                            .last_mut()
                            .expect("a caller, checked above")
                            .stack
                            .push(value);
                        continue;
                    }
                }
                Op::Pop => {
                    let frame = self.task.frames.last_mut().expect("no active frame");
                    frame.stack.pop().expect("operand stack underflow");
                    continue;
                }
                Op::LoadNone => {
                    self.task
                        .frames
                        .last_mut()
                        .expect("no active frame")
                        .stack
                        .push(Value::None);
                    continue;
                }
                Op::LoadGlobal(n) => {
                    // The resolved-builtin cache hit. A miss (or an exception
                    // class, which is never cached) falls through and resolves
                    // exactly as before.
                    let frame = self.task.frames.last_mut().expect("no active frame");
                    let hit = frame.code.builtin_cache.borrow()[n as usize].clone();
                    if let Some(v) = hit {
                        frame.stack.push(v);
                        continue;
                    }
                }
                Op::LoadAttr(n) => {
                    // An instance *field*. A method, a class attribute, a
                    // module member or anything that is not an instance at all
                    // declines, and `step` then repeats this same lookup — the
                    // one it always did first — before going on.
                    let frame = self.task.frames.last_mut().expect("no active frame");
                    let hit = match frame.stack.last() {
                        Some(Value::Instance(inst)) => {
                            inst.fields.borrow().get(&frame.code.names[n as usize]).cloned()
                        }
                        _ => None,
                    };
                    if let Some(v) = hit {
                        *frame.stack.last_mut().expect("the receiver") = v;
                        continue;
                    }
                }
                Op::LoadSubscript => {
                    // `xs[i]` for a list and a non-negative in-range index. A
                    // negative index, a dict, a string, a slice or an
                    // out-of-range index declines to `subscript_get`.
                    let frame = self.task.frames.last_mut().expect("no active frame");
                    let n = frame.stack.len();
                    if n >= 2 {
                        let hit = match (&frame.stack[n - 2], &frame.stack[n - 1]) {
                            (Value::List(l), Value::Int(i)) => {
                                let l = l.borrow();
                                let i = *i;
                                if i >= 0 && (i as usize) < l.len() {
                                    Some(l[i as usize].clone())
                                } else {
                                    None
                                }
                            }
                            // A missing key is a `KeyError` with a message to
                            // build, and an unhashable one is an error too, so
                            // both decline.
                            (Value::Dict(d), k) => match d.borrow().get(k) {
                                Ok(Some(v)) => Some(v),
                                _ => None,
                            },
                            _ => None,
                        };
                        if let Some(v) = hit {
                            frame.stack.truncate(n - 1);
                            frame.stack[n - 2] = v;
                            continue;
                        }
                    }
                }
                Op::StoreSubscript => {
                    // `xs[i] = v`, same shape and the same declines.
                    let frame = self.task.frames.last_mut().expect("no active frame");
                    let n = frame.stack.len();
                    // 0 = decline, 1 = list slot, 2 = hashable dict key.
                    let shape = if n < 3 {
                        0
                    } else {
                        match (&frame.stack[n - 2], &frame.stack[n - 1]) {
                            (Value::List(l), Value::Int(i)) => {
                                if *i >= 0 && (*i as usize) < l.borrow().len() {
                                    1
                                } else {
                                    0
                                }
                            }
                            // The four key shapes that are hashable by
                            // construction, so `insert` cannot fail. Anything
                            // else — including a key that owes an
                            // unhashable-type diagnostic — declines rather
                            // than hash itself twice to find out.
                            (
                                Value::Dict(_),
                                Value::Int(_) | Value::Str(_) | Value::Bool(_) | Value::None,
                            ) => 2,
                            _ => 0,
                        }
                    };
                    if shape != 0 {
                        let index = frame.stack.pop().expect("subscript index");
                        let target = frame.stack.pop().expect("subscript target");
                        let value = frame.stack.pop().expect("operand stack underflow");
                        match (target, index) {
                            (Value::List(l), Value::Int(i)) => l.borrow_mut()[i as usize] = value,
                            (Value::Dict(d), k) => {
                                d.borrow_mut().insert(k, value).expect("key hashed above")
                            }
                            _ => unreachable!("checked above"),
                        }
                        continue;
                    }
                }
                Op::Dup => {
                    let frame = self.task.frames.last_mut().expect("no active frame");
                    let v = frame.stack.last().expect("dup on empty stack").clone();
                    frame.stack.push(v);
                    continue;
                }
                Op::RotTwo => {
                    let stack = &mut self.task.frames.last_mut().expect("no active frame").stack;
                    let n = stack.len();
                    stack.swap(n - 1, n - 2);
                    continue;
                }
                Op::LoadCell(slot) => {
                    let frame = self.task.frames.last_mut().expect("no active frame");
                    let v = frame.cells[slot as usize].borrow().clone();
                    if !matches!(v, Value::Unbound) {
                        frame.stack.push(v);
                        continue;
                    }
                }
                Op::LoadFree(slot) => {
                    let frame = self.task.frames.last_mut().expect("no active frame");
                    let v = frame.free[slot as usize].borrow().clone();
                    if !matches!(v, Value::Unbound) {
                        frame.stack.push(v);
                        continue;
                    }
                }
                Op::ListAppend => {
                    let frame = self.task.frames.last_mut().expect("no active frame");
                    let n = frame.stack.len();
                    if n >= 2 {
                        if let Value::List(l) = &frame.stack[n - 2] {
                            let l = l.clone();
                            let v = frame.stack.pop().expect("the appended value");
                            l.borrow_mut().push(v);
                            continue;
                        }
                    }
                }
                Op::ForIter(target) => {
                    // The ordinary iterator: a range, a list, a tuple, a
                    // string, a dict's keys. A generator resumes a frame and a
                    // channel can park, so both decline — as does the mutated
                    // list `iter_next` refuses to walk, which owes a
                    // diagnostic. `iter_next` does not mutate on its error
                    // path, so running it again below is the same call twice,
                    // not a half-taken step.
                    let frame = self.task.frames.last_mut().expect("no active frame");
                    let advance = match frame.stack.last() {
                        Some(it @ Value::Iter(_)) => iter_next_pair(it).ok(),
                        _ => None,
                    };
                    match advance {
                        Some(Some((i, v))) => {
                            frame.stack.push(Value::Tuple(OroTuple::new(vec![i, v])));
                            continue;
                        }
                        Some(None) => {
                            frame.stack.pop().expect("the exhausted iterator");
                            frame.pc = target as usize;
                            continue;
                        }
                        None => {}
                    }
                }
                _ => {}
            }

            // Execute one op. A failing operation or a `raise` produces an
            // exception that unwinds the block/frame stack; if nothing catches
            // it, the run ends with that error.
            let to_raise = match self.step(op) {
                Ok(Step::Next) => continue,
                Ok(Step::Done(v)) => return sched::Slice::Returned(v),
                Ok(Step::Raise(exc)) => exc,
                Ok(Step::Park(park)) => return sched::Slice::Parked(park),
                Err(e) => match self.exit_request(&e) {
                    Some(exc) => exc,
                    None => self.error_to_exception(&e),
                },
            };
            if let Some((uncaught, source)) = self.unwind(to_raise) {
                let err = self.uncaught_error(&uncaught, source);
                return sched::Slice::Failed(uncaught, err);
            }
        }
    }

    /// Execute a single instruction, reporting how the loop should proceed.
    fn step(&mut self, op: Op) -> Result<Step, VmError> {
            match op {
                Op::LoadConst(i) => {
                    let v = self.task.frames.last().unwrap().code.consts[i as usize].clone();
                    self.push(v);
                }
                Op::LoadNone => self.push(Value::None),
                Op::LoadFast(s) => {
                    let v = self.top().locals[s as usize].clone();
                    if matches!(v, Value::Unbound) {
                        return Err(self.err(self.unbound_local_err(s)));
                    }
                    self.push(v);
                }
                Op::StoreFast(s) => {
                    let v = self.pop();
                    self.top().locals[s as usize] = v;
                }
                Op::LoadCell(s) => {
                    let v = self.top().cells[s as usize].borrow().clone();
                    if matches!(v, Value::Unbound) {
                        return Err(self.err(name_error(
                            "local variable referenced before assignment",
                        )));
                    }
                    self.push(v);
                }
                Op::StoreCell(s) => {
                    let v = self.pop();
                    *self.top().cells[s as usize].borrow_mut() = v;
                }
                Op::LoadFree(s) => {
                    let v = self.top().free[s as usize].borrow().clone();
                    if matches!(v, Value::Unbound) {
                        return Err(self.err(name_error(
                            "free variable referenced before assignment",
                        )));
                    }
                    self.push(v);
                }
                Op::StoreFree(s) => {
                    let v = self.pop();
                    *self.top().free[s as usize].borrow_mut() = v;
                }
                Op::LoadGlobal(n) => {
                    // Globals are the builtin functions plus the exception
                    // classes. Builtins resolve straight out of this call site's
                    // cache; see `CodeObject::builtin_cache` for why they are
                    // the only half that is cached.
                    let idx = n as usize;
                    let frame = self.task.frames.last().expect("no active frame");
                    let hit = frame.code.builtin_cache.borrow()[idx].clone();
                    match hit {
                        Some(v) => self.push(v),
                        None => {
                            let name = frame.code.names[idx].clone();
                            let resolved = exceptions::lookup(&self.excs, &name)
                                .or_else(|| crate::builtins::lookup(&name));
                            match resolved {
                                Some(v) => {
                                    if matches!(v, Value::Builtin(_)) {
                                        frame.code.builtin_cache.borrow_mut()[idx] =
                                            Some(v.clone());
                                    }
                                    self.push(v);
                                }
                                None => {
                                    // A global that used to exist says what
                                    // replaced it. The rule the whole language
                                    // runs on: reject with an error that names
                                    // the replacement, never leave the reader
                                    // to guess where a name went.
                                    let msg = crate::builtins::cut_global_message(&name)
                                        .map(str::to_string)
                                        .unwrap_or_else(|| {
                                            format!("name '{name}' is not defined")
                                        });
                                    return Err(self.err(name_error(msg)));
                                }
                            }
                        }
                    }
                }
                Op::Pop => {
                    self.pop();
                }
                Op::Dup => {
                    let v = self.top().stack.last().expect("dup on empty stack").clone();
                    self.push(v);
                }
                Op::DupTwo => {
                    let n = self.top().stack.len();
                    let a = self.top().stack[n - 2].clone();
                    let b = self.top().stack[n - 1].clone();
                    self.push(a);
                    self.push(b);
                }
                Op::RotTwo => {
                    let s = &mut self.top().stack;
                    let n = s.len();
                    s.swap(n - 1, n - 2);
                }
                Op::RotThree => {
                    // [a, b, c] -> [c, a, b]
                    let s = &mut self.top().stack;
                    let n = s.len();
                    s[n - 3..].rotate_right(1);
                }
                Op::UnaryNeg => {
                    let v = self.pop();
                    let r = self.wrap(arith::neg(&v))?;
                    self.push(r);
                }
                Op::UnaryPos => {
                    let v = self.pop();
                    let r = self.wrap(arith::pos(&v))?;
                    self.push(r);
                }
                Op::UnaryNot => {
                    let v = self.pop();
                    match v {
                        Value::Bool(b) => self.push(Value::Bool(!b)),
                        _ => return Err(self.err(not_bool(&v, "the operand of `not`"))),
                    }
                }
                Op::UnaryInvert => {
                    let v = self.pop();
                    let r = self.wrap(arith::invert(&v))?;
                    self.push(r);
                }
                Op::AssertBool => {
                    let ok = matches!(self.top().stack.last(), Some(Value::Bool(_)));
                    if !ok {
                        let v = self.top().stack.last().expect("operand stack underflow").clone();
                        return Err(self.err(not_bool(&v, "the operands of `and`/`or`")));
                    }
                }
                Op::BinAdd
                | Op::BinSub
                | Op::BinMul
                | Op::BinDiv
                | Op::BinFloorDiv
                | Op::BinMod
                | Op::BinPow
                | Op::BinBitAnd
                | Op::BinBitOr
                | Op::BinBitXor
                | Op::BinShl
                | Op::BinShr => {
                    let b = self.pop();
                    let a = self.pop();
                    // `None` for a bitwise operator: Oro's dunder set stops at
                    // the arithmetic ones, so there is no `__and__` to look for
                    // and an instance operand falls to the type error below
                    // rather than to a call.
                    let dunder = arith_dunder(&op);
                    match dunder.and_then(|d| instance_method(&a, d)) {
                        Some((f, defclass)) => {
                            self.invoke_user(f, a, defclass, vec![b], Vec::new(), ReturnAction::Normal)?;
                        }
                        None if matches!(a, Value::Instance(_)) => {
                            return Err(self.err(type_error(format!(
                                "unsupported operand type(s) for {}: '{}' and '{}'",
                                arith_symbol(&op),
                                a.type_label(),
                                b.type_label()
                            ))));
                        }
                        None => {
                            let r = self.wrap(arith::binary(&op, &a, &b))?;
                            self.push(r);
                        }
                    }
                }
                Op::Compare(cmp) => {
                    let b = self.pop();
                    let a = self.pop();
                    // The native answer, which is every comparison in a program
                    // with no user `__eq__`/`__lt__` under either operand.
                    match self.wrap(try_compare_op(cmp, &a, &b))? {
                        Some(r) => self.push(Value::Bool(r)),
                        // Some pair in there needs Oro code. A dunder on the
                        // operands themselves is dispatched straight from here,
                        // so that its value reaches the program unconverted
                        // (CPython's `a == b` is whatever `__eq__` returned, not
                        // its truthiness); anything deeper goes to the machine.
                        None => self.compare_slow(cmp, a, b)?,
                    }
                }
                Op::Jump(t) => self.top().pc = t as usize,
                // A defaulted parameter's prologue. The binder left the slot
                // `Unbound` when the call omitted the argument and its default
                // is not a constant; the expression that follows is evaluated
                // then, in this frame, on this call. A bound slot jumps over
                // it, which is every call that passed the argument.
                Op::DefaultIfBound(pair) => {
                    let frame = self.top();
                    let (param, target) = frame.code.pairs[pair as usize];
                    let bound = match frame.code.params[param as usize].target {
                        VarTarget::Local(s) => {
                            !matches!(frame.locals[s as usize], Value::Unbound)
                        }
                        VarTarget::Cell(s) => {
                            !matches!(*frame.cells[s as usize].borrow(), Value::Unbound)
                        }
                    };
                    if bound {
                        frame.pc = target as usize;
                    }
                }
                Op::PopJumpIfFalse(t) => {
                    let v = self.pop();
                    match v {
                        Value::Bool(b) => {
                            if !b {
                                self.top().pc = t as usize;
                            }
                        }
                        _ => return Err(self.err(not_bool(&v, "a condition"))),
                    }
                }
                Op::PopJumpIfTrue(t) => {
                    let v = self.pop();
                    match v {
                        Value::Bool(b) => {
                            if b {
                                self.top().pc = t as usize;
                            }
                        }
                        _ => return Err(self.err(not_bool(&v, "a condition"))),
                    }
                }
                Op::JumpIfFalseOrPop(t) => {
                    match self.top().stack.last() {
                        Some(Value::Bool(true)) => {
                            self.pop();
                        }
                        Some(Value::Bool(false)) => {
                            self.top().pc = t as usize;
                        }
                        _ => {
                            let v = self.top().stack.last().unwrap().clone();
                            return Err(self.err(not_bool(&v, "the operands of `and`/`or`")));
                        }
                    }
                }
                Op::JumpIfTrueOrPop(t) => {
                    match self.top().stack.last() {
                        Some(Value::Bool(true)) => {
                            self.top().pc = t as usize;
                        }
                        Some(Value::Bool(false)) => {
                            self.pop();
                        }
                        _ => {
                            let v = self.top().stack.last().unwrap().clone();
                            return Err(self.err(not_bool(&v, "the operands of `and`/`or`")));
                        }
                    }
                }
                Op::BuildList(n) => {
                    let items = self.popn(n as usize);
                    self.push(Value::List(OroList::new(items)));
                }
                Op::BuildTuple(n) => {
                    let items = self.popn(n as usize);
                    self.push(Value::Tuple(OroTuple::new(items)));
                }
                Op::BuildMap(n) => {
                    let items = self.popn(2 * n as usize);
                    let mut dict = OroDict::new();
                    let mut it = items.into_iter();
                    while let (Some(k), Some(v)) = (it.next(), it.next()) {
                        self.wrap(dict.insert(k, v))?;
                    }
                    self.push(Value::Dict(Rc::new(RefCell::new(dict))));
                }
                Op::ListAppend => {
                    let v = self.pop();
                    let list = self.expect_list_tos("ListAppend")?;
                    list.borrow_mut().push(v);
                }
                Op::MapSetItem => {
                    let v = self.pop();
                    let k = self.pop();
                    let dict = self.expect_dict_tos("MapSetItem")?;
                    self.wrap(dict.borrow_mut().insert(k, v))?;
                }
                Op::LoadSubscript => {
                    let index = self.pop();
                    let obj = self.pop();
                    let r = self.wrap(subscript_get(&obj, &index))?;
                    self.push(r);
                }
                Op::StoreSubscript => {
                    let index = self.pop();
                    let obj = self.pop();
                    let value = self.pop();
                    self.wrap(subscript_set(&obj, &index, value))?;
                }
                Op::LoadSlice => {
                    let step = self.pop();
                    let upper = self.pop();
                    let lower = self.pop();
                    let obj = self.pop();
                    let r = self.wrap(slice_get(&obj, &lower, &upper, &step))?;
                    self.push(r);
                }
                Op::LoadAttr(n) => {
                    let name = self.task.frames.last().unwrap().code.names[n as usize].clone();
                    let obj = self.pop();
                    let r = self.wrap(get_attr(&obj, &name))?;
                    self.push(r);
                }
                Op::StoreAttr(n) => {
                    let name = self.task.frames.last().unwrap().code.names[n as usize].clone();
                    let obj = self.pop();
                    let value = self.pop();
                    match &obj {
                        Value::Instance(inst) => {
                            inst.fields.borrow_mut().insert(name.clone(), value);
                        }
                        other => {
                            let msg = format!(
                                "cannot set attribute '{}' on '{}' object",
                                name,
                                other.type_label()
                            );
                            return Err(self.err(runtime_error(msg)));
                        }
                    }
                }
                Op::BuildClass(i) => {
                    let spec = self.task.frames.last().unwrap().code.classes[i as usize].clone();
                    self.build_class(&spec)?;
                }
                Op::ImportModule(n) => {
                    let path = self.task.frames.last().unwrap().code.names[n as usize].clone();
                    return self.import_module(&path);
                }
                Op::LoadSuper => {
                    let sup = match self.top().super_ctx.clone() {
                        Some((defclass, instance)) => Value::Super(Rc::new(SuperProxy {
                            start: defclass.base.clone(),
                            instance,
                        })),
                        None => {
                            return Err(
                                self.err(runtime_error("super() is only valid inside a method"))
                            )
                        }
                    };
                    self.push(sup);
                }
                Op::UnpackSequence(n) => {
                    let seq = self.pop();
                    // Shared with a destructuring collection callback, which is
                    // defined to unpack exactly as this does.
                    let items = self.wrap(unpack_exact(&seq, n as usize))?;
                    for v in items.into_iter().rev() {
                        self.push(v);
                    }
                }
                Op::FormatValue(conv) => {
                    let spec = self.pop();
                    let value = self.pop();
                    let spec_str = match &spec {
                        Value::Str(s) => s.s.clone(),
                        other => {
                            let msg = format!(
                                "format spec must be a string, not '{}'",
                                other.type_name()
                            );
                            return Err(self.err(type_error(msg)));
                        }
                    };
                    // An instance renders via __str__/__repr__ (which run on a
                    // frame); the format spec is then applied to the result.
                    if matches!(value, Value::Instance(_)) {
                        let want_repr = conv == crate::format::CONV_REPR;
                        let names: [&str; 2] =
                            if want_repr { ["__repr__", "__str__"] } else { ["__str__", "__repr__"] };
                        let mut dispatched = false;
                        for nm in names {
                            if let Some((f, defclass)) = instance_method(&value, nm) {
                                self.invoke_user(
                                    f,
                                    value.clone(),
                                    defclass,
                                    Vec::new(),
                                    Vec::new(),
                                    ReturnAction::FormatSpec(spec_str.clone()),
                                )?;
                                dispatched = true;
                                break;
                            }
                        }
                        if dispatched {
                            return Ok(Step::Next);
                        }
                    }
                    // A container is rendered element-by-element (element
                    // __repr__ dunders, cycle-safe), then the spec is applied.
                    if is_container(&value) {
                        self.begin_stringify(value, StrCont::FormatSpec(spec_str))?;
                        return Ok(Step::Next);
                    }
                    let out = self.wrap(crate::format::format_value(&value, conv, &spec_str))?;
                    self.push(Value::str(out));
                }
                Op::BuildString(n) => {
                    let parts = self.popn(n as usize);
                    let mut s = String::new();
                    for p in parts {
                        s.push_str(&p.display());
                    }
                    self.push(Value::str(s));
                }
                Op::MatchDispatch(pair) => {
                    let (table, default) = self.top().code.pairs[pair as usize];
                    let subject = self.pop();
                    let dict = match &self.top().code.consts[table as usize] {
                        Value::Dict(d) => d.clone(),
                        _ => unreachable!("MatchDispatch table is always a dict const"),
                    };
                    // An unhashable subject cannot equal any literal key, so it
                    // takes the default — matching the compare-chain path.
                    let target = match dict.borrow().get(&subject) {
                        Ok(Some(Value::Int(t))) => t as usize,
                        _ => default as usize,
                    };
                    self.top().pc = target;
                }
                Op::GetIter => {
                    let v = self.pop();
                    let it = self.wrap(get_iter(&v))?;
                    self.push(it);
                }
                Op::ForIter(target) => {
                    let target = target as usize;
                    let it = self.top().stack.last().expect("ForIter on empty stack").clone();
                    // A channel is its own iterator, and `for msg in ch` is a
                    // `recv` that ends the loop instead of raising when the
                    // channel closes and drains (§3). It can block, so this is
                    // a parking site.
                    if let Value::Channel(ch) = &it {
                        return self.chan_iter_next(ch.clone(), target);
                    }
                    // A generator is advanced by resuming its frame; the value
                    // (or exhaustion) arrives via Yield/Return, not inline.
                    if let Value::Generator(gen) = &it {
                        // Three states, not two: finished (`None`), suspended
                        // and ours to resume (`Some(Some(_))`), and *already
                        // being advanced* somewhere else (`Some(None)`) — its
                        // frame is on some task's frame stack right now. The
                        // last one used to be indistinguishable from finished,
                        // so the loop ended silently; two tasks driving one
                        // generator would have corrupted it outright.
                        let taken = {
                            let mut g = gen.borrow_mut();
                            if g.done {
                                None
                            } else {
                                Some(take_gen_frame(&mut g))
                            }
                        };
                        match taken {
                            Some(Some(frame)) => {
                                self.task.gen_stack.push((gen.clone(), GenDriver::ForLoop(target)));
                                self.task.frames.push(frame);
                            }
                            Some(None) => return Ok(Step::Raise(self.generator_busy())),
                            None => {
                                self.pop();
                                self.top().pc = target;
                            }
                        }
                    } else {
                        let next = self.wrap(iter_next_pair(&it))?;
                        match next {
                            Some((i, v)) => self.push(Value::Tuple(OroTuple::new(vec![i, v]))),
                            None => {
                                self.pop(); // discard the exhausted iterator
                                self.top().pc = target;
                            }
                        }
                    }
                }
                Op::MakeFunction(idx) => self.make_function(idx as usize)?,
                Op::Call(n) => return self.do_call(n as usize),
                Op::CallEx => return self.do_call_ex(),
                Op::LoadMethod(n) => {
                    let name = self.task.frames.last().unwrap().code.names[n as usize].clone();
                    let obj = self.pop();
                    match self.wrap(resolve_method(&obj, &name))? {
                        MethodRef::User { recv, func, defclass } => {
                            self.push(Value::Class(defclass));
                            self.push(Value::Func(func));
                            self.push(recv);
                        }
                        MethodRef::Native(recv) => {
                            self.push(Value::Unbound);
                            self.push(Value::None);
                            self.push(recv);
                        }
                        MethodRef::Plain(v) => {
                            self.push(Value::None);
                            self.push(Value::None);
                            self.push(v);
                        }
                    }
                }
                Op::CallMethod(pair) => return self.do_call_method(pair as usize),
                Op::Return => {
                    // A generator body reaching return (including the implicit
                    // one at the end) is exhausted: StopIteration for its driver.
                    if self.top().code.is_generator {
                        return self.generator_stop();
                    }
                    let value = self.pop();
                    return self.do_return(value);
                }
                Op::Yield => {
                    let value = self.pop();
                    // Suspend this generator frame back into its GenBox and hand
                    // the value to whatever is driving it.
                    let frame = self.task.frames.pop().expect("yield with no frame");
                    let (gen, driver) = self.task.gen_stack.pop().expect("yield outside a generator");
                    put_gen_frame(&mut gen.borrow_mut(), frame);
                    match driver {
                        GenDriver::ForLoop(_) => {
                            // Every `for` yields (index, value); a generator's
                            // index is a 0-based counter held on the GenBox.
                            let idx = {
                                let mut g = gen.borrow_mut();
                                let i = g.for_index;
                                g.for_index += 1;
                                i
                            };
                            self.push(Value::Tuple(OroTuple::new(vec![Value::Int(idx), value])));
                        }
                        GenDriver::Materialize => {
                            self.task.mat_jobs.last_mut().expect("materialise job").items.push(value);
                            return self.drive_materialize();
                        }
                    }
                }
                Op::SetupExcept(target) => {
                    let target = target as usize;
                    let jobs = self.job_depths();
                    let stack_len = self.top().stack.len();
                    self.top()
                        .blocks
                        .push(Block { kind: BlockKind::Except, target, stack_len, jobs });
                }
                Op::SetupFinally(target) => {
                    let target = target as usize;
                    let jobs = self.job_depths();
                    let stack_len = self.top().stack.len();
                    self.top()
                        .blocks
                        .push(Block { kind: BlockKind::Finally, target, stack_len, jobs });
                }
                Op::PopBlock => {
                    self.top().blocks.pop();
                }
                Op::SetupLoop(pair) => {
                    let (brk, cont) = self.top().code.pairs[pair as usize];
                    let jobs = self.job_depths();
                    let stack_len = self.top().stack.len();
                    self.top().blocks.push(Block {
                        kind: BlockKind::Loop { cont: cont as usize },
                        target: brk as usize,
                        stack_len,
                        jobs,
                    });
                }
                Op::Break => return Ok(self.do_break()),
                Op::Continue => return Ok(self.do_continue()),
                Op::Raise => {
                    let v = self.pop();
                    let exc = self.normalize_raise(v)?;
                    return Ok(Step::Raise(exc));
                }
                Op::Reraise => {
                    // Bare `raise` / no matching except: re-raise the exception
                    // currently being handled.
                    match self.task.handling.pop() {
                        Some(exc) => return Ok(Step::Raise(exc)),
                        None => {
                            return Err(self.err(runtime_error("No active exception to re-raise")))
                        }
                    }
                }
                Op::LoadHandling => {
                    let exc = self
                        .task
                        .handling
                        .last()
                        .cloned()
                        .expect("LoadHandling with no active exception");
                    self.push(exc);
                }
                Op::EndHandler => {
                    self.task.handling.pop();
                }
                Op::ExcMatch => {
                    let class = self.pop();
                    let exc = self.pop();
                    let matched = self.exc_matches(&exc, &class)?;
                    self.push(Value::Bool(matched));
                }
                Op::BeginFinally => {
                    // The normal fall-through into a finally body: nothing was
                    // suspended.
                    self.task.finally_why.push(Why::Normal);
                }
                Op::EndFinally => {
                    // Resume whatever was suspended to run this finally.
                    match self.task.finally_why.pop().expect("finally without a reason") {
                        Why::Normal => {}
                        Why::Raise(exc) => return Ok(Step::Raise(exc)),
                        Why::Return(v) => return self.do_return(v),
                        Why::Break => return Ok(self.do_break()),
                        Why::Continue => return Ok(self.do_continue()),
                    }
                }
            }
        Ok(Step::Next)
    }

    fn unbound_local_err(&self, slot: u16) -> VErr {
        let code = &self.task.frames.last().unwrap().code;
        // A name that also exists at module scope but was made local by an
        // assignment (no `global`) is the classic footgun — teach the fix.
        for (s, name) in &code.shadow_hints {
            if *s == slot {
                return name_error(format!(
                    "local variable '{name}' referenced before assignment: '{name}' is assigned \
                     inside this function, which makes it local and shadows the module-level \
                     '{name}'. To read and update the module value, declare `global {name}` at \
                     the top of the function; otherwise keep the state on an object, or rename \
                     the local."
                ));
            }
        }
        // Recover the variable's name from its parameter descriptor when we can,
        // for a friendlier message.
        for p in &code.params {
            if let VarTarget::Local(s) = p.target {
                if s == slot {
                    return name_error(format!(
                        "local variable '{}' referenced before assignment",
                        p.name
                    ));
                }
            }
        }
        name_error("local variable referenced before assignment")
    }

    // --- Closures ------------------------------------------------------------

    fn make_function(&mut self, idx: usize) -> Result<(), VmError> {
        let proto = self.task.frames.last().unwrap().code.protos[idx].clone();
        let frame = self.task.frames.last().unwrap();
        let freevars: Vec<Rc<RefCell<Value>>> = proto
            .captures
            .iter()
            .map(|c| match c {
                CaptureSource::Cell(i) => frame.cells[*i as usize].clone(),
                CaptureSource::Free(i) => frame.free[*i as usize].clone(),
            })
            .collect();
        let func = Function { code: proto.code.clone(), freevars };
        self.push(Value::Func(Rc::new(func)));
        Ok(())
    }

    // --- Calls ---------------------------------------------------------------

    fn do_call(&mut self, n: usize) -> Result<Step, VmError> {
        // Fast path: a plain Oro function whose parameters are all positional
        // and exactly covered by the arguments already sitting on the operand
        // stack. Binding straight off that stack is what keeps a call from
        // allocating a temporary argument vector — after frame pooling and the
        // static binding path, that vector was the last allocation left on the
        // call path, and a call-heavy program does nothing but pay for it.
        if let Some(func) = self.fast_call_target(n) {
            return self.call_fast(func, n);
        }
        let args = self.popn(n);
        let callee = self.pop();
        self.invoke(callee, args, Vec::new())
    }

    /// The callee for [`Vm::call_fast`], if this call site qualifies: an
    /// ordinary (non-generator) Oro function with only positional parameters,
    /// called with enough arguments to fill the ones that have no default.
    /// Everything else — builtins, methods, classes, `*args`, keywords, a
    /// wrong arity that owes a diagnostic — falls through to the general path.
    fn fast_call_target(&self, n: usize) -> Option<Rc<Function>> {
        let stack = &self.task.frames.last()?.stack;
        let callee = stack.get(stack.len().checked_sub(n + 1)?)?;
        let f = match callee {
            Value::Func(f) => f,
            _ => return None,
        };
        let code = &f.code;
        // One positional argument per parameter with no default, and no others:
        // under the argument rule that is the whole shape of a positional call,
        // so the test is one equality. Too few, or one that would reach a
        // keyword-only parameter, owes a diagnostic and goes the general way.
        if code.is_generator || n != code.params.len() - code.defaults.len() {
            return None;
        }
        Some(f.clone())
    }

    /// Move `n` arguments from the caller's operand stack straight into a fresh
    /// frame's slots. No argument vector, no re-copy — the values are moved once.
    fn call_fast(&mut self, func: Rc<Function>, n: usize) -> Result<Step, VmError> {
        if self.task.frames.len() >= MAX_FRAMES {
            return Err(self.err(recursion_error("maximum recursion depth exceeded")));
        }
        self.call_fast_unchecked(func, n);
        Ok(Step::Next)
    }

    /// [`Vm::call_fast`] with the frame-limit check already made by the caller,
    /// so that it can be a plain `()` and the dispatch loop's fast path need
    /// not carry an error route it can never take.
    fn call_fast_unchecked(&mut self, func: Rc<Function>, n: usize) {
        let mut frame = self.take_frame(func.code.clone(), &func.freevars);
        let params = &func.code.params;
        {
            let caller = self.task.frames.last_mut().expect("no active frame");
            let base = caller.stack.len() - n;
            for (p, v) in params.iter().zip(caller.stack.drain(base..)) {
                store_param(&mut frame, p.target, v);
            }
            caller.stack.pop().expect("the callee itself");
        }
        // Any trailing parameters the call did not supply take their defaults.
        // A non-constant default is `Unbound` here and is filled by the
        // callee's prologue, so this loop is the same one instruction either
        // way — the check costs the fast path nothing.
        let defaults = &func.code.defaults;
        let first_defaulted = params.len() - defaults.len();
        for (i, p) in params.iter().enumerate().skip(n) {
            store_param(&mut frame, p.target, defaults[i - first_defaulted].clone());
        }
        self.task.frames.push(frame);
    }

    /// Call what `LoadMethod` prepared: three slots and `argc` arguments above
    /// them. See [`Op::LoadMethod`] for what the slots hold.
    fn do_call_method(&mut self, pair: usize) -> Result<Step, VmError> {
        let (name_idx, argc) = self.task.frames.last().expect("no active frame").code.pairs[pair];
        // The top two bits are the chain-fusion hints, not part of the count.
        // Only a *native* method can be a chain step, so they are read in that
        // arm and nowhere else; an Oro method call never looks at them.
        let n = (argc & !crate::compiler::CHAIN_BITS) as usize;
        let tag_is = {
            let stack = &self.task.frames.last().expect("no active frame").stack;
            let tag = &stack[stack.len() - n - 3];
            match tag {
                Value::Class(_) => 0u8,
                Value::Unbound => 1,
                _ => 2,
            }
        };
        if tag_is == 0 {
            // An Oro method. The receiver sits directly beneath the arguments
            // and *is* the first one, so a simple signature binds the whole
            // call straight off the operand stack — the same trade `call_fast`
            // makes for a plain function, which a bound method could never
            // take while it arrived wrapped in an `Rc<BoundMethod>`.
            let bindable = {
                let stack = &self.task.frames.last().expect("no active frame").stack;
                match &stack[stack.len() - n - 2] {
                    Value::Func(f) => {
                        let code = &f.code;
                        // `n + 1` counts the receiver, which is the first
                        // parameter and never has a default.
                        !code.is_generator
                            && n + 1 == code.params.len() - code.defaults.len()
                    }
                    _ => false,
                }
            };
            if bindable && self.task.frames.len() < MAX_FRAMES {
                return self.call_method_fast(n);
            }
        }
        // Everything else assembles the argument vector the general paths take.
        let args = self.popn(n);
        let recv_or_callee = self.pop();
        let aux = self.pop();
        let tag = self.pop();
        match tag {
            Value::Class(defclass) => {
                let func = match aux {
                    Value::Func(f) => f,
                    other => unreachable!("LoadMethod pushed a non-function: {}", other.type_name()),
                };
                // A method with a `yield` in it produces a generator, the same
                // as a plain generator `def` — this is an ordinary `obj.m()`,
                // not a dispatched call, so `invoke_user`'s refusal (which is
                // for dunders and chain callbacks, each of which has a
                // continuation waiting on a value) must not be reached.
                if func.code.is_generator {
                    return self.make_method_generator(
                        &func,
                        recv_or_callee,
                        defclass,
                        args,
                        Vec::new(),
                    );
                }
                self.invoke_user(
                    func,
                    recv_or_callee,
                    defclass,
                    args,
                    Vec::new(),
                    ReturnAction::Normal,
                )
                .map(|()| Step::Next)
            }
            Value::Unbound => {
                let name =
                    self.task.frames.last().expect("no active frame").code.names[name_idx as usize]
                        .clone();
                self.invoke_native_method(
                    recv_or_callee,
                    &name,
                    args,
                    Vec::new(),
                    argc & crate::compiler::CHAIN_HINT != 0,
                    argc & crate::compiler::CHAIN_FLUSH != 0,
                )
            }
            _ => self.invoke(recv_or_callee, args, Vec::new()),
        }
    }

    /// The Oro-method twin of [`Vm::call_fast`]: the receiver is the first
    /// parameter, so receiver and arguments move from the caller's operand
    /// stack into the callee's slots in one pass, with no argument vector.
    fn call_method_fast(&mut self, n: usize) -> Result<Step, VmError> {
        let (func, defclass) = {
            let stack = &self.task.frames.last().expect("no active frame").stack;
            let func = match &stack[stack.len() - n - 2] {
                Value::Func(f) => f.clone(),
                _ => unreachable!("checked by the caller"),
            };
            let defclass = match &stack[stack.len() - n - 3] {
                Value::Class(c) => c.clone(),
                _ => unreachable!("checked by the caller"),
            };
            (func, defclass)
        };
        let mut frame = self.take_frame(func.code.clone(), &func.freevars);
        let params = &func.code.params;
        let receiver = {
            let caller = self.task.frames.last_mut().expect("no active frame");
            let base = caller.stack.len() - n;
            for (p, v) in params[1..].iter().zip(caller.stack.drain(base..)) {
                store_param(&mut frame, p.target, v);
            }
            let receiver = caller.stack.pop().expect("the receiver");
            caller.stack.pop().expect("the function slot");
            caller.stack.pop().expect("the class slot");
            receiver
        };
        store_param(&mut frame, params[0].target, receiver.clone());
        // Any trailing parameters the call did not supply take their defaults.
        let defaults = &func.code.defaults;
        let first_defaulted = params.len() - defaults.len();
        for (i, p) in params.iter().enumerate().skip(n + 1) {
            store_param(&mut frame, p.target, defaults[i - first_defaulted].clone());
        }
        frame.super_ctx = Some((defclass, receiver));
        self.task.frames.push(frame);
        Ok(Step::Next)
    }

    fn do_call_ex(&mut self) -> Result<Step, VmError> {
        let kwdict = self.pop();
        let poslist = self.pop();
        let callee = self.pop();
        let args = match poslist {
            Value::List(l) => l.borrow().clone(),
            _ => return Err(self.err(runtime_error("internal: CallEx positional list malformed"))),
        };
        let kwargs = match kwdict {
            Value::Dict(d) => {
                let d = d.borrow();
                let mut out = Vec::with_capacity(d.len());
                for (k, v) in d.items() {
                    match k {
                        Value::Str(s) => out.push((s.s.clone(), v.clone())),
                        _ => return Err(self.err(type_error("keywords must be strings"))),
                    }
                }
                out
            }
            _ => return Err(self.err(runtime_error("internal: CallEx keyword dict malformed"))),
        };
        self.invoke(callee, args, kwargs)
    }

    /// `apply(f, args=[], kwargs={})` — call `f` with a list bound by position
    /// and a dict bound by name.
    ///
    /// This is what replaced `f(*xs)` and `f(**d)`: the same two shapes, but as
    /// an ordinary call to an ordinary builtin, so there is no second argument
    /// syntax in the grammar and the argument rule governs the forwarded
    /// arguments exactly as it governs written ones. The binding is
    /// [`Vm::bind_call`]'s, the binder a written call uses, so a refusal here
    /// is the written call's refusal in the written call's words.
    ///
    /// It cannot be a plain native builtin: a native answers with a `Value`,
    /// and this one has to answer with a *call*, which is a frame the VM
    /// pushes — the same reason `spawn` is dispatched here.
    fn do_apply(
        &mut self,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<Step, VmError> {
        let (mut args, mut kwargs) = (args, kwargs);
        // `apply(apply, args=[f])` is legal, and means `f()`. Unwrapping it in
        // a loop rather than by re-entering `do_apply` keeps a nest of them off
        // the Rust stack, whose depth is not ours to bound.
        loop {
            let (callee, a, k) = self.apply_operands(args, kwargs)?;
            match &callee {
                Value::Builtin(b) if b.name == "apply" => {
                    args = a;
                    kwargs = k;
                }
                _ => return self.invoke(callee, a, k),
            }
        }
    }

    /// `apply`'s own arguments, checked and taken apart into the callee, the
    /// positional list and the keyword pairs.
    ///
    /// `apply` follows the rule it exists to serve: `f` has no default and is
    /// positional, `args=` and `kwargs=` have defaults and are named.
    fn apply_operands(
        &mut self,
        mut args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<ApplyOperands, VmError> {
        if args.len() != 1 {
            let msg = if args.is_empty() {
                "apply() missing required argument: 'f'".to_string()
            } else {
                format!(
                    "apply() takes 1 positional argument but {} were given — the arguments \
                     to forward are passed by name: `apply(f, args=[…], kwargs={{…}})`",
                    args.len()
                )
            };
            return Err(self.err(type_error(msg)));
        }
        let callee = args.pop().expect("exactly one");

        let (mut forward_args, mut forward_kwargs) = (None, None);
        for (name, value) in kwargs {
            let slot = match name.as_str() {
                "args" => &mut forward_args,
                "kwargs" => &mut forward_kwargs,
                other => {
                    return Err(self.err(type_error(format!(
                        "apply() got an unexpected keyword argument '{other}'"
                    ))))
                }
            };
            if slot.is_some() {
                return Err(self.err(type_error(format!(
                    "apply() got multiple values for keyword argument '{name}'"
                ))));
            }
            *slot = Some(value);
        }

        let forwarded = match forward_args {
            None => Vec::new(),
            Some(Value::List(l)) => l.borrow().clone(),
            Some(Value::None) => {
                return Err(self.err(crate::builtins::null_is_not_omitted("apply", "args", "a list")))
            }
            Some(other) => {
                return Err(self.err(type_error(format!(
                    "apply(): args= must be a list, not '{}'",
                    other.type_label()
                ))))
            }
        };
        let forwarded_kw = match forward_kwargs {
            None => Vec::new(),
            Some(Value::Dict(d)) => {
                let d = d.borrow();
                let mut out = Vec::with_capacity(d.len());
                for (k, v) in d.items() {
                    match k {
                        Value::Str(s) => out.push((s.s.to_string(), v.clone())),
                        other => {
                            return Err(self.err(type_error(format!(
                                "apply(): kwargs= keys must be str, not '{}' — they are \
                                 parameter names",
                                other.type_label()
                            ))))
                        }
                    }
                }
                out
            }
            Some(Value::None) => {
                return Err(self
                    .err(crate::builtins::null_is_not_omitted("apply", "kwargs", "a dict")))
            }
            Some(other) => {
                return Err(self.err(type_error(format!(
                    "apply(): kwargs= must be a dict, not '{}'",
                    other.type_label()
                ))))
            }
        };
        Ok((callee, forwarded, forwarded_kw))
    }

    /// Call a native (builtin) method on `receiver`.
    ///
    /// Lifted out of [`Vm::invoke`]'s `MethodKind::Native` arm so that
    /// `CallMethod` can reach it with a receiver and a name and never build
    /// the `Rc<BoundMethod>` that used to carry the two here. The body is
    /// unchanged, including the order of its tests: several of these methods
    /// must run *in the VM* rather than as native code, because they can park
    /// (`join`, `send`, `recv`, `close`, the io protocol), or run an Oro
    /// callback (`map`, `filter`, `sort`, `min`, `max`), or run a
    /// user `__str__` (`to_str`).
    ///
    /// `hint` and `flush` carry [`crate::compiler::CHAIN_HINT`] and
    /// [`crate::compiler::CHAIN_FLUSH`] through from the instruction: the first
    /// says this step's result feeds the next step of the same chain and
    /// nothing else, so it may defer itself rather than build a collection
    /// nobody will look at; the second says this is the step that runs what an
    /// earlier one deferred. They are bits on the instruction rather than
    /// questions about VM state so that a program with no chains in it pays
    /// nothing for the machinery.
    fn invoke_native_method(
        &mut self,
        receiver: Value,
        name: &Rc<str>,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
        hint: bool,
        flush: bool,
    ) -> Result<Step, VmError> {
        // Task and channel methods are dispatched ahead of
        // everything else in this arm for two reasons. They are the
        // ones that can *park*, so they must reach the VM rather
        // than `call_method`, which can only return a `Value`. And
        // a generator handed to `ch.send` has to arrive at the far
        // end as a generator — the materialise path below would
        // drain it into a list, which is precisely the "a generator
        // is a first-class value and can cross tasks" case §3
        // calls out.
        if let Some(step) = self.task_or_channel_method(&receiver, name, &args, &kwargs)? {
            return Ok(step);
        }
        // The io protocol's two methods, plus `read_until`,
        // `accept` and `close`. Four of the five can park on the
        // reactor and the fifth has to *wake* whoever is parked, so
        // none of them can be a `Value`-returning native method.
        //
        // The `matches!` is not redundant with the check inside:
        // this arm runs on *every* native method call in the
        // program, and a discriminant test here is what keeps that
        // from being a call into a cold function. Measured — it is
        // worth about a point on the method-heavy benchmarks.
        if matches!(receiver, Value::Stream(_)) {
            if let Some(step) =
                self.stream_io_method(&receiver, name, &args, &kwargs)?
            {
                return Ok(step);
            }
        }
        // `to_str` may need to run a user `__str__`, or render a
        // container's elements through their `__repr__`; both go
        // through frames, so they cannot run as native methods.
        if &**name == "to_str" && args.is_empty() && kwargs.is_empty() {
            if matches!(receiver, Value::Instance(_)) {
                return self
                    .stringify_instance(receiver.clone(), false)
                    .map(|()| Step::Next);
            }
            if is_container(&receiver) {
                return self
                    .begin_stringify(receiver.clone(), StrCont::Push)
                    .map(|()| Step::Next);
            }
        }
        // map/filter run Oro callbacks, so they are driven from the
        // VM rather than executed as native methods.
        if let Some(op) = SeqOp::from_name(name)
            .filter(|_| crate::builtins::is_collection(&receiver))
        {
            // A generator receiver has to be drained first; the
            // retry arrives back here with a list in its place.
            if matches!(receiver, Value::Generator(_)) {
                let callee = Self::rebound_method(&receiver, name);
                let with_recv = std::iter::once(receiver.clone())
                    .chain(args.iter().cloned())
                    .collect::<Vec<_>>();
                if let Some(step) =
                    self.materialize_receiver(&callee, with_recv, kwargs.clone())?
                {
                    return Ok(step);
                }
            }
            return self.do_seq_op(op, &receiver, args, kwargs, hint, flush).map(|()| Step::Next);
        }
        // `first()` / `take(n)` closing a fused chain. See `Vm::chain_tail`:
        // they take no callback, so the pipeline runs to `Collect` under a
        // limit, and the limit is what stops the upstream pass. Anything the
        // fused form cannot answer (a wrong arity, a non-integer count) leaves
        // the pipeline pending and falls through to the native method, which
        // raises the diagnostic it always did.
        if flush {
            if let Some(step) = self.chain_tail(&receiver, name, &args)? {
                return Ok(step);
            }
        }
        // The chain's two orderings. Like their builtin twins
        // they compare with `<`, so a receiver of instances needs
        // frames; anything else falls straight through to native.
        if matches!(&**name, "min" | "max")
            && !matches!(receiver, Value::Generator(_))
            && crate::builtins::is_collection(&receiver)
            && args.is_empty()
            && kwargs.is_empty()
        {
            let (_, items) = self.seq_receiver(name, &receiver)?;
            let keys = items.clone();
            let want_min = &**name == "min";
            let kind = OrdKind::Extreme { want_min, who: if want_min { "min" } else { "max" } };
            return self.begin_order(kind, items, keys, false).map(|()| Step::Next);
        }
        // A generator *receiver* is drained the same way a generator argument
        // is, for the same reason: the native method below iterates it, and
        // native code can never resume a generator. The retry arrives back here
        // with a list in the receiver's place. The `matches!` keeps the name
        // test — and the vector it builds — off the path every other native
        // method call takes.
        if matches!(receiver, Value::Generator(_))
            && crate::builtins::drains_generator_receiver(name)
        {
            let callee = Self::rebound_method(&receiver, name);
            let with_recv =
                std::iter::once(receiver.clone()).chain(args.iter().cloned()).collect::<Vec<_>>();
            if let Some(step) = self.materialize_receiver(&callee, with_recv, kwargs.clone())? {
                return Ok(step);
            }
        }
        // A generator *argument* is drained the same way, and the same
        // `matches!` discipline applies — except that here the thing being kept
        // off the ordinary path is not a name test but a heap allocation.
        // `rebound_method` builds an `Rc<BoundMethod>`, and spelling it as an
        // argument meant building one on **every native method call in the
        // program** so that `materialize_generator_args` could look at its
        // arguments, find no generator among them, and answer `None`. That is
        // the very allocation the `LoadMethod`/`CallMethod` pair exists to
        // remove: `xs.append(i)` was paying for a bound method it never used.
        if args.iter().any(|a| matches!(a, Value::Generator(_))) {
            if let Some(step) = self.materialize_generator_args(
                &Self::rebound_method(&receiver, name),
                &args,
                &kwargs,
            )? {
                return Ok(step);
            }
        }
        let r =
            self.wrap(crate::builtins::call_method(&receiver, name, args, kwargs))?;
        self.push(r);
        Ok(Step::Next)
    }

    /// The `Rc<BoundMethod>` a native method call needs only when it has to be
    /// *retried* — a generator receiver or a generator argument has to be
    /// drained through frames first, and the retry needs a callable to come
    /// back to. Rare by construction, so it can afford the allocation the
    /// ordinary path no longer makes.
    fn rebound_method(receiver: &Value, name: &Rc<str>) -> Value {
        Value::Method(Rc::new(BoundMethod {
            receiver: receiver.clone(),
            kind: MethodKind::Native(name.clone()),
        }))
    }

    /// Dispatch a call. Builtins and bound methods execute natively (they never
    /// re-enter Oro), so only Oro functions push a new frame — keeping the one
    /// flat loop intact.
    fn invoke(
        &mut self,
        callee: Value,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<Step, VmError> {
        match callee {
            Value::Builtin(b) => {
                // A few builtins may need to run an Oro dunder (which must go
                // through a frame, not a Rust re-entry), so they are handled in
                // the VM rather than as pure native functions.
                match b.name {
                    // `spawn` and `chan` are builtins because they are
                    // control-flow constructs, the same reason `print` and
                    // `len` are (§3) — but neither can be a plain native
                    // function: `spawn` has to build a stack segment the VM
                    // owns, and both must be able to answer with a `Step`.
                    // `apply(f, …)` calls `f`, and a call is a frame — which is
                    // the VM's to push, not a native's to return. See
                    // `Vm::do_apply`.
                    "apply" => return self.do_apply(args, kwargs),
                    "spawn" => return self.do_spawn(args, kwargs),
                    "chan" => return self.do_chan(args, kwargs),
                    "yield_now" => return self.do_yield_now(args, kwargs),
                    // `time.sleep` parks the calling task on the reactor's
                    // deadline list. It used to be `std::thread::sleep`, which
                    // stopped every task in the VM, so it too has to answer
                    // with a `Step` rather than a `Value`.
                    "time.sleep" => return self.do_sleep(args, kwargs),
                    // `net.dial` parks twice: on the system resolver, and then
                    // on the handshake. It was a plain native builtin, which
                    // is how it came to stop the whole VM for both — a
                    // `Builtin` answers with a `Value` and has no way to say
                    // "wait". See `sched::Vm::do_dial`.
                    "net.dial" => return self.do_dial(args, kwargs),
                    "print" => return self.do_print(args, kwargs).map(|()| Step::Next),
                    // proc.run is finished here so it can take keyword args and
                    // build a Completed instance.
                    "proc.run" => return self.do_proc_run(args, kwargs).map(|()| Step::Next),
                    // `net.listen` takes `reuseport=`, and the check further
                    // down this arm refuses keyword arguments to any plain
                    // `Builtin` — so, like `proc.run`, it is finished here. It
                    // does not park and needs nothing else from the VM, so this
                    // is the whole of it.
                    "net.listen" => {
                        let r = self.wrap(modules::net_listen_kw(args, &kwargs))?;
                        self.push(r);
                        return Ok(Step::Next);
                    }
                    // `repr` of an instance runs its `__repr__`, which lives in
                    // a frame — so it is driven from here, not natively.
                    //
                    // There is no `str` arm beside it any more. `str` is a type
                    // keyword now, not a builtin, and a type is not callable;
                    // the spelling for "render this" is `f"{x}"`, which is what
                    // the tree already used everywhere but one line. Before
                    // this, `str("x")` raised "not callable" while
                    // `str(some_instance)` quietly worked — one name, two
                    // answers, decided by the argument.
                    "repr" if matches!(args.first(), Some(Value::Instance(_))) && args.len() == 1 => {
                        return self
                            .stringify_instance(args.into_iter().next().unwrap(), true)
                            .map(|()| Step::Next);
                    }
                    // repr() of a container renders elements' __repr__ (and is
                    // cycle-safe), which needs VM dispatch, not native repr.
                    "repr" if args.len() == 1 && is_container(&args[0]) => {
                        return self
                            .begin_stringify(args.into_iter().next().unwrap(), StrCont::Push)
                            .map(|()| Step::Next);
                    }
                    "len" if matches!(args.first(), Some(Value::Instance(_))) && args.len() == 1 => {
                        return self.dunder_len(args.into_iter().next().unwrap()).map(|()| Step::Next);
                    }
                    _ => {}
                }
                // A generator argument must be drained through frames first,
                // and the test is spelled out here for the same reason it is
                // spelled out in `invoke_native_method`: passing the callee
                // means building a `Value::Builtin` and bumping an `Rc` on
                // *every* builtin call, so that `materialize_generator_args`
                // can look at the arguments, find no generator among them, and
                // answer `None`.
                if args.iter().any(|a| matches!(a, Value::Generator(_))) {
                    if let Some(step) =
                        self.materialize_generator_args(&Value::Builtin(b.clone()), &args, &kwargs)?
                    {
                        return Ok(step);
                    }
                }
                if !kwargs.is_empty() {
                    // `round(x, ndigits=)` and `open(path, mode=)` are the plain
                    // builtins with a defaulted parameter, so they are the ones
                    // that take a keyword; every other one refuses. Only a call
                    // that passed a keyword gets here, so no other call pays.
                    let r = self.wrap(crate::builtins::call_builtin_kw(b.name, args, &kwargs))?;
                    self.push(r);
                    return Ok(Step::Next);
                }
                let r = self.wrap((b.func)(args))?;
                self.push(r);
                Ok(Step::Next)
            }
            Value::Method(m) => match &m.kind {
                MethodKind::Native(name) => {
                    self.invoke_native_method(m.receiver.clone(), name, args, kwargs, false, false)
                }
                MethodKind::User { func, defclass } => {
                    if func.code.is_generator {
                        return self.make_method_generator(
                            func,
                            m.receiver.clone(),
                            defclass.clone(),
                            args,
                            kwargs,
                        );
                    }
                    self.invoke_user(
                        func.clone(),
                        m.receiver.clone(),
                        defclass.clone(),
                        args,
                        kwargs,
                        ReturnAction::Normal,
                    )
                    .map(|()| Step::Next)
                }
            },
            Value::Func(f) => {
                if self.task.frames.len() >= MAX_FRAMES {
                    return Err(self.err(recursion_error("maximum recursion depth exceeded")));
                }
                let frame = self.bind_call(&f, None, args, kwargs)?;
                if f.code.is_generator {
                    // Calling a generator function does not run it; it produces a
                    // generator holding the suspended (unstarted) frame.
                    let gen =
                        crate::value::GenBox { done: false, for_index: 0, frame: Some(Box::new(Some(frame))) };
                    self.push(Value::Generator(Rc::new(RefCell::new(gen))));
                } else {
                    self.task.frames.push(frame);
                }
                Ok(Step::Next)
            }
            Value::Class(class) => self.instantiate(class, args, kwargs).map(|()| Step::Next),
            // A builtin type. `range(n)` builds one; every other type name
            // refuses with the reason, which is the same refusal it gave when
            // these names were builtins.
            Value::Type(t) => {
                if !kwargs.is_empty() && t != crate::value::TypeTag::Range {
                    return Err(self.err(type_error(format!(
                        "{}() takes no keyword arguments",
                        t.name()
                    ))));
                }
                let r = self.wrap(crate::builtins::call_type(t, args, &kwargs))?;
                self.push(r);
                Ok(Step::Next)
            }
            other => Err(self.err(type_error(format!(
                "'{}' object is not callable",
                other.type_label()
            )))),
        }
    }

    /// The six-name concurrency surface's method half: `join`, `send`, `recv`
    /// and `close`. `Ok(None)` means "not one of mine, carry on".
    fn task_or_channel_method(
        &mut self,
        receiver: &Value,
        name: &str,
        args: &[Value],
        kwargs: &[(String, Value)],
    ) -> Result<Option<Step>, VmError> {
        let (task_recv, chan_recv) = match receiver {
            Value::Task(h) => (Some(h.clone()), None),
            Value::Channel(c) => (None, Some(c.clone())),
            _ => return Ok(None),
        };
        if !kwargs.is_empty() {
            return Ok(Some(
                self.raise(Exc::TypeError, format!("{name}() takes no keyword arguments")),
            ));
        }
        let arity = |vm: &Self, want: usize| -> Result<(), Step> {
            if args.len() == want {
                return Ok(());
            }
            let expected = match want {
                0 => "no arguments".to_string(),
                1 => "exactly 1 argument".to_string(),
                n => format!("exactly {n} arguments"),
            };
            Err(vm.raise(Exc::TypeError, format!(
                "{name}() takes {expected} ({} given)",
                args.len()
            )))
        };
        if let Some(handle) = task_recv {
            if name != "join" {
                return Ok(None);
            }
            if let Err(step) = arity(self, 0) {
                return Ok(Some(step));
            }
            return self.task_join(handle).map(Some);
        }
        let ch = chan_recv.expect("one of the two");
        let want = match name {
            "send" => 1,
            "recv" | "close" => 0,
            _ => return Ok(None),
        };
        if let Err(step) = arity(self, want) {
            return Ok(Some(step));
        }
        match name {
            "send" => self.chan_send(ch, args[0].clone()).map(Some),
            "recv" => self.chan_recv(ch).map(Some),
            _ => self.chan_close(ch).map(Some),
        }
    }

    /// `obj.m(...)` where `m` contains a `yield`: produce a generator rather
    /// than running the body, exactly as calling a plain generator `def` does.
    ///
    /// The frame is built the way an ordinary method call builds it and handed
    /// to a `GenBox` instead of being pushed. The one extra step is
    /// `super_ctx`, which goes on the frame *before* it is parked, so `super()`
    /// still resolves when the generator is resumed — possibly in another task,
    /// long after this call returned.
    ///
    /// Shared by both call paths on purpose. `Op::CallMethod` has its own
    /// user-method arm, and when this logic lived inline in `Vm::invoke`'s the
    /// two silently disagreed: the fast path fell through to `invoke_user`'s
    /// refusal and an ordinary `obj.m()` stopped producing a generator. There
    /// is one copy now, and both arms call it.
    fn make_method_generator(
        &mut self,
        func: &Rc<Function>,
        receiver: Value,
        defclass: Rc<Class>,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<Step, VmError> {
        if self.task.frames.len() >= MAX_FRAMES {
            return Err(self.err(recursion_error("maximum recursion depth exceeded")));
        }
        let mut frame = self.bind_call(func, Some(receiver.clone()), args, kwargs)?;
        frame.super_ctx = Some((defclass, receiver));
        let gen =
            crate::value::GenBox { done: false, for_index: 0, frame: Some(Box::new(Some(frame))) };
        self.push(Value::Generator(Rc::new(RefCell::new(gen))));
        Ok(Step::Next)
    }

    /// Call an Oro method: push `receiver` as `self`, then the rest, into a
    /// fresh frame carrying the `super()` context and the requested return
    /// action.
    fn invoke_user(
        &mut self,
        func: Rc<Function>,
        receiver: Value,
        defclass: Rc<Class>,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
        action: ReturnAction,
    ) -> Result<(), VmError> {
        if self.task.frames.len() >= MAX_FRAMES {
            return Err(self.err(recursion_error("maximum recursion depth exceeded")));
        }
        // An ordinary `obj.m()` with a `yield` in it is handled at the call
        // site, where it produces a generator the way a plain `def` does. What
        // is left here is the dispatched half — a dunder, or a bound method
        // used as a chain callback — and every one of those has a continuation
        // waiting for a *value* from a frame that runs now (`DriveStr` wants
        // the string, `DriveSeq` the element). Handing one
        // a generator instead is not a feature, it is a different bug. It was a
        // `yield outside a generator` panic before; refusing by name is the
        // same answer `sorted(key=…)` already gives.
        if func.code.is_generator {
            let name = func.code.name.clone();
            return Err(self.err(type_error(format!(
                "{name}() has a `yield` in it, and Oro does not carry generators \
                 through dunders and callbacks — move it to a module-level def"
            ))));
        }
        let mut frame = self.bind_call(&func, Some(receiver.clone()), args, kwargs)?;
        frame.ret_action = action;
        frame.super_ctx = Some((defclass, receiver));
        self.task.frames.push(frame);
        Ok(())
    }

    /// Construct an instance of `class`, running `__init__` if defined. The
    /// instance is left on the caller's stack as the constructor's result.
    fn instantiate(
        &mut self,
        class: Rc<Class>,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<(), VmError> {
        let inst = Value::Instance(Rc::new(Instance {
            class: class.clone(),
            fields: RefCell::new(Fields::new()),
        }));
        match Class::find(&class, "__init__") {
            Some((Value::Func(init), defclass)) => {
                // Leave the instance as the eventual result; __init__ returns
                // None (checked) and its frame is dropped.
                self.push(inst.clone());
                self.invoke_user(init, inst, defclass, args, kwargs, ReturnAction::DropForInit)
            }
            Some(_) => Err(self.err(type_error(format!(
                "{}.__init__ is not a function",
                class.name
            )))),
            // An exception class with no custom __init__ stores its args tuple
            // natively (BaseException-style), so `ValueError("x")` just works.
            None if class.is_exception => {
                if !kwargs.is_empty() {
                    return Err(self.err(type_error(format!(
                        "{}() takes no keyword arguments",
                        class.name
                    ))));
                }
                let exc = self.make_exception_instance(class, args);
                self.push(exc);
                Ok(())
            }
            None => {
                if !args.is_empty() || !kwargs.is_empty() {
                    return Err(self.err(type_error(format!(
                        "{}() takes no arguments",
                        class.name
                    ))));
                }
                self.push(inst);
                Ok(())
            }
        }
    }

    /// `str()`/`repr()` of an instance: run `__str__` (or `__repr__` when
    /// `want_repr`), falling back to the other, then to the default text.
    fn stringify_instance(&mut self, value: Value, want_repr: bool) -> Result<(), VmError> {
        let inst = match &value {
            Value::Instance(i) => i.clone(),
            _ => unreachable!("stringify_instance on a non-instance"),
        };
        let order: [&str; 2] = if want_repr {
            ["__repr__", "__str__"]
        } else {
            ["__str__", "__repr__"]
        };
        for name in order {
            if let Some((Value::Func(f), defclass)) = Class::find(&inst.class, name) {
                return self.invoke_user(f, value, defclass, Vec::new(), Vec::new(), ReturnAction::Normal);
            }
        }
        // No dunder: str() uses display() (an exception's message), repr() uses
        // repr() (its Name(args) form).
        let out = if want_repr { value.repr() } else { value.display() };
        self.push(Value::str(out));
        Ok(())
    }

    fn dunder_len(&mut self, value: Value) -> Result<(), VmError> {
        let inst = match &value {
            Value::Instance(i) => i.clone(),
            _ => unreachable!(),
        };
        match Class::find(&inst.class, "__len__") {
            Some((Value::Func(f), defclass)) => {
                self.invoke_user(f, value, defclass, Vec::new(), Vec::new(), ReturnAction::Normal)
            }
            _ => Err(self.err(type_error(format!(
                "object of type '{}' has no len()",
                inst.class.name
            )))),
        }
    }

    /// The callback-taking half of the collection protocol. Every one of these
    /// runs Oro code per element, so they are driven from the VM a frame at a
    /// time rather than executed as native methods.
    ///
    /// Which collection comes back is governed by [`SeqOp::preserves_shape`].
    /// A callback with several parameters destructures its element as `for`
    /// does ([`Spread`]), so `d.filter((k, v) => v > 1)` and
    /// `xs.enumerate().map((i, x) => …)` read naturally instead of forcing the
    /// caller to index a pair.
    ///
    /// `hint` is permission to defer this step into a [`PendingChain`] for the
    /// next step to run in the same pass; `flush` says an earlier step took
    /// that permission and this is the step that runs what it left.
    fn do_seq_op(
        &mut self,
        op: SeqOp,
        receiver: &Value,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
        hint: bool,
        flush: bool,
    ) -> Result<(), VmError> {
        let who = op.name();
        // `sort_by` takes one option, `reverse=`, by name; no other step takes
        // any.
        let reverse = match op {
            SeqOp::SortBy => self.reverse_kwarg(who, kwargs)?,
            _ if !kwargs.is_empty() => {
                return Err(self.err(type_error(format!("{who}() takes no keyword arguments"))))
            }
            _ => false,
        };
        let callable = |v: &Value| {
            matches!(v, Value::Func(_) | Value::Builtin(_) | Value::Method(_))
        };
        // Arity: reduce takes (initial, f); everything else takes exactly one
        // function. `any`/`all`/`count` once tested truthiness with none, and
        // `[0, 1, 2, ""].count()` read as a length and was 2; truthiness is a
        // predicate like any other now, so their missing one says how to
        // spell it.
        let (func, seed) = match op {
            SeqOp::Reduce => match args.as_slice() {
                [init, f] if callable(f) => (Some(f.clone()), Some(init.clone())),
                [_, other] => {
                    return Err(self.err(type_error(format!(
                        "reduce() needs a function as its second argument, not '{}'",
                        other.type_name()
                    ))))
                }
                _ => {
                    return Err(self.err(
                        type_error("reduce() takes an initial value and a function, e.g. \
                         xs.reduce(0, (acc, x) => acc + x)",)
                    ))
                }
            },
            SeqOp::Any | SeqOp::All | SeqOp::Count if args.is_empty() => {
                let len = if op == SeqOp::Count { "; `xs.len()` is the length" } else { "" };
                return Err(self.err(type_error(format!(
                    "{who}() needs a predicate — truthiness is `xs.{who}(x => x)`{len}"
                ))));
            }
            _ => match args.as_slice() {
                [f] if callable(f) => (Some(f.clone()), None),
                [other] => {
                    return Err(self.err(type_error(format!(
                        "{who}() needs a function, not '{}'",
                        other.type_name()
                    ))))
                }
                _ => return Err(self.err(type_error(format!("{who}() takes exactly 1 argument")))),
            },
        };

        // The steps before this one that were deferred into a pipeline, if the
        // step that deferred them was deferring against *this* receiver. The
        // identity check is what keeps a pipeline from being flushed into some
        // other collection that happens to reach a chain method first.
        let pending = if flush { self.take_chain(receiver) } else { None };
        let shape = match &pending {
            Some(p) => p.shape,
            None => self.seq_shape(who, receiver)?,
        };
        // How the callback takes each element, settled here once for the whole
        // step: whichever of a stage or the terminal ends up running it, it
        // reads this on every element.
        let spread = match &func {
            Some(f) => seq_spread(f, shape, usize::from(op == SeqOp::Reduce)),
            None => Spread::Whole,
        };

        // Defer in turn, when this step streams and the next one can flush.
        //
        // `map` over a **dict** declines. It rebuilds a dict out of whatever
        // the callback answers, and the check that those answers are
        // `(key, value)` pairs happens when the dict is built — so a callback
        // that answers something else must still raise after the whole receiver
        // has been walked, from the dict rebuild, exactly where it does today.
        // Deferring would move that check onto the first element. Every other
        // dict step passes the original pair through untouched and fuses.
        if hint {
            let kind = match (op, shape) {
                (SeqOp::Map, SeqShape::Dict) => None,
                (SeqOp::Map, _) => func.clone().map(StageKind::Map),
                (SeqOp::Filter, _) => func.clone().map(StageKind::Filter),
                _ => None,
            };
            if let Some(kind) = kind {
                let mut p = pending.unwrap_or_else(|| PendingChain {
                    source: receiver.clone(),
                    stages: Vec::new(),
                    shape,
                    frame_depth: self.task.frames.len(),
                });
                // Both deferrable steps are type-preserving, so the shape an
                // element carries out of the pipeline is the one it carried in.
                p.stages.push(Stage {
                    kind,
                    spread,
                    line: self.task.line,
                    col: self.task.col,
                });
                self.chains.push(p);
                // The receiver stands in for the collection this step did not
                // build. Nothing but the next step's `LoadMethod` will see it,
                // and that resolves the same method on the same type.
                self.push(receiver.clone());
                return Ok(());
            }
        }

        let mut stages = pending.map(|p| p.stages).unwrap_or_default();
        let mut op = op;
        let mut func = func;
        // A `filter` that *ends* a fused run is cheaper as one more stage than
        // as the terminal. As the terminal it would be handed every element the
        // stages produced, and would answer with a parallel vector of booleans
        // about them — two full-length vectors to build one short one. As a
        // stage it simply does not pass the elements it rejects on, and the
        // terminal collects what arrives.
        if !stages.is_empty() && op == SeqOp::Filter {
            let f = func.take().expect("filter has a callback");
            stages.push(Stage {
                kind: StageKind::Filter(f),
                spread,
                line: self.task.line,
                col: self.task.col,
            });
            op = SeqOp::Collect;
        }
        let (_, source) = self.seq_receiver(who, receiver)?;
        self.begin_seq(SeqJob {
            op,
            shape,
            items: Vec::new(),
            // Reduce seeds its accumulator here; the others accumulate results.
            results: seed.into_iter().collect(),
            next: 0,
            func,
            spread,
            stages,
            src: source,
            work: Vec::new(),
            held: Value::None,
            resume: SeqResume::Terminal,
            limit: usize::MAX,
            pick_first: false,
            drop_done: false,
            reverse,
            line: self.task.line,
            col: self.task.col,
        })
    }

    /// Start a job whose `src` holds the receiver's snapshot. With no fused
    /// stages every element reaches the terminal, so the snapshot *is* the
    /// terminal's input and is moved straight across — an unfused step keeps
    /// the one vector it has always had.
    fn begin_seq(&mut self, mut job: SeqJob) -> Result<(), VmError> {
        if job.stages.is_empty() {
            job.items = std::mem::take(&mut job.src);
        }
        self.task.seq_jobs.push(job);
        self.drive_seq()
    }

    /// Take the pipeline deferred against `receiver`, if the innermost pending
    /// one is it. Anything else is left where it is: a pipeline is only ever
    /// run by the step the compiler emitted to run it.
    fn take_chain(&mut self, receiver: &Value) -> Option<PendingChain> {
        match self.chains.last() {
            Some(p) if same_collection(&p.source, receiver) => self.chains.pop(),
            _ => None,
        }
    }

    /// `first()` and `take(n)` ending a fused chain.
    ///
    /// Neither takes a callback, so the pipeline runs with [`SeqOp::Collect`]
    /// as its terminal — and with a **limit**, which is the half of fusion
    /// worth more than the allocation it saves: `xs.map(f).first()` calls `f`
    /// once instead of four hundred thousand times.
    ///
    /// Returns `None` when the call is not one this can answer (a wrong arity,
    /// a non-integer count). The pipeline is left pending and the native method
    /// runs on the receiver, where it raises the diagnostic it always did.
    ///
    /// `#[inline(never)]` and out of line on purpose: the branch that reaches it
    /// is on every native method call in the program and is taken by almost
    /// none of them, and pass three's item 32 is the record of what an extra
    /// few hundred bytes in the middle of a hot function costs everything
    /// around it.
    #[inline(never)]
    fn chain_tail(
        &mut self,
        receiver: &Value,
        name: &str,
        args: &[Value],
    ) -> Result<Option<Step>, VmError> {
        if !matches!(
            receiver,
            Value::List(_) | Value::Tuple(_) | Value::Dict(_) | Value::Range(_)
        ) {
            return Ok(None);
        }
        let Some((limit, pick_first)) = (match (name, args) {
            ("first", []) => Some((1, true)),
            ("take", [Value::Int(n)]) if *n >= 0 => Some((*n as usize, false)),
            _ => None,
        }) else {
            return Ok(None);
        };
        let Some(p) = self.take_chain(receiver) else {
            return Ok(None);
        };
        let (_, source) = self.seq_receiver(name, receiver)?;
        self.begin_seq(SeqJob {
            op: SeqOp::Collect,
            shape: p.shape,
            items: Vec::new(),
            results: Vec::new(),
            next: 0,
            func: None,
            spread: Spread::Whole,
            stages: p.stages,
            src: source,
            work: Vec::new(),
            held: Value::None,
            resume: SeqResume::Terminal,
            limit,
            pick_first,
            drop_done: false,
            reverse: false,
            line: self.task.line,
            col: self.task.col,
        })
        .map(|()| Some(Step::Next))
    }

    /// The elements a collection operation walks, and the shape to rebuild.
    fn seq_receiver(
        &mut self,
        who: &str,
        receiver: &Value,
    ) -> Result<(SeqShape, Vec<Value>), VmError> {
        let shape = self.seq_shape(who, receiver)?;
        let items = match receiver {
            Value::List(l) => l.borrow().clone(),
            Value::Tuple(t) => t.as_slice().to_vec(),
            Value::Dict(d) => d
                .borrow()
                .items()
                .iter()
                .map(|(k, v)| Value::Tuple(OroTuple::new(vec![k.clone(), v.clone()])))
                .collect(),
            // A range has no literal to rebuild, so it materialises to a list.
            _ => self.wrap(iterate_to_vec(receiver))?,
        };
        Ok((shape, items))
    }

    /// The collection a chain step rebuilds, without taking the snapshot that
    /// running it would need. A step that defers itself still has to know its
    /// shape — the rebuild needs it, and so does a native callback in the
    /// *next* stage, which is handed a dict's pair as two arguments — but it
    /// must not copy the receiver, or fusing would put back the allocation it
    /// exists to remove.
    fn seq_shape(&self, who: &str, receiver: &Value) -> Result<SeqShape, VmError> {
        Ok(match receiver {
            Value::List(_) => SeqShape::List,
            Value::Tuple(_) => SeqShape::Tuple,
            Value::Dict(_) => SeqShape::Dict,
            Value::Range(_) => SeqShape::List,
            other => {
                return Err(self.err(attribute_error(format!(
                    "'{}' object has no method '{who}'",
                    other.type_name()
                ))))
            }
        })
    }

    /// Advance the active job by one callback.
    ///
    /// A fused chain is driven element-first rather than step-first: one
    /// element walks every stage and reaches the terminal before the next
    /// element is read. That is the whole of fusion — it is why the source is
    /// read once, why one collection is built instead of one per step, and why
    /// a `first()` or `take(n)` at the end can stop the upstream pass dead.
    fn drive_seq(&mut self) -> Result<(), VmError> {
        loop {
            // One borrow of the job for the whole decision: which element is
            // next, which callback it goes to, and the argument vector that
            // callback is called with. Building the arguments in here is what
            // lets the element *move* into them instead of being cloned — the
            // element is handled once per stage, so a clone per stage is a
            // clone per element per step of the chain.
            let (who, at, func, spread, item, acc) = {
                let job = self.task.seq_jobs.last_mut().expect("active seq job");
                // `find`, `any` and `all` stop as soon as the answer is settled,
                // so a predicate is never called more often than it must be;
                // `limit` is the same idea for a `take(n)`/`first()` terminal,
                // and it reaches back through the fused stages.
                let settled = match job.op {
                    SeqOp::Find | SeqOp::Any => job.results.iter().any(|r| r.truthy()),
                    SeqOp::All => job.results.iter().any(|r| !r.truthy()),
                    // `take_while` stops the moment its predicate first answers
                    // false: everything after is dropped, so the predicate is
                    // never called on it (a side-effecting or costly predicate
                    // must not run past the stopping point), and the early stop
                    // reaches back through the fused stages like `take`'s does.
                    SeqOp::TakeWhile => job.results.last().is_some_and(|r| !r.truthy()),
                    _ => job.items.len() >= job.limit,
                };
                // Where the next element comes from: part way down the stages,
                // or fresh off the source. With no stages the source *is*
                // `items`, walked in place exactly as it was before fusion.
                let next = if settled {
                    None
                } else if let Some(w) = job.work.pop() {
                    Some(w)
                } else {
                    let src = if job.stages.is_empty() { &job.items } else { &job.src };
                    if job.next < src.len() {
                        let v = src[job.next].clone();
                        job.next += 1;
                        Some((0, v))
                    } else {
                        None
                    }
                };
                let Some((stage, item)) = next else {
                    let job = self.task.seq_jobs.pop().unwrap();
                    return self.finish_seq(job);
                };

                if stage < job.stages.len() {
                    let st = &job.stages[stage];
                    let at = (st.line, st.col);
                    let spread = st.spread;
                    let (f, who, hold) = match &st.kind {
                        StageKind::Map(f) => (f.clone(), "map", false),
                        StageKind::Filter(f) => (f.clone(), "filter", true),
                    };
                    job.resume = SeqResume::Stage(stage);
                    // `filter` answers about an element it does not replace, so
                    // that element has to outlive the call; `map` replaces it
                    // and keeps nothing, which is why only one of the two pays
                    // for a clone.
                    job.held = if hold { item.clone() } else { Value::None };
                    (who, at, Some(f), spread, item, None)
                } else {
                    // Out the bottom of the pipeline: this one is the
                    // terminal's.
                    job.resume = SeqResume::Terminal;
                    // No callback: the `Collect` that a fused chain ends in,
                    // the one step without one. Its answer *is* the elements,
                    // so it takes this one whole and needs no frame at all.
                    let Some(f) = job.func.clone() else {
                        job.items.push(item);
                        continue;
                    };
                    // `drop_while` past its first false keeps every element and
                    // no longer calls the predicate: push the element (with no
                    // stages it is already in `items` from the source walk) and
                    // a synthetic falsy result so `skip_while` keeps it.
                    if job.op == SeqOp::DropWhile && job.drop_done {
                        if !job.stages.is_empty() {
                            job.items.push(item);
                        }
                        job.results.push(Value::Bool(false));
                        continue;
                    }
                    // With no stages `items` is the walk itself and already
                    // holds this element; with stages it has just arrived, and
                    // is kept only by the terminals whose answer is made of
                    // elements rather than of callback results.
                    if !job.stages.is_empty() && job.op.keeps_items() {
                        job.items.push(item.clone());
                    }
                    // Reduce hands over the accumulator it is threading first.
                    let acc = (job.op == SeqOp::Reduce)
                        .then(|| job.results.last().cloned().unwrap_or(Value::None));
                    (job.op.name(), (job.line, job.col), Some(f), job.spread, item, acc)
                }
            };

            let Some(func) = func else {
                unreachable!("a callback-less step records its element in place")
            };
            match func {
                Value::Func(f) => {
                    if self.task.frames.len() >= MAX_FRAMES {
                        return Err(self.err(recursion_error("maximum recursion depth exceeded")));
                    }
                    if f.code.is_generator {
                        // This says something about the *call*, not about the
                        // element it happened to be discovered on, so it is
                        // reported where the call is written — which is not
                        // where the VM's position has got to once a fused chain
                        // has run a callback or two.
                        (self.task.line, self.task.col) = at;
                        return Err(self.err(type_error(format!(
                            "{who}() callback must not be a generator function"
                        ))));
                    }
                    // Fast path: bind the element (and, for `reduce`, the
                    // accumulator) straight into the callee's slots, the way an
                    // ordinary positional call binds off the operand stack.
                    // `seq_args` built a `vec![item]` here, and a chain touches
                    // one element once per stage — so that vector was an
                    // allocation per element per step of the chain, which is the
                    // whole gap between a chain callback and a plain call.
                    // `Whole` is every list/tuple/range callback; a dict's
                    // `(k, v)` destructure (`Unpack`) can fail per element, so it
                    // keeps the vector path that owns that diagnostic.
                    if let Spread::Whole = spread {
                        let n = 1 + usize::from(acc.is_some());
                        if n == f.code.params.len() - f.code.defaults.len() {
                            let mut frame = self.take_frame(f.code.clone(), &f.freevars);
                            let params = &f.code.params;
                            let defaults = &f.code.defaults;
                            let first_defaulted = params.len() - defaults.len();
                            let mut idx = 0;
                            if let Some(acc) = acc {
                                store_param(&mut frame, params[idx].target, acc);
                                idx += 1;
                            }
                            store_param(&mut frame, params[idx].target, item);
                            idx += 1;
                            for (i, p) in params.iter().enumerate().skip(idx) {
                                store_param(
                                    &mut frame,
                                    p.target,
                                    defaults[i - first_defaulted].clone(),
                                );
                            }
                            frame.ret_action = ReturnAction::DriveSeq;
                            self.task.frames.push(frame);
                            return Ok(());
                        }
                    }
                    let call_args = self.seq_unpacked(Self::seq_args(spread, item, acc), at)?;
                    let mut frame = self.bind_call(&f, None, call_args, Vec::new())?;
                    frame.ret_action = ReturnAction::DriveSeq;
                    self.task.frames.push(frame);
                    return Ok(());
                }
                Value::Builtin(b) => {
                    let call_args = self.seq_unpacked(Self::seq_args(spread, item, acc), at)?;
                    let Some(call_args) = self.ord_callback(b.name, call_args, OrdCont::Seq)?
                    else {
                        return Ok(());
                    };
                    let r = self.wrap((b.func)(call_args))?;
                    self.record_seq_result(r);
                }
                Value::Method(m) => {
                    let call_args = self.seq_unpacked(Self::seq_args(spread, item, acc), at)?;
                    let r = match &m.kind {
                        MethodKind::Native(name) => {
                            self.wrap(crate::builtins::call_method(&m.receiver, name, call_args, Vec::new()))?
                        }
                        MethodKind::User { func, defclass } => {
                            if self.task.frames.len() >= MAX_FRAMES {
                                return Err(
                                    self.err(recursion_error("maximum recursion depth exceeded"))
                                );
                            }
                            return self.invoke_user(
                                func.clone(),
                                m.receiver.clone(),
                                defclass.clone(),
                                call_args,
                                Vec::new(),
                                ReturnAction::DriveSeq,
                            );
                        }
                    };
                    self.record_seq_result(r);
                }
                _ => {
                    (self.task.line, self.task.col) = at;
                    return Err(self.err(type_error(format!("{who}() callback is not callable"))));
                }
            }
        }
    }

    /// Record one callback result — the terminal's, or a fused stage's.
    ///
    /// A stage's answer decides what happens to the element it was asked
    /// about: `map` replaces it, `filter` keeps or drops it. Either way the
    /// survivor moves one stage down and the loop picks it up again, so the
    /// element reaches the terminal before the source is read a second time.
    /// `reduce` threads a single accumulator rather than collecting
    /// per-element results, so it replaces instead of appending.
    fn record_seq_result(&mut self, value: Value) {
        let job = self.task.seq_jobs.last_mut().expect("seq job");
        match job.resume {
            SeqResume::Terminal => {
                if job.op == SeqOp::Reduce {
                    job.results.clear();
                }
                // `drop_while` stops dropping — and stops calling its predicate —
                // the moment that predicate first answers false.
                if job.op == SeqOp::DropWhile && !value.truthy() {
                    job.drop_done = true;
                }
                job.results.push(value);
            }
            SeqResume::Stage(n) => {
                let held = std::mem::replace(&mut job.held, Value::None);
                match &job.stages[n].kind {
                    StageKind::Map(_) => job.work.push((n + 1, value)),
                    StageKind::Filter(_) => {
                        if value.truthy() {
                            job.work.push((n + 1, held));
                        }
                    }
                }
            }
        }
    }

    /// Rebuild the result once every callback result is in. Which collection
    /// comes back is governed by [`SeqOp::preserves_shape`]: operations that
    /// select or reorder keep the receiver's type, operations that reshape the
    /// data return a list.
    fn finish_seq(&mut self, job: SeqJob) -> Result<(), VmError> {
        // `first()` fused onto the end of a chain: the pipeline was run with a
        // limit of one, so the answer is the one element that got through.
        if job.pick_first {
            // `first()` **asks**, so an empty result answers `null` — the same
            // as `[].first()` on a bare list and as `d.get(k)` for a missing
            // key — whether the chain was empty to begin with or every element
            // was filtered out before reaching this terminal.
            self.push(job.items.into_iter().next().unwrap_or(Value::None));
            return Ok(());
        }
        let SeqJob { op, shape, items, results, reverse, .. } = job;

        // Scalar answers first — these do not rebuild a collection at all.
        match op {
            SeqOp::Reduce => {
                // `results` carries the accumulator, seeded with the initial value.
                let acc = results.into_iter().next_back().unwrap_or(Value::None);
                self.push(acc);
                return Ok(());
            }
            SeqOp::Find => {
                let found = items
                    .iter()
                    .zip(results.iter())
                    .find(|(_, hit)| hit.truthy())
                    .map(|(it, _)| it.clone());
                self.push(found.unwrap_or(Value::None));
                return Ok(());
            }
            SeqOp::Any => {
                self.push(Value::Bool(results.iter().any(|r| r.truthy())));
                return Ok(());
            }
            SeqOp::All => {
                self.push(Value::Bool(results.iter().all(|r| r.truthy())));
                return Ok(());
            }
            SeqOp::Count => {
                self.push(Value::Int(results.iter().filter(|r| r.truthy()).count() as i64));
                return Ok(());
            }
            SeqOp::MinBy | SeqOp::MaxBy => {
                // The callback's values are the keys, and they are user values:
                // if they are instances the `<` between them is a `__lt__`, so
                // this goes through the ordering machine like every other
                // extreme in the language.
                let want_min = op == SeqOp::MinBy;
                let who = op.name();
                return self.begin_order(
                    OrdKind::Extreme { want_min, who },
                    items,
                    results,
                    false,
                );
            }
            SeqOp::GroupBy => {
                let mut d = crate::value::OroDict::new();
                for (item, key) in items.iter().zip(results.iter()) {
                    let bucket = match self.wrap(d.get(key))? {
                        Some(Value::List(l)) => l,
                        _ => {
                            let l = OroList::new(Vec::new());
                            self.wrap(d.insert(key.clone(), Value::List(l.clone())))?;
                            l
                        }
                    };
                    bucket.borrow_mut().push(item.clone());
                }
                self.push(Value::Dict(Rc::new(RefCell::new(d))));
                return Ok(());
            }
            SeqOp::Partition => {
                let mut yes = Vec::new();
                let mut no = Vec::new();
                for (item, hit) in items.iter().zip(results.iter()) {
                    if hit.truthy() {
                        yes.push(item.clone());
                    } else {
                        no.push(item.clone());
                    }
                }
                let rebuild = |v: Vec<Value>| Self::rebuild_shape(shape, v);
                let pair = vec![self.wrap(rebuild(yes))?, self.wrap(rebuild(no))?];
                self.push(Value::Tuple(OroTuple::new(pair)));
                return Ok(());
            }
            _ => {}
        }

        // Collection answers.
        let kept: Vec<Value> = match op {
            SeqOp::Map => results,
            // A `take(n)` closing a chain: the elements that got through are
            // the answer, and the limit already stopped the pass.
            SeqOp::Collect => items,
            SeqOp::Filter => items
                .iter()
                .zip(results.iter())
                .filter(|(_, keep)| keep.truthy())
                .map(|(it, _)| it.clone())
                .collect(),
            SeqOp::FlatMap => {
                let mut out = Vec::new();
                for r in &results {
                    out.extend(self.wrap(iterate_to_vec(r))?);
                }
                out
            }
            // `sort_by` is finished by the ordering machine rather than
            // rebuilt here, because its keys may need `__lt__`.
            SeqOp::SortBy => return self.begin_order(OrdKind::Sort(shape), items, results, reverse),
            SeqOp::UniqueBy => {
                let mut seen = crate::value::OroDict::new();
                let mut out = Vec::new();
                for (item, key) in items.iter().zip(results.iter()) {
                    if !self.wrap(seen.contains(key))? {
                        self.wrap(seen.insert(key.clone(), Value::Bool(true)))?;
                        out.push(item.clone());
                    }
                }
                out
            }
            SeqOp::TakeWhile => items
                .iter()
                .zip(results.iter())
                .take_while(|(_, hit)| hit.truthy())
                .map(|(it, _)| it.clone())
                .collect(),
            SeqOp::DropWhile => items
                .iter()
                .zip(results.iter())
                .skip_while(|(_, hit)| hit.truthy())
                .map(|(it, _)| it.clone())
                .collect(),
            other => unreachable!("scalar op {} handled above", other.name()),
        };

        let shape = if op.preserves_shape() { shape } else { SeqShape::List };
        let out = self.wrap(Self::rebuild_shape(shape, kept))?;
        self.push(out);
        Ok(())
    }

    /// The argument vector one callback is called with: the accumulator first
    /// when `reduce` is threading one, then the element — whole, or unpacked
    /// into the callback's parameters exactly as `for` would (see [`Spread`]).
    ///
    /// It takes the element **by value**: a whole element moves straight into
    /// the vector, and a fused chain touches one element once per stage, so a
    /// clone here would be a clone per element per step of the chain.
    fn seq_args(spread: Spread, item: Value, acc: Option<Value>) -> VResult<Vec<Value>> {
        match (spread, acc) {
            (Spread::Whole, None) => Ok(vec![item]),
            (Spread::Whole, Some(acc)) => Ok(vec![acc, item]),
            (Spread::Unpack(n), None) => unpack_exact(&item, n as usize),
            (Spread::Unpack(n), Some(acc)) => {
                let mut args = Vec::with_capacity(1 + n as usize);
                args.push(acc);
                args.extend(unpack_exact(&item, n as usize)?);
                Ok(args)
            }
        }
    }

    /// The arguments [`Self::seq_args`] built, or its `ValueError` for an
    /// element that does not unpack — reported where the step is written, as
    /// `for`'s is reported at the loop. By then the VM's own position is
    /// wherever the previous callback returned.
    fn seq_unpacked(
        &mut self,
        args: VResult<Vec<Value>>,
        at: (u32, u32),
    ) -> Result<Vec<Value>, VmError> {
        match args {
            Ok(args) => Ok(args),
            Err(e) => Err(self.seq_unpack_error(e, at)),
        }
    }

    /// The cold half of [`Self::seq_unpacked`], out of line so the callback
    /// path it sits on stays the size it was.
    #[cold]
    #[inline(never)]
    fn seq_unpack_error(&mut self, e: VErr, at: (u32, u32)) -> VmError {
        (self.task.line, self.task.col) = at;
        self.err(e)
    }

    /// Turn a vector of elements back into the collection type `shape`. For a
    /// dict the elements are `(key, value)` pairs.
    fn rebuild_shape(shape: SeqShape, items: Vec<Value>) -> VResult<Value> {
        Ok(match shape {
            SeqShape::List => Value::List(OroList::new(items)),
            SeqShape::Tuple => Value::Tuple(OroTuple::new(items)),
            SeqShape::Dict => {
                let mut d = crate::value::OroDict::new();
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
                            "rebuilding a dict needs 2-element pairs, got {} elements",
                            pair.len()
                        )));
                    }
                    d.insert(pair[0].clone(), pair[1].clone())?;
                }
                Value::Dict(Rc::new(RefCell::new(d)))
            }
        })
    }

    /// If `args` contains a generator, begin draining it (and any later ones)
    /// and retry `callee` afterwards. Returns true when a job was started, in
    /// which case the caller must not proceed with the native call.
    fn materialize_generator_args(
        &mut self,
        callee: &Value,
        args: &[Value],
        kwargs: &[(String, Value)],
    ) -> Result<Option<Step>, VmError> {
        if !args.iter().any(|a| matches!(a, Value::Generator(_))) {
            return Ok(None);
        }
        self.task.mat_jobs.push(MatJob {
            callee: callee.clone(),
            args: args.to_vec(),
            kwargs: kwargs.to_vec(),
            idx: 0,
            items: Vec::new(),
            receiver_in_args: false,
        });
        self.drive_materialize().map(Some)
    }

    /// Drain a generator that is a method *receiver* (`g().map(f)`), then retry
    /// the call with the resulting list as the receiver.
    fn materialize_receiver(
        &mut self,
        callee: &Value,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<Option<Step>, VmError> {
        if !matches!(args.first(), Some(Value::Generator(_))) {
            return Ok(None);
        }
        self.task.mat_jobs.push(MatJob {
            callee: callee.clone(),
            args,
            kwargs,
            idx: 0,
            items: Vec::new(),
            receiver_in_args: true,
        });
        self.drive_materialize().map(Some)
    }

    /// Advance the active materialisation job: resume the generator being
    /// drained, move to the next generator argument, or — when none are left —
    /// pop the job and retry the original call with lists in their place.
    fn drive_materialize(&mut self) -> Result<Step, VmError> {
        loop {
            let next_gen = {
                let job = self.task.mat_jobs.last_mut().expect("materialise job");
                let mut found = None;
                for i in job.idx..job.args.len() {
                    if let Value::Generator(g) = &job.args[i] {
                        job.idx = i;
                        found = Some(g.clone());
                        break;
                    }
                }
                found
            };
            let Some(gen) = next_gen else {
                let mut job = self.task.mat_jobs.pop().expect("materialise job");
                if job.receiver_in_args {
                    // Rebuild the bound method around the drained receiver.
                    let recv = job.args.remove(0);
                    let callee = match &job.callee {
                        Value::Method(m) => Value::Method(Rc::new(BoundMethod {
                            receiver: recv,
                            kind: m.kind.clone(),
                        })),
                        other => other.clone(),
                    };
                    return self.invoke(callee, job.args, job.kwargs);
                }
                return self.invoke(job.callee, job.args, job.kwargs);
            };
            // Resume the generator; each Yield lands in this job's `items` and
            // calls back here, so the drain never grows the native stack.
            // `Some(None)` is a generator that is *running* somewhere — its
            // frame has been taken — as opposed to `None`, one that is
            // finished. See [`Vm::generator_busy`].
            let taken = {
                let mut g = gen.borrow_mut();
                if g.done {
                    None
                } else {
                    Some(take_gen_frame(&mut g))
                }
            };
            match taken {
                Some(Some(frame)) => {
                    if self.task.frames.len() >= MAX_FRAMES {
                        return Err(self.err(recursion_error("maximum recursion depth exceeded")));
                    }
                    self.task.gen_stack.push((gen, GenDriver::Materialize));
                    self.task.frames.push(frame);
                    return Ok(Step::Next);
                }
                Some(None) => return Ok(Step::Raise(self.generator_busy())),
                None => {
                    // Already exhausted: it contributes whatever was collected.
                    let job = self.task.mat_jobs.last_mut().expect("materialise job");
                    let items = std::mem::take(&mut job.items);
                    let idx = job.idx;
                    job.args[idx] = Value::List(OroList::new(items));
                    job.idx += 1;
                }
            }
        }
    }

    /// The `reverse=` option `sort` takes: a flag, so it
    /// is named, and a bool, so `reverse=null` is not a second spelling of
    /// leaving it out. It is a *stable* descending sort — the comparator is
    /// inverted, so equal keys keep their input order — which
    /// `xs.sort_by(f).reversed()` is not: reversing a sorted sequence flips the
    /// ties too.
    fn reverse_kwarg(&mut self, who: &str, kwargs: Vec<(String, Value)>) -> Result<bool, VmError> {
        let mut reverse = false;
        for (k, v) in kwargs {
            match (k.as_str(), v) {
                ("reverse", Value::Bool(b)) => reverse = b,
                ("reverse", other) => {
                    return Err(self.err(type_error(format!(
                        "{who}() reverse must be a bool, not '{}'",
                        other.type_name()
                    ))))
                }
                (other, _) => {
                    return Err(self.err(type_error(format!(
                        "{who}() got an unexpected keyword argument '{other}'"
                    ))))
                }
            }
        }
        Ok(reverse)
    }


    /// Drive an in-flight `print`: render remaining args left to right, calling
    /// `__str__` (through a frame) for instances that define one. When the last
    /// argument is rendered, join with spaces, emit, and push `None`.
    fn do_print(
        &mut self,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<(), VmError> {
        // print() accepts sep= and end=; both must be str or None (None means
        // "use the default"), matching CPython. `file=` and `flush=` are not
        // accepted — Oro has no writable stream objects to point them at.
        let mut sep = " ".to_string();
        let mut end = "\n".to_string();
        for (k, v) in kwargs {
            let slot = match k.as_str() {
                "sep" => &mut sep,
                "end" => &mut end,
                other => {
                    return Err(
                        self.err(type_error(format!(
                            "print() got an unexpected keyword argument '{other}'"
                        )))
                    )
                }
            };
            match v {
                Value::Str(s) => *slot = s.s.clone(),
                Value::None => {}
                other => {
                    return Err(self.err(type_error(format!(
                        "print() argument '{k}' must be str or None, not '{}'",
                        other.type_name()
                    ))))
                }
            }
        }
        self.task.prints.push(PrintJob { rendered: Vec::new(), remaining: args, next: 0, sep, end });
        self.drive_print()
    }

    fn drive_print(&mut self) -> Result<(), VmError> {
        loop {
            let next_val = {
                let job = self.task.prints.last().expect("active print job");
                if job.next >= job.remaining.len() {
                    let job = self.task.prints.pop().unwrap();
                    // `end` is written verbatim, so print(end="") emits no
                    // newline at all — hence write!/flush rather than println!.
                    use std::io::Write;
                    let out = std::io::stdout();
                    let mut out = out.lock();
                    let _ = write!(out, "{}{}", job.rendered.join(&job.sep), job.end);
                    let _ = out.flush();
                    self.push(Value::None);
                    return Ok(());
                }
                job.remaining[job.next].clone()
            };
            self.task.prints.last_mut().unwrap().next += 1;

            if let Value::Instance(inst) = &next_val {
                let cls = inst.class.clone();
                let hit = Class::find(&cls, "__str__").or_else(|| Class::find(&cls, "__repr__"));
                if let Some((Value::Func(f), defclass)) = hit {
                    return self.invoke_user(
                        f,
                        next_val.clone(),
                        defclass,
                        Vec::new(),
                        Vec::new(),
                        ReturnAction::DrivePrint,
                    );
                }
            }
            // A container argument is rendered element-by-element (running
            // element __repr__ dunders, cycle-safe); the StrJob feeds its result
            // back into this print job.
            if is_container(&next_val) {
                return self.begin_stringify(next_val, StrCont::Print);
            }
            let s = next_val.display();
            self.task.prints.last_mut().unwrap().rendered.push(s);
        }
    }

    /// Begin rendering a container `value` to a string, running element
    /// `__repr__` dunders through frames. If nothing needs a dunder, the string
    /// is built immediately; otherwise a [`StrJob`] drives the dunder calls.
    fn begin_stringify(&mut self, value: Value, cont: StrCont) -> Result<(), VmError> {
        let mut instances = Vec::new();
        let mut path = Vec::new();
        collect_repr_instances(&value, &mut instances, &mut path);
        self.task.str_jobs.push(StrJob { value, instances, results: Vec::new(), next: 0, cont });
        self.drive_str()
    }

    /// Advance the top str job by one element `__repr__` call, or finish it.
    /// Re-entered via the `DriveStr` return action after each dunder returns.
    fn drive_str(&mut self) -> Result<(), VmError> {
        let next_inst = {
            let job = self.task.str_jobs.last().expect("active str job");
            (job.next < job.instances.len()).then(|| job.instances[job.next].clone())
        };
        if let Some(inst) = next_inst {
            self.task.str_jobs.last_mut().unwrap().next += 1;
            // Every collected instance has an Oro __repr__ (the collection
            // criterion), so this always dispatches a frame.
            let (f, defclass) =
                instance_method(&inst, "__repr__").expect("collected instance has __repr__");
            return self.invoke_user(f, inst, defclass, Vec::new(), Vec::new(), ReturnAction::DriveStr);
        }
        // All element reprs are ready: rebuild the string and run the cont.
        let job = self.task.str_jobs.pop().unwrap();
        let mut idx = 0;
        let mut path = Vec::new();
        let s = build_repr(&job.value, &job.results, &mut idx, &mut path);
        match job.cont {
            StrCont::Push => self.push(Value::str(s)),
            StrCont::Print => {
                self.task.prints.last_mut().expect("print job").rendered.push(s);
                self.drive_print()?;
            }
            StrCont::FormatSpec(spec) => {
                let out = self.wrap(crate::format::format_value(
                    &Value::str(s),
                    crate::format::CONV_NONE,
                    &spec,
                ))?;
                self.push(Value::str(out));
            }
        }
        Ok(())
    }

    /// Dispatch a rich-comparison dunder for `a op b` at the *top level* of an
    /// operator. Returns `true` (and pushes a frame) when one was found;
    /// `false` to fall back to the default comparison.
    ///
    /// Two things separate this from the same dispatch inside a container
    /// (`Vm::step_pair`), and both are CPython's behaviour rather than
    /// convenience:
    ///
    /// * **The dunder's value is pushed raw.** `__eq__` may return anything,
    ///   and `a == b` is that thing, not its truthiness — `T() == T()` where
    ///   `T.__eq__` answers `[1, 2, 3]` *is* `[1, 2, 3]`. Truthiness is applied
    ///   only where a decision has to be made, which is inside `in` and
    ///   container comparison.
    /// * **There is no identity shortcut.** `a == a` runs `__eq__` and answers
    ///   `false` if that is what it says, while `a in [a]` is `true` without
    ///   calling anything.
    fn try_compare_dunder(&mut self, cmp: CmpOp, a: &Value, b: &Value) -> Result<bool, VmError> {
        let Some(name) = rich_dunder(cmp) else {
            // `in` and `not in` have no rich-comparison dunder.
            return Ok(false);
        };
        if let Some((f, defclass)) = instance_method(a, name) {
            self.invoke_user(f, a.clone(), defclass, vec![b.clone()], Vec::new(), ReturnAction::Normal)?;
            return Ok(true);
        }
        // `!=` falls back to the negation of `__eq__`.
        if matches!(cmp, CmpOp::NotEq) {
            if let Some((f, defclass)) = instance_method(a, "__eq__") {
                self.invoke_user(f, a.clone(), defclass, vec![b.clone()], Vec::new(), ReturnAction::NegateBool)?;
                return Ok(true);
            }
        }
        // The reflected operand: `1 == obj` asks `obj.__eq__(1)`, and `1 < obj`
        // asks `obj.__gt__(1)`. CPython reaches this when the left operand's
        // own attempt returns `NotImplemented`; Oro's types have no `__eq__` to
        // return it from, so "the left operand had nothing to say" is exactly
        // the case above having fallen through.
        let reflected = reflect_dunder(cmp);
        if let Some((f, defclass)) = instance_method(b, reflected) {
            self.invoke_user(f, b.clone(), defclass, vec![a.clone()], Vec::new(), ReturnAction::Normal)?;
            return Ok(true);
        }
        if matches!(cmp, CmpOp::NotEq) {
            if let Some((f, defclass)) = instance_method(b, "__eq__") {
                self.invoke_user(f, b.clone(), defclass, vec![a.clone()], Vec::new(), ReturnAction::NegateBool)?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    // --- deep comparison ------------------------------------------------------

    /// The half of `Op::Compare` that needs Oro code, kept out of line so the
    /// interpreter's dispatch loop stays the size it was. Comparison is the
    /// hottest non-arithmetic instruction in the language and this branch is
    /// taken only by programs with a comparison dunder in them.
    #[inline(never)]
    fn compare_slow(&mut self, cmp: CmpOp, a: Value, b: Value) -> Result<(), VmError> {
        // A dunder on the operands themselves is dispatched from here, so that
        // its value reaches the program unconverted: CPython's `a == b` is
        // whatever `__eq__` returned, not its truthiness.
        if self.try_compare_dunder(cmp, &a, &b)? {
            return Ok(());
        }
        // Anything deeper — a container holding one, or `in` over a list —
        // goes to the machine, where the answer *is* a decision and truthiness
        // does apply.
        let (op, negate) = match cmp {
            CmpOp::NotEq => (CmpOp::Eq, true),
            CmpOp::NotIn => (CmpOp::NotIn, true),
            other => (other, false),
        };
        self.begin_compare(op, a, b, negate)
    }

    /// Start a comparison that native code deferred: `a op b` where some pair
    /// inside it needs a user dunder. `negate` inverts the finished answer,
    /// which is how `!=` and `not in` are spelled once `==` and `in` exist.
    fn begin_compare(
        &mut self,
        op: CmpOp,
        a: Value,
        b: Value,
        negate: bool,
    ) -> Result<(), VmError> {
        self.task.cmp_jobs.push(CmpJob { levels: Vec::new(), cont: CmpCont::Push { negate } });
        let next = match op {
            CmpOp::In | CmpOp::NotIn => {
                let items = self.wrap(membership_items(&b, &a))?;
                self.task.cmp_jobs.last_mut().expect("cmp job").levels.push(CmpLevel::Contains {
                    items,
                    item: a,
                    i: 0,
                });
                self.advance_top(None)?
            }
            _ => CmpNext::Ask(a, b, op),
        };
        self.drive_cmp(next)
    }

    /// The comparison machine's loop. Runs until the whole comparison is
    /// decided, or until a dunder frame is pushed — at which point it returns,
    /// and `ReturnAction::DriveCmp` calls it again with the answer.
    fn drive_cmp(&mut self, mut next: CmpNext) -> Result<(), VmError> {
        loop {
            match next {
                CmpNext::Ask(a, b, op) => match self.step_pair(a, b, op)? {
                    PairStep::Done(v) => next = CmpNext::Give(v),
                    PairStep::Ask(a, b, op) => next = CmpNext::Ask(a, b, op),
                    PairStep::Pushed => next = self.advance_top(None)?,
                    PairStep::Dispatched => return Ok(()),
                },
                CmpNext::Give(v) => {
                    if self.task.cmp_jobs.last().expect("cmp job").levels.is_empty() {
                        let job = self.task.cmp_jobs.pop().expect("cmp job");
                        return self.finish_cmp(job.cont, v);
                    }
                    next = self.advance_top(Some(v))?;
                }
            }
        }
    }

    /// Deliver the finished comparison.
    fn finish_cmp(&mut self, cont: CmpCont, v: bool) -> Result<(), VmError> {
        match cont {
            CmpCont::Push { negate } => {
                self.push(Value::Bool(v != negate));
                Ok(())
            }
            CmpCont::Order => self.drive_ord(Some(v)),
        }
    }

    /// Decide one pair. Either it falls out natively, or a dunder is dispatched
    /// for it, or it is structural and becomes a level of its own.
    fn step_pair(&mut self, a: Value, b: Value, op: CmpOp) -> Result<PairStep, VmError> {
        // Native first: this is the same code the fast path runs, and it
        // answers every pair with no user dunder underneath it.
        if let Some(r) = self.wrap(try_compare_op(op, &a, &b))? {
            return Ok(PairStep::Done(r));
        }

        // A user dunder on the left, then the reflected one on the right.
        // Unlike the top-level dispatch, the answer here is a decision, so the
        // returned value is taken for its truthiness.
        if let Some(name) = rich_dunder(op) {
            if let Some((f, defclass)) = instance_method(&a, name) {
                self.task.cmp_jobs.last_mut().expect("cmp job")
                    .levels.push(CmpLevel::Dunder);
                self.invoke_user(f, a, defclass, vec![b], Vec::new(), ReturnAction::DriveCmp)?;
                return Ok(PairStep::Dispatched);
            }
            if let Some((f, defclass)) = instance_method(&b, reflect_dunder(op)) {
                self.task.cmp_jobs.last_mut().expect("cmp job")
                    .levels.push(CmpLevel::Dunder);
                self.invoke_user(f, b, defclass, vec![a], Vec::new(), ReturnAction::DriveCmp)?;
                return Ok(PairStep::Dispatched);
            }
        }

        // Structural: the pair itself has no dunder, but something inside it
        // does. Descending is what grows the level stack, so it is where the
        // cycle guard sits.
        if self.task.cmp_jobs.last().expect("cmp job").levels.len() >= CMP_DEPTH_LIMIT {
            return Err(self.err(recursion_error("maximum recursion depth exceeded in comparison")));
        }
        let level = match (&a, &b) {
            (Value::List(x), Value::List(y)) => {
                CmpLevel::Seq { a: x.borrow().clone(), b: y.borrow().clone(), i: 0, op, deciding: false }
            }
            (Value::Tuple(x), Value::Tuple(y)) => {
                CmpLevel::Seq { a: (**x).clone(), b: (**y).clone(), i: 0, op, deciding: false }
            }
            (Value::Dict(x), Value::Dict(y)) => {
                let (x, y) = (x.borrow(), y.borrow());
                if x.len() != y.len() {
                    return Ok(PairStep::Done(false));
                }
                let mut av = Vec::with_capacity(x.len());
                let mut bv = Vec::with_capacity(x.len());
                for (k, v) in x.items() {
                    // The key match is by `HKey` and never runs user code; only
                    // the values are compared with `==`.
                    match self.wrap(y.get(k))? {
                        Some(other) => {
                            av.push(v.clone());
                            bv.push(other);
                        }
                        None => return Ok(PairStep::Done(false)),
                    }
                }
                CmpLevel::Vals { a: av, b: bv, i: 0 }
            }
            // Two bound methods with the same function reduce to their
            // receivers, which may themselves be instances with `__eq__`.
            (Value::Method(x), Value::Method(y)) => {
                return Ok(PairStep::Ask(x.receiver.clone(), y.receiver.clone(), CmpOp::Eq))
            }
            // No dunder and no structure. For an ordering that is the
            // `TypeError`; for equality, identity, which is where an instance
            // whose class defines only `__lt__` lands.
            _ => {
                return match op {
                    CmpOp::Eq | CmpOp::NotEq => Ok(PairStep::Done(same_object(&a, &b))),
                    _ => Err(self.err(crate::value::unorderable(op_symbol(op), &a, &b))),
                }
            }
        };
        self.task.cmp_jobs.last_mut().expect("cmp job").levels.push(level);
        Ok(PairStep::Pushed)
    }

    /// Advance the innermost level, folding in the answer it was waiting on
    /// (`None` when the level has only just been pushed). A level that finishes
    /// is popped and its answer flows to the level beneath it.
    fn advance_top(&mut self, incoming: Option<bool>) -> Result<CmpNext, VmError> {
        // A loop, not a self-call: `LevelStep::Retry` fires once per element
        // settled by the identity shortcut, and a list of ten thousand
        // references to one object would otherwise be ten thousand Rust frames
        // — the recursion this whole design exists to not do.
        let mut incoming = incoming;
        loop {
            match self.advance_once(incoming)? {
                Some(next) => return Ok(next),
                None => incoming = None,
            }
        }
    }

    /// One step of [`advance_top`]. `None` means the level settled an element
    /// without asking anything and wants to be advanced again.
    fn advance_once(&mut self, incoming: Option<bool>) -> Result<Option<CmpNext>, VmError> {
        let job = self.task.cmp_jobs.last_mut().expect("cmp job");
        let level = job.levels.last_mut().expect("cmp level");
        let step = match level {
            // A dunder level is popped by the `DriveCmp` return action, which
            // is the only thing that can answer it.
            CmpLevel::Dunder => unreachable!("a dunder level is resumed by its frame"),
            CmpLevel::Seq { a, b, i, op, deciding } => {
                if *deciding {
                    // The deciding pair was asked with the original operator,
                    // so its answer is the sequence's answer.
                    LevelStep::Done(incoming.expect("a deciding pair was asked"))
                } else if incoming == Some(false) {
                    // The first pair that differs decides the whole comparison
                    // — by `op`, which for `==` is simply "unequal".
                    let k = *i - 1;
                    if matches!(op, CmpOp::Eq) {
                        LevelStep::Done(false)
                    } else {
                        *deciding = true;
                        LevelStep::Ask(a[k].clone(), b[k].clone(), *op)
                    }
                } else if *i < a.len().min(b.len()) {
                    let k = *i;
                    *i += 1;
                    if same_object(&a[k], &b[k]) {
                        // CPython's identity shortcut, which is why `a in [a]`
                        // is true even when `a.__eq__` says otherwise.
                        LevelStep::Retry
                    } else {
                        LevelStep::Ask(a[k].clone(), b[k].clone(), CmpOp::Eq)
                    }
                } else {
                    // One ran out: the lengths settle it.
                    LevelStep::Done(ord_holds(*op, a.len().cmp(&b.len())))
                }
            }
            CmpLevel::Vals { a, b, i } => {
                if incoming == Some(false) {
                    LevelStep::Done(false)
                } else if *i < a.len() {
                    let k = *i;
                    *i += 1;
                    if same_object(&a[k], &b[k]) {
                        LevelStep::Retry
                    } else {
                        LevelStep::Ask(a[k].clone(), b[k].clone(), CmpOp::Eq)
                    }
                } else {
                    LevelStep::Done(true)
                }
            }
            CmpLevel::Contains { items, item, i } => {
                if incoming == Some(true) {
                    LevelStep::Done(true)
                } else if *i < items.len() {
                    let k = *i;
                    *i += 1;
                    if same_object(&items[k], item) {
                        LevelStep::Done(true)
                    } else {
                        // The container's element goes on the *left*, which is
                        // what decides whose `__eq__` runs for `x in xs`.
                        LevelStep::Ask(items[k].clone(), item.clone(), CmpOp::Eq)
                    }
                } else {
                    LevelStep::Done(false)
                }
            }
        };
        match step {
            LevelStep::Ask(a, b, op) => Ok(Some(CmpNext::Ask(a, b, op))),
            LevelStep::Retry => Ok(None),
            LevelStep::Done(v) => {
                self.task.cmp_jobs.last_mut().expect("cmp job").levels.pop();
                Ok(Some(CmpNext::Give(v)))
            }
        }
    }

    // --- orderings that need `__lt__` -----------------------------------------

    /// Order `items` by `keys` — sort, or pick an extreme — running a user
    /// `__lt__` through frames when the keys need one.
    ///
    /// Keys with no user ordering take the native path unchanged: the scan that
    /// decides is one discriminant test per element, against a sort that is
    /// already `O(n log n)` comparisons.
    fn begin_order(
        &mut self,
        kind: OrdKind,
        items: Vec<Value>,
        keys: Vec<Value>,
        reverse: bool,
    ) -> Result<(), VmError> {
        self.begin_order_to(kind, items, keys, reverse, OrdCont::Push)
    }

    /// [`begin_order`](Self::begin_order) for an ordering whose answer does not
    /// belong on the operand stack. See [`OrdCont`].
    fn begin_order_to(
        &mut self,
        kind: OrdKind,
        items: Vec<Value>,
        keys: Vec<Value>,
        reverse: bool,
        cont: OrdCont,
    ) -> Result<(), VmError> {
        // The same predicate the entry points use, asked again here because
        // `sort_by`, `min_by` and `sorted(key=…)` arrive with keys a callback
        // produced, which nothing could have scanned earlier.
        if !keys.iter().any(ord_defers) {
            return self.finish_order_native(kind, items, keys, reverse, cont);
        }
        // `keys`, not `items`: an in-place sort carries its elements in the
        // lent storage and has no `items` at all.
        let n = keys.len();
        let state = match kind {
            OrdKind::Extreme { .. } => {
                if n == 0 {
                    // The native path owns the empty-sequence message.
                    return self.finish_order_native(kind, items, keys, reverse, cont);
                }
                OrdState::Fold { best: 0, next: 1 }
            }
            _ => OrdState::Merge {
                src: (0..n).collect(),
                dst: vec![0; n],
                width: 1,
                lo: 0,
                mid: 1.min(n),
                hi: 2.min(n),
                i: 0,
                j: 1.min(n),
            },
        };
        self.task.ord_jobs.push(OrdJob { kind, cont, keys, items, reverse, state });
        self.drive_ord(None)
    }

    /// The ordering that needs no frames, which is every ordering in a program
    /// with no user `__lt__` in it.
    fn finish_order_native(
        &mut self,
        kind: OrdKind,
        items: Vec<Value>,
        keys: Vec<Value>,
        reverse: bool,
        cont: OrdCont,
    ) -> Result<(), VmError> {
        match kind {
            OrdKind::Extreme { want_min, who } => {
                let want =
                    if want_min { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater };
                let sym = if want_min { "<" } else { ">" };
                let mut best = 0;
                if items.is_empty() {
                    return Err(self.err(value_error(format!("{who}() arg is an empty sequence"))));
                }
                for i in 1..items.len() {
                    let ord =
                        self.wrap(crate::builtins::ord_or_defer(&keys[i], &keys[best], sym))?;
                    if ord == want {
                        best = i;
                    }
                }
                let best = items[best].clone();
                self.deliver_order(cont, best)
            }
            OrdKind::Sort(shape) => {
                let sorted = self.wrap(crate::builtins::sort_by_keys(items, &keys, reverse))?;
                self.finish_order(shape, sorted, cont)
            }
        }
    }

    /// Turn a finished `sort_by` into the collection it produces.
    fn finish_order(
        &mut self,
        shape: SeqShape,
        out: Vec<Value>,
        cont: OrdCont,
    ) -> Result<(), VmError> {
        let v = self.wrap(Self::rebuild_shape(shape, out))?;
        self.deliver_order(cont, v)
    }

    /// Hand a sorted list's storage back to it and answer `null` — or, when a
    /// Hand a finished ordering to whatever asked for it.
    fn deliver_order(&mut self, cont: OrdCont, v: Value) -> Result<(), VmError> {
        match cont {
            OrdCont::Push => {
                self.push(v);
                Ok(())
            }
            OrdCont::Seq => {
                self.record_seq_result(v);
                self.drive_seq()
            }
        }
    }

    /// A native ordering reached as a *callback* rather than as a call the
    /// program wrote — `xs.map(sorted)`, `sorted(xs, key=min)`. Native code
    /// cannot run a `__lt__`, and a callback's result does not go on the
    /// operand stack, so both facts have to be handled here.
    ///
    /// `Ok(Some(args))` hands the arguments back for the ordinary native call,
    /// which is what every callback that is not one of these three gets.
    fn ord_callback(
        &mut self,
        name: &str,
        args: Vec<Value>,
        cont: OrdCont,
    ) -> Result<Option<Vec<Value>>, VmError> {
        if !matches!(name, "min" | "max") || !ord_needs_vm(&args) {
            return Ok(Some(args));
        }
        // Only the scalar form is left, so a single argument is the narrowed
        // arity's error rather than an iterable to range over.
        if args.len() < 2 {
            return Err(self.err(type_error(crate::builtins::extreme_arity_message(name))));
        }
        let keys = args.clone();
        let kind = match name {
            "min" => OrdKind::Extreme { want_min: true, who: "min" },
            _ => OrdKind::Extreme { want_min: false, who: "max" },
        };
        self.begin_order_to(kind, args, keys, false, cont).map(|()| None)
    }

    /// The ordering machine's loop: run until the ordering is finished, or
    /// until a `<` has to be decided by Oro code. `answer` is the `<` the
    /// previous suspension asked for.
    fn drive_ord(&mut self, mut answer: Option<bool>) -> Result<(), VmError> {
        loop {
            let (ask, op) = {
                let job = self.task.ord_jobs.last_mut().expect("ord job");
                match job.state {
                    // `min` asks "is the candidate below the winner?" and `max`
                    // the mirror, which is CPython's split: `max` of a class
                    // that defines only `__lt__` works, but by reflection, and
                    // a class that defines neither says so with a `>`.
                    OrdState::Fold { .. } => {
                        let op = match job.kind {
                            OrdKind::Extreme { want_min: true, .. } => CmpOp::Lt,
                            _ => CmpOp::Gt,
                        };
                        (Self::step_fold(job, answer.take()), op)
                    }
                    // A sort is spelled entirely in `<`, as CPython's is.
                    OrdState::Merge { .. } => (Self::step_merge(job, answer.take()), CmpOp::Lt),
                }
            };
            let Some((lhs, rhs)) = ask else {
                let job = self.task.ord_jobs.pop().expect("ord job");
                return self.finish_ord_job(job);
            };
            let job = self.task.ord_jobs.last().expect("ord job");
            let (lhs, rhs) = (job.keys[lhs].clone(), job.keys[rhs].clone());
            // Native comparison still answers most pairs even here: only the
            // ones that really are instances cost a frame.
            match self.wrap(try_compare_op(op, &lhs, &rhs))? {
                Some(v) => answer = Some(v),
                None => {
                    self.task.cmp_jobs.push(CmpJob { levels: Vec::new(), cont: CmpCont::Order });
                    return self.drive_cmp(CmpNext::Ask(lhs, rhs, op));
                }
            }
        }
    }

    /// One step of the min/max fold. `min` keeps the first of several equal
    /// smallest elements and `max` the first of several largest, which is
    /// CPython's rule and falls out of asking for a *strict* `<`.
    fn step_fold(job: &mut OrdJob, answer: Option<bool>) -> Option<(usize, usize)> {
        let n = job.keys.len();
        let OrdState::Fold { best, next } = &mut job.state else {
            unreachable!("step_fold on a merge")
        };
        if let Some(take) = answer {
            if take {
                *best = *next;
            }
            *next += 1;
        }
        (*next < n).then_some((*next, *best))
    }

    /// One step of the bottom-up merge sort: fold in the `<` that was asked
    /// for, then return the next pair to compare, or `None` when the sort is
    /// done.
    ///
    /// The entire sort state is the six indices in [`OrdState::Merge`], so
    /// suspending it costs nothing and resuming it is this function. Bottom-up
    /// is what makes that true: a top-down merge sort would have a Rust call
    /// stack to make explicit as well.
    fn step_merge(job: &mut OrdJob, answer: Option<bool>) -> Option<(usize, usize)> {
        let reverse = job.reverse;
        let n = job.keys.len();
        let OrdState::Merge { src, dst, width, lo, mid, hi, i, j } = &mut job.state else {
            unreachable!("step_merge on a fold")
        };
        if let Some(take_right) = answer {
            // The write position: `lo + (i - lo) + (j - mid)`.
            let k = *i + *j - *mid;
            if take_right {
                dst[k] = src[*j];
                *j += 1;
            } else {
                dst[k] = src[*i];
                *i += 1;
            }
        }
        loop {
            if *width >= n {
                return None;
            }
            if *lo >= n {
                // The pass is done: what was written becomes what is read.
                std::mem::swap(src, dst);
                *width *= 2;
                if *width >= n {
                    return None;
                }
                *lo = 0;
                *mid = (*width).min(n);
                *hi = (2 * *width).min(n);
                *i = 0;
                *j = *mid;
                continue;
            }
            if *i < *mid && *j < *hi {
                // Forward asks "is the right run's element strictly smaller?",
                // and takes from the left otherwise, which is what makes the
                // merge stable. Reversed asks the mirror — inverting the
                // comparator rather than the output, so equal elements keep
                // their original order in both directions, as CPython promises.
                let (l, r) = (src[*i], src[*j]);
                return Some(if reverse { (l, r) } else { (r, l) });
            }
            let k = *i + *j - *mid;
            if *i < *mid {
                dst[k] = src[*i];
                *i += 1;
            } else if *j < *hi {
                dst[k] = src[*j];
                *j += 1;
            } else {
                // This pair of runs is merged; line up the next pair.
                *lo = *hi;
                if *lo < n {
                    *mid = (*lo + *width).min(n);
                    *hi = (*lo + 2 * *width).min(n);
                    *i = *lo;
                    *j = *mid;
                }
            }
        }
    }

    /// Undecorate: turn the finished permutation back into values.
    fn finish_ord_job(&mut self, job: OrdJob) -> Result<(), VmError> {
        match job.state {
            OrdState::Fold { best, .. } => {
                let v = job.items[best].clone();
                self.deliver_order(job.cont, v)
            }
            OrdState::Merge { src, .. } => match job.kind {
                OrdKind::Sort(shape) => {
                    let mut out = job.items;
                    crate::builtins::apply_permutation(&mut out, src);
                    self.finish_order(shape, out, job.cont)
                }
                OrdKind::Extreme { .. } => unreachable!("an extreme is a fold, not a merge"),
            },
        }
    }

    /// Assemble a class from the member values on the stack (see
    /// [`Op::BuildClass`]) and push it.
    fn build_class(&mut self, spec: &ClassSpec) -> Result<(), VmError> {
        let member_vals = self.popn(spec.members.len());
        let base = if spec.has_base {
            match self.pop() {
                Value::Class(c) => Some(c),
                other => {
                    return Err(self.err(type_error(format!(
                        "base of class '{}' must be a class, not '{}'",
                        spec.name,
                        other.type_label()
                    ))))
                }
            }
        } else {
            None
        };
        let mut members = Fields::with_capacity(spec.members.len());
        for (n, v) in spec.members.iter().zip(member_vals) {
            members.insert(n.clone(), v);
        }
        // A class inherits exception-hood from its base, so user exceptions
        // (`class MyError(Exception)`) render and raise like built-in ones.
        let is_exception = base.as_ref().is_some_and(|b| b.is_exception);
        let class = Class {
            name: spec.name.clone(),
            base,
            members: RefCell::new(members),
            is_exception,
        };
        self.push(Value::Class(Rc::new(class)));
        Ok(())
    }

    // --- proc -----------------------------------------------------------------

    /// `proc.run(args, check=…, quiet=…, cwd=…, env=…, timeout=…)`. Args go
    /// straight to execve (no shell, ever), so shell injection is impossible by
    /// construction.
    ///
    /// The defaults are chosen for orchestration scripts rather than for CPython
    /// compatibility: output is **both** streamed live and captured, and a
    /// nonzero exit **raises** `CommandError`. The captured streams are `bytes`,
    /// since a child emits octets and nothing here knows whether they are text.
    fn do_proc_run(
        &mut self,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<(), VmError> {
        // The command is the only positional argument; every option below is
        // passed by name. A second positional used to be dropped unread.
        if args.len() > 1 {
            return Err(self.err(type_error(format!(
                "proc.run() takes 1 positional argument, the command, but {} were given — \
                 cwd=, env=, timeout=, check= and quiet= are passed by name",
                args.len()
            ))));
        }
        // The command must be a list of separate strings.
        let list = match args.first() {
            Some(Value::List(l)) => l.borrow().clone(),
            Some(Value::Str(_)) => {
                return Err(self.err(
                    type_error("proc.run() needs a list of separate string arguments, e.g. \
                     [\"git\", \"status\"], not a single string — Oro will not split it (that \
                     would mean reimplementing shell quoting) and there is no shell=True.",)
                ))
            }
            _ => return Err(self.err(type_error("proc.run() takes a list of strings"))),
        };
        if list.is_empty() {
            return Err(self.err(value_error("proc.run() got an empty argument list")));
        }
        let mut parts: Vec<String> = Vec::with_capacity(list.len());
        for v in &list {
            match v {
                Value::Str(s) => parts.push(s.s.clone()),
                other => {
                    return Err(self.err(type_error(format!(
                        "proc.run() arguments must all be strings, got '{}'",
                        other.type_label()
                    ))))
                }
            }
        }
        if parts[0].is_empty() || parts[0].contains(char::is_whitespace) {
            return Err(self.err(value_error(format!(
                "proc.run() program '{}' contains whitespace — pass separate arguments \
                 like [\"git\", \"status\"], not one combined string",
                parts[0]
            ))));
        }

        // Keyword args: cwd, env, timeout.
        let mut cwd: Option<String> = None;
        let mut env: Option<Vec<(String, String)>> = None;
        let mut timeout: Option<f64> = None;
        let mut check = true;
        let mut quiet = false;
        for (k, v) in &kwargs {
            match k.as_str() {
                "cwd" => match v {
                    Value::Str(s) => cwd = Some(s.s.clone()),
                    _ => return Err(self.err(type_error("proc.run() cwd must be a string"))),
                },
                "env" => match v {
                    Value::Dict(d) => {
                        let mut pairs = Vec::new();
                        for (ek, ev) in d.borrow().items() {
                            pairs.push((ek.display(), ev.display()));
                        }
                        env = Some(pairs);
                    }
                    _ => return Err(self.err(type_error("proc.run() env must be a dict"))),
                },
                "timeout" => match v {
                    Value::Int(i) => timeout = Some(*i as f64),
                    Value::Float(f) => timeout = Some(*f),
                    _ => return Err(self.err(type_error("proc.run() timeout must be a number"))),
                },
                "check" => check = v.truthy(),
                "quiet" => quiet = v.truthy(),
                // CPython's knobs for what Oro now does by default. Name them
                // explicitly rather than let them silently do nothing.
                "capture_output" | "text" => {
                    return Err(self.err(type_error(format!(
                        "proc.run() does not take '{k}' — it always captures stdout/stderr as \
                         bytes, and streams them live unless quiet=True. Call `.to_str()` on \
                         one to decode it."
                    ))))
                }
                other => {
                    return Err(self.err(type_error(format!(
                        "proc.run() got an unexpected keyword argument '{other}'"
                    ))))
                }
            }
        }

        // Always piped: the reader threads tee each stream onward as it arrives,
        // so the caller gets live output *and* a captured copy. Making every
        // script choose between watching a build and grepping its output is the
        // wart this default exists to remove.
        let mut cmd = std::process::Command::new(&parts[0]);
        cmd.args(&parts[1..]);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        if let Some(dir) = &cwd {
            cmd.current_dir(dir);
        }
        if let Some(e) = &env {
            cmd.env_clear();
            for (k, val) in e {
                cmd.env(k, val);
            }
        }

        let output = match run_process(cmd, timeout, !quiet) {
            Ok(o) => o,
            // "timed out" is classified into TimeoutError; a missing/inexecutable
            // program's io error into FileNotFoundError/PermissionError.
            Err(RunError::Timeout) => {
                return Err(self.err(timeout_error(format!(
                    "command timed out after {} seconds",
                    timeout.unwrap_or(0.0)
                ))))
            }
            Err(RunError::Io(e)) => return Err(self.err(modules::io_err(&e, &parts[0]))),
        };

        let returncode = output.status.code().unwrap_or(-1) as i64;

        // A command that fails and is never checked is one of the great sources
        // of silent breakage in shell scripts, so `check` defaults to on. The
        // message carries the tail of stderr: by the time you are reading it,
        // that is what you wanted, and going to fetch it is pure friction.
        if check && returncode != 0 {
            // The message is for a human, so it is the one place the captured
            // octets are decoded lossily rather than handed back as bytes.
            let stderr = String::from_utf8_lossy(&output.stderr);
            let tail: Vec<&str> = stderr.trim_end().lines().rev().take(3).collect();
            let mut detail = String::new();
            for line in tail.iter().rev() {
                detail.push_str("\n  ");
                detail.push_str(line);
            }
            return Err(self.err(command_error(format!(
                "command failed: {} exited with code {returncode}{detail}",
                parts.join(" "),
            ))));
        }

        let truncated =
            output.stdout.len() >= MAX_CAPTURE_BYTES || output.stderr.len() >= MAX_CAPTURE_BYTES;

        let mut fields = Fields::new();
        fields.insert(Rc::from("returncode"), Value::Int(returncode));
        fields.insert(Rc::from("ok"), Value::Bool(returncode == 0));
        fields.insert(Rc::from("truncated"), Value::Bool(truncated));
        // A child's streams are octets. It may emit a JPEG, or a UTF-8
        // sequence cut in half by the capture limit, and decoding either
        // lossily is how a pipeline quietly corrupts data. Decode with
        // `.to_str()` at the point the program knows it is text.
        fields.insert(Rc::from("stdout"), Value::bytes(output.stdout));
        fields.insert(Rc::from("stderr"), Value::bytes(output.stderr));
        fields.insert(Rc::from("args"), Value::List(OroList::new(list)));
        self.push(Value::Instance(Rc::new(Instance {
            class: self.proc_class.clone(),
            fields: RefCell::new(fields),
        })));
        Ok(())
    }

    // --- Imports -------------------------------------------------------------

    /// Import the module named by the dotted `path`: a built-in, a cached user
    /// module, or a freshly loaded one whose body runs once (as a frame) before
    /// its namespace is captured. Pushes the module value (or raises).
    fn import_module(&mut self, path: &str) -> Result<Step, VmError> {
        // Built-in modules first — except the underscored ones, which exist
        // only to hand an Oro-written stdlib module the two or three things
        // that must be Rust. They are not language surface, so they resolve
        // from inside `std/` and nowhere else.
        if !path.starts_with('_') || self.in_stdlib_module() {
            if let Some(m) = modules::build(path, &self.argv) {
                self.push(m);
                return Ok(Step::Next);
            }
        }
        // Already imported? Reuse the cached namespace (import runs once).
        if let Some(m) = self.module_cache.get(path) {
            self.push(m.clone());
            return Ok(Step::Next);
        }
        // A module body that is already running is either *this* task's — a
        // genuine cycle — or another task's, which is only something to wait
        // for. `importing` records the owner precisely so the two can be told
        // apart; a per-path flag reported the second task's ordinary import as
        // a circular one, and a per-*task* flag would have missed real cycles
        // that cross a module boundary.
        if let Some(&owner) = self.importing.get(path) {
            return Ok(self.await_import(path, owner));
        }

        // Embedded stdlib modules (written in Oro, baked into the binary)
        // resolve next; a same-named user file is never consulted, so a user
        // file can never shadow a stdlib name.
        //
        // `origin` is what a diagnostic raised inside the module will name.
        // For a user's file it is the very path this resolution just opened,
        // so it is a location the reader can act on; for an embedded module it
        // is a bracketed name, because there is no file (see
        // [`stdlib::display_name`]).
        let (source, origin) = if let Some(src) = stdlib::source_for(path) {
            (src.to_string(), stdlib::display_name(path))
        } else {
            // Resolve `a.b.c` to `<root>/a/b/c.oro`.
            let mut file = self.import_root.clone();
            for seg in path.split('.') {
                file.push(seg);
            }
            file.set_extension("oro");

            match std::fs::read_to_string(&file) {
                Ok(s) => (s, file.to_string_lossy().into_owned()),
                Err(_) => {
                    let class = self.excs["ModuleNotFoundError"].clone();
                    let msg = Value::str(if path == "subprocess" {
                        "No module named 'subprocess' — Oro's is called `proc`. It differs from \
                         CPython's on purpose: proc.run() streams the child's output live *and* \
                         captures it, and raises CommandError on a nonzero exit (pass check=False \
                         to allow one)."
                            .to_string()
                    } else {
                        format!("No module named '{path}'")
                    });
                    return Ok(Step::Raise(self.make_exception_instance(class, vec![msg])));
                }
            }
        };
        let code = match compile_source(&source, Rc::from(origin.as_str())) {
            Ok(c) => c,
            Err(e) => {
                let class = self.excs["ImportError"].clone();
                let msg = Value::str(format!("error importing '{path}': {e}"));
                return Ok(Step::Raise(self.make_exception_instance(class, vec![msg])));
            }
        };

        // Run the module body as a frame; BuildModule captures its namespace on
        // return and pushes the module value to the importer.
        self.importing.insert(path.to_string(), self.task.id);
        let mut frame = Frame {
            locals: vec![Value::Unbound; code.nlocals],
            cells: (0..code.ncells).map(|_| Rc::new(RefCell::new(Value::Unbound))).collect(),
            free: Vec::new(),
            stack: Vec::new(),
            pc: 0,
            code,
            ret_action: ReturnAction::BuildModule(Rc::from(path)),
            super_ctx: None,
            blocks: Vec::new(),
        };
        frame.pc = 0;
        self.task.frames.push(frame);
        Ok(Step::Next)
    }

    /// Whether the running frame is the body of a stdlib module shipped inside
    /// the binary.
    fn in_stdlib_module(&self) -> bool {
        match self.task.frames.last().map(|f| &f.ret_action) {
            Some(ReturnAction::BuildModule(p)) => stdlib::source_for(p).is_some(),
            _ => false,
        }
    }

    /// Build a module value from a finished module-body `frame`, cache it, and
    /// push it to the importer.
    fn finish_module(&mut self, path: Rc<str>, frame: &Frame) {
        let mut members = HashMap::new();
        for (name, target) in &frame.code.module_names {
            let value = match target {
                VarTarget::Local(s) => frame.locals[*s as usize].clone(),
                VarTarget::Cell(s) => frame.cells[*s as usize].borrow().clone(),
            };
            // Unbound names (declared but never assigned on this path) are skipped.
            if !matches!(value, Value::Unbound) {
                members.insert(name.clone(), value);
            }
        }
        let module = Value::Module(Rc::new(crate::value::Module {
            name: Rc::from(path.as_ref()),
            members: RefCell::new(members),
        }));
        self.module_cache.insert(path.to_string(), module.clone());
        // Release the path and hand the finished module to every task that
        // parked on it while this body was running.
        self.release_import(path.as_ref(), Ok(module.clone()));
        self.push(module);
    }

    // --- Exceptions ----------------------------------------------------------

    /// Turn a `raise EXPR` operand into the exception instance to propagate:
    /// a class is instantiated with no args; an existing exception instance is
    /// raised as-is; anything else is a TypeError.
    fn normalize_raise(&mut self, v: Value) -> Result<Value, VmError> {
        match v {
            // `raise E` and `raise E()` used to be the same, so `raise <name>`
            // meant re-raise or construct depending on what the name held at
            // run time. One spelling: the operand is always an instance.
            Value::Class(c) if c.is_exception => Err(self.err(type_error(format!(
                "`raise` needs an exception instance, not the class — write `raise {}()`",
                c.name
            )))),
            Value::Instance(ref i) if i.class.is_exception => Ok(v),
            other => Err(self.err(type_error(format!(
                "exceptions must derive from BaseException, not '{}'",
                other.type_label()
            )))),
        }
    }

    /// CPython's answer to a generator being advanced from two places at once,
    /// and now Oro's.
    ///
    /// It was never expressible before green threads *within* one execution —
    /// only by a generator that iterates itself — and it becomes ordinary with
    /// them: a generator is a first-class value, so it can travel down a
    /// channel to a task that starts driving it while the first task is still
    /// suspended inside it. Two tasks pushing frames into one `GenBox` would
    /// corrupt it; this raises instead.
    fn generator_busy(&self) -> Value {
        let class = self.excs["ValueError"].clone();
        self.make_exception_instance(class, vec![Value::str("generator already executing")])
    }

    /// Build an exception instance of `class`, storing its args tuple natively.
    fn make_exception_instance(&self, class: Rc<Class>, args: Vec<Value>) -> Value {
        let mut fields = Fields::new();
        fields.insert(Rc::from("args"), Value::Tuple(OroTuple::new(args)));
        Value::Instance(Rc::new(Instance { class, fields: RefCell::new(fields) }))
    }

    /// Whether `exc` is an instance of the exception class `class` (or a
    /// subclass) — the `except` matching test.
    fn exc_matches(&self, exc: &Value, class: &Value) -> Result<bool, VmError> {
        let cls = match class {
            Value::Class(c) if c.is_exception => c,
            other => {
                return Err(self.err(type_error(format!(
                    "catching classes that do not inherit from BaseException is not allowed \
                     (got '{}')",
                    other.type_label()
                ))))
            }
        };
        Ok(match exc {
            Value::Instance(i) => Class::is_subclass(&i.class, cls),
            _ => false,
        })
    }

    /// Convert an internal operation error into a typed exception instance, so
    /// runtime failures (index out of range, division by zero, …) are catchable
    /// with the same type CPython uses.
    /// Turn a `sys.exit` request into a `SystemExit` exception. It unwinds like
    /// any exception, so a user `except SystemExit` can still cancel the exit;
    /// only if uncaught does it set the exit code.
    ///
    /// The one class that cannot go through [`Vm::error_to_exception`], because
    /// its single argument is the exit code as an `int` and not a rendered
    /// message. `sys.exit` is the only thing in the crate that names it.
    fn exit_request(&mut self, e: &RuntimeError) -> Option<Value> {
        if e.class != Exc::SystemExit {
            return None;
        }
        let code: i32 = e.message.parse().unwrap_or(0);
        let class = self.excs[Exc::SystemExit.name()].clone();
        Some(self.make_exception_instance(class, vec![Value::Int(code as i64)]))
    }

    fn error_to_exception(&self, e: &RuntimeError) -> Value {
        let class = self.excs[e.class.name()].clone();
        // A KeyError's message is the missing key's repr, not a sentence, so
        // str(KeyError) matches CPython ("'z'").
        let msg = match e.class {
            Exc::KeyError => {
                e.message.strip_prefix("key error: ").unwrap_or(&e.message).to_string()
            }
            _ => e.message.to_string(),
        };
        let exc = self.make_exception_instance(class, vec![Value::str(msg)]);
        // Flag it: the argument above is a rendered message, not a constructor
        // argument, so `str()` must not apply `KeyError`'s repr rule to it a
        // second time. See [`crate::value::RENDERED_MESSAGE`].
        if let Value::Instance(i) = &exc {
            let key = Rc::from(crate::value::RENDERED_MESSAGE);
            i.fields.borrow_mut().insert(key, Value::Bool(true));
        }
        exc
    }

    /// Return `value` from the current frame, but first run any pending
    /// `finally` blocks in this frame (innermost first) so cleanup happens even
    /// on an early `return`.
    fn do_return(&mut self, value: Value) -> Result<Step, VmError> {
        // Run the innermost enclosing finally, if any, deferring the return.
        while let Some(b) = self.top().blocks.pop() {
            if let BlockKind::Finally = b.kind {
                let frame = self.top();
                frame.stack.truncate(b.stack_len);
                frame.pc = b.target;
                self.task.finally_why.push(Why::Return(value));
                return Ok(Step::Next);
            }
            // Except blocks are simply discarded on the way out.
        }

        let mut frame = self.task.frames.pop().expect("return with no frame");
        if self.task.frames.is_empty() {
            self.task.last_locals = frame.locals;
            return Ok(Step::Done(value));
        }
        // Take the action out so the whole `frame` stays usable (BuildModule
        // needs it to read the module namespace).
        let action = std::mem::replace(&mut frame.ret_action, ReturnAction::Normal);
        // BuildModule is the one action that reads the frame; it runs first,
        // then the frame retires like any other.
        if let ReturnAction::BuildModule(path) = action {
            self.finish_module(path, &frame);
            self.recycle(frame);
            return Ok(Step::Next);
        }
        self.recycle(frame);
        match action {
            ReturnAction::Normal => self.push(value),
            ReturnAction::DropForInit => {
                // __init__ must return None; the instance is already on the
                // caller's stack as the constructor result.
                if !matches!(value, Value::None) {
                    return Err(self.err(type_error("__init__() should return None")));
                }
            }
            ReturnAction::DrivePrint => {
                let s = match &value {
                    Value::Str(s) => s.s.clone(),
                    other => other.display(),
                };
                self.task.prints.last_mut().expect("print job").rendered.push(s);
                self.drive_print()?;
            }
            ReturnAction::DriveStr => {
                let s = match &value {
                    Value::Str(s) => s.s.clone(),
                    other => other.display(),
                };
                self.task.str_jobs.last_mut().expect("str job").results.push(s);
                self.drive_str()?;
            }
            ReturnAction::DriveSeq => {
                self.record_seq_result(value);
                self.drive_seq()?;
            }
            ReturnAction::NegateBool => self.push(Value::Bool(!value.truthy())),
            ReturnAction::DriveCmp => {
                let job = self.task.cmp_jobs.last_mut().expect("cmp job");
                match job.levels.pop() {
                    Some(CmpLevel::Dunder) => {}
                    _ => unreachable!("DriveCmp without a waiting dunder level"),
                }
                // Truthiness, not the value: this is a decision inside `in` or
                // a container comparison, where CPython applies `bool()` too.
                self.drive_cmp(CmpNext::Give(value.truthy()))?;
            }

            ReturnAction::FormatSpec(spec) => {
                let out =
                    self.wrap(crate::format::format_value(&value, crate::format::CONV_NONE, &spec))?;
                self.push(Value::str(out));
            }
            ReturnAction::BuildModule(_) => unreachable!("handled before the frame retired"),
        }
        Ok(Step::Next)
    }

    /// `break`: unwind blocks to the innermost loop, running each enclosing
    /// `finally` first (deferring the break), then leave the loop.
    fn do_break(&mut self) -> Step {
        while let Some(b) = self.top().blocks.pop() {
            match b.kind {
                BlockKind::Finally => {
                    let frame = self.top();
                    frame.stack.truncate(b.stack_len);
                    frame.pc = b.target;
                    self.task.finally_why.push(Why::Break);
                    return Step::Next;
                }
                BlockKind::Loop { .. } => {
                    // Restore the stack to loop entry (removing any iterator) and
                    // jump past the loop.
                    let frame = self.top();
                    frame.stack.truncate(b.stack_len);
                    frame.pc = b.target;
                    return Step::Next;
                }
                // An except block being exited is simply discarded.
                BlockKind::Except => {}
            }
        }
        unreachable!("break with no enclosing loop block")
    }

    /// `continue`: like `do_break`, but stop at the loop without leaving it and
    /// jump to its continue point (the loop block stays active).
    fn do_continue(&mut self) -> Step {
        loop {
            // Peek: a Loop block must remain on the stack.
            let kind_is_loop = matches!(
                self.top().blocks.last().map(|b| &b.kind),
                Some(BlockKind::Loop { .. })
            );
            if kind_is_loop {
                let b = self.top().blocks.last().unwrap();
                let cont = match b.kind {
                    BlockKind::Loop { cont } => cont,
                    _ => unreachable!(),
                };
                self.top().pc = cont;
                return Step::Next;
            }
            match self.top().blocks.pop() {
                Some(b) => match b.kind {
                    BlockKind::Finally => {
                        let frame = self.top();
                        frame.stack.truncate(b.stack_len);
                        frame.pc = b.target;
                        self.task.finally_why.push(Why::Continue);
                        return Step::Next;
                    }
                    BlockKind::Except => {}
                    BlockKind::Loop { .. } => unreachable!("handled above"),
                },
                None => unreachable!("continue with no enclosing loop block"),
            }
        }
    }

    /// A generator frame reached `return` (or fell off the end): mark it done
    /// and route its driver's `for` loop to the exhaustion target.
    fn generator_stop(&mut self) -> Result<Step, VmError> {
        if let Some(frame) = self.task.frames.pop() {
            self.recycle(frame);
        }
        let (gen, driver) = self.task.gen_stack.pop().expect("generator stop outside a driver");
        {
            let mut g = gen.borrow_mut();
            g.done = true;
            g.frame = None;
        }
        match driver {
            GenDriver::ForLoop(target) => {
                self.pop(); // discard the exhausted generator (the ForIter operand)
                self.top().pc = target;
            }
            GenDriver::Materialize => {
                // This argument is fully drained: swap the list in and move on to
                // the next generator argument, or retry the call.
                let job = self.task.mat_jobs.last_mut().expect("materialise job");
                let items = std::mem::take(&mut job.items);
                let idx = job.idx;
                job.args[idx] = Value::List(OroList::new(items));
                return self.drive_materialize();
            }
        }
        Ok(Step::Next)
    }

    fn job_depths(&self) -> JobDepths {
        JobDepths {
            prints: self.task.prints.len() as u32,
            str_jobs: self.task.str_jobs.len() as u32,
            seq_jobs: self.task.seq_jobs.len() as u32,
            mat_jobs: self.task.mat_jobs.len() as u32,
            cmp_jobs: self.task.cmp_jobs.len() as u32,
            ord_jobs: self.task.ord_jobs.len() as u32,
        }
    }

    /// Discard jobs started inside a block that an exception is unwinding out
    /// of. Their driver frames are gone, so nothing will ever complete them.
    fn truncate_jobs(&mut self, d: JobDepths) {
        self.task.prints.truncate(d.prints as usize);
        self.task.str_jobs.truncate(d.str_jobs as usize);
        self.task.seq_jobs.truncate(d.seq_jobs as usize);
        self.task.mat_jobs.truncate(d.mat_jobs as usize);
        self.task.cmp_jobs.truncate(d.cmp_jobs as usize);
        self.task.ord_jobs.truncate(d.ord_jobs as usize);
    }

    /// Unwind `exc` through the block and frame stacks. On success (a handler or
    /// finally took over) returns `None` and the loop resumes; if nothing
    /// catches it, returns the **exception value**, which is what ends this
    /// task.
    ///
    /// It returns the value rather than a rendered `RuntimeError` because §3
    /// rule 2 has to re-raise the very same exception object in whoever joins
    /// the task; the diagnostic string is derived from it afterwards, for the
    /// one case (rule 3) that prints instead.
    ///
    /// Alongside the value it returns **the file the raise came from**, read
    /// once here before the first frame is popped. `self.task.line`/`col` still
    /// name the raising instruction when this returns — unwinding executes no
    /// instructions — but the frame that gives those numbers a file is gone by
    /// then, so the file has to be taken on the way in. Taking it here also
    /// makes it agree with the line and the column in the one case where they
    /// move: a `finally` that re-raises reports its own `EndFinally`, and this
    /// is re-entered from that frame, so all three come from the same place.
    fn unwind(&mut self, exc: Value) -> Option<(Value, Rc<str>)> {
        let source = self.err_source();
        // A chain pending in the frame the exception came from will never reach
        // the step that was going to run it — a handler resumes at its own
        // target, not at the middle of the expression that raised.
        let depth = self.task.frames.len();
        self.chains.retain(|c| c.frame_depth < depth);
        loop {
            let block = self.task.frames.last_mut().and_then(|f| f.blocks.pop());
            match block {
                Some(b) => {
                    // Jobs started inside this block can never be completed now
                    // that the exception has blown past their drivers, so they
                    // are dropped along with the operand stack they belonged to.
                    self.truncate_jobs(b.jobs);
                    let frame = self.task.frames.last_mut().unwrap();
                    frame.stack.truncate(b.stack_len);
                    match b.kind {
                        BlockKind::Except => {
                            frame.pc = b.target;
                            self.task.handling.push(exc);
                            return None;
                        }
                        BlockKind::Finally => {
                            frame.pc = b.target;
                            // Run the finally body; EndFinally re-raises after.
                            self.task.finally_why.push(Why::Raise(exc));
                            return None;
                        }
                        // A loop being unwound by an exception is just abandoned.
                        BlockKind::Loop { .. } => {}
                    }
                }
                None => {
                    // No handler in this frame: discard it and try the caller.
                    // A chain is one expression, so a pending one belongs to the
                    // frame being discarded and can never be flushed now.
                    let depth = self.task.frames.len();
                    self.chains.retain(|c| c.frame_depth < depth);
                    if let Some(frame) = self.task.frames.pop() {
                        // A module body dying has to release its import, or the
                        // path stays in `importing` forever: a *retried* import
                        // then reports `circular import detected` instead of the
                        // real error, and any task parked on the rendezvous
                        // waits for a body that will never finish.
                        if matches!(frame.ret_action, ReturnAction::BuildModule(_)) {
                            self.abort_module_frame(&frame, &exc);
                        }
                        self.recycle(frame);
                    }
                    if self.task.frames.is_empty() {
                        // An uncaught SystemExit sets the process exit code.
                        if let Value::Instance(i) = &exc {
                            if i.class.name.as_ref() == "SystemExit" {
                                let code = match crate::value::exception_args(i).first() {
                                    Some(Value::Int(n)) => *n as i32,
                                    _ => 0,
                                };
                                self.exit_code = Some(code);
                            }
                        }
                        return Some((exc, source));
                    }
                }
            }
        }
    }

    /// Format an uncaught exception as `TypeName: message` at the current line.
    ///
    /// `source` is the file that was running when the exception was raised,
    /// taken by [`Vm::unwind`] before it popped the frame it came from — by the
    /// time an exception is known to be uncaught, its frame is gone.
    fn uncaught_error(&self, exc: &Value, source: Rc<str>) -> VmError {
        let (name, msg) = match exc {
            Value::Instance(i) => {
                (i.class.name.to_string(), crate::value::exception_message(i))
            }
            other => ("Exception".to_string(), other.display()),
        };
        let message = if msg.is_empty() { name } else { format!("{name}: {msg}") };
        Box::new(RuntimeError {
            // The rendering is `Class: message` already; nothing re-raises a
            // diagnostic built here, so the class field only has to be honest.
            class: Exc::RuntimeError,
            message: message.into_boxed_str(),
            source,
            line: self.task.line,
            col: self.task.col,
        })
    }

    /// Bind arguments to a fresh frame's slots and cells.
    ///
    /// Two paths, as the spec calls out: the **static** path fills positional
    /// parameters straight into their numbered slots; the **dynamic** path is
    /// taken when keyword arguments are present, where the argument names are
    /// only known at runtime and must be matched against the parameter list.
    ///
    /// `receiver`, when present, is the method call's `self`: it binds to the
    /// first parameter ahead of `args`. It is passed separately rather than
    /// prepended to `args` by the caller because prepending means allocating a
    /// second argument vector and re-copying every argument into it, on every
    /// method call.
    fn bind_call(
        &mut self,
        func: &Rc<Function>,
        receiver: Option<Value>,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<Frame, VmError> {
        let mut frame = self.take_frame(func.code.clone(), &func.freevars);
        let code = &func.code;

        // --- The static path ---------------------------------------------
        //
        // No keywords, and exactly one positional argument for each parameter
        // that has no default. Under the argument rule that is not merely the
        // common shape, it is the *only* shape a positional-only call can have:
        // a parameter with a default can only be passed by name, so one more
        // argument is an error rather than a longer call. The test is therefore
        // a single equality — cheaper than the pair of comparisons it replaces
        // — and it needs no name matching and no allocation.
        let supplied = args.len() + usize::from(receiver.is_some());
        let first_defaulted = code.params.len() - code.defaults.len();
        if kwargs.is_empty() && supplied == first_defaulted {
            let mut values = receiver.into_iter().chain(args);
            for p in &code.params[..supplied] {
                let v = values.next().expect("supplied counts the values exactly");
                store_param(&mut frame, p.target, v);
            }
            for (i, p) in code.params.iter().enumerate().skip(supplied) {
                let v = code.defaults[i - first_defaulted].clone();
                store_param(&mut frame, p.target, v);
            }
            return Ok(frame);
        }

        // --- The dynamic path ---------------------------------------------
        //
        // This is where the argument rule is enforced. A parameter with no
        // default is positional-only and a parameter with a default is
        // keyword-only, so each of the two kinds of argument has exactly one
        // half of the parameter list it may reach, and a call that crosses the
        // line is refused with its own call rewritten.
        let skip = usize::from(receiver.is_some());
        let mut args = args;
        if let Some(receiver) = receiver {
            // Only now is the combined vector worth building: this path has to
            // match names against it anyway.
            args.insert(0, receiver);
        }

        let params = &code.params;
        let first_defaulted = params.len() - code.defaults.len();

        // The checks below read the keyword list rather than a table of filled
        // slots, and the values go straight into the frame. A keyword list is
        // one or two entries on a real call, so scanning it twice is cheaper
        // than the `Vec<Option<Value>>` this path used to allocate on top of
        // everything else a keyword call already allocates.
        //
        // Validation comes first and borrows: while `args` and `kwargs` are
        // both still intact, a refusal can rewrite the reader's whole call.
        // Binding comes second and consumes, so no argument is copied.

        // 1. Positional arguments fill the parameters with no default, left to
        //    right. They stop there: the rest are keyword-only.
        if args.len() > first_defaulted {
            let writable = first_defaulted - skip;
            let takes = match writable {
                0 => "no positional arguments".to_string(),
                1 => "1 positional argument".to_string(),
                n => format!("{n} positional arguments"),
            };
            let given = args.len() - skip;
            let verb = if given == 1 { "was" } else { "were" };
            let fix = write_it_as(code, skip, &args, &kwargs);
            // Only say why when there is a keyword-only parameter to say it
            // about. A function with no defaults that was handed too many
            // arguments has a plain arity error, and always did.
            let because = if first_defaulted < params.len() {
                ": a parameter with a default is passed by name"
            } else {
                ""
            };
            return Err(self.err(type_error(format!(
                "{}() takes {takes} but {given} {verb} given{fix}{because}",
                code.name
            ))));
        }
        // 2. Keyword arguments name the parameters that have defaults.
        for (j, (name, value)) in kwargs.iter().enumerate() {
            let Some(pos) = params.iter().position(|p| &*p.name == name) else {
                return Err(self.err(type_error(format!(
                    "{}() got an unexpected keyword argument '{name}'",
                    code.name
                ))));
            };
            if pos < first_defaulted {
                // A parameter with no default. Passing it by name is the other
                // half of the rule, and it is refused for the same reason: one
                // spelling per argument, decided by the `=` in the signature.
                let fix = write_it_as(code, skip, &args, &kwargs);
                return Err(self.err(type_error(format!(
                    "{}() got '{name}' by name, but '{name}' has no default and is passed by \
                     position{fix}",
                    code.name
                ))));
            }
            if kwargs[..j].iter().any(|(earlier, _)| earlier == name) {
                return Err(self.err(type_error(format!(
                    "{}() got multiple values for argument '{name}'",
                    code.name
                ))));
            }
            // An explicit `null` is not a way of saying "omitted". Where the
            // default *is* `null`, `null` is a real value and passing it is
            // fine; anywhere else it would make `f(x=null)` a second spelling
            // of `f()`.
            let default = &code.defaults[pos - first_defaulted];
            if matches!(value, Value::None) && !matches!(default, Value::None) {
                let want = match default {
                    // A non-constant default is not a value until the call
                    // asks for it, so there is no type to name.
                    Value::Unbound => "a value",
                    other => other.type_name(),
                };
                return Err(
                    self.err(crate::builtins::null_is_not_omitted(&code.name, name, want))
                );
            }
        }

        // 3. Every parameter without a default must have been supplied, and
        //    only a positional argument can supply one — so the first one the
        //    arguments did not reach is the one that is missing.
        if args.len() < first_defaulted {
            return Err(self.err(type_error(format!(
                "{}() missing required argument: '{}'",
                code.name, params[args.len()].name
            ))));
        }

        // 4. Bind, consuming. The positional arguments now cover exactly the
        //    parameters with no default; every other parameter takes its
        //    default, and a keyword then writes over the ones it named.
        for (p, value) in params.iter().zip(args) {
            store_param(&mut frame, p.target, value);
        }
        for (i, p) in params.iter().enumerate().skip(first_defaulted) {
            store_param(&mut frame, p.target, code.defaults[i - first_defaulted].clone());
        }
        for (name, value) in kwargs {
            let pos = params
                .iter()
                .position(|p| *p.name == name)
                .expect("every keyword was matched to a parameter above");
            store_param(&mut frame, params[pos].target, value);
        }

        Ok(frame)
    }
}

/// Take a suspended generator's frame out of its box, leaving the box.
///
/// The box is the point. A `GenBox` used to hold `Box<Frame>`, so every
/// `yield` allocated one and every resume freed it — a malloc/free pair per
/// element, on the one path a generator pipeline is made of. It holds
/// `Box<Option<Frame>>` instead, allocated once when the generator is created
/// and written through for the rest of its life.
///
/// `None` here means the generator is *running* — its frame is on some task's
/// frame stack right now — which is exactly what an empty box meant before.
fn take_gen_frame(g: &mut crate::value::GenBox) -> Option<Frame> {
    g.frame.as_mut()?.downcast_mut::<Option<Frame>>().expect("gen frame").take()
}

/// Suspend `frame` back into `g`'s box, reusing it if it is still there.
fn put_gen_frame(g: &mut crate::value::GenBox, frame: Frame) {
    match g.frame.as_mut() {
        Some(b) => *b.downcast_mut::<Option<Frame>>().expect("gen frame") = Some(frame),
        None => g.frame = Some(Box::new(Some(frame))),
    }
}

/// The call the reader wrote, rewritten into the rule's shape: every parameter
/// with no default as a positional argument, every one with a default as
/// `name=value`.
///
/// `skip` is the number of leading parameters the call site does not write —
/// 1 for a method, whose receiver is bound from the `obj.` and is not an
/// argument. `None` when the arguments given do not cover the parameters with
/// no default, because then there is no correct call to show and a bare
/// diagnostic is the honest one.
///
/// This is [`crate::builtins::respell`], the same rewriting the natives do, so
/// a refusal from an Oro `def` reads exactly like a refusal from `find` or
/// `split`.
fn respelled_call(
    code: &CodeObject,
    skip: usize,
    args: &[Value],
    kwargs: &[(String, Value)],
) -> Option<String> {
    let required = code.params.len() - code.defaults.len();
    let mut slots: Vec<Option<&Value>> = vec![None; code.params.len()];
    for (i, a) in args.iter().enumerate() {
        *slots.get_mut(i)? = Some(a);
    }
    for (name, v) in kwargs {
        let pos = code.params.iter().position(|p| *p.name == *name)?;
        if slots[pos].is_some() {
            return None;
        }
        slots[pos] = Some(v);
    }
    if slots[skip..required].iter().any(Option::is_none) {
        return None;
    }
    let fixed: Vec<&Value> = slots[skip..required].iter().map(|s| s.expect("checked")).collect();
    let named: Vec<(&str, &Value)> = code.params[required..]
        .iter()
        .zip(&slots[required..])
        .filter_map(|(p, s)| s.map(|v| (&*p.name, v)))
        .collect();
    Some(crate::builtins::respell(&code.name, &fixed, &named))
}

/// " — write `f(1, c=2)`", or nothing when the call cannot be rewritten.
fn write_it_as(code: &CodeObject, skip: usize, args: &[Value], kwargs: &[(String, Value)]) -> String {
    match respelled_call(code, skip, args, kwargs) {
        Some(call) => format!(" — write {call}"),
        None => String::new(),
    }
}

fn store_param(frame: &mut Frame, target: VarTarget, value: Value) {
    match target {
        VarTarget::Local(s) => frame.locals[s as usize] = value,
        VarTarget::Cell(s) => *frame.cells[s as usize].borrow_mut() = value,
    }
}

// --- Iteration --------------------------------------------------------------

fn get_iter(v: &Value) -> VResult<Value> {
    let state = match v {
        Value::Range(r) => IterState::Range { cur: r.start, stop: r.stop, step: r.step, n: 0 },
        Value::List(l) => {
            IterState::List { list: l.clone(), idx: 0, orig_len: l.borrow().len() }
        }
        Value::Tuple(t) => IterState::Tuple { tuple: t.clone(), idx: 0 },
        Value::Str(s) => {
            let chars = s.s.chars().map(|c| c.to_string()).collect();
            IterState::Str { chars, idx: 0 }
        }
        Value::Bytes(b) => IterState::Bytes { bytes: b.clone(), idx: 0 },
        // Iterating a dict yields `(key, value)`, not the key — the pair shape
        // the rest of the collection protocol already uses for a dict. `.keys()`
        // and `.values()` are how you ask for one half.
        Value::Dict(d) => IterState::DictPairs { items: d.borrow().items().to_vec(), idx: 0 },
        // A generator is its own iterator; ForIter resumes it directly.
        Value::Generator(_) => return Ok(v.clone()),
        // So is a channel: `ForIter` recvs from it (and may park).
        Value::Channel(_) => return Ok(v.clone()),
        Value::Iter(_) => return Ok(v.clone()),
        other => return Err(type_error(format!("'{}' object is not iterable", other.type_name()))),
    };
    Ok(Value::Iter(Rc::new(RefCell::new(state))))
}

fn iter_next(it: &Value) -> VResult<Option<Value>> {
    let it = match it {
        Value::Iter(i) => i,
        _ => return Err(runtime_error("internal: ForIter target is not an iterator")),
    };
    let mut st = it.borrow_mut();
    match &mut *st {
        IterState::Range { cur, stop, step, .. } => {
            let go = if *step > 0 { *cur < *stop } else { *cur > *stop };
            if go {
                let v = *cur;
                *cur += *step;
                Ok(Some(Value::Int(v)))
            } else {
                Ok(None)
            }
        }
        IterState::List { list, idx, orig_len } => {
            let cur_len = list.borrow().len();
            if cur_len != *orig_len {
                return Err(runtime_error("list changed size during iteration"));
            }
            if *idx < cur_len {
                let v = list.borrow()[*idx].clone();
                *idx += 1;
                Ok(Some(v))
            } else {
                Ok(None)
            }
        }
        IterState::Tuple { tuple, idx } => {
            if *idx < tuple.len() {
                let v = tuple[*idx].clone();
                *idx += 1;
                Ok(Some(v))
            } else {
                Ok(None)
            }
        }
        IterState::Str { chars, idx } => {
            if *idx < chars.len() {
                let v = Value::str(chars[*idx].clone());
                *idx += 1;
                Ok(Some(v))
            } else {
                Ok(None)
            }
        }
        IterState::Bytes { bytes, idx } => {
            if *idx < bytes.len() {
                let v = bytes[*idx];
                *idx += 1;
                Ok(Some(Value::Int(v as i64)))
            } else {
                Ok(None)
            }
        }
        IterState::DictPairs { items, idx } => {
            if *idx < items.len() {
                let (k, v) = items[*idx].clone();
                *idx += 1;
                Ok(Some(Value::Tuple(OroTuple::new(vec![k, v]))))
            } else {
                Ok(None)
            }
        }
    }
}

/// The `for`-loop step: every `for` yields an `(index, value)` pair. The index
/// is the element's *position* for an ordered sequence (list, tuple, str, bytes,
/// range), and the *key* for a dict — the same pair the collection protocol
/// hands a dict callback. Generators and channels are self-driven and paired at
/// their delivery sites (with a 0-based counter), so they never reach here.
fn iter_next_pair(it: &Value) -> VResult<Option<(Value, Value)>> {
    let it = match it {
        Value::Iter(i) => i,
        _ => return Err(runtime_error("internal: ForIter target is not an iterator")),
    };
    let mut st = it.borrow_mut();
    match &mut *st {
        IterState::Range { cur, stop, step, n } => {
            let go = if *step > 0 { *cur < *stop } else { *cur > *stop };
            if go {
                let v = *cur;
                let idx = *n;
                *cur += *step;
                *n += 1;
                Ok(Some((Value::Int(idx), Value::Int(v))))
            } else {
                Ok(None)
            }
        }
        IterState::List { list, idx, orig_len } => {
            let cur_len = list.borrow().len();
            if cur_len != *orig_len {
                return Err(runtime_error("list changed size during iteration"));
            }
            if *idx < cur_len {
                let v = list.borrow()[*idx].clone();
                let i = *idx as i64;
                *idx += 1;
                Ok(Some((Value::Int(i), v)))
            } else {
                Ok(None)
            }
        }
        IterState::Tuple { tuple, idx } => {
            if *idx < tuple.len() {
                let v = tuple[*idx].clone();
                let i = *idx as i64;
                *idx += 1;
                Ok(Some((Value::Int(i), v)))
            } else {
                Ok(None)
            }
        }
        IterState::Str { chars, idx } => {
            if *idx < chars.len() {
                let v = Value::str(chars[*idx].clone());
                let i = *idx as i64;
                *idx += 1;
                Ok(Some((Value::Int(i), v)))
            } else {
                Ok(None)
            }
        }
        IterState::Bytes { bytes, idx } => {
            if *idx < bytes.len() {
                let v = Value::Int(bytes[*idx] as i64);
                let i = *idx as i64;
                *idx += 1;
                Ok(Some((Value::Int(i), v)))
            } else {
                Ok(None)
            }
        }
        IterState::DictPairs { items, idx } => {
            if *idx < items.len() {
                let (k, v) = items[*idx].clone();
                *idx += 1;
                Ok(Some((k, v)))
            } else {
                Ok(None)
            }
        }
    }
}

/// Collect every element of an iterable into a vector (for unpacking, `*args`
/// spreading, and `**` merging).
pub fn iterate_to_vec(v: &Value) -> VResult<Vec<Value>> {
    // Draining a channel means blocking, and blocking means parking, which a
    // native helper cannot do — the same rule that stops a builtin from
    // draining a generator. `for msg in ch` is the way.
    if matches!(v, Value::Channel(_)) {
        return Err(type_error(
            "'Channel' object is not iterable here: receiving may block, so `for msg in \
             ch` is the only way to drain one",
        ));
    }
    let it = get_iter(v)?;
    let mut out = Vec::new();
    while let Some(x) = iter_next(&it)? {
        out.push(x);
    }
    Ok(out)
}

// --- Indexing and slicing ---------------------------------------------------

fn as_index(v: &Value) -> VResult<i64> {
    match v {
        Value::Bool(b) => Ok(*b as i64),
        Value::Int(i) => Ok(*i),
        other => Err(type_error(format!(
            "indices must be integers, not '{}'",
            other.type_name()
        ))),
    }
}

/// Resolve a possibly-negative index against `len`, returning the non-negative
/// position or an out-of-range error.
fn resolve_index(idx: i64, len: usize, kind: &str) -> VResult<usize> {
    let adj = if idx < 0 { idx + len as i64 } else { idx };
    if adj < 0 || adj as usize >= len {
        Err(index_error(format!("{kind} index out of range")))
    } else {
        Ok(adj as usize)
    }
}

fn subscript_get(obj: &Value, index: &Value) -> VResult<Value> {
    match obj {
        Value::List(l) => {
            let l = l.borrow();
            let i = resolve_index(as_index(index)?, l.len(), "list")?;
            Ok(l[i].clone())
        }
        Value::Tuple(t) => {
            let i = resolve_index(as_index(index)?, t.len(), "tuple")?;
            Ok(t[i].clone())
        }
        Value::Str(s) => {
            let n = s.char_len();
            let i = resolve_index(as_index(index)?, n, "string")?;
            Ok(Value::str(s.char_at(i).expect("index checked in range")))
        }
        // `bytes` is a sequence of numbers, so one element is a number. The
        // asymmetry with `str` (where `s[i]` is a one-character `str`, since
        // Oro has no character type) is the two types telling the truth about
        // what they contain.
        Value::Bytes(b) => {
            let i = resolve_index(as_index(index)?, b.len(), "bytes")?;
            Ok(Value::Int(b[i] as i64))
        }
        Value::Dict(d) => match d.borrow().get(index)? {
            Some(v) => Ok(v),
            None => Err(key_error(format!("key error: {}", index.repr()))),
        },
        other => Err(type_error(format!("'{}' object is not subscriptable", other.type_name()))),
    }
}

fn subscript_set(obj: &Value, index: &Value, value: Value) -> VResult<()> {
    match obj {
        Value::List(l) => {
            let mut l = l.borrow_mut();
            let len = l.len();
            let i = resolve_index(as_index(index)?, len, "list")?;
            l[i] = value;
            Ok(())
        }
        Value::Dict(d) => d.borrow_mut().insert(index.clone(), value),
        other => {
            Err(type_error(format!(
                "'{}' object does not support item assignment",
                other.type_name()
            )))
        }
    }
}

fn slice_get(
    obj: &Value,
    lower: &Value,
    upper: &Value,
    step: &Value,
) -> VResult<Value> {
    let opt = |v: &Value| -> VResult<Option<i64>> {
        match v {
            Value::None => Ok(None),
            other => Ok(Some(as_index(other)?)),
        }
    };
    let (lo, hi, st) = (opt(lower)?, opt(upper)?, opt(step)?);
    let step = st.unwrap_or(1);
    if step == 0 {
        return Err(value_error("slice step cannot be zero"));
    }
    match obj {
        Value::Str(s) => {
            // `s[i:j]` with the default step is by far the common case, and it
            // is the one a byte-at-a-time parser runs once per token. Taking it
            // through the general path costs a `Vec<char>` of the *whole*
            // string plus a `Vec<usize>` of the slice, which makes an
            // O(slice) operation O(string) — the quadratic that
            // `std/json.oro` was paying on every number literal. The forward
            // unit-step case is a byte-range copy instead, O(slice) for ASCII
            // and O(end) for a string that is not.
            if step == 1 {
                let (start, stop) = unit_range(s.char_len(), lo, hi);
                return Ok(Value::str(s.byte_slice(start, stop).to_string()));
            }
            let chars: Vec<char> = s.s.chars().collect();
            let idxs = slice_indices(chars.len(), lo, hi, step);
            let out: String = idxs.into_iter().map(|i| chars[i]).collect();
            Ok(Value::str(out))
        }
        Value::Bytes(b) => {
            if step == 1 {
                let (start, stop) = unit_range(b.len(), lo, hi);
                return Ok(Value::bytes(b[start..stop].to_vec()));
            }
            let idxs = slice_indices(b.len(), lo, hi, step);
            Ok(Value::bytes(idxs.into_iter().map(|i| b[i]).collect::<Vec<u8>>()))
        }
        Value::List(l) => {
            let l = l.borrow();
            if step == 1 {
                let (start, stop) = unit_range(l.len(), lo, hi);
                return Ok(Value::List(OroList::new(l[start..stop].to_vec())));
            }
            let idxs = slice_indices(l.len(), lo, hi, step);
            Ok(Value::List(OroList::new(idxs.into_iter().map(|i| l[i].clone()).collect())))
        }
        Value::Tuple(t) => {
            if step == 1 {
                let (start, stop) = unit_range(t.len(), lo, hi);
                return Ok(Value::Tuple(OroTuple::new(t[start..stop].to_vec())));
            }
            let idxs = slice_indices(t.len(), lo, hi, step);
            Ok(Value::Tuple(OroTuple::new(idxs.into_iter().map(|i| t[i].clone()).collect())))
        }
        other => Err(type_error(format!("'{}' object is not sliceable", other.type_name()))),
    }
}

/// The `[start, stop)` a `step == 1` slice selects, with Python's clamping and
/// negative-index rules. `stop` is never below `start`, so the caller can index
/// with the range directly. This is [`slice_indices`] for the unit-step case
/// without materialising one index per selected element.
fn unit_range(len: usize, lower: Option<i64>, upper: Option<i64>) -> (usize, usize) {
    let n = len as i64;
    let resolve = |i: i64| (if i < 0 { i + n } else { i }).clamp(0, n) as usize;
    let start = lower.map_or(0, resolve);
    let stop = upper.map_or(len, resolve);
    (start, stop.max(start))
}

/// Compute the concrete indices a slice selects, applying Python's clamping and
/// negative-index rules for either direction of `step`.
fn slice_indices(len: usize, lower: Option<i64>, upper: Option<i64>, step: i64) -> Vec<usize> {
    let len = len as i64;
    let clamp = |i: i64, lo: i64, hi: i64| i.max(lo).min(hi);
    let (mut start, stop);
    if step > 0 {
        start = match lower {
            Some(l) => clamp(if l < 0 { l + len } else { l }, 0, len),
            None => 0,
        };
        stop = match upper {
            Some(u) => clamp(if u < 0 { u + len } else { u }, 0, len),
            None => len,
        };
        let mut out = Vec::new();
        while start < stop {
            out.push(start as usize);
            start += step;
        }
        out
    } else {
        start = match lower {
            Some(l) => clamp(if l < 0 { l + len } else { l }, -1, len - 1),
            None => len - 1,
        };
        stop = match upper {
            Some(u) => clamp(if u < 0 { u + len } else { u }, -1, len - 1),
            None => -1,
        };
        let mut out = Vec::new();
        while start > stop {
            out.push(start as usize);
            start += step;
        }
        out
    }
}

// --- Attributes -------------------------------------------------------------

/// What `LoadMethod` found, and how `CallMethod` should call it. See
/// [`Op::LoadMethod`].
enum MethodRef {
    /// An Oro method: the receiver, the function, and the class it is defined
    /// in (which fixes `super()`'s search origin).
    User { recv: Value, func: Rc<Function>, defclass: Rc<Class> },
    /// A native method on this receiver; the name is the instruction's.
    Native(Value),
    /// Not a method: a value to call with the arguments alone.
    Plain(Value),
}

/// [`get_attr`] for an attribute that is about to be called, answering with
/// the *parts* of a bound method rather than a bound method.
///
/// It must agree with `get_attr` case for case — the same value found in the
/// same order, and character-for-character the same message when there is
/// none — because whether a program takes this path or that one is decided by
/// the shape of the source, not by anything the program can observe. The two
/// arms that are not methods (`Value::Class`, `Value::Module`) simply defer to
/// `get_attr` rather than restate it.
fn resolve_method(obj: &Value, name: &Rc<str>) -> VResult<MethodRef> {
    let key: &str = name;
    match obj {
        Value::Instance(inst) => {
            if let Some(v) = inst.fields.borrow().get(key) {
                return Ok(MethodRef::Plain(v.clone()));
            }
            match Class::find(&inst.class, key) {
                Some((Value::Func(func), defclass)) => {
                    Ok(MethodRef::User { recv: obj.clone(), func, defclass })
                }
                Some((member, _)) => Ok(MethodRef::Plain(member)),
                None if crate::builtins::is_cast_method(key) => Ok(MethodRef::Native(obj.clone())),
                None => Err(attribute_error(format!(
                    "'{}' object has no attribute '{}'",
                    inst.class.name,
                    key
                ))),
            }
        }
        Value::Class(_) | Value::Module(_) => get_attr(obj, name).map(MethodRef::Plain),
        Value::Super(sp) => {
            let mut cur = sp.start.clone();
            while let Some(c) = cur {
                let found = c.members.borrow().get(key).cloned();
                if let Some(member) = found {
                    return Ok(match member {
                        Value::Func(func) => {
                            MethodRef::User { recv: sp.instance.clone(), func, defclass: c }
                        }
                        other => MethodRef::Plain(other),
                    });
                }
                cur = c.base.clone();
            }
            Err(attribute_error(format!("'super' object has no attribute '{key}'")))
        }
        Value::Stream(s) if s.has_addr_attr(key) => {
            Ok(MethodRef::Plain(Value::str(s.addr_attr(key)?)))
        }
        Value::Task(_) if key == "join" => Ok(MethodRef::Native(obj.clone())),
        Value::Channel(_) if matches!(key, "send" | "recv" | "close") => {
            Ok(MethodRef::Native(obj.clone()))
        }
        // A builtin type has no members, and says so the way a user class
        // does — `str.upper()` and `Square.nope` are the same mistake.
        Value::Type(t) => {
            Err(attribute_error(format!("type object '{}' has no attribute '{}'", t.name(), key)))
        }
        _ => {
            if crate::builtins::method_exists(obj, key) {
                Ok(MethodRef::Native(obj.clone()))
            } else if let Some(msg) = crate::builtins::cut_method_message(obj, key) {
                // A removed method: the message names the replacement, but it
                // is still an attribute that is not there.
                Err(attribute_error(msg))
            } else {
                Err(attribute_error(format!(
                    "'{}' object has no attribute '{}'",
                    obj.type_name(),
                    key
                )))
            }
        }
    }
}

/// Attribute read for any value. Instances, classes, and `super` proxies are
/// handled here (no `__getattr__` hook exists, so this never runs Oro code);
/// everything else falls back to builtin-method binding.
fn get_attr(obj: &Value, name: &Rc<str>) -> VResult<Value> {
    // `name` arrives as the code object's *interned* `Rc<str>` rather than as
    // a `&str` for one reason: every native method access — `xs.append`,
    // `s.split`, `ys.map` — used to rebuild that string with `Rc::from`, a
    // heap allocation and a copy per access, to store a name the code object
    // already owned. Taking the `Rc` makes each of those a refcount bump.
    // `key` is the same string as a `&str`, for the lookups.
    let key: &str = name;
    match obj {
        Value::Instance(inst) => {
            if let Some(v) = inst.fields.borrow().get(key) {
                return Ok(v.clone());
            }
            match Class::find(&inst.class, key) {
                Some((member, defclass)) => Ok(bind_member(member, obj.clone(), defclass)),
                // The conversion methods exist on every value, instances
                // included — `to_str` runs the class's `__str__` if it has one.
                None if crate::builtins::is_cast_method(key) => Ok(Value::Method(Rc::new(
                    BoundMethod { receiver: obj.clone(), kind: MethodKind::Native(name.clone()) },
                ))),
                None => Err(attribute_error(format!(
                    "'{}' object has no attribute '{}'",
                    inst.class.name,
                    key
                ))),
            }
        }
        Value::Class(class) => match Class::find(class, key) {
            // A method accessed on the class itself stays an unbound function.
            Some((member, _)) => Ok(member),
            None => Err(attribute_error(format!(
                "type object '{}' has no attribute '{}'",
                class.name,
                key
            ))),
        },
        // A builtin type has no members at all, so every attribute on one is
        // this — and it is the class message, not the generic one, because a
        // builtin type is the same kind of thing a user class is.
        Value::Type(t) => {
            Err(attribute_error(format!("type object '{}' has no attribute '{}'", t.name(), key)))
        }
        Value::Module(m) => match m.members.borrow().get(key) {
            Some(v) => Ok(v.clone()),
            None => Err(attribute_error(format!("module '{}' has no attribute '{}'", m.name, key))),
        },
        Value::Super(sp) => {
            let mut cur = sp.start.clone();
            while let Some(c) = cur {
                if let Some(member) = c.members.borrow().get(key).cloned() {
                    return Ok(bind_member(member, sp.instance.clone(), c.clone()));
                }
                cur = c.base.clone();
            }
            Err(attribute_error(format!("'super' object has no attribute '{key}'")))
        }
        // A socket's `peer` and `local` are data attributes, not methods
        // (§4): they are strings read once when the socket was opened.
        Value::Stream(s) if s.has_addr_attr(key) => Ok(Value::str(s.addr_attr(key)?)),
        // The concurrency surface is exactly four methods (§3). They bind here
        // rather than through `builtins::method_exists` because the VM, not
        // `call_method`, has to run them: each one may park.
        Value::Task(_) if key == "join" => Ok(native_method(obj, name)),
        Value::Channel(_) if matches!(key, "send" | "recv" | "close") => {
            Ok(native_method(obj, name))
        }
        _ => {
            if crate::builtins::method_exists(obj, key) {
                Ok(Value::Method(Rc::new(BoundMethod {
                    receiver: obj.clone(),
                    kind: MethodKind::Native(name.clone()),
                })))
            } else if let Some(msg) = crate::builtins::cut_method_message(obj, key) {
                // A removed method: the message names the replacement, but it
                // is still an attribute that is not there.
                Err(attribute_error(msg))
            } else {
                Err(attribute_error(format!(
                    "'{}' object has no attribute '{}'",
                    obj.type_name(),
                    key
                )))
            }
        }
    }
}

/// A bound native method — one the VM or `builtins::call_method` will run.
fn native_method(receiver: &Value, name: &Rc<str>) -> Value {
    Value::Method(Rc::new(BoundMethod {
        receiver: receiver.clone(),
        kind: MethodKind::Native(name.clone()),
    }))
}

/// Bind a looked-up class member to a receiver: a function becomes a bound
/// method; any other value (a class-level attribute) is returned unchanged.
fn bind_member(member: Value, receiver: Value, defclass: Rc<Class>) -> Value {
    match member {
        Value::Func(f) => Value::Method(Rc::new(BoundMethod {
            receiver,
            kind: MethodKind::User { func: f, defclass },
        })),
        other => other,
    }
}

/// A subprocess run failure: an I/O error (e.g. program not found) or a timeout.
enum RunError {
    Io(std::io::Error),
    Timeout,
}

/// How much of a child's output `proc.run` keeps. Capture is no longer optional
/// (it comes with streaming), so an unbounded buffer would turn a chatty child
/// into an out-of-memory kill. Past this the stream still flows to the terminal
/// in full and only the retained copy stops growing, with `.truncated` on the
/// result saying so.
const MAX_CAPTURE_BYTES: usize = 64 * 1024 * 1024;

/// Read a child pipe to EOF, optionally teeing every chunk onward to one of our
/// own streams as it arrives, and return everything that was read.
///
/// Teeing while accumulating is what lets `proc.run` be live *and* capturing at
/// once. Both pipes are drained on their own threads, which is also what keeps a
/// child that fills one of them from deadlocking against a parent reading the
/// other.
fn drain_pipe<R, W>(mut pipe: Option<R>, mut sink: Option<W>) -> std::thread::JoinHandle<Vec<u8>>
where
    R: std::io::Read + Send + 'static,
    W: std::io::Write + Send + 'static,
{
    std::thread::spawn(move || {
        let mut collected = Vec::new();
        let Some(p) = pipe.as_mut() else { return collected };
        let mut buf = [0u8; 8192];
        loop {
            match p.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Some(w) = sink.as_mut() {
                        // A failed passthrough (closed terminal, EPIPE) must not
                        // lose the capture, so the error is deliberately dropped.
                        let _ = w.write_all(&buf[..n]);
                        let _ = w.flush();
                    }
                    // Keep draining after the cap — the pipe must not fill, or
                    // the child blocks forever — but stop retaining.
                    if collected.len() < MAX_CAPTURE_BYTES {
                        let room = MAX_CAPTURE_BYTES - collected.len();
                        collected.extend_from_slice(&buf[..n.min(room)]);
                    }
                }
            }
        }
        collected
    })
}

/// Run a command to completion, optionally with a timeout. `stream` tees the
/// child's stdout/stderr onto ours as they arrive; the output is captured either
/// way.
fn run_process(
    mut cmd: std::process::Command,
    timeout: Option<f64>,
    stream: bool,
) -> Result<std::process::Output, RunError> {
    let mut child = cmd.spawn().map_err(RunError::Io)?;
    let out_handle = drain_pipe(
        child.stdout.take(),
        if stream { Some(std::io::stdout()) } else { None },
    );
    let err_handle = drain_pipe(
        child.stderr.take(),
        if stream { Some(std::io::stderr()) } else { None },
    );

    let status = match timeout {
        None => child.wait().map_err(RunError::Io)?,
        Some(secs) => {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f64(secs);
            loop {
                match child.try_wait().map_err(RunError::Io)? {
                    Some(s) => break s,
                    None => {
                        if std::time::Instant::now() >= deadline {
                            let _ = child.kill();
                            let _ = child.wait();
                            return Err(RunError::Timeout);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                }
            }
        }
    };
    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();
    Ok(std::process::Output { status, stdout, stderr })
}

/// Lex, parse, and compile module source (for `import`). Errors are flattened
/// to a string for the ImportError message.
fn compile_source(source: &str, origin: Rc<str>) -> Result<Rc<CodeObject>, String> {
    let tokens = crate::lexer::Lexer::new(source).tokenize().map_err(|e| e.to_string())?;
    let program = crate::parser::Parser::new(tokens).parse().map_err(|e| e.message.clone())?;
    crate::compiler::compile(&program, origin).map_err(|e| e.message.clone())
}

/// Whether `name` is a callback-taking collection operation (driven by the VM).
pub fn is_seq_op(name: &str) -> bool {
    SeqOp::from_name(name).is_some()
}

/// Whether `v` is a container whose stringification must render element
/// `__repr__` dunders (and be cycle-safe) rather than use native `repr`.
fn is_container(v: &Value) -> bool {
    matches!(v, Value::List(_) | Value::Tuple(_) | Value::Dict(_))
}

/// Whether `value`, walked as a container, reaches any instance with an Oro
/// `__repr__` — those need a frame to render. Also the phase-1 half of the
/// cycle-safe container repr: it collects such instances in build order.
fn collect_repr_instances(value: &Value, out: &mut Vec<Value>, path: &mut Vec<*const ()>) {
    match value {
        Value::List(l) => {
            let p = Rc::as_ptr(l) as *const ();
            if path.contains(&p) {
                return; // cycle: rendered as "[...]", no instances inside
            }
            path.push(p);
            for v in l.borrow().iter() {
                collect_repr_instances(v, out, path);
            }
            path.pop();
        }
        Value::Tuple(t) => {
            let p = Rc::as_ptr(t) as *const ();
            if path.contains(&p) {
                return;
            }
            path.push(p);
            for v in t.iter() {
                collect_repr_instances(v, out, path);
            }
            path.pop();
        }
        Value::Dict(d) => {
            let p = Rc::as_ptr(d) as *const ();
            if path.contains(&p) {
                return;
            }
            path.push(p);
            for (k, v) in d.borrow().items() {
                collect_repr_instances(k, out, path);
                collect_repr_instances(v, out, path);
            }
            path.pop();
        }
        Value::Instance(_) if instance_method(value, "__repr__").is_some() => {
            out.push(value.clone());
        }
        _ => {}
    }
}

/// Phase-3 half: rebuild the repr string, splicing the phase-2 `results` in for
/// instances that have an Oro `__repr__` (consumed left-to-right via `idx`), and
/// emitting `[...]`/`{...}`/`(...)` for reference cycles — matching CPython.
fn build_repr(value: &Value, results: &[String], idx: &mut usize, path: &mut Vec<*const ()>) -> String {
    match value {
        Value::List(l) => {
            let p = Rc::as_ptr(l) as *const ();
            if path.contains(&p) {
                return "[...]".to_string();
            }
            path.push(p);
            let parts: Vec<String> =
                l.borrow().iter().map(|v| build_repr(v, results, idx, path)).collect();
            path.pop();
            format!("[{}]", parts.join(", "))
        }
        Value::Tuple(t) => {
            let p = Rc::as_ptr(t) as *const ();
            if path.contains(&p) {
                return "(...)".to_string();
            }
            path.push(p);
            let parts: Vec<String> =
                t.iter().map(|v| build_repr(v, results, idx, path)).collect();
            path.pop();
            if parts.len() == 1 {
                format!("({},)", parts[0])
            } else {
                format!("({})", parts.join(", "))
            }
        }
        Value::Dict(d) => {
            let p = Rc::as_ptr(d) as *const ();
            if path.contains(&p) {
                return "{...}".to_string();
            }
            path.push(p);
            let parts: Vec<String> = d
                .borrow()
                .items()
                .iter()
                .map(|(k, v)| {
                    let ks = build_repr(k, results, idx, path);
                    let vs = build_repr(v, results, idx, path);
                    format!("{ks}: {vs}")
                })
                .collect();
            path.pop();
            format!("{{{}}}", parts.join(", "))
        }
        Value::Instance(_) if instance_method(value, "__repr__").is_some() => {
            let s = results.get(*idx).cloned().unwrap_or_default();
            *idx += 1;
            s
        }
        // Scalars, and instances without a __repr__ (default/exception form).
        other => other.repr(),
    }
}

/// The dunder method name for a binary-arithmetic opcode, or `None` for one
/// that has none.
///
/// The bitwise operators are the `None` cases, and deliberately: Oro's dunder
/// set is fixed (`docs/reference.md` §4.8), `&` on two integers is what the
/// operator is for, and a class that wants to mean something else by it is
/// asking for an overload the language does not offer.
fn arith_dunder(op: &Op) -> Option<&'static str> {
    Some(match op {
        Op::BinAdd => "__add__",
        Op::BinSub => "__sub__",
        Op::BinMul => "__mul__",
        Op::BinDiv => "__truediv__",
        Op::BinFloorDiv => "__floordiv__",
        Op::BinMod => "__mod__",
        Op::BinPow => "__pow__",
        _ => return None,
    })
}

/// The operator symbol for a binary-arithmetic opcode (for error messages).
fn arith_symbol(op: &Op) -> &'static str {
    match op {
        Op::BinAdd => "+",
        Op::BinSub => "-",
        Op::BinMul => "*",
        Op::BinDiv => "/",
        Op::BinFloorDiv => "//",
        Op::BinMod => "%",
        Op::BinPow => "**",
        Op::BinBitAnd => "&",
        Op::BinBitOr => "|",
        Op::BinBitXor => "^",
        Op::BinShl => "<<",
        Op::BinShr => ">>",
        _ => unreachable!("arith_symbol on a non-arithmetic op"),
    }
}

/// If `v` is an instance whose class chain defines method `name`, return the
/// function and the class it is defined in.
fn instance_method(v: &Value, name: &str) -> Option<(Rc<Function>, Rc<Class>)> {
    if let Value::Instance(inst) = v {
        if let Some((Value::Func(f), defclass)) = Class::find(&inst.class, name) {
            return Some((f, defclass));
        }
    }
    None
}

// --- Comparison -------------------------------------------------------------

/// Every comparison operator, as far as native code can decide it. `None` is
/// the signal that a user dunder is involved and the VM has to take over —
/// see [`Value::try_equals`] and `Vm::begin_compare`.
fn try_compare_op(op: CmpOp, a: &Value, b: &Value) -> VResult<Option<bool>> {
    use std::cmp::Ordering;
    Ok(match op {
        CmpOp::Eq => a.try_equals(b),
        CmpOp::NotEq => {
            // `__ne__` is its own dunder, and a class may define it without
            // `__eq__` — in which case `try_equals` would answer natively and
            // never reach it. One discriminant test per `!=` buys that.
            if matches!(a, Value::Instance(_)) || matches!(b, Value::Instance(_)) {
                None
            } else {
                a.try_equals(b).map(|r| !r)
            }
        }
        CmpOp::Lt => a.try_compare(b, "<")?.map(|o| o == Ordering::Less),
        CmpOp::Gt => a.try_compare(b, ">")?.map(|o| o == Ordering::Greater),
        CmpOp::LtEq => a.try_compare(b, "<=")?.map(|o| o != Ordering::Greater),
        CmpOp::GtEq => a.try_compare(b, ">=")?.map(|o| o != Ordering::Less),
        CmpOp::In => try_contains(b, a)?,
        CmpOp::NotIn => try_contains(b, a)?.map(|r| !r),
    })
}

/// Do these arguments to `sorted` / `min` / `max` need the VM — is any
/// candidate an instance, whose `<` is a user `__lt__`?
///
/// This is the guard that keeps `min(i, 7)` on the native path it has always
/// been on: one discriminant test per candidate, against an ordering that is
/// already at least linear in them. The uncertain cases — a generator, an
/// iterator, a dict — answer `true` and let `Vm::begin_order` make the same
/// decision with the elements in hand.
fn ord_needs_vm(args: &[Value]) -> bool {
    match args {
        [Value::List(l)] => l.borrow().iter().any(ord_defers),
        [Value::Tuple(t)] => t.iter().any(ord_defers),
        // The three iterables whose elements are never instances.
        [Value::Str(_) | Value::Bytes(_) | Value::Range(_)] => false,
        [_] => true,
        many => many.iter().any(ord_defers),
    }
}

/// How deep [`ord_defers`] looks before giving up and answering "yes".
const ORD_SCAN_DEPTH: u32 = 8;

/// Could ordering this value against another need Oro code? An instance can,
/// and so can a container, because of what may be *inside* it — `min` over a
/// list of lists of instances compares the instances.
///
/// `false` is the strong answer: it means the whole value was walked and holds
/// no instance anywhere, so native ordering cannot get stuck. Everything
/// uncertain — including anything past the scan depth, which is how a cyclic
/// value is kept from being walked forever here — answers `true` and lets the
/// resumable path decide, since that path is correct for every input and merely
/// slower.
fn ord_defers(v: &Value) -> bool {
    fn go(v: &Value, depth: u32) -> bool {
        match v {
            Value::Instance(_) => true,
            _ if depth >= ORD_SCAN_DEPTH => true,
            Value::List(l) => l.borrow().iter().any(|e| go(e, depth + 1)),
            Value::Tuple(t) => t.iter().any(|e| go(e, depth + 1)),
            _ => false,
        }
    }
    go(v, 0)
}

/// Does an ordering hold, given the ordering of the two operands?
fn ord_holds(op: CmpOp, o: std::cmp::Ordering) -> bool {
    use std::cmp::Ordering;
    match op {
        CmpOp::Lt => o == Ordering::Less,
        CmpOp::Gt => o == Ordering::Greater,
        CmpOp::LtEq => o != Ordering::Greater,
        CmpOp::GtEq => o != Ordering::Less,
        CmpOp::Eq => o == Ordering::Equal,
        CmpOp::NotEq => o != Ordering::Equal,
        CmpOp::In | CmpOp::NotIn => unreachable!("membership is not an ordering"),
    }
}

/// The rich-comparison dunder an operator dispatches, or `None` for the two
/// that have none.
fn rich_dunder(op: CmpOp) -> Option<&'static str> {
    Some(match op {
        CmpOp::Eq => "__eq__",
        CmpOp::NotEq => "__ne__",
        CmpOp::Lt => "__lt__",
        CmpOp::Gt => "__gt__",
        CmpOp::LtEq => "__le__",
        CmpOp::GtEq => "__ge__",
        CmpOp::In | CmpOp::NotIn => return None,
    })
}

/// The dunder the *right* operand is asked when the left has nothing to say:
/// `1 < obj` becomes `obj.__gt__(1)`. Equality is its own reflection.
fn reflect_dunder(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Lt => "__gt__",
        CmpOp::Gt => "__lt__",
        CmpOp::LtEq => "__ge__",
        CmpOp::GtEq => "__le__",
        // `!=` reflects to `__ne__`, not to `__eq__`: falling back to `__eq__`
        // here would take the *un-negated* path and answer `{} != obj` with
        // whatever `obj.__eq__` said. The negated fallback is a separate step
        // at the call site, and it has to stay separate.
        CmpOp::NotEq => "__ne__",
        _ => "__eq__",
    }
}

/// The symbol an operator is written with, for the `TypeError` that names it.
fn op_symbol(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Lt => "<",
        CmpOp::Gt => ">",
        CmpOp::LtEq => "<=",
        CmpOp::GtEq => ">=",
        _ => "<",
    }
}

/// Are these two the same heap object? CPython's identity shortcut inside
/// `PyObject_RichCompareBool`, which is why `a in [a]` is `true` even when
/// `a.__eq__` answers `false`, and why a self-referential list compares equal
/// to itself instead of recursing forever.
fn same_object(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::List(x), Value::List(y)) => Rc::ptr_eq(x, y),
        (Value::Tuple(x), Value::Tuple(y)) => Rc::ptr_eq(x, y),
        (Value::Dict(x), Value::Dict(y)) => Rc::ptr_eq(x, y),
        _ => match (a.identity(), b.identity()) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        },
    }
}

/// The elements `item in container` has to scan, for the containers whose
/// membership is a linear run of `==`. Everything else — a string, a `bytes`,
/// a `range`, a `dict` — answers without ever comparing two values with `==`,
/// so it never reaches here.
fn membership_items(container: &Value, item: &Value) -> VResult<Vec<Value>> {
    match container {
        Value::List(l) => Ok(l.borrow().clone()),
        Value::Tuple(t) => Ok((**t).clone()),
        other => Err(runtime_error(format!(
            "internal: {} membership does not dispatch (item {})",
            other.type_name(),
            item.type_name()
        ))),
    }
}

fn try_contains(container: &Value, item: &Value) -> VResult<Option<bool>> {
    match container {
        Value::Str(hay) => match item {
            Value::Str(needle) => Ok(Some(hay.s.contains(&needle.s))),
            _ => Err(type_error("'in <string>' requires string as left operand")),
        },
        // Subsequence, like `str`. CPython also lets an `int` on the left ask
        // whether one octet is present; that is a second meaning for one
        // spelling, so Oro says what it wants instead of guessing.
        Value::Bytes(hay) => match item {
            Value::Bytes(needle) => Ok(Some(subsequence(hay, needle))),
            _ => Err(type_error("'in <bytes>' requires bytes as left operand")),
        },
        Value::List(l) => Ok(seq_contains(&l.borrow(), item)),
        Value::Tuple(t) => Ok(seq_contains(t, item)),
        // A dict key is an `HKey` and a lookup has nowhere to call user code
        // from, which is the decision `docs/hash-and-equality.md` argues at
        // length: a class defining `__eq__` is not a key at all, so `in` over a
        // dict never has one to dispatch.
        Value::Dict(d) => d.borrow().contains(item).map(Some),
        Value::Range(r) => Ok(Some(range_contains(r, item))),
        other => Err(type_error(format!(
            "argument of type '{}' is not iterable",
            other.type_name()
        ))),
    }
}

/// `item in items` for a list or tuple, short-circuiting on the first hit and
/// deferring the whole scan the moment one element needs a user `__eq__`.
///
/// Deferring the *whole* scan rather than one element costs a re-walk of the
/// elements already rejected, and buys a machine that never has to remember how
/// far a native loop got. `in` over a list is O(n) comparisons either way.
fn seq_contains(items: &[Value], item: &Value) -> Option<bool> {
    let mut deferred = false;
    for v in items {
        match v.try_equals(item) {
            Some(true) => return Some(true),
            Some(false) => {}
            // The identity shortcut is asked only here, where the alternative
            // is a frame — so a scan over ints pays nothing for it.
            None if same_object(v, item) => return Some(true),
            None => {
                deferred = true;
                break;
            }
        }
    }
    (!deferred).then_some(false)
}

/// Whether `needle` appears contiguously in `hay`. The empty needle is present
/// in everything, as it is for `str`.
fn subsequence(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    hay.windows(needle.len()).any(|w| w == needle)
}

fn range_contains(r: &RangeVal, item: &Value) -> bool {
    let n = match item {
        Value::Int(i) => *i,
        Value::Bool(b) => *b as i64,
        _ => return false,
    };
    if r.step > 0 {
        n >= r.start && n < r.stop && (n - r.start) % r.step == 0
    } else {
        n <= r.start && n > r.stop && (r.start - n) % (-r.step) == 0
    }
}

#[cfg(test)]
mod tests;
