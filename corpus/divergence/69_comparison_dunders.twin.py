# The manual oracle for 69_comparison_dunders.oro: that file with Oro's
# collection-method spellings translated back into CPython's builtins. Run it
# and diff against the reviewed .expected; an empty diff is the review.
#
#   python3 corpus/divergence/69_comparison_dunders.twin.py \
#     | sed -E 's/\bTrue\b/true/g; s/\bFalse\b/false/g; s/\bNone\b/null/g' \
#     | diff - corpus/divergence/69_comparison_dunders.expected
#
# The translations, and nothing else differs:
#
#   xs.sort_by(x => x)                ->  sorted(xs)
#   xs.sort_by(f, reverse=True)       ->  sorted(xs, key=f, reverse=True)
#   xs = xs.sort(f, reverse=True) ->  xs.sort(key=f, reverse=True)
#   xs.min() / xs.max()               ->  min(xs) / max(xs)
#   true / false / null               ->  True / False / None
#
# `min(V(2), V(1), V(3))` is the same on both sides: the variadic scalar form is
# the half of `min` Oro keeps.


class Cell:
    def __init__(self, r):
        self.r = r

    def __eq__(self, other):
        return self.r == other.r

    def __repr__(self):
        return f"Cell({self.r})"


a = Cell(1)
b = Cell(1)
c = Cell(2)

print("--- the operator, and everywhere else")
print(a == b, a == c)
print(a != b, a != c)
print(a in [b], a in [c], a not in [c])
print(a in (b,), a in (c,))
print([a] == [b], [a] == [c], [a] == [b, c])
print((a,) == (b,), (a, c) == (b, c))
print({"k": a} == {"k": b}, {"k": a} == {"k": c})

print("--- nested arbitrarily")
print([[a]] == [[b]])
print([(a, [b])] == [(b, [a])])
print({"x": [{"y": (a,)}]} == {"x": [{"y": (b,)}]})
print([[a]] in [[[b]]])
print([{"k": [a]}] == [{"k": [b]}])

print("--- the reflected operand")
# CPython asks the right operand when the left has nothing to say, so a class
# with `__eq__` decides against a plain int from either side.
class Half:
    def __init__(self, n):
        self.n = n

    def __eq__(self, other):
        return self.n == other

    def __repr__(self):
        return f"Half({self.n})"


h = Half(7)
print(h == 7, 7 == h)
print(h in [7], 7 in [h])
print([h] == [7], [7] == [h])
# `!=` reflects to `__ne__`, and only then to the negation of `__eq__`. Taking
# the shortcut straight to `__eq__` answers `{} != h` with what `h.__eq__` said
# instead of with its negation.
print(h != 7, 7 != h, {} != h, [] != h)
print(h != 8, 8 != h)

print("--- identity is a shortcut inside a container, and never at `==`")
# `a == a` runs __eq__ and believes it; `a in [a]` does not ask at all. Four
# lines that have to disagree, and a language that gets this wrong either
# hangs on a self-referential list or contradicts itself.
class Never:
    def __eq__(self, other):
        return False

    def __repr__(self):
        return "Never()"


n = Never()
print(n == n)
print(n in [n], [n] == [n], (n,) == (n,), {"k": n} == {"k": n})

# The shortcut has to be a loop and not a recursion: one list of many
# references to a single object settles every element without asking anything,
# and a Rust frame per element would be a stack overflow rather than an answer.
wide = []
for i in range(20000):
    wide.append(n)
other = wide[0:20000]
print(len(wide), wide == other, n in wide)

print("--- __eq__ may answer with anything, and `==` hands it back unconverted")
class Truthy:
    def __eq__(self, other):
        return [1, 2, 3]

    def __repr__(self):
        return "Truthy()"


class Falsy:
    def __eq__(self, other):
        return []

    def __repr__(self):
        return "Falsy()"


print(Truthy() == 1)
print(Truthy() in [1], 1 in [Truthy()], [Truthy()] == [1])
print(Falsy() == 1)
print(Falsy() in [1], [Falsy()] == [1])

print("--- __ne__ is its own dunder, and containers do not consult it")
class Ne:
    def __ne__(self, other):
        return "not equal"

    def __repr__(self):
        return "Ne()"


