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
use crate::compiler::{CaptureSource, ClassSpec, CodeObject, Op, ParamInfo, VarTarget};
use crate::task::{TaskHandle, TaskId};
use crate::value::{
    BoundMethod, Class, Fields, Function, Instance, IterState, MethodKind, OroDict, OroList,
    OroTuple, RangeVal, SuperProxy, Value,
};
use std::collections::{HashMap, VecDeque};
use std::cell::Cell;

/// A runtime error carrying the source position of the faulting instruction.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeError {
    pub message: String,
    pub line: usize,
    pub col: usize,
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.line, self.col, self.message)
    }
}

impl std::error::Error for RuntimeError {}

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
    sort_jobs: u32,
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
    DriveSort,
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
    /// widen `Result<Step, RuntimeError>` on the hottest path in the system to
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
}

impl SeqOp {
    fn name(self) -> &'static str {
        match self {
            SeqOp::Map => "map",
            SeqOp::Filter => "filter",
            SeqOp::FlatMap => "flat_map",
            SeqOp::SortBy => "sort_by",
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
        }
    }

    fn from_name(name: &str) -> Option<SeqOp> {
        Some(match name {
            "map" => SeqOp::Map,
            "filter" => SeqOp::Filter,
            "flat_map" => SeqOp::FlatMap,
            "sort_by" => SeqOp::SortBy,
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
                | SeqOp::TakeWhile | SeqOp::DropWhile
        )
    }
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

/// An in-flight `.map(f)` / `.filter(p)`. Like [`SortJob`], the callback is Oro
/// code and must run in a frame, so elements are processed one at a time and the
/// collection is rebuilt once the last result lands.
struct SeqJob {
    op: SeqOp,
    shape: SeqShape,
    /// For a dict, each item is the `(key, value)` pair.
    items: Vec<Value>,
    results: Vec<Value>,
    next: usize,
    /// `None` for the predicate-less forms (`any()`, `all()`, `count()`), where
    /// each element stands in for its own callback result.
    func: Option<Value>,
}

/// An in-flight `sorted(key=…)` / `list.sort(key=…)`. The key function is Oro
/// code, which must run in a frame rather than by re-entering the interpreter,
/// so keys are computed one element at a time and collected here; when the last
/// one lands the sort runs natively over the finished key vector.
struct SortJob {
    items: Vec<Value>,
    keys: Vec<Value>,
    next: usize,
    keyfn: Value,
    reverse: bool,
    /// `Some(list)` for `list.sort()`, which sorts in place and yields None;
    /// `None` for `sorted()`, which pushes a new list.
    in_place: Option<Rc<OroList>>,
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
/// the same trade [`SortJob`] and [`SeqJob`] make — the difference is only that
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
    /// carries what they decorate.
    keys: Vec<Value>,
    items: Vec<Value>,
    reverse: bool,
    state: OrdState,
}

/// What a finished ordering's value is for — the same shape as [`StrCont`],
/// and for the same reason: an ordering is not always something the program
/// wrote at the top level. `xs.map(sorted)` runs one per element from inside a
/// [`SeqJob`], and `sorted(xs, key=min)` runs one per key from inside a
/// [`SortJob`], and neither wants its answer on the operand stack.
enum OrdCont {
    /// An ordering the program wrote: push it.
    Push,
    /// A collection callback's result: record it and carry on with the chain.
    Seq,
    /// A sort key: record it and carry on computing keys.
    Sort,
}

/// Which ordering an [`OrdJob`] is carrying out, and where its answer goes.
enum OrdKind {
    /// `sorted(...)` / `xs.sorted()`: push a new collection of this shape.
    Sort(SeqShape),
    /// `xs.sort(...)`: write back in place and push `None`.
    SortInPlace(Rc<OroList>),
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
    sort_jobs: Vec<SortJob>,
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
            sort_jobs: Vec::new(),
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
    import_root: std::path::PathBuf,
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
pub fn add_values(a: &Value, b: &Value) -> Result<Value, String> {
    arith::binary(&Op::BinAdd, a, b)
}

/// Run a compiled module to completion, returning its (ignored) result. Used by
/// tests; `sys.argv` is empty.
pub fn run(code: Rc<CodeObject>) -> Result<Value, RuntimeError> {
    let mut vm = Vm::new(Vec::new());
    vm.push_module_frame(code);
    vm.run_loop()
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
            None => Err(e),
        },
    }
}

