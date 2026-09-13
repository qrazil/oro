# CPython twin of listbuild.oro: the same program with Oro's (index, value)
# for-pair loops written in Python's spelling. Generated for the vs-CPython
# column after the loop rule made the .oro Oro-only.
# List-heavy: build, index, and mutate a 400k-element list, then reduce it with
# an explicit loop. Portable to CPython (no chains, no `=>`).
xs = []
for i in range(400000):
    xs.append(i * 3)

total = 0
n = len(xs)
for i in range(n):
    xs[i] = xs[i] + 1
    total = total + xs[i]

print(total)