ne = Ne()
print(ne != 1)
print(ne == 1)
print([ne] != [1], [ne] == [1])

print("--- the operator the program wrote is the one the TypeError names")
for op in ["lt", "gt", "le", "ge"]:
    try:
        if op == "lt":
            print(1 < {})
        elif op == "gt":
            print(1 > {})
        elif op == "le":
            print([1] <= {})
        else:
            print(Half(1) >= 2)
    except TypeError as e:
        print(e)

print("--- ordering: __lt__ is what sorts")
class V:
    def __init__(self, n):
        self.n = n

    def __lt__(self, other):
        return self.n < other.n

    def __repr__(self):
        return f"V({self.n})"


vs = [V(3), V(1), V(4), V(1), V(5)]
print(V(1) < V(2), V(2) < V(1), V(2) > V(1))
print(sorted(vs))
print(sorted(vs, reverse=True))
print(min(vs), max(vs))
print(min(V(2), V(1), V(3)), max(V(2), V(1), V(3)))
mutable = [V(3), V(1), V(2)]
mutable.sort()
print(mutable)
mutable.sort(reverse=True)
print(mutable)


def itself(v):
    return v


print(sorted(vs, key=itself))
print(sorted(vs, key=itself, reverse=True))
print(min([[V(2)], [V(1)]]), max([[V(2)], [V(1)]]))

print("--- a sort that needs __lt__ is still stable")
class Grade:
    def __init__(self, k, tag):
        self.k = k
        self.tag = tag

    def __lt__(self, other):
        return self.k < other.k

    def __repr__(self):
        return f"{self.k}{self.tag}"


ties = [Grade(1, "a"), Grade(0, "b"), Grade(1, "c"), Grade(0, "d"), Grade(1, "e")]
print(sorted(ties))
print(sorted(ties, reverse=True))

print("--- ordering nested in containers")
class W:
    def __init__(self, n):
        self.n = n

    def __eq__(self, other):
        return self.n == other.n

    def __lt__(self, other):
        return self.n < other.n

    def __repr__(self):
        return f"W({self.n})"


print([W(1)] < [W(2)], [W(2)] < [W(1)])
print([W(1), W(3)] < [W(1), W(4)])
print([W(1)] < [W(1), W(0)])
print((W(1), W(2)) < (W(1), W(3)))
print(sorted([[W(2)], [W(1)], [W(1), W(0)]]))

print("--- the reflected ordering dunder")
class OnlyGt:
    def __init__(self, n):
        self.n = n

    def __gt__(self, other):
        return self.n > other.n

    def __repr__(self):
        return f"OnlyGt({self.n})"


print(sorted([OnlyGt(2), OnlyGt(1)]))

print("--- a class with no ordering is still not orderable")
class Plain:
    def __repr__(self):
        return "Plain()"


for thunk in ["sorted", "min", "max", "lt", "nested"]:
    try:
        if thunk == "sorted":
            print(sorted([Plain(), Plain()]))
        elif thunk == "min":
            print(min([Plain(), Plain()]))
        elif thunk == "max":
            print(max([Plain(), Plain()]))
        elif thunk == "lt":
            print(Plain() < Plain())
        else:
            print([Plain()] < [Plain()])
    except TypeError as e:
        print(e)

print("--- an exception inside a dunder propagates")
class Boom:
    def __eq__(self, other):
        raise ValueError("boom-eq")

    def __lt__(self, other):
        raise ValueError("boom-lt")

    def __repr__(self):
        return "Boom()"


try:
    print(1 in [Boom(), 2])
except ValueError as e:
    print(e)
try:
    print([Boom()] == [1])
except ValueError as e:
    print(e)
try:
    print(sorted([Boom(), Boom()]))
except ValueError as e:
    print(e)
try:
    print(min([Boom(), Boom()]))
except ValueError as e:
    print(e)

# The job stack must be intact afterwards: a comparison abandoned mid-flight
# used to be able to leave one behind.
print(a in [b], sorted(vs))

print("--- a cycle is a RecursionError, not a hang and not a crash")
x = []
x.append(x)
y = []
y.append(y)
print(x == x)
try:
    print(x == y)
except RecursionError as e:
    print(e)
try:
    print(x in [y])
except RecursionError as e:
    print(e)
print(x == x, a in [b])
