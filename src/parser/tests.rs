//! Parser tests.
//!
//! These cover expression precedence and associativity, every statement form,
//! line/col propagation, and — importantly — that each deliberately cut feature
//! yields its own specific diagnostic rather than a generic syntax error.

use super::{ParseError, Parser};
use crate::ast::*;
use crate::lexer::Lexer;

// --- Helpers ---------------------------------------------------------------

/// Lex and parse, expecting success.
fn parse(src: &str) -> Vec<Stmt> {
    let tokens = Lexer::new(src).tokenize().expect("source should lex");
    Parser::new(tokens).parse().expect("source should parse")
}

/// Parse a single-statement program and return that statement.
fn parse_one(src: &str) -> Stmt {
    let mut prog = parse(src);
    assert_eq!(prog.len(), 1, "expected exactly one statement");
    prog.pop().unwrap()
}

/// Parse a single bare-expression statement and return the expression.
fn parse_expr(src: &str) -> Expr {
    match parse_one(src) {
        Stmt::Expr { value, .. } => value,
        other => panic!("expected an expression statement, got {other:?}"),
    }
}

/// Lex and parse, expecting a parse error.
fn parse_err(src: &str) -> ParseError {
    let tokens = Lexer::new(src).tokenize().expect("source should lex");
    Parser::new(tokens)
        .parse()
        .expect_err("source should fail to parse")
}

/// A compact s-expression rendering of an expression, for terse precedence
/// assertions.
fn sexp(e: &Expr) -> String {
    match e {
        Expr::Int { value, .. } => value.clone(),
        Expr::Float { value, .. } => value.clone(),
        Expr::Str { value, .. } => format!("\"{value}\""),
        Expr::FString { value, .. } => format!("f\"{value}\""),
        Expr::Bool { value, .. } => value.to_string(),
        Expr::NoneLit { .. } => "None".to_string(),
        Expr::Name { name, .. } => name.clone(),
        Expr::Unary { op, operand, .. } => {
            let o = match op {
                UnaryOp::Neg => "-",
                UnaryOp::Pos => "+",
                UnaryOp::Not => "not",
            };
            format!("({o} {})", sexp(operand))
        }
        Expr::Binary { op, left, right, .. } => {
            let o = match op {
                BinOp::Add => "+",
                BinOp::Sub => "-",
                BinOp::Mul => "*",
                BinOp::Div => "/",
                BinOp::FloorDiv => "//",
                BinOp::Mod => "%",
                BinOp::Pow => "**",
            };
            format!("({o} {} {})", sexp(left), sexp(right))
        }
        Expr::BoolOp { op, left, right, .. } => {
            let o = match op {
                BoolOp::And => "and",
                BoolOp::Or => "or",
            };
            format!("({o} {} {})", sexp(left), sexp(right))
        }
        Expr::Compare { first, rest, .. } => {
            let mut s = format!("(cmp {}", sexp(first));
            for (op, r) in rest {
                let o = match op {
                    CmpOp::Eq => "==",
                    CmpOp::NotEq => "!=",
                    CmpOp::Lt => "<",
                    CmpOp::Gt => ">",
                    CmpOp::LtEq => "<=",
                    CmpOp::GtEq => ">=",
                    CmpOp::Is => "is",
                    CmpOp::IsNot => "is-not",
                    CmpOp::In => "in",
                    CmpOp::NotIn => "not-in",
                };
                s.push_str(&format!(" {o} {}", sexp(r)));
            }
            s.push(')');
            s
        }
        Expr::Call { func, args, kwargs, .. } => {
            let mut parts: Vec<String> = args.iter().map(sexp).collect();
            for (k, v) in kwargs {
                parts.push(format!("{k}={}", sexp(v)));
            }
            format!("(call {} [{}])", sexp(func), parts.join(" "))
        }
        Expr::Attribute { value, attr, .. } => format!("(. {} {attr})", sexp(value)),
        Expr::Subscript { value, index, .. } => {
            format!("([] {} {})", sexp(value), sexp(index))
        }
        Expr::Slice { value, lower, upper, step, .. } => {
            let part = |o: &Option<Box<Expr>>| o.as_ref().map(|e| sexp(e)).unwrap_or_default();
            format!(
                "(slice {} {}:{}:{})",
                sexp(value),
                part(lower),
                part(upper),
                part(step)
            )
        }
        Expr::List { elements, .. } => {
            let parts: Vec<String> = elements.iter().map(sexp).collect();
            format!("(list {})", parts.join(" "))
        }
        Expr::Tuple { elements, .. } => {
            let parts: Vec<String> = elements.iter().map(sexp).collect();
            format!("(tuple {})", parts.join(" "))
        }
        Expr::Set { elements, .. } => {
            let parts: Vec<String> = elements.iter().map(sexp).collect();
            format!("(set {})", parts.join(" "))
        }
        Expr::Dict { entries, .. } => {
            let parts: Vec<String> =
                entries.iter().map(|(k, v)| format!("{}:{}", sexp(k), sexp(v))).collect();
            format!("(dict {})", parts.join(" "))
        }
    }
}

