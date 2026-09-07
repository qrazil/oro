//! VM unit tests, one cluster per opcode class. Each test runs a tiny program
//! whose module-level variables are inspected after execution (declaration
//! order fixes their slots, so the first declared name is slot 0).

use super::*;
use crate::compiler::compile;
use crate::lexer::Lexer;
use crate::parser::Parser;

/// Run `src` and return the module's local slots.
fn run_locals(src: &str) -> Vec<Value> {
    let tokens = Lexer::new(src).tokenize().expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    let code = compile(&program).expect("compile");
    let mut vm = Vm { frames: Vec::new(), line: 0, col: 0, last_locals: Vec::new() };
    let frame = Frame {
        locals: vec![Value::Unbound; code.nlocals],
        cells: (0..code.ncells).map(|_| Rc::new(RefCell::new(Value::Unbound))).collect(),
        free: Vec::new(),
        stack: Vec::new(),
        pc: 0,
        code,
    };
    vm.frames.push(frame);
    vm.run_loop().expect("run");
    vm.last_locals
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

fn run_err(src: &str) -> RuntimeError {
    let tokens = Lexer::new(src).tokenize().expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    let code = compile(&program).expect("compile");
    run(code).expect_err("expected a runtime error")
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
    assert!(matches!(eval("r = bool([])\n"), Value::Bool(false)));
    assert!(matches!(eval("r = bool([0])\n"), Value::Bool(true)));
    assert!(matches!(eval("r = bool(\"\")\n"), Value::Bool(false)));
    assert!(matches!(eval("r = bool(0.0)\n"), Value::Bool(false)));
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
    return a + sum(rest) + opts.get(\"bonus\", 0)
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
