# The manual oracle for 63_percent.oro: that file with Oro's spellings
# translated one for one into CPython's, and with `http.quote` and friends
# replaced by the `urllib.parse` functions they are named after. Run it and
# diff against the reviewed .expected; an empty diff is the review.
#
#   python3 corpus/divergence/63_percent.twin.py \
#     | diff - corpus/divergence/63_percent.expected
#
# The three translations, and nothing else differs:
#
#   http.quote/quote_plus  ->  urllib.parse.quote/quote_plus, same arguments
#   http.unquote(s)        ->  unquote(s, errors="strict")
#   http.unquote(s, plus=True) -> unquote_plus(s, errors="strict")
#
# `errors="strict"` because Oro's `bytes.to_str()` raises on octets that are
# not UTF-8 where `urllib`'s default quietly substitutes U+FFFD; nothing in
# *this* file decodes invalid UTF-8, so the flag only makes the equivalence
# exact rather than incidental. `unquote_to_bytes` stands in for Oro's
# bytes-in/bytes-out `unquote`, which is the one shape these two libraries
# genuinely disagree about — see 64_percent_policy.oro.
#
# `bool` is the one value here whose two languages print it differently, and
# `encode_query({"ok": true})` is the line that reaches it: Oro formats a
# bool as `true`, CPython as `True`, so the twin writes Oro's spelling.
from urllib.parse import quote, quote_plus, unquote, unquote_to_bytes, urlencode


def row(items):
    print(" ".join(items))


def table(label, f):
    print(f"--- {label}")
    for i in range(0, 256, 16):
        cells = []
        for j in range(16):
            cells.append(f(bytes([i + j])))
        row(cells)


table("quote(b, safe='')", lambda b: quote(b, safe=""))
table("quote_plus(b)", lambda b: quote_plus(b))

UNRESERVED = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~"
print("--- unreserved")
print(quote(UNRESERVED, safe=""))
print(quote_plus(UNRESERVED))
print(str(quote(UNRESERVED, safe="") == UNRESERVED).lower())

SUBJECT = "a/b?c=d&e f+g:h@i;j,k$l~m"
print("--- safe=")
for safe in ["", "/", "/?", ":/?#[]@", "!$&'()*+,;=", "abc", " "]:
    print(f"safe={safe!r} {quote(SUBJECT, safe=safe)}")
    print(f"safe={safe!r} {quote_plus(SUBJECT, safe=safe)}")

print("--- default safe")
print(quote("a/b c"))
print(quote("a/b c", safe=""))

print("--- utf-8")
for s in ["café", "☕", "naïve résumé", "日本語", "é", "ÿ", "\U0001F600"]:
    print(quote(s, safe=""), quote_plus(s))

print("--- hex case")
print(quote("/", safe=""), unquote("%2f", errors="strict"), unquote("%2F", errors="strict"))
print(
    unquote("%c3%a9", errors="strict"),
    unquote("%C3%A9", errors="strict"),
    unquote("%c3%A9", errors="strict"),
)

print("--- plus")
print(quote_plus("a b"), quote_plus("a+b"), quote_plus("a b+c"))
print(quote("a b", safe=""), quote("a+b", safe=""))
print(unquote("a+b", errors="strict"), unquote("a+b", errors="strict").replace("+", " "))
print(unquote("a%2Bb", errors="strict"), unquote("a%2Bb", errors="strict"))
print(unquote("a%20b", errors="strict"), unquote("a%20b", errors="strict"))

print("--- encode_query")
print(urlencode({}))
print(urlencode({"q": "a&b"}))
# `true` rather than `True`: see the note at the top.
print(urlencode({"q": "hello world", "page": 2}) + "&ok=true")
print(urlencode({"redirect": "http://h/a b?x=1&y=2#f"}))
print(urlencode([("a", "1"), ("a", "2"), ("b", "")]))
print(urlencode({"q": "café ☕"}))
print(urlencode({"a b": "c d"}))


def roundtrips(b):
    if unquote_to_bytes(quote(b, safe="")) != b:
        return False
    if unquote_to_bytes(quote_plus(b).replace("+", " ").replace(" ", "%20")) != b:
        return False
    if unquote_to_bytes(quote(b, safe="/ +&=?")) != b:
        return False
    return True


print("--- round trip")
bad = 0
for i in range(256):
    if not roundtrips(bytes([i])):
        bad = bad + 1
print("every single octet:", bad)

bad = 0
for i in range(256):
    for j in range(256):
        if not roundtrips(bytes([i, j])):
            bad = bad + 1
print("every pair of octets:", bad)

bad = 0
seed = 12345
for n in range(2000):
    length = seed % 40
    parts = []
    for k in range(length):
        seed = (seed * 1103515245 + 12345) % 2147483648
        parts.append(seed % 256)
    seed = (seed * 1103515245 + 12345) % 2147483648
    if not roundtrips(bytes(parts)):
        bad = bad + 1
print("2000 pseudo-random strings:", bad)
