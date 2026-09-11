# `__hash__`, and what `__eq__` costs

*An evaluation. Status: nothing built, and the recommendation is that nothing
should be. Written after `is` was cut, which changes the argument — see §6.*

The state of things, stated as plainly as it deserves:

> Adding `__eq__` to a class silently costs you the ability to use its
> instances as dict keys, with no way to get it back.

That is CPython's rule with the escape hatch sawn off. CPython clears
`__hash__` when a class defines `__eq__`, and then lets the class define
`__hash__` back. Oro's dunder set is fixed and has no `__hash__`, so the loss
is permanent. Worse than permanent: it is *invisible at the definition site*.
The `class` statement compiles, the `__eq__` works, and the program fails three
files later at a `d[obj]` that has nothing to do with the decision that broke
it.

This document asks whether `__hash__` should exist, and answers no — but the
interesting part is not the answer, it is that the investigation turned up a
larger bug sitting underneath the question (§8) and an architectural wall that
decides the question on its own (§3).

---

## 1. The failure, measured

The canonical shape, written as a program someone would actually write:

```python
class Cell:
    def __init__(self, row, col):
        self.row = row
        self.col = col

    def __eq__(self, other):
        return self.row == other.row and self.col == other.col


print(Cell(1, 2) == Cell(1, 2))     # true — exactly what was wanted
board = {}
board[Cell(1, 2)] = "X"             # TypeError
```

```
TypeError: unhashable type: 'Cell' — it defines __eq__, so its identity is not
what equality means for it
```

The message is honest about *why*, which puts it ahead of CPython's bare
`unhashable type: 'Cell'`. It is not honest about *what to do instead*, which
puts it behind every other removal message in the language (`lstrip`, `zfill`,
`rsplit` all name their replacement). That gap is fixable and is part of the
recommendation.

Two further measurements, both worse than the headline:

**The chain surface fails too, and does not say why.** `unique` is implemented
over an internal dict, so a semantically pure equality operation dies on
hashability:

```python
cells = [Cell(1, 2), Cell(1, 2), Cell(3, 4)]
cells.unique()      # TypeError: unhashable type: 'Cell'
```

`unique_by`, `group_by` and `to_dict` are the same. Nothing in
`xs.unique()` suggests a hash table, so this arrives as a non-sequitur.

**`__hash__` is already accepted, and already ignored.** Today:

```python
class Cell:
    def __eq__(self, other): ...
    def __hash__(self):
        return self.row * 31 + self.col


print(Cell(1, 2).__hash__())   # 33 — it is an ordinary method and it runs
d = {}
d[Cell(1, 2)] = "X"            # TypeError, unchanged
```

The class compiles. The method is callable. It has no effect on anything. A
programmer reaching for CPython's escape hatch gets a class that looks like it
took the hatch and does not. `unsupported_dunder` in
`src/compiler/codegen.rs` already rejects `__new__`, `__getattr__`,
`__setattr__` and `__slots__` by name; `__hash__` was simply never added to
that list, and silently doing nothing is the failure mode Oro's whole
rejection convention exists to prevent.

---

## 2. How common is the shape?

"A class with value equality, used as a dict key" is common in Python and
*uncommon in Oro*, and the difference is not an accident.

**In Oro's own tree it is zero.** The standard library defines eight classes —
`_Parser`, `BadRequest`, `Request`, `Response`, `_HeadParser`, `_LimitReader`,
`_ChunkedReader`, `Router` — and not one defines `__eq__`. Every one of them
is an object with behaviour and a lifetime, not a value. Across `std/`,
`corpus/`, `bench/` and `tests/`, exactly three classes define `__eq__`, and
all three exist to exercise the dunder rather than to get work done — `Point`
in `core/18_classes.oro`, `Money` in `core/19_class_features.oro`, and the
`Point` in `divergence/55_identity_of_runtime_types.oro` whose only job is to
demonstrate that it is not a key.

The middle one is worth pausing on, because it is the shape this document is
about, written by hand and unprompted: `Money(cents)` with `__eq__`, `__lt__`,
`__le__` and the arithmetic dunders. It is a value type in every sense, it is
the most natural dict key in the file — a price to a count — and the corpus
program never keys by it. It does not work around the restriction; it simply
never reached for it — which is one data point, pointing the same way the
three that follow do.

