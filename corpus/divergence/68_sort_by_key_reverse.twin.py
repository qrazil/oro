# The manual oracle for 68_sort_by_key_reverse.oro: that file with the one
# spelling CPython does not have written back into CPython's. Run it and diff
# against the reviewed .expected; an empty diff is the review.
#
#   python3 corpus/divergence/68_sort_by_key_reverse.twin.py \
#     | sed -E 's/\bTrue\b/true/g; s/\bFalse\b/false/g; s/\bNone\b/null/g' \
#     | diff - corpus/divergence/68_sort_by_key_reverse.expected
#
# The translations, plus the three literals:
#
#   xs.sort(f, reverse=True)  ->  sorted(xs, key=f, reverse=True)
#   xs.sort(x => x)           ->  sorted(xs)
#
def neg(x):
    return -x

def second(p):
    return p[1]

print(sorted([1, 3, 2], key=neg))
print(sorted([1, 3, 2], reverse=True))
print(sorted([1, 3, 2], key=neg, reverse=True))
print(sorted(["bbb", "a", "cc"], key=len))
rows = [("carol", 3), ("alice", 1), ("bob", 2)]
print(sorted(rows, key=second))
print(sorted(rows))
print(sorted([]), sorted([], key=neg))
ties = [("a", 1), ("b", 1), ("c", 0)]
print(sorted(ties, key=second))
print(sorted(ties, key=second, reverse=True))
