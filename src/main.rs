//! The `oro` command-line front-end.
//!
//! The lexer and parser exist so far, so the binary is a compiler-front-end
//! harness. Given a `.oro` file it prints the parsed AST by default; pass
//! `--tokens` to print the raw token stream instead.

use std::process::ExitCode;

use oro_lang::lexer::Lexer;
use oro_lang::parser::Parser;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    // Modes: `oro <file>` dumps the AST; `oro --tokens <file>` dumps tokens.
    let (tokens_mode, path) = match args.as_slice() {
        [_, flag, path] if flag == "--tokens" => (true, path),
        [_, path] if !path.starts_with('-') => (false, path),
        _ => {
            eprintln!("usage: oro [--tokens] <file.oro>");
            return ExitCode::from(64); // EX_USAGE
        }
    };

    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("oro: cannot read {path}: {e}");
            return ExitCode::from(66); // EX_NOINPUT
        }
    };

    let tokens = match Lexer::new(&source).tokenize() {
        Ok(tokens) => tokens,
        Err(e) => {
            eprintln!("{path}:{e}");
            return ExitCode::FAILURE;
        }
    };

    if tokens_mode {
        for t in &tokens {
            println!("{}:{}\t{:?}", t.line, t.col, t.kind);
        }
        return ExitCode::SUCCESS;
    }

    match Parser::new(tokens).parse() {
        Ok(program) => {
            for stmt in &program {
                println!("{stmt:#?}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{path}:{e}");
            ExitCode::FAILURE
        }
    }
}
