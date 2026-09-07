//! Integration tests: run each `tests/programs/*.oro` through the `oro` binary
//! and assert its stdout matches the checked-in `*.out` file.
//!
//! These are end-to-end — they exercise the real front-end path (lex → parse →
//! compile → run), which is the deliverable the task cares about: `oro
//! script.oro` executing whole programs.

use std::path::PathBuf;
use std::process::Command;

/// Path to the compiled `oro` binary under test (Cargo sets `CARGO_BIN_EXE_*`).
fn oro_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oro"))
}

fn run_program(name: &str) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/programs");
    let script = dir.join(format!("{name}.oro"));
    let expected = std::fs::read_to_string(dir.join(format!("{name}.out")))
        .unwrap_or_else(|_| panic!("missing expected output for {name}"));

    let output = Command::new(oro_bin())
        .arg(&script)
        .output()
        .expect("failed to launch the oro binary");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{name} exited with failure: {}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout, expected, "stdout mismatch for program `{name}`");
}

macro_rules! program_test {
    ($($name:ident),+ $(,)?) => {
        $(
            #[test]
            fn $name() {
                run_program(stringify!($name));
            }
        )+
    };
}

program_test!(
    fibonacci,
    loop_sum,
    strings,
    closures,
    collections,
    varargs,
    deep_recursion,
);

/// A runtime error should exit non-zero and report a source position, never
/// panic the interpreter.
#[test]
fn runtime_error_reports_position_without_panicking() {
    let dir = std::env::temp_dir();
    let script = dir.join("oro_err_test.oro");
    std::fs::write(&script, "x = 1\nprint(x + \"oops\")\n").unwrap();

    let output = Command::new(oro_bin()).arg(&script).output().expect("launch oro");
    assert!(!output.status.success(), "expected a failing exit code");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unsupported operand"),
        "expected a clean type error, got: {stderr}"
    );
    // The position of the faulting line should be reported.
    assert!(stderr.contains(":2:"), "error should carry a line number: {stderr}");
    let _ = std::fs::remove_file(&script);
}
