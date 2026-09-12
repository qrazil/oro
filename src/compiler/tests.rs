//! Compiler pre-pass tests: slot allocation, forward references, block scope,
//! and closure capture. These assert on the shape of the compiled
//! [`CodeObject`] rather than on runtime behaviour (see `tests/` for that).

use super::*;
use crate::lexer::Lexer;
use crate::parser::Parser;

fn compile_src(src: &str) -> Rc<CodeObject> {
    let tokens = Lexer::new(src).tokenize().expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    compile(&program, Rc::from("test.oro")).expect("compile")
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
    let code = compile_src("if true:\n    y = 5\nprint(y)\n");
    // The trailing print(y) compiles to a LoadGlobal because y never leaked.
    let has_global_y = code
        .ops
        .iter()
        .any(|op| matches!(op, Op::LoadGlobal(n) if &*code.names[*n as usize] == "y"));
    assert!(has_global_y, "y must not be visible after the block");
}

#[test]
fn loop_accumulator_rebinds_outer_variable() {
    // `total` is declared before the loop; the in-loop assignment rebinds it
    // rather than creating a fresh block-local, so there is exactly one local
    // for it and no cell.
    let code = compile_src("total = 0\nfor _, i in [1, 2, 3]:\n    total = total + i\n");
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
/// instruction-cache density of the interpreter, and a power-of-two stride is
/// what turns `ops[pc]` into a shift rather than a multiply. It started at 48
/// bytes; one word is the design.
///
/// `Copy` matters just as much: the loop must read the instruction out before
/// executing it (execution can restructure `frames`), and while `Op` held an
/// `Rc<str>` that read was a refcount bump on every single instruction.
///
/// The rule that keeps both true is that no variant carries more than one
/// `u32`. If a new one needs more, put the payload in a `CodeObject` side table
/// (`names`, `classes`, `pairs`) and carry the index — do not raise these
/// numbers.
#[test]
fn op_is_one_word() {
    assert_eq!(
        std::mem::size_of::<Op>(),
        8,
        "Op grew — move the payload into a CodeObject side table rather than \
         widening every instruction"
    );
    fn assert_copy<T: Copy>() {}
    assert_copy::<Op>();
}

// --- Jumps out of `finally` -------------------------------------------------

/// A `return`, `break` or `continue` that leaves a `finally` body would discard
/// the exception it is running for. Refused, including from inside an `if` or
/// an inner `try` within the `finally`.
#[test]
fn a_jump_out_of_finally_is_refused() {
    let cases = [
        ("def f():\n    try:\n        pass\n    finally:\n        return 1\n", "`return`"),
        ("while true:\n    try:\n        pass\n    finally:\n        break\n", "`break`"),
        ("for _, x in [1]:\n    try:\n        pass\n    finally:\n        continue\n", "`continue`"),
        (
            "def f():\n    try:\n        pass\n    finally:\n        if true:\n            return 1\n",
            "`return`",
        ),
        (
            "def f():\n    try:\n        pass\n    finally:\n        try:\n            pass\n        \
             except ValueError:\n            return 1\n",
            "`return`",
        ),
    ];
    for (src, word) in cases {
        let e = compile_err(src);
        assert!(
            e.message.starts_with(word) && e.message.contains("inside `finally`"),
            "{src:?} gave {:?}",
            e.message
        );
    }
}

/// A jump that stays inside the `finally` is fine — a loop that starts there,
/// or a function defined there — and so is a `return` from the `try` body.
#[test]
fn a_jump_that_stays_inside_finally_is_allowed() {
    compile_src(
        "try:\n    pass\nfinally:\n    for _, x in [1, 2]:\n        if x == 1:\n            \
         continue\n        break\n",
    );
    compile_src("try:\n    pass\nfinally:\n    def g():\n        return 1\n");
    compile_src("def f():\n    for _, x in [1]:\n        try:\n            return x\n        finally:\n            pass\n");
}

// --- Type keywords ----------------------------------------------------------

fn compile_err(src: &str) -> CompileError {
    let tokens = Lexer::new(src).tokenize().expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    compile(&program, Rc::from("test.oro")).expect_err("expected a compile error")
}

/// Every form that binds a name refuses a type keyword, and each says which
/// form it was — a `def` is not an assignment and the message should not
/// pretend otherwise.
///
/// These live here rather than in `corpus/` for a structural reason: a corpus
/// file stops at its first error, so eight rejections would be eight files;
/// and `tests/fmt_test.rs` formats every `.oro` in the tree, so a rejection
/// that happened at *parse* time could not have a corpus file at all. The one
/// that is oracled is `corpus/divergence/66_type_keywords.oro`.
#[test]
fn every_binding_form_refuses_a_type_keyword() {
    let cases = [
        ("dict = {}\n", "a variable"),
        ("str += 1\n", "a variable"),
        ("a, list = 1, 2\n", "a variable"),
        ("for _, str in [1]:\n    pass\n", "a loop variable"),
        ("def bytes():\n    pass\n", "a function name"),
        ("class int:\n    pass\n", "a class name"),
        ("def f(dict):\n    return dict\n", "a parameter name"),
        ("f = (str) => 1\n", "a parameter name"),
        ("import re as dict\n", "an imported name"),
        ("try:\n    pass\nexcept ValueError as bytes:\n    pass\n", "an `except ... as` name"),
        ("def f():\n    global int\n    int = 2\n", "a `global` declaration"),
        ("Task = 1\n", "a variable"),
        ("File = 1\n", "a variable"),
    ];
    for (src, phrase) in cases {
        let e = compile_err(src);
        assert!(
            e.message.contains("is a type name") && e.message.contains(phrase),
            "{src:?} gave {:?}, which does not name {phrase}",
            e.message
        );
    }
}

/// Every `for` binds an `(index, value)` pair. A single binding is refused
/// naming the pair form; a target of any width other than two is refused naming
/// the nested spelling for a tuple element.
#[test]
fn a_for_target_must_be_a_pair() {
    let single = compile_err("for x in [1, 2]:\n    pass\n");
    assert!(single.message.contains("binds an (index, value) pair"), "got {}", single.message);
    assert!(single.message.contains("for _, x in xs"), "got {}", single.message);

    let three = compile_err("for a, b, c in [(1, 2, 3)]:\n    pass\n");
    assert!(three.message.contains("this target has 3 names"), "got {}", three.message);
    assert!(three.message.contains("for _, (…) in xs"), "got {}", three.message);

    // The pair forms all compile.
    compile_src("for i, v in [1, 2]:\n    pass\n");
    compile_src("for _, v in [1, 2]:\n    pass\n");
    compile_src("for i, _ in [1, 2]:\n    pass\n");
    compile_src("for _, (a, b) in [(1, 2)]:\n    pass\n");
}

/// `while` is for a condition, not a counter: `while name < bound` whose body
/// steps `name` by an integer constant is refused, pointing at `for`. A step by
/// a runtime value, a non-`<`/`>` condition, a compound condition and
/// `while true` are genuine conditions and compile.
#[test]
fn a_counting_while_is_refused() {
    for src in [
        "i = 0\nwhile i < 10:\n    i = i + 1\n",
        "i = 0\nwhile i < 10:\n    i += 1\n",
        "i = 0\nwhile i <= 9:\n    x = i\n    i = i + 2\n",
        "i = 10\nwhile i > 0:\n    i = i - 1\n",
    ] {
        let e = compile_err(src);
        assert!(e.message.contains("counts `i` by a constant"), "{src:?} gave {}", e.message);
    }
    // Genuine conditions — all compile.
    compile_src("while true:\n    break\n");
    compile_src("c = b\"x\"\nwhile c != b\"\":\n    c = b\"\"\n"); // EOF-drain sentinel
    compile_src("got = 0\nn = 9\nk = 3\nwhile got < n:\n    got = got + k\n"); // runtime step
    compile_src("n = 1\nwhile n < 1000:\n    n = n * 3\n"); // not `+`/`-`
    compile_src("i = 0\nwhile i < 10 and true:\n    i = i + 1\n"); // compound condition
    compile_src("xs = [1]\nwhile len(xs) != 0:\n    xs = xs.drop(1)\n"); // condition, not a name
}

/// `_` is a discard: it binds nothing (so `for _, _ in xs` and `a, _ = pair`
/// compile with no duplicate-binding error), and reading it back is refused.
#[test]
fn underscore_is_a_discard() {
    compile_src("for _, _ in [(1, 2)]:\n    pass\n");
    compile_src("a, _ = (1, 2)\nprint(a)\n");
    compile_src("_ = f()\n"); // a call whose result is deliberately dropped

    let e = compile_err("a, _ = (1, 2)\nprint(_)\n");
    assert!(e.message.contains("`_` is a discard"), "got {}", e.message);
    let e2 = compile_err("x = _ + 1\n");
    assert!(e2.message.contains("cannot be read"), "got {}", e2.message);
}

/// A type keyword is reserved in the *variable* namespace and nowhere else. A
/// member is reached through an object and can never be mistaken for the type,
/// and the tree depends on this: `std/http.oro` alone calls `.bytes()` sixteen
/// times and defines one.
#[test]
fn a_type_keyword_is_still_a_member_name() {
    compile_src("class W:\n    def bytes(self):\n        return b\"\"\n\nw = W()\nx = w.bytes()\n");
    compile_src("d = {\"list\": 1}\nx = d[\"list\"]\n");
    compile_src("w = null\nw.dict = 1\n");
}

/// A type name is a constant, not a global lookup — which is also what takes
/// `range(n)` off the `LoadGlobal` path it used to sit on.
#[test]
fn a_type_keyword_compiles_to_a_constant() {
    let code = compile_src("x = type(1) == int\n");
    assert!(
        code.consts.iter().any(|c| matches!(c, Value::Type(crate::value::TypeTag::Int))),
        "`int` should be in the constant pool"
    );
    assert!(
        !code.ops.iter().any(|op| matches!(op, Op::LoadGlobal(n)
            if code.names[*n as usize].as_ref() == "int")),
        "`int` must not be looked up as a global"
    );
}
