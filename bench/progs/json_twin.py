# CPython twin for json.oro (named json_twin.py, not json.py, so that `import
# json` finds the standard library rather than this file). `json.loads`/`json.dumps` are the C-accelerated
# ones; `separators=(',', ':')` is CPython's spelling of Oro's compact default,
# and `indent=2` is the same pretty form.
import json

SMALL_ROUNDS = 400
BIG_ROUNDS = 12


def small_payload():
    items = []
    i = 0
    while i < 13:
        items.append({"sku": f"ABC-{i}", "qty": i, "price": i * 1.5, "note": None})
        i = i + 1
    return {
        "id": 12345,
        "name": "Widget Assembly",
        "active": True,
        "score": 98.6,
        "tags": ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"],
        "owner": {"id": 77, "email": "user@example.com", "roles": ["admin", "user"], "verified": False},
        "items": items,
        "meta": {"created": "2024-01-01T00:00:00Z", "rev": 4, "source": "api-gateway-v2"},
    }


def record_array(n):
    out = []
    i = 0
    while i < n:
        out.append({"id": i, "name": f"record-{i}", "v": i * 3.25, "ok": i % 2 == 0, "tags": ["x", "y"]})
        i = i + 1
    return out


def nested(n):
    v = 0
    i = 0
    while i < n:
        v = {"n": v}
        i = i + 1
    return v


def string_list(n):
    out = []
    i = 0
    while i < n:
        out.append(f"the quick brown fox jumps over the lazy dog {i}")
        i = i + 1
    return out


def number_list(n):
    out = []
    i = 0
    while i < n:
        out.append(i * 7919)
        out.append(i * 0.125)
        i = i + 1
    return out


def dumps(v, **kw):
    return json.dumps(v, separators=(",", ":"), **kw)


def rounds(text, value, n):
    total = 0
    i = 0
    while i < n:
        total = total + len(dumps(json.loads(text)))
        i = i + 1
    return total


shapes = [
    ["small", small_payload(), SMALL_ROUNDS],
    ["array", record_array(600), BIG_ROUNDS],
    ["nested", nested(200), SMALL_ROUNDS],
    ["strings", string_list(900), BIG_ROUNDS],
    ["numbers", number_list(1500), BIG_ROUNDS],
]

checksum = 0
for shape in shapes:
    text = dumps(shape[1])
    checksum = checksum + rounds(text, shape[1], shape[2])

pretty = 0
i = 0
while i < SMALL_ROUNDS:
    pretty = pretty + len(json.dumps(small_payload(), indent=2, separators=(",", ": ")))
    i = i + 1

print(checksum, pretty)