fn sexp_of(src: &str) -> String {
    sexp(&parse_expr(src))
}

// --- Precedence & associativity --------------------------------------------

#[test]
fn precedence_mul_over_add() {
    assert_eq!(sexp_of("2 + 3 * 4"), "(+ 2 (* 3 4))");
}

#[test]
fn precedence_parens_override() {
    assert_eq!(sexp_of("(2 + 3) * 4"), "(* (+ 2 3) 4)");
}

#[test]
fn power_is_right_associative() {
    assert_eq!(sexp_of("2 ** 3 ** 4"), "(** 2 (** 3 4))");
}

#[test]
fn unary_minus_binds_looser_than_power() {
    // Python: -2 ** 2 == -(2 ** 2)
    assert_eq!(sexp_of("-2 ** 2"), "(- (** 2 2))");
}

#[test]
fn power_right_operand_may_be_unary() {
    assert_eq!(sexp_of("2 ** -3"), "(** 2 (- 3))");
}

#[test]
fn unary_binds_tighter_than_mul() {
    assert_eq!(sexp_of("-a * b"), "(* (- a) b)");
}

#[test]
fn add_is_left_associative() {
    assert_eq!(sexp_of("1 - 2 - 3"), "(- (- 1 2) 3)");
}

#[test]
fn floor_div_and_mod_group_with_mul() {
    assert_eq!(sexp_of("a // b % c * d"), "(* (% (// a b) c) d)");
}

#[test]
fn double_unary() {
    assert_eq!(sexp_of("- -x"), "(- (- x))");
    assert_eq!(sexp_of("not not x"), "(not (not x))");
}

// --- Boolean / comparison precedence ---------------------------------------

#[test]
fn and_binds_tighter_than_or() {
    assert_eq!(sexp_of("a or b and c"), "(or a (and b c))");
}

#[test]
fn not_binds_looser_than_comparison() {
    // `not a == b` is `not (a == b)`.
    assert_eq!(sexp_of("not a == b"), "(not (cmp a == b))");
}

#[test]
fn not_binds_tighter_than_and() {
    assert_eq!(sexp_of("not a and b"), "(and (not a) b)");
}

#[test]
fn comparison_chains() {
    assert_eq!(sexp_of("a < b < c"), "(cmp a < b < c)");
    assert_eq!(sexp_of("a < b == c >= d"), "(cmp a < b == c >= d)");
}

#[test]
fn comparison_binds_looser_than_arithmetic() {
    assert_eq!(sexp_of("a + b < c * d"), "(cmp (+ a b) < (* c d))");
}

#[test]
fn is_not_and_not_in() {
    assert_eq!(sexp_of("a is not b"), "(cmp a is-not b)");
    assert_eq!(sexp_of("a not in b"), "(cmp a not-in b)");
    assert_eq!(sexp_of("a is b"), "(cmp a is b)");
    assert_eq!(sexp_of("a in b"), "(cmp a in b)");
}

#[test]
fn comparison_then_and() {
    assert_eq!(sexp_of("a < b and c"), "(and (cmp a < b) c)");
}

// --- Postfix: calls, attributes, subscripts, slices ------------------------

#[test]
fn call_no_args() {
    assert_eq!(sexp_of("f()"), "(call f [])");
}

#[test]
fn call_with_args_and_kwargs() {
    assert_eq!(sexp_of("f(1, 2, k=3)"), "(call f [1 2 k=3])");
}

