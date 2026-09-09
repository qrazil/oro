//! `oro fmt` — the canonical Oro source formatter.
//!
//! Modeled on `gofmt`: there is exactly one way `oro fmt` prints any given
//! program, and there are no configuration knobs. [`format_source`] lexes and
//! parses a source string, then walks the resulting AST to reprint it in the
//! house style (see the module-level style notes below), reattaching comments
//! along the way.
//!
//! # Style
//!
//! * 4-space indent, never tabs.
//! * One statement per line.
//! * Double-quoted strings, except a string containing a `"` (and no `'`)
//!   prints with single quotes instead of escaping. A bytes literal follows the
//!   same quote rule, and shows every octet outside printable ASCII as `\xNN`.
//! * Spaces around binary/boolean/comparison operators and after commas; none
//!   just inside brackets.
//! * No automatic line wrapping — the corpus this was modeled on never breaks
//!   a call, literal, or `def` header across lines, so neither does `oro fmt`.
//! * Blank lines between statements are reproduced from the source, run
//!   lengths capped at two, with no leading blank line at the top of a file
//!   or of a block.
//!
//! # Comments
//!
//! The lexer discards comments from the real token stream but records them on
//! the side (see [`crate::lexer::Comment`]) with enough context — source line,
//! whether a real token already preceded it on that line, whether it sits
//! inside an open bracket — for this module to reattach nearly all of them:
//!
//! * A comment alone on its own line, outside brackets, is a *leading*
//!   comment: printed immediately before whatever statement/clause follows it
//!   in source order (or, if nothing follows in the whole program, as a
//!   trailing file comment).
//! * A comment sharing a line with code, outside brackets, is a *trailing*
//!   comment: reattached to the end of that same output line, but only when
//!   that line is the exact source line some statement or clause header
//!   started on (the only case this module can place with certainty).
//! * A comment inside an open `(`/`[`/`{` — i.e. inside a multi-line call,
//!   literal, or parenthesized expression — cannot be safely reattached to a
//!   sub-expression (the AST does not carry that fine a position), nor can a
//!   trailing comment that lands on some other line of a multi-line
//!   statement. **`oro fmt` refuses to run rather than silently drop or
//!   misplace these** — see [`FmtError::Comment`], which names the line.
//!
//! In practice every comment in this repository's own corpus is the simple
//! leading, own-line, outside-brackets case, so this covers real usage fully.

use std::fmt;

use crate::ast::{
    Arg, AugOp, BinOp, BoolOp, CmpOp, ExceptHandler, Expr, Kwarg, Param, ParamKind, Pattern, Stmt,
    UnaryOp,
};
use crate::lexer::{Comment, LexError, Lexer, Token, TokenKind};
use crate::parser::{ParseError, Parser};

/// One indent level.
const INDENT: &str = "    ";
/// The maximum run of consecutive blank lines `oro fmt` will reproduce.
const MAX_BLANK_RUN: usize = 2;

// --- Precedence table --------------------------------------------------------
//
// Mirrors the parser's own binding-power table in `src/parser/mod.rs`, but
// used in reverse: given an AST node, decide whether it needs parentheses to
// reparse back into the same tree. `expr(e, min_bp)` prints `e`, wrapping it
// in parens whenever its own precedence is looser than `min_bp` — the same
// question the parser asks of the *next* token when deciding whether an
// operator may continue consuming the current expression.

/// Atoms and anything that already self-delimits (calls, literals, a
/// parenthesized/bracketed collection): never needs parens as anyone's child.
const ATOM: u8 = 9;
const POSTFIX_MIN: u8 = ATOM;
const OR_PREC: u8 = 1;
const AND_PREC: u8 = 2;
const NOT_PREC: u8 = 3;
const NOT_BP: u8 = 3;
const CMP_PREC: u8 = 4;
const CMP_BP: u8 = 4;
const UNARY_PREC: u8 = 7;
const UNARY_BP: u8 = 7;
/// A lambda's `=>` is the loosest thing in the grammar: forming one is
/// unconditional wherever an atom is immediately followed by `=>`, so a bare
/// lambda can only ever be the *last* thing in whatever expression contains
/// it. Anywhere else (the left of a binary op, the base of a postfix chain,
/// …) it needs parens — `prec = 0` guarantees that against every other
/// nonzero `min_bp` used below.
const LAMBDA_PREC: u8 = 0;

