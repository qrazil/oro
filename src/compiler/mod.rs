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

/// A single instruction. Jump targets are absolute instruction indices.
#[derive(Debug, Clone)]
pub enum Op {
    /// Push a constant from the pool.
    LoadConst(usize),
    /// Push `None`.
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
    /// Look a name up in the builtins (the only globals Oro has).
    LoadGlobal(Rc<str>),
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
    Jump(usize),
    PopJumpIfFalse(usize),
    PopJumpIfTrue(usize),
    /// Short-circuit `and`: if the top is falsy, leave it and jump; else pop.
    JumpIfFalseOrPop(usize),
    /// Short-circuit `or`: if the top is truthy, leave it and jump; else pop.
    JumpIfTrueOrPop(usize),

    // Collection construction.
    BuildList(usize),
    BuildTuple(usize),
    BuildSet(usize),
    /// Pop `2 * n` values (`k0, v0, k1, v1, ...`) into a new dict.
    BuildMap(usize),
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
    /// Load an attribute (used for method access like `s.split`).
    LoadAttr(Rc<str>),
    /// Pop an iterable and push `n` elements in reverse (top = first element).
    UnpackSequence(usize),
    /// Format an f-string replacement field: pop the format-spec string (top)
    /// and the value beneath it, apply the `!r`/`!s` conversion encoded in the
    /// byte, and push the resulting string.
    FormatValue(u8),
    /// Pop `n` strings and push their concatenation (f-string assembly).
    BuildString(usize),

    // Iteration.
    GetIter,
    /// If the iterator on top is exhausted, pop it and jump; otherwise push the
    /// next element (iterator stays underneath).
    ForIter(usize),

    // Functions and calls.
    /// Build a closure from prototype `index`, popping its default values.
    MakeFunction(usize),
    /// Call with `n` positional args already on the stack above the callable.
    Call(usize),
    /// Call with an assembled positional list and keyword dict on the stack:
    /// `func, poslist, kwdict`.
    CallEx,
    /// Return the top of the stack from the current frame.
    Return,
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
    /// Local slots whose name also exists at module scope but was made local by
    /// assignment (no `global` declaration). Used only to turn an
    /// unbound-local error into a teaching message. Empty for the module.
    pub shadow_hints: Vec<(u16, Rc<str>)>,
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
