//! `oro lint` — the mechanically-checkable half of the one-way audit.
//!
//! The audit found ~15 places where one job has a preferred spelling governed
//! only by prose. Some of those are *judgement calls* — chain versus `for`,
//! `first()` versus `[0]`, how to build a string — and a linter that flagged
//! them would be wrong as often as right, so this does not touch them (see
//! [`UNCHECKABLE`]). The rest are pure syntax: a shape that has a shorter,
//! clearer spelling with no loss, no matter the context. Those are here.
//!
//! Each rule is a pattern over the parsed AST, so the linter never runs the
//! program and never needs its types. A finding names the line, the rule, and
//! the spelling to use instead — advice, not an error: `oro lint` exits non-zero
//! when it finds something, but the program still compiles and runs.

use crate::ast::{CmpOp, Expr, LambdaData, Stmt, UnaryOp};

/// One lint hit: where it is, which rule, and what to write instead.
pub struct Finding {
    pub line: usize,
    pub col: usize,
    pub rule: &'static str,
    pub message: String,
}

/// The audit overlaps a linter cannot decide, and why — reported by
/// `oro lint --rules` so the boundary is visible rather than implied.
pub const UNCHECKABLE: &[(&str, &str)] = &[
    ("chain vs. for", "laziness and statements decide it; a one-line append is a chain, a multi-statement body is a loop — the body, not the syntax, is the signal"),
    ("first() vs. [0]", "`[0]` demands an element and `first()` asks for one (null/IndexError differ); which is right depends on whether emptiness is expected"),
    ("+ vs. f-string for building a string", "two pieces you already have vs. formatting or three-plus pieces — a taste line no pattern settles"),
    ("match vs. if/elif vs. a dict", "constant-to-value is a dict, constant-to-statements is match, predicates are if/elif — but telling a constant ladder from a predicate ladder needs intent"),
    ("sentinel vs. exception as a failure signal", "a lookup that can find nothing answers with a value; an operation that could not do its job raises — the choice is the API's meaning, not its shape"),
    ("is_digit() as a numeric-parse test", "on str it is Unicode-numeric, not parse-able-as-int; whether that matters is the caller's intent"),
    ("manual counter -> for i, v in xs", "already a *compile error* for a constant-step `while` (the loop rule), so there is nothing left for a lint to catch that the compiler does not"),
];

/// Lint a parsed module, returning every finding in source order.
pub fn lint(program: &[Stmt]) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut visit = |e: &Expr| check_expr(e, &mut out);
    walk_stmts(program, &mut visit);
    out.sort_by_key(|f| (f.line, f.col));
    out
}

// --- The rules ---------------------------------------------------------------

fn check_expr(e: &Expr, out: &mut Vec<Finding>) {
    if let Expr::Compare { first, rest, line, col } = e {
        if rest.len() == 1 {
            let (op, rhs) = &rest[0];
            check_find_membership(first, *op, rhs, *line, *col, out);
            check_get_null(first, *op, rhs, *line, *col, out);
            check_slice_affix(first, *op, rhs, *line, *col, out);
        }
    }
    if let Expr::Slice { value, step, lower, upper, line, col } = e {
        // `xs[::-1]` — a reversed copy spelled as punctuation.
        if lower.is_none() && upper.is_none() && step.as_deref().is_some_and(is_neg_one) {
            out.push(Finding {
                line: *line,
                col: *col,
                rule: "slice-reverse",
                message: "`xs[::-1]` reverses by slice punctuation — use `xs.reverse()`, which \
                          says what it does and preserves the receiver's type"
                    .to_string(),
            });
        }
        // `s[len(p):]` strips a prefix by arithmetic; `s[:-len(p)]` a suffix.
        if step.is_none() {
            if upper.is_none() && lower.as_deref().is_some_and(is_len_call) {
                out.push(Finding {
                    line: *line,
                    col: *col,
                    rule: "slice-rm-prefix",
                    message: format!(
                        "`{}[len(p):]` strips a prefix by arithmetic — `{}.rm_prefix(p)` says so, \
                         and leaves the string untouched when the prefix is absent",
                        describe(value),
                        describe(value),
                    ),
                });
            }
            if lower.is_none() && upper.as_deref().is_some_and(is_neg_len_call) {
                out.push(Finding {
                    line: *line,
                    col: *col,
                    rule: "slice-rm-suffix",
                    message: format!(
                        "`{}[:-len(s)]` strips a suffix by arithmetic — use `{}.rm_suffix(s)`",
                        describe(value),
                        describe(value),
                    ),
                });
            }
        }
    }
}

