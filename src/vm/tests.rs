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
