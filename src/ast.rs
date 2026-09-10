//! Abstract syntax tree for Oro.
//!
//! The tree is intentionally close to the concrete grammar: it is produced by
//! the parser (see `src/parser/`) and consumed by the compiler. Two enums carry
//! everything: [`Stmt`] for statements and [`Expr`] for expressions, boxed at
//! the recursive edges.
//!
//! Design commitments, matching the frozen language spec:
//!
//! * **Every node carries the 1-based `line`/`col` of its first token.** Later
//!   stages report errors against these positions, so no variant may omit them.
//! * **Numeric literals stay as raw source text.** The i64-vs-bignum promotion
//!   decision belongs to the compiler; the parser must not lose the original
//!   spelling.

/// A unary prefix operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// `-x`
    Neg,
    /// `+x`
    Pos,
    /// `not x`
    Not,
}

/// A binary arithmetic operator (the `and`/`or` logical operators are
/// [`Expr::BoolOp`], and comparisons are [`Expr::Compare`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `//`
    FloorDiv,
    /// `%`
    Mod,
    /// `**`
    Pow,
}

/// A short-circuiting logical operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoolOp {
    /// `and`
    And,
    /// `or`
    Or,
}

/// A comparison operator. All comparisons share one precedence level and chain
/// (`a < b < c`), so they live in [`Expr::Compare`] rather than [`Expr::Binary`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    /// `==`
    Eq,
    /// `!=`
    NotEq,
    /// `<`
    Lt,
    /// `>`
    Gt,
    /// `<=`
    LtEq,
    /// `>=`
    GtEq,
    /// `is`
    Is,
    /// `is not`
    IsNot,
    /// `in`
    In,
    /// `not in`
    NotIn,
}

/// An augmented-assignment operator (`+=`, `-=`, `*=`, `/=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AugOp {
    /// `+=`
    Add,
    /// `-=`
    Sub,
    /// `*=`
    Mul,
    /// `/=`
    Div,
}

/// The role of a parameter in a `def` header, which fixes both its calling
/// convention and its legal position in the list (positional → defaulted →
/// `*args` → `**kwargs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamKind {
    /// An ordinary parameter: `name`, `name = default`.
    Normal,
    /// The variadic positional parameter `*args`, collecting the extra
    /// positional arguments. At most one, after every `Normal` parameter.
    VarArgs,
    /// The variadic keyword parameter `**kwargs`, collecting the extra keyword
    /// arguments. At most one, and always last.
    KwArgs,
}

/// A single parameter in a `def` header: `name`, `name: ann`, `name = default`,
/// `name: ann = default`, or the variadic forms `*args` / `**kwargs`.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    /// Optional type annotation (`name: ann`). Kept as an expression; the parser
    /// does not interpret it. Always `None` for `*args` / `**kwargs`.
    pub annotation: Option<Expr>,
    /// Optional default value (`name = default`). Never present on `*args` /
    /// `**kwargs`.
    pub default: Option<Expr>,
    /// Whether this is an ordinary, `*args`, or `**kwargs` parameter.
    pub kind: ParamKind,
    pub line: usize,
    pub col: usize,
}

/// A lambda: `x => expr`, `(a, b) => expr`, `() => expr`. The body is a single
/// expression — a lambda that needs statements is a `def`.
#[derive(Debug, Clone, PartialEq)]
pub struct LambdaData {
    pub params: Vec<Param>,
    pub body: Box<Expr>,
    /// Scope id assigned by the symbol pass and read back by the resolve pass
    /// and codegen. Lambdas are *not* added to their parent's `children`, so the
    /// cursor that walks `def`/block scopes in source order is unaffected — this
    /// id is how a lambda finds its scope instead.
    pub scope: std::cell::Cell<usize>,
}

/// A single positional-side argument at a call site. Keyword-side arguments are
/// [`Kwarg`]. Kept as an ordered list so unpacking position is preserved
/// (`f(a, *b, c)` differs from `f(a, c, *b)`).
#[derive(Debug, Clone, PartialEq)]
pub enum Arg {
    /// A plain positional argument: `f(x)`.
    Positional(Expr),
    /// An iterable unpacked into positional arguments: `f(*xs)`.
    Star(Expr),
}

/// A single keyword-side argument at a call site.
#[derive(Debug, Clone, PartialEq)]
pub enum Kwarg {
    /// A named keyword argument: `f(name=value)`.
    Keyword(String, Expr),
    /// A mapping unpacked into keyword arguments: `f(**opts)`.
    DoubleStar(Expr),
}

/// A single `except` clause of a `try` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct ExceptHandler {
    /// The exception type being caught. Bare `except:` is rejected by the
    /// parser, so this is always present.
    pub exc_type: Expr,
    /// The bound name in `except E as e`.
    pub name: Option<String>,
    pub body: Vec<Stmt>,
    pub line: usize,
    pub col: usize,
}

