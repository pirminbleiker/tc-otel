"""
Scope-Resolver Proof-of-Concept.

Standalone test for the architecture proposed in
``docs/plans/scope-name-via-ads-symbol-resolution.md``: download the
PLC's full symbol table over ADS and prove that we can map an
instance path (e.g. ``Cell1.Axis1.Motor``) to its FB type name
(``FB_Motor``).

Run on TC-XAR-SIM (or any Windows host with TwinCAT ADS DLL +
``pyads`` installed) against the local PLC runtime. Pass an
optional ``--query`` argument to test a specific path; otherwise
the script just dumps a summary of the symbol table so you can
see which types live where.

Usage::

    pip install pyads
    python scripts/scope_resolver_poc.py
    python scripts/scope_resolver_poc.py --net-id 172.28.41.37.1.1 --port 852
    python scripts/scope_resolver_poc.py --query "Cell1.Axis1.Motor"

Why this exists: validating the resolver idea against the full
tc-otel + Victoria + Grafana stack is too slow to iterate on. This
script touches only the ADS layer — the same data path the planned
``ScopeResolver`` would use. If the symbol table answers the lookup
here, the architecture works; integrating it into Rust is then
mechanical.

The script intentionally has *no* dependency on tc-otel itself.
"""
from __future__ import annotations

import argparse
import sys
import time
from collections import Counter
from typing import Iterable

try:
    import pyads
except ImportError:
    sys.stderr.write(
        "pyads is required: pip install pyads\n"
        "(On Windows the TwinCAT ADS DLL must also be installed; on Linux "
        "the Beckhoff Linux ADS library.)\n"
    )
    sys.exit(2)


def parent_path(namespace: str) -> str:
    """Strip the trailing instance segment.

    Mirrors ``tc-otel-service::scope_resolver::parent_path``: the FB_Log
    / FB_Tracer / FB_Metrics variable lives at the leaf of the
    instance path, so its parent (one segment up) is the FB whose
    type we want.
    """
    trimmed = namespace.rstrip(".")
    idx = trimmed.rfind(".")
    return trimmed[:idx] if idx >= 0 else ""


def fetch_symbols(net_id: str, port: int) -> list:
    """Connect, dump symbol table, return the parsed list."""
    plc = pyads.Connection(net_id, port)
    plc.open()
    try:
        t0 = time.perf_counter()
        symbols = plc.get_all_symbols()
        elapsed_ms = (time.perf_counter() - t0) * 1000.0
        print(
            f"[ads] downloaded {len(symbols)} symbols from "
            f"{net_id}:{port} in {elapsed_ms:.1f} ms"
        )
        return symbols
    finally:
        plc.close()


def build_lookup(symbols: Iterable) -> dict[str, str]:
    """Build the map ``instance_path -> symbol_type`` (FB type name)."""
    return {s.name: s.symbol_type for s in symbols}


def summarise(lookup: dict[str, str]) -> None:
    """Print quick stats so the user can spot whether the table is
    well-formed before chasing a specific lookup."""
    print(f"[stats] unique paths       : {len(lookup)}")

    type_counts = Counter(lookup.values())
    print(f"[stats] distinct types     : {len(type_counts)}")
    print("[stats] top 10 most common types:")
    for t, n in type_counts.most_common(10):
        print(f"          {n:>5}  {t}")

    fb_paths = [p for p, t in lookup.items() if t.startswith("FB_") or t.startswith("PRG_")]
    print(f"[stats] FB / PRG instances : {len(fb_paths)}")
    print("[stats] first 5 FB / PRG paths:")
    for p in fb_paths[:5]:
        print(f"          {p}  ->  {lookup[p]}")


def resolve(lookup: dict[str, str], namespace: str) -> str:
    """Resolve a tc-otel namespace to the OTel scope name.

    The namespace tc-otel sees ends in the FB_Log / FB_Tracer /
    FB_Metrics instance variable. Drop that segment and decide:

    1. **Parent is an FB instance** (in the symbol table) → use the
       parent's *type name*, e.g. ``Cell1.Axis1.Motor`` →
       ``FB_Motor``. Same FB type across the whole project buckets
       under one scope.

    2. **Parent is a PRG** (TwinCAT program) — PRGs aren't listed
       as instance entries in the symbol table, only their members
       are. Fall back to the parent path string itself, e.g.
       ``PRG_TestSimpleApi.logger`` → scope = ``PRG_TestSimpleApi``.
       Per-PRG buckets, which is the right granularity for top-level
       application code.

    3. **No parent** (single-segment namespace, e.g. user supplied
       ``FB_Log('Drives.Motor')`` directly) → use the raw namespace
       as scope. Custom labels survive the round-trip unchanged.
    """
    print(f"[resolve] namespace={namespace!r}")
    parent = parent_path(namespace)
    if not parent:
        # Single-segment / no parent — treat the whole string as
        # the scope name (custom-label case).
        print(f"          (no parent; treating namespace as scope) -> {namespace}")
        return namespace
    print(f"          parent_path={parent!r}")
    parent_type = lookup.get(parent)
    if parent_type:
        # Parent is an FB instance — use its TYPE as scope.
        print(f"          (parent is FB instance of type) -> {parent_type}")
        return parent_type
    # Parent isn't a symbol-table entry — almost always a TwinCAT
    # PRG. Use the parent path as a stable per-PRG scope.
    print(f"          (parent likely PRG; using raw path)   -> {parent}")
    return parent


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument(
        "--net-id",
        default="127.0.0.1.1.1",
        help="Target AMS Net ID (default: local 127.0.0.1.1.1)",
    )
    p.add_argument(
        "--port",
        type=int,
        default=851,
        help="Target ADS port (default: 851 = first PLC runtime)",
    )
    p.add_argument(
        "--query",
        action="append",
        default=[],
        help=(
            "Instance path to resolve. Repeat to test multiple paths. "
            "Example: --query Cell1.Axis1.Motor.fbLog"
        ),
    )
    args = p.parse_args()

    print(f"[ads] connecting to {args.net_id}:{args.port}")
    symbols = fetch_symbols(args.net_id, args.port)
    lookup = build_lookup(symbols)
    summarise(lookup)

    if args.query:
        print()
        for q in args.query:
            resolve(lookup, q)
            print()


if __name__ == "__main__":
    main()
