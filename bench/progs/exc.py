# CPython twin of exc.oro: the same program with Oro's (index, value)
# for-pair loops written in Python's spelling. Generated for the vs-CPython
# column after the loop rule made the .oro Oro-only.
# Exception-heavy: a raise/catch on most iterations, plus a finally that always
# runs. Exercises block setup, unwinding, and handler dispatch.
class Boom(Exception):
    pass


def risky(i):
    if i % 3 == 0:
        raise Boom("nope")
    return i


caught = 0
total = 0
for i in range(200000):
    try:
        total = total + risky(i)
    except Boom:
        caught = caught + 1
    finally:
        total = total + 1

print(caught)
print(total)
