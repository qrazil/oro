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

use std::cell::RefCell;
use std::rc::Rc;

use crate::ast::CmpOp;
use crate::compiler::{CaptureSource, CodeObject, Op, ParamInfo, VarTarget};
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
}

enum BlockKind {
    /// A try...except: routes here with the exception pushed onto `handling`.
    Except,
    /// A try...finally: routes here with the exception pushed onto the operand
    /// stack so `EndFinally` can re-raise it after the finally body runs.
    Finally,
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
    /// Push the boolean negation of the return value's truthiness. Used for
    /// `!=` when a class defines `__eq__` but not `__ne__`.
    NegateBool,
    /// Apply an f-string format spec to the returned (string) value, then push.
    FormatSpec(String),
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
}

/// The virtual machine.
pub struct Vm {
    frames: Vec<Frame>,
    line: u32,
    col: u32,
    /// The module frame's locals, captured when the top-level frame returns.
    /// Written exactly once (at program end); used only by tests.
    last_locals: Vec<Value>,
    /// Stack of in-flight `print` calls whose instance args are being rendered
    /// through `__str__`. A `__str__` that itself prints nests cleanly.
    prints: Vec<PrintJob>,
    /// The built-in exception classes, by name (shared identity for the run).
    excs: HashMap<&'static str, Rc<Class>>,
    /// Exceptions currently being handled (top = innermost), for bare `raise`.
    handling: Vec<Value>,
    /// Why each in-flight `finally` body is running, so `EndFinally` can resume
    /// the exception or `return` that was suspended to run the cleanup.
    finally_why: Vec<Why>,
    /// Generators currently being advanced (innermost on top), with the
    /// `ForIter` target to jump to when each is exhausted. Pushed on resume,
    /// popped on `yield`/exhaustion.
    gen_stack: Vec<(Rc<RefCell<crate::value::GenBox>>, usize)>,
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
            frames: Vec::new(),
            line: 0,
            col: 0,
            last_locals: Vec::new(),
            prints: Vec::new(),
            excs: exceptions::build_registry(),
            handling: Vec::new(),
            finally_why: Vec::new(),
            gen_stack: Vec::new(),
            argv,
            exit_code: None,
            import_root: std::path::PathBuf::from("."),
            module_cache: HashMap::new(),
            importing: std::collections::HashSet::new(),
        }
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
        self.frames.push(frame);
    }
}

impl Vm {
    // --- Operand-stack helpers (always the top frame) ------------------------

