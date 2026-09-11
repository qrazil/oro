# The manual oracle for 73_lists.oro: that file with the one spelling CPython
# does not have written back into CPython's. Run it and diff against the
# reviewed .expected; an empty diff is the review.
#
#   python3 corpus/divergence/73_lists.twin.py \
#     | sed -E 's/\bTrue\b/true/g; s/\bFalse\b/false/g; s/\bNone\b/null/g' \
#     | diff - corpus/divergence/73_lists.expected
#
# The one translation:
#
#   xs.sort_in_place(x => x)  ->  xs.sort()
xs = [3, 1, 4, 1, 5, 9, 2, 6]
print(len(xs))
print(xs[0], xs[-1])
print(xs[2:5])
xs.append(99)
print(xs)
xs.sort()
print(xs)
xs.reverse()
print(xs)
nested = [[1, 2], [3, 4]]
print(nested[1][0])