/// `s.find(x) >= 0` (and its siblings) is a membership test written as an index
/// probe. `find` answers "where"; `in` answers "whether", and a `find` whose
/// result is only compared against the not-found sentinel is `in` longhand.
fn check_find_membership(
    first: &Expr,
    op: CmpOp,
    rhs: &Expr,
    line: usize,
    col: usize,
    out: &mut Vec<Finding>,
) {
    let Some((recv, arg)) = find_call(first) else { return };
    // Which sentinel comparison, and therefore whether it means `in` or `not in`.
    let membership = match (op, int_value(rhs)) {
        (CmpOp::GtEq, Some(0)) | (CmpOp::Gt, Some(-1)) | (CmpOp::NotEq, Some(-1)) => Some("in"),
        (CmpOp::Lt, Some(0)) | (CmpOp::Eq, Some(-1)) => Some("not in"),
        _ => None,
    };
    let Some(kw) = membership else { return };
    out.push(Finding {
        line,
        col,
        rule: "find-as-membership",
        message: format!(
            "`{}.find({})` compared only against the not-found sentinel is a membership test — \
             use `{} {} {}`",
            describe(recv),
            describe(arg),
            describe(arg),
            kw,
            describe(recv),
        ),
    });
}

/// `d.get(k) == null` cannot tell a missing key from a key holding `null`, and
/// when the answer is only compared to `null` the question was membership.
/// `d.get(k, default=…)` (a real fallback) is untouched — it has arguments.
fn check_get_null(
    first: &Expr,
    op: CmpOp,
    rhs: &Expr,
    line: usize,
    col: usize,
    out: &mut Vec<Finding>,
) {
    if !matches!(op, CmpOp::Eq | CmpOp::NotEq) || !matches!(rhs, Expr::NoneLit { .. }) {
        return;
    }
    // A bare `x.get(k)`: exactly one positional arg, no `default=`.
    let Expr::Call { func, args, kwargs, .. } = first else { return };
    if args.len() != 1 || !kwargs.is_empty() {
        return;
    }
    let Expr::Attribute { value, attr, .. } = &**func else { return };
    if attr != "get" {
        return;
    }
    let kw = if op == CmpOp::Eq { "not in" } else { "in" };
    out.push(Finding {
        line,
        col,
        rule: "get-null-as-membership",
        message: format!(
            "`{}.get({}) {} null` is a membership test that also mishandles a stored `null` — \
             use `{} {} {}`",
            describe(value),
            describe(&args[0]),
            if op == CmpOp::Eq { "==" } else { "!=" },
            describe(&args[0]),
            kw,
            describe(value),
        ),
    });
}

/// `s[0:n] == p` / `s[:n] == p` is `startswith`, and `s[-n:] == p` is
/// `endswith`, each written as a slice-and-compare.
fn check_slice_affix(
    first: &Expr,
    op: CmpOp,
    _rhs: &Expr,
    line: usize,
    col: usize,
    out: &mut Vec<Finding>,
) {
    if op != CmpOp::Eq {
        return;
    }
    let Expr::Slice { value, lower, upper, step, .. } = first else { return };
    if step.is_some() {
        return;
    }
    // `s[:n]` or `s[0:n]` compared to a prefix.
    let leading_zero = lower.is_none() || lower.as_deref().is_some_and(|e| int_value(e) == Some(0));
    if leading_zero && upper.is_some() {
        out.push(Finding {
            line,
            col,
            rule: "slice-eq-startswith",
            message: format!(
                "`{}[:n] == prefix` is a prefix test — use `{}.startswith(prefix)`",
                describe(value),
                describe(value),
            ),
        });
        return;
    }
    // `s[-n:]` compared to a suffix: a negative lower bound, no upper.
    if upper.is_none() && lower.as_deref().is_some_and(is_negative) {
        out.push(Finding {
            line,
            col,
            rule: "slice-eq-endswith",
            message: format!(
                "`{}[-n:] == suffix` is a suffix test — use `{}.endswith(suffix)`",
                describe(value),
                describe(value),
            ),
        });
    }
}

