# The manual oracle for 35_bytes.oro: that file with Oro's spellings
# translated one for one into CPython's. The Oro program uses the argument
# rule's keyword-only spellings (`d.get(default=)`), which CPython rejects,
# so the oracle cannot run the program itself; this twin is what its
# .expected is still generated from, and diffing the two is the review.
#
# This file prints Python's `True`/`False`/`None`, so its output goes through
# the same outbound rename `oracle.sh` applies before it is the expectation.
#
#   python3 corpus/divergence/35_bytes.twin.py \
#     | sed -E 's/\bTrue\b/true/g; s/\bFalse\b/false/g; s/\bNone\b/null/g' \
#     | diff - corpus/divergence/35_bytes.expected

# bytes: literals, the sequence protocol, the method set, repr and hashing.
# Every line here runs identically under CPython, which is the point of making
# `bytes` a second type rather than redefining `str`: the whole new surface
# stays inside the oracle.

# --- Literals and escapes ---------------------------------------------------
print(b"hello", b'hello', B"hello")
print(b"tab\tnl\ncr\rbs\\quote\"apos\'")
print(b"\0\a\b\f\v")
print(b"\x00\x41\x7f\x80\xff")
print(rb"\d+\n", b"\\d+\\n", rb"\d+\n" == b"\\d+\\n")
# repr picks the quote that avoids escaping, and shows every octet outside
# printable ASCII as \xNN.
print(repr(b"it's"), repr(b'say "hi"'), repr(b"both ' and \""))
print(repr(b""), repr(b"\xc3\xa9"))

# --- Length, indexing, slicing ----------------------------------------------
b = b"hello world"
print(len(b), len(b""))
# Indexing yields an int; slicing yields bytes. That asymmetry is the two
# types telling the truth about what they contain.
print(b[0], b[-1], b[6])
print(b[0:5], b[6:], b[:5], b[:], b[-5:])
print(b[::2], b[::-1], b[4:1:-1])
# Slice bounds clamp instead of raising.
print(b[10:20], b[20:30], b[-100:2])

# --- Iteration --------------------------------------------------------------
for x in b"abc":
    print(x)
total = 0
for x in b"abc":
    total = total + x
print("sum", total)
print(len(b"abc"), b"abc"[0] + b"abc"[1])

# --- Operators --------------------------------------------------------------
print(b"ab" + b"cd", b"ab" * 3, 3 * b"ab", b"ab" * 0)
print(b"ab" in b"xaby", b"ba" in b"xaby", b"" in b"x", b"xaby" in b"xaby")
print(b"abc" == b"abc", b"abc" == b"abd", b"abc" != b"abd")
print(b"abc" < b"abd", b"abc" < b"ab", b"Z" < b"a", b"\xff" > b"a")

# --- Truthiness -------------------------------------------------------------
if b"":
    print("unreachable")
else:
    print("empty bytes are falsy")
if b"\x00":
    print("a zero octet is still one octet")

# --- Type identity ----------------------------------------------------------
print(type(b"x"), type(b"x") == bytes, type("x") == bytes, type(b"x") == str)

# --- Methods ----------------------------------------------------------------
line = b"  Content-Type: text/html \t\n"
print(line.strip())
print(b"\x0b\x0c ab \x0b\x0c".strip())
print(line.strip().upper(), line.strip().lower())
# Case folding is ASCII-only: an octet is not a character.
print(b"AbC\xff".lower(), b"AbC\xff".upper())
print(b"a,b,,c".split(b","), b"a,b,c".split(b",", 1))
print(b"a b\x0bc  d".split(), b" a b c ".split(None, 1))
print(b"".split(b","), b"x".split(b","))
print(b"abc".find(b"b"), b"abc".find(b"z"), b"abc".find(b""), b"abc".find(b"abcd"))
print(b"aXbXc".replace(b"X", b"-"), b"abc".replace(b"", b"."), b"abc".replace(b"z", b"!"))
print(b"abc".startswith(b"ab"), b"abc".startswith(b"b"), b"abc".startswith(b""))
print(b"abc".endswith(b"bc"), b"abc".endswith(b"b"), b"abc".endswith(b""))
print(b"abcabc".count(b"bc"), b"abc".count(b""), b"".count(b"x"))
print(b"\xff\x00A".hex(), b"".hex())

# --- A byte-level parse, which is what the type is for -----------------------
COLON = 58
header = b"Host: example.com"
i = header.find(b":")
print(header[i] == COLON, header[:i], header[i + 2:])
for part in b"GET /a?b=1 HTTP/1.1".split(b" "):
    print(part, len(part))

# --- Hashing: bytes as dict keys --------------------------------------------
d = {b"alpha": 1, b"beta": 2}
d[b"gamma"] = 3
print(d)
print(d[b"alpha"], b"beta" in d, b"delta" in d)
print(len(d))
# A bytes key and the str that would decode to it are different keys.
mixed = {b"k": "bytes key", "k": "str key"}
print(len(mixed), mixed[b"k"], mixed["k"])
counts = {}
for w in b"a b a c b a".split(b" "):
    counts[w] = counts.get(w, 0) + 1
print(counts)

# --- Interpolation ----------------------------------------------------------
name = b"oro"
print(f"name={name} len={len(name)}")
print(f"{b'x'!r}")

# --- Octets a str cannot reach ----------------------------------------------
# Every octet is a valid byte, and most of them are not a valid UTF-8 character
# on their own. This is the gap `[ints].to_bytes()` fills (its spelling is a
# `to_` cast, so the cast itself lives in divergence/37_bytes_casts.oro) — and
# these are the values it has to produce, pinned here against CPython.
print(b"\xc8", len(b"\xc8"), b"\xc8"[0])
print(b"\x00\xc8\xff", len(b"\x00\xc8\xff"), b"\x00\xc8\xff"[1])
# The same codepoint as text is one character and two octets, which is exactly
# why a str cannot stand in for a byte.
print(len(chr(200)), len(b"\xc3\x88"), b"\xc8" == b"\xc3\x88")
print(b"\x00\xc8\xff"[-1])