**Where the tree does key by a composite, it uses a tuple.** `grid[(0, 0)]`,
`counts[("a", 1)]`, and `docs/stdlib-server-design.md`'s own example
`counts[(host, port)]`. The README already states the reason tuples survived
the cut of `set`: *"they are the only hashable composite, so `counts[(host,
port)]` has no substitute."* The idiom is in the language and is already the
one the codebase reaches for.

**Where a value needs canonicalising, the tree canonicalises to a value type.**
`std/http.oro` handles case-insensitive header lookup with
`self.headers.get(name.lower(), default)` — not with a `Header` class carrying
a case-folding `__eq__`. This is the shape a `__eq__`-plus-`__hash__` class
would take in Python, and Oro wrote it as a `dict` of lowercased `str`, which
is shorter, faster and needs no dunders at all.

**And in Python the hand-written version is already the rare one.** The common
Python spelling of a hashable value type is `@dataclass(frozen=True)` or
`typing.NamedTuple` — both of which *derive* `__hash__` from the same fields
`__eq__` compares, precisely because hand-writing the two in agreement is
error-prone enough that the standard library took the job away from users. Oro
has no decorators and no dataclasses. Its analogue of a frozen dataclass is the
tuple, and the tuple already hashes.

The shape is real, it is just not as common as the CPython habit suggests, and
Oro's idiom bends away from it rather than toward it.

---

## 3. What the machine can actually do

This section decides more of the question than the design arguments do, so it
comes before the options.

A dict key in Oro is an `HKey` — a pure Rust projection of a `Value`,
computed by `HKey::from_value` in `src/value.rs`. `OroDict` is a
`HashMap<HKey, usize>` beside an insertion-ordered `Vec` of entries. `HKey`
derives `PartialEq`, `Eq` and `Hash`.

Two consequences, and both are hard:

**1. A dict lookup never calls `__eq__`, and could not.** The `HashMap`
resolves collisions with `HKey`'s *derived* equality. There is no equality
callback anywhere in the dict path. So a `__hash__` on its own buys nothing at
all: `d[Cell(1, 2)]` would hash to the right bucket and then compare two
`HKey::Id` addresses and miss. To make a value class a working key you need
`__eq__` dispatched *inside* the lookup, not just `__hash__`.

**2. Neither dunder can be dispatched from there.** Oro's VM never recurses in
Rust. A native operation that needs to run Oro code — `print` calling
`__str__`, `sorted` calling a `key=` lambda, `==` calling `__eq__` — cannot
call it; it must push a frame, return control to the interpreter loop, and be
*resumed* when the frame returns. That machinery exists and is visible in
`ReturnAction` (`DrivePrint`, `DriveStr`, `DriveSort`, `DriveSeq`,
`NegateBool`, …), one variant plus a job struct per re-entrant operation.

Every place a program value gets hashed would need that treatment:

| entry point | where |
|---|---|
| `d[k]` | `Op::LoadSubscript` |
| `d[k] = v` | `Op::StoreSubscript` |
| `k in d` / `not in` | `Op::Compare` → `contains` |
| `{a: 1, **rest}` | `Op::BuildDict`, both arms |
| `match` dispatch | `Op::MatchDispatch`, an O(1) jump table keyed by `HKey` |
| `d.get(k, default=v)` | `dict_method` |
| `xs.unique()` | `builtins`, one hash per element |
| `xs.unique_by(f)` | `SeqOp::UniqueBy`, inside a job that is *already* re-entrant |
| `xs.group_by(f)` | `SeqOp::GroupBy`, likewise |
| `pairs.to_dict()` | `to_dict`, and `rebuild_shape` for every dict-preserving chain step |
| `d1 == d2` | `Value::equals`, which probes the other dict per key |
| the `match` table itself | built at *compile* time, in `codegen.rs` |

The last row is the one that ends it: the `match` jump table is a dict constant
built by the compiler. A user `__hash__` would mean the compiler cannot build
it, which means `match` loses the O(1) dispatch that is, per the README, "the
reason `match` earns its keep over `if`/`elif`".

The rest are not much better. `xs.unique()` over a thousand elements becomes a
thousand suspension points threaded through a resumable loop. `d[k]` — the
single hottest non-arithmetic operation in the language — grows a branch that
can suspend. And `Op::MatchDispatch` acquires the ability to run arbitrary user
code, including code that mutates the dict it is dispatching on.

This is not "expensive". It is a redesign of the dict, the chain protocol and
`match`, in service of one dunder.

---

## 4. The options

### (a) Add `__hash__` to the fixed dunder set

The CPython answer: `__eq__` clears hashability, `__hash__` restores it.

It does not work, for the reason in §3: `__hash__` alone gets you a lookup that
hashes with the user's function and then compares with the runtime's identity,
so `d[Cell(1, 2)]` misses a key that is present. Shipping that would be a
silent wrong answer, which is worse than the `TypeError` it replaces. To make
it work you need §4(d) as well, and then you need §3's whole redesign.

Its design cost, if the machine were free: it puts the invariant *"if `a == b`
then `hash(a) == hash(b)`"* on the user, unchecked and uncheckable, with a
failure mode of "the key is in the dict and the lookup misses". This is a
footgun CPython has and Oro currently does not, and a language whose thesis is
"one way to do each thing, and it is right" should think hard before importing
a hazard whose entire content is "two things you wrote must agree".

### (b) Leave it, and document what `__eq__` means

`__eq__` is a declaration: *this is a value type, its address is an
implementation detail, two of these with the same contents are the same
thing.* Oro's answer is that a value type with custom equality is not a key,
and that the key is the value it compares by:

```python
board[(c.row, c.col)] = "X"           # or
board[c.key()] = "X"                  # def key(self): return (self.row, self.col)
```

Cost: one method and one call site. The capability is not lost, which is the
test the README sets for a removal — *"a removal has to leave the capability
behind"* — and the key that results is a tuple, which prints, compares and
sorts on its own.

This is the status quo *plus* saying so, and the status quo is not currently
saying so anywhere.

### (c) Derive the hash from the fields `__eq__` compares

Impossible as stated, and Oro's class model does not rescue it. Instances store
attributes in an open `HashMap<Rc<str>, Value>` (`Instance::fields`) populated
by whatever `__init__` assigns; there is no declared field set — `__slots__` is
cut precisely because attributes are always a per-instance dict. And `__eq__`
is an arbitrary function body: it can call a helper, compare a normalised
projection, consult a class attribute, or be inherited from a base.

The only tractable version is to inspect `__eq__`'s AST and accept a restricted
shape (`return self.a == other.a and self.b == other.b`). That makes a
program's *hashability* depend on the syntactic form of a method body — the
same class, refactored to `return self._key() == other._key()`, silently stops
being a key. "You cannot tell what this does by reading it" is the reason
`__getattr__` and metaclasses are cut. Reject.

### (d) A declared key projection, `__key__`

The constructive option, and worth stating properly because it is the shape to
adopt if this is ever revisited.

Not a method — a class-level declaration of the fields that constitute the
value:

```python
class Cell:
    __key__ = ("row", "col")
