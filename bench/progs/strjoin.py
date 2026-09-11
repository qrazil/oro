# CPython twin of strjoin.oro. `xs.join(sep)` is the only spelling of a join in
# Oro — the separator is the argument, not the receiver — and CPython has no
# such method, so this is the same program in `sep.join(xs)` form. Nothing else
# differs: same 200k f-strings, same list, same one join at the end.
parts = []
i = 0
while i < 200000:
    parts.append(f"{i}")
    i = i + 1

s = ",".join(parts)
print(len(s))
