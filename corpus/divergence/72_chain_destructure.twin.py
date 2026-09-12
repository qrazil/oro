# The manual oracle for 72_chain_destructure.oro: that file with every chain
# written as the comprehension or loop it is defined to match. A destructuring
# callback becomes a `for` target list; a one-parameter callback becomes a
# plain loop variable. Run it and diff against the reviewed .expected; an
# empty diff is the review.
#
# This file prints Python's `True`, so its output goes through the same
# outbound rename `oracle.sh` applies before it is the expectation:
#
#   python3 corpus/divergence/72_chain_destructure.twin.py \
#     | sed -E 's/\bTrue\b/true/g; s/\bFalse\b/false/g; s/\bNone\b/null/g' \
#     | diff - corpus/divergence/72_chain_destructure.expected

words = ["ant", "bee", "cat", "dog"]
counts = [3, 1, 4, 1]

print("--- an index in a chain is range(len(xs)).zip(xs)")
print([f"{i}:{w}" for i, w in zip(range(len(words)), words)])
print([w for i, w in zip(range(len(words)), words) if i % 2 == 0])

print("--- zip")
pairs = list(zip(words, counts))
print([w * n for w, n in pairs if n > 1])
print(any(n == 4 for w, n in pairs), all(n > 0 for w, n in pairs), sum(1 for w, n in pairs if n == 1))


def sort_key(t):
    n, w = t
    return (n, w)


print(sorted([(n, w) for w, n in pairs], key=sort_key))
print([x for w, n in pairs for x in [w] * n])
acc = 0
for w, n in pairs:
    acc = acc + len(w) * n
print(acc)
print(next(((w, n) for w, n in pairs if n > 3), None))
print([f"{i}{w}{n}" for w, n, i in zip(words, counts, range(4))])

print("--- a dict's pairs, as a list")
stock = {"apples": 3, "pears": 0, "plums": 7}
print([f"{k}={v}" for k, v in stock.items() if v > 0])
print(min(stock.items(), key=lambda kv: kv[1]), max(stock.items(), key=lambda kv: kv[1]))

print("--- group_by")
orders = [
    {"region": "eu", "total": 30},
    {"region": "us", "total": 50},
    {"region": "eu", "total": 20},
]
by_region = {}
for o in orders:
    by_region.setdefault(o["region"], []).append(o)
print([(region, sum(r["total"] for r in rows)) for region, rows in by_region.items()])
print({region: len(rows) for region, rows in by_region.items()})

print("--- one parameter: the element whole, on every shape")
print([p[0] + p[1] for p in [(1, 2), (3, 4)]])
print(tuple(p for p in ((1, 2), (3, 4)) if p[0] > 1))
print(dict(p for p in stock.items() if p[1] > 0))
print(dict((p[0], p[1] * 2) for p in stock.items()))
acc = 0
for p in stock.items():
    acc = acc + p[1]
print(acc)
print([x for x in range(4) if x % 2 == 1])

print("--- which parameters count")


def label(k, v, sep="="):
    return k + sep + str(v)


def scaled(x, by=10):
    return x * by


def pair_len(item):
    return len(item)


class Shelf:
    def __init__(self, name):
        self.name = name

    def line(self, k, v):
        return self.name + ":" + k


shelf = Shelf("s")
print([label(k, v) for k, v in stock.items()])
print([scaled(x) for x in counts])
print([pair_len(p) for p in stock.items()])
print([shelf.line(k, v) for k, v in stock.items()])

print("--- the unpacking is for's")
print([b + a for a, b in ["xy", "ab"]])
print([a * b for a, b in [[1, 2], [3, 4]]])


def attempt(thunk):
    try:
        thunk()
    except ValueError as e:
        print(type(e), e)


attempt(lambda: [a for a, b, c in [(1, 2)]])
attempt(lambda: [a for a, b in [t for t in [(1, 2, 3)] if True]])