```

From that one declaration the runtime derives *both* equality and the hash, so
the invariant in §4(a) holds by construction and cannot be violated: there is
one definition, not two that must agree. And — the part that matters — it needs
no VM re-entry at all. `hkey_cold` reads the named fields off `inst.fields` and
builds an `HKey::Tuple`; `Value::equals` reads the same fields natively. It
therefore works everywhere hashing and equality work, including the places
`__eq__` currently does not reach (§8).

Why it still loses, today:

- It is a **new feature**, and a class-level protocol is a larger surface than
  a dunder. The freeze starts at 1.0, so this is admissible in principle — but
  it must be paid for by a need, and §2 says the tree does not have one.
- It is a **second way to define equality**. `__eq__` stays, because `__key__`
  cannot express case-insensitivity, tolerance, or equality across types. So
  the language would have two equality declarations with different powers and
  different consequences for hashability, which is exactly the accretion the
  thesis rejects.
- **It does not remove the failure.** A class that needs a real `__eq__` is
  still not a key, and still finds out at an unrelated `d[obj]`. `__key__`
  gives the common case a nicer spelling than `(c.row, c.col)`; it does not
  change the rule.

Hold it in reserve. Do not build it on a hypothesis.

### (e) Make the dict honour `__eq__` properly

The honest version of (a): dispatch `__hash__` *and* `__eq__` from inside every
dict operation. §3 prices it: a resumable `d[k]`, a resumable `unique`, a
`match` that can run user code, and a compile-time jump table that can no
longer be built. Reject on cost alone, before any design argument.

### (f) Key by identity anyway, silently

Delete the check, hash a `__eq__`-defining instance by address. `d[Cell(1, 2)]`
would then miss a key stored under an equal `Cell(1, 2)`, with no error. This
is the one option strictly worse than every other, and it is named here only
because it is the tempting one: it makes the reported symptom go away.

---

## 5. What the message should say

Independent of the decision above, two diagnostics are wrong today and should
be fixed either way.

**`def __hash__` should be rejected at the class**, in `unsupported_dunder`,
where `__new__`, `__getattr__`, `__setattr__` and `__slots__` already are. It
is currently accepted, callable, and inert. Something like:

> `__hash__` is not in Oro's dunder set — a class that defines `__eq__` is a
> value type and is not a dict key; key by the value instead, e.g.
> `d[(self.row, self.col)]`

**The runtime message should name the replacement.** Today it explains the
cause and stops:

> unhashable type: 'Cell' — it defines __eq__, so its identity is not what
> equality means for it

Every other removal in the language names what to use instead. This one should
too, and the replacement is a tuple key.

That pair converts the failure from "invisible at the definition site" to
"stated at the definition site", which is most of the complaint in the opening
paragraph. It costs two message strings and one match arm.

---

## 6. Does cutting `is` make this better or worse?

Better, on net, and the reasoning is worth having in full because the
first-order effect is a loss.

**The loss.** `is` was the last way to ask an identity question about an
instance whose class defines `__eq__`. Given two `Cell`s you can no longer ask
whether they are the same object — not through `==` (which the class has
taken over), not through the dict (which refuses them), and now not through
`is`. That question is genuinely gone, and it was a real debugging tool for
exactly the aliasing bugs a mutable value class invites.

**Why it is nonetheless the better state.** For this *particular* class of
object, `is` was asking a question the type had declared meaningless. A `Cell`
that says "I equal any Cell with my row and column" has said that its address
is not part of its identity. An operator that answers by address is answering
a question about the runtime, not about the program — which is the same
complaint that removed `is` from strings.

**And it strengthens the case against `__hash__`, twice over.**

First, the language now has exactly *one* equality question. Before, it had two
surfaces that could disagree about the same pair of objects, and the dict's
"key by address" rule sat comfortably beside `is`, because identity was already
a thing the surface talked about. Now the dict is the *last* place where an
object's address would leak into program behaviour — and it would leak
invisibly, since nothing else can observe it. Refusing is the consistent
answer; hashing by address behind `__eq__`'s back would make the dict the only
identity semantics left in the language, and an unspoken one.

Second, and more bluntly: `is` was cut because it was a pair of surfaces whose
disagreement was invisible and implementation-shaped, and which the
implementation could not be made to fix. `__hash__` is a pair of surfaces whose
disagreement is invisible and implementation-shaped, and which the
implementation cannot even *detect* — the user owns it. Removing one such
hazard and adding a larger one in the same milestone is not a design, it is a
mood.

---

## 7. Decision

**Do not add `__hash__`.** Option (b), with the diagnostics from §5.

1. `__hash__` stays out of the dunder set.
2. Add `__hash__` to `unsupported_dunder` so `def __hash__` is a compile error
   naming the tuple-key replacement, instead of a method that runs and does
   nothing.
3. Extend the runtime `unhashable type` message to name the replacement,
   bringing it to the register of `lstrip` / `zfill` / `rsplit`.
4. State the rule in the README where `__eq__` is described: **`__eq__` is a
   declaration that this is a value type; a value type with custom equality is
   not a key, and the key is the value it compares by.**
5. Keep `__key__` (§4(d)) on the shelf. Revisit only if real Oro programs show
   the tuple-key idiom failing — not before.

The reasoning, compressed: the machine says no (§3), the tree does not want it
(§2), the capability survives without it (§4(b)), and the only version that
would actually work reintroduces the exact hazard class the previous milestone
removed (§6). The complaint in the opening paragraph is real, but its content
is *silence*, not absence — the language's answer is fine, it just never says
it. §5 is the fix, and it is two strings and a match arm.

---

## 8. The bug this investigation found

Not the subject of this document, and more important than it.

`__eq__` is dispatched by `Op::Compare` and nowhere else. Every other equality
path in the runtime goes through `Value::equals`, which for an instance falls
through to `identity_eq`. So:

```python
class Cell:
    def __init__(self, r):
        self.r = r

    def __eq__(self, other):
        return self.r == other.r


