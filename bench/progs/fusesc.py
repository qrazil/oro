# CPython twin of fusesc.oro. Python's map/filter are lazy, so this is the
# behaviour Oro's chains now have at the head of a pipeline.
def f(x):
    return x * 2 + 1

xs = []
i = 0
while i < 20000:
    xs.append(i)
    i = i + 1

total = 0
i = 0
while i < 50:
    total = total + next(map(f, xs))
    total = total + list(map(f, xs))[:3][-1]
    total = total + next(map(f, (x for x in xs if x > i)))
    i = i + 1
print(total)
