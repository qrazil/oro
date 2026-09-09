//! The Oro virtual machine: one flat interpreter loop over a stack of heap
//! [`Frame`]s (architecture point 1).
//!
//! **Calling an Oro function never recurses in Rust.** A call pushes a new
//! [`Frame`] onto `frames` and the same loop keeps turning; `Return` pops the
//! frame and hands the value back to the caller's operand stack. This is what
//! makes 5000-deep recursion (and, later, generators/coroutines) possible
//! without growing the native stack.

pub mod arith;
mod exceptions;
pub mod modules;
mod stdlib;

use std::cell::RefCell;
use std::rc::Rc;

use crate::ast::CmpOp;
use crate::compiler::{CaptureSource, ClassSpec, CodeObject, Op, ParamInfo, VarTarget};
use crate::value::{
    BoundMethod, Class, Function, Instance, IterState, MethodKind, OroDict, RangeVal,
    SuperProxy, Value,
};
use std::collections::HashMap;

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
#[derive(Clone, Copy, Default)]
struct JobDepths {
    prints: usize,
    str_jobs: usize,
    sort_jobs: usize,
    seq_jobs: usize,
    mat_jobs: usize,
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
    /// The top-level frame returned; the run is over.
    Done(Value),
    /// Raise this exception value (unwind the block/frame stack).
    Raise(Value),
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
    in_place: Option<Rc<RefCell<Vec<Value>>>>,
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
    /// Exceptions currently being handled (top = innermost), for bare `raise`.
    handling: Vec<Value>,
    /// Why each in-flight `finally` body is running, so `EndFinally` can resume
    /// the exception or `return` that was suspended to run the cleanup.
    finally_why: Vec<Why>,
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
            handling: Vec::new(),
            finally_why: Vec::new(),
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
    /// The module frame's locals, captured when the top-level frame returns.
    /// Written exactly once (at program end); used only by tests.
    last_locals: Vec<Value>,
    /// The built-in exception classes, by name (shared identity for the run).
    excs: HashMap<&'static str, Rc<Class>>,
    /// Generators currently being advanced (innermost on top), with the
    /// `ForIter` target to jump to when each is exhausted. Pushed on resume,
    /// popped on `yield`/exhaustion.
    gen_stack: Vec<(Rc<RefCell<crate::value::GenBox>>, GenDriver)>,
    /// Program arguments, exposed as `sys.argv`.
    argv: Vec<String>,
    /// Set when `sys.exit(code)` runs; becomes the process exit status.
    exit_code: Option<i32>,
    /// Directory user modules are resolved against — the single search-path
    /// rule (the main script's directory). No runtime mutation.
    import_root: std::path::PathBuf,
    /// Imported user modules by dotted path (module identity), run once.
    module_cache: HashMap<String, Value>,
    /// Modules whose bodies are currently running, to detect circular imports.
    importing: std::collections::HashSet<String>,
    /// The class of `proc.run`'s result (a `Completed`).
    proc_class: Rc<Class>,
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
    match vm.run_loop() {
        Ok(_) => Ok(vm.exit_code.unwrap_or(0)),
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
            last_locals: Vec::new(),
            excs: exceptions::build_registry(),
            gen_stack: Vec::new(),
            argv,
            exit_code: None,
            import_root: std::path::PathBuf::from("."),
            module_cache: HashMap::new(),
            importing: std::collections::HashSet::new(),
            proc_class: Rc::new(Class {
                name: Rc::from("Completed"),
                base: None,
                members: RefCell::new(HashMap::new()),
                is_exception: false,
            }),
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
    fn expect_list_tos(&mut self, who: &str) -> Result<Rc<RefCell<Vec<Value>>>, RuntimeError> {
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

    fn run_loop(&mut self) -> Result<Value, RuntimeError> {
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

            // Execute one op. A failing operation or a `raise` produces an
            // exception that unwinds the block/frame stack; if nothing catches
            // it, the run ends with that error.
            let to_raise = match self.step(op) {
                Ok(Step::Next) => continue,
                Ok(Step::Done(v)) => return Ok(v),
                Ok(Step::Raise(exc)) => exc,
                Err(e) => match self.exit_request(&e) {
                    Some(exc) => exc,
                    None => self.error_to_exception(&e),
                },
            };
            if let Some(uncaught) = self.unwind(to_raise) {
                return Err(uncaught);
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
                    // An instance may define a rich-comparison dunder; if so it
                    // is dispatched and produces the result via its return.
                    if !(matches!(a, Value::Instance(_)) && self.try_compare_dunder(cmp, &a, &b)?) {
                        let r = self.wrap(compare(cmp, &a, &b))?;
                        self.push(Value::Bool(r));
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
                    self.push(Value::List(Rc::new(RefCell::new(items))));
                }
                Op::BuildTuple(n) => {
                    let items = self.popn(n as usize);
                    self.push(Value::Tuple(Rc::new(items)));
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
                    // A generator is advanced by resuming its frame; the value
                    // (or exhaustion) arrives via Yield/Return, not inline.
                    if let Value::Generator(gen) = &it {
                        let frame = {
                            let mut g = gen.borrow_mut();
                            if g.done {
                                None
                            } else {
                                g.frame.take().map(|b| *b.downcast::<Frame>().expect("gen frame"))
                            }
                        };
                        match frame {
                            Some(frame) => {
                                self.gen_stack.push((gen.clone(), GenDriver::ForLoop(target)));
                                self.task.frames.push(frame);
                            }
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
                Op::Call(n) => self.do_call(n as usize)?,
                Op::CallEx => self.do_call_ex()?,
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
                    let (gen, driver) = self.gen_stack.pop().expect("yield outside a generator");
                    gen.borrow_mut().frame = Some(Box::new(frame));
                    match driver {
                        GenDriver::ForLoop(_) => self.push(value),
                        GenDriver::Materialize => {
                            self.task.mat_jobs.last_mut().expect("materialise job").items.push(value);
                            self.drive_materialize()?;
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

    fn do_call(&mut self, n: usize) -> Result<(), RuntimeError> {
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
    fn call_fast(&mut self, func: Rc<Function>, n: usize) -> Result<(), RuntimeError> {
        if self.task.frames.len() >= MAX_FRAMES {
            return Err(self.err("maximum recursion depth exceeded"));
        }
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
        Ok(())
    }

    fn do_call_ex(&mut self) -> Result<(), RuntimeError> {
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
    ) -> Result<(), RuntimeError> {
        match callee {
            Value::Builtin(b) => {
                // A few builtins may need to run an Oro dunder (which must go
                // through a frame, not a Rust re-entry), so they are handled in
                // the VM rather than as pure native functions.
                match b.name {
                    "print" => return self.do_print(args, kwargs),
                    // sorted(key=…) has to call Oro code, so it is driven from
                    // the VM rather than run as a pure native builtin.
                    "sorted" if !kwargs.is_empty() => return self.do_sorted(args, kwargs),
                    // proc.run is finished here so it can take keyword args and
                    // build a Completed instance.
                    "proc.run" => return self.do_proc_run(args, kwargs),
                    "str" if matches!(args.first(), Some(Value::Instance(_))) && args.len() == 1 => {
                        return self.stringify_instance(args.into_iter().next().unwrap(), false);
                    }
                    "repr" if matches!(args.first(), Some(Value::Instance(_))) && args.len() == 1 => {
                        return self.stringify_instance(args.into_iter().next().unwrap(), true);
                    }
                    // str()/repr() of a container render elements' __repr__ (and
                    // are cycle-safe), which needs VM dispatch, not native repr.
                    "str" | "repr" if args.len() == 1 && is_container(&args[0]) => {
                        return self.begin_stringify(args.into_iter().next().unwrap(), StrCont::Push);
                    }
                    "len" if matches!(args.first(), Some(Value::Instance(_))) && args.len() == 1 => {
                        return self.dunder_len(args.into_iter().next().unwrap());
                    }
                    _ => {}
                }
                // A generator argument must be drained through frames first.
                if self.materialize_generator_args(&Value::Builtin(b.clone()), &args, &kwargs)? {
                    return Ok(());
                }
                if !kwargs.is_empty() {
                    return Err(self.err(format!("{}() takes no keyword arguments", b.name)));
                }
                let r = self.wrap((b.func)(args))?;
                self.push(r);
                Ok(())
            }
            Value::Method(m) => match &m.kind {
                MethodKind::Native(name) => {
                    // `to_str` may need to run a user `__str__`, or render a
                    // container's elements through their `__repr__`; both go
                    // through frames, so they cannot run as native methods.
                    if &**name == "to_str" && args.is_empty() && kwargs.is_empty() {
                        if matches!(m.receiver, Value::Instance(_)) {
                            return self.stringify_instance(m.receiver.clone(), false);
                        }
                        if is_container(&m.receiver) {
                            return self.begin_stringify(m.receiver.clone(), StrCont::Push);
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
                            if self.materialize_receiver(&callee, with_recv, kwargs.clone())? {
                                return Ok(());
                            }
                        }
                        return self.do_seq_op(op, &m.receiver, args, kwargs);
                    }
                    // list.sort(key=…, reverse=…) shares sorted()'s frame-driven
                    // key machinery; it just writes back in place.
                    if &**name == "sort" && !kwargs.is_empty() {
                        if let Value::List(l) = &m.receiver {
                            if !args.is_empty() {
                                return Err(self.err("sort() takes no positional arguments"));
                            }
                            let (keyfn, reverse) = self.sort_kwargs("sort", kwargs)?;
                            let items = l.borrow().clone();
                            return self.begin_sort(items, keyfn, reverse, Some(l.clone()));
                        }
                    }
                    if self.materialize_generator_args(&Value::Method(m.clone()), &args, &kwargs)? {
                        return Ok(());
                    }
                    if !kwargs.is_empty() {
                        return Err(self.err("methods take no keyword arguments in this build"));
                    }
                    let r = self.wrap(crate::builtins::call_method(&m.receiver, name, args))?;
                    self.push(r);
                    Ok(())
                }
                MethodKind::User { func, defclass } => self.invoke_user(
                    func.clone(),
                    m.receiver.clone(),
                    defclass.clone(),
                    args,
                    kwargs,
                    ReturnAction::Normal,
                ),
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
                Ok(())
            }
            Value::Class(class) => self.instantiate(class, args, kwargs),
            other => Err(self.err(format!("'{}' object is not callable", other.type_label()))),
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
            fields: RefCell::new(HashMap::new()),
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
                    .map(|(k, v)| Value::Tuple(Rc::new(vec![k.clone(), v.clone()])))
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
                    let r = self.wrap((b.func)(call_args))?;
                    self.record_seq_result(r);
                }
                Value::Method(m) => {
                    let r = match &m.kind {
                        MethodKind::Native(name) => {
                            self.wrap(crate::builtins::call_method(&m.receiver, name, call_args))?
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
                let want_min = op == SeqOp::MinBy;
                let mut best: Option<(usize, &Value)> = None;
                for (i, key) in results.iter().enumerate() {
                    match best {
                        None => best = Some((i, key)),
                        Some((_, bk)) => {
                            let ord = self.wrap(key.compare(bk))?;
                            let take = if want_min {
                                ord == std::cmp::Ordering::Less
                            } else {
                                ord == std::cmp::Ordering::Greater
                            };
                            if take {
                                best = Some((i, key));
                            }
                        }
                    }
                }
                let out = match best {
                    Some((i, _)) => items[i].clone(),
                    None => {
                        return Err(self.err(format!("{}() arg is an empty sequence", op.name())))
                    }
                };
                self.push(out);
                return Ok(());
            }
            SeqOp::GroupBy => {
                let mut d = crate::value::OroDict::new();
                for (item, key) in items.iter().zip(results.iter()) {
                    let bucket = match self.wrap(d.get(key))? {
                        Some(Value::List(l)) => l,
                        _ => {
                            let l = Rc::new(RefCell::new(Vec::new()));
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
                self.push(Value::Tuple(Rc::new(pair)));
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
            SeqOp::SortBy => self.wrap(crate::builtins::sort_by_keys(items, &results, false))?,
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
            SeqShape::List => Value::List(Rc::new(RefCell::new(items))),
            SeqShape::Tuple => Value::Tuple(Rc::new(items)),
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
    ) -> Result<bool, RuntimeError> {
        if !args.iter().any(|a| matches!(a, Value::Generator(_))) {
            return Ok(false);
        }
        self.task.mat_jobs.push(MatJob {
            callee: callee.clone(),
            args: args.to_vec(),
            kwargs: kwargs.to_vec(),
            idx: 0,
            items: Vec::new(),
            receiver_in_args: false,
        });
        self.drive_materialize()?;
        Ok(true)
    }

    /// Drain a generator that is a method *receiver* (`g().map(f)`), then retry
    /// the call with the resulting list as the receiver.
    fn materialize_receiver(
        &mut self,
        callee: &Value,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<bool, RuntimeError> {
        if !matches!(args.first(), Some(Value::Generator(_))) {
            return Ok(false);
        }
        self.task.mat_jobs.push(MatJob {
            callee: callee.clone(),
            args,
            kwargs,
            idx: 0,
            items: Vec::new(),
            receiver_in_args: true,
        });
        self.drive_materialize()?;
        Ok(true)
    }

    /// Advance the active materialisation job: resume the generator being
    /// drained, move to the next generator argument, or — when none are left —
    /// pop the job and retry the original call with lists in their place.
    fn drive_materialize(&mut self) -> Result<(), RuntimeError> {
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
            let frame = {
                let mut g = gen.borrow_mut();
                if g.done {
                    None
                } else {
                    g.frame.take().map(|b| *b.downcast::<Frame>().expect("gen frame"))
                }
            };
            match frame {
                Some(frame) => {
                    if self.task.frames.len() >= MAX_FRAMES {
                        return Err(self.err("maximum recursion depth exceeded"));
                    }
                    self.gen_stack.push((gen, GenDriver::Materialize));
                    self.task.frames.push(frame);
                    return Ok(());
                }
                None => {
                    // Already exhausted: it contributes whatever was collected.
                    let job = self.task.mat_jobs.last_mut().expect("materialise job");
                    let items = std::mem::take(&mut job.items);
                    let idx = job.idx;
                    job.args[idx] = Value::List(Rc::new(RefCell::new(items)));
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
    ) -> Result<(), RuntimeError> {
        // sorted() is intercepted before the generic builtin path, so the
        // generator drain has to be requested explicitly here too.
        if let Some(callee) = crate::builtins::lookup("sorted") {
            if self.materialize_generator_args(&callee, &args, &kwargs)? {
                return Ok(());
            }
        }
        let iterable = match args.as_slice() {
            [it] => it.clone(),
            _ => return Err(self.err("sorted() takes exactly 1 positional argument")),
        };
        let (keyfn, reverse) = self.sort_kwargs("sorted", kwargs)?;
        let items = self.wrap(crate::vm::iterate_to_vec(&iterable))?;
        self.begin_sort(items, keyfn, reverse, None)
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
        in_place: Option<Rc<RefCell<Vec<Value>>>>,
    ) -> Result<(), RuntimeError> {
        let keyfn = match keyfn {
            Some(f) => f,
            None => {
                // No key: the elements are their own keys.
                let keys = items.clone();
                let sorted = self.wrap(crate::builtins::sort_by_keys(items, &keys, reverse))?;
                return self.finish_sort(sorted, in_place);
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
                    let sorted =
                        self.wrap(crate::builtins::sort_by_keys(job.items, &job.keys, job.reverse))?;
                    return self.finish_sort(sorted, job.in_place);
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
                    let key = self.wrap((b.func)(vec![item]))?;
                    self.task.sort_jobs.last_mut().unwrap().keys.push(key);
                }
                Value::Method(m) => {
                    let key = match &m.kind {
                        MethodKind::Native(name) => self
                            .wrap(crate::builtins::call_method(&m.receiver, name, vec![item]))?,
                        _ => return Err(self.err("sort key must be a plain function")),
                    };
                    self.task.sort_jobs.last_mut().unwrap().keys.push(key);
                }
                _ => return Err(self.err("sort key is not callable")),
            }
        }
    }

    fn finish_sort(
        &mut self,
        sorted: Vec<Value>,
        in_place: Option<Rc<RefCell<Vec<Value>>>>,
    ) -> Result<(), RuntimeError> {
        match in_place {
            Some(list) => {
                *list.borrow_mut() = sorted;
                self.push(Value::None);
            }
            None => self.push(Value::List(Rc::new(RefCell::new(sorted)))),
        }
        Ok(())
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

    /// Dispatch a rich-comparison dunder for an instance `a`. Returns `true`
    /// (and pushes a frame) when one was found; `false` to fall back to the
    /// default comparison.
    fn try_compare_dunder(&mut self, cmp: CmpOp, a: &Value, b: &Value) -> Result<bool, RuntimeError> {
        let name = match cmp {
            CmpOp::Eq => "__eq__",
            CmpOp::NotEq => "__ne__",
            CmpOp::Lt => "__lt__",
            CmpOp::Gt => "__gt__",
            CmpOp::LtEq => "__le__",
            CmpOp::GtEq => "__ge__",
            // `is`, `in`, and their negations have no rich-comparison dunder.
            _ => return Ok(false),
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
        Ok(false)
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
        let mut members = HashMap::with_capacity(spec.members.len());
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

        let mut fields = HashMap::new();
        fields.insert(Rc::from("returncode"), Value::Int(returncode));
        fields.insert(Rc::from("ok"), Value::Bool(returncode == 0));
        fields.insert(Rc::from("truncated"), Value::Bool(truncated));
        // A child's streams are octets. It may emit a JPEG, or a UTF-8
        // sequence cut in half by the capture limit, and decoding either
        // lossily is how a pipeline quietly corrupts data. Decode with
        // `.to_str()` at the point the program knows it is text.
        fields.insert(Rc::from("stdout"), Value::bytes(output.stdout));
        fields.insert(Rc::from("stderr"), Value::bytes(output.stderr));
        fields.insert(Rc::from("args"), Value::List(Rc::new(RefCell::new(list))));
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
        // A module still initialising means a cycle.
        if self.importing.contains(path) {
            let class = self.excs["ImportError"].clone();
            let msg = Value::str(format!("circular import detected while importing '{path}'"));
            return Ok(Step::Raise(self.make_exception_instance(class, vec![msg])));
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
        self.importing.insert(path.to_string());
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
        self.importing.remove(path.as_ref());
        self.module_cache.insert(path.to_string(), module.clone());
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

    /// Build an exception instance of `class`, storing its args tuple natively.
    fn make_exception_instance(&self, class: Rc<Class>, args: Vec<Value>) -> Value {
        let mut fields = HashMap::new();
        fields.insert(Rc::from("args"), Value::Tuple(Rc::new(args)));
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
        self.make_exception_instance(class, vec![Value::str(msg)])
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
            self.last_locals = frame.locals;
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
        let (gen, driver) = self.gen_stack.pop().expect("generator stop outside a driver");
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
                job.args[idx] = Value::List(Rc::new(RefCell::new(items)));
                self.drive_materialize()?;
            }
        }
        Ok(Step::Next)
    }

    fn job_depths(&self) -> JobDepths {
        JobDepths {
            prints: self.task.prints.len(),
            str_jobs: self.task.str_jobs.len(),
            sort_jobs: self.task.sort_jobs.len(),
            seq_jobs: self.task.seq_jobs.len(),
            mat_jobs: self.task.mat_jobs.len(),
        }
    }

    /// Discard jobs started inside a block that an exception is unwinding out
    /// of. Their driver frames are gone, so nothing will ever complete them.
    fn truncate_jobs(&mut self, d: JobDepths) {
        self.task.prints.truncate(d.prints);
        self.task.str_jobs.truncate(d.str_jobs);
        self.task.sort_jobs.truncate(d.sort_jobs);
        self.task.seq_jobs.truncate(d.seq_jobs);
        self.task.mat_jobs.truncate(d.mat_jobs);
    }

    /// Unwind `exc` through the block and frame stacks. On success (a handler or
    /// finally took over) returns `None` and the loop resumes; if nothing
    /// catches it, returns the uncaught error to end the run.
    fn unwind(&mut self, exc: Value) -> Option<RuntimeError> {
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
                        return Some(self.uncaught_error(&exc));
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
            store_param(&mut frame, p.target, Value::Tuple(Rc::new(extra_positional)));
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
            let chars: Vec<char> = s.s.chars().collect();
            let idxs = slice_indices(chars.len(), lo, hi, step);
            let out: String = idxs.into_iter().map(|i| chars[i]).collect();
            Ok(Value::str(out))
        }
        Value::Bytes(b) => {
            let idxs = slice_indices(b.len(), lo, hi, step);
            Ok(Value::bytes(idxs.into_iter().map(|i| b[i]).collect::<Vec<u8>>()))
        }
        Value::List(l) => {
            let l = l.borrow();
            let idxs = slice_indices(l.len(), lo, hi, step);
            Ok(Value::List(Rc::new(RefCell::new(idxs.into_iter().map(|i| l[i].clone()).collect()))))
        }
        Value::Tuple(t) => {
            let idxs = slice_indices(t.len(), lo, hi, step);
            Ok(Value::Tuple(Rc::new(idxs.into_iter().map(|i| t[i].clone()).collect())))
        }
        other => Err(format!("'{}' object is not sliceable", other.type_name())),
    }
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
fn get_attr(obj: &Value, name: &str) -> Result<Value, String> {
    match obj {
        Value::Instance(inst) => {
            if let Some(v) = inst.fields.borrow().get(name) {
                return Ok(v.clone());
            }
            match Class::find(&inst.class, name) {
                Some((member, defclass)) => Ok(bind_member(member, obj.clone(), defclass)),
                // The conversion methods exist on every value, instances
                // included — `to_str` runs the class's `__str__` if it has one.
                None if crate::builtins::is_cast_method(name) => Ok(Value::Method(Rc::new(
                    BoundMethod { receiver: obj.clone(), kind: MethodKind::Native(Rc::from(name)) },
                ))),
                None => Err(format!("'{}' object has no attribute '{}'", inst.class.name, name)),
            }
        }
        Value::Class(class) => match Class::find(class, name) {
            // A method accessed on the class itself stays an unbound function.
            Some((member, _)) => Ok(member),
            None => Err(format!("type object '{}' has no attribute '{}'", class.name, name)),
        },
        Value::Module(m) => match m.members.borrow().get(name) {
            Some(v) => Ok(v.clone()),
            None => Err(format!("module '{}' has no attribute '{}'", m.name, name)),
        },
        Value::Super(sp) => {
            let mut cur = sp.start.clone();
            while let Some(c) = cur {
                if let Some(member) = c.members.borrow().get(name).cloned() {
                    return Ok(bind_member(member, sp.instance.clone(), c.clone()));
                }
                cur = c.base.clone();
            }
            Err(format!("'super' object has no attribute '{name}'"))
        }
        // A socket's `peer` and `local` are data attributes, not methods
        // (§4): they are strings read once when the socket was opened.
        Value::Stream(s) if s.has_addr_attr(name) => Ok(Value::str(s.addr_attr(name)?)),
        _ => {
            if crate::builtins::method_exists(obj, name) {
                Ok(Value::Method(Rc::new(BoundMethod {
                    receiver: obj.clone(),
                    kind: MethodKind::Native(Rc::from(name)),
                })))
            } else {
                Err(format!("'{}' object has no attribute '{}'", obj.type_name(), name))
            }
        }
    }
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
    } else if m.contains("has no attribute") {
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

fn compare(op: CmpOp, a: &Value, b: &Value) -> Result<bool, String> {
    use std::cmp::Ordering;
    Ok(match op {
        CmpOp::Eq => a.equals(b),
        CmpOp::NotEq => !a.equals(b),
        CmpOp::Lt => a.compare(b)? == Ordering::Less,
        CmpOp::Gt => a.compare(b)? == Ordering::Greater,
        CmpOp::LtEq => a.compare(b)? != Ordering::Greater,
        CmpOp::GtEq => a.compare(b)? != Ordering::Less,
        CmpOp::Is => value_is(a, b),
        CmpOp::IsNot => !value_is(a, b),
        CmpOp::In => contains(b, a)?,
        CmpOp::NotIn => !contains(b, a)?,
    })
}

/// Identity comparison. For the immutable scalars Oro shares by value this is
/// value equality; for heap objects it is `Rc` pointer identity.
fn value_is(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::None, Value::None) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => Rc::ptr_eq(x, y),
        (Value::Bytes(x), Value::Bytes(y)) => Rc::ptr_eq(x, y),
        (Value::List(x), Value::List(y)) => Rc::ptr_eq(x, y),
        (Value::Tuple(x), Value::Tuple(y)) => Rc::ptr_eq(x, y),
        (Value::Dict(x), Value::Dict(y)) => Rc::ptr_eq(x, y),
        (Value::Func(x), Value::Func(y)) => Rc::ptr_eq(x, y),
        _ => false,
    }
}

fn contains(container: &Value, item: &Value) -> Result<bool, String> {
    match container {
        Value::Str(hay) => match item {
            Value::Str(needle) => Ok(hay.s.contains(&needle.s)),
            _ => Err("'in <string>' requires string as left operand".to_string()),
        },
        // Subsequence, like `str`. CPython also lets an `int` on the left ask
        // whether one octet is present; that is a second meaning for one
        // spelling, so Oro says what it wants instead of guessing.
        Value::Bytes(hay) => match item {
            Value::Bytes(needle) => Ok(subsequence(hay, needle)),
            _ => Err("'in <bytes>' requires bytes as left operand".to_string()),
        },
        Value::List(l) => Ok(l.borrow().iter().any(|v| v.equals(item))),
        Value::Tuple(t) => Ok(t.iter().any(|v| v.equals(item))),
        Value::Dict(d) => d.borrow().contains(item),
        Value::Range(r) => Ok(range_contains(r, item)),
        other => Err(format!("argument of type '{}' is not iterable", other.type_name())),
    }
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