/// An error produced while formatting: a lex/parse failure on malformed
/// input, or a comment `oro fmt` cannot safely place (see the module docs).
#[derive(Debug)]
pub enum FmtError {
    Lex(LexError),
    Parse(ParseError),
    Comment { line: usize, message: String },
}

impl fmt::Display for FmtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FmtError::Lex(e) => write!(f, "{e}"),
            FmtError::Parse(e) => write!(f, "{e}"),
            FmtError::Comment { line, message } => write!(f, "{line}: {message}"),
        }
    }
}

impl std::error::Error for FmtError {}

impl From<LexError> for FmtError {
    fn from(e: LexError) -> Self {
        FmtError::Lex(e)
    }
}

impl From<ParseError> for FmtError {
    fn from(e: ParseError) -> Self {
        FmtError::Parse(e)
    }
}

/// Format an Oro source file into its canonical form.
pub fn format_source(source: &str) -> Result<String, FmtError> {
    let (tokens, comments) = Lexer::new(source).tokenize_with_comments()?;
    let program = Parser::new(tokens.clone()).parse()?;
    let mut printer =
        Printer { out: String::new(), indent: 0, last_line: 0, comments, cidx: 0, tokens, tidx: 0 };
    printer.program(&program)?;
    Ok(printer.out)
}

// --- The printer: statements, blocks, and comment placement -----------------

struct Printer {
    out: String,
    indent: usize,
    /// Source line most recently emitted (0 = nothing emitted yet), used to
    /// compute blank-line runs and to match trailing comments.
    last_line: usize,
    comments: Vec<Comment>,
    /// Index of the next not-yet-consumed comment (comments are in source
    /// order, so a single monotonic cursor suffices).
    cidx: usize,
    /// The real token stream (identical to what the parser saw), kept around
    /// purely to recover source positions the AST does not carry: where a
    /// statement's logical line actually ends (see [`Printer::logical_line_end`])
    /// and exactly which line an `else`/`finally` keyword sits on (see
    /// [`Printer::keyword_line`]).
    tokens: Vec<Token>,
    /// Index of the next not-yet-consumed token — advances monotonically in
    /// lockstep with printing, the same way `cidx` does for comments.
    tidx: usize,
}

impl Printer {
    fn program(&mut self, stmts: &[Stmt]) -> Result<(), FmtError> {
        for s in stmts {
            self.leading(s.line())?;
            self.stmt(s)?;
        }
        self.trailing_remainder()
    }

    fn body(&mut self, stmts: &[Stmt]) -> Result<(), FmtError> {
        self.indent += 1;
        for s in stmts {
            self.leading(s.line())?;
            self.stmt(s)?;
        }
        self.indent -= 1;
        Ok(())
    }

    /// Drain and print every standalone (own-line, outside-brackets) comment
    /// that precedes `before_line`. Errors if it runs into a comment it
    /// cannot place first (inside brackets, or an inline comment that no
    /// `trailing` call has claimed).
    fn leading(&mut self, before_line: usize) -> Result<(), FmtError> {
        while let Some(c) = self.comments.get(self.cidx) {
            if c.line >= before_line {
                break;
            }
            if c.in_brackets {
                return Err(FmtError::Comment {
                    line: c.line,
                    message: "comment sits inside a multi-line bracketed expression — oro fmt \
                              cannot safely place it"
                        .to_string(),
                });
            }
            if c.inline {
                return Err(FmtError::Comment {
                    line: c.line,
                    message: "trailing comment could not be attached to the statement or clause \
                              on this line"
                        .to_string(),
                });
            }
            let (line, text) = (c.line, c.text.clone());
            self.cidx += 1;
            self.emit_comment(line, &text);
        }
        Ok(())
    }

