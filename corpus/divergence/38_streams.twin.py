# The manual oracle for 38_streams.oro (docs/stdlib-server-design.md §2).
#
# This is that file with the mode strings written CPython's way — "rb"/"wb"/"ab"
# instead of "r"/"w"/"a" — and Oro's spellings translated one for one:
#
#   s.to_bytes()            -> s.encode()
#   b.to_str()              -> b.decode()
#   r.read_until(d, limit)  -> r.readline(limit)   (equivalent while no line
#                              reaches the limit; see the note below)
#
# Run it under CPython and diff against 38_streams.expected. An empty diff is
# the review: it says Oro's byte streams answer exactly what CPython's binary
# file object answers, and that the mode string is the only thing that changed.
#
#   python3 corpus/divergence/38_streams.twin.py | diff - corpus/divergence/38_streams.expected
import os

tmp = "oro_corpus_scratch.bin"

w = open(tmp, "wb")
w.write(b"alpha\nbeta\n")
w.write("gamma\n".encode())
w.close()

r = open(tmp, "rb")
print(r.read(5))
print(r.read(6))
print(r.read(1000))
print(r.read(1000))
print(r.read(1000) == b"")

r2 = open(tmp, "rb")
whole = r2.read(1000)
print(whole.decode().strip().split("\n"))
print(len(whole))

r3 = open(tmp, "rb")
print(r3.readline(64))
print(r3.readline(64))
print(r3.read(1000))
print(r3.readline(64))

# No CPython equivalent: readline(size) returns a partial line where Oro's
# read_until raises, because CPython has no limit argument that means "refuse".
# The line is printed literally so the diff stays empty; the raise itself is
# checked in src/stream/tests.rs and by the corpus running Oro.
print("limit reached without a delimiter: ValueError")

a = open(tmp, "ab")
a.write(b"delta\n")
a.close()
r5 = open(tmp, "rb")
print(r5.read(1000).decode().strip().split("\n"))

r6 = open(tmp, "rb")
r6.close()
r6.close()
try:
    r6.read(1)
    print("no error")
except ValueError:
    print("read on a closed stream: ValueError")

# io.UnsupportedOperation is a subclass of ValueError, so `except ValueError`
# catches it here exactly as it catches Oro's.
try:
    open(tmp, "rb").write(b"x")
    print("no error")
except ValueError:
    print("write to a reader: ValueError")
try:
    open(tmp, "wb").read(1)
    print("no error")
except ValueError:
    print("read from a writer: ValueError")

os.remove(tmp)
print(os.path.exists(tmp))
