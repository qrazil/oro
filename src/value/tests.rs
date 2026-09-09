//! Value-model invariants.

use super::*;

/// `Value` is copied, cloned and pushed on the operand stack constantly, and
/// every list, tuple and dict entry is one. Two words is the design: a
/// discriminant plus a single pointer or inline `i64`. A variant that needs
/// more payload must box it (as `Big`, `Str` and `Bytes` do) rather than widen
/// every value in the program.
#[test]
fn value_is_two_words() {
    assert_eq!(
        std::mem::size_of::<Value>(),
        16,
        "Value grew — box the new variant's payload rather than widening every \
         value on the stack and in every container"
    );
}

#[test]
fn bytes_repr_matches_cpython() {
    // Printable ASCII stays literal; everything else is \xNN, including the
    // escapes `str` spells \a and \v.
    assert_eq!(Value::bytes(&b"ab c"[..]).repr(), "b'ab c'");
    assert_eq!(Value::bytes(&b"\x07\x0b\x00\x7f\x80\xff"[..]).repr(), "b'\\x07\\x0b\\x00\\x7f\\x80\\xff'");
    assert_eq!(Value::bytes(&b"tab\tnl\ncr\rbs\\"[..]).repr(), "b'tab\\tnl\\ncr\\rbs\\\\'");
    // A `'` alone flips the quote; both quotes present keeps `'` and escapes it.
    assert_eq!(Value::bytes(&b"it's"[..]).repr(), "b\"it's\"");
    assert_eq!(Value::bytes(&b"say \"hi\""[..]).repr(), "b'say \"hi\"'");
    assert_eq!(Value::bytes(&b"both ' and \""[..]).repr(), "b'both \\' and \"'");
}

#[test]
fn bytes_order_and_equality_are_lexicographic() {
    assert!(Value::bytes(&b"abc"[..]).equals(&Value::bytes(&b"abc"[..])));
    // A `bytes` never equals the `str` that would decode to it.
    assert!(!Value::bytes(&b"abc"[..]).equals(&Value::str("abc")));
    assert_eq!(
        Value::bytes(&b"abc"[..]).compare(&Value::bytes(&b"abd"[..])).unwrap(),
        std::cmp::Ordering::Less
    );
    // Ordering is by octet, so the whole non-ASCII range sorts above ASCII.
    assert_eq!(
        Value::bytes(&b"\xff"[..]).compare(&Value::bytes(&b"a"[..])).unwrap(),
        std::cmp::Ordering::Greater
    );
}

#[test]
fn bytes_hash_as_dict_keys_and_never_collide_with_str() {
    let mut d = OroDict::new();
    d.insert(Value::bytes(&b"k"[..]), Value::Int(1)).unwrap();
    d.insert(Value::str("k"), Value::Int(2)).unwrap();
    assert_eq!(d.len(), 2);
    assert!(d.get(&Value::bytes(&b"k"[..])).unwrap().unwrap().equals(&Value::Int(1)));
}
