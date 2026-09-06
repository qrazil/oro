//! The `oro` command-line front-end.
//!
//! Only the lexer exists so far, so the binary is a lexer harness: given a
//! `.oro` file it prints the token stream, one token per line, with positions.

use std::process::ExitCode;

use oro_lang::lexer::Lexer;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: oro <file.oro>");
        return ExitCode::from(64); // EX_USAGE
    }

    let path = &args[1];
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("oro: cannot read {path}: {e}");
            return ExitCode::from(66); // EX_NOINPUT
        }
    };

    match Lexer::new(&source).tokenize() {
        Ok(tokens) => {
            for t in &tokens {
                println!("{}:{}\t{:?}", t.line, t.col, t.kind);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{path}:{e}");
            ExitCode::FAILURE
        }
    }
}