// --- Small AST predicates ----------------------------------------------------

/// A `something.find(arg)` call with exactly one positional argument, returning
/// `(receiver, arg)`.
fn find_call(e: &Expr) -> Option<(&Expr, &Expr)> {
    let Expr::Call { func, args, kwargs, .. } = e else { return None };
    if args.len() != 1 || !kwargs.is_empty() {
        return None;
    }
    let Expr::Attribute { value, attr, .. } = &**func else { return None };
    (attr == "find").then_some((&**value, &args[0]))
}

/// The integer a literal denotes, with an optional leading `-`.
fn int_value(e: &Expr) -> Option<i64> {
    match e {
        Expr::Int { value, .. } => value.replace('_', "").parse().ok(),
        Expr::Unary { op: UnaryOp::Neg, operand, .. } => int_value(operand).map(|n| -n),
        _ => None,
    }
}

fn is_neg_one(e: &Expr) -> bool {
    int_value(e) == Some(-1)
}

fn is_negative(e: &Expr) -> bool {
    matches!(int_value(e), Some(n) if n < 0)
}

/// A `len(...)` call with one positional argument — the length arithmetic that
/// an affix-strip slice is built from.
fn is_len_call(e: &Expr) -> bool {
    matches!(e, Expr::Call { func, args, kwargs, .. }
        if args.len() == 1 && kwargs.is_empty()
            && matches!(&**func, Expr::Name { name, .. } if name == "len"))
}

/// `-len(...)`: the negated length an `s[:-len(p)]` suffix strip uses.
fn is_neg_len_call(e: &Expr) -> bool {
    matches!(e, Expr::Unary { op: UnaryOp::Neg, operand, .. } if is_len_call(operand))
}

/// A short human rendering of a leaf-ish expression for a lint message. Anything
/// that is not a simple name/literal/attribute is elided to `…` — the message is
/// advice, not a rewrite.
fn describe(e: &Expr) -> String {
    match e {
        Expr::Name { name, .. } => name.clone(),
        Expr::Str { value, .. } => format!("{value:?}"),
        Expr::Int { value, .. } => value.clone(),
        Expr::NoneLit { .. } => "null".to_string(),
        Expr::Attribute { value, attr, .. } => format!("{}.{}", describe(value), attr),
        _ => "…".to_string(),
    }
}

// --- Traversal ---------------------------------------------------------------

fn walk_stmts(stmts: &[Stmt], f: &mut impl FnMut(&Expr)) {
    for s in stmts {
        walk_stmt(s, f);
    }
}

fn walk_stmt(s: &Stmt, f: &mut impl FnMut(&Expr)) {
    match s {
        Stmt::Expr { value, .. } | Stmt::AugAssign { value, .. } => walk_expr(value, f),
        Stmt::Assign { targets, value, .. } => {
            for t in targets {
                walk_expr(t, f);
            }
            walk_expr(value, f);
        }
        Stmt::If { cond, body, elifs, orelse, .. } => {
            walk_expr(cond, f);
            walk_stmts(body, f);
            for (c, b) in elifs {
                walk_expr(c, f);
                walk_stmts(b, f);
            }
            if let Some(b) = orelse {
                walk_stmts(b, f);
            }
        }
        Stmt::While { cond, body, .. } => {
            walk_expr(cond, f);
            walk_stmts(body, f);
        }
        Stmt::For { target, iter, body, .. } => {
            walk_expr(target, f);
            walk_expr(iter, f);
            walk_stmts(body, f);
        }
        Stmt::Def { body, .. } | Stmt::Class { body, .. } => walk_stmts(body, f),
        Stmt::Return { value: Some(v), .. }
        | Stmt::Yield { value: Some(v), .. }
        | Stmt::Raise { exc: Some(v), .. } => walk_expr(v, f),
        Stmt::Try { body, handlers, finalbody, .. } => {
            walk_stmts(body, f);
            for h in handlers {
                walk_stmts(&h.body, f);
            }
            if let Some(b) = finalbody {
                walk_stmts(b, f);
            }
        }
        Stmt::Match { subject, cases, .. } => {
            walk_expr(subject, f);
            for c in cases {
                walk_stmts(&c.body, f);
            }
        }
        _ => {}
    }
}

