# Keyword-heavy calls, and the blind spot this program exists to close.
#
# The argument rule (docs/reference.md §2) made every parameter with a default
# **keyword-only**, so a keyword call stopped being the unusual shape and
# became the ordinary one. Nothing else in this suite measures one, which is
# how a regression on the keyword-binding path can reach a release unseen.
#
# Three shapes, because the VM reaches keyword calls through different paths:
#   a  a plain function      `step(i, by=2, ...)`    -> CallKw (+ its fast path)
#   b  a method              `acc.add(i, weight=1)`   -> the method keyword path
#   c  a native              `s.split(sep=",")`       -> the builtin kwarg path
#
# The callees are trivial on purpose: what is measured is binding a name to a
# parameter, not the callee's work. Loop-rule spelling (`for i, _ in range`),
# with a `.py` twin in Python's `for i in range` for the CPython comparison.
def step(x, by=1, scale=1, bias=0):
    return (x + by) * scale + bias


class Acc:
    def __init__(self):
        self.total = 0

    def add(self, x, weight=1, offset=0):
        self.total = self.total + x * weight + offset
        return self.total


acc = Acc()
total = 0
for i in range(200000):
    total = total + step(i, by=2, scale=1, bias=3)
    acc.add(i, weight=1, offset=1)
    total = total + len("a,b,c".split(sep=",", maxsplit=1))

print(total)
print(acc.total)
