//! The `oro` command-line front-end.
//!
//! `oro <file.oro>` compiles and runs a program. The earlier inspection modes
//! are kept: `--tokens` dumps the lexer output and `--ast` dumps the parse tree.

use std::process::ExitCode;

use oro_lang::compiler;
use oro_lang::lexer::Lexer;
use oro_lang::parser::Parser;
use oro_lang::vm;

enum Mode {
    Run,
    Tokens,
    Ast,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    // Program arguments after the script populate `sys.argv` (argv[0] is the
    // script path, Python-style).
    let mut prog_argv: Vec<String> = Vec::new();
    // `--version`/`-V` as the first argument short-circuits before any file
    // handling. Only the first arg: `oro script.oro -V` passes `-V` to the
    // script (in sys.argv), matching CPython.
    if matches!(args.get(1).map(String::as_str), Some("--version") | Some("-V")) {
        println!("oro {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    let (mode, path) = match args.as_slice() {
        [_, flag, path] if flag == "--tokens" => (Mode::Tokens, path),
        [_, flag, path] if flag == "--ast" => (Mode::Ast, path),
        [_, path, rest @ ..] if !path.starts_with('-') => {
            prog_argv.push(path.clone());
            prog_argv.extend(rest.iter().cloned());
            (Mode::Run, path)
        }
        _ => {
            eprintln!("usage: oro [--tokens | --ast] <file.oro> [args...]");
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

    if let Mode::Tokens = mode {
        for t in &tokens {
            println!("{}:{}\t{:?}", t.line, t.col, t.kind);
        }
        return ExitCode::SUCCESS;
    }

    let program = match Parser::new(tokens).parse() {
        Ok(program) => program,
        Err(e) => {
            eprintln!("{path}:{e}");
            return ExitCode::FAILURE;
        }
    };

    if let Mode::Ast = mode {
        for stmt in &program {
            println!("{stmt:#?}");
        }
        return ExitCode::SUCCESS;
    }

    let code = match compiler::compile(&program) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{path}:{e}");
            return ExitCode::FAILURE;
        }
    };

    match vm::run_main(code, prog_argv) {
        Ok(0) => ExitCode::SUCCESS,
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("{path}:{e}");
            ExitCode::FAILURE
        }
    }
}
