# InstrumentationScope.name via ADS Symbol Resolution

## Goal

Auto-resolve OTel `InstrumentationScope.name` for **logs, traces, and
metrics** to the parent FB's **type name** (e.g. `FB_Motor`) — fully
automatic, no PLC-side naming convention required, no explicit
constructor parameter.

## Why

Two earlier auto-derivation strategies were considered and rejected:

1. **PLC-side `__POUNAME()`**. From inside `FB_Log` returns `FB_Log`,
   not the parent. Useless.
2. **Last segment of instance path**. Relies on the user naming the
   FB variable after the type (`Motor : FB_Motor`). Convention not
   enforceable; users routinely do `fbMotor : FB_Motor` or
   `Spindle : FB_Motor`. Defeats the "filter all Motors" workflow.

The only reliable source of the parent FB's type is the PLC's symbol
table, which tc-otel can already query via ADS
(`tc-otel-ads/src/ads_client.rs::read_symbol_info` + symbol-table
download — already used by the web UI).

## Architecture

```
PLC (sends)                tc-otel (receives + resolves)
─────────────              ─────────────────────────────
F_Log(...)              → Type cache miss?
  .CreateLog()             ↓
                           ADS read SYM_INFOBYNAMEEX(net_id, parent_path)
                           → "FB_Motor"
                           ↓ stamp scope.name on subsequent records
                           ↓ cache (net_id, parent_path) → "FB_Motor"
```

Three pillars share one resolver. One symbol-table fetch per
`(net_id, OnlineChangeCnt)` covers everything.

### Wire flow

1. PLC emits `code.namespace = <auto-instance-path>` on every
   record. Already free for logs (FB_Log gains `code.namespace`
   attribute via `{attribute 'instance-path'}`); needs adding for
   `FB_TcOtelTracer` (extend Begin event) and `FB_Metrics` (extend
   descriptor frame).

2. tc-otel parses the path, strips the last segment (the FB_Log /
   FB_Tracer / FB_Metrics instance var name) — that's the parent
   FB's instance path, e.g. `Cell1.Axis1.Motor`.

3. Resolver looks up the parent path in a cached symbol table
   (one per `(net_id, OnlineChangeCnt)`); cache miss triggers an
   async ADS symbol-table fetch.

4. Once resolved, every record from this parent path is stamped
   with `scope.name = <type_name>` (e.g. `FB_Motor`).

### Cache lifecycle

- **Populate**: lazy on first cache miss for a `(net_id, occ)` pair.
  Single ADS fetch downloads full symbol table (~100 KB typical),
  parses into `HashMap<instance_path, type_name>`. Reused for the
  lifetime of that registration.
- **Invalidate**: when `RegistrationMessage.online_change_count`
  bumps, drop the cache for that net_id and re-fetch on next miss.
- **Bound**: TTL on the cache map itself (default: 1 h) so a
  silent PLC doesn't keep stale entries forever.

### Failure modes

- ADS fetch fails (PLC unreachable, port closed, transient error):
  records emit with empty `scope.name` → encoder falls back to
  the crate-default `tc-otel`. Same as today. No record loss.
- Symbol table doesn't contain the parent path (rare —
  introspectable FBs only): same fallback.
- Records arriving before the first fetch completes: emitted
  without scope.name. Acceptable — once the cache fills, all
  subsequent batches get tagged. For long-running PLCs the gap is
  the first ~50 ms.

## Existing infrastructure

What's already in tree:

- `crates/tc-otel-ads/src/ads_client.rs::AdsClient` — full ADS
  read/write client over local-router or TCP, with
  `read_symbol_info` + table download already wired for the web
  UI (`crates/tc-otel-service/src/web.rs` uses it for the symbol
  browser).
- Symbol-table parsing landed for the browse endpoint
  (`crates/tc-otel-ads/tests/browse_parser.rs`); produces a list
  of `{name, type_name, ...}` entries — exactly what we need.
- `TaskRegistry` already tracks `(net_id, occ)` pairs and signals
  online-change events through registration messages.

So the resolver becomes a thin layer on top, not a from-scratch
ADS client.

## Phased delivery

### Phase 1 — Resolver core (Rust only, no PLC changes)

1. New crate module `tc-otel-service/src/scope_resolver.rs`:
   - `pub struct ScopeResolver { client_pool, cache }`
   - `fn resolve(&self, net_id, instance_path) -> Option<String>`
     - Strips last segment, looks up parent in cache
     - On miss, fires async ADS fetch (returns None synchronously,
       fills cache for next call)
2. Wire into log dispatch: when an entry's `code.namespace` is
   set, post-process to stamp `scope_name` on the LogRecord.
3. Test against a mocked ADS client.

Lands the resolver without touching PLC. Logs that already carry
`code.namespace` (or that we synthesise from `entry.logger` as
a fallback) start picking up real type names.

### Phase 2 — PLC: add `code.namespace` everywhere

`FB_Log` already has the `{attribute 'instance-path'}` annotation.
Read the path, store as a record attribute on every `_WriteEntry`:

- Wire format: extend the V2 entry to include a final
  `code.namespace` string field (1-byte length + UTF-8). Bump the
  log type byte (or just append; entry length already framed).
- Same for `FB_TcOtelTracer.Begin` and `FB_Metrics` descriptor.

Default behaviour after Phase 2: every record carries a precise
instance path. The resolver can map that to a type via Phase 1.

### Phase 3 — Trace + metric scope.name

Extend `TraceRecord` and `MetricRecord` with `scope_name: String`
(mirrors LogRecord). Encoders bucket per (resource, scope) — same
two-level grouping as logs. Tests cover separate-scope behaviour.

### Phase 4 — Polish

- Cache prefetch on registration (bulk-fetch symbol table when a
  new PLC registers, before its first record arrives).
- Optional `code.lineno` / `code.function` if the PLC emits a stack
  frame snapshot.
- Web UI: surface "scope coverage" stats (% of records resolved
  vs. fallback).

## Out of scope for this plan

- Symbol-table caching policy beyond per-`(net_id, occ)` invalidation
  — could become smarter (e.g. partial-tree refresh) once we see
  real-world cache-miss patterns.
- gRPC inbound side. Same as D4 in the OTel-conformance audit:
  receiver is logs-only and not a current use case.
- `service.namespace`. OTel sem-conv has it; we'd map to whatever
  the user wants (PLC project group? task scheduler?). Defer.

## Risks

1. **ADS fetch latency under load** — single fetch is ~10–100 ms.
   First record per `(net_id, occ)` is unscoped. Mitigate via
   Phase-4 prefetch on registration.
2. **PLC ADS port not reachable** in some MQTT topologies — the
   PLC may only push outbound, no inbound channel for the
   resolver. Detect, fall back to pure instance-path scope (still
   useful, just not class-name).
3. **Symbol table size** — a big project can be ~1 MB. Fetch is
   one-shot and cached, but that 1 MB needs to make it across the
   ADS connection on first miss. Use Phase-4 prefetch to amortise
   into the registration flow rather than blocking a record path.

## Branch & PR plan

| PR | Branch | Phase |
|----|--------|-------|
| 1 | `feat/scope-resolver-core` | Phase 1 — resolver + log integration |
| 2 | `feat/scope-resolver-plc-namespace` | Phase 2 — PLC wire-bump |
| 3 | `feat/scope-resolver-traces-metrics` | Phase 3 — extend to other pillars |
| 4 | `feat/scope-resolver-prefetch` | Phase 4 — perf polish |

Phase 1 unblocks log scope.name with a real type name and is the
high-value first ship. Everything else is incremental.
