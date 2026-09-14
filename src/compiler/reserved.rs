//! The type-keyword check.
//!
//! `str`, `int`, `dict` and the rest are **type keywords**: each denotes a
//! [`Value::Type`](crate::value::Value::Type), which is exactly what `type(x)`
//! answers with, so `type(x) == str` is true for the same reason
//! `type(p) == Point` is. There is one spelling of a type test.
//!
//! That only holds if a type keyword is not also a variable. Before this, `str`
//! was an ordinary shadowable global and `dict = {}` quietly worked — after
//! which `type(x) == dict` compared a value against a dict, and read exactly
//! like the test it was no longer performing. So this pass walks the module
//! once before the symbol pre-pass and rejects every binding of a type name,
//! naming the reason.
//!
//! **Only bindings.** A type keyword is reserved in the *variable* namespace
//! and nowhere else: `w.bytes()` is a method call, `def bytes(self)` inside a
//! class declares a member, and `{"list": 1}` is a string key. None of those
//! is a variable, and all three appear in the tree (`std/http.oro` alone has
//! sixteen `.bytes()` calls), so reserving the attribute namespace too would
//! break working code to no purpose — a member is reached through an object
//! and can never be mistaken for the type.
//!
//! Compile-time rather than parse-time, and deliberately: a parse error cannot
//! have a `corpus/` file, because `tests/fmt_test.rs` formats every `.oro` in
//! the tree and a program that does not parse breaks it. A compile error can,
//! so the diagnostics below are oracled like any other behaviour.

use crate::ast::{Expr, Param, Pattern, Stmt};
use crate::value::keyword_type;

use super::CompileError;

/// Reject every binding of a type keyword in `program`.
pub fn check(program: &[Stmt]) -> Result<(), CompileError> {
    check_body(program)
}

/// What the program tried to make the name, for the diagnostic.
#[derive(Clone, Copy)]
enum As {
    Variable,
    LoopVariable,
    Function,
    Class,
    Parameter,
    Import,
    Global,
    Caught,
}

impl As {
    fn phrase(self) -> &'static str {
        match self {
            As::Variable => "a variable",
            As::LoopVariable => "a loop variable",
            As::Function => "a function name",
            As::Class => "a class name",
            As::Parameter => "a parameter name",
            As::Import => "an imported name",
            As::Global => "a `global` declaration",
            As::Caught => "an `except ... as` name",
        }
    }
}

fn reject(name: &str, what: As, line: usize, col: usize) -> Result<(), CompileError> {
    match keyword_type(name) {
        None => Ok(()),
        Some(_) => Err(CompileError {
            message: format!(
                "`{name}` is a type name and cannot be used as {} — type names are keywords \
                 in Oro, so that `type(x) == {name}` always means the type. Pick another name.",
                what.phrase()
            ),
            line,
            col,
        }),
    }
}

fn check_body(stmts: &[Stmt]) -> Result<(), CompileError> {
    for s in stmts {
        check_stmt(s)?;
    }
    Ok(())
}

fn check_stmt(stmt: &Stmt) -> Result<(), CompileError> {
    match stmt {
        Stmt::Expr { value, .. } => check_expr(value)?,
        Stmt::Assign { targets, value, .. } => {
            for t in targets {
                check_target(t, As::Variable)?;
            }
            check_expr(value)?;
        }
        Stmt::AugAssign { target, value, .. } => {
            check_target(target, As::Variable)?;
            check_expr(value)?;
        }
        Stmt::If {
            cond,
            body,
            elifs,
            orelse,
            ..
        } => {
            check_expr(cond)?;
            check_body(body)?;
            for (c, b) in elifs {
                check_expr(c)?;
                check_body(b)?;
            }
            if let Some(b) = orelse {
                check_body(b)?;
            }
        }
        Stmt::While { cond, body, .. } => {
            check_expr(cond)?;
            check_body(body)?;
        }
        Stmt::For {
            target, iter, body, ..
        } => {
            check_target(target, As::LoopVariable)?;
            check_expr(iter)?;
            check_body(body)?;
        }
        Stmt::Def {
            name,
            params,
            body,
            line,
            col,
        } => {
            reject(name, As::Function, *line, *col)?;
            check_params(params)?;
            check_body(body)?;
        }
        Stmt::Class {
            name,
            base,
            body,
            line,
            col,
        } => {
            reject(name, As::Class, *line, *col)?;
            if let Some(b) = base {
                check_expr(b)?;
            }
            // A method's name is a member, not a variable: `def bytes(self)` is
            // reached as `x.bytes()` and never shadows `bytes`. So the class
            // body's own `def`s are skipped, and only their insides checked.
            for s in body {
                match s {
                    Stmt::Def { params, body, .. } => {
                        check_params(params)?;
                        check_body(body)?;
                    }
                    other => check_stmt(other)?,
                }
            }
        }
        Stmt::Return { value, .. } | Stmt::Yield { value, .. } => {
            if let Some(v) = value {
                check_expr(v)?;
            }
        }
        Stmt::Try {
            body,
            handlers,
            finalbody,
            ..
        } => {
            check_body(body)?;
            for h in handlers {
                check_expr(&h.exc_type)?;
                if let Some(n) = &h.name {
                    reject(n, As::Caught, h.line, h.col)?;
                }
                check_body(&h.body)?;
            }
            if let Some(b) = finalbody {
                check_body(b)?;
            }
        }
        Stmt::Raise { exc, .. } => {
            if let Some(e) = exc {
                check_expr(e)?;
            }
        }
        Stmt::Import {
            path,
            alias,
            line,
            col,
        } => {
            if let Some(bound) = super::symbols::import_bound_name(path, alias) {
                reject(bound, As::Import, *line, *col)?;
            }
        }
        Stmt::Global { names, line, col } => {
            for n in names {
                reject(n, As::Global, *line, *col)?;
            }
        }
        Stmt::Match { subject, cases, .. } => {
            check_expr(subject)?;
            for c in cases {
                // A pattern is a literal or a dotted name; neither binds.
                match &c.pattern {
                    Pattern::Literal(e) | Pattern::Dotted(e) => check_expr(e)?,
                    Pattern::Wildcard => {}
                }
                check_body(&c.body)?;
            }
        }
        Stmt::Break { .. } | Stmt::Continue { .. } | Stmt::Pass { .. } => {}
    }
    Ok(())
}