#[test]
fn attribute_chain() {
    assert_eq!(sexp_of("a.b.c"), "(. (. a b) c)");
}

#[test]
fn call_and_attribute_and_subscript_mix() {
    assert_eq!(sexp_of("a.b().c[0]"), "([] (. (call (. a b) []) c) 0)");
}

#[test]
fn postfix_binds_tighter_than_power() {
    // 2 ** f() groups as 2 ** (f())
    assert_eq!(sexp_of("2 ** f()"), "(** 2 (call f []))");
}

#[test]
fn slices() {
    assert_eq!(sexp_of("a[1:2]"), "(slice a 1:2:)");
    assert_eq!(sexp_of("a[:]"), "(slice a ::)");
    assert_eq!(sexp_of("a[::2]"), "(slice a ::2)");
    assert_eq!(sexp_of("a[1:2:3]"), "(slice a 1:2:3)");
    assert_eq!(sexp_of("a[i]"), "([] a i)");
}

// --- Collection literals ----------------------------------------------------

#[test]
fn nested_collection_literals() {
    assert_eq!(sexp_of("[1, [2, 3], 4]"), "(list 1 (list 2 3) 4)");
    assert_eq!(sexp_of("{1: [2, 3]}"), "(dict 1:(list 2 3))");
    assert_eq!(sexp_of("{1, 2, 3}"), "(set 1 2 3)");
}

#[test]
fn empty_collections() {
    assert_eq!(sexp_of("[]"), "(list )");
    assert_eq!(sexp_of("{}"), "(dict )"); // {} is an empty dict
    assert_eq!(sexp_of("()"), "(tuple )");
}

#[test]
fn tuples_parenthesised_and_bare() {
    assert_eq!(sexp_of("(1, 2, 3)"), "(tuple 1 2 3)");
    // Bare tuple in an expression-statement position.
    assert_eq!(sexp_of("1, 2, 3"), "(tuple 1 2 3)");
    // Trailing comma.
    assert_eq!(sexp_of("(1,)"), "(tuple 1)");
}

#[test]
fn parenthesised_single_expr_is_not_a_tuple() {
    assert_eq!(sexp_of("(1 + 2)"), "(+ 1 2)");
}

#[test]
fn literals() {
    assert!(matches!(parse_expr("42"), Expr::Int { .. }));
    assert!(matches!(parse_expr("3.14"), Expr::Float { .. }));
    assert!(matches!(parse_expr("'hi'"), Expr::Str { .. }));
    assert!(matches!(parse_expr("f'x'"), Expr::FString { .. }));
    assert!(matches!(parse_expr("True"), Expr::Bool { value: true, .. }));
    assert!(matches!(parse_expr("False"), Expr::Bool { value: false, .. }));
    assert!(matches!(parse_expr("None"), Expr::NoneLit { .. }));
}

#[test]
fn numbers_kept_as_raw_text() {
    match parse_expr("007") {
        Expr::Int { value, .. } => assert_eq!(value, "007"),
        other => panic!("expected int, got {other:?}"),
    }
    match parse_expr("1e9") {
        Expr::Float { value, .. } => assert_eq!(value, "1e9"),
        other => panic!("expected float, got {other:?}"),
    }
}

// --- Assignments ------------------------------------------------------------

#[test]
fn simple_assignment() {
    match parse_one("x = 1 + 2") {
        Stmt::Assign { targets, value, .. } => {
            assert_eq!(targets.len(), 1);
            assert_eq!(sexp(&targets[0]), "x");
            assert_eq!(sexp(&value), "(+ 1 2)");
        }
        other => panic!("expected assign, got {other:?}"),
    }
}

#[test]
fn chained_assignment() {
    match parse_one("a = b = 3") {
        Stmt::Assign { targets, value, .. } => {
            assert_eq!(targets.len(), 2);
            assert_eq!(sexp(&targets[0]), "a");
            assert_eq!(sexp(&targets[1]), "b");
            assert_eq!(sexp(&value), "3");
        }
        other => panic!("expected assign, got {other:?}"),
    }
}