    fn top(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("no active frame")
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
        RuntimeError { message: message.into(), line: self.line as usize, col: self.col as usize }
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
            // Fetch. A short borrow reads the instruction and its span, then we
            // release it so call/return can restructure `frames`.
            let (op, pc) = {
                let frame = self.frames.last().expect("no active frame");
                let pc = frame.pc;
                let op = frame.code.ops[pc].clone();
                let (l, c) = frame.code.spans[pc];
                self.line = l;
                self.col = c;
                (op, pc)
            };
            self.frames.last_mut().unwrap().pc = pc + 1;

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
                    let v = self.frames.last().unwrap().code.consts[i].clone();
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
                Op::LoadGlobal(name) => {
                    // Globals are the builtin functions plus the exception classes.
                    let v = exceptions::lookup(&self.excs, &name)
                        .or_else(|| crate::builtins::lookup(&name));
                    match v {
                        Some(v) => self.push(v),
                        None => return Err(self.err(format!("name '{name}' is not defined"))),
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
                Op::Jump(t) => self.top().pc = t,
                Op::PopJumpIfFalse(t) => {
                    let v = self.pop();
                    if !v.truthy() {
                        self.top().pc = t;
                    }
                }
                Op::PopJumpIfTrue(t) => {
                    let v = self.pop();
                    if v.truthy() {
                        self.top().pc = t;
                    }
                }
                Op::JumpIfFalseOrPop(t) => {
                    if self.top().stack.last().unwrap().truthy() {
                        self.pop();
                    } else {
                        self.top().pc = t;
                    }
                }
                Op::JumpIfTrueOrPop(t) => {
                    if self.top().stack.last().unwrap().truthy() {
                        self.top().pc = t;
                    } else {
                        self.pop();
                    }
                }
                Op::BuildList(n) => {
                    let items = self.popn(n);
                    self.push(Value::List(Rc::new(RefCell::new(items))));
                }
                Op::BuildTuple(n) => {
                    let items = self.popn(n);
                    self.push(Value::Tuple(Rc::new(items)));
                }
                Op::BuildMap(n) => {
                    let items = self.popn(2 * n);
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
                Op::LoadAttr(name) => {
                    let obj = self.pop();
                    let r = self.wrap(get_attr(&obj, &name))?;
                    self.push(r);
                }
                Op::StoreAttr(name) => {
                    let obj = self.pop();
                    let value = self.pop();
                    match &obj {
                        Value::Instance(inst) => {
                            inst.fields.borrow_mut().insert(name.to_string(), value);
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
                Op::BuildClass { name, members, has_base } => {
                    self.build_class(name, members, has_base)?;
                }
                Op::ImportModule(path) => {
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
                    let out = self.wrap(crate::format::format_value(&value, conv, &spec_str))?;
                    self.push(Value::str(out));
                }
                Op::BuildString(n) => {
                    let parts = self.popn(n);
                    let mut s = String::new();
                    for p in parts {
                        s.push_str(&p.display());
                    }
                    self.push(Value::str(s));
                }
                Op::MatchDispatch { table, default } => {
                    let subject = self.pop();
                    let dict = match &self.top().code.consts[table] {
                        Value::Dict(d) => d.clone(),
                        _ => unreachable!("MatchDispatch table is always a dict const"),
                    };
                    // An unhashable subject cannot equal any literal key, so it
                    // takes the default — matching the compare-chain path.
                    let target = match dict.borrow().get(&subject) {
                        Ok(Some(Value::Int(t))) => t as usize,
                        _ => default,
                    };
                    self.top().pc = target;
                }
                Op::GetIter => {
                    let v = self.pop();
                    let it = self.wrap(get_iter(&v))?;
                    self.push(it);
                }
                Op::ForIter(target) => {
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
                                self.gen_stack.push((gen.clone(), target));
                                self.frames.push(frame);
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
                Op::MakeFunction(idx) => self.make_function(idx)?,
                Op::Call(n) => self.do_call(n)?,
                Op::CallEx => self.do_call_ex()?,
                Op::Return => {
                    // A generator body reaching return (including the implicit
                    // one at the end) is exhausted: StopIteration for its driver.
                    if self.top().code.is_generator {
                        return Ok(self.generator_stop());
                    }
                    let value = self.pop();
                    return self.do_return(value);
                }
                Op::Yield => {
                    let value = self.pop();
                    // Suspend this generator frame back into its GenBox and hand
                    // the value to the driver (the frame beneath).
                    let frame = self.frames.pop().expect("yield with no frame");
                    let (gen, _target) = self.gen_stack.pop().expect("yield outside a generator");
                    gen.borrow_mut().frame = Some(Box::new(frame));
                    self.push(value);
                }
                Op::SetupExcept(target) => {
                    let stack_len = self.top().stack.len();
                    self.top().blocks.push(Block { kind: BlockKind::Except, target, stack_len });
                }
                Op::SetupFinally(target) => {
                    let stack_len = self.top().stack.len();
                    self.top().blocks.push(Block { kind: BlockKind::Finally, target, stack_len });
                }
                Op::PopBlock => {
                    self.top().blocks.pop();
                }
                Op::Raise => {
                    let v = self.pop();
                    let exc = self.normalize_raise(v)?;
                    return Ok(Step::Raise(exc));
                }
                Op::Reraise => {
                    // Bare `raise` / no matching except: re-raise the exception
                    // currently being handled.
                    match self.handling.pop() {
                        Some(exc) => return Ok(Step::Raise(exc)),
                        None => {
                            return Err(self.err("No active exception to re-raise".to_string()))
                        }
                    }
                }
                Op::LoadHandling => {
                    let exc = self
                        .handling
                        .last()
                        .cloned()
                        .expect("LoadHandling with no active exception");
                    self.push(exc);
                }
                Op::EndHandler => {
                    self.handling.pop();
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
                    self.finally_why.push(Why::Normal);
                }
                Op::EndFinally => {
                    // Resume whatever was suspended to run this finally.
                    match self.finally_why.pop().expect("finally without a reason") {
                        Why::Normal => {}
                        Why::Raise(exc) => return Ok(Step::Raise(exc)),
                        Why::Return(v) => return self.do_return(v),
                    }
                }
            }
        Ok(Step::Next)
    }

    fn unbound_local_msg(&self, slot: u16) -> String {
        let code = &self.frames.last().unwrap().code;
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
        let proto = self.frames.last().unwrap().code.protos[idx].clone();
        let defaults = self.popn(proto.n_defaults);
        let frame = self.frames.last().unwrap();
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
        let args = self.popn(n);
        let callee = self.pop();
        self.invoke(callee, args, Vec::new())
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
                    "str" if matches!(args.first(), Some(Value::Instance(_))) && args.len() == 1 => {
                        return self.stringify_instance(args.into_iter().next().unwrap(), false);
                    }
                    "repr" if matches!(args.first(), Some(Value::Instance(_))) && args.len() == 1 => {
                        return self.stringify_instance(args.into_iter().next().unwrap(), true);
                    }
                    "len" if matches!(args.first(), Some(Value::Instance(_))) && args.len() == 1 => {
                        return self.dunder_len(args.into_iter().next().unwrap());
                    }
                    _ => {}
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
                if self.frames.len() >= MAX_FRAMES {
                    return Err(self.err("maximum recursion depth exceeded"));
                }
                let frame = self.bind_call(&f, args, kwargs)?;
                if f.code.is_generator {
                    // Calling a generator function does not run it; it produces a
                    // generator holding the suspended (unstarted) frame.
                    let gen = crate::value::GenBox { done: false, frame: Some(Box::new(frame)) };
                    self.push(Value::Generator(Rc::new(RefCell::new(gen))));
                } else {
                    self.frames.push(frame);
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
        if self.frames.len() >= MAX_FRAMES {
            return Err(self.err("maximum recursion depth exceeded"));
        }
        let mut call_args = Vec::with_capacity(args.len() + 1);
        call_args.push(receiver.clone());
        call_args.extend(args);
        let mut frame = self.bind_call(&func, call_args, kwargs)?;
        frame.ret_action = action;
        frame.super_ctx = Some((defclass, receiver));
        self.frames.push(frame);
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

    /// Drive an in-flight `print`: render remaining args left to right, calling
    /// `__str__` (through a frame) for instances that define one. When the last
    /// argument is rendered, join with spaces, emit, and push `None`.
    fn do_print(
        &mut self,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<(), RuntimeError> {
        if !kwargs.is_empty() {
            return Err(self.err("print() keyword arguments are not supported in this build"));
        }
        self.prints.push(PrintJob { rendered: Vec::new(), remaining: args, next: 0 });
        self.drive_print()
    }

    fn drive_print(&mut self) -> Result<(), RuntimeError> {
        loop {
            let next_val = {
                let job = self.prints.last().expect("active print job");
                if job.next >= job.remaining.len() {
                    let job = self.prints.pop().unwrap();
                    println!("{}", job.rendered.join(" "));
                    self.push(Value::None);
                    return Ok(());
                }
                job.remaining[job.next].clone()
            };
            self.prints.last_mut().unwrap().next += 1;

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
            let s = next_val.display();
            self.prints.last_mut().unwrap().rendered.push(s);
        }
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
    fn build_class(
        &mut self,
        name: Rc<str>,
        member_names: Vec<Rc<str>>,
        has_base: bool,
    ) -> Result<(), RuntimeError> {
        let member_vals = self.popn(member_names.len());
        let base = if has_base {
            match self.pop() {
                Value::Class(c) => Some(c),
                other => {
                    return Err(self.err(format!(
                        "base of class '{}' must be a class, not '{}'",
                        name,
                        other.type_label()
                    )))
                }
            }
        } else {
            None
        };
        let mut members = HashMap::with_capacity(member_names.len());
        for (n, v) in member_names.into_iter().zip(member_vals) {
            members.insert(n.to_string(), v);
        }
        // A class inherits exception-hood from its base, so user exceptions
        // (`class MyError(Exception)`) render and raise like built-in ones.
        let is_exception = base.as_ref().is_some_and(|b| b.is_exception);
        let class = Class { name, base, members: RefCell::new(members), is_exception };
        self.push(Value::Class(Rc::new(class)));
        Ok(())
    }

    // --- Imports -------------------------------------------------------------

    /// Import the module named by the dotted `path`: a built-in, a cached user
    /// module, or a freshly loaded one whose body runs once (as a frame) before
    /// its namespace is captured. Pushes the module value (or raises).
    fn import_module(&mut self, path: &str) -> Result<Step, RuntimeError> {
        // Built-in modules first.
        if let Some(m) = modules::build(path, &self.argv) {
            self.push(m);
            return Ok(Step::Next);
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

        // Resolve `a.b.c` to `<root>/a/b/c.oro`.
        let mut file = self.import_root.clone();
        for seg in path.split('.') {
            file.push(seg);
        }
        file.set_extension("oro");

        let source = match std::fs::read_to_string(&file) {
            Ok(s) => s,
            Err(_) => {
                let class = self.excs["ModuleNotFoundError"].clone();
                let msg = Value::str(format!("No module named '{path}'"));
                return Ok(Step::Raise(self.make_exception_instance(class, vec![msg])));
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
        self.frames.push(frame);
        Ok(Step::Next)
    }

    /// Build a module value from a finished module-body `frame`, cache it, and
    /// push it to the importer.
    fn finish_module(&mut self, path: Rc<str>, frame: Frame) {
        let mut members = HashMap::new();
        for (name, target) in &frame.code.module_names {
            let value = match target {
                VarTarget::Local(s) => frame.locals[*s as usize].clone(),
                VarTarget::Cell(s) => frame.cells[*s as usize].borrow().clone(),
            };
            // Unbound names (declared but never assigned on this path) are skipped.
            if !matches!(value, Value::Unbound) {
                members.insert(name.to_string(), value);
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
        fields.insert("args".to_string(), Value::Tuple(Rc::new(args)));
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
                self.finally_why.push(Why::Return(value));
                return Ok(Step::Next);
            }
            // Except blocks are simply discarded on the way out.
        }

        let mut frame = self.frames.pop().expect("return with no frame");
        if self.frames.is_empty() {
            self.last_locals = frame.locals;
            return Ok(Step::Done(value));
        }
        // Take the action out so the whole `frame` stays usable (BuildModule
        // needs it to read the module namespace).
        let action = std::mem::replace(&mut frame.ret_action, ReturnAction::Normal);
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
                self.prints.last_mut().expect("print job").rendered.push(s);
                self.drive_print()?;
            }
            ReturnAction::NegateBool => self.push(Value::Bool(!value.truthy())),
            ReturnAction::FormatSpec(spec) => {
                let out =
                    self.wrap(crate::format::format_value(&value, crate::format::CONV_NONE, &spec))?;
                self.push(Value::str(out));
            }
            ReturnAction::BuildModule(path) => self.finish_module(path, frame),
        }
        Ok(Step::Next)
    }

    /// A generator frame reached `return` (or fell off the end): mark it done
    /// and route its driver's `for` loop to the exhaustion target.
    fn generator_stop(&mut self) -> Step {
        self.frames.pop();
        let (gen, target) = self.gen_stack.pop().expect("generator stop outside a driver");
        {
            let mut g = gen.borrow_mut();
            g.done = true;
            g.frame = None;
        }
        self.pop(); // discard the exhausted generator (the ForIter operand)
        self.top().pc = target;
        Step::Next
    }

    /// Unwind `exc` through the block and frame stacks. On success (a handler or
    /// finally took over) returns `None` and the loop resumes; if nothing
    /// catches it, returns the uncaught error to end the run.
    fn unwind(&mut self, exc: Value) -> Option<RuntimeError> {
        loop {
            let block = self.frames.last_mut().and_then(|f| f.blocks.pop());
            match block {
                Some(b) => {
                    let frame = self.frames.last_mut().unwrap();
                    frame.stack.truncate(b.stack_len);
                    match b.kind {
                        BlockKind::Except => {
                            frame.pc = b.target;
                            self.handling.push(exc);
                            return None;
                        }
                        BlockKind::Finally => {
                            frame.pc = b.target;
                            // Run the finally body; EndFinally re-raises after.
                            self.finally_why.push(Why::Raise(exc));
                            return None;
                        }
                    }
                }
                None => {
                    // No handler in this frame: discard it and try the caller.
                    self.frames.pop();
                    if self.frames.is_empty() {
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
        RuntimeError { message, line: self.line as usize, col: self.col as usize }
    }

    /// Bind arguments to a fresh frame's slots and cells.
    ///
    /// Two paths, as the spec calls out: the **static** path fills positional
    /// parameters straight into their numbered slots; the **dynamic** path is
    /// taken when keyword arguments are present or the function has `*args` /
    /// `**kwargs`, where some argument names are only known at runtime and must
    /// be matched by name against the parameter list.
    fn bind_call(
        &self,
        func: &Rc<Function>,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<Frame, RuntimeError> {
        let code = &func.code;
        let mut frame = Frame {
            locals: vec![Value::Unbound; code.nlocals],
            cells: (0..code.ncells).map(|_| Rc::new(RefCell::new(Value::Unbound))).collect(),
            free: func.freevars.clone(),
            stack: Vec::new(),
            pc: 0,
            code: code.clone(),
            ret_action: ReturnAction::Normal,
            super_ctx: None,
        blocks: Vec::new(),
        };

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
        Value::Dict(d) => IterState::Snapshot { items: d.borrow().keys(), idx: 0 },
        Value::File(f) => IterState::File { file: f.clone() },
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
        IterState::Snapshot { items, idx } => {
            if *idx < items.len() {
                let v = items[*idx].clone();
                *idx += 1;
                Ok(Some(v))
            } else {
                Ok(None)
            }
        }
        IterState::File { file } => {
            use std::io::BufRead;
            let mut f = file.borrow_mut();
            if f.closed {
                return Err("I/O operation on closed file".to_string());
            }
            let reader = f.reader.as_mut().ok_or("file not open for reading")?;
            let mut line = String::new();
            if reader.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
                Ok(None)
            } else {
                Ok(Some(Value::str(line)))
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
    // Order matters: check the more specific substrings first.
    if m.contains("No such file or directory") {
        "FileNotFoundError"
    } else if m.contains("Permission denied") {
        "PermissionError"
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
    } else if m.contains("values to unpack")
        || m.contains("could not convert string to float")
        || m.starts_with("invalid literal for int")
        || m.contains("empty separator")
        || m.contains("step")
        || m.contains("arg is an empty sequence")
        || m.contains("expected at least")
    {
        "ValueError"
    } else if m.contains("unsupported operand")
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
        Value::List(l) => Ok(l.borrow().iter().any(|v| v.equals(item))),
        Value::Tuple(t) => Ok(t.iter().any(|v| v.equals(item))),
        Value::Dict(d) => d.borrow().contains(item),
        Value::Range(r) => Ok(range_contains(r, item)),
        other => Err(format!("argument of type '{}' is not iterable", other.type_name())),
    }
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