    /// After the whole program is printed, whatever comments remain are
    /// trailing-of-file comments (nothing followed them in source order).
    fn trailing_remainder(&mut self) -> Result<(), FmtError> {
        while let Some(c) = self.comments.get(self.cidx) {
            if c.in_brackets || c.inline {
                return Err(FmtError::Comment {
                    line: c.line,
                    message: "comment could not be attached to any statement".to_string(),
                });
            }
            let (line, text) = (c.line, c.text.clone());
            self.cidx += 1;
            self.emit_comment(line, &text);
        }
        Ok(())
    }

    /// If the next unconsumed comment is an inline comment on exactly `line`,
    /// consume and return it.
    fn trailing(&mut self, line: usize) -> Option<String> {
        if let Some(c) = self.comments.get(self.cidx) {
            if c.line == line && c.inline && !c.in_brackets {
                self.cidx += 1;
                return Some(c.text.clone());
            }
        }
        None
    }

    /// Print one output line for a comment: any blank lines called for first,
    /// then the current indent, the comment text, and a newline. Comments are
    /// not tokens, so there is no logical-line span to look up — the comment
    /// occupies exactly the one source line it was found on.
    fn emit_comment(&mut self, line: usize, text: &str) {
        self.write_line(line, text, None);
        self.last_line = line;
    }

    /// Print one output line of *code* — a statement or a clause header —
    /// starting at source line `start_line`, then advance `last_line` to
    /// where that logical line actually ends in the real token stream. Since
    /// `oro fmt` never wraps a long line, a statement whose expressions
    /// actually spanned several source lines (a multi-line call, collection
    /// literal, or condition — implicit line joining inside open brackets)
    /// still prints as a single output line here — but the *next* line's
    /// blank-line gap must be measured from where that source statement
    /// actually ended, not from where it started, or source lines legitimately
    /// "used up" by the now-collapsed statement would misread as blank lines
    /// to reproduce. See [`Printer::logical_line_end`].
    fn emit_code(&mut self, start_line: usize, text: &str) {
        let trailing = self.trailing(start_line);
        self.write_line(start_line, text, trailing);
        self.last_line = self.logical_line_end(start_line);
    }

    fn write_line(&mut self, line: usize, text: &str, trailing: Option<String>) {
        if self.last_line != 0 {
            let gap = line.saturating_sub(self.last_line).saturating_sub(1).min(MAX_BLANK_RUN);
            for _ in 0..gap {
                self.out.push('\n');
            }
        }
        for _ in 0..self.indent {
            self.out.push_str(INDENT);
        }
        self.out.push_str(text);
        if let Some(c) = trailing {
            self.out.push_str("  ");
            self.out.push_str(&c);
        }
        self.out.push('\n');
    }

    /// The line the logical source line starting at `start_line` actually
    /// ends on, found by scanning the real token stream forward from the
    /// cursor to the `Newline` token that terminates it (the lexer suppresses
    /// `Newline` entirely while brackets are open, so content tokens between
    /// here and the next `Newline` — skipping structural `Indent`/`Dedent` —
    /// are exactly this one logical line, however many source lines it
    /// implicitly joined). Exact, not an approximation.
    fn logical_line_end(&mut self, start_line: usize) -> usize {
        while self.tidx < self.tokens.len() && self.tokens[self.tidx].line < start_line {
            self.tidx += 1;
        }
        let mut end = start_line;
        while self.tidx < self.tokens.len() {
            match self.tokens[self.tidx].kind {
                TokenKind::Eof => break,
                TokenKind::Newline => {
                    self.tidx += 1;
                    break;
                }
                TokenKind::Indent | TokenKind::Dedent => {
                    self.tidx += 1;
                }
                _ => {
                    end = end.max(self.tokens[self.tidx].line);
                    self.tidx += 1;
                }
            }
        }
        end
    }

