//! VM unit tests, one cluster per opcode class. Each test runs a tiny program
//! whose module-level variables are inspected after execution (declaration
//! order fixes their slots, so the first declared name is slot 0).

use super::*;
use crate::compiler::compile;
use crate::lexer::Lexer;
use crate::parser::Parser;

/// Compile `src` as a module body.
fn compile_module(src: &str) -> Rc<CodeObject> {
    let tokens = Lexer::new(src).tokenize().expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    compile(&program, Rc::from("test.oro")).expect("compile")
}

/// Run `src` and return the module's local slots.
fn run_locals(src: &str) -> Vec<Value> {
    let mut vm = Vm::new(Vec::new());
    vm.push_module_frame(compile_module(src));
    vm.run_loop().expect("run");
    vm.task.last_locals
}

/// Run `src` and return the value bound to the first module variable (slot 0).
fn eval(src: &str) -> Value {
    run_locals(src).into_iter().next().expect("at least one local")
}

/// Run `src` and return the highest-numbered module local — handy when a `def`
/// or earlier binding occupies the low slots and the result variable is last.
fn eval_last(src: &str) -> Value {
    run_locals(src).into_iter().next_back().expect("at least one local")
}

/// Run `src` and return the module variable called `name`.
///
/// Safer than [`eval_last`] for anything with a `for` in it: a loop variable
/// takes a slot of its own without being a module name, so "the last slot" and
/// "the last variable I wrote" stop agreeing.
fn eval_var(src: &str, name: &str) -> Value {
    let code = compile_module(src);
    let mut vm = Vm::new(Vec::new());
    vm.push_module_frame(code.clone());
    vm.run_loop().expect("run");
    let (_, target) = code
        .module_names
        .iter()
        .find(|(n, _)| &**n == name)
        .unwrap_or_else(|| panic!("no module variable called `{name}`"));
    match target {
        VarTarget::Local(s) => vm.task.last_locals[*s as usize].clone(),
        VarTarget::Cell(_) => {
            panic!("`{name}` is captured by a closure; pass it as an argument instead")
        }
    }
}

fn run_err(src: &str) -> RuntimeError {
    let tokens = Lexer::new(src).tokenize().expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    let code = compile(&program, Rc::from("test.oro")).expect("compile");
    run(code).expect_err("expected a runtime error")
}

fn compile_err(src: &str) -> crate::compiler::CompileError {
    let tokens = Lexer::new(src).tokenize().expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    compile(&program, Rc::from("test.oro")).expect_err("expected a compile error")
}

/// `raise` takes an instance. `raise E` used to construct one silently, so
/// `raise <name>` meant re-raise or construct depending on what the name held.
#[test]
fn raise_needs_an_instance_not_a_class() {
    let src = "def f():\n    try:\n        raise ValueError\n    except TypeError as e:\n        return f\"{e}\"\n\
               r = f()\n";
    match eval_last(src) {
        Value::Str(s) => assert!(s.s.contains("write `raise ValueError()`"), "{:?}", s.s),
        other => panic!("expected str, got {}", other.repr()),
    }
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        other => panic!("expected int, got {}", other.repr()),
    }
}

#[test]
fn arithmetic_and_precedence() {
    assert_eq!(int(&eval("r = 2 + 3 * 4\n")), 14);
    assert_eq!(int(&eval("r = (2 + 3) * 4\n")), 20);
    assert_eq!(int(&eval("r = 17 % 5\n")), 2);
    assert_eq!(int(&eval("r = 17 // 5\n")), 3);
    assert_eq!(eval("r = 3 / 2\n").repr(), "1.5");
    assert_eq!(int(&eval("r = 2 ** 10\n")), 1024);
}

#[test]
fn integer_overflow_promotes_to_bigint() {
    // i64::MAX + 1 must not wrap; it promotes to a BigInt.
    let v = eval("r = 9223372036854775807 + 1\n");
    assert!(matches!(v, Value::Big(_)));
    assert_eq!(v.repr(), "9223372036854775808");
}

#[test]
fn floor_div_and_mod_follow_python_signs() {
    assert_eq!(int(&eval("r = -7 // 2\n")), -4);
    assert_eq!(int(&eval("r = -7 % 2\n")), 1);
    assert_eq!(int(&eval("r = 7 % -2\n")), -1);
}

#[test]
fn comparison_and_chaining() {
    assert!(matches!(eval("r = 1 < 2 < 3\n"), Value::Bool(true)));
    assert!(matches!(eval("r = 1 < 2 > 5\n"), Value::Bool(false)));
    assert!(matches!(eval("r = 3 == 3\n"), Value::Bool(true)));
    assert!(matches!(eval("r = 1 == 1.0\n"), Value::Bool(true)));
}

#[test]
fn boolean_short_circuit() {
    assert_eq!(int(&eval("r = 0 or 5\n")), 5);
    assert_eq!(int(&eval("r = 3 and 4\n")), 4);
    assert!(matches!(eval("r = not 0\n"), Value::Bool(true)));
}

#[test]
fn truthiness_matches_python() {
    assert!(matches!(eval("r = [].to_bool()\n"), Value::Bool(false)));
    assert!(matches!(eval("r = [0].to_bool()\n"), Value::Bool(true)));
    assert!(matches!(eval("r = \"\".to_bool()\n"), Value::Bool(false)));
    assert!(matches!(eval("r = (0.0).to_bool()\n"), Value::Bool(false)));
}

#[test]
fn control_flow_if_while() {
    let src = "\
r = 0
n = 5
while n > 0:
    r = r + n
    n = n - 1
";
    assert_eq!(int(&eval(src)), 15);

    // `r` is declared at module level first, so the in-branch assignments
    // rebind that one variable (slot 0) rather than creating block-locals.
    let src2 = "\
r = 0
x = 7
if x > 10:
    r = 1
elif x > 5:
    r = 2
else:
    r = 3
";
    assert_eq!(int(&eval(src2)), 2);
}

#[test]
fn for_loop_over_range_and_break_continue() {
    let src = "\
r = 0
for i in range(10):
    if i == 3:
        continue
    if i == 6:
        break
    r = r + i
";
    // 0 + 1 + 2 + 4 + 5 = 12
    assert_eq!(int(&eval(src)), 12);
}

#[test]
fn subscript_and_slice() {
    assert_eq!(int(&eval("r = [10, 20, 30][1]\n")), 20);
    assert_eq!(int(&eval("r = [10, 20, 30][-1]\n")), 30);
    assert_eq!(eval("r = \"abcdef\"[1:4]\n").repr(), "'bcd'");
    assert_eq!(eval("r = [1, 2, 3, 4][::2]\n").repr(), "[1, 3]");
    assert_eq!(eval("r = \"abc\"[::-1]\n").repr(), "'cba'");
}

/// The `step == 1` fast path in `slice_get` is a *different* code path from the
/// general one, so it needs its own clamping cases: out-of-range bounds, an
/// inverted range, and negative indices, on a string that is not ASCII (where
/// character indices and byte offsets disagree and a naive byte slice would
/// either panic or cut a character in half).
#[test]
fn unit_step_slices_clamp_like_python() {
    let s = "s = \"a\u{e9}\u{4e2d}\u{1f600}b\"\n";
    assert_eq!(eval_last(&format!("{s}r = s[1:3]\n")).repr(), "'\u{e9}\u{4e2d}'");
    assert_eq!(eval_last(&format!("{s}r = s[2:]\n")).repr(), "'\u{4e2d}\u{1f600}b'");
    assert_eq!(eval_last(&format!("{s}r = s[:2]\n")).repr(), "'a\u{e9}'");
    assert_eq!(eval_last(&format!("{s}r = s[-2:]\n")).repr(), "'\u{1f600}b'");
    assert_eq!(eval_last(&format!("{s}r = s[:-3]\n")).repr(), "'a\u{e9}'");
    // Out of range in both directions, and an inverted range, are all empty
    // or clamped rather than an error — Python's rule, and the one the general
    // path already implemented.
    assert_eq!(eval_last(&format!("{s}r = s[3:1]\n")).repr(), "''");
    assert_eq!(eval_last(&format!("{s}r = s[9:99]\n")).repr(), "''");
    assert_eq!(eval_last(&format!("{s}r = s[-99:99]\n")).repr(), "'a\u{e9}\u{4e2d}\u{1f600}b'");
    assert_eq!(eval_last(&format!("{s}r = s[5:5]\n")).repr(), "''");
    assert_eq!(eval("r = \"\"[0:5]\n").repr(), "''");
    assert_eq!(eval("r = [1, 2, 3][2:99]\n").repr(), "[3]");
    assert_eq!(eval("r = (1, 2, 3)[-99:2]\n").repr(), "(1, 2)");
    assert_eq!(eval("r = b\"hello\"[3:1]\n").repr(), "b''");
}

