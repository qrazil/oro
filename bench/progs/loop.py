# CPython twin of loop.oro: the same program with Oro's (index, value)
# for-pair loops written in Python's spelling. Generated for the vs-CPython
# column after the loop rule made the .oro Oro-only.
# Dispatch-bound: a 3M-iteration loop doing integer arithmetic. This is close to
# a pure measure of instruction-fetch + dispatch cost.
#
# It counts with `for i, _ in range(n)` because that is now the only bounded
# count Oro has — a `while` stepping a variable by a constant is a compile error
# (see README, "The loop rule"). This used to be a hand-rolled `while` counter,
# kept on purpose so the measured loop matched the VM's dispatch rather than
# `range`'s Rust-side stepping; the loop rule ended that, so these numbers must
# be rebaselined against the `for` form (see bench/RESULTS.md).
total = 0
for i in range(3000000):
    total = total + i

print(total)
