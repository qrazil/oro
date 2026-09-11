#!/usr/bin/env python3
"""Interleaved A/B benchmark harness, pinned to one core.

  ab.py A_BIN B_BIN [-n ROUNDS] [-c CORE] [bench ...]

Every round runs each benchmark under both binaries back to back; which of the
two goes first flips each round, so neither owns the warm slot. Reports best-of
and median per benchmark, plus the mean delta across all of them.
"""
import os, statistics, subprocess, sys, time

PROGS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "progs")
BENCHES = ["fib","loop","strjoin","strops","dictops","dictstr","oo","genpipe","exc",
           "listbuild","builtins","chain","fuse","fusesc","json"]

def run(binary, src, core):
    cmd = ["taskset","-c",str(core),binary,src]
    t0 = time.perf_counter()
    r = subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    t1 = time.perf_counter()
    if r.returncode != 0:
        sys.exit(f"{binary} {src} exited {r.returncode}")
    return t1 - t0

def main():
    args = sys.argv[1:]
    n, core, sel = 9, 3, []
    a = b = None
    i = 0
    while i < len(args):
        if args[i] == "-n": n = int(args[i+1]); i += 2
        elif args[i] == "-c": core = int(args[i+1]); i += 2
        elif a is None: a = args[i]; i += 1
        elif b is None: b = args[i]; i += 1
        else: sel.append(args[i]); i += 1
    benches = sel or BENCHES
    times = {x: {a: [], b: []} for x in benches}

    # Correctness: the two binaries must print the same thing.
    for x in benches:
        src = os.path.join(PROGS, x + ".oro")
        oa = subprocess.run([a, src], capture_output=True)
        ob = subprocess.run([b, src], capture_output=True)
        if (oa.stdout, oa.stderr, oa.returncode) != (ob.stdout, ob.stderr, ob.returncode):
            sys.exit(f"OUTPUT MISMATCH in {x}")

    for r in range(n):
        order = [a, b] if r % 2 == 0 else [b, a]
        for x in benches:
            src = os.path.join(PROGS, x + ".oro")
            for binary in order:
                times[x][binary].append(run(binary, src, core))
        print(f"  round {r+1}/{n}", file=sys.stderr, flush=True)

    print(f"{'bench':<12}{'A min':>9}{'B min':>9}{'d min':>9}"
          f"{'A med':>9}{'B med':>9}{'d med':>9}")
    dmins, dmeds = [], []
    for x in benches:
        ta, tb = times[x][a], times[x][b]
        amin, bmin = min(ta), min(tb)
        amed, bmed = statistics.median(ta), statistics.median(tb)
        dmin = (bmin - amin) / amin * 100
        dmed = (bmed - amed) / amed * 100
        dmins.append(dmin); dmeds.append(dmed)
        print(f"{x:<12}{amin:>9.4f}{bmin:>9.4f}{dmin:>8.2f}%"
              f"{amed:>9.4f}{bmed:>9.4f}{dmed:>8.2f}%")
    print(f"{'MEAN':<12}{'':>9}{'':>9}{statistics.mean(dmins):>8.2f}%"
          f"{'':>9}{'':>9}{statistics.mean(dmeds):>8.2f}%")

main()