#[test]
fn augmented_assignment_on_name_and_subscript() {
    assert_eq!(int(&eval("r = 10\nr += 5\nr *= 2\n")), 30);
    let src = "\
d = {}
d[\"x\"] = 1
d[\"x\"] += 41
r = d[\"x\"]
";
    let locals = run_locals(src);
    // d slot 0, r slot 1
    assert_eq!(int(&locals[1]), 42);
}

#[test]
fn functions_defaults_and_recursion() {
    assert_eq!(int(&eval_last("def f(a, b=10):\n    return a + b\nr = f(5)\n")), 15);
    assert_eq!(int(&eval_last("def f(a, b=10):\n    return a + b\nr = f(5, 20)\n")), 25);
    let fib = "\
def fib(n):
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)
r = fib(12)
";
    assert_eq!(int(&eval_last(fib)), 144);
}

#[test]
fn varargs_and_kwargs_binding() {
    let src = "\
def f(a, *rest, **opts):
    return a + rest.sum() + opts.get(\"bonus\", 0)
r = f(1, 2, 3, bonus=100)
";
    assert_eq!(int(&eval_last(src)), 106);
}

#[test]
fn closure_captures_by_reference() {
    let src = "\
def make():
    box = [0]
    def bump():
        box[0] = box[0] + 1
        return box[0]
    return bump
b = make()
x = b()
r = b()
";
    // `make`, `b`, `x`, `r` are all module locals; `r` holds the last call's
    // result, showing the captured cell persisted across calls.
    assert_eq!(int(&eval_last(src)), 2);
}

#[test]
fn deep_recursion_does_not_overflow_native_stack() {
    // 30000 nested Oro calls; would blow an 8MB native stack if the interpreter
    // recursed in Rust. It does not, because frames live on the heap.
    let src = "\
def down(n):
    if n == 0:
        return 0
    return 1 + down(n - 1)
r = down(30000)
";
    assert_eq!(int(&eval(src)), 30000);
}

#[test]
fn runtime_type_error_is_clean_with_position() {
    let err = run_err("x = 1\ny = x + \"z\"\n");
    assert_eq!(err.line, 2);
    assert!(err.message.contains("unsupported operand"));
}

#[test]
fn mutating_list_during_iteration_is_a_clean_error() {
    // Must surface as an Oro runtime error, never a Rust panic (RefCell borrow).
    let err = run_err("xs = [1, 2, 3]\nfor v in xs:\n    xs.append(v)\n");
    assert!(err.message.contains("changed size"), "got: {}", err.message);
}

#[test]
fn name_error_for_unknown_global() {
    let err = run_err("r = nonexistent_name\n");
    assert!(err.message.contains("is not defined"));
}

// --- f-strings: format specs, conversions, nesting, escapes ---------------

/// The string bound to the last module variable.
fn fstr(src: &str) -> String {
    match eval_last(src) {
        Value::Str(s) => s.s.clone(),
        other => panic!("expected str, got {}", other.repr()),
    }
}

#[test]
fn fstring_plain_interpolation() {
    assert_eq!(fstr("name = \"oro\"\nout = f\"lang={name}\"\n"), "lang=oro");
    assert_eq!(fstr("n = 42\nout = f\"n={n} x2={n * 2}\"\n"), "n=42 x2=84");
}

#[test]
fn fstring_width_and_alignment() {
    assert_eq!(fstr("n = 42\nout = f\"{n:5}\"\n"), "   42");
    assert_eq!(fstr("n = 42\nout = f\"{n:<5}\"\n"), "42   ");
    assert_eq!(fstr("n = 42\nout = f\"{n:^6}\"\n"), "  42  ");
    assert_eq!(fstr("s = \"hi\"\nout = f\"{s:>5}\"\n"), "   hi");
}

#[test]
fn fstring_zero_pad_and_thousands() {
    assert_eq!(fstr("n = 42\nout = f\"{n:05d}\"\n"), "00042");
    assert_eq!(fstr("n = 1234567\nout = f\"{n:,}\"\n"), "1,234,567");
}

#[test]
fn fstring_float_precision() {
    assert_eq!(fstr("x = 3.14159\nout = f\"{x:.2f}\"\n"), "3.14");
    assert_eq!(fstr("x = 3.14159\nout = f\"{x:10.3f}\"\n"), "     3.142");
}

#[test]
fn fstring_conversions() {
    assert_eq!(fstr("s = \"hi\"\nout = f\"{s!r}\"\n"), "'hi'");
    assert_eq!(fstr("s = \"hi\"\nout = f\"{s!s}\"\n"), "hi");
}

#[test]
fn fstring_nested_spec() {
    // Precision comes from an inner expression.
    assert_eq!(fstr("x = 3.14159\np = 2\nout = f\"{x:.{p}f}\"\n"), "3.14");
    assert_eq!(fstr("x = 3.14159\np = 4\nout = f\"{x:.{p}f}\"\n"), "3.1416");
    // Width from an inner expression, too.
    assert_eq!(fstr("n = 7\nw = 4\nout = f\"{n:{w}}\"\n"), "   7");
}

#[test]
fn fstring_escapes_and_literal_braces() {
    assert_eq!(fstr("out = f\"a\\tb\"\n"), "a\tb");
    assert_eq!(fstr("out = f\"a\\nb\"\n"), "a\nb");
    assert_eq!(fstr("out = f\"{{literal}}\"\n"), "{literal}");
}

// --- global statement -----------------------------------------------------

#[test]
fn global_lets_a_function_mutate_module_state() {
    let src = "count = 0\n\
               def bump():\n    global count\n    count = count + 1\n    return count\n\
               r1 = bump()\nr2 = bump()\n";
    // count, bump, r1, r2 — inspect module `count` (slot 0, a cell).
    let locals = run_locals(src);
    // After two bumps the module count is 2; r2 is 2, r1 is 1.
    let vals: Vec<i64> = locals.iter().filter_map(|v| match v {
        Value::Int(i) => Some(*i),
        _ => None,
    }).collect();
    assert!(vals.contains(&2), "expected count/r2 == 2, got {vals:?}");
    assert!(vals.contains(&1), "expected r1 == 1, got {vals:?}");
}

#[test]
fn global_creates_module_binding_when_absent() {
    let src = "def make():\n    global created\n    created = 99\n\
               make()\nout = created\n";
    assert_eq!(int(&eval_last(src)), 99);
}

#[test]
fn global_with_multiple_names() {
    // `a`/`b` are captured (cells), so read them into plain module locals to
    // observe the swap.
    let src = "a = 1\nb = 2\n\
               def swap():\n    global a, b\n    a, b = b, a\n\
               swap()\nra = a\nrb = b\n";
    let locals = run_locals(src);
    let ra = int(locals.iter().rev().nth(1).unwrap());
    let rb = int(locals.last().unwrap());
    assert_eq!(ra, 2);
    assert_eq!(rb, 1);
}

#[test]
fn assignment_without_global_stays_local() {
    // The module binding must be untouched.
    let src = "n = 100\n\
               def shadow():\n    n = 5\n    return n\n\
               inner = shadow()\n";
    let locals = run_locals(src);
    assert_eq!(int(&locals[0]), 100); // module n unchanged
}

#[test]
fn unbound_local_shadowing_module_teaches_global() {
    let err = run_err("count = 0\n\
                       def bump():\n    count = count + 1\n    return count\n\
                       bump()\n");
    assert!(err.message.contains("global count"), "got: {}", err.message);
    assert!(err.message.contains("shadows the module-level"), "got: {}", err.message);
}

// --- match: value-only switch ---------------------------------------------

#[test]
fn match_literal_jump_table() {
    // All-literal patterns compile to MatchDispatch.
    let src = "def f(w):\n    match w:\n        case \"a\":\n            return 1\n        \
               case \"b\":\n            return 2\n        case _:\n            return 0\n\
               r1 = f(\"a\")\nr2 = f(\"b\")\nr3 = f(\"z\")\n";
    let locals = run_locals(src);
    let got: Vec<i64> = locals.iter().filter_map(|v| match v {
        Value::Int(i) => Some(*i),
        _ => None,
    }).collect();
    assert!(got.contains(&1) && got.contains(&2) && got.contains(&0), "got {got:?}");
}

#[test]
fn match_no_default_is_noop() {
    let src = "def f(n):\n    x = 10\n    match n:\n        case 1:\n            x = 1\n\
               \n    return x\n\
               hit = f(1)\nmiss = f(9)\n";
    let locals = run_locals(src);
    let got: Vec<i64> = locals.iter().filter_map(|v| match v {
        Value::Int(i) => Some(*i),
        _ => None,
    }).collect();
    assert!(got.contains(&1), "expected a hit==1, got {got:?}");
    assert!(got.contains(&10), "expected a miss==10, got {got:?}");
}

#[test]
fn match_numeric_cross_equality_first_wins() {
    // 1 == true == 1.0; the first matching case wins, even through the table.
    let src = "def f(v):\n    match v:\n        case 1:\n            return \"one\"\n        \
               case true:\n            return \"hit\"\n        case _:\n            return \"x\"\n\
               a = f(1)\nb = f(true)\n";
    let locals = run_locals(src);
    for v in locals {
        if let Value::Str(s) = v {
            if s.s == "hit" {
                panic!("true should have matched `case 1` first, not `case true`");
            }
        }
    }
}

#[test]
fn match_case_body_is_block_scoped() {
    // A name bound in a case body must not leak (Oro divergence from Python).
    let err = run_err("match 1:\n    case 1:\n        leaked = 5\nr = leaked\n");
    assert!(err.message.contains("not defined"), "got: {}", err.message);
}

#[test]
fn match_dotted_pattern_runs_and_falls_through() {
    // Dotted pattern forces the compare-chain path; a string method never
    // equals the subject, so control reaches the default.
    let src = "def f(s):\n    text = \"x\"\n    match s:\n        case text.upper:\n            \
               return 1\n        case _:\n            return 0\n\
               r = f(\"anything\")\n";
    assert_eq!(int(&eval_last(src)), 0);
}

// --- classes --------------------------------------------------------------

#[test]
fn class_instantiation_and_methods() {
    let src = "class Counter:\n    def __init__(self, start):\n        self.n = start\n    \
               def inc(self):\n        self.n = self.n + 1\n        return self.n\n\
               c = Counter(10)\nr1 = c.inc()\nr2 = c.inc()\n";
    let locals = run_locals(src);
    let ints: Vec<i64> = locals.iter().filter_map(|v| match v {
        Value::Int(i) => Some(*i),
        _ => None,
    }).collect();
    assert!(ints.contains(&11) && ints.contains(&12), "got {ints:?}");
}

#[test]
fn class_super_and_inheritance() {
    let src = "class A:\n    def val(self):\n        return 1\n\
               class B(A):\n    def val(self):\n        return super().val() + 10\n\
               b = B()\nr = b.val()\n";
    assert_eq!(int(&eval_last(src)), 11);
}

#[test]
fn class_arithmetic_dunder() {
    let src = "class N:\n    def __init__(self, v):\n        self.v = v\n    \
               def __add__(self, other):\n        return N(self.v + other.v)\n\
               r = (N(2) + N(3)).v\n";
    assert_eq!(int(&eval_last(src)), 5);
}

#[test]
fn unsupported_class_dunder_is_rejected() {
    let err = compile_err("class X:\n    def __getattr__(self, n):\n        return 0\n");
    assert!(err.message.contains("__getattr__"), "got: {}", err.message);
}

/// `__hash__` used to be accepted, callable and inert — the one shape a removal
/// must never take. It is rejected at the class now, naming the tuple key that
/// replaces it (`docs/hash-and-equality.md` §7).
#[test]
fn hash_dunder_is_rejected() {
    let err = compile_err("class X:\n    def __hash__(self):\n        return 0\n");
    assert!(err.message.contains("__hash__"), "got: {}", err.message);
    assert!(err.message.contains("d[(self.row, self.col)]"), "got: {}", err.message);
}

// --- exceptions -----------------------------------------------------------

#[test]
fn exception_caught_by_type() {
    let src = "def f():\n    try:\n        raise ValueError(\"x\")\n    except ValueError as e:\n        return f\"{e}\"\n\
               r = f()\n";
    assert_eq!(fstr("def g():\n    return \"x\"\nr=g()\n"), "x"); // sanity
    match eval_last(src) {
        Value::Str(s) => assert_eq!(s.s, "x"),
        other => panic!("expected str, got {}", other.repr()),
    }
}

#[test]
fn exception_base_catches_subclass() {
    let src = "def f():\n    try:\n        raise ValueError(\"boom\")\n    except Exception:\n        return 1\n\
               r = f()\n";
    assert_eq!(int(&eval_last(src)), 1);
}

#[test]
fn runtime_error_is_catchable_with_right_type() {
    let src = "def f():\n    try:\n        return [][0]\n    except IndexError:\n        return 7\n\
               r = f()\n";
    assert_eq!(int(&eval_last(src)), 7);
    let src2 = "def f():\n    try:\n        return 1 // 0\n    except ZeroDivisionError:\n        return 9\n\
                r = f()\n";
    assert_eq!(int(&eval_last(src2)), 9);
}

#[test]
fn finally_runs_on_return() {
    // `log` mutated via global proves finally ran despite the early return.
    let src = "log = 0\ndef f():\n    global log\n    try:\n        return 1\n    finally:\n        log = 99\n\
               r = f()\n";
    let locals = run_locals(src);
    // log is captured (cell), so read it into a plain local:
    let src2 = "log = 0\ndef f():\n    global log\n    try:\n        return 1\n    finally:\n        log = 99\n\
                r = f()\nout = log\n";
    assert_eq!(int(&eval_last(src2)), 99);
    let _ = locals;
}

#[test]
fn uncaught_exception_names_type_and_message() {
    let err = run_err("raise ValueError(\"nope\")\n");
    assert!(err.message.contains("ValueError"), "got: {}", err.message);
    assert!(err.message.contains("nope"), "got: {}", err.message);
}

#[test]
fn user_exception_subclass() {
    let src = "class MyError(Exception):\n    pass\n\
               def f():\n    try:\n        raise MyError(\"custom\")\n    except Exception as e:\n        return f\"{e}\"\n\
               r = f()\n";
    match eval_last(src) {
        Value::Str(s) => assert_eq!(s.s, "custom"),
        other => panic!("expected str, got {}", other.repr()),
    }
}

// --- imports & stdlib -----------------------------------------------------

#[test]
fn import_binds_last_segment() {
    // `import os` binds `os` to a module.
    assert!(matches!(eval("import os\nr = os\n"), Value::Module(_)));
}

#[test]
fn os_path_functions() {
    assert_eq!(fstr("import os\nr = os.path.join(\"a\", \"b\")\n"), "a/b");
    assert_eq!(fstr("import os\nr = os.path.basename(\"/x/y/z.txt\")\n"), "z.txt");
}

#[test]
fn unknown_module_raises_module_not_found() {
    let err = run_err("import nonexistent_module\n");
    assert!(err.message.contains("No module named"), "got: {}", err.message);
}

// --- generators -----------------------------------------------------------

#[test]
fn generator_basic_iteration() {
    let src = "def up(n):\n    i = 0\n    while i < n:\n        yield i\n        i = i + 1\n\
               total = 0\nfor v in up(5):\n    total = total + v\n";
    // total is a module local; sum 0..4 = 10
    let locals = run_locals(src);
    assert!(locals.iter().any(|v| matches!(v, Value::Int(10))), "expected 10 in {locals:?}");
}

#[test]
fn generator_consumes_generator() {
    let src = "def up(n):\n    i = 0\n    while i < n:\n        yield i\n        i = i + 1\n\
               def evens(n):\n    for x in up(n):\n        if x % 2 == 0:\n            yield x\n\
               got = []\nfor v in evens(10):\n    got.append(v)\n";
    let locals = run_locals(src);
    let list = locals.iter().find_map(|v| match v {
        Value::List(l) => Some(l.borrow().iter().map(|x| match x { Value::Int(i)=>*i, _=>-1 }).collect::<Vec<_>>()),
        _ => None,
    }).expect("a list local");
    assert_eq!(list, vec![0, 2, 4, 6, 8]);
}

#[test]
fn calling_generator_does_not_run_body() {
    // If the body ran eagerly, `marker` would be mutated; it must not be.
    let src = "log = []\ndef g():\n    log.append(1)\n    yield 1\n\
               it = g()\nn = len(log)\n";
    let locals = run_locals(src);
    // n == 0 proves the body has not executed yet.
    assert!(locals.iter().any(|v| matches!(v, Value::Int(0))), "gen body ran too early: {locals:?}");
}

#[test]
fn generator_is_a_generator_value() {
    assert!(matches!(
        eval_last("def g():\n    yield 1\nr = g()\n"),
        Value::Generator(_)
    ));
}

// --- break/continue run finally -------------------------------------------

#[test]
fn break_runs_enclosing_finally() {
    let src = "log = []\ndef f():\n    global log\n    for i in range(3):\n        try:\n            if i == 1:\n                break\n            log.append(i)\n        finally:\n            log.append(10 + i)\nf()\nout = log\n";
    let v = eval_last(src);
    let got: Vec<i64> = match v {
        Value::List(l) => l.borrow().iter().map(|x| match x { Value::Int(i)=>*i, _=>-1 }).collect(),
        _ => panic!("expected list"),
    };
    // i=0: append 0, finally 10; i=1: break but finally 11 still runs.
    assert_eq!(got, vec![0, 10, 11]);
}

#[test]
fn continue_runs_enclosing_finally() {
    let src = "log = []\ndef f():\n    global log\n    for i in range(3):\n        try:\n            if i == 1:\n                continue\n            log.append(i)\n        finally:\n            log.append(10 + i)\nf()\nout = log\n";
    let got: Vec<i64> = match eval_last(src) {
        Value::List(l) => l.borrow().iter().map(|x| match x { Value::Int(i)=>*i, _=>-1 }).collect(),
        _ => panic!("expected list"),
    };
    // Every iteration runs its finally, even the one that continues.
    assert_eq!(got, vec![0, 10, 11, 2, 12]);
}

// --- container repr dispatches element dunders ----------------------------

#[test]
fn container_repr_runs_element_dunders() {
    let src = "class P:\n    def __init__(self, n):\n        self.n = n\n    def __repr__(self):\n        return \"P(\" + self.n.to_str() + \")\"\n\
               out = [P(1), P(2)].to_str()\n";
    assert_eq!(fstr(src), "[P(1), P(2)]");
}

#[test]
fn nested_container_repr() {
    let src = "class P:\n    def __init__(self, n):\n        self.n = n\n    def __repr__(self):\n        return \"P\" + self.n.to_str()\n\
               out = {\"k\": [P(1), P(2)]}.to_str()\n";
    assert_eq!(fstr(src), "{'k': [P1, P2]}");
}

#[test]
fn self_referential_list_repr_terminates() {
    let src = "a = [1]\na.append(a)\nout = repr(a)\n";
    assert_eq!(fstr(src), "[1, [...]]");
}

// --- stdlib primitives: re / subprocess / time ----------------------------

#[test]
fn re_search_and_groups() {
    let src = "import re\nm = re.search(r\"(\\w+)-(\\d+)\", \"id: abc-42\")\nout = m.group(1) + \"/\" + m.group(2)\n";
    assert_eq!(fstr(src), "abc/42");
}

#[test]
fn re_finditer_positions() {
    let src = "import re\nspans = []\nfor m in re.finditer(r\"\\w+\", \"aa bb\"):\n    spans.append(m.start())\n    spans.append(m.end())\n";
    let got: Vec<i64> = run_locals(src)
        .iter()
        .find_map(|v| match v {
            Value::List(l) => {
                Some(l.borrow().iter().map(|x| match x { Value::Int(i) => *i, _ => -1 }).collect())
            }
            _ => None,
        })
        .expect("a list local");
    assert_eq!(got, vec![0, 2, 3, 5]);
}

#[test]
fn re_match_is_cut() {
    let err = run_err("import re\nre.match(r\"x\", \"x\")\n");
    assert!(err.message.contains("re.match is not supported"), "got: {}", err.message);
}

#[test]
fn re_backreference_rejected() {
    let err = run_err("import re\nre.search(r\"(a)\\1\", \"aa\")\n");
    assert!(err.message.contains("backreference") || err.message.contains("linear-time"),
        "got: {}", err.message);
}

#[test]
fn proc_rejects_bare_string() {
    let err = run_err("import proc\nproc.run(\"echo hi\")\n");
    assert!(err.message.contains("list of separate string"), "got: {}", err.message);
}

#[test]
fn proc_runs_and_captures() {
    // quiet=true suppresses the live tee; the capture happens either way. The
    // captured streams are octets, so decoding is explicit.
    let src = "import proc\nr = proc.run([\"echo\", \"hi\"], quiet=true)\nout = r.returncode.to_str() + \":\" + r.stdout.strip().to_str()\n";
    assert_eq!(fstr(src), "0:hi");
}

#[test]
fn proc_raises_on_nonzero_exit_by_default() {
    let err = run_err("import proc\nproc.run([\"sh\", \"-c\", \"exit 4\"], quiet=true)\n");
    assert!(err.message.contains("command failed"), "got: {}", err.message);
}

#[test]
fn proc_check_false_allows_nonzero_exit() {
    let src = "import proc\nr = proc.run([\"sh\", \"-c\", \"exit 4\"], check=false, quiet=true)\nout = r.returncode.to_str() + \":\" + r.ok.to_str()\n";
    assert_eq!(fstr(src), "4:false");
}

#[test]
fn proc_rejects_cpython_capture_kwargs() {
    let err = run_err(
        "import proc\nproc.run([\"echo\", \"hi\"], capture_output=true)\n",
    );
    assert!(err.message.contains("always captures"), "got: {}", err.message);
}

#[test]
fn time_monotonic_never_decreases() {
    let src = "import time\na = time.monotonic()\nb = time.monotonic()\nout = b >= a\n";
    assert!(matches!(eval_last(src), Value::Bool(true)));
}

#[test]
fn exception_in_callback_does_not_leak_jobs() {
    // Each caught exception abandons a map job; without unwinding the job
    // stacks those entries would accumulate for the life of the VM.
    let src = r#"
def boom(x):
    raise ValueError("x")

i = 0
while i < 200:
    try:
        [1].map(boom)
    except ValueError:
        pass
    i = i + 1
out = [1, 2].map(x => x * 3).to_str()
"#;
    assert_eq!(fstr(src), "[3, 6]");
}

#[test]
fn enclosing_job_survives_inner_handler() {
    // The outer map is in flight while an inner try/except unwinds: only jobs
    // started *inside* the block may be discarded, never an enclosing one.
    let src = r#"
def risky(x):
    try:
        if x == 2:
            raise ValueError("inner")
        return x * 10
    except ValueError:
        return -1

out = [1, 2, 3].map(risky).to_str()
"#;
    assert_eq!(fstr(src), "[10, -1, 30]");
}

#[test]
fn exception_escapes_nested_jobs_and_vm_recovers() {
    let src = r#"
def boom(x):
    raise ValueError("deep")

try:
    [[1]].map(row => row.map(boom))
except ValueError:
    pass
out = [1, 2].map(x => x + 1).to_str()
"#;
    assert_eq!(fstr(src), "[2, 3]");
}

#[test]
fn collection_protocol_natives() {
    let src = r#"
xs = [5, 3, 8, 1]
letters = ["a", "b"]
joined = letters.join("-")
out = f"{xs.sum()} {xs.min()} {xs.max()} {xs.len()} {xs.first()} {xs.last()} {xs.sorted()} {xs.take(2)} {joined}"
"#;
    assert_eq!(fstr(src), "17 1 8 4 5 1 [1, 3, 5, 8] [5, 3] a-b");
}

#[test]
fn collection_protocol_callbacks() {
    let src = r#"
xs = [5, 3, 8, 1]
a = xs.sort_by(x => -x)
b = xs.max_by(x => -x)
c = xs.find(x => x > 4)
d = xs.count(x => x > 2)
e = xs.reduce(0, (acc, x) => acc + x)
g = xs.partition(x => x > 4)
out = f"{a} {b} {c} {d} {e} {g}"
"#;
    assert_eq!(fstr(src), "[8, 5, 3, 1] 1 5 3 17 ([5, 8], [3, 1])");
}

#[test]
fn group_by_buckets_into_a_dict() {
    let src = r#"
out = [1, 2, 3, 4, 5].group_by(x => x % 2).to_str()
"#;
    assert_eq!(fstr(src), "{1: [1, 3, 5], 0: [2, 4]}");
}

#[test]
fn seq_ops_preserve_the_receiver_type() {
    // Selecting and reordering keep the shape; reshaping returns a list.
    let src = r#"
t = (3, 1, 2)
d = {"a": 2, "b": 1}
sorted_d = d.sort_by((k, v) => v)
flat = t.flat_map(x => [x])
out = f"{t.sorted()} {t.take(2)} {sorted_d} {flat}"
"#;
    assert_eq!(fstr(src), "(1, 2, 3) (3, 1) {'b': 1, 'a': 2} [3, 1, 2]");
}

// --- A callback destructures its element the way `for` does -------------------

/// The protocol's own pair-makers — `enumerate`, `zip`, a dict's `to_list()`,
/// `group_by`, a generator of tuples — feed its callbacks. Every chain here has
/// two or three steps, so the destructuring happens in fused stages as well as
/// in the terminal.
#[test]
fn multi_parameter_callbacks_destructure_in_fused_chains() {
    let src = r#"
def gen():
    yield (1, 2)
    yield (3, 4)

xs = ["a", "b", "c"]
ns = [1, 2, 3]
d = {"x": 1, "y": 2, "z": 3}
orders = [{"r": "eu", "t": 3}, {"r": "us", "t": 5}, {"r": "eu", "t": 2}]
a = xs.enumerate().map((i, s) => s * (i + 1)).filter(s => s != "bb")
b = xs.zip(ns).filter((s, n) => n > 1).map((s, n) => s * n)
c = d.to_list().filter((k, v) => v != 2).map((k, v) => (v, k)).filter((v, k) => v < 3)
g = orders.group_by(o => o["r"]).to_list().map((r, rows) => (r, rows.len())).filter((r, n) => n > 1)
h = gen().map((p, q) => p * q)
out = f"{a} {b} {c} {g} {h}"
"#;
    assert_eq!(fstr(src), "['a', 'ccc'] ['bb', 'ccc'] [(1, 'x')] [('eu', 2)] [2, 12]");
}

/// Every step that takes a callback, as the terminal of a fused chain over
/// `enumerate` output — `reduce` counting its parameters after the accumulator.
#[test]
fn every_callback_terminal_destructures() {
    let src = r#"
e = [5, 3, 8].enumerate()
a = e.filter((i, x) => x > 3).any((i, x) => i == 2)
b = e.map((i, x) => (x, i)).all((x, i) => x > i)
c = e.filter((i, x) => i > 0).count((i, x) => x > 4)
f = e.filter((i, x) => x > 3).flat_map((i, x) => [i, x])
g = e.map((i, x) => (i, x * 2)).reduce(0, (acc, i, x) => acc + i * x)
h = e.filter((i, x) => x != 3).sort_by((i, x) => -x)
j = e.map((i, x) => (x, i)).find((x, i) => i == 1)
k = e.filter((i, x) => true).min_by((i, x) => x)
m = e.filter((i, x) => true).max_by((i, x) => x)
n = e.filter((i, x) => true).partition((i, x) => x > 4)
p = e.filter((i, x) => true).group_by((i, x) => x % 2)
q = e.filter((i, x) => true).unique_by((i, x) => x % 2)
r = e.take_while((i, x) => x > 4)
s = e.drop_while((i, x) => x > 4)
out = f"{a} {b} {c} {f} {g} {h} {j} {k} {m} {n} {p} {q} {r} {s}"
"#;
    assert_eq!(
        fstr(src),
        "true true 1 [0, 5, 2, 8] 38 [(2, 8), (0, 5)] (3, 1) (1, 3) (2, 8) \
         ([(0, 5), (2, 8)], [(1, 3)]) {1: [(0, 5), (1, 3)], 0: [(2, 8)]} \
         [(0, 5), (2, 8)] [(0, 5)] [(1, 3), (2, 8)]"
    );
}

/// One parameter takes the element whole, on every shape — a dict included,
/// where it used to be an arity error because the pair was always spread.
#[test]
fn a_one_parameter_callback_takes_the_element_whole_on_every_shape() {
    let src = r#"
d = {"a": 1, "b": 2}
a = [(1, 2), (3, 4)].map(p => p[0] + p[1])
b = ((1, 2), (3, 4)).filter(p => p[0] > 1)
c = d.map(p => (p[0], p[1] * 10))
e = d.filter(p => p[1] > 1)
f = d.reduce(0, (acc, p) => acc + p[1])
g = d.filter(p => true).map(p => p).count(p => p[1] > 0)
h = range(3).map(x => x * 2).filter(x => x > 0)
out = f"{a} {b} {c} {e} {f} {g} {h}"
"#;
    assert_eq!(fstr(src), "[3, 7] ((3, 4),) {'a': 10, 'b': 20} {'b': 2} 3 2 [2, 4]");
}

/// A dict receiver is an instance of the rule, not a special case: every
/// spelling it already had reads the same, fused or not.
#[test]
fn dict_callbacks_are_an_instance_of_the_rule() {
    let src = r#"
d = {"a": 1, "b": 2, "c": 3}
a = d.filter((k, v) => v > 1)
b = d.map((k, v) => (k, v * 10))
c = d.flat_map((k, v) => [k])
e = d.reduce([], (acc, k, v) => acc + [v])
f = d.filter((k, v) => v != 2).map((k, v) => (k + k, v))
g = d.sort_by((k, v) => -v).find((k, v) => v < 3)
out = f"{a} {b} {c} {e} {f} {g}"
"#;
    assert_eq!(
        fstr(src),
        "{'b': 2, 'c': 3} {'a': 10, 'b': 20, 'c': 30} ['a', 'b', 'c'] [1, 2, 3] \
         {'aa': 1, 'cc': 3} ('b', 2)"
    );
}

/// What counts is the positional parameters without a default. Defaults,
/// `*args` and `**kwargs` do not count, a bound method's `self` does not, and
/// `reduce` counts only the ones after its accumulator. A native callable
/// declares nothing to count, so it is handed a dict's pair as two arguments
/// and any other element whole.
#[test]
fn which_parameters_count_toward_destructuring() {
    let src = r#"
def scaled(x, by=10):
    return x * by

def label(k, v, sep="="):
    return k + sep + v.to_str()

def arity(*args):
    return len(args)

def collect(acc, *rest):
    return acc + [len(rest)]

def kw(p, **opts):
    return p

def fold(acc, k, v, extra=0):
    return acc + v + extra

class Shelf:
    def __init__(self):
        self.n = 0

    def pair(self, k, v):
        return k

    def whole(self, p):
        return p

s = Shelf()
d = {"a": 1, "b": 2}
a = [1, 2].map(scaled)
b = d.to_list().map(label)
c = d.to_list().map(arity)
e = d.reduce([], collect)
f = [(1, 2)].map(kw)
g = d.reduce(0, fold)
h = d.to_list().map(s.pair)
i = d.map(s.whole)
j = {3: 1, 0: 0}.count(max)
k = [(1, 2), (3, 4, 5)].map(len)
out = f"{a} {b} {c} {e} {f} {g} {h} {i} {j} {k}"
"#;
    assert_eq!(
        fstr(src),
        "[10, 20] ['a=1', 'b=2'] [1, 1] [1, 1] [(1, 2)] 3 ['a', 'b'] {'a': 1, 'b': 2} 1 [2, 3]"
    );
}

/// A length mismatch is `for`'s `ValueError`, word for word — not the
/// "missing required argument" of a callback bound to the wrong number of
/// values. An element `for` cannot unpack at all fails the same way too.
#[test]
fn a_destructuring_mismatch_is_fors_error() {
    let cases = [
        ("x = [(1, 2)].map((a, b, c) => a)\n", "for a, b, c in [(1, 2)]:\n    pass\n"),
        ("x = [(1, 2, 3)].map((a, b) => a)\n", "for a, b in [(1, 2, 3)]:\n    pass\n"),
        (
            "x = [(1, 2)].filter(p => true).map((a, b, c) => a).first()\n",
            "for a, b, c in [(1, 2)]:\n    pass\n",
        ),
        ("x = [1].map((a, b) => a)\n", "for a, b in [1]:\n    pass\n"),
        ("x = {1: 2}.reduce(0, (acc, a, b, c) => acc)\n", "for a, b, c in {1: 2}:\n    pass\n"),
    ];
    for (chain, stmt) in cases {
        let got = run_err(chain);
        let want = run_err(stmt);
        assert_eq!((got.class, &*got.message), (want.class, &*want.message), "{chain}");
    }
    // Uncaught, it reaches the top level named by its class.
    let e = run_err("x = [(1, 2)].map((a, b, c) => a)\n");
    assert_eq!(&*e.message, "ValueError: not enough values to unpack (expected 3, got 2)");
}

/// Reported where the step is written. By the second element the VM's own
/// position is the `return` of the callback that ran for the first, which is
/// on another line — in a fused stage as much as in a terminal.
#[test]
fn a_destructuring_mismatch_names_the_step() {
    let head = "def first(a, b):\n    return a\n\nxs = [(1, 2), (3,)]\n";
    for step in ["y = xs.map(first)\n", "y = xs.map(first).filter(x => true)\n"] {
        let e = run_err(&format!("{head}{step}"));
        assert_eq!(
            (e.line, &*e.message),
            (5, "ValueError: not enough values to unpack (expected 2, got 1)"),
            "{step}"
        );
    }
}

#[test]
fn find_and_any_short_circuit() {
    // The predicate must stop being called once the answer is settled.
    let src = r#"
seen = []
def watch(x):
    seen.append(x)
    return x > 1

hit = [1, 2, 3, 4].find(watch)
out = f"{hit} {seen}"
"#;
    assert_eq!(fstr(src), "2 [1, 2]");
}

#[test]
fn protocol_names_do_not_steal_string_methods() {
    // `find` exists on both str and collections; each keeps its own meaning,
    // chosen by the receiver's type. `join` used to be the second such name
    // and is not any more: it lives on the collection alone, so a `str`
    // receiver is the cut message rather than the other half of a pair.
    let src = r#"
letters = ["a", "b"]
a = "abcb".find("b")
c = letters.join("-")
d = [1, 2, 3].find(x => x > 1)
out = f"{a} {c} {d}"
"#;
    assert_eq!(fstr(src), "1 a-b 2");
}

/// The builtin/collection-method line: six builtins are cut, `min`/`max` are
/// narrowed to their variadic scalar form, and `len` is the one exception.
#[test]
fn the_six_duplicate_builtins_are_cut_naming_their_methods() {
    for (name, call, want) in [
        ("sum", "r = sum([1, 2])\n", "`sum` is not defined in Oro"),
        ("sorted", "r = sorted([2, 1])\n", "`sorted` is not defined in Oro"),
        ("any", "r = any([true])\n", "`any` is not defined in Oro"),
        ("all", "r = all([true])\n", "`all` is not defined in Oro"),
        ("enumerate", "r = enumerate([1])\n", "`enumerate` is not defined in Oro"),
        ("zip", "r = zip([1], [2])\n", "`zip` is not defined in Oro"),
    ] {
        let e = run_err(call);
        assert!(e.message.contains(want), "{name}: got {}", e.message);
        // The replacement is named, which is the whole convention.
        assert!(e.message.contains("collection method"), "{name}: got {}", e.message);
    }
}

/// `min(a, b)` is the half that has no chain spelling, so it stays; `min(xs)`
/// is the half that duplicates `xs.min()`, so it goes.
#[test]
fn min_and_max_keep_only_the_variadic_scalar_form() {
    assert_eq!(int(&eval("r = min(3, 1)\n")), 1);
    assert_eq!(int(&eval("r = max(3, 1)\n")), 3);
    assert_eq!(int(&eval("r = min(5, 2, 9)\n")), 2);
    for call in ["r = min([3, 1])\n", "r = max([3, 1])\n"] {
        let e = run_err(call);
        assert!(e.message.contains("two or more values"), "got: {}", e.message);
        assert!(e.message.contains(".min()") || e.message.contains(".max()"),
            "the replacement must be named: {}", e.message);
    }
    // A receiver of instances takes the VM's frame-driven path, which has to
    // narrow the same way or the two arities disagree about what they accept.
    let src = "class V:\n    def __init__(self, n):\n        self.n = n\n\n    def __lt__(self, o):\n        return self.n < o.n\n\nr = min([V(2), V(1)])\n";
    let e = run_err(src);
    assert!(e.message.contains("two or more values"), "got: {}", e.message);
}

/// `len` is the single exception, and both spellings still work.
#[test]
fn len_survives_on_both_sides_of_the_line() {
    assert_eq!(int(&eval("r = len(\"abc\")\n")), 3);
    assert_eq!(int(&eval("r = len(b\"abc\")\n")), 3);
    assert_eq!(int(&eval("r = [1, 2].len()\n")), 2);
    assert_eq!(int(&eval("r = len([1, 2])\n")), 2);
    // On a `str` the method form is not the answer, and says which is.
    let e = run_err("r = \"abc\".len()\n");
    assert!(e.message.contains("write `len(s)`"), "got: {}", e.message);
}

/// The keywords moved with `sorted`, and they had to: `xs.sorted(reverse=true)`
/// is a *stable* descending sort and `xs.sorted().reversed()` is not.
#[test]
fn sorted_keeps_key_and_reverse_on_the_method() {
    assert_eq!(eval("r = [3, 1, 2].sorted(reverse=true)\n").repr(), "[3, 2, 1]");
    assert_eq!(eval("r = (3, 1, 2).sorted(reverse=true)\n").repr(), "(3, 2, 1)");
    let src = "def snd(p):\n    return p[1]\n\nties = [(\"a\", 2), (\"b\", 1), (\"c\", 2), (\"d\", 1)]\nr = ties.sorted(key=snd, reverse=true)\n";
    assert_eq!(
        eval_var(src, "r").repr(),
        "[('a', 2), ('c', 2), ('b', 1), ('d', 1)]",
        "reverse= must not disturb ties"
    );
    let flipped = "def snd(p):\n    return p[1]\n\nties = [(\"a\", 2), (\"b\", 1), (\"c\", 2), (\"d\", 1)]\nr = ties.sort_by(snd).reversed()\n";
    assert_eq!(
        eval_var(flipped, "r").repr(),
        "[('c', 2), ('a', 2), ('d', 1), ('b', 1)]",
        "and `.reversed()` does disturb them, which is why the keyword moved"
    );
}

/// `str` and `bytes` are outside the collection protocol, and the cut makes
/// that reachable from a call that used to work as a builtin.
#[test]
fn a_str_is_not_a_collection_and_the_message_names_the_bridge() {
    let e = run_err("r = \"ba\".sorted()\n");
    assert!(e.message.contains("not a collection in Oro"), "got: {}", e.message);
    assert!(e.message.contains("to_list()"), "got: {}", e.message);
    let e = run_err("r = b\"ba\".min()\n");
    assert!(e.message.contains("not a collection in Oro"), "got: {}", e.message);
    // And the bridge works.
    assert_eq!(eval("r = \"ba\".to_list().sorted()\n").repr(), "['a', 'b']");
}

/// `str.join`/`bytes.join` are cut, and say so. Both halves were byte-for-byte
/// the same operation; the one that survives is the one that ends a chain.
#[test]
fn str_and_bytes_join_are_cut_naming_the_collection_form() {
    let e = run_err("r = \", \".join([\"a\", \"b\"])\n");
    assert!(e.message.contains("`str.join` is not in Oro"), "got: {}", e.message);
    assert!(e.message.contains("xs.join(sep)"), "got: {}", e.message);
    let e = run_err("r = b\",\".join([b\"a\"])\n");
    assert!(e.message.contains("`bytes.join` is not in Oro"), "got: {}", e.message);
    // One separator, and only one: the extra argument used to be ignored.
    let e = run_err("r = [\"a\"].join(\"-\", 2)\n");
    assert!(e.message.contains("join() takes 1 argument"), "got: {}", e.message);
}

#[test]
fn lambda_in_fstring_is_rejected_clearly() {
    // f-string fields are parsed at codegen time, so the symbol pass never
    // assigns the lambda a scope. That must be a clear error, not an internal one.
    let e = compile_err("out = f\"{[1].map(x => x)}\"\n");
    assert!(e.message.contains("cannot appear inside an f-string"), "got: {}", e.message);
}

// --- json: the embedded-Oro-stdlib module (src/vm/stdlib.rs) --------------

#[test]
fn json_is_an_embedded_stdlib_module_not_a_rust_builtin() {
    // Unlike `os`/`sys` (native `modules::build`), `json` resolves through
    // the embedded-Oro-stdlib branch of `import_module` — it is a real
    // module value all the same.
    assert!(matches!(eval("import json\nr = json\n"), Value::Module(_)));
}

#[test]
fn json_parse_distinguishes_ints_from_floats_and_decodes_literals() {
    let src = r#"
import json
a = json.parse('{"n": 3, "f": 2.5, "s": "hi", "b": true, "z": null, "xs": [1, 2, 3]}')
out = f"{a['n']} {type(a['n'])} {a['f']} {type(a['f'])} {a['s']} {a['b']} {a['z']} {a['xs']}"
"#;
    assert_eq!(fstr(src), "3 <class 'int'> 2.5 <class 'float'> hi true null [1, 2, 3]");
}

#[test]
fn json_parse_decodes_unicode_escapes_and_surrogate_pairs() {
    // A and é are plain BMP escapes; the grinning-face emoji is a UTF-16
    // surrogate pair that must combine into one non-BMP scalar (U+1F600),
    // so `len` counts it as a single character.
    let src = r#"
import json
s = json.parse('"Aé 😀"')
out = f"{s} {len(s)}"
"#;
    assert_eq!(fstr(src), "Aé 😀 4");
}

#[test]
fn json_stringify_round_trips_through_parse() {
    let src = r#"
import json
data = {"a": 1, "b": [1, 2.5, "x", true, false, null]}
out = f"{json.parse(json.stringify(data)) == data}"
"#;
    assert_eq!(fstr(src), "true");
}

#[test]
fn json_stringify_default_is_compact_with_no_incidental_whitespace() {
    let src = "import json\nout = json.stringify({\"a\": 1, \"b\": [1, 2]})\n";
    assert_eq!(fstr(src), "{\"a\":1,\"b\":[1,2]}");
}

#[test]
fn json_stringify_indent_pretty_prints() {
    let src = "import json\nout = json.stringify({\"a\": 1}, indent=2)\n";
    assert_eq!(fstr(src), "{\n  \"a\": 1\n}");
}

#[test]
fn json_stringify_rejects_non_str_keys_and_non_finite_floats() {
    let err1 = run_err("import json\njson.stringify({1: \"a\"})\n");
    assert!(err1.message.contains("TypeError"), "got: {}", err1.message);
    assert!(err1.message.contains("keys must be str"), "got: {}", err1.message);

    let err2 = run_err("import json\njson.stringify(\"nan\".to_float())\n");
    assert!(err2.message.contains("ValueError"), "got: {}", err2.message);
}

#[test]
fn json_parse_malformed_input_names_the_offset() {
    // Mirrors the README's own example shape: "unexpected 'X' at position N".
    let err = run_err("import json\njson.parse('{\"a\": 1,}')\n");
    assert!(err.message.contains("ValueError"), "got: {}", err.message);
    assert!(err.message.contains("at position 8"), "got: {}", err.message);
}

#[test]
fn chr_and_ord_round_trip() {
    let src = "out = f\"{chr(65)} {ord('A')} {chr(128512)} {ord('\u{1F600}')}\"\n";
    assert_eq!(fstr(src), "A 65 \u{1F600} 128512");
}

#[test]
fn ord_raises_type_error_and_chr_raises_value_error() {
    // CPython's exception types, not just its messages: ord() on the wrong
    // shape of value is a TypeError, chr() out of range is a ValueError.
    let e = run_err("ord(\"ab\")\n");
    assert!(e.message.contains("expected a character"), "got: {}", e.message);
    let e2 = run_err("chr(1114112)\n");
    assert!(e2.message.contains("arg not in range"), "got: {}", e2.message);
}

/// The sequence protocol on `bytes`, all of it matching CPython (the corpus
/// oracle checks the same ground). The one asymmetry with `str` is deliberate:
/// indexing yields the octet as an `int`, slicing yields `bytes`.
#[test]
fn bytes_sequence_protocol() {
    assert_eq!(int(&eval("r = len(b\"hello\")\n")), 5);
    assert_eq!(int(&eval("r = b\"hello\"[0]\n")), 104);
    assert_eq!(int(&eval("r = b\"hello\"[-1]\n")), 111);
    assert_eq!(eval("r = b\"hello\"[1:3]\n").repr(), "b'el'");
    // Slice bounds clamp rather than raise, as everywhere else.
    assert_eq!(eval("r = b\"hi\"[10:20]\n").repr(), "b''");
    assert_eq!(eval("r = b\"hi\"[::-1]\n").repr(), "b'ih'");
    assert_eq!(eval("r = b\"ab\" + b\"cd\"\n").repr(), "b'abcd'");
    assert_eq!(eval("r = b\"ab\" * 3\n").repr(), "b'ababab'");
    assert_eq!(eval("r = b\"ab\" * -1\n").repr(), "b''");
    // Iteration yields ints, so a sum over bytes is a sum of octets.
    assert_eq!(int(&eval("r = 0\nfor x in b\"abc\":\n    r = r + x\n")), 294);
}

#[test]
fn bytes_membership_ordering_and_hashing() {
    assert!(eval("r = b\"ab\" in b\"xaby\"\n").truthy());
    assert!(eval("r = b\"\" in b\"x\"\n").truthy());
    assert!(!eval("r = b\"ba\" in b\"xaby\"\n").truthy());
    assert!(eval("r = b\"abc\" < b\"abd\"\n").truthy());
    // A byte string never equals the str that would decode to it, and the two
    // are distinct dict keys.
    assert!(!eval("r = b\"abc\" == \"abc\"\n").truthy());
    assert_eq!(eval_last("d = {b\"k\": 1, \"k\": 2}\nr = len(d)\n").repr(), "2");
    assert_eq!(eval_last("d = {b\"k\": 1, \"k\": 2}\nr = d[b\"k\"]\n").repr(), "1");
    // `in` on bytes means subsequence; an int asks a different question, so it
    // is refused rather than silently answered.
    let e = run_err("r = 97 in b\"abc\"\n");
    assert!(e.message.contains("TypeError"), "got: {}", e.message);
}

#[test]
fn bytes_index_out_of_range_is_an_index_error() {
    let e = run_err("r = b\"ab\"[5]\n");
    assert!(e.message.contains("IndexError"), "got: {}", e.message);
}

/// The `bytes` method set — the same names `str` carries, each matching
/// CPython byte for byte (the corpus checks the same ground against the
/// oracle). Case folding is ASCII-only, which is why `\xff` survives `lower`.
#[test]
fn bytes_methods_mirror_the_str_set() {
    assert_eq!(eval("r = b\"AbC\\xff\".lower()\n").repr(), "b'abc\\xff'");
    assert_eq!(eval("r = b\"AbC\".upper()\n").repr(), "b'ABC'");
    // Vertical tab and form feed count as whitespace, as they do in CPython.
    assert_eq!(eval("r = b\" \\x0b a b \\t\\n\".strip()\n").repr(), "b'a b'");
    assert_eq!(eval("r = b\"  ab  \".strip(side=\"left\")\n").repr(), "b'ab  '");
    assert_eq!(eval("r = b\"  ab  \".strip(side=\"right\")\n").repr(), "b'  ab'");
    assert_eq!(eval("r = b\"a,b,,c\".split(b\",\")\n").repr(), "[b'a', b'b', b'', b'c']");
    assert_eq!(eval("r = b\"a,b,c\".split(b\",\", 1)\n").repr(), "[b'a', b'b,c']");
    assert_eq!(eval("r = b\"a b\\x0bc\".split()\n").repr(), "[b'a', b'b', b'c']");
    assert_eq!(eval("r = [b\"a\", b\"b\"].join(b\"-\")\n").repr(), "b'a-b'");
    assert_eq!(int(&eval("r = b\"abc\".find(b\"b\")\n")), 1);
    assert_eq!(int(&eval("r = b\"abc\".find(b\"z\")\n")), -1);
    // The empty needle is found at 0, as it is for `str`.
    assert_eq!(int(&eval("r = b\"abc\".find(b\"\")\n")), 0);
    assert_eq!(eval("r = b\"abc\".replace(b\"b\", b\"XY\")\n").repr(), "b'aXYc'");
    assert!(eval("r = b\"abc\".startswith(b\"ab\")\n").truthy());
    assert!(eval("r = b\"abc\".endswith(b\"bc\")\n").truthy());
    // `find(sub, reverse=true)` is the whole of what `rfind` used to be.
    assert_eq!(int(&eval("r = b\"abcabc\".find(b\"bc\", reverse=true)\n")), 4);
    assert_eq!(int(&eval("r = b\"abc\".count(b\"\")\n")), 4);
    assert_eq!(eval("r = b\"a.png\".rm_suffix(b\".png\")\n").repr(), "b'a'");
    assert_eq!(eval("r = b\"a.png\".rm_suffix(b\".gif\")\n").repr(), "b'a.png'");
    assert!(eval("r = b\"12\".is_digit()\n").truthy());
    assert!(!eval("r = b\"\".is_digit()\n").truthy());
    assert_eq!(eval("r = b\"\\xff\\x00A\".hex()\n").repr(), "'ff0041'");
}

/// `bytes.scan(allowed)` — the length of the longest prefix whose every byte is
/// in `allowed`. The generic primitive `docs/stdlib-server-design.md` §5 said
/// to reach for when Oro-level parsing crossed its threshold; `std/http.oro`'s
/// three grammar checks are its first caller.
#[test]
fn bytes_scan_measures_the_prefix_inside_a_byte_class() {
    assert_eq!(int(&eval("r = b\"abc\".scan(b\"abc\")\n")), 3);
    assert_eq!(int(&eval("r = b\"abc\".scan(b\"ab\")\n")), 2);
    assert_eq!(int(&eval("r = b\"abc\".scan(b\"xyz\")\n")), 0);
    // An empty set admits nothing; an empty subject has no prefix to measure.
    assert_eq!(int(&eval("r = b\"abc\".scan(b\"\")\n")), 0);
    assert_eq!(int(&eval("r = b\"\".scan(b\"abc\")\n")), 0);
    // The whole octet range, high bytes and NUL included — this is a set of
    // numbers, not of characters.
    assert_eq!(int(&eval("r = b\"\\xff\\x00\\x80\".scan(b\"\\x00\\x80\\xff\")\n")), 3);
    assert_eq!(int(&eval("r = b\"\\xff\\x00\".scan(b\"\\xff\")\n")), 1);
    // A repeated member is still one member.
    assert_eq!(int(&eval("r = b\"aaa\".scan(b\"aaaa\")\n")), 3);
    // `== len(b)` is "every byte is in the class", which is the question a
    // grammar asks; anything less is where the first offending byte is.
    assert_eq!(int(&eval("r = b\"Content Type\".scan(b\"ContenTyp\")\n")), 7);
    let e = run_err("r = b\"a\".scan(\"a\")\n");
    assert!(e.message.contains("scan() argument must be bytes, not 'str'"), "got: {}", e.message);
    let e = run_err("r = \"a\".scan(b\"a\")\n");
    assert!(e.message.contains("has no attribute 'scan'"), "got: {}", e.message);
}

/// The `str` surface after the strip/find fold: one `strip` with a `side=`
/// keyword, one `find` with a `reverse=` keyword, and the four `is_*`
/// predicates. Every CPython-shaped answer here is oracled by
/// `corpus/core/38_str_bytes_optional_args.oro` as well; these pin the
/// Oro-only spellings, which CPython cannot check.
#[test]
fn str_strip_takes_a_side_and_find_takes_a_reverse() {
    assert_eq!(eval("r = \"  ab  \".strip()\n").repr(), "'ab'");
    assert_eq!(eval("r = \"  ab  \".strip(side=\"left\")\n").repr(), "'ab  '");
    assert_eq!(eval("r = \"  ab  \".strip(side=\"right\")\n").repr(), "'  ab'");
    assert_eq!(eval("r = \"  ab  \".strip(side=\"both\")\n").repr(), "'ab'");
    // `chars` is a cut set, and it composes with `side` rather than replacing it.
    assert_eq!(eval("r = \"xyaxy\".strip(\"xy\")\n").repr(), "'a'");
    assert_eq!(eval("r = \"xyaxy\".strip(\"xy\", side=\"left\")\n").repr(), "'axy'");
    assert_eq!(eval("r = \"xyaxy\".strip(\"xy\", side=\"right\")\n").repr(), "'xya'");
    // Anything but the three is a ValueError that names the three.
    let e = run_err("r = \"x\".strip(side=\"middle\")\n");
    assert!(e.message.contains("ValueError"), "got: {}", e.message);
    assert!(e.message.contains("\"both\", \"left\" or \"right\""), "got: {}", e.message);

    assert_eq!(int(&eval("r = \"abcabc\".find(\"bc\")\n")), 1);
    assert_eq!(int(&eval("r = \"abcabc\".find(\"bc\", reverse=true)\n")), 4);
    // The positional window still applies, from whichever end.
    assert_eq!(int(&eval("r = \"abcabc\".find(\"bc\", 0, 4, reverse=true)\n")), 1);
    assert_eq!(int(&eval("r = \"abcabc\".find(\"zz\", reverse=true)\n")), -1);
    assert_eq!(int(&eval("r = \"abc\".find(\"\", reverse=true)\n")), 3);
}

/// `split(sep, maxsplit, side="right")` — the capability `rsplit` had. Every
/// answer here is CPython's `rsplit(sep, maxsplit)`; the corpus twin
/// `divergence/51_string_surface.twin.py` checks the whole matrix against it,
/// and these pin the shape of the call.
#[test]
fn split_takes_a_side() {
    assert_eq!(eval("r = \"a.b.c\".split(\".\", 1)\n").repr(), "['a', 'b.c']");
    assert_eq!(eval("r = \"a.b.c\".split(\".\", 1, side=\"left\")\n").repr(), "['a', 'b.c']");
    assert_eq!(eval("r = \"a.b.c\".split(\".\", 1, side=\"right\")\n").repr(), "['a.b', 'c']");
    // Unlimited splits: the two ends agree, and `side` is a no-op rather than
    // an error. See `split_side` for why an error could not be honest here.
    assert_eq!(eval("r = \"a.b.c\".split(\".\", side=\"right\")\n").repr(), "['a', 'b', 'c']");
    // Whitespace splitting keeps the remainder verbatim at the far end.
    assert_eq!(eval("r = \" a  b  c \".split(null, 1, side=\"right\")\n").repr(), "[' a  b', 'c']");
    assert_eq!(eval("r = \" a  b \".split(null, 0, side=\"right\")\n").repr(), "[' a  b']");
    assert_eq!(eval("r = \"   \".split(null, 1, side=\"right\")\n").repr(), "[]");
    // Empty fields survive from either end.
    assert_eq!(eval("r = \"a..b\".split(\".\", 1, side=\"right\")\n").repr(), "['a.', 'b']");
    assert_eq!(eval("r = \".a.\".split(\".\", 1, side=\"right\")\n").repr(), "['.a', '']");
    // And on bytes, the same sixteen names meaning the same sixteen things.
    assert_eq!(eval("r = b\"a.b.c\".split(b\".\", 1, side=\"right\")\n").repr(), "[b'a.b', b'c']");
    assert_eq!(
        eval("r = b\" a  b  c \".split(null, 1, side=\"right\")\n").repr(),
        "[b' a  b', b'c']"
    );

    // A split has no "both" end, so the error names two values, not three.
    let e = run_err("r = \"x\".split(\".\", 1, side=\"both\")\n");
    assert!(e.message.contains("ValueError"), "got: {}", e.message);
    assert!(e.message.contains("\"left\" or \"right\""), "got: {}", e.message);
    let e = run_err("r = \"x\".split(\".\", 1, side=1)\n");
    assert!(e.message.contains("TypeError"), "got: {}", e.message);
    let e = run_err("r = \"x\".split(\".\", bogus=1)\n");
    assert!(e.message.contains("unexpected keyword argument"), "got: {}", e.message);
}

/// `rm_prefix`/`rm_suffix` exist because `strip(chars)` is a character *set*
/// and gets mistaken for suffix removal. The two lines here are the footgun and
/// its answer, side by side.
#[test]
fn rm_prefix_and_rm_suffix_are_literal() {
    assert_eq!(eval("r = \"ping.png\".strip(\".png\", side=\"right\")\n").repr(), "'pi'");
    assert_eq!(eval("r = \"ping.png\".rm_suffix(\".png\")\n").repr(), "'ping'");
    assert_eq!(eval("r = \"ping.png\".rm_suffix(\".gif\")\n").repr(), "'ping.png'");
    assert_eq!(eval("r = \"ping.png\".rm_prefix(\"ping\")\n").repr(), "'.png'");
    assert_eq!(eval("r = \"abc\".rm_prefix(\"\")\n").repr(), "'abc'");
}

/// `count` and the four `is_*` predicates, including the empty-sequence rule
/// (`false` for all four) that CPython also has and everyone forgets.
///
/// `count` takes `find`'s window, off the same helper. The whole matrix is
/// oracled in `corpus/core/38_str_bytes_optional_args.oro`; these are the
/// three cases worth naming — the window applies, a negative bound counts from
/// the end, and a start past the end is a negative-width window that not even
/// the empty needle matches in.
#[test]
fn count_and_the_is_predicates() {
    assert_eq!(int(&eval("r = \"abcabc\".count(\"bc\")\n")), 2);
    assert_eq!(int(&eval("r = \"aaa\".count(\"aa\")\n")), 1);
    assert_eq!(int(&eval("r = \"abc\".count(\"\")\n")), 4);
    assert_eq!(int(&eval("r = \"abcabc\".count(\"bc\", 2)\n")), 1);
    assert_eq!(int(&eval("r = \"abcabc\".count(\"bc\", 0, 4)\n")), 1);
    assert_eq!(int(&eval("r = \"abcabc\".count(\"bc\", -3)\n")), 1);
    assert_eq!(int(&eval("r = \"abc\".count(\"\", 1, 2)\n")), 2);
    assert_eq!(int(&eval("r = \"abc\".count(\"\", 3)\n")), 1);
    assert_eq!(int(&eval("r = \"abc\".count(\"\", 99)\n")), 0);
    // Character indices, not byte offsets — the same rule `find` follows.
    assert_eq!(int(&eval("r = \"ha\u{e9}\u{e9}ha\".count(\"\u{e9}\", 3)\n")), 1);
    assert_eq!(int(&eval("r = b\"abcabc\".count(b\"bc\", 2)\n")), 1);
    assert_eq!(int(&eval("r = b\"abc\".count(b\"\", 99)\n")), 0);
    assert!(eval("r = \"123\".is_digit()\n").truthy());
    assert!(!eval("r = \"12a\".is_digit()\n").truthy());
    assert!(eval("r = \"caf\u{e9}\".is_alpha()\n").truthy());
    assert!(eval("r = \"a1\".is_alnum()\n").truthy());
    assert!(eval("r = \" \\t\\n\".is_space()\n").truthy());
    for m in ["is_digit", "is_alpha", "is_alnum", "is_space"] {
        assert!(!eval(&format!("r = \"\".{m}()\n")).truthy(), "empty {m}");
        assert!(!eval(&format!("r = b\"\".{m}()\n")).truthy(), "empty bytes {m}");
    }
}

/// A removed method names its replacement. Answering "no such attribute" would
/// leave the reader to guess whether it moved or never existed.
#[test]
fn removed_string_methods_name_their_replacement() {
    let cases = [
        ("\"x\".lstrip()", "strip(side=\"left\")"),
        ("\"x\".rstrip()", "strip(side=\"right\")"),
        ("\"x\".rsplit(\",\")", "split(sep, maxsplit, side=\"right\")"),
        ("\"x\".rfind(\"a\")", "find(sub, reverse=true)"),
        ("\"x\".zfill(3)", "f\"{n:05d}\""),
        ("\"x\".index(\"a\")", "find(sub)"),
        ("b\"x\".lstrip()", "strip(side=\"left\")"),
        ("b\"x\".zfill(3)", "f\"{n:05d}\""),
        ("\"x\".removeprefix(\"a\")", "`rm_prefix`"),
        ("\"x\".removesuffix(\"a\")", "`rm_suffix`"),
        ("\"x\".isdigit()", "`is_digit`"),
        ("\"x\".isspace()", "`is_space`"),
    ];
    for (src, want) in cases {
        let e = run_err(&format!("r = {src}\n"));
        assert!(e.message.contains("AttributeError"), "{src}: {}", e.message);
        assert!(e.message.contains(want), "{src} should name {want}, got: {}", e.message);
    }
}

/// Only `strip` and `find` take a keyword, and only their own.
#[test]
fn other_methods_refuse_keywords() {
    let e = run_err("r = \"x\".upper(side=\"left\")\n");
    assert!(e.message.contains("takes no keyword arguments"), "got: {}", e.message);
    let e = run_err("r = \"x\".strip(bogus=1)\n");
    assert!(e.message.contains("unexpected keyword argument 'bogus'"), "got: {}", e.message);
    let e = run_err("r = \"x\".find(\"x\", bogus=1)\n");
    assert!(e.message.contains("unexpected keyword argument 'bogus'"), "got: {}", e.message);
}

/// The two conversions at the wire/program boundary. `to_bytes` cannot fail
/// (UTF-8 is the one encoding); `to_str` is strict, because silently
/// substituting replacement characters corrupts a body rather than reporting
/// it. CPython's `UnicodeDecodeError` is a `ValueError` subclass, so raising
/// `ValueError` is caught by the same `except`.
#[test]
fn bytes_and_str_convert_explicitly() {
    assert_eq!(eval("r = \"h\u{e9}llo\".to_bytes()\n").repr(), "b'h\\xc3\\xa9llo'");
    // A character is one `str` element and two octets — the whole reason these
    // are two types.
    assert_eq!(int(&eval("r = len(\"h\u{e9}llo\".to_bytes())\n")), 6);
    assert_eq!(int(&eval("r = len(\"h\u{e9}llo\")\n")), 5);
    assert_eq!(eval("r = \"h\u{e9}\".to_bytes().to_str()\n").repr(), "'h\u{e9}'");
    // Each conversion is a no-op on its own type.
    assert_eq!(eval("r = b\"ab\".to_bytes()\n").repr(), "b'ab'");

    let e = run_err("r = b\"\\xff\".to_str()\n");
    assert!(e.message.contains("ValueError"), "got: {}", e.message);
    assert!(e.message.contains("invalid byte 0xff at position 0"), "got: {}", e.message);
}

// --- the io protocol ---------------------------------------------------------

/// The whole point of the naming convention: `io.read` and `io.copy` are
/// written against `read(n)` and `write(b)` alone, so an Oro class with a
/// `read` method is a Reader on exactly the same terms as a `File`.
#[test]
fn io_read_works_on_an_oro_class_that_only_has_read() {
    let src = "import io\n\
               class Dribble:\n\
               \x20   def __init__(self, data):\n\
               \x20       self.data = data\n\
               \x20   def read(self, n):\n\
               \x20       out = self.data[0:1]\n\
               \x20       self.data = self.data[1:]\n\
               \x20       return out\n\
               r = io.read(Dribble(b\"drip\"))\n";
    assert_eq!(eval_last(src).repr(), "b'drip'");
}

/// A Reader may legally return fewer bytes than asked for without being at
/// EOF. `io.read(r, n)` is the function that hides that; `r.read(n)` is the
/// primitive that does not.
#[test]
fn io_read_with_a_count_loops_over_short_reads() {
    let src = "import io\n\
               class Dribble:\n\
               \x20   def __init__(self, data):\n\
               \x20       self.data = data\n\
               \x20   def read(self, n):\n\
               \x20       out = self.data[0:1]\n\
               \x20       self.data = self.data[1:]\n\
               \x20       return out\n\
               r = io.read(Dribble(b\"drip\"), 3)\n";
    assert_eq!(eval_last(src).repr(), "b'dri'");

    // A stream that ends short of `n` is an EOFError — the case a hand-rolled
    // read loop gets wrong when a request spans two packets.
    let e = run_err(
        "import io\nr = io.read(io.buffer(b\"ab\"), 5)\n",
    );
    assert!(e.message.contains("EOFError"), "got: {}", e.message);
    assert!(e.message.contains("stream ended after 2 bytes"), "got: {}", e.message);
}

#[test]
fn io_copy_moves_bytes_between_any_two_streams() {
    let src = "import io\n\
               dst = io.buffer()\n\
               n = io.copy(dst, io.buffer(b\"payload\"))\n\
               r = f\"{n}:{dst.bytes().to_str()}\"\n";
    assert_eq!(fstr(src), "7:payload");
}

/// `io.buffer` is a Reader and a Writer at once, and a queue between them.
#[test]
fn a_buffer_reads_what_was_written_to_it() {
    let src = "import io\n\
               b = io.buffer(b\"one \")\n\
               b.write(b\"two\")\n\
               r = b.read(4).to_str() + \"|\" + b.bytes().to_str()\n";
    assert_eq!(fstr(src), "one |two");
}

/// The underscored built-in modules are the stdlib's Rust primitives, not
/// language surface: `std/io.oro` can reach `_io` and a user program cannot.
#[test]
fn private_native_modules_resolve_only_inside_the_stdlib() {
    let e = run_err("import _io\n");
    assert!(e.message.contains("No module named '_io'"), "got: {}", e.message);
    let e = run_err("import _json\n");
    assert!(e.message.contains("No module named '_json'"), "got: {}", e.message);
    // ...and the module in front of it is reachable, so the rule is hiding the
    // primitive rather than the feature.
    assert_eq!(
        eval_last("import json\nr = json.stringify(json.parse(\"[1,2]\"))\n").repr(),
        "'[1,2]'"
    );
}

/// `bytes` is a sequence of ints that, until this cast, could not be *built*
/// from ints: `chr(200).to_bytes()` is the two octets of U+00C8 in UTF-8, not
/// one octet 200, so `b"\xc8"` could not be produced from a computed value at
/// all. It is a building block for binary protocols, not a convenience.
#[test]
fn a_list_of_ints_converts_to_bytes() {
    assert_eq!(eval("r = [97, 98, 99].to_bytes()\n").repr(), "b'abc'");
    assert_eq!(eval("r = [].to_bytes()\n").repr(), "b''");
    assert_eq!(eval("r = (0, 200, 255).to_bytes()\n").repr(), "b'\\x00\\xc8\\xff'");
    // A bool is an int everywhere else in the language, so it is one here.
    assert_eq!(eval("r = [true, 98].to_bytes()\n").repr(), "b'\\x01b'");
    // The whole point is the octet a `str` cannot reach.
    assert_eq!(int(&eval("r = len([200].to_bytes())\n")), 1);
    assert_eq!(int(&eval("r = len(f\"{200:c}\".to_bytes())\n")), 2);

    for src in ["r = [256].to_bytes()\n", "r = [-1].to_bytes()\n", "r = [b\"a\"].to_bytes()\n"] {
        let e = run_err(src);
        assert!(e.message.contains("ValueError"), "{src}: got {}", e.message);
    }
}

// --- The Task split ----------------------------------------------------------

/// Two `Task`s coexist in one `Vm` — the whole point of the M2 refactor.
///
/// Nothing in the language spawns a task yet, so this is the *structural*
/// claim under test: a live stack segment can be parked in an ordinary local
/// (exactly where a scheduler's ready queue would hold it), a different
/// execution can run to completion in the same VM meanwhile, and the parked
/// segment can be made current again and resumed to the right answer — while
/// everything process-wide stays shared between the two.
#[test]
fn two_tasks_coexist_in_one_vm() {
    let mut vm = Vm::new(Vec::new());

    // Task A: a live frame stack, parked before executing a single instruction.
    vm.push_module_frame(compile_module("import json\na = 6\nb = a * 7\n"));
    assert_eq!(vm.task.frames.len(), 1);
    let parked = std::mem::replace(&mut vm.task, Task::new());

    // Task B runs to completion in the same VM, with its own everything.
    assert!(vm.task.frames.is_empty(), "a fresh task starts with no stack segment");
    vm.push_module_frame(compile_module("import json\nc = 40 + 2\n"));
    vm.run_loop().expect("task B runs");
    assert_eq!(int(&vm.task.last_locals[1]), 42);

    // A's segment survived B's entire execution untouched.
    assert_eq!(parked.frames.len(), 1);
    assert_eq!(parked.frames[0].pc, 0);
    assert!(parked.last_locals.is_empty());

    // Make A current again and resume it.
    let finished_b = std::mem::replace(&mut vm.task, parked);
    vm.run_loop().expect("task A resumes");
    assert_eq!(int(&vm.task.last_locals[2]), 42);

    // B's result was not clobbered by A finishing. This is the assertion that
    // fails if `last_locals` is left on `Vm`: A's outermost `return` sees an
    // empty frame stack and overwrites it.
    assert_eq!(int(&finished_b.last_locals[1]), 42);

    // Process-wide state is genuinely shared, not duplicated: `import json`
    // ran once and the second task's import was a cache hit.
    assert_eq!(vm.module_cache.len(), 1, "the module cache is per-VM, not per-task");
    assert!(!vm.frame_pool.is_empty(), "retired frames are pooled across tasks");
}

/// Per-execution state really is per-execution: a task parked mid-`finally`,
/// mid-`except` and mid-`print` keeps its own copy of each.
#[test]
fn task_state_is_not_shared_between_tasks() {
    let mut vm = Vm::new(Vec::new());
    vm.task.handling.push(Value::Int(1));
    vm.task.finally_why.push(Why::Normal);
    vm.task.line = 17;
    vm.task.col = 5;

    let parked = std::mem::replace(&mut vm.task, Task::new());
    assert!(vm.task.handling.is_empty());
    assert!(vm.task.finally_why.is_empty());
    assert_eq!((vm.task.line, vm.task.col), (0, 0));

    vm.task = parked;
    assert_eq!(vm.task.handling.len(), 1);
    assert_eq!(vm.task.finally_why.len(), 1);
    assert_eq!((vm.task.line, vm.task.col), (17, 5));
}

// --- The scheduler -----------------------------------------------------------

/// `Step::Park` cannot carry a borrow, and the compiler is what enforces it.
///
/// `src/net.rs` records the hazard this stands against: the blocking socket
/// calls hold a `RefCell` borrow of the stream's interior *across* the syscall,
/// which becomes a `BorrowMutError` **panic** — not a catchable exception — the
/// moment a second task can run while the first is suspended there. Parking
/// must release the borrow before it yields.
///
/// A `Ref<'a, T>` is not `'static`, so this bound is exactly the property that
/// makes "the borrow is released before the suspend" unexpressible-otherwise
/// rather than a rule someone has to remember. Adding a lifetime parameter to
/// `Park` or `Step` in order to smuggle a guard through fails right here.
#[test]
fn park_cannot_carry_a_borrow() {
    fn owned_only<T: 'static>() {}
    owned_only::<sched::Park>();
    owned_only::<Step>();
}

/// The scheduler is a `Vec::push` and a `mem::replace`, and it stays that way:
/// nothing on the dispatch path grew.
#[test]
fn step_stayed_small() {
    // `Step` is the return value of `Vm::step`, which runs once per
    // instruction. `Park`'s payload is boxed so the new variant costs the hot
    // path a discriminant it already had rather than widening every return.
    assert!(
        std::mem::size_of::<Step>() <= 24,
        "Step grew to {} bytes — box the payload rather than widening the value \
         `Vm::step` returns on every instruction",
        std::mem::size_of::<Step>()
    );

    // `Step` alone was never the thing the hot path returns — `Vm::step`
    // returns `Result<Step, RuntimeError>`, and the error half is just as able
    // to widen it. This test asserted only the half that had been shrunk on
    // purpose, and so said nothing when adding the source file to
    // `RuntimeError` (an `Rc<str>` beside two `usize`s) took the `Result` from
    // 48 bytes to 64: **+14% on `loop`, +6.6% across the suite**, from a change
    // that touches no instruction. That is the same wall the reverted
    // lazy-spans attempt hit, reached from the other side. Assert the whole
    // return value, which is what the dispatch loop actually moves.
    // The error half is now a `Box` (`vm::VmError`), so the pair is 24 bytes:
    // the `Result` discriminant rides in `Step`'s own spare tag values and the
    // error costs one pointer instead of forty inline bytes. It was 48 before
    // that, and adding the source file to `RuntimeError` (an `Rc<str>` beside
    // two `usize`s) had already taken it to 64 once: **+14% on `loop`, +6.6%
    // across the suite**, from a change that touches no instruction. That is
    // the same wall the reverted lazy-spans attempt hit, reached from the other
    // side. Assert the whole return value, which is what the dispatch loop
    // actually moves.
    assert!(
        std::mem::size_of::<Result<Step, super::VmError>>() <= 24,
        "Result<Step, VmError> grew to {} bytes — `Vm::step` returns one of \
         these on every instruction, through a hidden return pointer that is \
         written and read back each time. Keep the error half behind the \
         `VmError` box and the payload of any new `Step` variant boxed too, \
         rather than paying for the width once per dispatch",
        std::mem::size_of::<Result<Step, super::VmError>>()
    );

    // And the unboxed error stays small enough that boxing it is the only
    // thing standing between the hot path and a 64-byte return: a message that
    // is never appended to is a `Box<str>`, a line and a column are `u32`, and
    // the exception class is a one-byte `Exc` rather than the sixteen a
    // `&'static str` would cost. 40 bytes became 48 when the class moved onto
    // the error — the change that deleted `classify_error` — and that is paid
    // in the cold allocation only, because the `Result` the dispatch loop
    // returns holds a `Box` and is asserted above to be unmoved at 24.
    assert!(
        std::mem::size_of::<RuntimeError>() <= 48,
        "RuntimeError grew to {} bytes",
        std::mem::size_of::<RuntimeError>()
    );
    // One byte, and it must stay one byte: it rides in `RuntimeError`, in
    // `VErr`, and so in the error half of every builtin's return value.
    assert_eq!(std::mem::size_of::<crate::exc::Exc>(), 1);
}

/// Two tasks alternate, and the alternation is decided by the channel rather
/// than by luck: an unbuffered send cannot complete until a receiver is there.
#[test]
fn two_tasks_interleave_deterministically() {
    let v = eval_var(
        "\
ch = chan()
log = []
def echo():
    for x in ch:
        log.append(\"task \" + x.to_str())
t = spawn(echo)
i = 0
while i < 3:
    log.append(\"main \" + i.to_str())
    ch.send(i)
    i = i + 1
ch.close()
t.join()
r = log
",
        "r",
    );
    // main's first `send` finds nobody waiting and parks, which is what lets
    // `echo` run at all; from then on each side hands off to the other.
    assert_eq!(
        v.repr(),
        "['main 0', 'task 0', 'main 1', 'main 2', 'task 1', 'task 2']",
        "the interleaving is a property of the rendezvous, not of timing"
    );
}

/// A value goes out and comes back, through two tasks and two channels.
#[test]
fn a_channel_round_trip() {
    let v = eval_var(
        "\
req = chan()
rep = chan()
def square():
    for n in req:
        rep.send(n * n)
s = spawn(square)
out = []
for n in [2, 3, 4]:
    req.send(n)
    out.append(rep.recv())
req.close()
s.join()
r = out
",
        "r",
    );
    assert_eq!(v.repr(), "[4, 9, 16]");
}

/// A buffered channel absorbs `cap` sends without a receiver and blocks on the
/// next one; the blocked sender is released the moment a slot frees up.
#[test]
fn a_buffered_channel_blocks_only_when_full() {
    let v = eval_var(
        "\
ch = chan(2)
log = []
def fill():
    for i in [1, 2, 3]:
        ch.send(i)
        log.append(\"sent \" + i.to_str())
def nothing():
    return 0
f = spawn(fill)
# Hand the CPU to `fill` without becoming a receiver, so the buffer is what
# stops it rather than a rendezvous with main.
spawn(nothing).join()
log.append(\"main resumes\")
log.append(\"took \" + ch.recv().to_str())
log.append(\"took \" + ch.recv().to_str())
log.append(\"took \" + ch.recv().to_str())
f.join()
r = log
",
        "r",
    );
    // `fill` fills both slots, blocks on the third, and only resumes once
    // main's first `recv` frees a slot.
    assert_eq!(
        v.repr(),
        "['sent 1', 'sent 2', 'main resumes', 'took 1', 'took 2', 'took 3', 'sent 3']"
    );
}

/// §3 rule 2: a task's exception is re-raised in whoever joins it, with its
/// class intact, and joining is idempotent.
#[test]
fn a_failing_task_is_re_raised_in_the_joiner() {
    let v = eval_var(
        "\
def boom():
    raise KeyError(\"k\")
t = spawn(boom)
out = []
for _ in [1, 2]:
    try:
        t.join()
        out.append(\"no raise\")
    except KeyError as e:
        out.append(\"KeyError \" + e.to_str())
r = out
",
        "r",
    );
    // `KeyError`'s message is the repr of its argument, as CPython's is — this
    // fixture used to ratify the unquoted `KeyError k` Oro produced before.
    assert_eq!(v.repr(), "[\"KeyError 'k'\", \"KeyError 'k'\"]");
}

/// A task that fails does not touch its peers, and the program keeps running.
#[test]
fn one_task_failing_leaves_the_others_alone() {
    let v = eval_var(
        "\
def crash():
    raise ValueError(\"x\")
def ok(n):
    return n * 2
hs = [spawn(crash), spawn(ok, 5), spawn(crash), spawn(ok, 7)]
out = []
for h in hs:
    try:
        out.append(h.join())
    except ValueError:
        out.append(\"failed\")
r = out
",
        "r",
    );
    assert_eq!(v.repr(), "['failed', 10, 'failed', 14]");
}

/// `MAX_FRAMES` counts the *running task's* frames, so a runaway task exhausts
/// its own stack segment and nothing else's.
#[test]
fn the_frame_limit_is_per_task() {
    let v = eval_var(
        "\
def runaway(n):
    return runaway(n + 1)
def depth(n):
    if n == 0:
        return 0
    return 1 + depth(n - 1)
out = []
try:
    spawn(runaway, 0).join()
    out.append(\"no limit\")
except RuntimeError:
    out.append(\"limited\")
out.append(depth(2000))
r = out
",
        "r",
    );
    assert_eq!(v.repr(), "['limited', 2000]");
}

/// Two tasks cannot advance one generator. Before green threads the state was
/// only reachable by a generator that iterates itself, and the loop ended
/// silently instead of complaining.
#[test]
fn a_generator_cannot_be_driven_by_two_tasks() {
    let v = eval_var(
        "\
gate = chan()
def slow():
    n = 0
    while n < 2:
        gate.recv()
        yield n
        n = n + 1
g = slow()
def drive():
    for _ in g:
        pass
a = spawn(drive)
b = spawn(drive)
gate.send(0)
out = []
try:
    b.join()
    out.append(\"no raise\")
except ValueError as e:
    out.append(e.to_str())
gate.close()
try:
    a.join()
except ChannelClosed:
    out.append(\"owner ended at close\")
r = out
",
        "r",
    );
    assert_eq!(v.repr(), "['generator already executing', 'owner ended at close']");
}

/// A generator sent down a channel arrives as a generator — it must not be
/// drained into a list on the way, which is what the materialise path would do
/// to any other native call's generator argument.
#[test]
fn a_generator_survives_a_channel() {
    let v = eval_var(
        "\
def nums():
    yield 1
    yield 2
ch = chan(1)
def consume():
    g = ch.recv()
    out = []
    for x in g:
        out.append(x)
    return out
c = spawn(consume)
ch.send(nums())
r = c.join()
",
        "r",
    );
    assert_eq!(v.repr(), "[1, 2]");
}

/// Every task blocked with nothing able to wake anyone is a deadlock, and it is
/// reported as one rather than hanging.
#[test]
fn a_deadlock_is_reported() {
    let e = run_err("ch = chan()\nx = ch.recv()\n");
    assert!(e.message.starts_with("deadlock:"), "got {}", e.message);
    assert!(e.message.contains("recv"), "the diagnostic names what is waited on: {}", e.message);
    // A task left parked forever after main returns is the same failure.
    let e = run_err("ch = chan()\ndef stuck():\n    ch.recv()\nspawn(stuck)\n");
    assert!(e.message.starts_with("deadlock:"), "got {}", e.message);
}

/// A task cannot join itself, and the diagnostic says so rather than reporting
/// a deadlock several instructions later.
#[test]
fn a_task_cannot_join_itself() {
    let e = run_err(
        "\
box = []
def me():
    box[0].join()
t = spawn(me)
box.append(t)
t.join()
",
    );
    assert!(e.message.contains("cannot join itself"), "got {}", e.message);
}

/// `spawn` needs something that can suspend, which means an Oro frame.
#[test]
fn spawn_rejects_what_cannot_park() {
    for (src, want) in [
        ("spawn(len, [1])\n", "function defined in Oro"),
        ("def g():\n    yield 1\nspawn(g)\n", "generator function"),
        ("spawn()\n", "at least 1 argument"),
    ] {
        let e = run_err(src);
        assert!(e.message.contains(want), "{src}: got {}", e.message);
    }
}

/// `spawn(f, *args, **kwargs)` binds exactly as `f(*args, **kwargs)` would:
/// the same binder, in the spawner, before a task exists. So keywords reach the
/// task, and every binding error is the direct call's — same class, same
/// message, same line — raised at the `spawn` call, where a positional arity
/// error has always surfaced.
#[test]
fn spawn_binds_keywords_like_a_direct_call() {
    let v = eval_var(
        "\
def f(a, b=1, c=2):
    return f\"{a} {b} {c}\"
class K:
    def m(self, x, y=0):
        return x + y
kw = {\"b\": 7}
r = [spawn(f, 1, c=9).join(), spawn(f, a=5).join(), spawn(f, *[3], **kw).join(), spawn(K().m, 1, y=41).join()]
",
        "r",
    );
    assert_eq!(v.repr(), "['1 1 9', '5 1 2', '3 7 2', 42]");

    let def = "def f(a, b=1, c=2):\n    return a\n";
    for (direct, spawned) in [
        ("f(1, x=1)", "spawn(f, 1, x=1)"),
        ("f(1, a=2)", "spawn(f, 1, a=2)"),
        ("f(b=2)", "spawn(f, b=2)"),
        ("f()", "spawn(f)"),
        ("f(1, 2, 3, 4)", "spawn(f, 1, 2, 3, 4)"),
    ] {
        let d = run_err(&format!("{def}{direct}\n"));
        let s = run_err(&format!("{def}{spawned}\n"));
        // Uncaught, both arrive rendered as `Class: message`.
        assert!(d.message.starts_with("TypeError: "), "{direct}: {}", d.message);
        assert_eq!((&d.message, d.line), (&s.message, s.line), "{spawned}");
    }

    // Synchronous: the error is catchable around the `spawn` line, and no task
    // was created to run later — the implicit join-all at exit finds nothing.
    let v = eval_var(
        "\
out = []
def f(a):
    out.append(\"ran\")
try:
    spawn(f, bogus=1)
except TypeError as e:
    out.append(\"caught at spawn\")
r = out
",
        "r",
    );
    assert_eq!(v.repr(), "['caught at spawn']");
}

/// `yield_now()` hands the CPU over and answers `null`.
///
/// The two properties worth pinning: with a peer ready the tasks alternate,
/// and with nothing else ready the yield returns immediately instead of
/// declaring a deadlock — a yielding task is not waiting for anything, so it
/// never reaches `parked`.
#[test]
fn yield_now_hands_over_and_never_deadlocks() {
    let v = eval_var(
        "\
out = []
def w(log, name):
    for i in range(2):
        log.append(name + i.to_str())
        yield_now()
a = spawn(w, out, \"a\")
b = spawn(w, out, \"b\")
a.join()
b.join()
",
        "out",
    );
    assert_eq!(v.repr(), "['a0', 'b0', 'a1', 'b1']");

    // Alone in the program, and inside a generator's frame, it is a no-op that
    // evaluates to `null`.
    assert_eq!(eval("r = yield_now()\n").repr(), "null");
    let v = eval_var(
        "\
out = []
def g():
    for i in range(2):
        yield_now()
        yield i
for x in g():
    out.append(x)
",
        "out",
    );
    assert_eq!(v.repr(), "[0, 1]");

    // It takes nothing, and says so rather than discarding an argument.
    assert!(run_err("yield_now(1)\n").message.contains("takes 0 argument(s)"));
    assert!(run_err("yield_now(x=1)\n").message.contains("no keyword arguments"));
}

/// `chan(0)` is the default spelled out, not an error; a negative or
/// non-integer capacity is.
#[test]
fn chan_capacity_is_checked() {
    assert_eq!(eval("r = chan(0)\n").repr(), "<channel cap=0>");
    assert_eq!(eval("r = chan()\n").repr(), "<channel cap=0>");
    assert_eq!(eval("r = chan(4)\n").repr(), "<channel cap=4>");
    assert!(run_err("r = chan(-1)\n").message.contains("must not be negative"));
    assert!(run_err("r = chan(\"x\")\n").message.contains("must be an int"));
}

/// The concurrency surface's misuse diagnostics are ordinary typed exceptions,
/// so a program can catch them. They name their class outright — which was once
/// this file's own rule and is now the whole crate's.
#[test]
fn the_concurrency_diagnostics_are_catchable_by_class() {
    let v = eval_var(
        "\
out = []
def f():
    return 0
for thunk in [() => chan(-1), () => chan(\"x\"), () => spawn(len, []), () => chan().send()]:
    try:
        thunk()
        out.append(\"no raise\")
    except ValueError:
        out.append(\"ValueError\")
    except TypeError:
        out.append(\"TypeError\")
r = out
",
        "r",
    );
    assert_eq!(v.repr(), "['ValueError', 'TypeError', 'TypeError', 'TypeError']");
}

/// Two tasks importing one module is a rendezvous, not a cycle — but a module
/// that imports itself still is.
#[test]
fn concurrent_imports_rendezvous_but_real_cycles_still_raise() {
    // A real cycle inside one task: `json` is a builtin module, so use the
    // import machinery's own bookkeeping to check the owner test directly.
    let mut vm = Vm::new(Vec::new());
    vm.importing.insert("m".to_string(), 7);
    vm.task.id = 7;
    assert!(
        matches!(vm.await_import("m", 7), Step::Raise(_)),
        "the task already running the body sees a cycle"
    );
    assert!(vm.import_waiters.is_empty(), "a cycle does not queue anybody");
    vm.task.id = 9;
    assert!(
        matches!(vm.await_import("m", 7), Step::Park(_)),
        "a different task waits for it instead"
    );
    vm.task.id = 11;
    assert!(matches!(vm.await_import("m", 7), Step::Park(_)));
    assert_eq!(vm.import_waiters["m"], vec![9, 11]);
}

/// A module body that raises must release the path, or a *retry* reports
/// `circular import detected` instead of the real error. That bug predates the
/// scheduler; the rendezvous is what made it worth fixing rather than noting.
#[test]
fn a_failed_module_body_releases_its_path() {
    let mut vm = Vm::new(Vec::new());
    vm.importing.insert("m".to_string(), 0);
    vm.import_waiters.insert("m".to_string(), vec![]);
    let exc = vm.make_exception_instance(vm.excs["ValueError"].clone(), Vec::new());
    vm.release_import("m", Err(exc));
    assert!(vm.importing.is_empty(), "the path must not survive a failed body");
    assert!(vm.import_waiters.is_empty());
}

/// Resolving happens for a name and for nothing else.
///
/// The half of `net.dial` a program cannot see. A literal `ip:port` must leave
/// the resolver pool completely untouched — no thread started, no `Waker` fd
/// created, no park — because it has nothing to look up; a hostname must start
/// exactly the machinery that a literal does not. This is asserted against the
/// reactor rather than through behaviour because there is no behaviour to
/// assert on: `/etc/hosts` answers `localhost` in microseconds, so a lookup
/// that happened and a lookup that did not look identical from the outside.
///
/// It is also the standing check on the cost claim. A program that never
/// resolves a name pays for none of this, and "pays for none of it" means the
/// number below is zero.
#[test]
fn resolving_only_happens_for_a_name() {
    // A real listener, so the dials connect rather than failing for an
    // unrelated reason. Loopback only, and the port is the kernel's.
    let ln = crate::net::listen("127.0.0.1:0", false).expect("bind an ephemeral port");
    let addr = ln.addr_attr("local").expect("read the port back");
    let port = addr.rsplit(':').next().expect("an address has a port").to_string();

    fn resolver_threads(src: &str) -> usize {
        let mut vm = Vm::new(Vec::new());
        vm.push_module_frame(compile_module(src));
        vm.run_loop().expect("run");
        vm.reactor.resolver_threads()
    }

    assert_eq!(
        resolver_threads(&format!("import net\nc = net.dial(\"{addr}\")\n")),
        0,
        "dialling a literal ip:port started a resolver thread — it has no name to look up, \
         and this is the path every test in this tree took while DNS was blocking"
    );
    assert_eq!(
        resolver_threads(&format!("import net\nc = net.dial(\"localhost:{port}\")\n")),
        1,
        "dialling a name should start exactly one resolver thread: the pool grows one \
         worker per *concurrent* lookup, and there is one"
    );
}

// --- Which file a diagnostic names -------------------------------------------
//
// A location is a file, a line and a column, and for as long as `std/` was two
// short modules nobody noticed that Oro only ever knew two of the three: the
// line and the column came from the running frame's span table, and the file
// was whatever the CLI had been handed. An exception raised inside an embedded
// stdlib module was therefore reported as the *user's script* at the
// *library's* line — a pair that points at a real line of a real file and is
// wrong about both. `std/http.oro` is ~1300 lines, so the number it invents is
// usually past the end of the script; on a script long enough it is not, which
// is worse.
//
// These assert the file only. The lines inside `std/*.oro` belong to those
// modules and will move; that a diagnostic names the module at all is the
// property under test, and it is the one that was broken.

#[test]
fn an_error_in_an_embedded_stdlib_module_names_that_module() {
    let err = run_err("import json\njson.stringify({1: 2})\n");
    assert_eq!(
        &*err.source, "<std/json.oro>",
        "an error raised inside std/json.oro was reported against `{}`",
        err.source
    );

    let err = run_err("import io\nio.read(io.buffer(b\"ab\"), 5)\n");
    assert_eq!(&*err.source, "<std/io.oro>", "got: {}", err.source);
}

#[test]
fn an_error_in_the_script_still_names_the_script() {
    // The other half of the fix, and the half a differential would catch: a
    // program that never imports anything must report exactly what it always
    // reported. `run_err` compiles under the name "test.oro".
    let err = run_err("x = 1\ny = x + \"z\"\n");
    assert_eq!(&*err.source, "test.oro");
    assert_eq!((err.line, err.col), (2, 5));
}

#[test]
fn an_error_in_a_user_callback_names_the_callers_file_not_the_librarys() {
    // The boundary crossed the other way: `io.copy` is a frame in
    // `std/io.oro`, and the `write` it calls is the user's. The exception is
    // raised in the user's frame, so the user's file is what a reader needs.
    let err = run_err(
        "import io\n\
         class W:\n\
         \x20   def write(self, b):\n\
         \x20       raise OSError(\"disk on fire\")\n\
         io.copy(W(), io.buffer(b\"hello\"))\n",
    );
    assert_eq!(&*err.source, "test.oro", "got: {}", err.source);
    assert_eq!(err.line, 4, "the raise, not the call into io.copy");
}

#[test]
fn a_stdlib_error_caught_in_user_code_is_still_the_users_to_re_raise() {
    // Catching is unaffected — the file rides on the *diagnostic*, not on the
    // exception value — and a `raise` in the handler is reported where the
    // handler is.
    let err = run_err(
        "import json\n\
         try:\n\
         \x20   json.stringify({1: 2})\n\
         except TypeError:\n\
         \x20   raise ValueError(\"mine now\")\n",
    );
    assert_eq!(&*err.source, "test.oro", "got: {}", err.source);
    assert_eq!(err.line, 5);
}

#[test]
fn the_file_and_the_line_always_come_from_the_same_frame() {
    // The invariant behind the fix, stated as a test: whatever frame supplies
    // `line`, supplies `source`. A `finally` that re-raises reports its own
    // `EndFinally` — that is pre-existing behaviour and not what is under test
    // here; what is under test is that the *file* moved with it, so the pair
    // still names somewhere that exists.
    let err = run_err(
        "import json\n\
         try:\n\
         \x20   json.stringify({1: 2})\n\
         finally:\n\
         \x20   x = 1\n",
    );
    assert_eq!(&*err.source, "test.oro", "got: {}", err.source);
    assert_eq!(err.line, 2, "the try statement's EndFinally, in the script's own frame");
}

/// Every [`Exc`] names a class the registry actually has.
///
/// `error_to_exception` indexes the registry by `Exc::name()`, so a variant
/// with no class behind it is a panic on a cold path — which is precisely the
/// failure mode the old `raise("TypeError", …)` spelling had, moved from a
/// string literal to an enum where the compiler can at least see it. This is
/// the check the compiler cannot do.
#[test]
fn every_exception_class_exists() {
    let registry = super::exceptions::build_registry();
    for e in crate::exc::Exc::ALL {
        assert!(registry.contains_key(e.name()), "{} is not in the registry", e.name());
    }
}