a = Cell(1)
b = Cell(1)
print(a == b)              # true
print(a in [b])            # false   ← CPython: True
print([a] == [b])          # false   ← CPython: True
print((a,) == (b,))        # false   ← CPython: True
print([b].any(y => y == a))  # true  ← disagrees with `a in [b]`, one line up
```

Four lines of one program giving two different answers to one question. This is
not a deliberate divergence, it is not in `corpus/divergence/`, and it is in
the computational core, where the README says surprise is pure cost. `a in xs`
and `xs.any(y => y == a)` are the same question in two spellings and they
disagree.

The same wall from §3 explains it: `contains` and `seq_eq` are native code and
cannot re-enter the VM to run `__eq__`. But unlike the dict, these have no
`match` table, no compile-time constant and no hot subscript path — `in` over a
list is already O(n) with a comparison per element, and `Op::Compare` already
knows how to suspend for `__eq__`. The cost of fixing it is a job struct and a
`ReturnAction`, the same shape `DriveSeq` already has.

The doc comment on `Value::equals` currently claims the opposite of the
measurement — *"instances included, where a `__eq__` dunder, when present, is
dispatched by the VM before this fallback is reached"* — which is true for `==`
and false for `in`, list equality, tuple equality and dict value comparison.

`__lt__` has the identical shape: `V(1) < V(2)` works, `sorted([V(2), V(1)])`
raises `'<' not supported between instances of 'object' and 'object'`.

Recommendation: file it, fix `in` / `seq_eq` / dict-value comparison first, and
add a `corpus/core/` program for it so CPython holds the answer. A missing
escape hatch is a design question. This is a wrong answer.
