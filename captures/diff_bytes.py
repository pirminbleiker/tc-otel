#!/usr/bin/env python3
"""Byte-aligned diff for a specific (IG, IO, rsz) response payload across time.

Usage: diff_bytes.py <capture.log> <ig_hex> <io_hex> <rsz> [N]
  - ig_hex / io_hex: e.g. 0xF200  0x0000
  - rsz: expected response size (bytes)
  - N: number of samples to show (default 20)

Prints a table of samples aligned byte-by-byte. Columns that change across
samples are marked with '*'. Helps identify which bytes carry dynamic data
(timestamps, counters) vs static (header/ids).
"""
import sys, struct
from collections import defaultdict

def parse(path, ig_want, io_want, rsz_want):
    # Build req→resp correlation by invoke_id + src/dst swap
    pending = {}  # (src,dst,invoke) -> req info
    samples = []  # list of (ts, payload_bytes)
    with open(path) as f:
        for ln in f:
            parts = ln.rstrip().split(" ", 2)
            if len(parts) != 3: continue
            ts = parts[0]
            try: b = bytes.fromhex(parts[2])
            except ValueError: continue
            if len(b) < 32: continue
            dst = b[0:8]; src = b[8:16]
            cmd, flags, dlen, err, invoke = struct.unpack_from("<HHIII", b, 16)
            is_resp = bool(flags & 1)
            data = b[32:32+dlen]
            if cmd == 2 and not is_resp and len(data) >= 12:
                ig, io, sz = struct.unpack_from("<III", data, 0)
                if ig == ig_want and io == io_want and sz == rsz_want:
                    # Key: response will arrive with src/dst swapped and same invoke
                    pending[(dst, src, invoke)] = ts
            elif cmd == 2 and is_resp and len(data) >= 8:
                key = (src, dst, invoke)  # request key was (orig-dst, orig-src, invoke)
                if key in pending:
                    res, rlen = struct.unpack_from("<II", data, 0)
                    if rlen == rsz_want and res == 0:
                        samples.append((pending.pop(key), data[8:8+rlen]))
    return samples

def show(samples, n, rsz):
    show_n = min(n, len(samples))
    if show_n == 0:
        print("no samples found")
        return
    cols = rsz
    # Find changing bytes
    changing = [False]*cols
    for i in range(cols):
        vals = set(s[1][i] for s in samples[:n*4] if len(s[1]) > i)
        if len(vals) > 1:
            changing[i] = True
    # Header
    hdr = "      ts              "
    for i in range(cols):
        hdr += ("*" if changing[i] else " ") + f"{i:02x} "
    print(hdr)
    for ts, payload in samples[:show_n]:
        row = f"{ts} "
        for i in range(cols):
            row += f" {payload[i]:02x} "
        print(row)
    print(f"\n... {len(samples)} total samples")
    # Summary: per-changing-byte value range
    print("\nChanging bytes summary:")
    for i in range(cols):
        if not changing[i]: continue
        vals = [s[1][i] for s in samples]
        print(f"  byte[0x{i:02x}]  min={min(vals):#04x}  max={max(vals):#04x}  unique={len(set(vals))}")
    # Try common field widths
    print("\nInterpret as little-endian fields:")
    def ule(buf, off, sz):
        return int.from_bytes(buf[off:off+sz], "little")
    # u32 LE at each 4-byte aligned offset
    for off in range(0, cols-3, 4):
        ivals = [ule(s[1], off, 4) for s in samples]
        if len(set(ivals)) > 1:
            print(f"  u32 @+0x{off:02x}  first={ivals[0]} last={ivals[-1]} delta={ivals[-1]-ivals[0]}  monotonic_inc={all(ivals[i+1]>=ivals[i] for i in range(len(ivals)-1))}")

def main():
    if len(sys.argv) < 5:
        print(__doc__); sys.exit(1)
    path = sys.argv[1]
    ig = int(sys.argv[2], 16)
    io = int(sys.argv[3], 16)
    rsz = int(sys.argv[4])
    n = int(sys.argv[5]) if len(sys.argv) > 5 else 20
    samples = parse(path, ig, io, rsz)
    print(f"matched {len(samples)} samples for IG=0x{ig:08x} IO=0x{io:08x} rsz={rsz}\n")
    show(samples, n, rsz)

if __name__ == "__main__":
    main()
