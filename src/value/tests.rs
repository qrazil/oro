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

/// Identity semantics, at the `Value` level: a reference type is equal to
/// itself and to nothing else, and it is a dict key.
///
/// Everything reachable from Oro is covered by the corpus; this pins the two
/// properties the type system cannot — that identity is by *allocation* rather
/// than by content, and that two `Rc`s of one object agree.
#[test]
fn reference_types_are_equal_to_themselves_and_hashable() {
    // Two modules with the *same name* — the content-equal pair that a
    // "compare what is inside" shortcut would wrongly fold into one.
    let module = || {
        Value::Module(Rc::new(Module {
            name: Rc::from("m"),
            members: RefCell::new(HashMap::new()),
        }))
    };
    let a = module();
    let b = module();
    assert_eq!(a.try_equals(&a), Some(true), "an object must equal itself");
    assert_eq!(a.try_equals(&a.clone()), Some(true), "a clone is a refcount bump, not a new object");
    assert_eq!(a.try_equals(&b), Some(false), "two objects with identical contents are two objects");

    let mut d = OroDict::new();
    d.insert(a.clone(), Value::Int(1)).unwrap();
    d.insert(b.clone(), Value::Int(2)).unwrap();
    assert_eq!(d.len(), 2, "two distinct objects are two keys");
    assert_eq!(d.get(&a).unwrap().unwrap().try_equals(&Value::Int(1)), Some(true));
    assert_eq!(d.get(&b).unwrap().unwrap().try_equals(&Value::Int(2)), Some(true));

    // Identity is by allocation, and a value type never has one.
    assert!(a.identity().is_some());
    assert!(Value::Int(1).identity().is_none());
    assert!(Value::str("s").identity().is_none());
    assert!(Value::Tuple(Rc::new(vec![Value::Int(1)])).identity().is_none());
}

/// A `range` is a sequence, and CPython compares it as one: same length, same
/// values. `step` stops mattering below two elements, and `start` below one.
#[test]
fn ranges_compare_and_hash_as_the_sequence_they_denote() {
    let r = |start, stop, step| Value::Range(Rc::new(RangeVal { start, stop, step }));
    assert_eq!(r(0, 3, 1).try_equals(&r(0, 3, 1)), Some(true));
    // Two empty ranges are equal however they got there.
    assert_eq!(r(0, 0, 1).try_equals(&r(2, 2, 7)), Some(true));
    // One element: `step` is unobservable.
    assert_eq!(r(1, 2, 1).try_equals(&r(1, 2, 5)), Some(true));
    // Different lengths, and same length with a different step.
    assert_eq!(r(1, 4, 1).try_equals(&r(1, 4, 2)), Some(false));
    assert_eq!(r(0, 4, 2).try_equals(&r(0, 4, 3)), Some(false));

    // Hashing agrees with all of it, or a dict would lose keys.
    let mut d = OroDict::new();
    d.insert(r(0, 3, 1), Value::Int(1)).unwrap();
    d.insert(r(2, 2, 7), Value::Int(2)).unwrap();
    d.insert(r(0, 0, 1), Value::Int(3)).unwrap();
    assert_eq!(d.len(), 2, "the two empty ranges are one key");
    assert_eq!(d.get(&r(0, 3, 1)).unwrap().unwrap().try_equals(&Value::Int(1)), Some(true));
    assert_eq!(d.get(&r(0, 0, 1)).unwrap().unwrap().try_equals(&Value::Int(3)), Some(true));
}

/// The two things that stay unhashable, and the one that newly is not.
#[test]
fn mutable_containers_stay_unhashable() {
    let mut d = OroDict::new();
    let list = Value::List(Rc::new(RefCell::new(vec![Value::Int(1)])));
    let dict = Value::Dict(Rc::new(RefCell::new(OroDict::new())));
    for v in [list, dict] {
        let e = d.insert(v.clone(), Value::Int(0)).expect_err("must not be a key");
        assert!(e.contains("unhashable type"), "got: {e}");
    }
}

/// `OroDict::remove` splices an entry out of a compact array and slides every
/// index past it down one. The corpus checks the answers against CPython; this
/// pins the invariant underneath them — that the index still points at the
/// right entry afterwards, which is the thing a shifted `Vec` breaks silently.
#[test]
fn dict_remove_keeps_insertion_order_and_the_index_honest() {
    let mut d = OroDict::new();
    for i in 0..5 {
        d.insert(Value::Int(i), Value::Int(i * 10)).unwrap();
    }
    assert_eq!(d.remove(&Value::Int(2)).unwrap().unwrap().try_equals(&Value::Int(20)), Some(true));
    assert_eq!(d.len(), 4);
    // Every surviving key still resolves to its own value, including the three
    // that moved down a slot.
    for i in [0, 1, 3, 4] {
        let got = d.get(&Value::Int(i)).unwrap().expect("key survived the removal");
        assert_eq!(got.try_equals(&Value::Int(i * 10)), Some(true), "key {i}");
    }
    let order: Vec<i64> = d.items().iter().map(|(k, _)| match k {
        Value::Int(i) => *i,
        other => panic!("expected int key, got {}", other.repr()),
    }).collect();
    assert_eq!(order, vec![0, 1, 3, 4], "insertion order survives a removal from the middle");

    // A miss answers `None` rather than erroring, which is what lets the
    // two-argument `pop` hand back its default.
    assert!(d.remove(&Value::Int(2)).unwrap().is_none());
    // An unhashable key is still an error, not a miss.
    let list = Value::List(Rc::new(RefCell::new(vec![])));
    assert!(d.remove(&list).is_err());
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
    assert_eq!(Value::bytes(&b"abc"[..]).try_equals(&Value::bytes(&b"abc"[..])), Some(true));
    // A `bytes` never equals the `str` that would decode to it.
    assert_eq!(Value::bytes(&b"abc"[..]).try_equals(&Value::str("abc")), Some(false));
    assert_eq!(
        Value::bytes(&b"abc"[..]).try_compare(&Value::bytes(&b"abd"[..]), "<").unwrap().unwrap(),
        std::cmp::Ordering::Less
    );
    // Ordering is by octet, so the whole non-ASCII range sorts above ASCII.
    assert_eq!(
        Value::bytes(&b"\xff"[..]).try_compare(&Value::bytes(&b"a"[..]), "<").unwrap().unwrap(),
        std::cmp::Ordering::Greater
    );
}

#[test]
fn bytes_hash_as_dict_keys_and_never_collide_with_str() {
    let mut d = OroDict::new();
    d.insert(Value::bytes(&b"k"[..]), Value::Int(1)).unwrap();
    d.insert(Value::str("k"), Value::Int(2)).unwrap();
    assert_eq!(d.len(), 2);
    assert_eq!(d.get(&Value::bytes(&b"k"[..])).unwrap().unwrap().try_equals(&Value::Int(1)), Some(true));
}