#[test]
fn tuple_unpacking_assignment() {
    match parse_one("a, b = 1, 2") {
        Stmt::Assign { targets, value, .. } => {
            assert_eq!(sexp(&targets[0]), "(tuple a b)");
            assert_eq!(sexp(&value), "(tuple 1 2)");
        }
        other => panic!("expected assign, got {other:?}"),
    }
}

#[test]
fn augmented_assignments() {
    for (src, want) in [
        ("x += 1", AugOp::Add),
        ("x -= 1", AugOp::Sub),
        ("x *= 1", AugOp::Mul),
        ("x /= 1", AugOp::Div),
    ] {
        match parse_one(src) {
            Stmt::AugAssign { op, .. } => assert_eq!(op, want),
            other => panic!("expected augassign, got {other:?}"),
        }
    }
}

#[test]
fn assign_target_subscript_and_attribute() {
    assert!(matches!(parse_one("a[0] = 1"), Stmt::Assign { .. }));
    assert!(matches!(parse_one("a.b = 1"), Stmt::Assign { .. }));
}

// --- Control flow -----------------------------------------------------------

#[test]
fn if_elif_else() {
    let src = "\
if a:
    x = 1
elif b:
    x = 2
elif c:
    x = 3
else:
    x = 4
";
    match parse_one(src) {
        Stmt::If { cond, body, elifs, orelse, .. } => {
            assert_eq!(sexp(&cond), "a");
            assert_eq!(body.len(), 1);
            assert_eq!(elifs.len(), 2);
            assert_eq!(sexp(&elifs[0].0), "b");
            assert_eq!(sexp(&elifs[1].0), "c");
            assert!(orelse.is_some());
            assert_eq!(orelse.unwrap().len(), 1);
        }
        other => panic!("expected if, got {other:?}"),
    }
}

#[test]
fn nested_if_inside_while() {
    let src = "\
while a:
    if b:
        c = 1
    d = 2
";
    match parse_one(src) {
        Stmt::While { body, .. } => {
            assert_eq!(body.len(), 2);
            assert!(matches!(body[0], Stmt::If { .. }));
        }
        other => panic!("expected while, got {other:?}"),
    }
}

#[test]
fn for_loop_with_tuple_target() {
    let src = "\
for k, v in items:
    print(k)
";
    match parse_one(src) {
        Stmt::For { target, iter, body, .. } => {
            assert_eq!(sexp(&target), "(tuple k v)");
            assert_eq!(sexp(&iter), "items");
            assert_eq!(body.len(), 1);
        }
        other => panic!("expected for, got {other:?}"),
    }
}

#[test]
fn inline_suite() {
    match parse_one("if a: x = 1") {
        Stmt::If { body, orelse, .. } => {
            assert_eq!(body.len(), 1);
            assert!(orelse.is_none());
        }
        other => panic!("expected if, got {other:?}"),
    }
}

#[test]
fn semicolon_separated_simple_statements() {
    let prog = parse("x = 1; y = 2; z = 3");
    assert_eq!(prog.len(), 3);
    assert!(prog.iter().all(|s| matches!(s, Stmt::Assign { .. })));
}

// --- Functions & classes ----------------------------------------------------

#[test]
fn def_with_params_and_defaults() {
    let src = "\
def f(a, b, c=1, d=2):
    return a
";
    match parse_one(src) {
        Stmt::Def { name, params, ret, body, .. } => {
            assert_eq!(name, "f");
            assert_eq!(params.len(), 4);
            assert!(params[0].default.is_none());
            assert!(params[2].default.is_some());
            assert!(ret.is_none());
            assert_eq!(body.len(), 1);
        }
        other => panic!("expected def, got {other:?}"),
    }
}

#[test]
fn def_with_annotations_and_return_type() {
    let src = "\
def f(a: int, b: str = 'x') -> bool:
    return True
";
    match parse_one(src) {
        Stmt::Def { params, ret, .. } => {
            assert!(params[0].annotation.is_some());
            assert!(params[1].annotation.is_some());
            assert!(params[1].default.is_some());
            assert_eq!(sexp(ret.as_ref().unwrap()), "bool");
        }
        other => panic!("expected def, got {other:?}"),
    }
}

