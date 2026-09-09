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
    let mut vm = Vm::new(Vec::new());
    vm.push_module_frame(code);
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

fn compile_err(src: &str) -> crate::compiler::CompileError {
    let tokens = Lexer::new(src).tokenize().expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    compile(&program).expect_err("expected a compile error")
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
    // 1 == True == 1.0; the first matching case wins, even through the table.
    let src = "def f(v):\n    match v:\n        case 1:\n            return \"one\"\n        \
               case True:\n            return \"true\"\n        case _:\n            return \"x\"\n\
               a = f(1)\nb = f(True)\n";
    let locals = run_locals(src);
    for v in locals {
        if let Value::Str(s) = v {
            if s.s == "true" {
                panic!("True should have matched `case 1` first, not `case True`");
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

// --- exceptions -----------------------------------------------------------

#[test]
fn exception_caught_by_type() {
    let src = "def f():\n    try:\n        raise ValueError(\"x\")\n    except ValueError as e:\n        return str(e)\n\
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
               def f():\n    try:\n        raise MyError(\"custom\")\n    except Exception as e:\n        return str(e)\n\
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
    let src = "a = [1]\na.append(a)\nout = str(a)\n";
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
    // quiet=True suppresses the live tee; the capture happens either way. The
    // captured streams are octets, so decoding is explicit.
    let src = "import proc\nr = proc.run([\"echo\", \"hi\"], quiet=True)\nout = r.returncode.to_str() + \":\" + r.stdout.strip().to_str()\n";
    assert_eq!(fstr(src), "0:hi");
}

#[test]
fn proc_raises_on_nonzero_exit_by_default() {
    let err = run_err("import proc\nproc.run([\"sh\", \"-c\", \"exit 4\"], quiet=True)\n");
    assert!(err.message.contains("command failed"), "got: {}", err.message);
}

#[test]
fn proc_check_false_allows_nonzero_exit() {
    let src = "import proc\nr = proc.run([\"sh\", \"-c\", \"exit 4\"], check=False, quiet=True)\nout = r.returncode.to_str() + \":\" + r.ok.to_str()\n";
    assert_eq!(fstr(src), "4:False");
}

#[test]
fn proc_rejects_cpython_capture_kwargs() {
    let err = run_err(
        "import proc\nproc.run([\"echo\", \"hi\"], capture_output=True)\n",
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
    // `find` and `join` exist on both str and collections; each keeps its own
    // meaning, chosen by the receiver's type.
    let src = r#"
letters = ["a", "b"]
a = "abcb".find("b")
b = ", ".join(letters)
c = letters.join("-")
d = [1, 2, 3].find(x => x > 1)
out = f"{a} {b} {c} {d}"
"#;
    assert_eq!(fstr(src), "1 a, b a-b 2");
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
    assert_eq!(fstr(src), "3 <class 'int'> 2.5 <class 'float'> hi True None [1, 2, 3]");
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
data = {"a": 1, "b": [1, 2.5, "x", True, False, None]}
out = f"{json.parse(json.stringify(data)) == data}"
"#;
    assert_eq!(fstr(src), "True");
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
    assert_eq!(eval("r = b\"  ab  \".lstrip()\n").repr(), "b'ab  '");
    assert_eq!(eval("r = b\"  ab  \".rstrip()\n").repr(), "b'  ab'");
    assert_eq!(eval("r = b\"a,b,,c\".split(b\",\")\n").repr(), "[b'a', b'b', b'', b'c']");
    assert_eq!(eval("r = b\"a,b,c\".split(b\",\", 1)\n").repr(), "[b'a', b'b,c']");
    assert_eq!(eval("r = b\"a,b,c\".rsplit(b\",\", 1)\n").repr(), "[b'a,b', b'c']");
    assert_eq!(eval("r = b\"a b\\x0bc\".split()\n").repr(), "[b'a', b'b', b'c']");
    assert_eq!(eval("r = [b\"a\", b\"b\"].join(b\"-\")\n").repr(), "b'a-b'");
    assert_eq!(eval("r = b\"-\".join([b\"a\", b\"b\"])\n").repr(), "b'a-b'");
    assert_eq!(int(&eval("r = b\"abc\".find(b\"b\")\n")), 1);
    assert_eq!(int(&eval("r = b\"abc\".find(b\"z\")\n")), -1);
    // The empty needle is found at 0, as it is for `str`.
    assert_eq!(int(&eval("r = b\"abc\".find(b\"\")\n")), 0);
    assert_eq!(eval("r = b\"abc\".replace(b\"b\", b\"XY\")\n").repr(), "b'aXYc'");
    assert!(eval("r = b\"abc\".startswith(b\"ab\")\n").truthy());
    assert!(eval("r = b\"abc\".endswith(b\"bc\")\n").truthy());
    assert_eq!(eval("r = b\"-7\".zfill(4)\n").repr(), "b'-007'");
    assert_eq!(eval("r = b\"\\xff\\x00A\".hex()\n").repr(), "'ff0041'");
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
}
