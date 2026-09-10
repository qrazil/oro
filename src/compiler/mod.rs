//! The Oro bytecode compiler.
//!
//! Two passes, exactly as architecture point 3 requires:
//!
//! 1. A **symbol pre-pass** ([`symbols`]) walks each scope collecting every
//!    binding (assignment target, `def`, `for` target, parameter) and numbers it
//!    a slot *before any body is compiled*. This is what makes forward
//!    references and mutual recursion work: when a function body is compiled the
//!    slot of every name it might call already exists. The same pass performs
//!    closure analysis (which locals are captured by an inner function) and
//!    honours Oro's block scoping (architecture point 6).
//! 2. **Codegen** ([`codegen`]) walks the tree again emitting [`Op`]s, resolving
//!    each name to the slot the pre-pass assigned.
//!
//! The unit of output is a [`CodeObject`]: a flat instruction vector plus the
//! metadata the VM needs (constant pool, slot counts, parameter descriptors,
//! and the nested-function prototypes).

mod codegen;
mod symbols;

use std::cell::RefCell;
use std::rc::Rc;

use crate::ast::{CmpOp, ParamKind, Stmt};
use crate::value::Value;

#[cfg(test)]
mod tests;

/// A compile error, carrying the 1-based source position of the offending node.
#[derive(Debug, Clone, PartialEq)]
pub struct CompileError {
    pub message: String,
    pub line: usize,
    pub col: usize,
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.line, self.col, self.message)
    }
}

impl std::error::Error for CompileError {}

/// Where a variable lives in a frame: a plain local slot or a shared cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarTarget {
    Local(u16),
    Cell(u16),
}

/// The static description of one `class` statement: the class's name, its
/// members in stack order, and whether a base class was pushed beneath them.
///
/// Held in [`CodeObject::classes`] and named by index from [`Op::BuildClass`],
/// never inline. Inline, its `Vec<Rc<str>>` alone made *every* `Op` 48 bytes
/// wide — a `class` statement, compiled once per program and executed once, was
/// setting the cache density of the entire instruction stream.
#[derive(Debug)]
pub struct ClassSpec {
    pub name: Rc<str>,
    pub members: Vec<Rc<str>>,
    pub has_base: bool,
}

/// A single instruction: a one-byte tag plus at most one 32-bit operand, so
/// **`Op` is exactly 8 bytes and `Copy`**. Jump targets are absolute
/// instruction indices.
///
/// Both properties are load-bearing, and neither is an accident:
///
/// * **8 bytes.** The dispatch loop streams through `ops` linearly, so the
///   width of `Op` is the instruction-cache density of the whole interpreter,
///   and a power-of-two stride turns `ops[pc]` into a shift instead of a
///   multiply. `Op` was 48 bytes; at 8 the same cache line holds six
///   instructions instead of one.
/// * **`Copy`.** The loop reads the current instruction out before executing
///   it (execution can restructure `frames`, so the borrow cannot be held).
///   While `Op` held an `Rc<str>`, that read was a refcount bump per
///   instruction. A `Copy` `Op` makes it a register move.
///
/// The rule that keeps both true: **a variant may carry at most one `u32`.**
/// Anything larger goes in a side table on [`CodeObject`] and is named by
/// index — strings in [`CodeObject::names`], class descriptions in
/// [`CodeObject::classes`], the two-operand instructions in
/// [`CodeObject::pairs`]. `compiler::tests::op_is_one_word` is the tripwire.
#[derive(Debug, Clone, Copy)]
pub enum Op {
    /// Push a constant from the pool.
    LoadConst(u32),
    /// Push `null`.
    LoadNone,
    /// Read/write a plain local slot.
    LoadFast(u16),
    StoreFast(u16),
    /// Read/write one of this frame's own captured cells.
    LoadCell(u16),
    StoreCell(u16),
    /// Read/write a cell captured from an enclosing function.
    LoadFree(u16),
    StoreFree(u16),
    /// Look a name up in the builtins (the only globals Oro has). The operand
    /// indexes [`CodeObject::names`].
    LoadGlobal(u32),
    /// Discard the top of the stack.
    Pop,
    /// Duplicate the top of the stack.
    Dup,
    /// Duplicate the top two values: `[a, b] -> [a, b, a, b]`.
    DupTwo,
    /// Swap the top two values.
    RotTwo,
    /// Lift the top value beneath the next two: `[a, b, c] -> [c, a, b]`.
    RotThree,

    // Unary / binary operators.
    UnaryNeg,
    UnaryPos,
    UnaryNot,
    BinAdd,
    BinSub,
    BinMul,
    BinDiv,
    BinFloorDiv,
    BinMod,
    BinPow,
    Compare(CmpOp),

