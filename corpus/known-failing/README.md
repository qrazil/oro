# known-failing

Programs that are **correct Python** which Oro currently gets **wrong**, and
that we intend to fix. This is NOT `divergence/` — that directory is for
*deliberate* design choices (block scope, no `with`, …). Mixing bugs into it
would turn a design record into a junk drawer.

Each `.oro` file starts with a one-line comment saying what is broken. When a fix
lands, the file moves to `core/` (with a regenerated `.expected`).

It is empty right now, which is the state to keep it in: an empty bug list is
the goal, and a file sitting here is a bug nobody has got to yet — never a
divergence that was quietly reclassified.

`./corpus/run.sh` reports the known-failing count separately; it never fails the
build on them — the point is that they stay *visible* rather than being omitted
(a suite that only agrees with itself is exactly the blind spot the CPython
oracle exists to catch).
