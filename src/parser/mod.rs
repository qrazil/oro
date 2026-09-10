//! Hand-written parser for Oro.
//!
//! Two techniques, as the design mandates:
//!
//! * **Recursive descent for statements** — one function per grammar rule
//!   ([`Parser::if_stmt`], [`Parser::def_stmt`], and so on).
//! * **Pratt / precedence-climbing for expressions** — a single binding-power
//!   loop in [`Parser::parse_expr`] driven by [`Parser::infix_bp`], rather than
//!   a ladder of one-function-per-precedence-level.
//!
//! Precedence, lowest to highest:
//!
//! ```text
//! or
//! and
//! not            (unary prefix)
//! == != < > <= >= in  not in                (comparison, chaining)
//! + -
//! * / // %
//! - +            (unary prefix)
//! **             (right associative)
//! call  attr  subscript   (postfix, tightest)
//! ```
//!
//! Features that were deliberately cut from the language produce a *specific*
//! error naming the feature, not a generic "syntax error" — these are design
//! decisions and the diagnostics say so.

use std::fmt;

use crate::ast::{
    Arg, AugOp, BinOp, BoolOp, CmpOp, ExceptHandler, Expr, Kwarg, MatchCase, Param, ParamKind,
    Pattern, Stmt, UnaryOp,
};
use crate::lexer::{Token, TokenKind};

/// Oro has no type annotations, in any of their three positions.
///
/// They used to parse and be thrown away — `def f(a: int) -> int` accepted a
/// string and said nothing — which is decorative syntax that looks like it
/// does something, in a language whose whole thesis is that it does not do
/// that. There is nothing that reads an annotation and nothing that will, so
/// the removal is the same shape as every other one: reject, and name what to
/// write instead.
const ANNOTATION_CUT: &str = "Oro has no type annotations — write `def f(a)` rather than \
                              `def f(a: int) -> int`, and `x = 5` rather than `x: int = 5`; \
                              nothing reads them";

const CHAINED_ASSIGN_CUT: &str = "Oro has no chained assignment — write `a, b = 1, 2`, which \
                                  also lets the values differ. `a = b = []` binds *one* list to \
                                  both names, so appending through `a` changes `b`; `a, b = [], \
                                  []` makes two";

/// An error produced while parsing, with a 1-based source position.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub message: String,
    pub line: usize,
    pub col: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: {}", self.line, self.col, self.message)
    }
}

impl std::error::Error for ParseError {}

type PResult<T> = Result<T, ParseError>;

// --- Binding powers ---------------------------------------------------------
//
// Left binding power (`lbp`) decides whether an infix operator binds against the
// expression on its left; the right recursion power decides associativity
// (left-associative operators recurse one level tighter, right-associative ones
// recurse at their own level). These mirror the precedence table above; the gaps
// leave room for the prefix operators, which are not in `infix_bp`.

/// Comparison operators all share this precedence and chain.
const CMP_BP: u8 = 4;
/// Right-recursion power for the unary `-`/`+` prefix operators.
const UNARY_BP: u8 = 7;
/// Right-recursion power for the `not` prefix operator.
const NOT_BP: u8 = 3;