    // Control flow. Targets are absolute op indices.
    Jump(u32),
    PopJumpIfFalse(u32),
    PopJumpIfTrue(u32),
    /// Short-circuit `and`: if the top is falsy, leave it and jump; else pop.
    JumpIfFalseOrPop(u32),
    /// Short-circuit `or`: if the top is truthy, leave it and jump; else pop.
    JumpIfTrueOrPop(u32),

    // Collection construction.
    BuildList(u32),
    BuildTuple(u32),
    /// Pop `2 * n` values (`k0, v0, k1, v1, ...`) into a new dict.
    BuildMap(u32),
    /// Append the top value to the list one below it (list stays on the stack).
    ListAppend,
    /// Extend the list one below the top with the iterable on top.
    ListExtend,
    /// Set `dict[key] = value` for the dict below `key, value`.
    MapSetItem,
    /// Merge the mapping on top into the dict below it.
    MapMerge,

    // Indexing.
    LoadSubscript,
    StoreSubscript,
    /// Pop `step, upper, lower, value` and push `value[lower:upper:step]`.
    LoadSlice,
    /// Load an attribute (used for method access like `s.split`). The operand
    /// indexes [`CodeObject::names`].
    LoadAttr(u32),
    /// Pop the object (top) then the value; set `obj.<name> = value`. Only
    /// user-class instances have settable attributes. The operand indexes
    /// [`CodeObject::names`].
    StoreAttr(u32),
    /// Build a class from the base (if `has_base`, below the members) and the
    /// members above it, then push the resulting class. Members are methods and
    /// class-level attributes. The operand indexes [`CodeObject::classes`].
    BuildClass(u32),
    /// Push a `super()` proxy for the current method's `super_ctx`.
    LoadSuper,
    /// Import the module named by the dotted path and push it (bound by the
    /// caller to a name). Only built-in modules resolve in this build. The
    /// operand indexes [`CodeObject::names`].
    ImportModule(u32),
    /// Pop an iterable and push `n` elements in reverse (top = first element).
    UnpackSequence(u32),
    /// Format an f-string replacement field: pop the format-spec string (top)
    /// and the value beneath it, apply the `!r`/`!s` conversion encoded in the
    /// byte, and push the resulting string.
    FormatValue(u8),
    /// Pop `n` strings and push their concatenation (f-string assembly).
    BuildString(u32),
    /// All-literal `match` dispatch. Pop the subject and look it up in the
    /// dict constant at `table` (mapping each literal pattern to the op index of
    /// its case body, first case winning on equal keys); jump there, or to
    /// `default` on no match / an unhashable subject. O(1) versus a compare
    /// chain — the reason `match` earns its keep over `if`/`elif`.
    ///
    /// Needs two operands, so they live in [`CodeObject::pairs`] as
    /// `(table, default)` and the instruction carries the index.
    MatchDispatch(u32),

    // Iteration.
    GetIter,
    /// If the iterator on top is exhausted, pop it and jump; otherwise push the
    /// next element (iterator stays underneath).
    ForIter(u32),

    // Functions and calls.
    /// Build a closure from prototype `index`, popping its default values.
    MakeFunction(u32),
    /// Call with `n` positional args already on the stack above the callable.
    Call(u32),
    /// Call with an assembled positional list and keyword dict on the stack:
    /// `func, poslist, kwdict`.
    CallEx,
    /// Return the top of the stack from the current frame.
    Return,
    /// Suspend the current generator frame, yielding the top value.
    Yield,

    // Exceptions.
    /// Push a try/except block; an exception routes to `target` (dispatch).
    SetupExcept(u32),
    /// Push a try/finally block; an exception routes to `target` (finally body).
    SetupFinally(u32),
    /// Pop the innermost active block (try body finished normally).
    PopBlock,
    /// Pop the top value and raise it as an exception.
    Raise,
    /// Re-raise the exception currently being handled (bare `raise`, or a
    /// handler that matched nothing).
    Reraise,
    /// Push a copy of the exception currently being handled.
    LoadHandling,
    /// Finish handling the current exception (pop it from the handling stack).
    EndHandler,
    /// Pop a class then the exception; push whether the exception matches it.
    ExcMatch,
    /// Push a loop block so `break`/`continue` can unwind through any `finally`
    /// bodies between them and the loop.
    ///
    /// Needs two operands, so they live in [`CodeObject::pairs`] as
    /// `(brk, cont)` — the after-loop target and the loop's continue point —
    /// and the instruction carries the index.
    SetupLoop(u32),
    /// Leave the innermost loop, running enclosing `finally` bodies first.
    Break,
    /// Jump to the innermost loop's continue point, running enclosing `finally`
    /// bodies first.
    Continue,
    /// Enter a finally body on the normal fall-through path (no suspended
    /// exception or return).
    BeginFinally,
    /// End a finally body: resume the suspended exception/return, or continue.
    EndFinally,
}

