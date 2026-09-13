# CPython twin of genpipe.oro: the same program with Oro's (index, value)
# for-pair loops written in Python's spelling. Generated for the vs-CPython
# column after the loop rule made the .oro Oro-only.
# Generator pipeline: three chained generators, each resuming a suspended frame
# per element. Exercises frame push/pop and the yield/resume path.
def counter(n):
    for i in range(n):
        yield i


def evens(src):
    for x in src:
        if x % 2 == 0:
            yield x


def scaled(src, k):
    for x in src:
        yield x * k


total = 0
for v in scaled(evens(counter(300000)), 3):
    total = total + v

print(total)
