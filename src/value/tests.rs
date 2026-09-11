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
    assert!(Value::Tuple(OroTuple::new(vec![Value::Int(1)])).identity().is_none());
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
    let list = Value::List(OroList::new(vec![Value::Int(1)]));
    let dict = Value::Dict(Rc::new(RefCell::new(OroDict::new())));
    for v in [list, dict] {
        let e = d.insert(v.clone(), Value::Int(0)).expect_err("must not be a key");
        assert!(e.message.contains("unhashable type"), "got: {e}");
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
    let list = Value::List(OroList::new(vec![]));
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

/// Freeing a value must not recurse in Rust, for *any* nesting depth.
///
/// This is not a performance property, it is a safety one: a recursive teardown
/// overflows the stack and `abort()`s — no panic, no Oro exception, nothing a
/// server can catch — and the depth is chosen by whoever built the value, which
/// on a socket is an anonymous client.
///
/// A depth *limit* cannot substitute for this. The depth a recursive drop
/// survives depends on the frame size the optimizer happened to produce: before
/// this change, a 10 000-deep list freed fine under `--release` and aborted the
/// test runner under `cargo test`, on a thread with a 2 MiB stack. So the
/// numbers here are deliberately absurd — far past any limit `json` or anything
/// else imposes — because a test that a constant happens to clear is a test of
/// the constant, not of the property.
///
/// Each case runs on a thread with a *small* stack, so that a regression fails
/// here rather than only on whichever machine has the least headroom.
#[test]
fn freeing_deeply_nested_values_never_recurses() {
    fn on_a_small_stack(f: impl FnOnce() + Send + 'static) {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(f)
            .expect("spawn")
            .join()
            .expect("a recursive teardown would have aborted, not unwound");
    }

    const DEEP: usize = 200_000;

    // Lists.
    on_a_small_stack(|| {
        let mut v = Value::List(OroList::new(Vec::new()));
        for _ in 0..DEEP {
            v = Value::List(OroList::new(vec![v]));
        }
        drop(v);
    });

    // Tuples.
    on_a_small_stack(|| {
        let mut v = Value::Tuple(OroTuple::new(Vec::new()));
        for _ in 0..DEEP {
            v = Value::Tuple(OroTuple::new(vec![v]));
        }
        drop(v);
    });

    // Dicts, nested through the *value* side...
    on_a_small_stack(|| {
        let mut v = Value::None;
        for i in 0..DEEP {
            let mut d = OroDict::new();
            d.insert(Value::Int(i as i64), v).expect("int keys hash");
            v = Value::Dict(Rc::new(RefCell::new(d)));
        }
        drop(v);
    });

    // Instances, whose children live in a field map.
    on_a_small_stack(|| {
        let class = Rc::new(Class {
            name: Rc::from("Node"),
            base: None,
            members: RefCell::new(Fields::new()),
            is_exception: false,
        });
        let mut v = Value::None;
        for _ in 0..DEEP {
            let mut fields = Fields::new();
            fields.insert(Rc::from("next"), v);
            v = Value::Instance(Rc::new(Instance { class: class.clone(), fields: RefCell::new(fields) }));
        }
        drop(v);
    });

    // And the mixed chain, which is what a real document is: no single one of
    // the four appears twice in a row, so a fix that only broke one kind of
    // chain would still recurse here.
    on_a_small_stack(|| {
        let mut v = Value::None;
        for i in 0..DEEP / 4 {
            v = Value::List(OroList::new(vec![v]));
            v = Value::Tuple(OroTuple::new(vec![v]));
            let mut d = OroDict::new();
            d.insert(Value::Int(i as i64), v).expect("int keys hash");
            v = Value::Dict(Rc::new(RefCell::new(d)));
            v = Value::List(OroList::new(vec![v]));
        }
        drop(v);
    });
}

/// A container that is still shared must not be emptied when one reference to
/// it goes away — the teardown unlinks children only when it holds the *last*
/// reference, which is exactly when the drop glue would otherwise recurse.
#[test]
fn teardown_only_unlinks_the_last_reference() {
    let shared = OroList::new(vec![Value::Int(1), Value::Int(2)]);
    let a = Value::List(shared.clone());
    let b = Value::List(shared.clone());
    drop(a);
    assert_eq!(shared.borrow().len(), 2, "dropping one reference emptied the list");
    match &b {
        Value::List(l) => assert_eq!(l.borrow()[1].repr(), "2"),
        _ => unreachable!(),
    }
    drop(b);
    assert_eq!(Rc::strong_count(&shared), 1);
    assert_eq!(shared.borrow().len(), 2, "the last *Value* went, but the Rc here still holds it");

    // A dict entry is a pair, and *both* halves have to be released: a teardown
    // that unlinked only the values would leave keys to the recursive glue.
    // Shown by refcount rather than by depth, because a deep key cannot be
    // built — hashing a nested tuple recurses, which is a separate limit this
    // change does not touch (see the note in `mod teardown`).
    let key_items = OroTuple::new(vec![Value::Int(7)]);
    let mut d = OroDict::new();
    d.insert(Value::Tuple(key_items.clone()), Value::None).expect("a tuple of ints hashes");
    assert_eq!(Rc::strong_count(&key_items), 2);
    drop(Value::Dict(Rc::new(RefCell::new(d))));
    assert_eq!(Rc::strong_count(&key_items), 1, "the key half of the entry was not released");

    // Cycles are collected by neither scheme — Oro has no GC — but tearing one
    // down must still terminate rather than spin or recurse.
    let cycle = OroList::new(Vec::new());
    cycle.borrow_mut().push(Value::List(cycle.clone()));
    let outer = Value::List(OroList::new(vec![Value::List(cycle.clone())]));
    drop(outer);
    assert_eq!(Rc::strong_count(&cycle), 2, "the self-reference is what keeps it alive");
    cycle.borrow_mut().clear();
}
