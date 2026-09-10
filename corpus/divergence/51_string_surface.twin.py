# The manual oracle for 51_string_surface.oro: that file with Oro's spellings
# translated one for one into CPython's. Run it and diff against the reviewed
# .expected; an empty diff is the review.
#
#   python3 corpus/divergence/51_string_surface.twin.py | diff - corpus/divergence/51_string_surface.expected

STRS = ["  hi  ", "xxhixx", "xyxhixyx", "", "xxx", "\thi\n ", "ααhiα"]
CUTS = ["x", "xy", "", "α", "abc", None]

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
print("--- find(sub, reverse=True)")
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
