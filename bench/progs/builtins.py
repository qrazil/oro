# CPython twin of builtins.oro: the same program with Oro's (index, value)
# for-pair loops written in Python's spelling. Generated for the vs-CPython
# column after the loop rule made the .oro Oro-only.
# Builtin-heavy: four global lookups and four native calls per iteration.
# Every `len(...)` in a loop is a LoadGlobal, and the rest of the suite barely
# exercises that path — real scripts do it constantly.
xs = [1, 2, 3, 4, 5]
total = 0
for i in range(300000):
    total = total + len(xs) + abs(0 - i) + min(i, 7) + max(i, 3)

print(total)
