# The manual oracle for 70_sort_by_shape.oro: that file with Oro's
# `xs.sort_by(...)` written back as CPython's `sorted(xs, ...)`. Run it and diff
# against the reviewed .expected; an empty diff is the review.
#
#   python3 corpus/divergence/70_sort_by_shape.twin.py \
#     | sed -E 's/\bTrue\b/true/g; s/\bFalse\b/false/g; s/\bNone\b/null/g' \
#     | diff - corpus/divergence/70_sort_by_shape.expected
#
# The translations:
#
#   xs.sort_by(f) / xs.sort_by(x => x)  ->  sorted(xs, key=f) / sorted(xs)
#   s.to_list().sort_by(x => x)         ->  sorted(s) — the bridge a str needs
#                                           now that it is outside the
#                                           collection protocol, and the one
#                                           capability the cut actually costs
#   true                                ->  True

print(sorted([3, 1, 2]))
print(sorted([]))
print(sorted("cab"))
print(sorted(range(3, 0, -1)))
print(sorted([3, 1, 2], reverse=True))


def neg(x):
    return -x


print(sorted([3, 1, 2], key=neg))


def gen():
    yield 3
    yield 1


print(sorted(gen()))
