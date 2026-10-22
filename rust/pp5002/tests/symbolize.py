#!/usr/bin/env python3
"""Aggregate the samples a `--features prof` bench wrote (/tmp/bench.prof) by function: symbolize.py <binary> [prof]"""
import bisect, collections, subprocess, sys
binary = sys.argv[1]; prof = sys.argv[2] if len(sys.argv) > 2 else "/tmp/bench.prof"
syms = []
for l in subprocess.run(["nm", "-n", "--defined-only", binary], capture_output=True, text=True).stdout.splitlines():
    w = l.split(None, 2)
    if len(w) == 3 and w[1] in "tTwW": syms.append((int(w[0], 16), w[2]))
addrs = [a for a, _ in syms]
lines = open(prof).read().split()
base = int(lines[1], 16); pcs = [int(x, 16) for x in lines[2:]]
c = collections.Counter()
for pc in pcs:
    i = bisect.bisect_right(addrs, pc - base) - 1
    c[syms[i][1] if i >= 0 else "?"] += 1
tot = sum(c.values())
def short(n):
    n = subprocess.run(["rustfilt"], input=n, capture_output=True, text=True).stdout.strip() if False else n
    return n[:110]
print("%d samples" % tot)
for n, k in c.most_common(25): print("%5.1f%%  %s" % (100 * k / tot, short(n)))