fn walk_expr(e: &Expr, f: &mut impl FnMut(&Expr)) {
    f(e);
    match e {
        Expr::Unary { operand, .. } => walk_expr(operand, f),
        Expr::Binary { left, right, .. } | Expr::BoolOp { left, right, .. } => {
            walk_expr(left, f);
            walk_expr(right, f);
        }
        Expr::Compare { first, rest, .. } => {
            walk_expr(first, f);
            for (_, r) in rest {
                walk_expr(r, f);
            }
        }
        Expr::Call { func, args, kwargs, .. } => {
            walk_expr(func, f);
            for a in args {
                walk_expr(a, f);
            }
            for (_, v) in kwargs {
                walk_expr(v, f);
            }
        }
        Expr::Attribute { value, .. } => walk_expr(value, f),
        Expr::Subscript { value, index, .. } => {
            walk_expr(value, f);
            walk_expr(index, f);
        }
        Expr::Slice { value, lower, upper, step, .. } => {
            walk_expr(value, f);
            for p in [lower, upper, step].into_iter().flatten() {
                walk_expr(p, f);
            }
        }
        Expr::List { elements, .. } | Expr::Tuple { elements, .. } => {
            for el in elements {
                walk_expr(el, f);
            }
        }
        Expr::Dict { entries, .. } => {
            for (k, v) in entries {
                walk_expr(k, f);
                walk_expr(v, f);
            }
        }
        Expr::Lambda { data, .. } => {
            let LambdaData { body, .. } = &**data;
            walk_expr(body, f);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn rules(src: &str) -> Vec<&'static str> {
        let toks = Lexer::new(src).tokenize().expect("lex");
        let prog = Parser::new(toks).parse().expect("parse");
        lint(&prog).into_iter().map(|f| f.rule).collect()
    }

    #[test]
    fn flags_each_mechanical_overlap() {
        assert_eq!(rules("r = s.find(x) >= 0\n"), ["find-as-membership"]);
        assert_eq!(rules("r = s.find(x) == -1\n"), ["find-as-membership"]);
        assert_eq!(rules("r = s.find(x) != -1\n"), ["find-as-membership"]);
        assert_eq!(rules("r = d.get(k) == null\n"), ["get-null-as-membership"]);
        assert_eq!(rules("r = d.get(k) != null\n"), ["get-null-as-membership"]);
        assert_eq!(rules("r = s[0:2] == p\n"), ["slice-eq-startswith"]);
        assert_eq!(rules("r = s[:2] == p\n"), ["slice-eq-startswith"]);
        assert_eq!(rules("r = s[-2:] == p\n"), ["slice-eq-endswith"]);
        assert_eq!(rules("t = s[len(p):]\n"), ["slice-rm-prefix"]);
        assert_eq!(rules("t = s[:-len(p)]\n"), ["slice-rm-suffix"]);
        assert_eq!(rules("t = s[::-1]\n"), ["slice-reverse"]);
    }

    #[test]
    fn leaves_the_legitimate_forms_alone() {
        // A real fallback (has a default) is not a membership test.
        assert!(rules("r = d.get(k, default=0) == null\n").is_empty());
        // A `find` whose result is actually used as an index, not a sentinel probe.
        assert!(rules("i = s.find(x)\nr = s[i:]\n").is_empty());
        // A plain comparison, an ordinary slice, an ordinary get.
        assert!(rules("r = a == b\nt = s[1:2]\nv = d.get(k)\n").is_empty());
        // `find(x) > 5` is a genuine position test, not a membership one.
        assert!(rules("r = s.find(x) > 5\n").is_empty());
        // The reverse slice needs step -1 specifically; a plain copy is fine.
        assert!(rules("t = s[::2]\n").is_empty());
    }
}
