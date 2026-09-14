//! Unit tests for the JSON codec.
//!
//! The behavioural baseline is `corpus/divergence/36_json.oro`, which pins the
//! exact accepted inputs, rejected inputs and diagnostics this replaced, and
//! passes unmodified. What is here is what a corpus program cannot reach: the
//! two stack disciplines (a document deeper than the Rust stack could carry,
//! and a value deeper than an Oro frame stack could carry), the character-vs-
//! byte offset agreement that only shows up on non-ASCII input, and the
//! exception class each diagnostic names.

use super::*;
use crate::exc::Exc;

fn ok(text: &str) -> Value {
    parse(text).unwrap_or_else(|e| panic!("{text:?} should parse: {e}"))
}

fn err(text: &str) -> String {
    parse(text)
        .expect_err(&format!("{text:?} should not parse"))
        .message
}

fn err_class(text: &str) -> Exc {
    parse(text)
        .expect_err(&format!("{text:?} should not parse"))
        .class
}

#[test]
fn scalars_and_containers() {
    assert_eq!(ok("null").repr(), "null");
    assert_eq!(ok("  \n\t [ 1 , 2 ,3]\r\n ").repr(), "[1, 2, 3]");
    assert_eq!(
        ok(r#"{"a": {"b": [1, [2, 3], {}]}}"#).repr(),
        "{'a': {'b': [1, [2, 3], {}]}}"
    );
    // A repeated key takes the last value at the first key's position, which is
    // `OroDict`'s rule for `d[k] = v` and CPython's for `json.loads`.
    assert_eq!(ok(r#"{"a":1,"b":2,"a":3}"#).repr(), "{'a': 3, 'b': 2}");
}

#[test]
fn integers_promote_past_i64() {
    assert_eq!(ok("9223372036854775807").repr(), "9223372036854775807");
    assert_eq!(ok("9223372036854775808").repr(), "9223372036854775808");
    assert_eq!(ok("-9223372036854775809").repr(), "-9223372036854775809");
    assert_eq!(
        ok("123456789012345678901234567890").repr(),
        "123456789012345678901234567890"
    );
    // `-0` is the integer zero; `-0.0` keeps its sign, as every other Oro float
    // does.
    assert_eq!(ok("-0").repr(), "0");
    assert_eq!(ok("-0.0").repr(), "-0.0");
    // An exponent past the float range is an infinity rather than an error —
    // `to_float()`'s answer, and CPython's `json`'s.
    assert_eq!(ok("1e400").repr(), "inf");
    assert_eq!(ok("1e-400").repr(), "0.0");
}

#[test]
fn surrogate_pairs_join_and_lone_surrogates_are_rejected() {
    assert_eq!(ok(r#""\ud83d\ude00""#).repr(), "'\u{1f600}'");
    // Oro's `str` is UTF-8, so an unpaired surrogate is not a value it can
    // hold. CPython's `json` accepts one and hands back a `str` that cannot be
    // encoded; this rejects it, as the Oro-written parser did.
    for text in [
        r#""\ud83d""#,
        r#""\ude00""#,
        r#""\ud83d\ud83d""#,
        r#""\ud83dA""#,
    ] {
        assert_eq!(
            err(text),
            "unpaired UTF-16 surrogate in \\u escape at position 7"
        );
    }
    // The recovery path: a high surrogate followed by something that starts
    // like a pair and is not one must rewind to the high surrogate's own
    // position, not report where the second escape ended.
    assert_eq!(
        err(r#""\ud83d\u0041""#),
        "unpaired UTF-16 surrogate in \\u escape at position 7"
    );
    // ...but a *malformed* second escape is reported as malformed.
    assert_eq!(
        err(r#""\ud83d\uZZZZ""#),
        "invalid \\u escape 'ZZZZ' at position 9"
    );
}

/// Every diagnostic counts characters, because the parser this replaced indexed
/// the text by character. Byte offsets and character offsets only disagree on
/// non-ASCII input, so nothing else in the suite would catch a slip.
#[test]
fn positions_are_character_offsets() {
    assert_eq!(err("[\u{4e2d}]"), "unexpected '\u{4e2d}' at position 1");
    assert_eq!(
        err("[\"\u{1f600}\u{e9}\", }"),
        "unexpected '}' at position 7"
    );
    assert_eq!(
        err("{\"\u{e9}\u{4e2d}\": }"),
        "unexpected '}' at position 7"
    );
    // A non-ASCII character where four hex digits were promised is quoted
    // whole, and the "four characters remain" test counts characters too.
    assert_eq!(
        err("\"\\u\u{e9}\u{e9}\u{e9}\u{e9}\""),
        "invalid \\u escape '\u{e9}\u{e9}\u{e9}\u{e9}' at position 3"
    );
    assert_eq!(
        err("\"\\u\u{e9}\u{e9}\""),
        "incomplete \\u escape at position 3"
    );
}

/// Nesting is bounded, and the bound is reported as an ordinary parse error at
/// an ordinary position — so a server that answers `except ValueError` with a
/// 400 keeps answering 400 rather than falling through to a 500.
///
/// The bound is on *memory amplification* — one input byte of `[` buys a heap
/// container — and on nothing else. Stack safety is not its job and must not
/// depend on it: neither parsing nor freeing the result recurses in Rust at any
/// depth (`value::tests::freeing_deeply_nested_values_never_recurses` is the
/// teardown half, on a deliberately small stack). This test runs on a small
/// stack too, so that a parser or teardown that started recursing again would
/// fail here rather than only on the machine with the least headroom.
#[test]
fn nesting_is_bounded_and_never_reaches_the_rust_stack() {
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            let deep = "[".repeat(MAX_DEPTH) + &"]".repeat(MAX_DEPTH);
            let parsed = parse(&deep).expect("MAX_DEPTH containers must still parse");
            // Freeing it is the other half, and it happens right here.
            drop(parsed);

            let deeper = "[".repeat(MAX_DEPTH + 2) + &"]".repeat(MAX_DEPTH + 2);
            let e = parse(&deeper).expect_err("past the limit");
            assert!(
                e.message
                    .starts_with("maximum nesting depth (10000) exceeded at position "),
                "got: {e}"
            );
            assert_eq!(e.class, Exc::ValueError);

            // A million opening brackets is the shape that overflows a
            // recursive parser. It must simply be an error — and the partial
            // structure built before the limit tripped has to be freed on the
            // way out, which is the teardown path again.
            let hostile = "[".repeat(1_000_000);
            assert!(parse(&hostile).is_err());

            // Same for the encoder: a value deeper than it will walk is an
            // error, and the value still has to be freed afterwards.
            let mut v = Value::List(OroList::new(Vec::new()));
            for _ in 0..MAX_DEPTH + 10 {
                v = Value::List(OroList::new(vec![v]));
            }
            assert!(stringify(&v, None).is_err());
        })
        .expect("spawn")
        .join()
        .expect("a recursive parse or teardown would have aborted, not unwound");
}

#[test]
fn stringify_matches_the_module_it_replaced() {
    let v = ok(r#"{"a":1,"b":[1,2,3],"c":"x\"y","d":null,"e":true,"f":1.5}"#);
    assert_eq!(
        stringify(&v, None).unwrap(),
        r#"{"a":1,"b":[1,2,3],"c":"x\"y","d":null,"e":true,"f":1.5}"#
    );
    let two = Value::Int(2);
    assert_eq!(
        stringify(&ok(r#"{"a":1,"b":[1,2]}"#), Some(&two)).unwrap(),
        "{\n  \"a\": 1,\n  \"b\": [\n    1,\n    2\n  ]\n}"
    );
    // An empty container never looks at `indent`, so it never reports one.
    assert_eq!(stringify(&ok("[]"), Some(&Value::str("x"))).unwrap(), "[]");
    assert_eq!(stringify(&ok("{}"), Some(&Value::str("x"))).unwrap(), "{}");
    assert_eq!(
        stringify(&ok("[1]"), Some(&Value::str("x")))
            .unwrap_err()
            .message,
        "unsupported operand type(s) for *: 'str' and 'str'"
    );
}

/// A structure too deep to write — or one that contains itself — reports the
/// same runaway recursion the Oro encoder reported. `RecursionError` is a
/// subclass of `RuntimeError`, so `except RuntimeError` still catches it. It
/// must not be a Rust stack overflow.
#[test]
fn stringify_is_bounded_too() {
    let mut v = Value::List(OroList::new(Vec::new()));
    for _ in 0..MAX_DEPTH + 10 {
        v = Value::List(OroList::new(vec![v]));
    }
    let e = stringify(&v, None).expect_err("past the limit");
    assert_eq!(e.message, "maximum recursion depth exceeded");
    assert_eq!(e.class, Exc::RecursionError);

    let cycle = OroList::new(Vec::new());
    cycle.borrow_mut().push(Value::List(cycle.clone()));
    assert_eq!(
        stringify(&Value::List(cycle.clone()), None)
            .unwrap_err()
            .message,
        "maximum recursion depth exceeded"
    );
    // Break the cycle so the test does not leak it into the next one.
    cycle.borrow_mut().clear();
}

/// Every diagnostic this module produces names its own class, at the site that
/// detected the fault. Nothing downstream reads the prose.
///
/// A parse fault is always a `ValueError`, whatever the document says: the
/// module is documented to raise one naming the offset, and a server that wraps
/// a body parse in `except ValueError` to answer 400 must keep catching all of
/// them — including a document whose *contents* spell another exception's
/// message, which is exactly what the old substring table could not promise.
#[test]
fn every_diagnostic_names_its_class() {
    assert_eq!(err_class("}"), Exc::ValueError);
    assert_eq!(err_class(r#"{"a" 1}"#), Exc::ValueError);
    assert_eq!(err_class(r#""timed out"x"#), Exc::ValueError);
    assert_eq!(
        err_class(r#"{"No such file or directory": }"#),
        Exc::ValueError
    );

    let nan = stringify(&Value::Float(f64::NAN), None).unwrap_err();
    assert_eq!(nan.class, Exc::ValueError);
    assert_eq!(nan.message, "nan is not JSON serializable");
    let inf = stringify(&Value::Float(f64::INFINITY), None).unwrap_err();
    assert_eq!(inf.class, Exc::ValueError);

    // An unsupported *type* is a TypeError; the value being unwritable is a
    // ValueError. `_stringify_value` has always drawn it there, and so does
    // CPython.
    let bad_type = stringify(&Value::Bool(true), None);
    assert!(bad_type.is_ok(), "bools are writable");
    let d = OroDict::new();
    let mut d = d;
    d.insert(Value::Int(1), Value::Int(2))
        .expect("int keys hash");
    let keys = stringify(&Value::Dict(Rc::new(RefCell::new(d))), None).unwrap_err();
    assert_eq!(keys.class, Exc::TypeError, "a non-str key is a TypeError");
}
