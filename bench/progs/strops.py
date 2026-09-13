# CPython twin of strops.oro: the same program with Oro's (index, value)
# for-pair loops written in Python's spelling. Generated for the vs-CPython
# column after the loop rule made the .oro Oro-only.
# The hot string methods: strip, find and replace over 200k short lines. Written
# in the spellings both Oro and CPython share, so the harness can run it on
# either without a twin.
ROUNDS = 200000
line = "  key = value ; trailing  "
hits = 0
for _ in range(ROUNDS):
    s = line.strip()
    i = s.find("=")
    if i >= 0:
        hits = hits + 1
    k = s.replace(" ", "")

print(hits, ROUNDS)