fn check_params(params: &[Param]) -> Result<(), CompileError> {
    for p in params {
        reject(&p.name, As::Parameter, p.line, p.col)?;
        if let Some(d) = &p.default {
            check_expr(d)?;
        }
    }
    Ok(())
}

/// An assignment target. Only `Name` and the sequence forms bind; a subscript
/// or an attribute target stores into an object that already exists.
fn check_target(target: &Expr, what: As) -> Result<(), CompileError> {
    match target {
        Expr::Name { name, line, col } => reject(name, what, *line, *col),
        Expr::Tuple { elements, .. } | Expr::List { elements, .. } => {
            for e in elements {
                check_target(e, what)?;
            }
            Ok(())
        }
        other => check_expr(other),
    }
}

/// Expressions bind exactly one way — a lambda's parameters — so the walk
/// exists to find the lambdas. A *read* of a type name is fine anywhere and is
/// not looked at here; codegen turns it into the type.
fn check_expr(expr: &Expr) -> Result<(), CompileError> {
    match expr {
        Expr::Lambda { data, .. } => {
            check_params(&data.params)?;
            check_expr(&data.body)?;
        }
        Expr::Unary { operand, .. } => check_expr(operand)?,
        Expr::Binary { left, right, .. } | Expr::BoolOp { left, right, .. } => {
            check_expr(left)?;
            check_expr(right)?;
        }
        Expr::Compare { first, rest, .. } => {
            check_expr(first)?;
            for (_, e) in rest {
                check_expr(e)?;
            }
        }
        Expr::Ternary {
            cond, then, orelse, ..
        } => {
            check_expr(cond)?;
            check_expr(then)?;
            check_expr(orelse)?;
        }
        Expr::Call {
            func, args, kwargs, ..
        } => {
            check_expr(func)?;
            for a in args {
                check_expr(a)?;
            }
            // A keyword *argument* name is a parameter of the callee, not a
            // binding here — and the callee's own parameters were checked where
            // it was defined.
            for (_, e) in kwargs {
                check_expr(e)?;
            }
        }
        Expr::Attribute { value, .. } => check_expr(value)?,
        Expr::Subscript { value, index, .. } => {
            check_expr(value)?;
            check_expr(index)?;
        }
        Expr::Slice {
            value,
            lower,
            upper,
            step,
            ..
        } => {
            check_expr(value)?;
            for part in [lower, upper, step].into_iter().flatten() {
                check_expr(part)?;
            }
        }
        Expr::List { elements, .. } | Expr::Tuple { elements, .. } => {
            for e in elements {
                check_expr(e)?;
            }
        }
        Expr::Dict { entries, .. } => {
            for (k, v) in entries {
                check_expr(k)?;
                check_expr(v)?;
            }
        }
        Expr::Int { .. }
        | Expr::Float { .. }
        | Expr::Str { .. }
        | Expr::Bytes { .. }
        | Expr::FString { .. }
        | Expr::Bool { .. }
        | Expr::NoneLit { .. }
        | Expr::Name { .. } => {}
    }
    Ok(())
}
