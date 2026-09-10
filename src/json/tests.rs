//! Unit tests for the JSON codec.
//!
//! The behavioural baseline is `corpus/divergence/36_json.oro`, which pins the
//! exact accepted inputs, rejected inputs and diagnostics this replaced, and
//! passes unmodified. What is here is what a corpus program cannot reach: the
//! two stack disciplines (a document deeper than the Rust stack could carry,
//! and a value deeper than an Oro frame stack could carry), the character-vs-
//! byte offset agreement that only shows up on non-ASCII input, and the
//! classification table.

use super::*;

fn ok(text: &str) -> Value {
    parse(text).unwrap_or_else(|e| panic!("{text:?} should parse: {e}"))
}

fn err(text: &str) -> String {
    parse(text).expect_err(&format!("{text:?} should not parse"))
}

#[test]
fn scalars_and_containers() {
    assert_eq!(ok("null").repr(), "null");
    assert_eq!(ok("  \n\t [ 1 , 2 ,3]\r\n ").repr(), "[1, 2, 3]");
    assert_eq!(ok(r#"{"a": {"b": [1, [2, 3], {}]}}"#).repr(), "{'a': {'b': [1, [2, 3], {}]}}");
    // A repeated key takes the last value at the first key's position, which is
    // `OroDict`'s rule for `d[k] = v` and CPython's for `json.loads`.
    assert_eq!(ok(r#"{"a":1,"b":2,"a":3}"#).repr(), "{'a': 3, 'b': 2}");
}

#[test]
fn integers_promote_past_i64() {
    assert_eq!(ok("9223372036854775807").repr(), "9223372036854775807");
    assert_eq!(ok("9223372036854775808").repr(), "9223372036854775808");
    assert_eq!(ok("-9223372036854775809").repr(), "-9223372036854775809");
    assert_eq!(ok("123456789012345678901234567890").repr(), "123456789012345678901234567890");
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
    for text in [r#""\ud83d""#, r#""\ude00""#, r#""\ud83d\ud83d""#, r#""\ud83dA""#] {
        assert_eq!(err(text), "unpaired UTF-16 surrogate in \\u escape at position 7");
    }
    // The recovery path: a high surrogate followed by something that starts
    // like a pair and is not one must rewind to the high surrogate's own
    // position, not report where the second escape ended.
    assert_eq!(err(r#""\ud83d\u0041""#), "unpaired UTF-16 surrogate in \\u escape at position 7");
    // ...but a *malformed* second escape is reported as malformed.
    assert_eq!(err(r#""\ud83d\uZZZZ""#), "invalid \\u escape 'ZZZZ' at position 9");
}

/// Every diagnostic counts characters, because the parser this replaced indexed
/// the text by character. Byte offsets and character offsets only disagree on
/// non-ASCII input, so nothing else in the suite would catch a slip.
#[test]
fn positions_are_character_offsets() {
    assert_eq!(err("[\u{4e2d}]"), "unexpected '\u{4e2d}' at position 1");
    assert_eq!(err("[\"\u{1f600}\u{e9}\", }"), "unexpected '}' at position 7");
    assert_eq!(err("{\"\u{e9}\u{4e2d}\": }"), "unexpected '}' at position 7");
    // A non-ASCII character where four hex digits were promised is quoted
    // whole, and the "four characters remain" test counts characters too.
    assert_eq!(err("\"\\u\u{e9}\u{e9}\u{e9}\u{e9}\""), "invalid \\u escape '\u{e9}\u{e9}\u{e9}\u{e9}' at position 3");
    assert_eq!(err("\"\\u\u{e9}\u{e9}\""), "incomplete \\u escape at position 3");
}

/// Nesting is bounded, and the bound is reported as an ordinary parse error at
/// an ordinary position — so a server that answers `except ValueError` with a
/// 400 keeps answering 400 rather than falling through to a 500.
#[test]
fn nesting_is_bounded_and_never_reaches_the_rust_stack() {
    let deep = "[".repeat(MAX_DEPTH) + &"]".repeat(MAX_DEPTH);
    assert!(parse(&deep).is_ok(), "{MAX_DEPTH} containers must still parse");
    let deeper = "[".repeat(MAX_DEPTH + 2) + &"]".repeat(MAX_DEPTH + 2);
    let e = parse(&deeper).expect_err("past the limit");
    assert!(e.starts_with("maximum nesting depth (10000) exceeded at position "), "got: {e}");
    assert_eq!(classify(&e), Some("ValueError"));
    // A million opening brackets is the shape that would have overflowed a
    // recursive parser's stack; it must simply be an error.
    let hostile = "[".repeat(1_000_000);
    assert!(parse(&hostile).is_err());
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
        stringify(&ok("[1]"), Some(&Value::str("x"))).unwrap_err(),
        "unsupported operand type(s) for *: 'str' and 'str'"
    );
}

/// A structure too deep to write — or one that contains itself — reports the
/// same runaway recursion the Oro encoder reported, so `except RuntimeError`
/// still catches it. It must not be a Rust stack overflow.
#[test]
fn stringify_is_bounded_too() {
    let mut v = Value::List(Rc::new(RefCell::new(Vec::new())));
    for _ in 0..MAX_DEPTH + 10 {
        v = Value::List(Rc::new(RefCell::new(vec![v])));
    }
    let e = stringify(&v, None).expect_err("past the limit");
    assert_eq!(e, "maximum recursion depth exceeded");
    assert_eq!(classify(&e), None, "the VM's own table answers this one");

    let cycle = Rc::new(RefCell::new(Vec::new()));
    cycle.borrow_mut().push(Value::List(cycle.clone()));
    assert_eq!(
        stringify(&Value::List(cycle.clone()), None).unwrap_err(),
        "maximum recursion depth exceeded"
    );
    // Break the cycle so the test does not leak it into the next one.
    cycle.borrow_mut().clear();
}

#[test]
fn classification_is_owned_here() {
    assert_eq!(classify("unexpected '}' at position 14"), Some("ValueError"));
    assert_eq!(classify("nan is not JSON serializable"), Some("ValueError"));
    assert_eq!(classify("Infinity is not JSON serializable"), Some("ValueError"));
    assert_eq!(
        classify("object of type <class 'set'> is not JSON serializable"),
        Some("TypeError")
    );
    // `keys must be str, not …` is left to the VM's table, which already sends
    // "must be str" to TypeError; claiming it here would be a second answer to
    // a question that has one.
    assert_eq!(classify("keys must be str, not <class 'int'>"), None);
    assert_eq!(classify("something else entirely"), None);
}