/// A statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// A bare expression used for its side effects.
    Expr { value: Expr, line: usize, col: usize },
    /// `a = b`, `a = b = c` (chained; `targets` holds every left-hand side).
    Assign {
        targets: Vec<Expr>,
        value: Expr,
        line: usize,
        col: usize,
    },
    /// `a += b` and friends.
    AugAssign {
        target: Expr,
        op: AugOp,
        value: Expr,
        line: usize,
        col: usize,
    },
    /// `if` / `elif` / `else`.
    If {
        cond: Expr,
        body: Vec<Stmt>,
        elifs: Vec<(Expr, Vec<Stmt>)>,
        orelse: Option<Vec<Stmt>>,
        line: usize,
        col: usize,
    },
    /// `while`.
    While {
        cond: Expr,
        body: Vec<Stmt>,
        line: usize,
        col: usize,
    },
    /// `for target in iter:`.
    For {
        target: Expr,
        iter: Expr,
        body: Vec<Stmt>,
        line: usize,
        col: usize,
    },
    /// `def name(params) -> ret:`.
    Def {
        name: String,
        params: Vec<Param>,
        ret: Option<Expr>,
        body: Vec<Stmt>,
        line: usize,
        col: usize,
    },
    /// `class name(base):` (single inheritance only).
    Class {
        name: String,
        base: Option<Expr>,
        body: Vec<Stmt>,
        line: usize,
        col: usize,
    },
    /// `return` with an optional value.
    Return {
        value: Option<Expr>,
        line: usize,
        col: usize,
    },
    Break { line: usize, col: usize },
    Continue { line: usize, col: usize },
    Pass { line: usize, col: usize },
    /// `try` / `except` / `finally`.
    Try {
        body: Vec<Stmt>,
        handlers: Vec<ExceptHandler>,
        finalbody: Option<Vec<Stmt>>,
        line: usize,
        col: usize,
    },
    /// `raise` with an optional exception (bare `raise` re-raises).
    Raise {
        exc: Option<Expr>,
        line: usize,
        col: usize,
    },
    /// `import a.b.c` / `import a.b.c as name`. `path` is the dotted segments.
    Import {
        path: Vec<String>,
        alias: Option<String>,
        line: usize,
        col: usize,
    },
    /// `yield` with an optional value, used as a statement.
    Yield {
        value: Option<Expr>,
        line: usize,
        col: usize,
    },
    /// `global a, b` — binds the listed names to module scope for the rest of
    /// the enclosing function. Module scope only; `nonlocal` is unsupported.
    Global {
        names: Vec<String>,
        line: usize,
        col: usize,
    },
    /// `match subject:` — a value-only switch (see [`MatchCase`]/[`Pattern`]).
    /// Not full pattern matching: no destructuring, binding, or or-patterns.
    Match {
        subject: Expr,
        cases: Vec<MatchCase>,
        line: usize,
        col: usize,
    },
}

/// One `case PATTERN:` clause of a [`Stmt::Match`]. Its body is a block scope,
/// exactly like an `if` body, so names bound inside do not leak.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchCase {
    pub pattern: Pattern,
    pub body: Vec<Stmt>,
    pub line: usize,
    pub col: usize,
}

/// The allowed `case` patterns. Deliberately a strict subset of Python's
/// pattern grammar — only what makes `match` a switch, nothing that binds.
#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    /// A literal: int / float / str / `true` / `false` / `null`, with an
    /// optional leading `-` on numbers. Matched by equality. The inner `Expr`
    /// is guaranteed by the parser to be one of those literal forms.
    Literal(Expr),
    /// A dotted name such as `Cmd.QUIT`: looked up at runtime and matched by
    /// equality. The inner `Expr` is an attribute-access chain.
    Dotted(Expr),
    /// `case _`: the wildcard/default. Matches anything.
    Wildcard,
}

