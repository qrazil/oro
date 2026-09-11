# The manual oracle for 74_sort_in_place.oro: that file with Oro's
# `sort_in_place` written back as CPython's `list.sort(key=…)`. Run it and diff
# against the reviewed .expected; an empty diff is the review.
#
#   python3 corpus/divergence/74_sort_in_place.twin.py \
#     | sed -E 's/\bTrue\b/true/g; s/\bFalse\b/false/g; s/\bNone\b/null/g' \
#     | diff - corpus/divergence/74_sort_in_place.expected
#
# The translations, and nothing else differs:
#
#   xs.sort_in_place(f, reverse=True)  ->  xs.sort(key=f, reverse=True)
#   xs.sort_in_place(x => x)           ->  xs.sort()
#   xs.sort_by(f).reversed()           ->  list(reversed(sorted(xs, key=f)))
#   xs.first() / xs.last() / xs.take(n) ->  xs[0] / xs[-1] / xs[:n]
#   null                               ->  None
def second(p):
    return p[1]


print("--- it answers null, and sorts where it is")
xs = [3, 1, 2]
print(xs.sort())
print(xs)

print("--- every name for the list sees it, because it is the same storage")
ys = [5, 4, 6]
alias = ys
nested = [ys]
ys.sort()
print(ys, alias, nested)
print(len(ys), ys[0], ys[-1])

print("--- empty, and one element")
empty = []
empty.sort()
print(empty)
one = [7]
one.sort()
print(one)

print("--- stable, and stable in reverse")
ties = [("a", 2), ("b", 1), ("c", 2), ("d", 1)]
up = [("a", 2), ("b", 1), ("c", 2), ("d", 1)]
up.sort(key=second)
print(up)
down = [("a", 2), ("b", 1), ("c", 2), ("d", 1)]
down.sort(key=second, reverse=True)
print(down)
flipped = list(reversed(sorted(ties, key=second)))
print(flipped)

print("--- a key that is a native callable, and a destructuring key")
words = ["bbb", "a", "cc"]
words.sort(key=len)
print(words)
pairs = [("x", 3), ("y", 1), ("z", 2)]
pairs.sort(key=lambda p: p[1])
print(pairs)

print("--- keys that are containers, and a sort that keeps duplicates")
rows = [[2, "b"], [1, "a"], [2, "a"], [1, "b"]]
rows.sort(key=lambda r: r)
print(rows)
dupes = [3, 1, 3, 1, 2]
dupes.sort(reverse=True)
print(dupes)

print("--- sorting a long list, where the reordering is the whole of the work")
long = []
for i in range(50):
    long.append((i * 37) % 50)
long.sort()
print(long[0], long[-1], len(long))
print(long[:5])
