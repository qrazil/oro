# The CPython twin of dictops.oro: the same program, with the pair scan spelled
# `d.items()`. That is the one translation — iterating a dict yields its
# (key, value) pairs in Oro and its keys in CPython, so `for k, v in d` has no
# CPython spelling and `.items()` is what it means.
d = {}
i = 0
while i < 500000:
    d[i] = i * 2
    i = i + 1

total = 0
for k, v in d.items():
    total = total + v

print(total)