/// An expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// Integer literal, raw source text (e.g. `"42"`).
    Int { value: String, line: usize, col: usize },
    /// Float literal, raw source text (e.g. `"3.14"`).
    Float { value: String, line: usize, col: usize },
    /// String literal (escapes already decoded by the lexer).
    /// String literal (escapes already decoded). `raw` records whether the
    /// source wrote `r"..."`; it does not affect the value, only how the
    /// formatter reprints it.
    Str { value: String, raw: bool, line: usize, col: usize },
    /// Bytes literal (escapes already decoded to octets). `raw` records whether
    /// the source wrote `rb"..."`; it does not affect the value, only how the
    /// formatter reprints it.
    Bytes { value: Vec<u8>, raw: bool, line: usize, col: usize },
    /// f-string literal (raw inner text; interpolation parsed later).
    FString { value: String, line: usize, col: usize },
    /// `true` / `false`.
    Bool { value: bool, line: usize, col: usize },
    /// `null`. (The variant keeps the old name; only the spelling moved.)
    NoneLit { line: usize, col: usize },
    /// `x => expr` — an anonymous single-expression function.
    Lambda { data: Box<LambdaData>, line: usize, col: usize },
    /// A bare identifier used as a value.
    Name { name: String, line: usize, col: usize },
    /// A unary prefix operation.
    Unary {
        op: UnaryOp,
        operand: Box<Expr>,
        line: usize,
        col: usize,
    },
    /// A binary arithmetic operation.
    Binary {
        op: BinOp,
        left: Box<Expr>,
        right: Box<Expr>,
        line: usize,
        col: usize,
    },
    /// A short-circuiting `and`/`or`.
    BoolOp {
        op: BoolOp,
        left: Box<Expr>,
        right: Box<Expr>,
        line: usize,
        col: usize,
    },
    /// A (possibly chained) comparison: `first (op right)+`.
    Compare {
        first: Box<Expr>,
        rest: Vec<(CmpOp, Expr)>,
        line: usize,
        col: usize,
    },
    /// A call: `func(args, *rest, kw=val, **opts)`. `args` holds the
    /// positional-side arguments in order (including `*` unpacking); `kwargs`
    /// holds the keyword-side arguments in order (including `**` unpacking).
    Call {
        func: Box<Expr>,
        args: Vec<Arg>,
        kwargs: Vec<Kwarg>,
        line: usize,
        col: usize,
    },
    /// Attribute access: `value.attr`.
    Attribute {
        value: Box<Expr>,
        attr: String,
        line: usize,
        col: usize,
    },
    /// Subscript: `value[index]`.
    Subscript {
        value: Box<Expr>,
        index: Box<Expr>,
        line: usize,
        col: usize,
    },
    /// Slice: `value[lower:upper:step]` (any part may be absent).
    Slice {
        value: Box<Expr>,
        lower: Option<Box<Expr>>,
        upper: Option<Box<Expr>>,
        step: Option<Box<Expr>>,
        line: usize,
        col: usize,
    },
    /// List literal `[...]`.
    List { elements: Vec<Expr>, line: usize, col: usize },
    /// Tuple literal (parenthesised or bare).
    Tuple { elements: Vec<Expr>, line: usize, col: usize },
    /// Dict literal `{k: v}`.
    Dict {
        entries: Vec<(Expr, Expr)>,
        line: usize,
        col: usize,
    },
}

impl Expr {
    /// The 1-based `(line, col)` of the expression's first token.
    pub fn pos(&self) -> (usize, usize) {
        match self {
            Expr::Int { line, col, .. }
            | Expr::Float { line, col, .. }
            | Expr::Str { line, col, .. }
            | Expr::Bytes { line, col, .. }
            | Expr::FString { line, col, .. }
            | Expr::Bool { line, col, .. }
            | Expr::NoneLit { line, col, .. }
            | Expr::Lambda { line, col, .. }
            | Expr::Name { line, col, .. }
            | Expr::Unary { line, col, .. }
            | Expr::Binary { line, col, .. }
            | Expr::BoolOp { line, col, .. }
            | Expr::Compare { line, col, .. }
            | Expr::Call { line, col, .. }
            | Expr::Attribute { line, col, .. }
            | Expr::Subscript { line, col, .. }
            | Expr::Slice { line, col, .. }
            | Expr::List { line, col, .. }
            | Expr::Tuple { line, col, .. }
            | Expr::Dict { line, col, .. } => (*line, *col),
        }
    }

    /// The 1-based line of the expression's first token.
    pub fn line(&self) -> usize {
        self.pos().0
    }

    /// The 1-based column of the expression's first token.
    pub fn col(&self) -> usize {
        self.pos().1
    }
}

impl Stmt {
    /// The 1-based `(line, col)` of the statement's first token.
    pub fn pos(&self) -> (usize, usize) {
        match self {
            Stmt::Expr { line, col, .. }
            | Stmt::Assign { line, col, .. }
            | Stmt::AugAssign { line, col, .. }
            | Stmt::If { line, col, .. }
            | Stmt::While { line, col, .. }
            | Stmt::For { line, col, .. }
            | Stmt::Def { line, col, .. }
            | Stmt::Class { line, col, .. }
            | Stmt::Return { line, col, .. }
            | Stmt::Break { line, col, .. }
            | Stmt::Continue { line, col, .. }
            | Stmt::Pass { line, col, .. }
            | Stmt::Try { line, col, .. }
            | Stmt::Raise { line, col, .. }
            | Stmt::Import { line, col, .. }
            | Stmt::Yield { line, col, .. }
            | Stmt::Global { line, col, .. }
            | Stmt::Match { line, col, .. } => (*line, *col),
        }
    }

    /// The 1-based line of the statement's first token.
    pub fn line(&self) -> usize {
        self.pos().0
    }

    /// The 1-based column of the statement's first token.
    pub fn col(&self) -> usize {
        self.pos().1
    }
}