#[test]
fn class_without_base() {
    let src = "\
class C:
    pass
";
    match parse_one(src) {
        Stmt::Class { name, base, body, .. } => {
            assert_eq!(name, "C");
            assert!(base.is_none());
            assert!(matches!(body[0], Stmt::Pass { .. }));
        }
        other => panic!("expected class, got {other:?}"),
    }
}

#[test]
fn class_with_base() {
    let src = "\
class C(Base):
    x = 1
";
    match parse_one(src) {
        Stmt::Class { base, .. } => {
            assert_eq!(sexp(base.as_ref().unwrap()), "Base");
        }
        other => panic!("expected class, got {other:?}"),
    }
}

#[test]
fn class_empty_parens_has_no_base() {
    let src = "\
class C():
    pass
";
    match parse_one(src) {
        Stmt::Class { base, .. } => assert!(base.is_none()),
        other => panic!("expected class, got {other:?}"),
    }
}

// --- try / except / finally / raise ----------------------------------------

#[test]
fn try_except_finally() {
    let src = "\
try:
    risky()
except ValueError as e:
    handle(e)
except KeyError:
    other()
finally:
    cleanup()
";
    match parse_one(src) {
        Stmt::Try { body, handlers, finalbody, .. } => {
            assert_eq!(body.len(), 1);
            assert_eq!(handlers.len(), 2);
            assert_eq!(sexp(&handlers[0].exc_type), "ValueError");
            assert_eq!(handlers[0].name.as_deref(), Some("e"));
            assert_eq!(sexp(&handlers[1].exc_type), "KeyError");
            assert!(handlers[1].name.is_none());
            assert!(finalbody.is_some());
        }
        other => panic!("expected try, got {other:?}"),
    }
}

#[test]
fn try_finally_only() {
    let src = "\
try:
    x = 1
finally:
    y = 2
";
    match parse_one(src) {
        Stmt::Try { handlers, finalbody, .. } => {
            assert!(handlers.is_empty());
            assert!(finalbody.is_some());
        }
        other => panic!("expected try, got {other:?}"),
    }
}

#[test]
fn raise_with_and_without_value() {
    assert!(matches!(parse_one("raise E('boom')"), Stmt::Raise { exc: Some(_), .. }));
    assert!(matches!(parse_one("raise"), Stmt::Raise { exc: None, .. }));
}

#[test]
fn try_requires_except_or_finally() {
    let e = parse_err("try:\n    x = 1\n");
    assert!(e.message.contains("except"), "got: {}", e.message);
}

// --- Imports ----------------------------------------------------------------

#[test]
fn import_dotted() {
    match parse_one("import a.b.c") {
        Stmt::Import { path, alias, .. } => {
            assert_eq!(path, vec!["a", "b", "c"]);
            assert!(alias.is_none());
        }
        other => panic!("expected import, got {other:?}"),
    }
}

#[test]
fn import_with_alias() {
    match parse_one("import a.b.c as name") {
        Stmt::Import { path, alias, .. } => {
            assert_eq!(path, vec!["a", "b", "c"]);
            assert_eq!(alias.as_deref(), Some("name"));
        }
        other => panic!("expected import, got {other:?}"),
    }
}

// --- Simple statements ------------------------------------------------------

#[test]
fn break_continue_pass_return() {
    assert!(matches!(parse_one("break"), Stmt::Break { .. }));
    assert!(matches!(parse_one("continue"), Stmt::Continue { .. }));
    assert!(matches!(parse_one("pass"), Stmt::Pass { .. }));
    assert!(matches!(parse_one("return"), Stmt::Return { value: None, .. }));
    assert!(matches!(parse_one("return 1"), Stmt::Return { value: Some(_), .. }));
}

#[test]
fn return_tuple() {
    match parse_one("return 1, 2") {
        Stmt::Return { value: Some(v), .. } => assert_eq!(sexp(&v), "(tuple 1 2)"),
        other => panic!("expected return, got {other:?}"),
    }
}

#[test]
fn yield_statement() {
    assert!(matches!(parse_one("yield"), Stmt::Yield { value: None, .. }));
    assert!(matches!(parse_one("yield x"), Stmt::Yield { value: Some(_), .. }));
}

// --- line / col propagation -------------------------------------------------

