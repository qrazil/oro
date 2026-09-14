//! `oro lint` must find nothing in the shipped Oro — `std/` and `examples/`.
//!
//! This is the gate that was missing: the lint rules migrated `std/` but not
//! `examples/`, and nothing caught the drift because linting was only ever run
//! by hand. Running it here means `cargo test` fails the moment shipped code
//! uses a spelling the linter flags, in `examples/` as much as `std/`.

use std::rc::Rc;

use oro_lang::compiler::compile;
use oro_lang::lexer::Lexer;
use oro_lang::linter::lint;
use oro_lang::parser::Parser;

/// Every `.oro` file directly under `dir`.
fn oro_files(dir: &str) -> Vec<std::path::PathBuf> {
    let mut out: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {dir}: {e}"))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "oro"))
        .collect();
    out.sort();
    out
}

fn lint_dir(dir: &str) {
    let mut hits = Vec::new();
    for path in oro_files(dir) {
        let src = std::fs::read_to_string(&path).unwrap();
        let tokens = Lexer::new(&src).tokenize().expect("lex");
        let program = Parser::new(tokens).parse().expect("parse");
        // Parse is enough for the linter, but compiling too keeps this honest:
        // shipped code must also *compile*.
        compile(&program, Rc::from("test")).expect("compile");
        for f in lint(&program) {
            hits.push(format!("{}:{}:{}: [{}] {}", path.display(), f.line, f.col, f.rule, f.message));
        }
    }
    assert!(hits.is_empty(), "oro lint found {} hit(s) in {dir}:\n{}", hits.len(), hits.join("\n"));
}

#[test]
fn std_is_lint_clean() {
    lint_dir("std");
}

#[test]
fn examples_are_lint_clean() {
    lint_dir("examples");
}
