# The manual oracle for 51_string_surface.oro: that file with Oro's spellings
# translated one for one into CPython's. Run it and diff against the reviewed
# .expected; an empty diff is the review.
#
# The one thing that is not a name: this file prints Python's `True`/`False`/
# `None`, so its output goes through the same outbound rename `oracle.sh`
# applies before it is the expectation.
#
#   python3 corpus/divergence/51_string_surface.twin.py \
#     | sed -E 's/\bTrue\b/true/g; s/\bFalse\b/false/g; s/\bNone\b/null/g' \
#     | diff - corpus/divergence/51_string_surface.expected

STRS = ["  hi  ", "xxhixx", "xyxhixyx", "", "xxx", "\thi\n ", "ααhiα"]
CUTS = ["x", "xy", "", "α", "abc"]

print("--- strip(side=...)")
for s in STRS:
    print(repr(s), [s.strip(), s.lstrip(), s.rstrip(), s.strip()])

print("--- strip(chars, side=...)")
for s in STRS:
    for c in CUTS:
        print(repr(s), repr(c), s.strip(c), s.lstrip(c), s.rstrip(c))

print("--- bytes.strip(side=...)")
for b in [b"  hi  ", b"aabaa", b"", b"\x00a\xff"]:
    print(b.strip(), b.lstrip(), b.rstrip())
    print(b.strip(b"a"), b.lstrip(b"a"), b.rstrip(b"a"))

# `find(sub, reverse=True)` replaces `rfind` — one name, one keyword, and the
# same positional window from either end.
print("--- find(sub, reverse=true)")
FIND = ["abcabc", "abc", "", "haééha"]
SUBS = ["b", "", "abc", "é", "zz"]
for s in FIND:
    for sub in SUBS:
        row = [s.find(sub), s.rfind(sub)]
        for start in [0, 1, -2]:
            row.append(s.rfind(sub, start))
        for end in [0, 3, 99]:
            row.append(s.rfind(sub, 0, end))
        print(repr(s), repr(sub), row)

print("--- bytes find, reversed")
for b in [b"abcabc", b"abc", b"", b"\x00a\xff"]:
    for sub in [b"b", b"", b"abc", b"\xff"]:
        print(b.find(sub), b.rfind(sub), b.rfind(sub, 1))

# The reason these two exist: `strip(chars)` is a character *set*, and reading
# it as a suffix is the classic Python footgun. Both spellings are printed side
# by side so the difference is impossible to miss.
print("--- rm_prefix / rm_suffix")
for s in ["ping.png", "banana.png", "png", "", "x.png.png"]:
    print(repr(s), repr(s.rstrip(".png")), repr(s.removesuffix(".png")))
    print(repr(s), repr(s.lstrip("pin")), repr(s.removeprefix("pin")))
print(repr("abc".removeprefix("")), repr("abc".removesuffix("")))
print(b"a.png".removesuffix(b".png"), b"a.png".removesuffix(b".gif"), b"a.png".removeprefix(b"a."))

# `split(sep, maxsplit, side="right")` is what `rsplit(sep, maxsplit)` did. The
# *right* arm of the split matrix; the left arm is oracled directly in
# core/30_str_split_maxsplit.oro, over the same inputs.
print('--- split(sep=, maxsplit=, side="right")')
SPLITS = [("a.b.c", "."), ("a..b", "."), (".a.", "."), ("", "."), ("aXXbXXc", "XX")]
for s, sep in SPLITS:
    for m in [-1, 0, 1, 2, 5]:
        print(repr(s), repr(sep), m, s.rsplit(sep, m))

print('--- split(maxsplit=, side="right")')
for s in [" a  b  c ", "  a b  ", "   ", "", "a"]:
    for m in [-1, 0, 1, 2, 5]:
        print(repr(s), m, s.rsplit(None, m))

print('--- bytes.split(sep=, maxsplit=, side="right")')
for m in [-1, 0, 1, 2, 5]:
    print(m, b"a.b.c".rsplit(b".", m), b"a..b".rsplit(b".", m))

print('--- bytes.split(maxsplit=, side="right")')
for m in [-1, 0, 1, 2, 5]:
    print(m, b" a  b  c ".rsplit(None, m), b"  a b  ".rsplit(None, m))

# The case the removal made worse, written both ways. `find(sep, reverse=True)`
# hands back an index and leaves the `+ 1` and the slice to the caller; `side=`
# hands back the fields.
print("--- split off the last field")
for path in ["a/b/c.txt", "c.txt", "/leading", "trailing/", ""]:
    i = path.rfind("/")
    print(repr(path), [path[:i], path[i + 1:]], path.rsplit("/", 1))

print("--- count(sub)")
for s in FIND:
    row = []
    for sub in SUBS:
        row.append(s.count(sub))
    print(repr(s), row)
for b in [b"abcabc", b"abc", b"", b"\x00a\xff"]:
    row = []
    for sub in [b"b", b"", b"abc", b"\xff"]:
        row.append(b.count(sub))
    print(row)

print("--- is_digit / is_alpha / is_alnum / is_space")
for s in ["123", "12a", "abc", "café", "a1", "", " ", " \t\n", "\x1c", "３", "ｄ"]:
    print(repr(s), s.isdigit(), s.isalpha(), s.isalnum(), s.isspace())
for b in [b"123", b"12a", b"abc", b"a1", b"", b" ", b" \t\x0b", b"\xff"]:
    print(b, b.isdigit(), b.isalpha(), b.isalnum(), b.isspace())

# `join`, written the way CPython spells it: the separator is the receiver.
print("--- join")
print(repr("-".join(["a", "b", "c"])), repr("-".join(["a"])), repr("-".join([])))
print(repr("".join(["a", "b"])), repr(", ".join(["alice", "bob"])))
print(repr("-".join(("a", "b"))))
print(b",".join([b"a", b"b", b"c"]), b"".join([b"a", b"b"]), b",".join([]))

try:
    "-".join(["a"], 2)
    print('["a"].join("-", 2)', "-> no error")
except TypeError:
    print('["a"].join("-", 2)', "-> TypeError")

try:
    "-".join()
    print('["a"].join()', "-> no error")
except TypeError:
    print('["a"].join()', "-> TypeError")

try:
    "-".join(["a", 1])
    print('["a", 1].join("-")', "-> no error")
except TypeError:
    print('["a", 1].join("-")', "-> TypeError")
