//! Doc conformance: every ` ```oro ` snippet in the reference and the README
//! must compile and run without error.
//!
//! The reference's fenced `oro` blocks are written to be self-contained and
//! print something — they are the language's worked examples — so "it still
//! runs" is a real check: a snippet that used cut surface (`set()`,
//! `min(a, b)`, `a if c else b`) would fail here the moment the surface changed
//! under it. That is the class of doc rot that reached a release before this
//! test existed.
//!
//! It checks execution, not output: the blocks are not paired with an expected
//! transcript in the source, so matching one would need a convention the docs
//! do not carry. The README's other fences are ` ```python ` — illustrative
//! fragments, Python-vs-Oro comparisons, and server examples that block — none
//! of them runnable standalone, so they are deliberately not run; the README's
//! *prose* truth is checked by hand. Any real ` ```oro ` block added to the
//! README is picked up here automatically.

use std::rc::Rc;

use oro_lang::compiler::compile;
use oro_lang::lexer::Lexer;
use oro_lang::parser::Parser;
use oro_lang::vm;

/// Every ` ```oro … ``` ` block in `md`, in source order.
fn oro_blocks(md: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut lines = md.lines();
    while let Some(line) = lines.next() {
        if line.trim_end() == "```oro" {
            let mut block = String::new();
            for l in lines.by_ref() {
                if l.trim_end() == "```" {
                    break;
                }
                block.push_str(l);
                block.push('\n');
            }
            out.push(block);
        }
    }
    out
}

/// Compile and run one snippet, mapping any failure to its message.
fn run_snippet(src: &str) -> Result<(), String> {
    let tokens = Lexer::new(src).tokenize().map_err(|e| format!("lex: {}", e.message))?;
    let program = Parser::new(tokens).parse().map_err(|e| format!("parse: {}", e.message))?;
    let code = compile(&program, Rc::from("<doc>")).map_err(|e| format!("compile: {}", e.message))?;
    // `run_main` with an argv, as the binary runs a script: a snippet may read
    // `sys.argv[0]` (one does), and `sys.exit` is a clean exit code here rather
    // than an escaping error.
    vm::run_main(code, vec!["doc.oro".to_string()])
        .map(|_| ())
        .map_err(|e| format!("{:?}: {}", e.class, e.message))
}

fn check(path: &str, min_expected: usize) {
    let md = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let blocks = oro_blocks(&md);
    assert!(
        blocks.len() >= min_expected,
        "{path}: found {} `oro` snippets, expected at least {min_expected} — has the gate \
         silently stopped seeing them?",
        blocks.len()
    );
    for (i, b) in blocks.iter().enumerate() {
        if let Err(e) = run_snippet(b) {
            panic!("{path}: `oro` snippet #{i} failed to run ({e})\n--- snippet ---\n{b}");
        }
    }
}

#[test]
fn reference_oro_snippets_run() {
    // The reference ships ~53 worked examples; the floor guards the gate itself.
    check("docs/reference.md", 50);
}

#[test]
fn readme_oro_snippets_run() {
    // The README currently has no `oro`-fenced blocks (its examples are
    // `python`-fenced fragments and comparisons); this still runs any that are
    // added, and asks for none.
    check("README.md", 0);
}
