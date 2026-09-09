//! Compiler pre-pass tests: slot allocation, forward references, block scope,
//! and closure capture. These assert on the shape of the compiled
//! [`CodeObject`] rather than on runtime behaviour (see `tests/` for that).

use super::*;
use crate::lexer::Lexer;
use crate::parser::Parser;

fn compile_src(src: &str) -> Rc<CodeObject> {
    let tokens = Lexer::new(src).tokenize().expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    compile(&program).expect("compile")
}

/// Find the first `MakeFunction` prototype in a code object.
fn first_proto(code: &CodeObject) -> &FuncProto {
    code.protos.first().expect("expected a function prototype")
}

#[test]
fn top_level_names_get_local_slots() {
    let code = compile_src("x = 1\ny = 2\nz = x + y\n");
    // Three module-level names, none captured, so three plain locals.
    assert_eq!(code.nlocals, 3);
    assert_eq!(code.ncells, 0);
}

#[test]
fn forward_reference_between_functions_resolves() {
    // `a` calls `b`, defined later: the pre-pass must have numbered `b` before
    // `a`'s body is compiled. `b` is captured by `a`, so the module holds it in
    // a cell.
    let code = compile_src(
        "def a():\n    return b()\n\ndef b():\n    return 1\n\nresult = a()\n",
    );
    assert!(code.ncells >= 1, "b must be a module cell captured by a");
    let a_proto = first_proto(&code);
    assert_eq!(a_proto.code.nfree, 1, "a captures b as a free variable");
}

#[test]
fn closure_captures_outer_local() {
    let code = compile_src(
        "def outer():\n    n = 10\n    def inner():\n        return n\n    return inner\n",
    );
    let outer = &first_proto(&code).code;
    assert_eq!(outer.ncells, 1, "n is captured, so it lives in a cell");
    let inner = &first_proto(outer).code;
    assert_eq!(inner.nfree, 1, "inner sees n as a free variable");
}

#[test]
fn block_scope_variable_is_function_local_not_leaked_upward() {
    // `y` is only bound inside the `if` block; referencing it afterwards must
    // resolve to a builtin/global lookup, not a local slot.
    let code = compile_src("if True:\n    y = 5\nprint(y)\n");
    // The trailing print(y) compiles to a LoadGlobal because y never leaked.
    let has_global_y = code
        .ops
        .iter()
        .any(|op| matches!(op, Op::LoadGlobal(n) if &**n == "y"));
    assert!(has_global_y, "y must not be visible after the block");
}

#[test]
fn loop_accumulator_rebinds_outer_variable() {
    // `total` is declared before the loop; the in-loop assignment rebinds it
    // rather than creating a fresh block-local, so there is exactly one local
    // for it and no cell.
    let code = compile_src("total = 0\nfor i in [1, 2, 3]:\n    total = total + i\n");
    // `total` and (nothing else at module level) => at least one local.
    assert!(code.nlocals >= 1);
    assert_eq!(code.ncells, 0);
}

#[test]
fn params_are_locals_of_the_function() {
    let code = compile_src("def f(a, b, c):\n    return a + b + c\n");
    let f = &first_proto(&code).code;
    assert_eq!(f.params.len(), 3);
    assert_eq!(f.nlocals, 3);
}

#[test]
fn integer_literal_promotes_to_bigint_when_too_large() {
    let code = compile_src("x = 100000000000000000000000000000\n");
    assert!(
        code.consts.iter().any(|c| matches!(c, Value::Big(_))),
        "an over-large literal must be stored as a BigInt constant"
    );
}

/// `Op` is the unit the dispatch loop streams through, so its width *is* the
/// instruction-cache density of the interpreter. It was 48 bytes because
/// `BuildClass` carried an inline `Vec<Rc<str>>` — one variant, compiled and
/// executed once per program, taxing every instruction in every program.
/// Boxing that payload was the fix; this assertion is what keeps it fixed.
///
/// If a new variant widens `Op`, box its payload (see `ClassSpec`) rather than
/// raising this number.
#[test]
fn op_stays_narrow() {
    assert_eq!(
        std::mem::size_of::<Op>(),
        24,
        "Op grew — box the offending variant's payload instead of widening every instruction"
    );
}