    /// The exact source line of the next `kind` token from the cursor onward
    /// (without consuming it) — used for `else`/`finally`, whose keyword line
    /// `Stmt::If`/`Stmt::Try` do not otherwise record (only their bodies'
    /// statements carry positions).
    fn keyword_line(&self, kind: &TokenKind) -> usize {
        self.tokens[self.tidx..]
            .iter()
            .find(|t| &t.kind == kind)
            .map(|t| t.line)
            .unwrap_or(self.last_line + 1)
    }

    fn stmt(&mut self, s: &Stmt) -> Result<(), FmtError> {
        match s {
            Stmt::Expr { value, line, .. } => self.emit_code(*line, &expr_bare(value)),
            Stmt::Assign { targets, value, line, .. } => {
                // The AST cannot distinguish `t = (1, 2, 3)` (a single name
                // bound to a tuple *value*) from `a, b = 1, 2` (unpacking into
                // multiple targets) — both are just `Expr::Tuple`. Mirror the
                // corpus convention by keying off whether any target itself
                // looks like an unpacking target: if so, print a tuple value
                // bare (`a, b = 1, 2`, `a, b = b, a`); otherwise keep it
                // parenthesized as a value in its own right (`t = (1, 2, 3)`).
                let unpacking = targets.iter().any(is_nonempty_tuple);
                let mut text = String::new();
                for t in targets {
                    text.push_str(&expr_bare(t));
                    text.push_str(" = ");
                }
                if unpacking {
                    text.push_str(&expr_bare(value));
                } else {
                    text.push_str(&expr(value, 0));
                }
                self.emit_code(*line, &text);
            }
            Stmt::AugAssign { target, op, value, line, .. } => {
                let text = format!("{} {} {}", expr_bare(target), augop_str(*op), expr_bare(value));
                self.emit_code(*line, &text);
            }
            Stmt::If { cond, body, elifs, orelse, line, .. } => {
                self.emit_code(*line, &format!("if {}:", expr(cond, 0)));
                self.body(body)?;
                for (econd, ebody) in elifs {
                    self.leading(econd.line())?;
                    self.emit_code(econd.line(), &format!("elif {}:", expr(econd, 0)));
                    self.body(ebody)?;
                }
                if orelse.is_some() {
                    let anchor = self.keyword_line(&TokenKind::Else);
                    self.leading(anchor)?;
                    self.emit_code(anchor, "else:");
                    self.body(orelse.as_ref().unwrap())?;
                }
            }
            Stmt::While { cond, body, line, .. } => {
                self.emit_code(*line, &format!("while {}:", expr(cond, 0)));
                self.body(body)?;
            }
            Stmt::For { target, iter, body, line, .. } => {
                let text = format!("for {} in {}:", expr_bare(target), expr_bare(iter));
                self.emit_code(*line, &text);
                self.body(body)?;
            }
            Stmt::Def { name, params, ret, body, line, .. } => {
                let mut text = format!("def {name}({})", def_params_str(params));
                if let Some(r) = ret {
                    text.push_str(&format!(" -> {}", expr(r, 0)));
                }
                text.push(':');
                self.emit_code(*line, &text);
                self.body(body)?;
            }
            Stmt::Class { name, base, body, line, .. } => {
                let mut text = format!("class {name}");
                if let Some(b) = base {
                    text.push_str(&format!("({})", expr(b, 0)));
                }
                text.push(':');
                self.emit_code(*line, &text);
                self.body(body)?;
            }
            Stmt::Return { value, line, .. } => {
                let text = match value {
                    Some(v) => format!("return {}", expr_bare(v)),
                    None => "return".to_string(),
                };
                self.emit_code(*line, &text);
            }
            Stmt::Break { line, .. } => self.emit_code(*line, "break"),
            Stmt::Continue { line, .. } => self.emit_code(*line, "continue"),
            Stmt::Pass { line, .. } => self.emit_code(*line, "pass"),
            Stmt::Try { body, handlers, finalbody, line, .. } => {
                self.emit_code(*line, "try:");
                self.body(body)?;
                for h in handlers {
                    self.leading(h.line)?;
                    self.emit_code(h.line, &except_header(h));
                    self.body(&h.body)?;
                }
                if let Some(fb) = finalbody {
                    let anchor = self.keyword_line(&TokenKind::Finally);
                    self.leading(anchor)?;
                    self.emit_code(anchor, "finally:");
                    self.body(fb)?;
                }
            }
            Stmt::Raise { exc, line, .. } => {
                let text = match exc {
                    Some(e) => format!("raise {}", expr(e, 0)),
                    None => "raise".to_string(),
                };
                self.emit_code(*line, &text);
            }
            Stmt::Import { path, alias, line, .. } => {
                let mut text = format!("import {}", path.join("."));
                if let Some(a) = alias {
                    text.push_str(&format!(" as {a}"));
                }
                self.emit_code(*line, &text);
            }
            Stmt::Yield { value, line, .. } => {
                let text = match value {
                    Some(v) => format!("yield {}", expr_bare(v)),
                    None => "yield".to_string(),
                };
                self.emit_code(*line, &text);
            }
            Stmt::Global { names, line, .. } => {
                self.emit_code(*line, &format!("global {}", names.join(", ")));
            }
            Stmt::Match { subject, cases, line, .. } => {
                self.emit_code(*line, &format!("match {}:", expr(subject, 0)));
                // `case` clauses are a genuine nested block under `match`
                // (the parser requires an `Indent` after `match SUBJECT:`),
                // unlike `elif`/`except`, which are siblings of `if`/`try` at
                // the same level.
                self.indent += 1;
                for c in cases {
                    self.leading(c.line)?;
                    self.emit_code(c.line, &format!("case {}:", pattern_str(&c.pattern)));
                    self.body(&c.body)?;
                }
                self.indent -= 1;
            }
        }
        Ok(())
    }
}