/// How a parameter is filled at call time.
#[derive(Debug, Clone)]
pub struct ParamInfo {
    pub name: Rc<str>,
    pub kind: ParamKind,
    pub target: VarTarget,
    /// True for a `Normal` parameter that has a default value.
    pub has_default: bool,
}

/// The source of one captured cell when a closure is built.
#[derive(Debug, Clone, Copy)]
pub enum CaptureSource {
    /// A cell owned by the enclosing frame.
    Cell(u16),
    /// A cell the enclosing frame itself captured.
    Free(u16),
}

/// A nested function prototype, resolved against its enclosing function.
#[derive(Debug)]
pub struct FuncProto {
    pub code: Rc<CodeObject>,
    /// One entry per free variable of `code`, telling the VM where in the
    /// enclosing frame to fetch the shared cell.
    pub captures: Vec<CaptureSource>,
    /// Number of default values the enclosing frame pushes before
    /// `MakeFunction`, in defaulted-parameter order.
    pub n_defaults: usize,
}

/// A compiled unit of code: a module body or a function body.
#[derive(Debug)]
pub struct CodeObject {
    pub name: String,
    pub ops: Vec<Op>,
    /// Parallel to `ops`: the 1-based `(line, col)` each instruction came from,
    /// used to position runtime errors.
    pub spans: Vec<(u32, u32)>,
    pub consts: Vec<Value>,
    /// Interned names, indexed by `LoadGlobal`, `LoadAttr`, `StoreAttr` and
    /// `ImportModule`. Keeping the strings here rather than inline in the
    /// instruction is what lets `Op` be 8 bytes and `Copy`; a name used at ten
    /// call sites is stored once and its `Rc` is never cloned per dispatch.
    pub names: Vec<Rc<str>>,
    /// One slot per entry in `names`, holding the builtin that `LoadGlobal`
    /// resolved there, filled on first execution.
    ///
    /// This is an inline cache, and it is the rare kind that needs no
    /// invalidation at all. Oro's globals are exactly the builtin functions and
    /// the exception classes, and neither set can change while a program runs —
    /// there is no assignable module namespace to invalidate against. Only
    /// *builtins* are cached: exception classes have a per-VM `Rc` identity
    /// (`ValueError == ValueError` is a pointer comparison), and a code object
    /// could in principle be run by a second `Vm`, so caching those would be
    /// unsound. A builtin has no observable identity — `Value::Builtin` is
    /// unhashable and never compares equal, even to itself — so caching one is
    /// invisible to any program.
    ///
    /// Without it, every `len(...)` in a loop hashed a string, walked a match
    /// arm per builtin name, and then *allocated* a fresh `Rc<Builtin>` wrapper
    /// to hand back.
    pub builtin_cache: RefCell<Vec<Option<Value>>>,
    /// Class descriptions, indexed by `BuildClass`. See [`ClassSpec`].
    pub classes: Vec<Rc<ClassSpec>>,
    /// Operand pairs for the two instructions that need two of them —
    /// `MatchDispatch` and `SetupLoop`. They are executed once per `match` and
    /// once per loop *entry*, so an extra indirection on them is free, and it
    /// is what keeps every other instruction one word wide.
    pub pairs: Vec<(u32, u32)>,
    /// Nested function prototypes, indexed by `MakeFunction`.
    pub protos: Vec<Rc<FuncProto>>,
    /// Number of plain local slots to allocate for a frame.
    pub nlocals: usize,
    /// Number of own cells (captured locals) to allocate.
    pub ncells: usize,
    /// Number of free variables (captured from enclosing scopes).
    pub nfree: usize,
    /// Parameters in declaration order (empty for a module).
    pub params: Vec<ParamInfo>,
    /// True when every parameter is a plain positional one — no `*args`, no
    /// `**kwargs`. Computed here so the VM's argument binder can take its
    /// static path on a simple `f(a, b)` without walking `params` first.
    pub simple_params: bool,
    /// Local slots whose name also exists at module scope but was made local by
    /// assignment (no `global` declaration). Used only to turn an
    /// unbound-local error into a teaching message. Empty for the module.
    pub shadow_hints: Vec<(u16, Rc<str>)>,
    /// True when this function's body contains `yield`; calling it produces a
    /// generator instead of running the body.
    pub is_generator: bool,
    /// For a module code object only: its top-level names and where each is
    /// stored, so an `import` can capture the module's namespace after it runs.
    pub module_names: Vec<(Rc<str>, VarTarget)>,
}

impl std::fmt::Debug for Value {
    // A terse debug so `CodeObject` can derive `Debug` without a noisy dump of
    // every constant's internals.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.repr())
    }
}

/// Compile a parsed module into its top-level [`CodeObject`].
pub fn compile(program: &[Stmt]) -> Result<Rc<CodeObject>, CompileError> {
    let mut table = symbols::SymTable::new();
    table.build_module(program)?;
    table.resolve_module(program);
    table.allocate();
    codegen::compile_module(&table, program)
}
