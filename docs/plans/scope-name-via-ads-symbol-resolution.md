# InstrumentationScope.name via ADS Symbol Resolution

## Goal

Auto-resolve OTel `InstrumentationScope.name` to the parent FB's
**actual type** (e.g. `FB_Motor`) by querying the PLC's symbol table
over ADS at the tc-otel resolver layer — fully automatic, no PLC-side
naming convention required, no explicit constructor parameter.

Logs are the proof-of-concept (no wire-bump needed — the namespace
already rides on `entry.logger`). Traces and metrics follow only if
Phase 1 holds up empirically.

## Why

Two earlier auto-derivation strategies were considered and rejected:

1. **PLC-side `__POUNAME()`**. From inside `FB_Log` returns `FB_Log`,
   not the parent. Useless.
2. **Last segment of instance path**. Relies on the user naming the
   FB variable after the type (`Motor : FB_Motor`). Not enforceable
   — users routinely do `fbMotor : FB_Motor` or `Spindle : FB_Motor`.

The only reliable source of the parent FB's type is the PLC's symbol
table, which tc-otel can already query via ADS
(`tc-otel-ads/src/ads_client.rs::read_symbol_info` — already used by
the web-UI symbol browser).

## Existing infrastructure

- `crates/tc-otel-ads/src/ads_client.rs::AdsClient` — full ADS
  read/write client over local-router or TCP, with
  `read_symbol_info` + symbol-table download wired for the web UI.
- Symbol-table parser landed for the browse endpoint
  (`crates/tc-otel-ads/tests/browse_parser.rs`); produces
  `{name, type_name, ...}` entries — exactly what we need.
- `TaskRegistry` already tracks `(net_id, OnlineChangeCnt)` per task
  and signals online-change events via the registration messages.

The resolver is a thin layer on top, not a from-scratch ADS client.

## Architecture

### Logs proof-of-concept (no wire change)

The PLC log path already carries the namespace as `entry.logger`:
- Default: auto-derived from `{attribute 'instance-path'}` in
  `FB_Log.FB_Init` (e.g. `PRG_TestSimpleApi.logger`).
- Custom: user-passed via `FB_Log('Drives.Motor')` — a string literal
  that is *not* a symbol-table path.

Resolver flow on every incoming log:

```
LogRecord.scope_name (was = entry.logger)
   ↓
ScopeResolver.resolve(net_id, entry.logger)
   ├─ cache hit → return cached value (positive or negative)
   └─ cache miss → schedule async ADS lookup
        ├─ strip last segment (= the FB_Log var, e.g. `.fbLog`)
        ├─ SYM_INFOBYNAMEEX(net_id, parent_path)
        ├─ Some(type_name) → cache (entry.logger → type_name)
        └─ None / not-a-symbol → cache (entry.logger → entry.logger)
                                  i.e. negative cache, treat as
                                  custom name, never re-query
```

Records arriving before the first cache fill emit with the raw
`entry.logger` (= the namespace). After ~1 ADS round-trip, all
subsequent records from that namespace get the resolved type.

### Cache semantics

- **Key**: `(net_id, namespace_string)`.
- **Value**: `Either(type_name, namespace_string_unchanged)`. Both
  outcomes cached identically; the consumer doesn't care whether
  the resolver hit or missed in ADS.
- **Invalidation**: when `RegistrationMessage.online_change_count`
  bumps for a `net_id`, drop all entries for that net_id —
  symbol layout may have changed.
- **TTL**: 1 h on the cache map itself so long-silent net_ids
  don't keep stale entries forever.

### Failure modes

- ADS fetch fails (PLC unreachable, port closed, transient): cache
  the failure as "use raw value", emit the next batch with raw
  namespace. Backfill on next online-change cycle if the network
  recovers.
- Symbol table doesn't contain the parent path (rare — non-
  introspectable FB or custom name): cached as raw, never re-queried.

## Phased delivery

