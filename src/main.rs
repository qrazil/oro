//! The `oro` command-line front-end.
//!
//! `oro <file.oro>` compiles and runs a program. The earlier inspection modes
//! are kept: `--tokens` dumps the lexer output and `--ast` dumps the parse tree.

// The binary front-end carries the same rule as the library (see `lib.rs`):
// no `unsafe` here, and the compiler rather than a convention is what says so.
#![deny(unsafe_code)]

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

    if args.get(1).map(String::as_str) == Some("fmt") {
        return run_fmt(&args[2..]);
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

    let code = match compiler::compile(&program, std::rc::Rc::from(path.as_str())) {
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
            // No `{path}:` prefix here, unlike the front-end errors above. A
            // lex, parse or compile error is always *this* file; a runtime one
            // can come from any module the program imported, so the error names
            // its own file (`RuntimeError`).
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// `oro fmt [--write|-w | --check] <file.oro>` — see `src/fmt.rs`.
enum FmtMode {
    /// Print the formatted source to stdout.
    Print,
    /// Rewrite the file in place.
    Write,
    /// Exit 1 if the file is not already canonically formatted; silent on
    /// success, otherwise silent too (no diff printed, matching `gofmt -l`
    /// minus the filename — the exit code is the signal).
    Check,
}

fn run_fmt(args: &[String]) -> ExitCode {
    let mut mode = FmtMode::Print;
    let mut path: Option<&str> = None;
    for a in args {
        match a.as_str() {
            "--write" | "-w" => mode = FmtMode::Write,
            "--check" => mode = FmtMode::Check,
            other if path.is_none() && !other.starts_with('-') => path = Some(other),
            other => {
                eprintln!("oro fmt: unrecognized argument '{other}'");
                return ExitCode::from(64); // EX_USAGE
            }
        }
    }
    let Some(path) = path else {
        eprintln!("usage: oro fmt [--write | --check] <file.oro>");
        return ExitCode::from(64); // EX_USAGE
    };

    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("oro: cannot read {path}: {e}");
            return ExitCode::from(66); // EX_NOINPUT
        }
    };

    let formatted = match oro_lang::fmt::format_source(&source) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{path}:{e}");
            return ExitCode::FAILURE;
        }
    };

    match mode {
        FmtMode::Print => {
            print!("{formatted}");
            ExitCode::SUCCESS
        }
        FmtMode::Write => {
            if formatted != source {
                if let Err(e) = std::fs::write(path, &formatted) {
                    eprintln!("oro: cannot write {path}: {e}");
                    return ExitCode::FAILURE;
                }
            }
            ExitCode::SUCCESS
        }
        FmtMode::Check => {
            if formatted == source {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
    }
}