/// The parser. Construct from a token stream with [`Parser::new`] and consume
/// with [`Parser::parse`].
pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    /// Build a parser over a lexed token stream. The stream is expected to end
    /// with [`TokenKind::Eof`] (the lexer guarantees this).
    pub fn new(tokens: Vec<Token>) -> Self {
        Parser { tokens, pos: 0 }
    }

    /// Parse the whole token stream into a module body (a list of top-level
    /// statements). Stops and returns at the first error.
    pub fn parse(mut self) -> PResult<Vec<Stmt>> {
        let mut out = Vec::new();
        while !self.check(&TokenKind::Eof) {
            if self.eat(&TokenKind::Newline) {
                continue;
            }
            self.parse_line(&mut out)?;
        }
        Ok(out)
    }

    // --- Statement dispatch --------------------------------------------------

    /// Parse one logical line: a single compound statement, or a single simple
    /// statement terminated by a newline.
    fn parse_line(&mut self, out: &mut Vec<Stmt>) -> PResult<()> {
        if self.check(&TokenKind::At) {
            return Err(self.error("decorators are not supported in Oro"));
        }
        if self.at_match_stmt() {
            let s = self.match_stmt()?;
            out.push(s);
            Ok(())
        } else if self.is_compound_start() {
            let s = self.compound_statement()?;
            out.push(s);
            Ok(())
        } else {
            self.simple_line(out)
        }
    }

    /// `match` is a soft keyword: it opens a `match` statement only when a
    /// subject expression follows it. `match = 1`, `match(x)`, `match.y`, and a
    /// bare `match` stay ordinary identifiers. (A subject may not begin with
    /// `(`/`[`/`{`; write `match value:` rather than `match (value):`.)
    fn at_match_stmt(&self) -> bool {
        if !matches!(self.cur_kind(), TokenKind::Ident(n) if n == "match") {
            return false;
        }
        matches!(
            self.peek_kind(),
            TokenKind::Int(_)
                | TokenKind::Float(_)
                | TokenKind::Str(..)
                | TokenKind::Bytes(..)
                | TokenKind::FString(_)
                | TokenKind::True
                | TokenKind::False
                | TokenKind::None
                | TokenKind::Ident(_)
                | TokenKind::Minus
                | TokenKind::Not
        )
    }

    fn is_compound_start(&self) -> bool {
        matches!(
            self.cur_kind(),
            TokenKind::If
                | TokenKind::While
                | TokenKind::For
                | TokenKind::Def
                | TokenKind::Class
                | TokenKind::Try
        )
    }

    /// A single simple statement, terminated by a newline (or end of block).
    ///
    /// Semicolons are deliberately not a statement separator in Oro: they are a
    /// second way to write a block, which the one-way-to-do-each-thing thesis
    /// rules out. The `;` token still lexes, so we can reject it by name.
    fn simple_line(&mut self, out: &mut Vec<Stmt>) -> PResult<()> {
        let s = self.simple_statement()?;
        out.push(s);
        if self.check(&TokenKind::Semicolon) {
            return Err(self.error(
                "semicolons are not supported in Oro — put each statement on its own line",
            ));
        }
        if self.eat(&TokenKind::Newline)
            || self.check(&TokenKind::Eof)
            || self.check(&TokenKind::Dedent)
        {
            return Ok(());
        }
        Err(self.error(format!(
            "expected a newline to end the statement, found {}",
            describe(self.cur_kind())
        )))
    }

    fn compound_statement(&mut self) -> PResult<Stmt> {
        match self.cur_kind() {
            TokenKind::If => self.if_stmt(),
            TokenKind::While => self.while_stmt(),
            TokenKind::For => self.for_stmt(),
            TokenKind::Def => self.def_stmt(),
            TokenKind::Class => self.class_stmt(),
            TokenKind::Try => self.try_stmt(),
            _ => unreachable!("compound_statement called on a non-compound token"),
        }
    }

    fn simple_statement(&mut self) -> PResult<Stmt> {
        let (line, col) = self.cur_pos();
        match self.cur_kind() {
            TokenKind::Return => {
                self.advance();
                let value = if self.at_line_end() {
                    None
                } else {
                    Some(self.expr_list()?)
                };
                Ok(Stmt::Return { value, line, col })
            }
            TokenKind::Break => {
                self.advance();
                Ok(Stmt::Break { line, col })
            }
            TokenKind::Continue => {
                self.advance();
                Ok(Stmt::Continue { line, col })
            }
            TokenKind::Pass => {
                self.advance();
                Ok(Stmt::Pass { line, col })
            }
            TokenKind::Raise => {
                self.advance();
                if self.at_line_end() {
                    return Ok(Stmt::Raise { exc: None, line, col });
                }
                let exc = self.expression()?;
                if matches!(self.cur_kind(), TokenKind::Ident(n) if n == "from") {
                    return Err(self.error(
                        "`raise X from Y` is not supported in Oro — raise the exception on its own",
                    ));
                }
                Ok(Stmt::Raise { exc: Some(exc), line, col })
            }
            TokenKind::Ident(n) if n == "global" => self.global_stmt(line, col),
            TokenKind::Import => self.import_stmt(line, col),
            TokenKind::Yield => {
                self.advance();
                let value = if self.at_line_end() {
                    None
                } else {
                    Some(self.expr_list()?)
                };
                Ok(Stmt::Yield { value, line, col })
            }
            _ => self.expr_or_assign_stmt(line, col),
        }
    }

    // --- Simple statements ---------------------------------------------------

    fn global_stmt(&mut self, line: usize, col: usize) -> PResult<Stmt> {
        self.advance(); // `global`
        let mut names = vec![self.expect_ident("a name after `global`")?.0];
        while self.eat(&TokenKind::Comma) {
            names.push(self.expect_ident("a name after `,` in a `global` statement")?.0);
        }
        Ok(Stmt::Global { names, line, col })
    }

    fn import_stmt(&mut self, line: usize, col: usize) -> PResult<Stmt> {
        self.advance(); // `import`
        if self.check(&TokenKind::Star) {
            return Err(self.error("`import *` is not supported in Oro — name the modules you need"));
        }
        let mut path = vec![self.expect_ident("a module name after `import`")?.0];
        while self.eat(&TokenKind::Dot) {
            if self.check(&TokenKind::Star) {
                return Err(self.error(
                    "`import *` is not supported in Oro — name the modules you need",
                ));
            }
            path.push(self.expect_ident("a name after `.` in the import path")?.0);
        }
        let alias = if self.eat(&TokenKind::As) {
            Some(self.expect_ident("a name after `as`")?.0)
        } else {
            None
        };
        // A bare multi-segment import would bind the last segment (Go-style),
        // which is NOT what Python does — Python binds the first. Requiring `as`
        // makes the binding explicit, so `import a.b.c as c` means the same
        // thing to both readers and to CPython. Oro no longer promises full
        // semantic parity, but a silently *different* binding for identical
        // syntax is still the worst of both worlds.
        if path.len() > 1 && alias.is_none() {
            return Err(self.error(format!(
                "a multi-segment import must use `as` to name the binding: write \
                 `import {p} as {last}` (Python binds the first segment here, Oro the last, so \
                 Oro requires you to say which)",
                p = path.join("."),
                last = path.last().unwrap(),
            )));
        }
        Ok(Stmt::Import { path, alias, line, col })
    }

    /// A bare expression, or an assignment / augmented assignment.
    fn expr_or_assign_stmt(&mut self, line: usize, col: usize) -> PResult<Stmt> {
        let first = self.expr_list()?;

        // `x := ...` — the walrus lexes as `:` then `=`.
        if self.check(&TokenKind::Colon) && *self.peek_kind() == TokenKind::Eq {
            return Err(self.error("the walrus operator `:=` is not supported in Oro"));
        }

        // `x: int = 5` / `x: int` — a variable annotation, on the three targets
        // Python allows one on. Already an error before this arm existed, but
        // one that named the colon rather than the feature.
        if self.check(&TokenKind::Colon)
            && matches!(
                first,
                Expr::Name { .. } | Expr::Attribute { .. } | Expr::Subscript { .. }
            )
        {
            return Err(self.error(ANNOTATION_CUT));
        }

        match self.cur_kind() {
            TokenKind::Eq => {
                self.advance();
                let targets = vec![first];
                let value = self.expr_list()?;
                if self.check(&TokenKind::Eq) {
                    return Err(self.error(CHAINED_ASSIGN_CUT));
                }
                Ok(Stmt::Assign { targets, value, line, col })
            }
            TokenKind::PlusEq => self.aug_assign(first, AugOp::Add, line, col),
            TokenKind::MinusEq => self.aug_assign(first, AugOp::Sub, line, col),
            TokenKind::StarEq => self.aug_assign(first, AugOp::Mul, line, col),
            TokenKind::SlashEq => self.aug_assign(first, AugOp::Div, line, col),
            _ => Ok(Stmt::Expr { value: first, line, col }),
        }
    }

    fn aug_assign(&mut self, target: Expr, op: AugOp, line: usize, col: usize) -> PResult<Stmt> {
        self.advance(); // the += / -= / *= / /= token
        let value = self.expr_list()?;
        Ok(Stmt::AugAssign { target, op, value, line, col })
    }

    // --- Compound statements -------------------------------------------------

    fn if_stmt(&mut self) -> PResult<Stmt> {
        let (line, col) = self.cur_pos();
        self.advance(); // `if`
        let cond = self.expression()?;
        let body = self.block()?;

        let mut elifs = Vec::new();
        while self.check(&TokenKind::Elif) {
            self.advance();
            let cond = self.expression()?;
            let body = self.block()?;
            elifs.push((cond, body));
        }

        let orelse = if self.eat(&TokenKind::Else) {
            Some(self.block()?)
        } else {
            None
        };

        Ok(Stmt::If { cond, body, elifs, orelse, line, col })
    }

    /// `match SUBJECT:` followed by an indented block of `case` clauses.
    fn match_stmt(&mut self) -> PResult<Stmt> {
        let (line, col) = self.cur_pos();
        self.advance(); // soft-keyword `match`
        let subject = self.expression()?;
        self.expect(&TokenKind::Colon, "`:` after the match subject")?;
        if !self.eat(&TokenKind::Newline) {
            return Err(self.error("the body of a `match` must be on its own indented line"));
        }
        self.expect(&TokenKind::Indent, "an indented block of `case` clauses")?;

        let mut cases = Vec::new();
        while !self.check(&TokenKind::Dedent) && !self.check(&TokenKind::Eof) {
            if self.eat(&TokenKind::Newline) {
                continue;
            }
            if !matches!(self.cur_kind(), TokenKind::Ident(n) if n == "case") {
                return Err(self.error(format!(
                    "expected a `case` clause inside `match`, found {}",
                    describe(self.cur_kind())
                )));
            }
            cases.push(self.case_clause()?);
        }
        self.eat(&TokenKind::Dedent);
        if cases.is_empty() {
            return Err(self.error("a `match` needs at least one `case` clause"));
        }
        // `case _` is irrefutable: any case after it is unreachable. CPython
        // makes this a SyntaxError, so Oro rejects it too — otherwise a valid
        // Oro program would not be valid Python.
        if let Some(pos) = cases.iter().position(|c| c.pattern == Pattern::Wildcard) {
            if pos != cases.len() - 1 {
                return Err(self.error(
                    "`case _` matches everything, so the cases after it can never run — put the \
                     wildcard last (this matches CPython, which rejects it as a SyntaxError).",
                ));
            }
        }
        Ok(Stmt::Match { subject, cases, line, col })
    }

    /// `case PATTERN:` and its block. `case` is a soft keyword recognised only
    /// here, at the head of a clause inside a `match`.
    fn case_clause(&mut self) -> PResult<MatchCase> {
        let (line, col) = self.cur_pos();
        self.advance(); // soft-keyword `case`
        let pattern = self.case_pattern()?;
        // `case_pattern` stops on the `:`; `block` consumes it.
        let body = self.block()?;
        Ok(MatchCase { pattern, body, line, col })
    }

    /// Parse one case pattern — the value-only subset — leaving the cursor on
    /// the trailing `:`. Every rejected Python pattern form gets its own
    /// diagnostic explaining why Oro does not adopt it.
    fn case_pattern(&mut self) -> PResult<Pattern> {
        let pat = self.core_pattern()?;
        // Reject the pattern *combinators* that turn a switch into matching.
        match self.cur_kind() {
            TokenKind::Pipe => Err(self.error(
                "or-patterns (`case a | b:`) are not supported in Oro — write separate `case` \
                 clauses with the same body. `|` means \"or\" only in languages without an `or` \
                 keyword; Oro has `or`, so it does not reuse `|` for alternation.",
            )),
            TokenKind::As => Err(self.error(
                "as-patterns (`case PATTERN as name:`) are not supported in Oro — a `case` may \
                 not bind names; use the matched value directly.",
            )),
            TokenKind::If => Err(self.error(
                "guards (`case PATTERN if cond:`) are not supported in Oro — use a nested `if` \
                 inside the case body, or an `if`/`elif` chain instead of `match`.",
            )),
            TokenKind::Colon => Ok(pat),
            other => Err(self.error(format!(
                "expected `:` after the case pattern, found {}",
                describe(other)
            ))),
        }
    }

    /// The core of a case pattern: a literal, a dotted name, or `_`.
    fn core_pattern(&mut self) -> PResult<Pattern> {
        match self.cur_kind() {
            TokenKind::LBracket => Err(self.error(
                "sequence patterns (`case [a, b]:`) are not supported in Oro — `match` is a \
                 value switch, not destructuring. Compare a whole value, or index the sequence \
                 inside the case body.",
            )),
            TokenKind::LBrace => Err(self.error(
                "mapping patterns (`case {\"k\": v}:`) are not supported in Oro — `match` is a \
                 value switch, not destructuring. Look keys up inside the case body.",
            )),
            TokenKind::Int(_)
            | TokenKind::Float(_)
            | TokenKind::Str(..)
            | TokenKind::Bytes(..)
            | TokenKind::True
            | TokenKind::False
            | TokenKind::None
            | TokenKind::Minus => {
                let expr = self.pattern_literal()?;
                Ok(Pattern::Literal(expr))
            }
            TokenKind::FString(_) => Err(self.error(
                "an f-string is not a valid pattern — a `case` needs a constant literal or a \
                 dotted name.",
            )),
            TokenKind::Ident(name) => {
                let name = name.clone();
                if name == "_" && !matches!(self.peek_kind(), TokenKind::Dot) {
                    self.advance();
                    return Ok(Pattern::Wildcard);
                }
                match self.peek_kind() {
                    TokenKind::Dot => self.pattern_dotted(),
                    TokenKind::LParen => Err(self.error(format!(
                        "class patterns (`case {name}(...):`) are not supported in Oro — there \
                         is no destructuring; compare a value or a dotted name like `Enum.MEMBER`."
                    ))),
                    _ => Err(self.error(format!(
                        "`case {name}:` is a bare capture name. In Python this SILENTLY REBINDS \
                         `{name}` and matches everything — a well-known footgun. Oro rejects it \
                         on purpose: write a literal (`case 1:`, `case \"{name}\":`) or a dotted \
                         name (`case Enum.{name}:`); use `case _:` for the default."
                    ))),
                }
            }
            other => Err(self.error(format!(
                "expected a case pattern, found {}",
                describe(other)
            ))),
        }
    }

    /// A literal pattern value: an optionally-negated number, a string, or one
    /// of `True`/`False`/`None`.
    fn pattern_literal(&mut self) -> PResult<Expr> {
        let (line, col) = self.cur_pos();
        if self.eat(&TokenKind::Minus) {
            let operand = match self.cur_kind() {
                TokenKind::Int(_) | TokenKind::Float(_) => self.atom()?,
                other => {
                    return Err(self.error(format!(
                        "expected a number after `-` in a case pattern, found {}",
                        describe(other)
                    )))
                }
            };
            return Ok(Expr::Unary { op: UnaryOp::Neg, operand: Box::new(operand), line, col });
        }
        // A plain literal atom.
        self.atom()
    }

    /// A dotted-name pattern: `A.B`, `A.B.C`, … matched by value at runtime.
    fn pattern_dotted(&mut self) -> PResult<Pattern> {
        let (line, col) = self.cur_pos();
        let name = self.expect_ident("a name at the start of a dotted pattern")?.0;
        let mut expr = Expr::Name { name, line, col };
        while self.eat(&TokenKind::Dot) {
            let attr = self.expect_ident("an attribute name after `.` in a case pattern")?.0;
            expr = Expr::Attribute { value: Box::new(expr), attr, line, col };
        }
        Ok(Pattern::Dotted(expr))
    }

    fn while_stmt(&mut self) -> PResult<Stmt> {
        let (line, col) = self.cur_pos();
        self.advance(); // `while`
        let cond = self.expression()?;
        let body = self.block()?;
        Ok(Stmt::While { cond, body, line, col })
    }

    fn for_stmt(&mut self) -> PResult<Stmt> {
        let (line, col) = self.cur_pos();
        self.advance(); // `for`
        let target = self.for_target()?;
        self.expect(&TokenKind::In, "`in` after the loop variable")?;
        let iter = self.expr_list()?;
        let body = self.block()?;
        Ok(Stmt::For { target, iter, body, line, col })
    }

    /// A `for` loop target: a name/attribute/subscript, or a tuple of them.
    /// Parsed as postfix atoms so a bare `in` is not mistaken for the comparison
    /// operator.
    fn for_target(&mut self) -> PResult<Expr> {
        let first = self.parse_postfix_atom()?;
        if !self.check(&TokenKind::Comma) {
            return Ok(first);
        }
        let (line, col) = first.pos();
        let mut elements = vec![first];
        while self.eat(&TokenKind::Comma) {
            if self.check(&TokenKind::In) {
                break; // trailing comma: `for a, in xs`
            }
            elements.push(self.parse_postfix_atom()?);
        }
        Ok(Expr::Tuple { elements, line, col })
    }

    fn def_stmt(&mut self) -> PResult<Stmt> {
        let (line, col) = self.cur_pos();
        self.advance(); // `def`
        let name = self.expect_ident("a function name after `def`")?.0;
        self.expect(&TokenKind::LParen, "`(` to start the parameter list")?;
        let params = self.param_list()?;
        self.expect(&TokenKind::RParen, "`)` to close the parameter list")?;
        if self.check(&TokenKind::Arrow) {
            return Err(self.error(ANNOTATION_CUT));
        }
        let body = self.block()?;
        Ok(Stmt::Def { name, params, body, line, col })
    }

    /// Parse a `def` parameter list, enforcing the fixed order: positional
    /// parameters, then defaulted ones, then a single `*args`, then a single
    /// `**kwargs`. Each ordering violation gets its own diagnostic.
    fn param_list(&mut self) -> PResult<Vec<Param>> {
        let mut params = Vec::new();
        let mut seen_default = false;
        let mut seen_varargs = false;
        let mut seen_kwargs = false;

        while !self.check(&TokenKind::RParen) {
            let (tok_line, tok_col) = self.cur_pos();

            if self.eat(&TokenKind::DoubleStar) {
                // `**kwargs`
                if seen_kwargs {
                    return Err(self.error_at(
                        "a function may have only one `**kwargs` parameter",
                        tok_line,
                        tok_col,
                    ));
                }
                let (name, line, col) = self.expect_ident("a parameter name after `**`")?;
                if self.check(&TokenKind::Colon) {
                    return Err(self.error(ANNOTATION_CUT));
                }
                params.push(Param {
                    name,
                    default: None,
                    kind: ParamKind::KwArgs,
                    line,
                    col,
                });
                seen_kwargs = true;
            } else if self.eat(&TokenKind::Star) {
                // `*args`
                if seen_kwargs {
                    return Err(self.error_at(
                        "`*args` must come before `**kwargs`",
                        tok_line,
                        tok_col,
                    ));
                }
                if seen_varargs {
                    return Err(self.error_at(
                        "a function may have only one `*args` parameter",
                        tok_line,
                        tok_col,
                    ));
                }
                let (name, line, col) = self.expect_ident("a parameter name after `*`")?;
                if self.check(&TokenKind::Colon) {
                    return Err(self.error(ANNOTATION_CUT));
                }
                params.push(Param {
                    name,
                    default: None,
                    kind: ParamKind::VarArgs,
                    line,
                    col,
                });
                seen_varargs = true;
            } else {
                // An ordinary parameter.
                if seen_kwargs {
                    return Err(self.error_at(
                        "`**kwargs` must be the last parameter",
                        tok_line,
                        tok_col,
                    ));
                }
                if seen_varargs {
                    return Err(self.error_at(
                        "a parameter cannot follow `*args` — only `**kwargs` may",
                        tok_line,
                        tok_col,
                    ));
                }
                let (name, line, col) = self.expect_ident("a parameter name")?;
                if self.check(&TokenKind::Colon) {
                    return Err(self.error(ANNOTATION_CUT));
                }
                let default = if self.eat(&TokenKind::Eq) {
                    seen_default = true;
                    Some(self.expression()?)
                } else {
                    if seen_default {
                        return Err(self.error_at(
                            "a required parameter cannot follow a defaulted parameter",
                            line,
                            col,
                        ));
                    }
                    None
                };
                params.push(Param {
                    name,
                    default,
                    kind: ParamKind::Normal,
                    line,
                    col,
                });
            }

            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        Ok(params)
    }

    fn class_stmt(&mut self) -> PResult<Stmt> {
        let (line, col) = self.cur_pos();
        self.advance(); // `class`
        let name = self.expect_ident("a class name after `class`")?.0;

        let base = if self.eat(&TokenKind::LParen) {
            if self.eat(&TokenKind::RParen) {
                None
            } else {
                // A class keyword argument such as `metaclass=` appears as a
                // name immediately followed by `=`.
                if matches!(self.cur_kind(), TokenKind::Ident(_))
                    && matches!(self.peek_kind(), TokenKind::Eq)
                {
                    return Err(self.error(
                        "class keyword arguments (metaclass=, etc.) are not supported in Oro — \
                         metaclasses are cut; a class takes at most a single positional base",
                    ));
                }
                let base = self.expression()?;
                if self.check(&TokenKind::Comma) {
                    return Err(self.error(
                        "multiple inheritance is not supported in Oro — a class may have at most one base",
                    ));
                }
                self.expect(&TokenKind::RParen, "`)` to close the base class list")?;
                Some(base)
            }
        } else {
            None
        };

        let body = self.block()?;
        Ok(Stmt::Class { name, base, body, line, col })
    }

    fn try_stmt(&mut self) -> PResult<Stmt> {
        let (line, col) = self.cur_pos();
        self.advance(); // `try`
        let body = self.block()?;

        let mut handlers = Vec::new();
        while self.check(&TokenKind::Except) {
            let (hline, hcol) = self.cur_pos();
            self.advance(); // `except`
            if self.check(&TokenKind::Colon) {
                return Err(self.error(
                    "bare `except:` is not supported in Oro — catch a specific exception type",
                ));
            }
            let exc_type = self.expression()?;
            let name = if self.eat(&TokenKind::As) {
                Some(self.expect_ident("an exception name after `as`")?.0)
            } else {
                None
            };
            let hbody = self.block()?;
            handlers.push(ExceptHandler {
                exc_type,
                name,
                body: hbody,
                line: hline,
                col: hcol,
            });
        }

        if self.check(&TokenKind::Else) {
            return Err(self.error(
                "try/except/else is not supported in Oro — put the else code after the `try` \
                 block, or inside the `try`",
            ));
        }

        let finalbody = if self.eat(&TokenKind::Finally) {
            Some(self.block()?)
        } else {
            None
        };

        if handlers.is_empty() && finalbody.is_none() {
            return Err(self.error(
                "`try` must be followed by at least one `except` or a `finally` block",
            ));
        }

        Ok(Stmt::Try { body, handlers, finalbody, line, col })
    }

    // --- Blocks --------------------------------------------------------------

    /// Parse the `:`-introduced suite of a compound statement.
    ///
    /// Only the indented block form is accepted. The inline single-line form
    /// (`if x: y`) is deliberately cut: it is a second way to write a block, and
    /// it is a large part of why Python needs autoformatters.
    fn block(&mut self) -> PResult<Vec<Stmt>> {
        self.expect(&TokenKind::Colon, "`:` to start the block")?;

        if !self.eat(&TokenKind::Newline) {
            return Err(self.error("the body of a block must be on its own indented line"));
        }
        self.expect(&TokenKind::Indent, "an indented block")?;
        let mut body = Vec::new();
        while !self.check(&TokenKind::Dedent) && !self.check(&TokenKind::Eof) {
            if self.eat(&TokenKind::Newline) {
                continue;
            }
            self.parse_line(&mut body)?;
        }
        self.eat(&TokenKind::Dedent);
        if body.is_empty() {
            return Err(self.error("expected an indented block"));
        }
        Ok(body)
    }

    // --- Expressions (Pratt) -------------------------------------------------

    /// Parse a single expression at the loosest precedence.
    fn expression(&mut self) -> PResult<Expr> {
        self.parse_expr(0)
    }

    /// Parse an expression, then fold a trailing comma-separated run into a bare
    /// tuple. Used where Python allows unparenthesised tuples: statement value
    /// positions, assignment sides, `return`, and `for ... in <iter>`.
    fn expr_list(&mut self) -> PResult<Expr> {
        let first = self.expression()?;
        if !self.check(&TokenKind::Comma) {
            return Ok(first);
        }
        let (line, col) = first.pos();
        let mut elements = vec![first];
        while self.eat(&TokenKind::Comma) {
            if !self.can_start_expr() {
                break; // trailing comma
            }
            elements.push(self.expression()?);
        }
        Ok(Expr::Tuple { elements, line, col })
    }

    /// The Pratt core: parse an expression whose operators all bind at least as
    /// tightly as `min_bp`.
    fn parse_expr(&mut self, min_bp: u8) -> PResult<Expr> {
        let mut left = self.parse_prefix(min_bp)?;

        // `x => body` / `(a, b) => body`. The parameter list is only recognised
        // as one once `=>` is seen, so it arrives here already parsed as an
        // expression and is converted back. This keeps the atom grammar
        // unchanged and needs no lookahead.
        if matches!(self.cur_kind(), TokenKind::FatArrow) {
            let (line, col) = left.pos();
            self.advance();
            let params = lambda_params(&left).ok_or_else(|| {
                self.error(
                    "the left of `=>` must be a parameter name or a parenthesised list of them, \
                     e.g. `x => x * 2` or `(a, b) => a + b`",
                )
            })?;
            let body = self.parse_expr(min_bp)?;
            return Ok(Expr::Lambda {
                data: Box::new(crate::ast::LambdaData {
                    params,
                    body: Box::new(body),
                    scope: std::cell::Cell::new(usize::MAX),
                }),
                line,
                col,
            });
        }

        loop {
            // `is` is still a reserved word, so the message lands on the
            // operator itself rather than on whatever follows it.
            if matches!(self.cur_kind(), TokenKind::Is) {
                return Err(self.cut_is());
            }

            // Comparison operators chain, so they are handled as a group rather
            // than as ordinary left-associative infix operators.
            if self.peek_compare().is_some() {
                if CMP_BP < min_bp {
                    break;
                }
                let (line, col) = left.pos();
                let mut rest = Vec::new();
                while let Some((op, consume)) = self.peek_compare() {
                    for _ in 0..consume {
                        self.advance();
                    }
                    let rhs = self.parse_expr(CMP_BP + 1)?;
                    rest.push((op, rhs));
                }
                left = Expr::Compare { first: Box::new(left), rest, line, col };
                continue;
            }

            let (lbp, rbp) = match self.infix_bp() {
                Some(bp) => bp,
                None => break,
            };
            if lbp < min_bp {
                break;
            }

            let op_tok = self.advance();
            let (line, col) = left.pos();
            let right = self.parse_expr(rbp)?;
            left = build_infix(&op_tok.kind, left, right, line, col);
        }

        Ok(left)
    }

    /// Parse a prefix position: the unary operators `not`, `-`, `+`, then an
    /// atom with its postfix trailers.
    fn parse_prefix(&mut self, min_bp: u8) -> PResult<Expr> {
        match self.cur_kind() {
            TokenKind::Not => {
                // `not` binds looser than comparison; reject it where an operand
                // that tight is required (e.g. `2 * not x`).
                if min_bp > NOT_BP {
                    return Err(self.error("unexpected keyword `not` in this position"));
                }
                let (line, col) = self.cur_pos();
                self.advance();
                let operand = self.parse_expr(NOT_BP)?;
                Ok(Expr::Unary { op: UnaryOp::Not, operand: Box::new(operand), line, col })
            }
            TokenKind::Minus => {
                let (line, col) = self.cur_pos();
                self.advance();
                let operand = self.parse_expr(UNARY_BP)?;
                Ok(Expr::Unary { op: UnaryOp::Neg, operand: Box::new(operand), line, col })
            }
            TokenKind::Plus => {
                let (line, col) = self.cur_pos();
                self.advance();
                let operand = self.parse_expr(UNARY_BP)?;
                Ok(Expr::Unary { op: UnaryOp::Pos, operand: Box::new(operand), line, col })
            }
            TokenKind::Is => Err(self.cut_is()),
            _ => self.parse_postfix_atom(),
        }
    }

    /// Parse an atom, then apply postfix trailers (call, attribute, subscript),
    /// which bind more tightly than any binary operator.
    fn parse_postfix_atom(&mut self) -> PResult<Expr> {
        let mut expr = self.atom()?;
        loop {
            match self.cur_kind() {
                TokenKind::LParen => expr = self.finish_call(expr)?,
                TokenKind::Dot => {
                    self.advance();
                    let (attr, _, _) = self.expect_ident("an attribute name after `.`")?;
                    let (line, col) = expr.pos();
                    expr = Expr::Attribute { value: Box::new(expr), attr, line, col };
                }
                TokenKind::LBracket => expr = self.finish_subscript(expr)?,
                _ => break,
            }
        }
        Ok(expr)
    }

    fn finish_call(&mut self, func: Expr) -> PResult<Expr> {
        let (line, col) = func.pos();
        self.advance(); // `(`
        let mut args = Vec::new();
        let mut kwargs = Vec::new();
        while !self.check(&TokenKind::RParen) {
            if self.eat(&TokenKind::DoubleStar) {
                // `**mapping` keyword unpacking.
                let value = self.expression()?;
                kwargs.push(Kwarg::DoubleStar(value));
            } else if self.eat(&TokenKind::Star) {
                // `*iterable` positional unpacking.
                if !kwargs.is_empty() {
                    return Err(self.error(
                        "positional arguments cannot follow keyword arguments",
                    ));
                }
                let value = self.expression()?;
                args.push(Arg::Star(value));
            } else if matches!(self.cur_kind(), TokenKind::Ident(_))
                && *self.peek_kind() == TokenKind::Eq
            {
                // Keyword argument: `name = value`, distinguished from `==`.
                let (name, _, _) = self.expect_ident("a keyword argument name")?;
                self.advance(); // `=`
                let value = self.expression()?;
                kwargs.push(Kwarg::Keyword(name, value));
            } else {
                if !kwargs.is_empty() {
                    return Err(self.error(
                        "positional arguments cannot follow keyword arguments",
                    ));
                }
                args.push(Arg::Positional(self.expression()?));
            }
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RParen, "`)` to close the argument list")?;
        Ok(Expr::Call { func: Box::new(func), args, kwargs, line, col })
    }

    fn finish_subscript(&mut self, value: Expr) -> PResult<Expr> {
        let (line, col) = value.pos();
        self.advance(); // `[`

        let lower = if self.check(&TokenKind::Colon) || self.check(&TokenKind::RBracket) {
            None
        } else {
            Some(Box::new(self.expression()?))
        };

        if self.eat(&TokenKind::Colon) {
            // Slice form.
            let upper = if self.check(&TokenKind::Colon) || self.check(&TokenKind::RBracket) {
                None
            } else {
                Some(Box::new(self.expression()?))
            };
            let step = if self.eat(&TokenKind::Colon) {
                if self.check(&TokenKind::RBracket) {
                    None
                } else {
                    Some(Box::new(self.expression()?))
                }
            } else {
                None
            };
            self.expect(&TokenKind::RBracket, "`]` to close the slice")?;
            Ok(Expr::Slice { value: Box::new(value), lower, upper, step, line, col })
        } else {
            let index = match lower {
                Some(e) => e,
                None => return Err(self.error("expected an index expression inside `[ ]`")),
            };
            self.expect(&TokenKind::RBracket, "`]` to close the subscript")?;
            Ok(Expr::Subscript { value: Box::new(value), index, line, col })
        }
    }

    /// Parse a primary expression: a literal, name, or a parenthesised /
    /// bracketed / braced collection.
    fn atom(&mut self) -> PResult<Expr> {
        let tok = self.cur().clone();
        let (line, col) = (tok.line, tok.col);
        match tok.kind {
            TokenKind::Int(value) => {
                self.advance();
                Ok(Expr::Int { value, line, col })
            }
            TokenKind::Float(value) => {
                self.advance();
                Ok(Expr::Float { value, line, col })
            }
            TokenKind::Str(value, raw) => {
                self.advance();
                Ok(Expr::Str { value, raw, line, col })
            }
            TokenKind::Bytes(value, raw) => {
                self.advance();
                Ok(Expr::Bytes { value, raw, line, col })
            }
            TokenKind::FString(value) => {
                self.advance();
                Ok(Expr::FString { value, line, col })
            }
            TokenKind::True => {
                self.advance();
                Ok(Expr::Bool { value: true, line, col })
            }
            TokenKind::False => {
                self.advance();
                Ok(Expr::Bool { value: false, line, col })
            }
            TokenKind::None => {
                self.advance();
                Ok(Expr::NoneLit { line, col })
            }
            TokenKind::Ident(name) => {
                if let Some(msg) = cut_keyword_message(&name) {
                    return Err(self.error_at(msg, line, col));
                }
                self.advance();
                Ok(Expr::Name { name, line, col })
            }
            TokenKind::LParen => self.group_or_tuple(line, col),
            TokenKind::LBracket => self.list_literal(line, col),
            TokenKind::LBrace => self.dict_stmt(line, col),
            other => Err(self.error(format!(
                "expected an expression, found {}",
                describe(&other)
            ))),
        }
    }

    fn group_or_tuple(&mut self, line: usize, col: usize) -> PResult<Expr> {
        self.advance(); // `(`
        if self.eat(&TokenKind::RParen) {
            return Ok(Expr::Tuple { elements: Vec::new(), line, col }); // `()`
        }

        let first = self.expression()?;

        if self.check(&TokenKind::Colon) && *self.peek_kind() == TokenKind::Eq {
            return Err(self.error("the walrus operator `:=` is not supported in Oro"));
        }
        if self.check(&TokenKind::For) {
            return Err(self.error(
                "generator expressions are not supported in Oro — build a list or use a loop",
            ));
        }

        if self.check(&TokenKind::Comma) {
            let mut elements = vec![first];
            while self.eat(&TokenKind::Comma) {
                if self.check(&TokenKind::RParen) {
                    break;
                }
                elements.push(self.expression()?);
            }
            self.expect(&TokenKind::RParen, "`)` to close the tuple")?;
            Ok(Expr::Tuple { elements, line, col })
        } else {
            // A single parenthesised expression is just that expression; the
            // grouping introduces no node of its own.
            self.expect(&TokenKind::RParen, "`)` to close the parenthesised expression")?;
            Ok(first)
        }
    }

    fn list_literal(&mut self, line: usize, col: usize) -> PResult<Expr> {
        self.advance(); // `[`
        if self.eat(&TokenKind::RBracket) {
            return Ok(Expr::List { elements: Vec::new(), line, col });
        }
        let first = self.expression()?;
        if self.check(&TokenKind::For) {
            return Err(self.error("list comprehensions are not supported in Oro — use a loop"));
        }
        let mut elements = vec![first];
        while self.eat(&TokenKind::Comma) {
            if self.check(&TokenKind::RBracket) {
                break;
            }
            elements.push(self.expression()?);
        }
        self.expect(&TokenKind::RBracket, "`]` to close the list")?;
        Ok(Expr::List { elements, line, col })
    }

    /// `{ ... }` is always a dict — sets are cut, so there is no ambiguity and
    /// no colon-lookahead. `{}` is the empty dict, matching Python.
    fn dict_stmt(&mut self, line: usize, col: usize) -> PResult<Expr> {
        self.advance(); // `{`
        if self.eat(&TokenKind::RBrace) {
            return Ok(Expr::Dict { entries: Vec::new(), line, col });
        }

        let first = self.expression()?;
        // A `{...}` without `:` was a set literal — now a designed error.
        if !self.check(&TokenKind::Colon) {
            return Err(self.error(
                "set literals are not supported in Oro — sets are cut. Use a dict for membership \
                 (`{1: True, 2: True}`) or a list; a Set data structure may return in the stdlib.",
            ));
        }
        self.advance(); // `:`
        let value = self.expression()?;
        if self.check(&TokenKind::For) {
            return Err(self.error("dict comprehensions are not supported in Oro — use a loop"));
        }
        let mut entries = vec![(first, value)];
        while self.eat(&TokenKind::Comma) {
            if self.check(&TokenKind::RBrace) {
                break;
            }
            let key = self.expression()?;
            self.expect(&TokenKind::Colon, "`:` between a dict key and value")?;
            let value = self.expression()?;
            entries.push((key, value));
        }
        self.expect(&TokenKind::RBrace, "`}` to close the dict")?;
        Ok(Expr::Dict { entries, line, col })
    }

    // --- Operator tables -----------------------------------------------------

    /// The diagnostic for `is` / `is not`, which were cut.
    ///
    /// `==` and `!=` already compare every reference type by identity, so `is`
    /// was a second spelling of an answer Oro already had — except in the one
    /// place it disagreed, where its answer was unfixable: `"hel" + "lo" is
    /// "hello"` is a question about CPython's string-interning table, not about
    /// the program, and Oro has no such table to consult.
    fn cut_is(&self) -> ParseError {
        if *self.peek_kind() == TokenKind::Not {
            self.error(
                "`is not` is not in Oro — use `!=`, which already compares \
                 reference types by identity",
            )
        } else {
            self.error(
                "`is` is not in Oro — use `==`, which already compares \
                 reference types by identity",
            )
        }
    }

    /// If the current position is the start of a comparison operator, return its
    /// [`CmpOp`] and how many tokens it spans (`not in` spans two).
    fn peek_compare(&self) -> Option<(CmpOp, usize)> {
        match self.cur_kind() {
            TokenKind::EqEq => Some((CmpOp::Eq, 1)),
            TokenKind::NotEq => Some((CmpOp::NotEq, 1)),
            TokenKind::Lt => Some((CmpOp::Lt, 1)),
            TokenKind::Gt => Some((CmpOp::Gt, 1)),
            TokenKind::LtEq => Some((CmpOp::LtEq, 1)),
            TokenKind::GtEq => Some((CmpOp::GtEq, 1)),
            TokenKind::In => Some((CmpOp::In, 1)),
            TokenKind::Not if *self.peek_kind() == TokenKind::In => Some((CmpOp::NotIn, 2)),
            _ => None,
        }
    }

    /// The `(left, right)` binding powers of the current infix arithmetic /
    /// logical operator, or `None` if the current token is not one. Comparisons
    /// are handled separately (they chain).
    fn infix_bp(&self) -> Option<(u8, u8)> {
        Some(match self.cur_kind() {
            TokenKind::Or => (1, 2),
            TokenKind::And => (2, 3),
            TokenKind::Plus | TokenKind::Minus => (5, 6),
            TokenKind::Star | TokenKind::Slash | TokenKind::DoubleSlash | TokenKind::Percent => {
                (6, 7)
            }
            // `**` is right-associative: recurse at its own level, not one tighter.
            TokenKind::DoubleStar => (8, 8),
            _ => return None,
        })
    }

    // --- Token cursor helpers ------------------------------------------------

    fn cur(&self) -> &Token {
        // The token stream always ends in Eof, so the last index is valid.
        &self.tokens[self.pos.min(self.tokens.len() - 1)]
    }

    fn cur_kind(&self) -> &TokenKind {
        &self.cur().kind
    }

    fn cur_pos(&self) -> (usize, usize) {
        let t = self.cur();
        (t.line, t.col)
    }

    /// The kind of the token *after* the current one (Eof if past the end).
    fn peek_kind(&self) -> &TokenKind {
        let i = (self.pos + 1).min(self.tokens.len() - 1);
        &self.tokens[i].kind
    }

    fn check(&self, kind: &TokenKind) -> bool {
        self.cur_kind() == kind
    }

    fn advance(&mut self) -> Token {
        let tok = self.cur().clone();
        if !matches!(tok.kind, TokenKind::Eof) {
            self.pos += 1;
        }
        tok
    }

    fn eat(&mut self, kind: &TokenKind) -> bool {
        if self.check(kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: &TokenKind, what: &str) -> PResult<Token> {
        if self.check(kind) {
            Ok(self.advance())
        } else {
            Err(self.error(format!("expected {what}, found {}", describe(self.cur_kind()))))
        }
    }

    fn expect_ident(&mut self, what: &str) -> PResult<(String, usize, usize)> {
        let tok = self.cur().clone();
        if let TokenKind::Ident(name) = tok.kind {
            self.advance();
            Ok((name, tok.line, tok.col))
        } else {
            Err(self.error(format!("expected {what}, found {}", describe(&tok.kind))))
        }
    }

    /// True at a point where a simple statement may end.
    fn at_line_end(&self) -> bool {
        matches!(
            self.cur_kind(),
            TokenKind::Newline | TokenKind::Semicolon | TokenKind::Eof | TokenKind::Dedent
        )
    }

    /// True if the current token can begin an expression (used to detect a
    /// trailing comma in a bare tuple).
    fn can_start_expr(&self) -> bool {
        matches!(
            self.cur_kind(),
            TokenKind::Int(_)
                | TokenKind::Float(_)
                | TokenKind::Str(..)
                | TokenKind::Bytes(..)
                | TokenKind::FString(_)
                | TokenKind::True
                | TokenKind::False
                | TokenKind::None
                | TokenKind::Ident(_)
                | TokenKind::LParen
                | TokenKind::LBracket
                | TokenKind::LBrace
                | TokenKind::Minus
                | TokenKind::Plus
                | TokenKind::Not
        )
    }

    fn error(&self, message: impl Into<String>) -> ParseError {
        let (line, col) = self.cur_pos();
        ParseError { message: message.into(), line, col }
    }

    fn error_at(&self, message: impl Into<String>, line: usize, col: usize) -> ParseError {
        ParseError { message: message.into(), line, col }
    }
}

/// Build an infix arithmetic / logical node from an operator token.
fn build_infix(op: &TokenKind, left: Expr, right: Expr, line: usize, col: usize) -> Expr {
    let boxed = |l: Expr, r: Expr| (Box::new(l), Box::new(r));
    match op {
        TokenKind::Or => {
            let (left, right) = boxed(left, right);
            Expr::BoolOp { op: BoolOp::Or, left, right, line, col }
        }
        TokenKind::And => {
            let (left, right) = boxed(left, right);
            Expr::BoolOp { op: BoolOp::And, left, right, line, col }
        }
        _ => {
            let bin = match op {
                TokenKind::Plus => BinOp::Add,
                TokenKind::Minus => BinOp::Sub,
                TokenKind::Star => BinOp::Mul,
                TokenKind::Slash => BinOp::Div,
                TokenKind::DoubleSlash => BinOp::FloorDiv,
                TokenKind::Percent => BinOp::Mod,
                TokenKind::DoubleStar => BinOp::Pow,
                _ => unreachable!("build_infix called on a non-infix token"),
            };
            let (left, right) = boxed(left, right);
            Expr::Binary { op: bin, left, right, line, col }
        }
    }
}

/// Reinterpret an already-parsed expression as a lambda parameter list. Only
/// plain names are accepted — no defaults and no `*args`; a lambda
/// that needs those is a `def`.
fn lambda_params(left: &Expr) -> Option<Vec<crate::ast::Param>> {
    fn one(e: &Expr) -> Option<crate::ast::Param> {
        match e {
            Expr::Name { name, line, col } => Some(crate::ast::Param {
                name: name.clone(),
                default: None,
                kind: crate::ast::ParamKind::Normal,
                line: *line,
                col: *col,
            }),
            _ => None,
        }
    }
    match left {
        Expr::Name { .. } => one(left).map(|p| vec![p]),
        // `()` parses as an empty tuple, `(a, b)` as a tuple: both are lists.
        Expr::Tuple { elements, .. } => elements.iter().map(one).collect(),
        _ => None,
    }
}

/// If `name` is an identifier standing in for a deliberately cut feature, return
/// the specific diagnostic explaining the design decision.
fn cut_keyword_message(name: &str) -> Option<String> {
    let msg = match name {
        "with" => {
            "the `with` statement is not supported in Oro — files close automatically at end of block"
        }
        "from" => "`from X import Y` is not supported in Oro — use `import X`",
        "lambda" => "Oro spells a lambda `x => x * 2` (or `(a, b) => a + b`); the `lambda` keyword is not used",
        "global" => "the `global` statement is not supported in Oro",
        "nonlocal" => "the `nonlocal` statement is not supported in Oro",
        "async" | "await" => "async/await is not supported in Oro",
        "del" => "the `del` statement is not supported in Oro",
        "assert" => "the `assert` statement is not supported in Oro",
        _ => return None,
    };
    Some(msg.to_string())
}

/// A short, human-readable description of a token for error messages.
fn describe(kind: &TokenKind) -> String {
    use TokenKind::*;
    match kind {
        Int(s) => format!("integer `{s}`"),
        Float(s) => format!("float `{s}`"),
        Str(..) => "a string literal".to_string(),
        Bytes(..) => "a bytes literal".to_string(),
        FString(_) => "an f-string literal".to_string(),
        Ident(s) => format!("identifier `{s}`"),
        True => "keyword `true`".to_string(),
        False => "keyword `false`".to_string(),
        None => "keyword `null`".to_string(),
        If => "keyword `if`".to_string(),
        Elif => "keyword `elif`".to_string(),
        Else => "keyword `else`".to_string(),
        For => "keyword `for`".to_string(),
        While => "keyword `while`".to_string(),
        In => "keyword `in`".to_string(),
        Def => "keyword `def`".to_string(),
        Class => "keyword `class`".to_string(),
        Return => "keyword `return`".to_string(),
        Break => "keyword `break`".to_string(),
        Continue => "keyword `continue`".to_string(),
        Try => "keyword `try`".to_string(),
        Except => "keyword `except`".to_string(),
        Finally => "keyword `finally`".to_string(),
        Raise => "keyword `raise`".to_string(),
        Import => "keyword `import`".to_string(),
        As => "keyword `as`".to_string(),
        Yield => "keyword `yield`".to_string(),
        And => "keyword `and`".to_string(),
        Or => "keyword `or`".to_string(),
        Not => "keyword `not`".to_string(),
        Is => "keyword `is`".to_string(),
        FatArrow => "`=>`".to_string(),
        Pass => "keyword `pass`".to_string(),
        Plus => "`+`".to_string(),
        Minus => "`-`".to_string(),
        Star => "`*`".to_string(),
        Slash => "`/`".to_string(),
        DoubleSlash => "`//`".to_string(),
        Percent => "`%`".to_string(),
        DoubleStar => "`**`".to_string(),
        Eq => "`=`".to_string(),
        PlusEq => "`+=`".to_string(),
        MinusEq => "`-=`".to_string(),
        StarEq => "`*=`".to_string(),
        SlashEq => "`/=`".to_string(),
        EqEq => "`==`".to_string(),
        NotEq => "`!=`".to_string(),
        Lt => "`<`".to_string(),
        Gt => "`>`".to_string(),
        LtEq => "`<=`".to_string(),
        GtEq => "`>=`".to_string(),
        LParen => "`(`".to_string(),
        RParen => "`)`".to_string(),
        LBracket => "`[`".to_string(),
        RBracket => "`]`".to_string(),
        LBrace => "`{`".to_string(),
        RBrace => "`}`".to_string(),
        Pipe => "`|`".to_string(),
        Comma => "`,`".to_string(),
        Dot => "`.`".to_string(),
        Colon => "`:`".to_string(),
        Semicolon => "`;`".to_string(),
        Arrow => "`->`".to_string(),
        At => "`@`".to_string(),
        Newline => "a newline".to_string(),
        Indent => "an indent".to_string(),
        Dedent => "a dedent".to_string(),
        Eof => "end of file".to_string(),
    }
}

#[cfg(test)]
mod tests;
