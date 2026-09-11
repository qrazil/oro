# The manual oracle for 71_collection_methods.oro: that file with Oro's
# collection methods written back as CPython's builtins — which is what each of
# those lines said before it left core/. Run it and diff against the reviewed
# .expected; an empty diff is the review.
#
#   python3 corpus/divergence/71_collection_methods.twin.py \
#     | diff - corpus/divergence/71_collection_methods.expected
#
# The translations, and nothing else differs:
#
#   xs.sum() / xs.min() / xs.max()   ->  sum(xs) / min(xs) / max(xs)
#   xs.sort_by(x => x, reverse=True) ->  sorted(xs, reverse=True)
#   d.keys().sort_by(k => k)         ->  sorted(d.keys())
#   b.to_list().sort_by(b => b)      ->  sorted(b) — a bytes is not a collection
#                                        in Oro, so the bridge is explicit

print(sum([1, 2, 3]))
xs = [99, 9, 6, 5, 4, 3, 2, 1, 1]
print(min(xs), max(xs))

d = {"a": 1, "b": 2, "c": 3}
print(sorted(d.keys()))
print(sorted(d.values()))
counts = {"x": 3, "y": 1, "z": 1}
print(sorted(counts.keys()))

cities = ["dallas", "austin", "houston"]
print(sorted(cities))

print(sorted([b"pear", b"Apple", b"apple", b"\xff", b""]))
bd = {b"alpha": 1, b"beta": 2, b"gamma": 3}
print(sorted(bd.keys()))
print(sorted(b"\x00\xc8\xff"))


class Counter:
    def __init__(self, n):
        self.n = n

    def up(self):
        for i in range(self.n):
            yield i


c = Counter(3)
print(sum(c.up()), max(c.up()), sorted(c.up(), reverse=True))
