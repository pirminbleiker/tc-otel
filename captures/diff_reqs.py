#!/usr/bin/env python3
"""Diff index-group+offset patterns between two captures."""
import sys, struct
from collections import Counter

def iter_reqs(path):
    with open(path) as f:
        for ln in f:
            parts = ln.rstrip().split(" ", 2)
            if len(parts) != 3: continue
            try: payload = bytes.fromhex(parts[2])
            except ValueError: continue
            if len(payload) < 32: continue
            cmd, flags, dlen = struct.unpack_from("<HHI", payload, 16)
            is_resp = bool(flags & 1)
            if is_resp: continue
            data = payload[32:32+dlen]
            if cmd == 2 and len(data) >= 12:
                ig, io, sz = struct.unpack_from("<III", data, 0)
                yield ("Read", ig, io, sz, 0)
            elif cmd == 3 and len(data) >= 12:
                ig, io, sz = struct.unpack_from("<III", data, 0)
                yield ("Write", ig, io, sz, sz)
            elif cmd == 9 and len(data) >= 16:
                ig, io, rsz, wsz = struct.unpack_from("<IIII", data, 0)
                yield ("ReadWrite", ig, io, rsz, wsz)

def count(path):
    c = Counter()
    for r in iter_reqs(path):
        c[r] += 1
    return c

a = count(sys.argv[1])
b = count(sys.argv[2])
all_keys = set(a) | set(b)
print(f"{'cmd':<10} {'ig':>10} {'io':>10} {'rsz':>6} {'wsz':>6}  {'A':>6} {'B':>6}  {'Δ':>6}")
rows = []
for k in all_keys:
    av, bv = a.get(k, 0), b.get(k, 0)
    rows.append((bv - av, k, av, bv))
rows.sort(key=lambda x: -abs(x[0]))
for delta, k, av, bv in rows[:50]:
    cmd, ig, io, rsz, wsz = k
    print(f"{cmd:<10} 0x{ig:08x} 0x{io:08x} {rsz:>6} {wsz:>6}  {av:>6} {bv:>6}  {delta:>+6}")