impl Vm {
    fn new(argv: Vec<String>) -> Vm {
        Vm {
            task: Task::new(),
            frame_pool: Vec::new(),
            excs: exceptions::build_registry(),
            argv,
            exit_code: None,
            import_root: std::path::PathBuf::from("."),
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

    fn err(&self, message: impl Into<String>) -> RuntimeError {
        let (line, col) = (self.task.line as usize, self.task.col as usize);
        RuntimeError { message: message.into(), line, col }
    }

    fn wrap<T>(&self, r: Result<T, String>) -> Result<T, RuntimeError> {
        r.map_err(|m| self.err(m))
    }

    /// The list at the top of the stack (left in place), for the incremental
    /// call-argument assembly ops.
    fn expect_list_tos(&mut self, who: &str) -> Result<Rc<OroList>, RuntimeError> {
        match self.top().stack.last() {
            Some(Value::List(l)) => Ok(l.clone()),
            _ => Err(self.err(format!("internal: {who} on non-list"))),
        }
    }

    fn expect_dict_tos(&mut self, who: &str) -> Result<Rc<RefCell<OroDict>>, RuntimeError> {
        match self.top().stack.last() {
            Some(Value::Dict(d)) => Ok(d.clone()),
            _ => Err(self.err(format!("internal: {who} on non-dict"))),
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
            if let Some(uncaught) = self.unwind(exc) {
                let err = self.uncaught_error(&uncaught);
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
            // `Result<Step, RuntimeError>` — 48 bytes, written through a
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
                        if let (Value::List(l), Value::Int(i)) =
                            (&frame.stack[n - 2], &frame.stack[n - 1])
                        {
                            let l = l.borrow();
                            let i = *i;
                            if i >= 0 && (i as usize) < l.len() {
                                let v = l[i as usize].clone();
                                drop(l);
                                frame.stack.truncate(n - 1);
                                frame.stack[n - 2] = v;
                                continue;
                            }
                        }
                    }
                }
                Op::StoreSubscript => {
                    // `xs[i] = v`, same shape and the same declines.
                    let frame = self.task.frames.last_mut().expect("no active frame");
                    let n = frame.stack.len();
                    let ok = n >= 3
                        && match (&frame.stack[n - 2], &frame.stack[n - 1]) {
                            (Value::List(l), Value::Int(i)) => {
                                *i >= 0 && (*i as usize) < l.borrow().len()
                            }
                            _ => false,
                        };
                    if ok {
                        let i = match frame.stack.pop() {
                            Some(Value::Int(i)) => i as usize,
                            _ => unreachable!("checked above"),
                        };
                        let list = match frame.stack.pop() {
                            Some(Value::List(l)) => l,
                            _ => unreachable!("checked above"),
                        };
                        let value = frame.stack.pop().expect("operand stack underflow");
                        list.borrow_mut()[i] = value;
                        continue;
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
            if let Some(uncaught) = self.unwind(to_raise) {
                let err = self.uncaught_error(&uncaught);
                return sched::Slice::Failed(uncaught, err);
            }
        }
    }

    /// Execute a single instruction, reporting how the loop should proceed.
    fn step(&mut self, op: Op) -> Result<Step, RuntimeError> {
            match op {
                Op::LoadConst(i) => {
                    let v = self.task.frames.last().unwrap().code.consts[i as usize].clone();
                    self.push(v);
                }
                Op::LoadNone => self.push(Value::None),
                Op::LoadFast(s) => {
                    let v = self.top().locals[s as usize].clone();
                    if matches!(v, Value::Unbound) {
                        return Err(self.err(self.unbound_local_msg(s)));
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
                        return Err(self.err("local variable referenced before assignment"));
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
                        return Err(self.err("free variable referenced before assignment"));
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
                                    return Err(
                                        self.err(format!("name '{name}' is not defined"))
                                    )
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
                    self.push(Value::Bool(!v.truthy()));
                }
                Op::BinAdd
                | Op::BinSub
                | Op::BinMul
                | Op::BinDiv
                | Op::BinFloorDiv
                | Op::BinMod
                | Op::BinPow => {
                    let b = self.pop();
                    let a = self.pop();
                    let dunder = arith_dunder(&op);
                    match instance_method(&a, dunder) {
                        Some((f, defclass)) => {
                            self.invoke_user(f, a, defclass, vec![b], Vec::new(), ReturnAction::Normal)?;
                        }
                        None if matches!(a, Value::Instance(_)) => {
                            return Err(self.err(format!(
                                "unsupported operand type(s) for {}: '{}' and '{}'",
                                arith_symbol(&op),
                                a.type_label(),
                                b.type_label()
                            )));
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
                Op::PopJumpIfFalse(t) => {
                    let v = self.pop();
                    if !v.truthy() {
                        self.top().pc = t as usize;
                    }
                }
                Op::PopJumpIfTrue(t) => {
                    let v = self.pop();
                    if v.truthy() {
                        self.top().pc = t as usize;
                    }
                }
                Op::JumpIfFalseOrPop(t) => {
                    if self.top().stack.last().unwrap().truthy() {
                        self.pop();
                    } else {
                        self.top().pc = t as usize;
                    }
                }
                Op::JumpIfTrueOrPop(t) => {
                    if self.top().stack.last().unwrap().truthy() {
                        self.top().pc = t as usize;
                    } else {
                        self.pop();
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
                Op::ListExtend => {
                    let iterable = self.pop();
                    let items = self.wrap(iterate_to_vec(&iterable))?;
                    let list = self.expect_list_tos("ListExtend")?;
                    list.borrow_mut().extend(items);
                }
                Op::MapSetItem => {
                    let v = self.pop();
                    let k = self.pop();
                    let dict = self.expect_dict_tos("MapSetItem")?;
                    self.wrap(dict.borrow_mut().insert(k, v))?;
                }
                Op::MapMerge => {
                    let mapping = self.pop();
                    let pairs = self.wrap(dict_pairs(&mapping))?;
                    let dict = self.expect_dict_tos("MapMerge")?;
                    for (k, v) in pairs {
                        self.wrap(dict.borrow_mut().insert(k, v))?;
                    }
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
                            return Err(self.err(msg));
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
                            return Err(self.err(
                                "super() is only valid inside a method".to_string(),
                            ))
                        }
                    };
                    self.push(sup);
                }
                Op::UnpackSequence(n) => {
                    let n = n as usize;
                    let seq = self.pop();
                    let items = self.wrap(iterate_to_vec(&seq))?;
                    if items.len() != n {
                        let msg = if items.len() < n {
                            format!("not enough values to unpack (expected {n}, got {})", items.len())
                        } else {
                            format!("too many values to unpack (expected {n})")
                        };
                        return Err(self.err(msg));
                    }
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
                            return Err(self.err(msg));
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
                                Some(
                                    g.frame
                                        .take()
                                        .map(|b| *b.downcast::<Frame>().expect("gen frame")),
                                )
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
                        let next = self.wrap(iter_next(&it))?;
                        match next {
                            Some(v) => self.push(v),
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
                    gen.borrow_mut().frame = Some(Box::new(frame));
                    match driver {
                        GenDriver::ForLoop(_) => self.push(value),
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
                            return Err(self.err("No active exception to re-raise".to_string()))
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

    fn unbound_local_msg(&self, slot: u16) -> String {
        let code = &self.task.frames.last().unwrap().code;
        // A name that also exists at module scope but was made local by an
        // assignment (no `global`) is the classic footgun — teach the fix.
        for (s, name) in &code.shadow_hints {
            if *s == slot {
                return format!(
                    "local variable '{name}' referenced before assignment: '{name}' is assigned \
                     inside this function, which makes it local and shadows the module-level \
                     '{name}'. To read and update the module value, declare `global {name}` at \
                     the top of the function; otherwise keep the state on an object, or rename \
                     the local."
                );
            }
        }
        // Recover the variable's name from its parameter descriptor when we can,
        // for a friendlier message.
        for p in &code.params {
            if let VarTarget::Local(s) = p.target {
                if s == slot {
                    return format!("local variable '{}' referenced before assignment", p.name);
                }
            }
        }
        "local variable referenced before assignment".to_string()
    }

    // --- Closures ------------------------------------------------------------

    fn make_function(&mut self, idx: usize) -> Result<(), RuntimeError> {
        let proto = self.task.frames.last().unwrap().code.protos[idx].clone();
        let defaults = self.popn(proto.n_defaults);
        let frame = self.task.frames.last().unwrap();
        let freevars: Vec<Rc<RefCell<Value>>> = proto
            .captures
            .iter()
            .map(|c| match c {
                CaptureSource::Cell(i) => frame.cells[*i as usize].clone(),
                CaptureSource::Free(i) => frame.free[*i as usize].clone(),
            })
            .collect();
        let func = Function { code: proto.code.clone(), defaults, freevars };
        self.push(Value::Func(Rc::new(func)));
        Ok(())
    }

    // --- Calls ---------------------------------------------------------------

    fn do_call(&mut self, n: usize) -> Result<Step, RuntimeError> {
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
        if code.is_generator || !code.simple_params || n > code.params.len() {
            return None;
        }
        let first_defaulted = code.params.len() - f.defaults.len();
        if n < first_defaulted {
            return None;
        }
        Some(f.clone())
    }

    /// Move `n` arguments from the caller's operand stack straight into a fresh
    /// frame's slots. No argument vector, no re-copy — the values are moved once.
    fn call_fast(&mut self, func: Rc<Function>, n: usize) -> Result<Step, RuntimeError> {
        if self.task.frames.len() >= MAX_FRAMES {
            return Err(self.err("maximum recursion depth exceeded"));
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
        let first_defaulted = params.len() - func.defaults.len();
        for (i, p) in params.iter().enumerate().skip(n) {
            store_param(&mut frame, p.target, func.defaults[i - first_defaulted].clone());
        }
        self.task.frames.push(frame);
    }

    fn do_call_ex(&mut self) -> Result<Step, RuntimeError> {
        let kwdict = self.pop();
        let poslist = self.pop();
        let callee = self.pop();
        let args = match poslist {
            Value::List(l) => l.borrow().clone(),
            _ => return Err(self.err("internal: CallEx positional list malformed")),
        };
        let kwargs = match kwdict {
            Value::Dict(d) => {
                let d = d.borrow();
                let mut out = Vec::with_capacity(d.len());
                for (k, v) in d.items() {
                    match k {
                        Value::Str(s) => out.push((s.s.clone(), v.clone())),
                        _ => return Err(self.err("keywords must be strings")),
                    }
                }
                out
            }
            _ => return Err(self.err("internal: CallEx keyword dict malformed")),
        };
        self.invoke(callee, args, kwargs)
    }

    /// Dispatch a call. Builtins and bound methods execute natively (they never
    /// re-enter Oro), so only Oro functions push a new frame — keeping the one
    /// flat loop intact.
    fn invoke(
        &mut self,
        callee: Value,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<Step, RuntimeError> {
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
                    "spawn" => return self.do_spawn(args, kwargs),
                    "chan" => return self.do_chan(args, kwargs),
                    "yield_now" => return self.do_yield_now(args, kwargs),
                    // `time.sleep` parks the calling task on the reactor's
                    // deadline list. It used to be `std::thread::sleep`, which
                    // stopped every task in the VM, so it too has to answer
                    // with a `Step` rather than a `Value`.
                    "time.sleep" => return self.do_sleep(args, kwargs),
                    "print" => return self.do_print(args, kwargs).map(|()| Step::Next),
                    // `sorted` and `min`/`max` decide with `<`, which may be
                    // a user `__lt__` — so they are driven from the VM, which
                    // is the only layer that can run one. Elements with no
                    // dunder still take the native path, one branch further in.
                    "sorted" if !kwargs.is_empty() || ord_needs_vm(&args) => {
                        return self.do_sorted(args, kwargs)
                    }
                    "min" | "max" if ord_needs_vm(&args) => {
                        return self.do_extreme(b.name, args, kwargs)
                    }
                    // proc.run is finished here so it can take keyword args and
                    // build a Completed instance.
                    "proc.run" => return self.do_proc_run(args, kwargs).map(|()| Step::Next),
                    "str" if matches!(args.first(), Some(Value::Instance(_))) && args.len() == 1 => {
                        return self
                            .stringify_instance(args.into_iter().next().unwrap(), false)
                            .map(|()| Step::Next);
                    }
                    "repr" if matches!(args.first(), Some(Value::Instance(_))) && args.len() == 1 => {
                        return self
                            .stringify_instance(args.into_iter().next().unwrap(), true)
                            .map(|()| Step::Next);
                    }
                    // str()/repr() of a container render elements' __repr__ (and
                    // are cycle-safe), which needs VM dispatch, not native repr.
                    "str" | "repr" if args.len() == 1 && is_container(&args[0]) => {
                        return self
                            .begin_stringify(args.into_iter().next().unwrap(), StrCont::Push)
                            .map(|()| Step::Next);
                    }
                    "len" if matches!(args.first(), Some(Value::Instance(_))) && args.len() == 1 => {
                        return self.dunder_len(args.into_iter().next().unwrap()).map(|()| Step::Next);
                    }
                    _ => {}
                }
                // A generator argument must be drained through frames first.
                if let Some(step) =
                    self.materialize_generator_args(&Value::Builtin(b.clone()), &args, &kwargs)?
                {
                    return Ok(step);
                }
                if !kwargs.is_empty() {
                    return Err(self.err(format!("{}() takes no keyword arguments", b.name)));
                }
                let r = self.wrap((b.func)(args))?;
                self.push(r);
                Ok(Step::Next)
            }
            Value::Method(m) => match &m.kind {
                MethodKind::Native(name) => {
                    // Task and channel methods are dispatched ahead of
                    // everything else in this arm for two reasons. They are the
                    // ones that can *park*, so they must reach the VM rather
                    // than `call_method`, which can only return a `Value`. And
                    // a generator handed to `ch.send` has to arrive at the far
                    // end as a generator — the materialise path below would
                    // drain it into a list, which is precisely the "a generator
                    // is a first-class value and can cross tasks" case §3
                    // calls out.
                    if let Some(step) = self.task_or_channel_method(&m.receiver, name, &args, &kwargs)? {
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
                    if matches!(m.receiver, Value::Stream(_)) {
                        if let Some(step) =
                            self.stream_io_method(&m.receiver, name, &args, &kwargs)?
                        {
                            return Ok(step);
                        }
                    }
                    // `to_str` may need to run a user `__str__`, or render a
                    // container's elements through their `__repr__`; both go
                    // through frames, so they cannot run as native methods.
                    if &**name == "to_str" && args.is_empty() && kwargs.is_empty() {
                        if matches!(m.receiver, Value::Instance(_)) {
                            return self
                                .stringify_instance(m.receiver.clone(), false)
                                .map(|()| Step::Next);
                        }
                        if is_container(&m.receiver) {
                            return self
                                .begin_stringify(m.receiver.clone(), StrCont::Push)
                                .map(|()| Step::Next);
                        }
                    }
                    // map/filter run Oro callbacks, so they are driven from the
                    // VM rather than executed as native methods.
                    if let Some(op) = SeqOp::from_name(name)
                        .filter(|_| crate::builtins::is_collection(&m.receiver))
                    {
                        // A generator receiver has to be drained first; the
                        // retry arrives back here with a list in its place.
                        if matches!(m.receiver, Value::Generator(_)) {
                            let callee = Value::Method(m.clone());
                            let with_recv = std::iter::once(m.receiver.clone())
                                .chain(args.iter().cloned())
                                .collect::<Vec<_>>();
                            if let Some(step) =
                                self.materialize_receiver(&callee, with_recv, kwargs.clone())?
                            {
                                return Ok(step);
                            }
                        }
                        return self.do_seq_op(op, &m.receiver, args, kwargs).map(|()| Step::Next);
                    }
                    // list.sort(key=…, reverse=…) shares sorted()'s frame-driven
                    // key machinery; it just writes back in place.
                    if &**name == "sort" {
                        if let Value::List(l) = &m.receiver {
                            if !args.is_empty() {
                                return Err(self.err("sort() takes no positional arguments"));
                            }
                            let (keyfn, reverse) = self.sort_kwargs("sort", kwargs)?;
                            let items = l.borrow().clone();
                            return self
                                .begin_sort(items, keyfn, reverse, Some(l.clone()))
                                .map(|()| Step::Next);
                        }
                    }
                    // The chain's three orderings. Like their builtin twins
                    // they compare with `<`, so a receiver of instances needs
                    // frames; anything else falls straight through to native.
                    if matches!(&**name, "sorted" | "min" | "max")
                        && !matches!(m.receiver, Value::Generator(_))
                        && crate::builtins::is_collection(&m.receiver)
                        && args.is_empty()
                        && kwargs.is_empty()
                    {
                        let (shape, items) = self.seq_receiver(name, &m.receiver)?;
                        let keys = items.clone();
                        let kind = match &**name {
                            "sorted" => OrdKind::Sort(shape),
                            other => OrdKind::Extreme {
                                want_min: other == "min",
                                who: if other == "min" { "min" } else { "max" },
                            },
                        };
                        return self.begin_order(kind, items, keys, false).map(|()| Step::Next);
                    }
                    // A generator *receiver* is drained the same way a
                    // generator argument is, for the same reason: the native
                    // method below iterates it, and native code can never
                    // resume a generator. The retry arrives back here with a
                    // list in the receiver's place. The `matches!` keeps the
                    // name test — and the vector it builds — off the path every
                    // other native method call takes.
                    if matches!(m.receiver, Value::Generator(_))
                        && crate::builtins::drains_generator_receiver(name)
                    {
                        let callee = Value::Method(m.clone());
                        let with_recv = std::iter::once(m.receiver.clone())
                            .chain(args.iter().cloned())
                            .collect::<Vec<_>>();
                        if let Some(step) =
                            self.materialize_receiver(&callee, with_recv, kwargs.clone())?
                        {
                            return Ok(step);
                        }
                    }
                    if let Some(step) =
                        self.materialize_generator_args(&Value::Method(m.clone()), &args, &kwargs)?
                    {
                        return Ok(step);
                    }
                    let r =
                        self.wrap(crate::builtins::call_method(&m.receiver, name, args, kwargs))?;
                    self.push(r);
                    Ok(Step::Next)
                }
                MethodKind::User { func, defclass } => {
                    // A `def` with a `yield` in it is a generator function
                    // wherever it is written, and calling one produces a
                    // generator rather than running the body. A method is no
                    // exception in CPython, and is none here: the frame is
                    // built exactly as the plain-function path builds it and
                    // handed to a `GenBox` instead of being pushed. The one
                    // extra step is `super_ctx`, which goes on the frame before
                    // it is parked, so `super()` still resolves when the
                    // generator is resumed — possibly in another task, long
                    // after this call returned.
                    if func.code.is_generator {
                        if self.task.frames.len() >= MAX_FRAMES {
                            return Err(self.err("maximum recursion depth exceeded"));
                        }
                        let receiver = m.receiver.clone();
                        let mut frame =
                            self.bind_call(func, Some(receiver.clone()), args, kwargs)?;
                        frame.super_ctx = Some((defclass.clone(), receiver));
                        let gen = crate::value::GenBox {
                            done: false,
                            frame: Some(Box::new(frame)),
                        };
                        self.push(Value::Generator(Rc::new(RefCell::new(gen))));
                        return Ok(Step::Next);
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
                    return Err(self.err("maximum recursion depth exceeded"));
                }
                let frame = self.bind_call(&f, None, args, kwargs)?;
                if f.code.is_generator {
                    // Calling a generator function does not run it; it produces a
                    // generator holding the suspended (unstarted) frame.
                    let gen = crate::value::GenBox { done: false, frame: Some(Box::new(frame)) };
                    self.push(Value::Generator(Rc::new(RefCell::new(gen))));
                } else {
                    self.task.frames.push(frame);
                }
                Ok(Step::Next)
            }
            Value::Class(class) => self.instantiate(class, args, kwargs).map(|()| Step::Next),
            other => Err(self.err(format!("'{}' object is not callable", other.type_label()))),
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
    ) -> Result<Option<Step>, RuntimeError> {
        let (task_recv, chan_recv) = match receiver {
            Value::Task(h) => (Some(h.clone()), None),
            Value::Channel(c) => (None, Some(c.clone())),
            _ => return Ok(None),
        };
        if !kwargs.is_empty() {
            return Ok(Some(
                self.raise("TypeError", format!("{name}() takes no keyword arguments")),
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
            Err(vm.raise("TypeError", format!("{name}() takes {expected} ({} given)", args.len())))
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
    ) -> Result<(), RuntimeError> {
        if self.task.frames.len() >= MAX_FRAMES {
            return Err(self.err("maximum recursion depth exceeded"));
        }
        // An ordinary `obj.m()` with a `yield` in it is handled at the call
        // site, where it produces a generator the way a plain `def` does. What
        // is left here is the dispatched half — a dunder, or a bound method
        // used as a chain callback — and every one of those has a continuation
        // waiting for a *value* from a frame that runs now (`DriveStr` wants
        // the string, `DriveSort` the key, `DriveSeq` the element). Handing one
        // a generator instead is not a feature, it is a different bug. It was a
        // `yield outside a generator` panic before; refusing by name is the
        // same answer `sorted(key=…)` already gives.
        if func.code.is_generator {
            let name = func.code.name.clone();
            return Err(self.err(format!(
                "{name}() has a `yield` in it, and Oro does not carry generators \
                 through dunders and callbacks — move it to a module-level def"
            )));
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
    ) -> Result<(), RuntimeError> {
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
            Some(_) => Err(self.err(format!("{}.__init__ is not a function", class.name))),
            // An exception class with no custom __init__ stores its args tuple
            // natively (BaseException-style), so `ValueError("x")` just works.
            None if class.is_exception => {
                if !kwargs.is_empty() {
                    return Err(self.err(format!("{}() takes no keyword arguments", class.name)));
                }
                let exc = self.make_exception_instance(class, args);
                self.push(exc);
                Ok(())
            }
            None => {
                if !args.is_empty() || !kwargs.is_empty() {
                    return Err(self.err(format!("{}() takes no arguments", class.name)));
                }
                self.push(inst);
                Ok(())
            }
        }
    }

    /// `str()`/`repr()` of an instance: run `__str__` (or `__repr__` when
    /// `want_repr`), falling back to the other, then to the default text.
    fn stringify_instance(&mut self, value: Value, want_repr: bool) -> Result<(), RuntimeError> {
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

    fn dunder_len(&mut self, value: Value) -> Result<(), RuntimeError> {
        let inst = match &value {
            Value::Instance(i) => i.clone(),
            _ => unreachable!(),
        };
        match Class::find(&inst.class, "__len__") {
            Some((Value::Func(f), defclass)) => {
                self.invoke_user(f, value, defclass, Vec::new(), Vec::new(), ReturnAction::Normal)
            }
            _ => Err(self.err(format!("object of type '{}' has no len()", inst.class.name))),
        }
    }

    /// The callback-taking half of the collection protocol. Every one of these
    /// runs Oro code per element, so they are driven from the VM a frame at a
    /// time rather than executed as native methods.
    ///
    /// Which collection comes back is governed by [`SeqOp::preserves_shape`].
    /// A dict's callback is called with two arguments (key and value), so
    /// `d.filter((k, v) => v > 1)` reads naturally instead of forcing the caller
    /// to index a pair.
    fn do_seq_op(
        &mut self,
        op: SeqOp,
        receiver: &Value,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<(), RuntimeError> {
        let who = op.name();
        if !kwargs.is_empty() {
            return Err(self.err(format!("{who}() takes no keyword arguments")));
        }
        let callable = |v: &Value| {
            matches!(v, Value::Func(_) | Value::Builtin(_) | Value::Method(_))
        };
        // Arity: reduce takes (initial, f); any/all/count take an optional
        // predicate; everything else takes exactly one function.
        let (func, seed) = match op {
            SeqOp::Reduce => match args.as_slice() {
                [init, f] if callable(f) => (Some(f.clone()), Some(init.clone())),
                [_, other] => {
                    return Err(self.err(format!(
                        "reduce() needs a function as its second argument, not '{}'",
                        other.type_name()
                    )))
                }
                _ => {
                    return Err(self.err(
                        "reduce() takes an initial value and a function, e.g. \
                         xs.reduce(0, (acc, x) => acc + x)",
                    ))
                }
            },
            SeqOp::Any | SeqOp::All | SeqOp::Count => match args.as_slice() {
                [] => (None, None),
                [f] if callable(f) => (Some(f.clone()), None),
                [other] => {
                    return Err(self.err(format!(
                        "{who}() needs a function, not '{}'",
                        other.type_name()
                    )))
                }
                _ => return Err(self.err(format!("{who}() takes at most 1 argument"))),
            },
            _ => match args.as_slice() {
                [f] if callable(f) => (Some(f.clone()), None),
                [other] => {
                    return Err(self.err(format!(
                        "{who}() needs a function, not '{}'",
                        other.type_name()
                    )))
                }
                _ => return Err(self.err(format!("{who}() takes exactly 1 argument"))),
            },
        };

        let (shape, items) = self.seq_receiver(who, receiver)?;
        self.task.seq_jobs.push(SeqJob {
            op,
            shape,
            items,
            // Reduce seeds its accumulator here; the others accumulate results.
            results: seed.into_iter().collect(),
            next: 0,
            func,
        });
        self.drive_seq()
    }

    /// The elements a collection operation walks, and the shape to rebuild.
    fn seq_receiver(
        &mut self,
        who: &str,
        receiver: &Value,
    ) -> Result<(SeqShape, Vec<Value>), RuntimeError> {
        Ok(match receiver {
            Value::List(l) => (SeqShape::List, l.borrow().clone()),
            Value::Tuple(t) => (SeqShape::Tuple, t.as_slice().to_vec()),
            Value::Dict(d) => {
                let pairs: Vec<Value> = d
                    .borrow()
                    .items()
                    .iter()
                    .map(|(k, v)| Value::Tuple(OroTuple::new(vec![k.clone(), v.clone()])))
                    .collect();
                (SeqShape::Dict, pairs)
            }
            // A range has no literal to rebuild, so it materialises to a list.
            Value::Range(_) => (SeqShape::List, self.wrap(iterate_to_vec(receiver))?),
            other => {
                return Err(self.err(format!(
                    "'{}' object has no method '{who}'",
                    other.type_name()
                )))
            }
        })
    }

    fn drive_seq(&mut self) -> Result<(), RuntimeError> {
        loop {
            let (item, func, shape, op) = {
                let job = self.task.seq_jobs.last().expect("active seq job");
                // `find`, `any` and `all` stop as soon as the answer is settled,
                // so a predicate is never called more often than it must be.
                let settled = match job.op {
                    SeqOp::Find | SeqOp::Any => job.results.iter().any(|r| r.truthy()),
                    SeqOp::All => job.results.iter().any(|r| !r.truthy()),
                    _ => false,
                };
                if settled || job.next >= job.items.len() {
                    let job = self.task.seq_jobs.pop().unwrap();
                    return self.finish_seq(job);
                }
                (job.items[job.next].clone(), job.func.clone(), job.shape, job.op)
            };
            self.task.seq_jobs.last_mut().unwrap().next += 1;

            // No predicate (`any()`, `all()`, `count()`): the element is its own
            // result, so no frame is needed at all.
            let Some(func) = func else {
                self.task.seq_jobs.last_mut().unwrap().results.push(item);
                continue;
            };

            // A dict callback is spread over two parameters; reduce prepends the
            // accumulator.
            let mut call_args = match shape {
                SeqShape::Dict => match &item {
                    Value::Tuple(t) => vec![t[0].clone(), t[1].clone()],
                    _ => vec![item.clone()],
                },
                _ => vec![item.clone()],
            };
            if op == SeqOp::Reduce {
                let acc = self
                    .task
                    .seq_jobs
                    .last()
                    .and_then(|j| j.results.last().cloned())
                    .unwrap_or(Value::None);
                call_args.insert(0, acc);
            }

            match func {
                Value::Func(f) => {
                    if self.task.frames.len() >= MAX_FRAMES {
                        return Err(self.err("maximum recursion depth exceeded"));
                    }
                    if f.code.is_generator {
                        return Err(self.err(format!(
                            "{}() callback must not be a generator function",
                            op.name()
                        )));
                    }
                    let mut frame = self.bind_call(&f, None, call_args, Vec::new())?;
                    frame.ret_action = ReturnAction::DriveSeq;
                    self.task.frames.push(frame);
                    return Ok(());
                }
                Value::Builtin(b) => {
                    let Some(call_args) = self.ord_callback(b.name, call_args, OrdCont::Seq)?
                    else {
                        return Ok(());
                    };
                    let r = self.wrap((b.func)(call_args))?;
                    self.record_seq_result(r);
                }
                Value::Method(m) => {
                    let r = match &m.kind {
                        MethodKind::Native(name) => {
                            self.wrap(crate::builtins::call_method(&m.receiver, name, call_args, Vec::new()))?
                        }
                        MethodKind::User { func, defclass } => {
                            if self.task.frames.len() >= MAX_FRAMES {
                                return Err(self.err("maximum recursion depth exceeded"));
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
                _ => return Err(self.err(format!("{}() callback is not callable", op.name()))),
            }
        }
    }

    /// Record one callback result. `reduce` threads a single accumulator rather
    /// than collecting per-element results, so it replaces instead of appending.
    fn record_seq_result(&mut self, value: Value) {
        let job = self.task.seq_jobs.last_mut().expect("seq job");
        if job.op == SeqOp::Reduce {
            job.results.clear();
        }
        job.results.push(value);
    }

    /// Rebuild the result once every callback result is in. Which collection
    /// comes back is governed by [`SeqOp::preserves_shape`]: operations that
    /// select or reorder keep the receiver's type, operations that reshape the
    /// data return a list.
    fn finish_seq(&mut self, job: SeqJob) -> Result<(), RuntimeError> {
        let SeqJob { op, shape, items, results, .. } = job;

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
            SeqOp::SortBy => return self.begin_order(OrdKind::Sort(shape), items, results, false),
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

    /// Turn a vector of elements back into the collection type `shape`. For a
    /// dict the elements are `(key, value)` pairs.
    fn rebuild_shape(shape: SeqShape, items: Vec<Value>) -> Result<Value, String> {
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
                            return Err(format!(
                                "rebuilding a dict needs (key, value) pairs, not '{}'",
                                other.type_name()
                            ))
                        }
                    };
                    if pair.len() != 2 {
                        return Err(format!(
                            "rebuilding a dict needs 2-element pairs, got {} elements",
                            pair.len()
                        ));
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
    ) -> Result<Option<Step>, RuntimeError> {
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
    ) -> Result<Option<Step>, RuntimeError> {
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
    fn drive_materialize(&mut self) -> Result<Step, RuntimeError> {
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
                    Some(g.frame.take().map(|b| *b.downcast::<Frame>().expect("gen frame")))
                }
            };
            match taken {
                Some(Some(frame)) => {
                    if self.task.frames.len() >= MAX_FRAMES {
                        return Err(self.err("maximum recursion depth exceeded"));
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

    /// `sorted(iterable, key=…, reverse=…)`. Without a key this is the plain
    /// native sort; with one, every element's key is computed through a frame
    /// first (see [`SortJob`]).
    fn do_sorted(
        &mut self,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<Step, RuntimeError> {
        // sorted() is intercepted before the generic builtin path, so the
        // generator drain has to be requested explicitly here too.
        if let Some(callee) = crate::builtins::lookup("sorted") {
            if let Some(step) = self.materialize_generator_args(&callee, &args, &kwargs)? {
                return Ok(step);
            }
        }
        let iterable = match args.as_slice() {
            [it] => it.clone(),
            _ => return Err(self.err("sorted() takes exactly 1 positional argument")),
        };
        let (keyfn, reverse) = self.sort_kwargs("sorted", kwargs)?;
        let items = self.wrap(crate::vm::iterate_to_vec(&iterable))?;
        self.begin_sort(items, keyfn, reverse, None).map(|()| Step::Next)
    }

    /// `min(...)` / `max(...)`. With one argument it ranges over an iterable,
    /// with several over the arguments themselves — and either way the `<` it
    /// decides with may be a user `__lt__`.
    fn do_extreme(
        &mut self,
        who: &'static str,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<Step, RuntimeError> {
        if let Some(callee) = crate::builtins::lookup(who) {
            if let Some(step) = self.materialize_generator_args(&callee, &args, &kwargs)? {
                return Ok(step);
            }
        }
        if !kwargs.is_empty() {
            return Err(self.err(format!("{who}() takes no keyword arguments")));
        }
        let items = match args.len() {
            0 => return Err(self.err(format!("{who}() expected at least 1 argument"))),
            1 => self.wrap(iterate_to_vec(&args[0]))?,
            _ => args,
        };
        let keys = items.clone();
        self.begin_order(OrdKind::Extreme { want_min: who == "min", who }, items, keys, false)
            .map(|()| Step::Next)
    }

    /// Shared parsing of the `key=`/`reverse=` pair for `sorted` and `list.sort`.
    fn sort_kwargs(
        &mut self,
        who: &str,
        kwargs: Vec<(String, Value)>,
    ) -> Result<(Option<Value>, bool), RuntimeError> {
        let mut keyfn: Option<Value> = None;
        let mut reverse = false;
        for (k, v) in kwargs {
            match k.as_str() {
                "key" => match v {
                    Value::None => {}
                    f @ (Value::Func(_) | Value::Builtin(_) | Value::Method(_)) => keyfn = Some(f),
                    other => {
                        return Err(self.err(format!(
                            "{who}() key must be callable or None, not '{}'",
                            other.type_name()
                        )))
                    }
                },
                "reverse" => reverse = v.truthy(),
                other => {
                    return Err(
                        self.err(format!("{who}() got an unexpected keyword argument '{other}'"))
                    )
                }
            }
        }
        Ok((keyfn, reverse))
    }

    /// Start a sort. With no key function the whole thing is native; otherwise a
    /// [`SortJob`] computes the keys one frame at a time.
    fn begin_sort(
        &mut self,
        items: Vec<Value>,
        keyfn: Option<Value>,
        reverse: bool,
        in_place: Option<Rc<OroList>>,
    ) -> Result<(), RuntimeError> {
        let keyfn = match keyfn {
            Some(f) => f,
            None => {
                // No key: the elements are their own keys.
                let keys = items.clone();
                return self.begin_order(sort_kind(in_place), items, keys, reverse);
            }
        };
        let n = items.len();
        self.task.sort_jobs.push(SortJob {
            items,
            keys: Vec::with_capacity(n),
            next: 0,
            keyfn,
            reverse,
            in_place,
        });
        self.drive_sort()
    }

    fn drive_sort(&mut self) -> Result<(), RuntimeError> {
        loop {
            let (item, keyfn) = {
                let job = self.task.sort_jobs.last().expect("active sort job");
                if job.next >= job.items.len() {
                    let job = self.task.sort_jobs.pop().unwrap();
                    // The keys are user values and may be instances, so the
                    // sort itself can need frames too — `begin_order` decides.
                    return self.begin_order(
                        sort_kind(job.in_place),
                        job.items,
                        job.keys,
                        job.reverse,
                    );
                }
                (job.items[job.next].clone(), job.keyfn.clone())
            };
            self.task.sort_jobs.last_mut().unwrap().next += 1;

            match keyfn {
                Value::Func(f) => {
                    // A plain (non-method) key function: bind it the same way an
                    // ordinary call does, but route its return into the sort job.
                    if self.task.frames.len() >= MAX_FRAMES {
                        return Err(self.err("maximum recursion depth exceeded"));
                    }
                    if f.code.is_generator {
                        return Err(self.err("sort key must not be a generator function"));
                    }
                    let mut frame = self.bind_call(&f, None, vec![item], Vec::new())?;
                    frame.ret_action = ReturnAction::DriveSort;
                    self.task.frames.push(frame);
                    return Ok(());
                }
                // A native key (len, str, …) cannot re-enter Oro, so it can be
                // called inline and the loop continues without a frame.
                Value::Builtin(b) => {
                    let Some(args) = self.ord_callback(b.name, vec![item], OrdCont::Sort)? else {
                        return Ok(());
                    };
                    let key = self.wrap((b.func)(args))?;
                    self.task.sort_jobs.last_mut().unwrap().keys.push(key);
                }
                Value::Method(m) => {
                    let key = match &m.kind {
                        MethodKind::Native(name) => self
                            .wrap(crate::builtins::call_method(&m.receiver, name, vec![item], Vec::new()))?,
                        _ => return Err(self.err("sort key must be a plain function")),
                    };
                    self.task.sort_jobs.last_mut().unwrap().keys.push(key);
                }
                _ => return Err(self.err("sort key is not callable")),
            }
        }
    }

    /// Drive an in-flight `print`: render remaining args left to right, calling
    /// `__str__` (through a frame) for instances that define one. When the last
    /// argument is rendered, join with spaces, emit, and push `None`.
    fn do_print(
        &mut self,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<(), RuntimeError> {
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
                        self.err(format!("print() got an unexpected keyword argument '{other}'"))
                    )
                }
            };
            match v {
                Value::Str(s) => *slot = s.s.clone(),
                Value::None => {}
                other => {
                    return Err(self.err(format!(
                        "print() argument '{k}' must be str or None, not '{}'",
                        other.type_name()
                    )))
                }
            }
        }
        self.task.prints.push(PrintJob { rendered: Vec::new(), remaining: args, next: 0, sep, end });
        self.drive_print()
    }

    fn drive_print(&mut self) -> Result<(), RuntimeError> {
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
    fn begin_stringify(&mut self, value: Value, cont: StrCont) -> Result<(), RuntimeError> {
        let mut instances = Vec::new();
        let mut path = Vec::new();
        collect_repr_instances(&value, &mut instances, &mut path);
        self.task.str_jobs.push(StrJob { value, instances, results: Vec::new(), next: 0, cont });
        self.drive_str()
    }

    /// Advance the top str job by one element `__repr__` call, or finish it.
    /// Re-entered via the `DriveStr` return action after each dunder returns.
    fn drive_str(&mut self) -> Result<(), RuntimeError> {
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
    fn try_compare_dunder(&mut self, cmp: CmpOp, a: &Value, b: &Value) -> Result<bool, RuntimeError> {
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
    fn compare_slow(&mut self, cmp: CmpOp, a: Value, b: Value) -> Result<(), RuntimeError> {
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
    ) -> Result<(), RuntimeError> {
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
    fn drive_cmp(&mut self, mut next: CmpNext) -> Result<(), RuntimeError> {
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
    fn finish_cmp(&mut self, cont: CmpCont, v: bool) -> Result<(), RuntimeError> {
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
    fn step_pair(&mut self, a: Value, b: Value, op: CmpOp) -> Result<PairStep, RuntimeError> {
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
            return Err(self.err("maximum recursion depth exceeded in comparison"));
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
    fn advance_top(&mut self, incoming: Option<bool>) -> Result<CmpNext, RuntimeError> {
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
    fn advance_once(&mut self, incoming: Option<bool>) -> Result<Option<CmpNext>, RuntimeError> {
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
    ) -> Result<(), RuntimeError> {
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
    ) -> Result<(), RuntimeError> {
        // The same predicate the entry points use, asked again here because
        // `sort_by`, `min_by` and `sorted(key=…)` arrive with keys a callback
        // produced, which nothing could have scanned earlier.
        if !keys.iter().any(ord_defers) {
            return self.finish_order_native(kind, items, keys, reverse, cont);
        }
        let n = items.len();
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
    ) -> Result<(), RuntimeError> {
        match kind {
            OrdKind::Extreme { want_min, who } => {
                let want =
                    if want_min { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater };
                let sym = if want_min { "<" } else { ">" };
                let mut best = 0;
                if items.is_empty() {
                    return Err(self.err(format!("{who}() arg is an empty sequence")));
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
            kind => {
                let sorted = self.wrap(crate::builtins::sort_by_keys(items, &keys, reverse))?;
                self.finish_order(kind, sorted, cont)
            }
        }
    }

    /// Turn a finished ordering into the value it produces.
    fn finish_order(
        &mut self,
        kind: OrdKind,
        out: Vec<Value>,
        cont: OrdCont,
    ) -> Result<(), RuntimeError> {
        let v = match kind {
            OrdKind::SortInPlace(list) => {
                *list.borrow_mut() = out;
                Value::None
            }
            OrdKind::Sort(shape) => self.wrap(Self::rebuild_shape(shape, out))?,
            OrdKind::Extreme { .. } => unreachable!("an extreme produces its winner directly"),
        };
        self.deliver_order(cont, v)
    }

    /// Hand a finished ordering to whatever asked for it.
    fn deliver_order(&mut self, cont: OrdCont, v: Value) -> Result<(), RuntimeError> {
        match cont {
            OrdCont::Push => {
                self.push(v);
                Ok(())
            }
            OrdCont::Seq => {
                self.record_seq_result(v);
                self.drive_seq()
            }
            OrdCont::Sort => {
                self.task.sort_jobs.last_mut().expect("sort job").keys.push(v);
                self.drive_sort()
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
    ) -> Result<Option<Vec<Value>>, RuntimeError> {
        if !matches!(name, "sorted" | "min" | "max") || !ord_needs_vm(&args) {
            return Ok(Some(args));
        }
        let items = match args.as_slice() {
            [it] => self.wrap(iterate_to_vec(it))?,
            _ => args,
        };
        let keys = items.clone();
        let kind = match name {
            "sorted" => OrdKind::Sort(SeqShape::List),
            "min" => OrdKind::Extreme { want_min: true, who: "min" },
            _ => OrdKind::Extreme { want_min: false, who: "max" },
        };
        self.begin_order_to(kind, items, keys, false, cont).map(|()| None)
    }

    /// The ordering machine's loop: run until the ordering is finished, or
    /// until a `<` has to be decided by Oro code. `answer` is the `<` the
    /// previous suspension asked for.
    fn drive_ord(&mut self, mut answer: Option<bool>) -> Result<(), RuntimeError> {
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
        let n = job.items.len();
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
    fn finish_ord_job(&mut self, job: OrdJob) -> Result<(), RuntimeError> {
        match job.state {
            OrdState::Fold { best, .. } => {
                let v = job.items[best].clone();
                self.deliver_order(job.cont, v)
            }
            OrdState::Merge { src, .. } => {
                let mut slots: Vec<Option<Value>> = job.items.into_iter().map(Some).collect();
                let out =
                    src.into_iter().map(|i| slots[i].take().expect("index used once")).collect();
                self.finish_order(job.kind, out, job.cont)
            }
        }
    }

    /// Assemble a class from the member values on the stack (see
    /// [`Op::BuildClass`]) and push it.
    fn build_class(&mut self, spec: &ClassSpec) -> Result<(), RuntimeError> {
        let member_vals = self.popn(spec.members.len());
        let base = if spec.has_base {
            match self.pop() {
                Value::Class(c) => Some(c),
                other => {
                    return Err(self.err(format!(
                        "base of class '{}' must be a class, not '{}'",
                        spec.name,
                        other.type_label()
                    )))
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
    ) -> Result<(), RuntimeError> {
        // The command must be a list of separate strings.
        let list = match args.first() {
            Some(Value::List(l)) => l.borrow().clone(),
            Some(Value::Str(_)) => {
                return Err(self.err(
                    "proc.run() needs a list of separate string arguments, e.g. \
                     [\"git\", \"status\"], not a single string — Oro will not split it (that \
                     would mean reimplementing shell quoting) and there is no shell=True.",
                ))
            }
            _ => return Err(self.err("proc.run() takes a list of strings")),
        };
        if list.is_empty() {
            return Err(self.err("proc.run() got an empty argument list"));
        }
        let mut parts: Vec<String> = Vec::with_capacity(list.len());
        for v in &list {
            match v {
                Value::Str(s) => parts.push(s.s.clone()),
                other => {
                    return Err(self.err(format!(
                        "proc.run() arguments must all be strings, got '{}'",
                        other.type_label()
                    )))
                }
            }
        }
        if parts[0].is_empty() || parts[0].contains(char::is_whitespace) {
            return Err(self.err(format!(
                "proc.run() program '{}' contains whitespace — pass separate arguments \
                 like [\"git\", \"status\"], not one combined string",
                parts[0]
            )));
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
                    _ => return Err(self.err("proc.run() cwd must be a string")),
                },
                "env" => match v {
                    Value::Dict(d) => {
                        let mut pairs = Vec::new();
                        for (ek, ev) in d.borrow().items() {
                            pairs.push((ek.display(), ev.display()));
                        }
                        env = Some(pairs);
                    }
                    _ => return Err(self.err("proc.run() env must be a dict")),
                },
                "timeout" => match v {
                    Value::Int(i) => timeout = Some(*i as f64),
                    Value::Float(f) => timeout = Some(*f),
                    _ => return Err(self.err("proc.run() timeout must be a number")),
                },
                "check" => check = v.truthy(),
                "quiet" => quiet = v.truthy(),
                // CPython's knobs for what Oro now does by default. Name them
                // explicitly rather than let them silently do nothing.
                "capture_output" | "text" => {
                    return Err(self.err(format!(
                        "proc.run() does not take '{k}' — it always captures stdout/stderr as \
                         bytes, and streams them live unless quiet=True. Call `.to_str()` on \
                         one to decode it."
                    )))
                }
                other => {
                    return Err(self.err(format!(
                        "proc.run() got an unexpected keyword argument '{other}'"
                    )))
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
                return Err(self.err(format!(
                    "command timed out after {} seconds",
                    timeout.unwrap_or(0.0)
                )))
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
            return Err(self.err(format!(
                "command failed: {} exited with code {returncode}{detail}",
                parts.join(" "),
            )));
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
    fn import_module(&mut self, path: &str) -> Result<Step, RuntimeError> {
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
        let source = if let Some(src) = stdlib::source_for(path) {
            src.to_string()
        } else {
            // Resolve `a.b.c` to `<root>/a/b/c.oro`.
            let mut file = self.import_root.clone();
            for seg in path.split('.') {
                file.push(seg);
            }
            file.set_extension("oro");

            match std::fs::read_to_string(&file) {
                Ok(s) => s,
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
        let code = match compile_source(&source) {
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
    fn normalize_raise(&mut self, v: Value) -> Result<Value, RuntimeError> {
        match v {
            Value::Class(c) if c.is_exception => {
                Ok(self.make_exception_instance(c, Vec::new()))
            }
            Value::Instance(ref i) if i.class.is_exception => Ok(v),
            other => Err(self.err(format!(
                "exceptions must derive from BaseException, not '{}'",
                other.type_label()
            ))),
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
    fn exc_matches(&self, exc: &Value, class: &Value) -> Result<bool, RuntimeError> {
        let cls = match class {
            Value::Class(c) if c.is_exception => c,
            other => {
                return Err(self.err(format!(
                    "catching classes that do not inherit from BaseException is not allowed \
                     (got '{}')",
                    other.type_label()
                )))
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
    /// Recognise the `sys.exit` sentinel error and turn it into a `SystemExit`
    /// exception. It unwinds like any exception, so a user `except SystemExit`
    /// can still cancel the exit; only if uncaught does it set the exit code.
    fn exit_request(&mut self, e: &RuntimeError) -> Option<Value> {
        let rest = e.message.strip_prefix("\u{0}exit\u{0}")?;
        let code: i32 = rest.parse().unwrap_or(0);
        let class = self.excs["SystemExit"].clone();
        Some(self.make_exception_instance(class, vec![Value::Int(code as i64)]))
    }

    fn error_to_exception(&self, e: &RuntimeError) -> Value {
        let kind = classify_error(&e.message);
        let class = self.excs[kind].clone();
        // A KeyError's message is the missing key's repr, not a sentence, so
        // str(KeyError) matches CPython ("'z'").
        let msg = match kind {
            "KeyError" => e.message.strip_prefix("key error: ").unwrap_or(&e.message).to_string(),
            _ => e.message.clone(),
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
    fn do_return(&mut self, value: Value) -> Result<Step, RuntimeError> {
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
                    return Err(self.err("__init__() should return None".to_string()));
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
            ReturnAction::DriveSort => {
                self.task.sort_jobs.last_mut().expect("sort job").keys.push(value);
                self.drive_sort()?;
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
    fn generator_stop(&mut self) -> Result<Step, RuntimeError> {
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
            sort_jobs: self.task.sort_jobs.len() as u32,
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
        self.task.sort_jobs.truncate(d.sort_jobs as usize);
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
    fn unwind(&mut self, exc: Value) -> Option<Value> {
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
                        return Some(exc);
                    }
                }
            }
        }
    }

    /// Format an uncaught exception as `TypeName: message` at the current line.
    fn uncaught_error(&self, exc: &Value) -> RuntimeError {
        let (name, msg) = match exc {
            Value::Instance(i) => {
                (i.class.name.to_string(), crate::value::exception_message(i))
            }
            other => ("Exception".to_string(), other.display()),
        };
        let message = if msg.is_empty() { name } else { format!("{name}: {msg}") };
        let (line, col) = (self.task.line as usize, self.task.col as usize);
        RuntimeError { message, line, col }
    }

    /// Bind arguments to a fresh frame's slots and cells.
    ///
    /// Two paths, as the spec calls out: the **static** path fills positional
    /// parameters straight into their numbered slots; the **dynamic** path is
    /// taken when keyword arguments are present or the function has `*args` /
    /// `**kwargs`, where some argument names are only known at runtime and must
    /// be matched by name against the parameter list.
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
    ) -> Result<Frame, RuntimeError> {
        let mut frame = self.take_frame(func.code.clone(), &func.freevars);
        let code = &func.code;

        // --- The static path ---------------------------------------------
        //
        // No keywords, no `*args`/`**kwargs`, and every parameter filled by a
        // positional argument or its own default. That is the overwhelming
        // majority of calls, and it needs no name matching at all — so it needs
        // no allocation either. The dynamic path below builds three vectors
        // (the normal-parameter list, the fill slots, the leftovers) before it
        // can bind anything; on a call-heavy program that dominated.
        let supplied = args.len() + usize::from(receiver.is_some());
        let first_defaulted = code.params.len().saturating_sub(func.defaults.len());
        if kwargs.is_empty()
            && code.simple_params
            && supplied <= code.params.len()
            && supplied >= first_defaulted
        {
            let mut values = receiver.into_iter().chain(args);
            for p in &code.params[..supplied] {
                let v = values.next().expect("supplied counts the values exactly");
                store_param(&mut frame, p.target, v);
            }
            for (i, p) in code.params.iter().enumerate().skip(supplied) {
                let v = func.defaults[i - first_defaulted].clone();
                store_param(&mut frame, p.target, v);
            }
            return Ok(frame);
        }

        // --- The dynamic path ---------------------------------------------
        let mut args = args;
        if let Some(receiver) = receiver {
            // Only now is the combined vector worth building: this path has to
            // match names against it anyway.
            args.insert(0, receiver);
        }

        let normal: Vec<&ParamInfo> =
            code.params.iter().filter(|p| p.kind == crate::ast::ParamKind::Normal).collect();
        let var_param = code.params.iter().find(|p| p.kind == crate::ast::ParamKind::VarArgs);
        let kw_param = code.params.iter().find(|p| p.kind == crate::ast::ParamKind::KwArgs);

        // Slots for normal params, filled as we go (None = still missing).
        let mut filled: Vec<Option<Value>> = vec![None; normal.len()];

        // 1. Positional arguments fill normal params left to right.
        if args.len() > normal.len() && var_param.is_none() {
            return Err(self.err(format!(
                "{}() takes {} positional argument{} but {} were given",
                code.name,
                normal.len(),
                if normal.len() == 1 { "" } else { "s" },
                args.len()
            )));
        }
        let mut extra_positional = Vec::new();
        for (i, a) in args.into_iter().enumerate() {
            if i < normal.len() {
                filled[i] = Some(a);
            } else {
                extra_positional.push(a);
            }
        }

        // 2. Keyword arguments: match by name, else collect for **kwargs.
        let mut extra_kw = OroDict::new();
        for (name, value) in kwargs {
            if let Some(pos) = normal.iter().position(|p| *p.name == name) {
                if filled[pos].is_some() {
                    return Err(self.err(format!(
                        "{}() got multiple values for argument '{name}'",
                        code.name
                    )));
                }
                filled[pos] = Some(value);
            } else if kw_param.is_some() {
                self.wrap(extra_kw.insert(Value::str(name), value))?;
            } else {
                return Err(self.err(format!(
                    "{}() got an unexpected keyword argument '{name}'",
                    code.name
                )));
            }
        }

        // 3. Defaults fill any remaining normal params; error if none.
        //    `func.defaults` aligns with the trailing defaulted params.
        let n_defaults = func.defaults.len();
        let first_defaulted = normal.len() - n_defaults;
        for (i, slot) in filled.iter_mut().enumerate() {
            if slot.is_none() {
                if i >= first_defaulted {
                    *slot = Some(func.defaults[i - first_defaulted].clone());
                } else {
                    return Err(self.err(format!(
                        "{}() missing required argument: '{}'",
                        code.name, normal[i].name
                    )));
                }
            }
        }

        // 4. Write bound values into the frame via each parameter's target.
        for (p, value) in normal.iter().zip(filled) {
            store_param(&mut frame, p.target, value.expect("all normal params filled"));
        }
        if let Some(p) = var_param {
            store_param(&mut frame, p.target, Value::Tuple(OroTuple::new(extra_positional)));
        }
        if let Some(p) = kw_param {
            store_param(&mut frame, p.target, Value::Dict(Rc::new(RefCell::new(extra_kw))));
        }

        Ok(frame)
    }
}

fn store_param(frame: &mut Frame, target: VarTarget, value: Value) {
    match target {
        VarTarget::Local(s) => frame.locals[s as usize] = value,
        VarTarget::Cell(s) => *frame.cells[s as usize].borrow_mut() = value,
    }
}

// --- Iteration --------------------------------------------------------------

fn get_iter(v: &Value) -> Result<Value, String> {
    let state = match v {
        Value::Range(r) => IterState::Range { cur: r.start, stop: r.stop, step: r.step },
        Value::List(l) => {
            IterState::List { list: l.clone(), idx: 0, orig_len: l.borrow().len() }
        }
        Value::Tuple(t) => IterState::Tuple { tuple: t.clone(), idx: 0 },
        Value::Str(s) => {
            let chars = s.s.chars().map(|c| c.to_string()).collect();
            IterState::Str { chars, idx: 0 }
        }
        Value::Bytes(b) => IterState::Bytes { bytes: b.clone(), idx: 0 },
        Value::Dict(d) => IterState::Snapshot { items: d.borrow().keys(), idx: 0 },
        // A generator is its own iterator; ForIter resumes it directly.
        Value::Generator(_) => return Ok(v.clone()),
        // So is a channel: `ForIter` recvs from it (and may park).
        Value::Channel(_) => return Ok(v.clone()),
        Value::Iter(_) => return Ok(v.clone()),
        other => return Err(format!("'{}' object is not iterable", other.type_name())),
    };
    Ok(Value::Iter(Rc::new(RefCell::new(state))))
}

fn iter_next(it: &Value) -> Result<Option<Value>, String> {
    let it = match it {
        Value::Iter(i) => i,
        _ => return Err("internal: ForIter target is not an iterator".to_string()),
    };
    let mut st = it.borrow_mut();
    match &mut *st {
        IterState::Range { cur, stop, step } => {
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
                return Err("list changed size during iteration".to_string());
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
        IterState::Snapshot { items, idx } => {
            if *idx < items.len() {
                let v = items[*idx].clone();
                *idx += 1;
                Ok(Some(v))
            } else {
                Ok(None)
            }
        }
    }
}

/// Collect every element of an iterable into a vector (for unpacking, `*args`
/// spreading, and `**` merging).
pub fn iterate_to_vec(v: &Value) -> Result<Vec<Value>, String> {
    // Draining a channel means blocking, and blocking means parking, which a
    // native helper cannot do — the same rule that stops a builtin from
    // draining a generator. `for msg in ch` is the way.
    if matches!(v, Value::Channel(_)) {
        return Err("'Channel' object is not iterable here: receiving may block, so `for msg in \
                    ch` is the only way to drain one"
            .to_string());
    }
    let it = get_iter(v)?;
    let mut out = Vec::new();
    while let Some(x) = iter_next(&it)? {
        out.push(x);
    }
    Ok(out)
}

fn dict_pairs(v: &Value) -> Result<Vec<(Value, Value)>, String> {
    match v {
        Value::Dict(d) => Ok(d.borrow().items().to_vec()),
        other => Err(format!("argument after ** must be a mapping, not '{}'", other.type_name())),
    }
}

// --- Indexing and slicing ---------------------------------------------------

fn as_index(v: &Value) -> Result<i64, String> {
    match v {
        Value::Bool(b) => Ok(*b as i64),
        Value::Int(i) => Ok(*i),
        other => Err(format!("indices must be integers, not '{}'", other.type_name())),
    }
}

/// Resolve a possibly-negative index against `len`, returning the non-negative
/// position or an out-of-range error.
fn resolve_index(idx: i64, len: usize, kind: &str) -> Result<usize, String> {
    let adj = if idx < 0 { idx + len as i64 } else { idx };
    if adj < 0 || adj as usize >= len {
        Err(format!("{kind} index out of range"))
    } else {
        Ok(adj as usize)
    }
}

fn subscript_get(obj: &Value, index: &Value) -> Result<Value, String> {
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
            None => Err(format!("key error: {}", index.repr())),
        },
        other => Err(format!("'{}' object is not subscriptable", other.type_name())),
    }
}

fn subscript_set(obj: &Value, index: &Value, value: Value) -> Result<(), String> {
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
            Err(format!("'{}' object does not support item assignment", other.type_name()))
        }
    }
}

fn slice_get(
    obj: &Value,
    lower: &Value,
    upper: &Value,
    step: &Value,
) -> Result<Value, String> {
    let opt = |v: &Value| -> Result<Option<i64>, String> {
        match v {
            Value::None => Ok(None),
            other => Ok(Some(as_index(other)?)),
        }
    };
    let (lo, hi, st) = (opt(lower)?, opt(upper)?, opt(step)?);
    let step = st.unwrap_or(1);
    if step == 0 {
        return Err("slice step cannot be zero".to_string());
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
        other => Err(format!("'{}' object is not sliceable", other.type_name())),
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

/// Attribute read for any value. Instances, classes, and `super` proxies are
/// handled here (no `__getattr__` hook exists, so this never runs Oro code);
/// everything else falls back to builtin-method binding.
fn get_attr(obj: &Value, name: &Rc<str>) -> Result<Value, String> {
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
                None => Err(format!("'{}' object has no attribute '{}'", inst.class.name, key)),
            }
        }
        Value::Class(class) => match Class::find(class, key) {
            // A method accessed on the class itself stays an unbound function.
            Some((member, _)) => Ok(member),
            None => Err(format!("type object '{}' has no attribute '{}'", class.name, key)),
        },
        Value::Module(m) => match m.members.borrow().get(key) {
            Some(v) => Ok(v.clone()),
            None => Err(format!("module '{}' has no attribute '{}'", m.name, key)),
        },
        Value::Super(sp) => {
            let mut cur = sp.start.clone();
            while let Some(c) = cur {
                if let Some(member) = c.members.borrow().get(key).cloned() {
                    return Ok(bind_member(member, sp.instance.clone(), c.clone()));
                }
                cur = c.base.clone();
            }
            Err(format!("'super' object has no attribute '{key}'"))
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
                Err(msg.to_string())
            } else {
                Err(format!("'{}' object has no attribute '{}'", obj.type_name(), key))
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
fn compile_source(source: &str) -> Result<Rc<CodeObject>, String> {
    let tokens = crate::lexer::Lexer::new(source).tokenize().map_err(|e| e.to_string())?;
    let program = crate::parser::Parser::new(tokens).parse().map_err(|e| e.message.clone())?;
    crate::compiler::compile(&program).map_err(|e| e.message.clone())
}

/// Map an internal error message to the CPython exception type it should raise.
/// Every message here is produced by this crate, so the matching is reliable.
fn classify_error(msg: &str) -> &'static str {
    let m = msg;
    // Socket errors carry an errno and map to CPython's `ConnectionError`
    // subclasses; `crate::net` owns that table because it owns the messages.
    if let Some(kind) = crate::net::classify(m) {
        return kind;
    }
    // Likewise the JSON codec: a format's diagnostics belong to the format, not
    // to this table.
    if let Some(kind) = crate::json::classify(m) {
        return kind;
    }
    // Order matters: check the more specific substrings first.
    if m.starts_with("command failed:") {
        "CommandError"
    } else if m.contains("No such file or directory") {
        "FileNotFoundError"
    } else if m.contains("Permission denied") {
        "PermissionError"
    } else if m.contains("timed out") {
        "TimeoutError"
    } else if m.contains("File exists") || m.starts_with("[Errno") {
        "OSError"
    } else if m.contains("division by zero")
        || m.contains("modulo by zero")
        || m.contains("division or modulo by zero")
    {
        "ZeroDivisionError"
    } else if m.contains("index out of range") || m.contains("pop from empty list") {
        "IndexError"
    } else if m.starts_with("key error:") || m.contains("KeyError") {
        "KeyError"
    } else if m.contains("is not defined") {
        "NameError"
    } else if m.contains("has no attribute")
        // A removed method: the message names the replacement, but it is still
        // an attribute that is not there, and still catchable as one.
        || m.contains("is not in Oro —")
        || m.contains("is spelled")
    {
        "AttributeError"
    } else if m.contains("arg not in range")
        || m.contains("values to unpack")
        || m.contains("could not convert string to float")
        || m.contains("could not be decoded as UTF-8")
        || m.starts_with("invalid literal for int")
        || m.contains("empty separator")
        || m.contains("step")
        || m.contains("arg is an empty sequence")
        || m.contains("expected at least")
        // Stream faults. CPython answers ValueError for a bad mode, for an
        // operation on a closed file, and (via io.UnsupportedOperation, a
        // ValueError subclass) for reading a writer.
        || m.contains("invalid file mode")
        || m.contains("must be in range(0, 256)")
        || m.contains("must be an int, not")
        || m.contains("must be at least")
        || m.contains("must not be empty")
        || m.contains("found no delimiter")
        || m.contains("on a closed ")
        || m.contains("on a stream open for")
        // `strip(side="middle")`. The quote is what separates it from
        // `side must be str`, which is a TypeError like every other bad type.
        || m.contains("side must be \"")
    {
        "ValueError"
    } else if m.contains("expected a character")
        || m.contains("unsupported operand")
        || m.contains("not callable")
        || m.contains("not iterable")
        || m.contains("not a mapping")
        || m.contains("must be a mapping")
        || m.contains("has no len()")
        || m.contains("unhashable type")
        || m.contains("bad operand type")
        || m.contains("argument must be")
        || m.contains("must be str")
        || m.contains("requires string")
        || m.contains("as left operand")
        || m.contains("not supported between")
        || m.contains("takes")
        || m.contains("missing a required argument")
        || m.contains("object is not")
        || m.contains("unexpected keyword argument")
    {
        "TypeError"
    } else {
        // A genuine internal/uncategorised failure.
        "RuntimeError"
    }
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

/// The dunder method name for a binary-arithmetic opcode.
fn arith_dunder(op: &Op) -> &'static str {
    match op {
        Op::BinAdd => "__add__",
        Op::BinSub => "__sub__",
        Op::BinMul => "__mul__",
        Op::BinDiv => "__truediv__",
        Op::BinFloorDiv => "__floordiv__",
        Op::BinMod => "__mod__",
        Op::BinPow => "__pow__",
        _ => unreachable!("arith_dunder on a non-arithmetic op"),
    }
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
fn try_compare_op(op: CmpOp, a: &Value, b: &Value) -> Result<Option<bool>, String> {
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

/// `sorted()` builds a new list; `list.sort()` writes back where it was.
fn sort_kind(in_place: Option<Rc<OroList>>) -> OrdKind {
    match in_place {
        Some(l) => OrdKind::SortInPlace(l),
        None => OrdKind::Sort(SeqShape::List),
    }
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
fn membership_items(container: &Value, item: &Value) -> Result<Vec<Value>, String> {
    match container {
        Value::List(l) => Ok(l.borrow().clone()),
        Value::Tuple(t) => Ok((**t).clone()),
        other => Err(format!(
            "internal: {} membership does not dispatch (item {})",
            other.type_name(),
            item.type_name()
        )),
    }
}

fn try_contains(container: &Value, item: &Value) -> Result<Option<bool>, String> {
    match container {
        Value::Str(hay) => match item {
            Value::Str(needle) => Ok(Some(hay.s.contains(&needle.s))),
            _ => Err("'in <string>' requires string as left operand".to_string()),
        },
        // Subsequence, like `str`. CPython also lets an `int` on the left ask
        // whether one octet is present; that is a second meaning for one
        // spelling, so Oro says what it wants instead of guessing.
        Value::Bytes(hay) => match item {
            Value::Bytes(needle) => Ok(Some(subsequence(hay, needle))),
            _ => Err("'in <bytes>' requires bytes as left operand".to_string()),
        },
        Value::List(l) => Ok(seq_contains(&l.borrow(), item)),
        Value::Tuple(t) => Ok(seq_contains(t, item)),
        // A dict key is an `HKey` and a lookup has nowhere to call user code
        // from, which is the decision `docs/hash-and-equality.md` argues at
        // length: a class defining `__eq__` is not a key at all, so `in` over a
        // dict never has one to dispatch.
        Value::Dict(d) => d.borrow().contains(item).map(Some),
        Value::Range(r) => Ok(Some(range_contains(r, item))),
        other => Err(format!("argument of type '{}' is not iterable", other.type_name())),
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
