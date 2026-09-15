//! Correctness tests for `oro fmt` (see `src/fmt.rs`).
//!
//! These properties are checked across real programs rather than hand-picked
//! snippets, because those are the properties a formatter must never violate:
//!
//! * **Idempotence** — `fmt(fmt(x)) == fmt(x)` — over every `.oro` file in
//!   the repository: `corpus/`, `std/`, `tests/programs/` and `bench/progs/`.
//! * **Semantics preservation** — running a formatted `corpus/core/*.oro`
//!   file produces the same output as running the original — over every file
//!   in `corpus/core/` (the files with a `.expected` oracle; see
//!   `corpus/run.sh`).
//! * **The tree is clean** — every `.oro` file in the repository is already in
//!   canonical form, apart from a short, named list that deliberately is not
//!   (see `only_the_documented_files_are_unformatted`).
//!
//! The shape rules themselves — in particular which collection literals break
//! across lines — are unit-tested next to the code, in `src/fmt.rs`.

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

/// Every directory in the repository that holds `.oro` source.
const ORO_DIRS: [&str; 7] = [
    "corpus/core",
    "corpus/divergence",
    "corpus/known-failing",
    "std",
    "examples",
    "tests/programs",
    "bench/progs",
];

/// Every `.oro` file in the repository.
fn all_oro_files() -> Vec<PathBuf> {
    let root = repo_root();
    let mut files = Vec::new();
    for dir in ORO_DIRS {
        files.extend(oro_files(&root.join(dir)));
    }
    assert!(
        files.len() > 30,
        "expected to find a good number of .oro files, found {}",
        files.len()
    );
    files
}

/// The `.oro` files that `oro fmt --check` is *expected* to report as dirty.
///
/// Both are written the way they are on purpose, and formatting them would
/// delete the thing they exist to cover:
///
/// * `corpus/core/35_bytes.oro` spells its literals in the alternate lexer
///   forms it is testing (`b'…'`, `B"…"`, `\0\a\b\f\v`); `oro fmt` canonicalises
///   every one of them away.
/// * `corpus/divergence/36_json.oro` writes JSON with escaped double quotes
///   (`"{\"a\": 1}"`) on purpose, where the house quote rule prefers
///   `'{"a": 1}'`.
///
/// Neither is about line breaks. If this list needs to grow, that is a decision
/// worth making deliberately, which is what this test is for.
// Files the formatter deliberately would change, kept unformatted because the
// exact source *is* the fixture. Each entry says why; an allowlist without
// reasons rots into "some files, for some reason".
const EXPECTED_UNFORMATTED: [&str; 3] = [
    // Exercises every byte-literal spelling the lexer accepts — `b'…'`, `B"…"`,
    // `\0`/`\a`/`\x41` — which the formatter canonicalises to one. Formatting it
    // would delete the variety it exists to test.
    "corpus/divergence/35_bytes.oro",
    // Feeds `json.parse` strings written with escaped double-quotes
    // (`"{\"a\":…}"`); the formatter reprints those as single-quoted. The
    // escaped spellings are the input under test.
    "corpus/divergence/36_json.oro",
    // Keeps an author-written parenthesised nested ternary so the depth-cap
    // test shows parentheses do not exempt it; the formatter would strip the
    // redundant parens (the middle branch is greedy, so the parse is the same).
    "corpus/divergence/92_ternary_depth3_parens.oro",
];

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
            let idx = a
                .iter()
                .zip(b.iter())
                .position(|(x, y)| x != y)
                .unwrap_or(a.len().min(b.len()));
            failures.push(format!(
                "{}: not idempotent at line {}: {:?} vs {:?}",
                path.display(),
                idx + 1,
                a.get(idx),
                b.get(idx)
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "idempotence failures:\n{}",
        failures.join("\n")
    );
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
            (
                out.status.code(),
                String::from_utf8_lossy(&out.stdout).into_owned(),
            )
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
    assert!(
        failures.is_empty(),
        "semantics-preservation failures:\n{}",
        failures.join("\n")
    );
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
        assert_eq!(
            format_source(src).expect("should format"),
            want,
            "source: {src:?}"
        );
    }
}

/// A numeric literal reprints in the spelling the author wrote it in.
///
/// This is the one formatter rule for numbers, and it exists because the
/// alternative loses information: `0xff` and `255` are the same value and are
/// not the same statement about what the value *is*, and neither are `1_000_000`
/// and `1000000`. The formatter is not entitled to that choice, so it keeps the
/// token's raw text — prefix letter case included.
#[test]
fn numeric_literals_reprint_in_the_authors_spelling() {
    for src in [
        "x = 0xff\n",
        "x = 0XFF\n",
        "x = 0o755\n",
        "x = 0b1010_1010\n",
        "x = 1_000_000\n",
        "x = 0x_dead_beef\n",
        "x = 255\n",
        "x = 1_000.5\n",
        "x = 1e1_0\n",
        "x = .5\n",
        "x = 0\n",
        "x = 000\n",
    ] {
        assert_eq!(
            format_source(src).expect("should format"),
            src,
            "source: {src:?}"
        );
    }
}

