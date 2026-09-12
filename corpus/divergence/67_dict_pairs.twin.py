# The manual oracle for 67_dict_pairs.oro: that file with `.items()` written
# back in, so the baseline below is still CPython's output for everything
# except the divergence the file exists to record. Run it and diff against the
# reviewed .expected; an empty diff is the review.
#
#   python3 corpus/divergence/67_dict_pairs.twin.py \
#     | diff - corpus/divergence/67_dict_pairs.expected
#
# The translations, and nothing else differs:
#
#   for k, v in d            ->  for k, v in d.items()          (the divergence)
#   for pair in d            ->  for pair in d.items()
#   pos(*d)                  ->  pos(*d.items())
#   d.keys() / d.values()    ->  list(...) — Oro's answer a list, which is its
#                                own divergence, recorded in 59_dict_views.oro
#   d.to_list()              ->  list(d.items())
#   d.sort_by(p => p)        ->  dict(sorted(d.items())) — `sort_by` rebuilds the
#                                shape it was handed, and a sorted sequence of
#                                entries is a dict
#   d.min() / d.max()        ->  min(...) / max(...) — the builtins are cut; a
#                                builtin takes scalars, a collection method
#                                takes a collection
#   d.map / d.filter         ->  the dict comprehension each one is
#   true / false             ->  `b()` below. Oro spells the two literals
#                                lowercase; this is the same rename the corpus
#                                oracle applies to core/, done by hand here.
#
# Three things CPython cannot produce at all, and each is stated where it
# appears below:
#
#   * the `.items()` AttributeError message — the record of the cut itself;
#   * `for k, v in {}` printing nothing, which is the same in both, but the
#     twin keeps the line so the files stay in step;
#   * the mutation block. Oro's dict iterator walks a snapshot taken when the
#     loop starts, so an insert during the loop is neither seen nor an error.
#     CPython's iterator is live and raises `RuntimeError: dictionary changed
#     size during iteration`, so the twin spells the snapshot `list(m.items())`.
#     That difference is older than this file and is not what it records.


def b(x):
    return "true" if x else "false"


d = {"b": 2, "a": 1, "c": 3}

for k, v in d.items():
    print(k, v)

for pair in d.items():
    print(pair, pair[0], pair[1])

print(list(d.keys()))
seen = []
for k, v in d.items():
    seen.append(k)
print(b(seen == list(d.keys())))

print(b("a" in d), b("z" in d))
print(b(("a", 1) in d))
print(b("a" in list(d.keys())), b(("a", 1) in list(d.items())))

print(list(d.keys()), list(d.values()))
for k in d.keys():
    print("key", k)

# The one line with no CPython counterpart: `dict.items` exists there. What is
# printed is Oro's message for the cut, quoted.
print(
    "`dict.items()` is not in Oro — iterating a dict already yields its "
    "(key, value) pairs, so write `for k, v in d`; `d.to_list()` is the list of pairs"
)

print(list(d.items()))
print(dict(sorted(d.items())))
print(min(d.items()), max(d.items()))
print({k: v * 10 for k, v in d.items()})
print({k: v for k, v in d.items() if v > 1})


def named(a=0, b=0, c=0):
    print(a, b, c)


def positional(first, second, third):
    print(first, second, third)


named(**d)
positional(*d.items())

for k, v in {}.items():
    print("never")
print("empty ok")

m = {"a": 1, "b": 2}
for k, v in list(m.items()):
    m["c"] = 3
    m["b"] = 20
    print(k, v, m[k])
print(m)
