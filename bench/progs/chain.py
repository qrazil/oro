# CPython twin of chain.oro. Oro's collection protocol (`.filter`/`.map`/
# `.reduce` with `=>` lambdas) has no Python spelling, so this is the
# equivalent-behaviour version in Python's idiom, not a byte-for-byte copy.
# Every other bench program is portable and is run under CPython as-is.
xs = []
i = 0
while i < 200000:
    xs.append(i)
    i = i + 1

ys = [x * 3 for x in xs if x % 2 == 0]
total = 0
for x in ys:
    total = total + x
print(len(ys))
print(total)
