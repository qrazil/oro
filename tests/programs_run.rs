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
    apply,
    deep_recursion,
);

/// A runtime error should exit non-zero and report a source position, never
/// panic the interpreter.
#[test]
fn runtime_error_reports_position_without_panicking() {
    let dir = std::env::temp_dir();
    let script = dir.join("oro_err_test.oro");
    std::fs::write(&script, "x = 1\nprint(x + \"oops\")\n").unwrap();

    let output = Command::new(oro_bin())
        .arg(&script)
        .output()
        .expect("launch oro");
    assert!(!output.status.success(), "expected a failing exit code");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unsupported operand"),
        "expected a clean type error, got: {stderr}"
    );
    // The position of the faulting line should be reported.
    assert!(
        stderr.contains(":2:"),
        "error should carry a line number: {stderr}"
    );
    let _ = std::fs::remove_file(&script);
}

#[test]
fn version_flag_prints_version() {
    let bin = env!("CARGO_BIN_EXE_oro");
    let out = std::process::Command::new(bin)
        .arg("--version")
        .output()
        .expect("run");
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.starts_with("oro "), "got: {s}");
    // The short flag works too.
    let out2 = std::process::Command::new(bin)
        .arg("-V")
        .output()
        .expect("run");
    assert_eq!(String::from_utf8_lossy(&out2.stdout), s);
}

/// Run a throwaway program through the real binary and report (stdout, stderr,
/// exit code). Used for the rules a `.expected` file cannot see.
fn run_source(name: &str, src: &str) -> (String, String, i32) {
    let script = std::env::temp_dir().join(format!("oro_{name}.oro"));
    std::fs::write(&script, src).unwrap();
    let out = Command::new(oro_bin())
        .arg(&script)
        .output()
        .expect("launch oro");
    let _ = std::fs::remove_file(&script);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

/// `docs/stdlib-server-design.md` §3, rule 3: a task that dies with nobody
/// holding its handle is reported to stderr the moment the handle dies, and the
/// process exits 1 — the property that stops a dead worker being silent.
///
/// The corpus checks the *text*; a `.expected` file cannot see an exit code, so
/// that half is checked here.
#[test]
fn an_unjoined_failing_task_reports_and_exits_one() {
    let (stdout, stderr, code) = run_source(
        "unjoined",
        "def boom():\n    raise ValueError('nobody joined me')\nspawn(boom)\nprint('main done')\n",
    );
    assert_eq!(stdout, "main done\n");
    assert!(
        stderr.contains("ValueError: nobody joined me"),
        "stderr was: {stderr}"
    );
    assert!(
        stderr.contains(":2:"),
        "it carries the faulting position: {stderr}"
    );
    assert_eq!(code, 1, "an unreported task failure must not exit 0");
}

/// Rule 2's other half: a joined failure belongs to the joiner, so nothing is
/// printed and nothing is double-reported.
#[test]
fn a_joined_failure_is_silent_and_exits_zero() {
    let (stdout, stderr, code) = run_source(
        "joined",
        "def boom():\n    raise ValueError('mine')\nt = spawn(boom)\ntry:\n    t.join()\nexcept ValueError as e:\n    print('caught', e)\n",
    );
    assert_eq!(stdout, "caught mine\n");
    assert_eq!(stderr, "", "a joined failure must not also print");
    assert_eq!(code, 0);
}

/// `sys.exit(code)` is the deliberate act (§3's escape hatch), so it outranks
/// the failure flag — and it stops the program from wherever it is called,
/// abandoning in-flight work rather than waiting for the implicit join-all.
#[test]
fn sys_exit_outranks_a_dropped_task_failure() {
    let (stdout, _, code) = run_source(
        "exitwins",
        "import sys\ndef boom():\n    raise ValueError('x')\nspawn(boom).join()\n",
    );
    assert!(stdout.is_empty());
    assert_eq!(code, 1, "an uncaught re-raise in main is still a failure");

    // The task really does fail first — `spawn(nothing).join()` hands it the
    // CPU — so the failure flag is set and then overruled.
    let (stdout, stderr, code) = run_source(
        "exitzero",
        "import sys\ndef boom():\n    raise ValueError('x')\ndef nothing():\n    return 0\nspawn(boom)\nspawn(nothing).join()\nprint('the task already died')\nsys.exit(0)\n",
    );
    assert_eq!(stdout, "the task already died\n");
    assert!(
        stderr.contains("task failed: ") && stderr.contains("ValueError: x"),
        "the drop report still happens, and still says a *task* died: {stderr}"
    );
    assert_eq!(code, 0, "sys.exit(0) is deliberate and wins");
}

/// The unjoined-failure line says a **task** failed, and the exit code is
/// unchanged by it saying so.
///
/// The prefix is the whole content of §7 item 12: without it the line is
/// byte-for-byte what a program prints as it dies, in a runtime whose central
/// promise is that one handler's `KeyError` does not take down the other 9,999
/// connections.
#[test]
fn an_unjoined_task_failure_says_a_task_failed() {
    let (stdout, stderr, code) = run_source(
        "taskfailedline",
        "def boom():\n    d = {}\n    return d['user']\nspawn(boom)\nprint('still serving')\n",
    );
    assert_eq!(stdout, "still serving\n");
    let line = stderr.trim_end();
    assert!(line.starts_with("task failed: "), "stderr was: {stderr}");
    // Everything after the prefix is what it always was: path, line, column,
    // exception class and message.
    assert!(
        line.ends_with(":3:12: KeyError: 'user'"),
        "stderr was: {stderr}"
    );
    assert_eq!(code, 1, "an unjoined failure still exits 1");
}

/// `sys.exit` from inside a task ends the *program*, not just that task — §3's
/// one way to abandon in-flight work on purpose.
#[test]
fn sys_exit_inside_a_task_ends_the_program() {
    let (stdout, _, code) = run_source(
        "exitintask",
        "import sys\ndef quitter():\n    sys.exit(3)\ndef later():\n    print('must not run')\nspawn(quitter)\nspawn(later)\nspawn(quitter).join()\n",
    );
    assert!(!stdout.contains("must not run"), "stdout was: {stdout}");
    assert_eq!(code, 3);
}

/// The implicit join-all: main returning does not kill the tasks it started.
/// Go loses this line; §3 says Oro must not.
#[test]
fn spawned_tasks_finish_after_main_returns() {
    let (stdout, _, code) = run_source(
        "joinall",
        "def logger():\n    print('the last line the logger was writing')\nspawn(logger)\nprint('main returns')\n",
    );
    assert_eq!(
        stdout,
        "main returns\nthe last line the logger was writing\n"
    );
    assert_eq!(code, 0);
}
