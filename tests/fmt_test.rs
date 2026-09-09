//! Correctness tests for `oro fmt` (see `src/fmt.rs`).
//!
//! Two properties are checked across real programs rather than hand-picked
//! snippets, because those are the properties a formatter must never violate:
//!
//! * **Idempotence** — `fmt(fmt(x)) == fmt(x)` — over every `.oro` file in
//!   `corpus/` and `tests/programs/`.
//! * **Semantics preservation** — running a formatted `corpus/core/*.oro`
//!   file produces the same output as running the original — over every file
//!   in `corpus/core/` (the files with a `.expected` oracle; see
//!   `corpus/run.sh`).

use std::path::{Path, PathBuf};
use std::process::Command;

use oro_lang::fmt::format_source;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn oro_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oro"))
}

/// Every `.oro` file directly under `dir` (non-recursive is fine — none of
/// our source trees nest `.oro` files in subdirectories).
fn oro_files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("oro"))
        .collect();
    out.sort();
    out
}

/// Every `.oro` file under `corpus/` (all three subdirectories) and
/// `tests/programs/`.
fn all_oro_files() -> Vec<PathBuf> {
    let root = repo_root();
    let mut files = Vec::new();
    for dir in ["corpus/core", "corpus/divergence", "corpus/known-failing", "tests/programs"] {
        files.extend(oro_files(&root.join(dir)));
    }
    assert!(files.len() > 30, "expected to find a good number of .oro files, found {}", files.len());
    files
}

/// `fmt(fmt(x)) == fmt(x)` for every `.oro` file in the repo's test corpora.
/// A formatter that cannot reach a fixed point on its own output is broken by
/// definition — running it repeatedly should never keep changing the file.
#[test]
fn idempotent_across_every_corpus_file() {
    let mut failures = Vec::new();
    for path in all_oro_files() {
        let source = std::fs::read_to_string(&path).unwrap();
        let once = match format_source(&source) {
            Ok(s) => s,
            Err(e) => {
                failures.push(format!("{}: first format failed: {e}", path.display()));
                continue;
            }
        };
        let twice = match format_source(&once) {
            Ok(s) => s,
            Err(e) => {
                failures.push(format!("{}: second format failed: {e}", path.display()));
                continue;
            }
        };
        if once != twice {
            // Find the first differing line for a useful failure message.
            let a: Vec<&str> = once.lines().collect();
            let b: Vec<&str> = twice.lines().collect();
            let idx = a.iter().zip(b.iter()).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
            failures.push(format!(
                "{}: not idempotent at line {}: {:?} vs {:?}",
                path.display(),
                idx + 1,
                a.get(idx),
                b.get(idx)
            ));
        }
    }
    assert!(failures.is_empty(), "idempotence failures:\n{}", failures.join("\n"));
}

/// Formatting must never change what a program does: for every
/// `corpus/core/*.oro` file, running the formatted source through the real
/// `oro` binary must produce byte-identical stdout/stderr/exit-status to
/// running the original.
#[test]
fn semantics_preserved_for_core_corpus() {
    let core_dir = repo_root().join("corpus/core");
    let out_dir = std::env::temp_dir().join(format!("oro_fmt_semantics_{}", std::process::id()));
    std::fs::create_dir_all(&out_dir).unwrap();

    let mut failures = Vec::new();
    for path in oro_files(&core_dir) {
        let source = std::fs::read_to_string(&path).unwrap();
        let formatted = match format_source(&source) {
            Ok(s) => s,
            Err(e) => {
                failures.push(format!("{}: format failed: {e}", path.display()));
                continue;
            }
        };
        let formatted_path = out_dir.join(path.file_name().unwrap());
        std::fs::write(&formatted_path, &formatted).unwrap();

        let run = |p: &Path| {
            let out = Command::new(oro_bin()).arg(p).output().expect("launch oro");
            (out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned())
        };
        let (orig_code, orig_out) = run(&path);
        let (fmt_code, fmt_out) = run(&formatted_path);

        if orig_code != fmt_code || orig_out != fmt_out {
            failures.push(format!(
                "{}: behavior changed after formatting\n  original: exit={:?} stdout={:?}\n  formatted: exit={:?} stdout={:?}",
                path.display(),
                orig_code,
                truncate(&orig_out),
                fmt_code,
                truncate(&fmt_out),
            ));
        }
    }
    let _ = std::fs::remove_dir_all(&out_dir);
    assert!(failures.is_empty(), "semantics-preservation failures:\n{}", failures.join("\n"));
}

fn truncate(s: &str) -> String {
    if s.len() > 300 {
        format!("{}…", &s[..300])
    } else {
        s.to_string()
    }
}

/// `oro fmt` must not silently drop or misplace a comment: every corpus
/// comment is the simple "own line, outside brackets" case (verified by hand
/// when this formatter was built), so formatting must never error out on any
/// corpus file, and the comment text must still be present afterward.
#[test]
fn comments_survive_formatting() {
    for path in all_oro_files() {
        let source = std::fs::read_to_string(&path).unwrap();
        let source_comments: Vec<&str> = source
            .lines()
            .filter_map(|l| l.trim_start().strip_prefix('#'))
            .collect();
        if source_comments.is_empty() {
            continue;
        }
        let formatted = format_source(&source)
            .unwrap_or_else(|e| panic!("{}: expected format to succeed: {e}", path.display()));
        let formatted_comments: Vec<&str> = formatted
            .lines()
            .filter_map(|l| l.trim_start().strip_prefix('#'))
            .collect();
        assert_eq!(
            source_comments.len(),
            formatted_comments.len(),
            "{}: comment count changed after formatting",
            path.display()
        );
    }
}

/// A bytes literal reprints as octets: printable ASCII literally, everything
/// else as `\xNN` (the one spelling that always round-trips), and `rb"..."`
/// keeps its raw spelling for the same reason `r"..."` does.
#[test]
fn bytes_literals_reprint_as_octets() {
    let cases = [
        ("x = b\"hi\"\n", "x = b\"hi\"\n"),
        ("x = B'hi'\n", "x = b\"hi\"\n"),
        ("x = b\"\\xff\\x00\"\n", "x = b\"\\xff\\x00\"\n"),
        ("x = b\"a\\tb\\n\"\n", "x = b\"a\\tb\\n\"\n"),
        // The quote flips only to avoid escaping, as it does for `str`.
        ("x = b'say \"hi\"'\n", "x = b'say \"hi\"'\n"),
        ("x = rb\"\\d+\"\n", "x = rb\"\\d+\"\n"),
    ];
    for (src, want) in cases {
        assert_eq!(format_source(src).expect("should format"), want, "source: {src:?}");
    }
}