#[test]
fn positions_are_first_token_based() {
    // The binary node and its left operand start at the `x`; the right operand
    // carries its own column.
    let e = parse_expr("x + yy");
    assert_eq!(e.pos(), (1, 1));
    if let Expr::Binary { left, right, .. } = &e {
        assert_eq!(left.pos(), (1, 1));
        assert_eq!(right.pos(), (1, 5));
    } else {
        panic!("expected binary");
    }
}

#[test]
fn statement_position_on_later_line() {
    let prog = parse("x = 1\ny = 2\n");
    assert_eq!(prog[0].pos(), (1, 1));
    assert_eq!(prog[1].pos(), (2, 1));
}

#[test]
fn nested_body_positions() {
    let src = "\
if a:
    y = 2
";
    match parse_one(src) {
        Stmt::If { cond, body, .. } => {
            assert_eq!(cond.pos(), (1, 4));
            assert_eq!(body[0].pos(), (2, 5));
        }
        other => panic!("expected if, got {other:?}"),
    }
}

#[test]
fn unary_node_position_is_the_operator() {
    let e = parse_expr("-x");
    assert_eq!(e.pos(), (1, 1));
}

// --- Cut features: each must produce its own specific message ----------------

fn assert_cut(src: &str, needle: &str) {
    let e = parse_err(src);
    assert!(
        e.message.contains(needle),
        "for `{src}` expected message containing {needle:?}, got: {}",
        e.message
    );
}

#[test]
fn cut_walrus() {
    assert_cut("x := 5", "walrus");
    assert_cut("if (n := f()):\n    pass\n", "walrus");
}

#[test]
fn cut_match() {
    assert_cut("match x:\n    pass\n", "match statements are not supported");
}

#[test]
fn cut_list_comprehension() {
    assert_cut("[x for x in xs]", "list comprehensions are not supported");
}

#[test]
fn cut_dict_comprehension() {
    assert_cut("{k: v for k in xs}", "dict comprehensions are not supported");
}

#[test]
fn cut_set_comprehension() {
    assert_cut("{x for x in xs}", "set comprehensions are not supported");
}

#[test]
fn cut_generator_expression() {
    assert_cut("(x for x in xs)", "generator expressions are not supported");
}

#[test]
fn cut_with_statement() {
    assert_cut("with open('f') as fh:\n    pass\n", "`with` statement is not supported");
}

#[test]
fn cut_bare_except() {
    let src = "\
try:
    x = 1
except:
    pass
";
    assert_cut(src, "bare `except:` is not supported");
}

#[test]
fn cut_from_import() {
    assert_cut("from x import y", "`from X import Y` is not supported");
}

#[test]
fn cut_import_star() {
    assert_cut("import x.*", "`import *` is not supported");
}

#[test]
fn cut_multiple_inheritance() {
    assert_cut("class C(A, B):\n    pass\n", "multiple inheritance is not supported");
}

#[test]
fn cut_lambda() {
    assert_cut("f = lambda x: x", "lambda expressions are not supported");
}

#[test]
fn cut_global_and_nonlocal() {
    assert_cut("global x", "`global` statement is not supported");
    assert_cut("nonlocal x", "`nonlocal` statement is not supported");
}

#[test]
fn cut_async_await() {
    assert_cut("async x", "async/await is not supported");
    assert_cut("await x", "async/await is not supported");
}

#[test]
fn cut_del() {
    assert_cut("del x", "`del` statement is not supported");
}

#[test]
fn cut_assert() {
    assert_cut("assert x", "`assert` statement is not supported");
}

#[test]
fn cut_raise_from() {
    assert_cut("raise X from Y", "`raise X from Y` is not supported");
}

// --- Ordinary malformed input still errors gracefully (no panic) ------------

#[test]
fn unclosed_paren_errors() {
    let e = parse_err("(1 + 2");
    assert!(e.message.contains("expected"), "got: {}", e.message);
}

#[test]
fn missing_block_errors() {
    let e = parse_err("if a:\n");
    assert!(e.message.contains("indented block"), "got: {}", e.message);
}

#[test]
fn dangling_operator_errors() {
    let e = parse_err("1 +");
    assert!(e.message.contains("expected an expression"), "got: {}", e.message);
}

#[test]
fn empty_program_is_ok() {
    assert!(parse("").is_empty());
    assert!(parse("\n\n# just a comment\n").is_empty());
}