fn except_header(h: &ExceptHandler) -> String {
    let mut text = format!("except {}", expr(&h.exc_type, 0));
    if let Some(name) = &h.name {
        text.push_str(&format!(" as {name}"));
    }
    text.push(':');
    text
}

fn pattern_str(p: &Pattern) -> String {
    match p {
        Pattern::Literal(e) | Pattern::Dotted(e) => expr(e, 0),
        Pattern::Wildcard => "_".to_string(),
    }
}

fn augop_str(op: AugOp) -> &'static str {
    match op {
        AugOp::Add => "+=",
        AugOp::Sub => "-=",
        AugOp::Mul => "*=",
        AugOp::Div => "/=",
    }
}

fn binop_str(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::FloorDiv => "//",
        BinOp::Mod => "%",
        BinOp::Pow => "**",
    }
}

fn cmp_str(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "==",
        CmpOp::NotEq => "!=",
        CmpOp::Lt => "<",
        CmpOp::Gt => ">",
        CmpOp::LtEq => "<=",
        CmpOp::GtEq => ">=",
        CmpOp::Is => "is",
        CmpOp::IsNot => "is not",
        CmpOp::In => "in",
        CmpOp::NotIn => "not in",
    }
}

/// `(left_min_bp, right_min_bp, own_prec)` for a binary arithmetic operator,
/// matching the parser's table except `Pow`: the parser's own `(8, 8)` encodes
/// right-recursion for *parsing*, but a right-associative operator's *left*
/// child needs strictly tighter precedence than its right child to reprint
/// unambiguously (`(2 ** 3) ** 2` must not print as `2 ** 3 ** 2`, which would
/// reparse right-associated).
fn binop_bp(op: BinOp) -> (u8, u8, u8) {
    match op {
        BinOp::Add | BinOp::Sub => (5, 6, 5),
        BinOp::Mul | BinOp::Div | BinOp::FloorDiv | BinOp::Mod => (6, 7, 6),
        BinOp::Pow => (ATOM, 8, 8),
    }
}