/// Every `.oro` file in the repository is already in canonical form, except
/// the deliberate exceptions named in [`EXPECTED_UNFORMATTED`].
///
/// This is `oro fmt --check` over the whole tree, as a test. It is the guard
/// that keeps the formatter honest in both directions: a formatter nobody can
/// leave switched on is the bug that motivated the author's-line-breaks rule
/// in the first place, and a file that quietly stops being checked is how that
/// comes back.
#[test]
fn only_the_documented_files_are_unformatted() {
    let root = repo_root();
    let mut dirty = Vec::new();
    for path in all_oro_files() {
        let source = std::fs::read_to_string(&path).unwrap();
        let formatted = format_source(&source)
            .unwrap_or_else(|e| panic!("{}: expected format to succeed: {e}", path.display()));
        if formatted != source {
            dirty.push(
                path.strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }
    dirty.sort();
    let mut want: Vec<String> = EXPECTED_UNFORMATTED.iter().map(|s| s.to_string()).collect();
    want.sort();
    assert_eq!(
        dirty, want,
        "\n`oro fmt --check` disagrees with EXPECTED_UNFORMATTED.\n\
         A file in `dirty` but not `want` is unformatted: run \
         `oro fmt --write <file>` to fix it, or — if its exact source is a \
         fixture the formatter must not touch — add it to EXPECTED_UNFORMATTED \
         with a one-line reason.\n\
         A file in `want` but not `dirty` is now formatted: remove it from \
         EXPECTED_UNFORMATTED.\n\
         If this passes locally but fails in CI (or the reverse), the toolchain \
         drifted: the formatter classifies Unicode with std's tables, so the \
         version must match rust-toolchain.toml — that is what pins it."
    );
}

/// The author's line breaks survive a round trip through the formatter, over
/// the repository's own files rather than snippets.
///
/// Checked structurally: a list or dict literal the author broke the line after
/// must still be broken afterwards, and one they did not must still be joined.
/// `(` is excluded — it is overwhelmingly a call or a grouping rather than a
/// tuple literal, and argument lists deliberately do not follow the rule; the
/// tuple cases are covered by the unit tests in `src/fmt.rs`, which can name
/// them exactly.
///
/// Most repository files are already canonical, so for them this restates
/// idempotence; it earns its keep on the files that are not
/// ([`EXPECTED_UNFORMATTED`]) and on whatever is added later.
#[test]
fn authored_line_breaks_survive_formatting() {
    use oro_lang::lexer::{Lexer, TokenKind};

    /// Can this token end an expression? If so, a `[` right after it is a
    /// *subscript*, not a list literal — the same prefix-vs-postfix test the
    /// parser makes. Subscripts do not follow the author's-line-breaks rule
    /// (nothing but literals does), so they must not be counted.
    fn ends_an_expression(k: &TokenKind) -> bool {
        matches!(
            k,
            TokenKind::Ident(_)
                | TokenKind::Int(_)
                | TokenKind::Float(_)
                | TokenKind::Str(_, _)
                | TokenKind::Bytes(_, _)
                | TokenKind::FString(_)
                | TokenKind::True
                | TokenKind::False
                | TokenKind::None
                | TokenKind::RParen
                | TokenKind::RBracket
                | TokenKind::RBrace
        )
    }

    /// How many list/dict literals in `src` have a newline directly after the
    /// opening bracket, and how many do not.
    fn breaks(src: &str) -> (usize, usize) {
        let tokens = Lexer::new(src).tokenize().expect("tokenize");
        let mut broken = 0;
        let mut joined = 0;
        for (i, t) in tokens.iter().enumerate() {
            // `{` is always a dict in Oro (sets are cut, and there is no
            // `{...}` postfix); `[` is a list only in prefix position.
            let is_literal = match t.kind {
                TokenKind::LBrace => true,
                TokenKind::LBracket => i == 0 || !ends_an_expression(&tokens[i - 1].kind),
                _ => false,
            };
            if !is_literal {
                continue;
            }
            match tokens.get(i + 1) {
                // An empty literal has no elements to break between, so it is
                // neither: `[\n]` legitimately prints as `[]`.
                Some(n) if matches!(n.kind, TokenKind::RBracket | TokenKind::RBrace) => {}
                Some(n) if n.line > t.line => broken += 1,
                Some(_) => joined += 1,
                None => {}
            }
        }
        (broken, joined)
    }

    let mut failures = Vec::new();
    for path in all_oro_files() {
        let source = std::fs::read_to_string(&path).unwrap();
        let formatted = format_source(&source)
            .unwrap_or_else(|e| panic!("{}: expected format to succeed: {e}", path.display()));
        let before = breaks(&source);
        let after = breaks(&formatted);
        if before != after {
            failures.push(format!(
                "{}: (broken, joined) literals went from {before:?} to {after:?}",
                path.display()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "line breaks were not preserved:\n{}",
        failures.join("\n")
    );
}
