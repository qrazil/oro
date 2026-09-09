//! The built-in exception hierarchy, built once per VM as real [`Class`]
//! objects so that `raise`, `except`, and user subclasses
//! (`class MyError(Exception)`) all work through ordinary class machinery.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::value::{Class, Value};

/// Build the exception classes and return them by name. The hierarchy mirrors
/// CPython for the subset Oro supports, so `except` respects inheritance
/// (`except Exception` catches `ValueError`, `except LookupError` catches
/// `KeyError`, and so on).
pub fn build_registry() -> HashMap<&'static str, Rc<Class>> {
    let mut m: HashMap<&'static str, Rc<Class>> = HashMap::new();

    // (name, parent-name) in top-down order so each parent exists first.
    let edges: &[(&str, Option<&str>)] = &[
        ("BaseException", None),
        ("SystemExit", Some("BaseException")),
        ("Exception", Some("BaseException")),
        ("ImportError", Some("Exception")),
        ("ModuleNotFoundError", Some("ImportError")),
        ("ValueError", Some("Exception")),
        ("TypeError", Some("Exception")),
        ("LookupError", Some("Exception")),
        ("KeyError", Some("LookupError")),
        ("IndexError", Some("LookupError")),
        ("AttributeError", Some("Exception")),
        ("NameError", Some("Exception")),
        ("ArithmeticError", Some("Exception")),
        ("ZeroDivisionError", Some("ArithmeticError")),
        ("RuntimeError", Some("Exception")),
        ("NotImplementedError", Some("RuntimeError")),
        ("StopIteration", Some("Exception")),
        ("EOFError", Some("Exception")),
        ("CommandError", Some("Exception")),
        ("OSError", Some("Exception")),
        ("FileNotFoundError", Some("OSError")),
        ("PermissionError", Some("OSError")),
        ("TimeoutError", Some("OSError")),
        // CPython's exact shape, so `except ConnectionError` catches all four
        // and `except OSError` catches those plus everything above. `net` is
        // the only thing that raises them (`docs/stdlib-server-design.md` §4).
        ("ConnectionError", Some("OSError")),
        ("ConnectionRefusedError", Some("ConnectionError")),
        ("ConnectionResetError", Some("ConnectionError")),
        ("ConnectionAbortedError", Some("ConnectionError")),
        ("BrokenPipeError", Some("ConnectionError")),
    ];

    for (name, parent) in edges {
        let base = parent.map(|p| m[p].clone());
        let class = Rc::new(Class {
            name: Rc::from(*name),
            base,
            members: RefCell::new(HashMap::new()),
            is_exception: true,
        });
        m.insert(name, class);
    }
    m
}

/// Look up a builtin exception class by name (used by `LoadGlobal`).
pub fn lookup(registry: &HashMap<&'static str, Rc<Class>>, name: &str) -> Option<Value> {
    registry.get(name).map(|c| Value::Class(c.clone()))
}