fn boolop_bp(op: BoolOp) -> (u8, u8, u8) {
    match op {
        BoolOp::Or => (OR_PREC, 2, OR_PREC),
        BoolOp::And => (AND_PREC, 3, AND_PREC),
    }
}

// --- Expressions --------------------------------------------------------

/// Print `e` for a position whose grammar production is `expr_list` — i.e. one
/// where the parser builds a *bare* tuple from a comma-separated run with no
/// enclosing parens (assignment targets/value, `return`/`yield` value, a bare
/// expression statement, a `for` target/iterable). The AST cannot distinguish
/// a bare tuple from a parenthesized one — both are plain `Expr::Tuple` — so
/// any non-empty tuple prints bare here; this matches the corpus convention
/// (`a, b = 1, 2`, `return lo, hi`) and is always a faithful round-trip.
fn expr_bare(e: &Expr) -> String {
    if let Expr::Tuple { elements, .. } = e {
        if !elements.is_empty() {
            return bare_tuple_elements(elements);
        }
    }
    expr(e, 0)
}

fn is_nonempty_tuple(e: &Expr) -> bool {
    matches!(e, Expr::Tuple { elements, .. } if !elements.is_empty())
}

fn bare_tuple_elements(elements: &[Expr]) -> String {
    let parts: Vec<String> = elements.iter().map(|e| expr(e, 0)).collect();
    if elements.len() == 1 {
        format!("{},", parts[0])
    } else {
        parts.join(", ")
    }
}

/// Print `e`, adding parens if `e`'s own precedence is looser than `min_bp`
/// (see the precedence-table comment above).
fn expr(e: &Expr, min_bp: u8) -> String {
    let (text, prec) = expr_inner(e);
    if prec < min_bp {
        format!("({text})")
    } else {
        text
    }
}

/// A `Compare` operand: a nested `Compare` (only constructible via explicit
/// source parens, since comparisons otherwise chain into one flat node) is
/// always parenthesized regardless of `min_bp` — comparisons do not
/// associate, so `(a < b) < c` and `a < b < c` are different programs.
fn compare_operand(e: &Expr, min_bp: u8) -> String {
    if matches!(e, Expr::Compare { .. }) {
        format!("({})", expr(e, 0))
    } else {
        expr(e, min_bp)
    }
}