### Phase 1 — Logger resolver (Rust only, no PLC change)

1. New `tc-otel-service/src/scope_resolver.rs`:
   - `pub struct ScopeResolver` — cache + lazy ADS client per net_id.
   - `async fn resolve(&self, net_id, namespace) -> String` — see
     architecture flow above.
2. Wire into `LogDispatcher`: before the LogRecord ships, call
   `resolver.resolve(net_id, &record.scope_name)` and replace
   `scope_name` with the resolved value.
3. Tests: mocked ADS client, verify (a) hit, (b) miss → cache
   raw, (c) cache invalidation on online-change.

This phase ships independently. Filter `scope.name="FB_Motor"`
matches every Motor instance regardless of variable naming.

Tradeoffs to validate empirically before proceeding:
- ADS round-trip latency under typical IPC load (~10–100 ms expected)
- Cache hit-rate for a representative project
- Symbol-table size / fetch cost on the first miss
- Behaviour with custom-named loggers (the negative-cache path)

### Phase 2 — Extend to traces + metrics (only after Phase 1 holds)

Traces and metrics don't currently carry the instance path. Wire-
bump needed:

- `FB_TcOtelTracer.Begin` event — append `code.namespace` string
  (the tracer's `{attribute 'instance-path'}` value).
- `FB_Metrics` descriptor frame — append `code.namespace` string.
- Rust parsers extended to read the new field; tests cover the
  bumped wire format.
- `TraceRecord` and `MetricRecord` gain `scope_name: String`,
  defaulting to the namespace; the resolver from Phase 1 stamps
  the resolved type on all three pillars uniformly.
- Encoders bucket per `(resource, scope_name)` (mirrors the log
  encoder).

PLC + Rust deploy in lockstep, same coordinated-redeploy pattern
PR #100 used.

### Phase 3 — Optional prefetch (perf polish)

User correctly noted prefetch on registration doesn't help: the
PLC-side namespaces emerge only at runtime as records arrive, so
the resolver doesn't know what to look up at registration time.

What *is* possible: download the full symbol table once on first
cache miss (one ADS read transfers all symbols, ~100 KB–1 MB) and
serve every subsequent lookup from RAM. Beats per-namespace
SYM_INFOBYNAMEEX round-trips when the project has many components.

Defer until Phase 1 reveals whether the per-namespace path is fast
enough — a small project may never need it.

## Out of scope

- gRPC inbound side. Same as D4 in the OTel-conformance audit:
  receiver is logs-only and no current use case.
- `service.namespace` resource attribute. OTel sem-conv has it; we'd
  map to whatever the user wants (PLC project group, task scheduler).
- Symbol-table caching policy beyond `(net_id, OnlineChangeCnt)`
  invalidation — could become smarter (partial-tree refresh) once
  we see real-world miss patterns.

## Risks

1. **ADS fetch latency under load**: first record per
   `(net_id, namespace)` is unscoped. Mitigate by emitting the raw
   namespace immediately while the lookup runs async — users still
   see *something* useful, just not the type name on the very first
   record from a new component.
2. **PLC ADS port unreachable** in some MQTT topologies (PLC pushes
   outbound only). Detect, fall back to raw namespace permanently
   for that net_id. No record loss.
3. **Wire-bump risk for Phase 2** — same coordinated PLC + Rust
   redeploy pattern PR #100 used.

## Branch & PR plan

| PR | Branch | Phase | Wire change |
|----|--------|-------|-------------|
| 1 | `feat/scope-resolver-logs` | Phase 1 — resolver + log integration | none |
| 2 | `feat/scope-resolver-traces-metrics` | Phase 2 — wire-bump, extend pillars | yes |
| 3 | `feat/scope-resolver-prefetch` | Phase 3 — full-table cache | none |

Phase 1 ships first and proves the architecture on logs — high-value
without any wire-format risk. Phase 2 follows only if Phase 1 holds
empirically (cache-hit rate, ADS latency, custom-name behaviour).
