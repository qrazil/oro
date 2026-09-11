# CPython twin of fuse.oro: same work, Python's spelling.
def f(x):
    return x + 1

xs = []
i = 0
while i < 120000:
    xs.append(i)
    i = i + 1

a = [f(x) for x in xs]
b = [f(x) for x in xs if x % 2 == 0]
c = [x for x in [f(x) for x in [x for x in [f(x) for x in xs] if x % 3 != 0]] if x % 5 != 0]
d = [x for x in [f(x) for x in sorted([x for x in [f(x) for x in xs] if x % 7 == 0])] if x > 10]
print(len(a), len(b), len(c), len(d))