fn expr_inner(e: &Expr) -> (String, u8) {
    match e {
        Expr::Int { value, .. } | Expr::Float { value, .. } => (value.clone(), ATOM),
        Expr::Str { value, raw, .. } => (quote_str_maybe_raw(value, *raw), ATOM),
        Expr::Bytes { value, raw, .. } => (quote_bytes_maybe_raw(value, *raw), ATOM),
        Expr::FString { value, .. } => (quote_fstring(value), ATOM),
        Expr::Bool { value, .. } => ((if *value { "True" } else { "False" }).to_string(), ATOM),
        Expr::NoneLit { .. } => ("None".to_string(), ATOM),
        Expr::Name { name, .. } => (name.clone(), ATOM),
        Expr::Lambda { data, .. } => {
            let params = lambda_params_str(&data.params);
            let body = expr(&data.body, 0);
            (format!("{params} => {body}"), LAMBDA_PREC)
        }
        Expr::Unary { op: UnaryOp::Not, operand, .. } => {
            (format!("not {}", expr(operand, NOT_BP)), NOT_PREC)
        }
        Expr::Unary { op: UnaryOp::Neg, operand, .. } => {
            (format!("-{}", expr(operand, UNARY_BP)), UNARY_PREC)
        }
        Expr::Unary { op: UnaryOp::Pos, operand, .. } => {
            (format!("+{}", expr(operand, UNARY_BP)), UNARY_PREC)
        }
        Expr::Binary { op, left, right, .. } => {
            let (lmin, rmin, prec) = binop_bp(*op);
            let text =
                format!("{} {} {}", expr(left, lmin), binop_str(*op), expr(right, rmin));
            (text, prec)
        }
        Expr::BoolOp { op, left, right, .. } => {
            let (lmin, rmin, prec) = boolop_bp(*op);
            let word = match op {
                BoolOp::And => "and",
                BoolOp::Or => "or",
            };
            let text = format!("{} {} {}", expr(left, lmin), word, expr(right, rmin));
            (text, prec)
        }
        Expr::Compare { first, rest, .. } => {
            let mut text = compare_operand(first, CMP_BP);
            for (op, rhs) in rest {
                text.push(' ');
                text.push_str(cmp_str(*op));
                text.push(' ');
                text.push_str(&compare_operand(rhs, CMP_BP + 1));
            }
            (text, CMP_PREC)
        }
        Expr::Call { func, args, kwargs, .. } => {
            let f = postfix_base(func);
            (format!("{f}({})", call_args_str(args, kwargs)), ATOM)
        }
        Expr::Attribute { value, attr, .. } => {
            (format!("{}.{attr}", postfix_base(value)), ATOM)
        }
        Expr::Subscript { value, index, .. } => {
            (format!("{}[{}]", postfix_base(value), expr(index, 0)), ATOM)
        }
        Expr::Slice { value, lower, upper, step, .. } => {
            let l = lower.as_deref().map(|e| expr(e, 0)).unwrap_or_default();
            let u = upper.as_deref().map(|e| expr(e, 0)).unwrap_or_default();
            let text = match step {
                Some(st) => format!("{}[{l}:{u}:{}]", postfix_base(value), expr(st, 0)),
                None => format!("{}[{l}:{u}]", postfix_base(value)),
            };
            (text, ATOM)
        }
        Expr::List { elements, .. } => {
            let parts: Vec<String> = elements.iter().map(|e| expr(e, 0)).collect();
            (format!("[{}]", parts.join(", ")), ATOM)
        }
        Expr::Tuple { elements, .. } => (parenthesized_tuple(elements), ATOM),
        Expr::Dict { entries, .. } => {
            let parts: Vec<String> =
                entries.iter().map(|(k, v)| format!("{}: {}", expr(k, 0), expr(v, 0))).collect();
            (format!("{{{}}}", parts.join(", ")), ATOM)
        }
    }
}

/// Print `value` as the base of a postfix trailer (`.attr`, `(...)`, `[...]`).
/// Ordinary precedence rules cover this: `42.to_str()` lexes correctly, because
/// `Lexer::scan_number` only treats a `.` after digits as a decimal point when a
/// digit actually follows it.
fn postfix_base(value: &Expr) -> String {
    expr(value, POSTFIX_MIN)
}

fn parenthesized_tuple(elements: &[Expr]) -> String {
    match elements {
        [] => "()".to_string(),
        [only] => format!("({},)", expr(only, 0)),
        many => {
            let parts: Vec<String> = many.iter().map(|e| expr(e, 0)).collect();
            format!("({})", parts.join(", "))
        }
    }
}

/// A lambda's own parameter list is always plain names (the parser rejects
/// anything else at `=>`), so no annotations/defaults/varargs to consider.
fn lambda_params_str(params: &[Param]) -> String {
    match params {
        [only] => only.name.clone(),
        many => {
            let names: Vec<&str> = many.iter().map(|p| p.name.as_str()).collect();
            format!("({})", names.join(", "))
        }
    }
}

fn def_params_str(params: &[Param]) -> String {
    params.iter().map(def_param_str).collect::<Vec<_>>().join(", ")
}

fn def_param_str(p: &Param) -> String {
    let mut s = String::new();
    match p.kind {
        ParamKind::VarArgs => s.push('*'),
        ParamKind::KwArgs => s.push_str("**"),
        ParamKind::Normal => {}
    }
    s.push_str(&p.name);
    if let Some(ann) = &p.annotation {
        s.push_str(": ");
        s.push_str(&expr(ann, 0));
    }
    if let Some(default) = &p.default {
        if p.annotation.is_some() {
            s.push_str(" = ");
        } else {
            s.push('=');
        }
        s.push_str(&expr(default, 0));
    }
    s
}

fn call_args_str(args: &[Arg], kwargs: &[Kwarg]) -> String {
    let mut parts = Vec::with_capacity(args.len() + kwargs.len());
    for a in args {
        match a {
            Arg::Positional(e) => parts.push(expr(e, 0)),
            Arg::Star(e) => parts.push(format!("*{}", expr(e, 0))),
        }
    }
    for k in kwargs {
        match k {
            Kwarg::Keyword(name, e) => parts.push(format!("{name}={}", expr(e, 0))),
            Kwarg::DoubleStar(e) => parts.push(format!("**{}", expr(e, 0))),
        }
    }
    parts.join(", ")
}

// --- String literal quoting --------------------------------------------------

/// Choose `'` only when the (already escape-decoded) content has a `"` and no
/// `'`; otherwise `"`, escaping any `"` inside.
/// Reprint a string literal, keeping `r"..."` form when the source used it and
/// the content still allows it. Without this a regex like `r"\d+"` reformats to
/// `"\\d+"` — identical in meaning, materially worse to read, which is enough to
/// stop people turning the formatter on.
fn quote_str_maybe_raw(value: &str, raw: bool) -> String {
    if raw && can_be_raw(value) {
        let quote = if value.contains('"') { '\'' } else { '"' };
        return format!("r{quote}{value}{quote}");
    }
    quote_str(value)
}

/// A raw string cannot express a trailing backslash (it would escape the closing
/// quote), a newline, or both quote styles at once.
fn can_be_raw(value: &str) -> bool {
    !value.ends_with('\\')
        && !value.contains('\n')
        && !value.contains('\r')
        && !(value.contains('"') && value.contains('\''))
}

fn quote_str(value: &str) -> String {
    let quote = if value.contains('"') && !value.contains('\'') { '\'' } else { '"' };
    let mut out = String::with_capacity(value.len() + 2);
    out.push(quote);
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Reprint a bytes literal, keeping `rb"..."` form on the same terms as
/// `r"..."`. Only printable ASCII can be shown literally; every other octet is
/// written back as `\xNN`, which is the one spelling that always round-trips.
fn quote_bytes_maybe_raw(value: &[u8], raw: bool) -> String {
    if raw {
        if let Ok(text) = std::str::from_utf8(value) {
            if can_be_raw(text) {
                let quote = if text.contains('"') { '\'' } else { '"' };
                return format!("rb{quote}{text}{quote}");
            }
        }
    }
    quote_bytes(value)
}

fn quote_bytes(value: &[u8]) -> String {
    let quote =
        if value.contains(&b'"') && !value.contains(&b'\'') { '\'' } else { '"' };
    let mut out = String::with_capacity(value.len() + 3);
    out.push('b');
    out.push(quote);
    for &x in value {
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
            _ => out.push_str(&format!("\\x{x:02x}")),
        }
    }
    out.push(quote);
    out
}

/// f-strings keep their raw inner text verbatim (it is not escape-decoded by
/// the lexer — see [`crate::ast::Expr::FString`]) — only the delimiter is
/// chosen, and only when safe: if the raw text has an *unescaped* `"`
/// anywhere, the original source must have used `'` (an unescaped `"` would
/// otherwise have ended the string), so `'` is the only safe delimiter here
/// too. Otherwise `"` is safe regardless of any `'` inside.
fn quote_fstring(value: &str) -> String {
    let mut chars = value.chars();
    let mut has_unescaped_double = false;
    while let Some(c) = chars.next() {
        if c == '\\' {
            chars.next();
            continue;
        }
        if c == '"' {
            has_unescaped_double = true;
            break;
        }
    }
    let quote = if has_unescaped_double { '\'' } else { '"' };
    format!("f{quote}{value}{quote}")
}
