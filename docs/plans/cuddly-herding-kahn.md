# OTel-Conformance Analysis & Action Plan for tc-otel

## Context

After Phases P0–P4 of the OTLP-Protobuf rollout, tc-otel speaks
OTLP-Protobuf for all three pillars and ingests directly into
VictoriaLogs / VictoriaMetrics / VictoriaTraces without a sidecar.
Two structural questions remain before the implementation can be
called OTel-conformant:

1. **Five significant wire-level deviations** from the OTLP /
   OpenTelemetry standard were identified. Are any of them worth
   fixing, or are they intentional trade-offs?
2. **Several attribute keys are semantically mismapped** — they use
   reserved OTel Semantic-Conventions names but carry data with the
   wrong meaning. Downstream tooling that follows the OTel registry
   gets misled. A redesign of the attribute schema is needed.

This document is **analysis + recommendations**, not an implementation
plan yet. Each deviation gets a recommendation; the user picks which
ones move into a concrete action-plan.

## Already done

- **Default `WireFormat` flipped to `Protobuf`** in
  `crates/tc-otel-core/src/config.rs`. OTLP/HTTP spec makes protobuf
  mandatory and JSON optional; `Content-Type: application/x-protobuf`
  is the spec default. JSON is now an explicit opt-in via
  `format = "json"`. Doc-comment updated to reflect the rationale.
  Ship-note: bump CHANGELOG to call out the migration ("set
  `format = json` if you point at Loki / a JSON-only collector").

---

## Part 0 — Open-issue: `tc.task.cycle_count` 3-way fragmentation

User observed in VM UI screenshot: same metric name produces **three
distinct active timeseries** with different label sets:

| Series | `plc.ams_net_id` | `plc.ams_source_port` |
|---|---|---|
| 1 | `172.28.41.37.1.1` | `350` |
| 2 | `172.28.41.37.1.1` | *(absent)* |
| 3 | *(absent)* | *(absent)* |

### What the code says

Repository grep finds **only two emit sites** of
`MetricEntry::sum("tc.task.cycle_count", …)`:

- `crates/tc-otel-service/src/diagnostics_bridge.rs:126` (TaskStats
  handler — `cycle_counter as f64`, `is_monotonic = true`).
- `crates/tc-otel-service/src/diagnostics_bridge.rs:488`
  (`batch_to_metrics`, TaskDiagBatch handler — `cycle_count_end as f64`).

Both call sites are wrapped in
`with_task(net_id_str, task_port, &task_name, …)`
(`diagnostics_bridge.rs:671`), which unconditionally writes
`m.ams_net_id = net_id` and `m.ams_source_port = task_port`.
`net_id_str = target_net_id.to_string()`
(`diagnostics_bridge.rs:61`); `AmsNetId::Display`
(`crates/tc-otel-ads/src/ams.rs:69`) always produces
`"a.b.c.d.e.f"` — never an empty string. The dispatcher
(`crates/tc-otel-service/src/dispatcher.rs:411-428`) calls
`MetricMapper::apply` which never touches `ams_net_id` /
`ams_source_port` (`crates/tc-otel-core/src/metric_mapper.rs:45-67`)
and is anyway a no-op when `custom_metrics: []`.
`MetricRecord::from_metric_entry`
(`crates/tc-otel-core/src/models.rs:248,254`) skips the resource
labels only when `entry.ams_net_id.is_empty()` /
`entry.ams_source_port == 0`.

**Conclusion from code**: every current emit path produces a record
with a non-empty `plc.ams_net_id`. Neither the OTLP/HTTP nor the
gRPC receiver accepts inbound metrics, so no external pusher
contributes. There is no second `MetricMapper`-style mutator
between the entry and the encoder.

### Three hypotheses, prioritised by plausibility

**H0 — the deployed `tc-otel.exe` on 172.18.129.178 is older than
the current source tree.** Recent branch commits are UI-only
(`feat(ui): domain-centric …`, `fix(ui): translate …`) — no
service rebuild+redeploy. If the running binary predates P1.1A
(the empty-label drop) or predates the `with_task` consolidation,
it can produce the series-2 and series-3 label shapes live, even
right now. This is the most likely explanation for "active in
last hour" combined with grep finding zero such emit paths in
HEAD.

**H1 — series 2 & 3 are stale grandfathered series.** VM default
retention is 30 days. Even after a fresh redeploy, samples written
by an older binary remain queryable as their own series identity
until retention ages them out. "Active" status in VM UI can also
be misleading if the time range includes any old sample.

**H2 — a current emit path bypasses `with_task`.** No grep evidence
in `Z:\Open Source\log4TC\crates\**`. Only possible if a sister
tool on the IPC pushes the same metric name directly into VM
(outside this repo). Lowest plausibility.

### Diagnostic sequence (read-only, before P5 ships)

0. **Rebuild and redeploy first.** Run
   `cargo build --release -p tc-otel-service`, copy
   `target\release\tc-otel.exe` into `dist\local-router\`, push to
   the IPC, restart the `tc-otel` Scheduled Task. Without this,
   every observation conflates source-state vs deployed-state.

1. **VM last-sample-per-series query** (non-destructive):
   ```
   {__name__="tc.task.cycle_count"}
   ```
   over a short range (e.g. 5 minutes after the redeploy). For
   each of the three series, record the *last* sample timestamp.
   - All three series last-sample > redeploy time → H2 holds, hunt
     for the rogue emitter.
   - Only series 1 has fresh samples; series 2 & 3 are frozen at
     timestamps before the redeploy → H0/H1 confirmed; old shapes
     age out naturally.

2. **If H2 holds after the redeploy:** add a one-line
   `tracing::warn!` in `MetricDispatcher::dispatch`
   (`crates/tc-otel-service/src/dispatcher.rs:411`) gated on
   `entry.name == "tc.task.cycle_count"
    && (entry.ams_net_id.is_empty() || entry.ams_source_port == 0)`,
   logging once per occurrence. Identify the path, patch at source.
   This is **not part of P5** — it would be a separate fix.

### What this means for the rest of the plan

**P5.1+P5.2 do NOT promise to retroactively collapse the screenshot's
three series.** They guarantee that *future* samples from the
freshly built binary go to a single canonical shape. Old shapes
remain in VM as their own timeseries identity until VM retention
(30 days) expires them. If the user wants immediate cleanup, an
explicit `delete_series` API call is required and is **out of P5
scope** — propose only on user request.

**Recommendation**: do the rebuild+redeploy in step 0 *before*
opening PR-A. Document the post-redeploy last-sample timestamps
in PR-A's description so reviewers see the empirical state. No
additional code change is added speculatively.

---

## Part 1 — The five significant wire-level deviations

### D1. JSON was the default (RESOLVED)

**Was**: `WireFormat::default() == Json`. **Now**: `Protobuf`.
OTLP-spec-conformant. Closed.

### D2. VictoriaLogs JSONL fast-path bypasses OTLP entirely

**Symptom**: when `export.endpoint` matches `/insert/jsonline`,
`crates/tc-otel-service/src/dispatcher.rs::flush_batch` POSTs raw
NDJSON with `Content-Type: application/stream+json` instead of any
OTLP shape (`flush_batch`, line 207 onwards).

**Why it exists**: VictoriaLogs ingests JSONL ~3× faster than OTLP
JSON because there's no per-record `ResourceLogs` / `ScopeLogs`
wrapping overhead. For high-volume PLC log streams this matters.

**Trade-off**:

| Keep JSONL | Drop JSONL, force OTLP |
|---|---|
| Fast (raw line-per-log POST) | Spec-conformant |
| Tied to VL's specific schema (no portability) | Same wire format works with any OTLP-Logs receiver |
| Cannot use the same endpoint with otel-collector | Single code path, fewer corner cases |

**Recommendation**: **KEEP** as a non-default opt-in path. The
config currently uses the JSONL endpoint; switching the dist to
`http://127.0.0.1:9428/insert/opentelemetry/v1/logs` (VL's
OTLP-Logs ingest) makes it OTLP-conformant without losing VL.
Document JSONL as the explicit "VL-only fast path".

**Effort**: ~10 lines — change the dist `config.json` endpoint and
update the comment in `dispatcher.rs::is_otlp_endpoint`. Verify VL
accepts OTLP-Protobuf-Logs at `/insert/opentelemetry/v1/logs`
(documented as supported).

### D3. Resource grouping uses only the first record's resource

**Symptom**: in
`metrics_proto::build_request` (line ~62), `traces_proto::build_request`
(line ~50), and `logs_proto::build_request` (line ~50), one
`Resource` block is built from `records[0].resource_attributes` and
applied to the **whole batch**. Records with different
`plc.ams_net_id` etc. silently get merged under the first record's
resource.

**Real-world risk**: low — tc-otel batches usually come from one
local-router connection (one PLC's NetId), so all records in a
batch share the same resource. **But**: when MQTT transport
aggregates frames from multiple TwinCAT systems through a single
broker, a batch can mix resources. The current encoder loses that
fidelity.

**Trade-off**:

| Group by resource hash | Stay with first-record |
|---|---|
| Spec-correct in all topologies | ~10 lines, simple |
| ~30 LoC per encoder, HashMap allocation per batch | Wrong on multi-PLC MQTT batches |
| Negligible perf overhead (resource-attr cardinality is ~5–10) | Already shipped |

**Recommendation**: **FIX** in all three encoders. The pattern is
simple: `HashMap<resource_hash, Vec<&Record>>`, one
`ResourceMetrics`/`Spans`/`Logs` block per group. Deterministic
hash via stable `BTreeMap` over the resource attribute key/value
pairs. Tests easy to add (multi-resource batch round-trip).

**Effort**: ~30 LoC per encoder × 3 = ~90 LoC + 3 tests. ~½ day.

### D4. gRPC OTLP receiver only accepts Logs

**Symptom**: `crates/tc-otel-export/src/grpc.rs` defines
`LogsServiceServer` (~line 464) using hand-rolled prost types.
`MetricsServiceServer` and `TraceServiceServer` are not implemented.
The `client-bridge` feature in `main.rs` documents this as
"inbound logs only".

**Real-world risk**: depends on whether anyone wants to push
metrics/traces INTO tc-otel (vs PLC-only emission). The TwinCAT
integration today produces all telemetry via ADS, never via
external OTLP push. The gRPC receiver was added for log-only
forwarding from a sister application, not as a general OTLP
ingest.

**Trade-off**:

| Add Metrics + Traces gRPC servers | Stay logs-only |
|---|---|
| Full OTLP/gRPC compliance | YAGNI — no current use case |
| ~600 LoC (server + dispatch + tests) | Footprint stays small |
| Risk: opens a new attack surface (must be hardened) | One less protocol endpoint to defend |

**Recommendation**: **DEFER** until someone has an actual use case.
Document the limitation in the gRPC receiver section of the
README (`crates/tc-otel-export/src/lib.rs` doc-comment is misleading
right now — says "OTLP gRPC endpoint (4317)" without "logs only").
**Effort to document only**: 5 minutes.

### D5. No HTTP compression

**Symptom**: `OtelExporter` POSTs request bodies uncompressed.
`Content-Encoding: gzip` (allowed by OTLP/HTTP spec) is never set.

**Real-world impact for IPC use-case**: low — tc-otel and the
Victoria stack run on the same machine over loopback TCP. Gzip
saves bandwidth, not cost; for loopback the CPU cost of gzip
(~5 µs/KB at level 1) is comparable to the bandwidth saved
(~5 GB/s loopback → no real savings). The win shows up on
remote-tc-otel deployments (TCP transport) where the wire is the
bottleneck.

| Add gzip | Skip |
|---|---|
| ~5–10× smaller wire bytes for big batches | One less moving part |
| CPU cost worth it for >1 KB payloads | Loopback users see no gain |
| Some backends require Accept-Encoding: identity | We'd add a config flag |

**Recommendation**: **DEFER** — make it a follow-up enhancement
after the Sem-Conv cleanup. Behind a config flag
(`export.compression: "none" | "gzip"`) defaulting to `none` to
avoid breaking unusual receivers. Worth doing for production
deployments that span LAN.

**Effort**: ~50 LoC + tests, ~½ day.

### Summary — D1–D5

| ID | Deviation | Priority | Recommendation | Effort |
|---|---|---|---|---|
| D1 | Default = Json | — | DONE (default → Protobuf) | done |
| D2 | VL JSONL fast-path | LOW | Keep but make endpoint configurable to OTLP-Logs path; document as opt-in | ~10 LoC |
| D3 | First-record resource grouping | **MEDIUM** | Fix — group by resource hash in all 3 encoders | ~½ day |
| D4 | gRPC receiver logs-only | LOW | Document the limitation; defer implementation | 5 min |
| D5 | No compression | LOW | Defer; add gzip behind a flag in a later iteration | ~½ day |

---

## Part 2 — Custom Sem-Conv re-validation

Audit of `crates/tc-otel-core/src/models.rs::from_*_entry` outputs
and the call sites that populate `Entry` structs. Three categories
emerged:

- **CRITICAL mismaps** (3): OTel-reserved keys carry semantically
  wrong data. Downstream tooling that follows the OTel registry
  reads them and gets confused.
- **HIGH mismaps** (3): keys with subtly wrong meaning that
  surprise OTel-aware operators.
- **LOW issues** (2): redundancy / cosmetic.

### CRITICAL mismaps

#### M1. `process.pid` carries `entry.task_index` (LogRecord)

`models.rs::from_log_entry` (~line 826) sets `process.pid =
entry.task_index`. **OTel sem-conv** for `process.pid`: "Process
identifier (PID)" — operating-system-level integer (typically 4–5
digits on Linux, 1000s+ on Windows). tc-otel emits 1–10 (PLC task
slot index).

**Fix**: rename to `tc.task.index` (custom namespace). Drop
`process.pid` entirely from the LogRecord resource — the
TwinCAT runtime is itself a single OS process; one tc-otel instance
maps to one PLC, the OS PID concept doesn't fit per-task records.

#### M2. `process.command_line` carries `entry.task_name` (LogRecord)

`models.rs::from_log_entry` (~line 830) sets `process.command_line =
entry.task_name` (e.g. `"PlcTask"`). **OTel sem-conv**: full process
invocation line. tc-otel emits the PLC task name.

**Fix**: rename to `tc.task.name`. Resource attribute lives at
the level of the PLC task, not the OS process.

#### M3. `service.instance.id` carries `entry.app_name` (all three records)

Resource builder at `models.rs:215, 527, 807` sets
`service.instance.id = entry.app_name`. **OTel sem-conv**: "MUST be
unique for each instance of the same `service.namespace,service.name`
pair" — typically a UUID, pod-name, or container-id. tc-otel emits
the user-friendly app name (e.g. `"HydraulicSystem"`), which
collides across replicas and is **not** a unique instance ID.

**Fix**: derive a stable per-PLC-task instance ID using AMS Net Id
*and* source port — the user clarified that the source port is
needed to distinguish PLC tasks running on different ADS ports
under the same NetId. Composition rule:

```rust
match (app_name.is_empty(), ams_net_id.is_empty(), ams_source_port == 0) {
    (false, false, false) => format!("{app_name}@{ams_net_id}:{ams_source_port}"),
    (false, false, true)  => format!("{app_name}@{ams_net_id}"),
    (false, true,  _)     => app_name.clone(),                  // unusual fallback
    (true,  false, false) => format!("{ams_net_id}:{ams_source_port}"),
    (true,  false, true)  => ams_net_id.clone(),
    (true,  true,  _)     => /* skip the attribute entirely */,
}
```

Examples:
- `HydraulicSystem@172.28.41.37.1.1:851` — full per-task identity
- `HydraulicSystem@172.28.41.37.1.1` — PLC without per-task port
- `172.28.41.37.1.1:851` — task identity when app_name not yet known
  (early frames before registration arrives)

This guarantees: stable across tc-otel restarts (NetId+port don't
move), unique per PLC task even when multiple tasks share the same
`app_name` / `service.name`, and human-readable.

### HIGH mismaps

#### M4. `host.name` is `format!("plc-{}", source_net_id)` (all three records)

`crates/tc-otel-ads/src/listener.rs:179` and the AMS-receive paths
in `router.rs:238` populate `entry.hostname` as `"plc-<netid>"`.
**OTel sem-conv** for `host.name`: FQDN or hostname of the host the
data was collected from — i.e. the IPC running tc-otel, NOT the PLC
identity (that's `service.instance.id` / `plc.ams_net_id`).

**Fix**: `host.name` should be the local IPC's actual hostname
(`hostname::get()` on Rust side). The PLC's identity already lives
in `plc.ams_net_id` (resource) and `service.instance.id` (after
M3 fix). Strip the synthetic `plc-{netid}` value entirely.

#### M5. `source.address` is the AMS Net ID (LogRecord, MetricRecord, TraceRecord)

`models.rs:264, 359, 864` set `source.address = entry.source` where
`entry.source` is the AMS Net ID string (`192.168.1.1.1.1`).
**OTel sem-conv** for `source.address`: "Source address — domain
name if available without reverse DNS lookup; otherwise IP address
or Unix domain socket name." Usually IP:port.

**Fix**: extract the actual IP from the TCP peer addr or transport
metadata (we have it in `listener.rs`); store as `source.address`
(real IP) or `network.peer.address` (newer sem-conv). Move the AMS
Net ID to the dedicated `plc.ams_net_id` (which is already in the
resource).

#### M6. `level` (lowercase severity) duplicates `severity_text`

`models.rs:889` adds `level = severity_text.to_ascii_lowercase()` to
log_attributes as a Grafana workaround. OTLP already provides
`severity_text` ("INFO") and `severity_number` (9). The custom
`level` attribute is non-standard and was a hack for a Grafana
parser limitation that may no longer apply.

**Fix**: drop the attribute. If a Grafana panel still needs lowercase,
do it client-side via `lower(severity_text)` in LogsQL.

### LOW issues

#### M7. Resource attributes also duplicated as per-record attributes

`plc.ams_net_id` and `plc.ams_source_port` get inserted both at
resource scope (models.rs:249-258) AND at per-record scope when
non-empty. That's needless cardinality and contributed to the
4-series fragmentation we already partially fixed in P1.1A.

**Fix**: drop the per-record copies. Resource scope is enough
once we trust the post-P1.1A skip-empty rule.

#### M8. `plc.timestamp` is the LESS precise sibling of the toplevel timestamp — VERIFIED

User flagged this for verification: are the two timestamps actually
the same DC-µs-precise value, or different? Answer is in the source
itself, models.rs:900-904 doc-comment on `LogRecord::from_log_entry`:

> `clock_timestamp` carries the cycle-accurate DC time from the PLC
> (ns-precise, same source the push-diag samples use).
> `plc_timestamp` is the ~100 ms FILETIME from GETSYSTEMTIME kept
> for compatibility.

So:
- **Toplevel `LogRecord.timestamp = entry.clock_timestamp`** —
  DC-time, **ns-precise**, cycle-accurate. Same source as
  push-diagnostics samples. **This is the µs-precision the user
  needs** — already correctly populated. ✓
- **`plc.timestamp` log_attribute = `entry.plc_timestamp.to_rfc3339()`** —
  Windows FILETIME from `GETSYSTEMTIME`, refresh rate ~100 ms.
  **NOT µs-precise**. Lower-fidelity wall-clock.

The two are derived from **different PLC clocks** (DC subsystem vs.
GETSYSTEMTIME), so they're semantically different — but the
*precise* one users want for analysis is already in the OTLP
toplevel `timestamp` field.

**Fix**: drop `plc.timestamp` log_attribute. The required µs
precision is already in the OTLP-standard `timestamp` field via
`clock_timestamp`. The 100ms FILETIME has no real analytical
value (it's just a rounded version of the same wall-clock instant
the precise DC time already carries) and confusingly suggests it
might be more authoritative than the toplevel timestamp.

If anyone later wants the raw DC time as a u64 ns counter for
cycle-tick math (rather than a chrono::DateTime), a separate
`tc.task.dc_time_ns` log_attribute can be added carrying that
unprocessed value. **Not part of P5.1** — only on demand.

### Concrete remap table

| Today | After remap | Why |
|---|---|---|
| `process.pid` (= task_index) | `tc.task.index` (custom ns) | OTel-pid is OS-level, ours is PLC-task; rename to fit |
| `process.command_line` (= task_name) | `tc.task.name` | Same — OTel-cmdline is OS-level |
| `service.instance.id` (= app_name) | `service.instance.id` = `app_name@ams_net_id:ams_source_port` (with sensible fallbacks when fields empty) | Make instance ID actually unique per PLC task |
| `host.name` (= "plc-<netid>") | `host.name` = local IPC hostname; PLC identity already in `plc.ams_net_id` | host.name is *where data was collected*, not who produced it |
| `source.address` (= ams_net_id) | `source.address` = real peer IP | Sem-conv expects IP, not opaque AMS string |
| `level` (lowercase) | dropped | duplicates `severity_text` |
| `plc.ams_net_id` in per-record attrs | dropped from per-record (kept in resource) | resource is the right scope |
| `plc.ams_source_port` in per-record attrs | dropped from per-record | same |
| `plc.timestamp` log_attr (FILETIME ~100ms) | dropped | toplevel `timestamp` already carries the µs-precise DC time (`entry.clock_timestamp`); FILETIME variant is a low-resolution duplicate, not an extra signal |

`task.name`, `task.index`, `task.cycle`, `online.changes`, `arg.0..N`,
`logger.name`: KEEP — these are valid custom-namespace attributes
without OTel-Sem-Conv conflicts.

---

## Part 3 — Recommended action plan (priority-ordered)

### P5.1 (CRITICAL — semantic-correctness fix, ~½ day)

Rename mismapped keys in `crates/tc-otel-core/src/models.rs`:
- `from_log_entry`: drop `process.pid`, `process.command_line`,
  `level` from log_attributes; add `tc.task.index`, `tc.task.name`
  to log_attributes (only when non-zero / non-empty).
- All three `from_*_entry`: change `service.instance.id` builder to
  `format!("{app_name}@{ams_net_id}:{ams_source_port}")` (full
  per-task identity). When `ams_source_port == 0` drop the `:port`
  suffix; when `app_name` is empty fall back to
  `{ams_net_id}:{port}` only; when `ams_net_id` is empty fall back
  to `app_name` only; when all three empty, skip the attribute.
- All three: change `host.name` source from `entry.hostname`
  (currently `plc-<netid>`) to a process-wide cached
  `gethostname::gethostname()`. Wire that hostname through at
  `TcOtelService::new`; pass into the dispatchers; never let the
  per-record `entry.hostname` override it. Drop the
  `format!("plc-{}", net_id)` synthesis in
  `crates/tc-otel-ads/src/listener.rs:179` and
  `crates/tc-otel-ads/src/router.rs:238` — leave `entry.hostname`
  empty there; the dispatcher fills it.
- All three: change `source.address` source from `entry.source`
  (AMS Net ID) to the actual peer IP captured in
  `crates/tc-otel-ads/src/transport/{tcp.rs,local_router.rs}` and
  threaded into the entry. Keep `plc.ams_net_id` for the AMS-level
  identity.

Tests: update the existing `models::tests::test_*_record_*` — the
fixtures that assert `process.pid == 1` etc. need to be reframed
around the new `tc.task.*` keys. ~10 test edits.

### P5.2 (HIGH — duplication cleanup, ~30 LoC)

In `models.rs::from_log_entry` and `from_metric_entry`,
`from_span_entry`: drop the duplicate `plc.ams_net_id` /
`plc.ams_source_port` inserts at per-record scope. Resource scope
already carries them after P1.1A. Drop `plc.timestamp` log_attribute.

### P5.3 (MEDIUM — D3 wire-correctness, ~½ day)

Per-resource batch grouping in all three encoders (`metrics_proto`,
`traces_proto`, `logs_proto`). Hash resource attrs (BTreeMap-based
for stable hash), bucket records, emit one `Resource*` block per
bucket. Add multi-resource roundtrip tests.

### P5.4 (LOW — D2 docu/config nudge, ~10 LoC)

Switch `dist/local-router/config.json` `export.endpoint` from
`http://127.0.0.1:9428/insert/jsonline` to
`http://127.0.0.1:9428/insert/opentelemetry/v1/logs` so the dist
deploys spec-conformant out of the box. Keep the JSONL fast-path
detection in `dispatcher.rs::is_otlp_endpoint` as a documented
opt-in for users who want the higher VL throughput.

### Out of P5 scope (separate roadmap)

- D4: OTLP/gRPC Metrics + Traces receivers — only document the
  current limitation in `crates/tc-otel-export/src/lib.rs` doc.
- D5: gzip compression behind a flag — separate iteration.
- DELTA aggregation temporality, exponential histograms, span
  links — only when a real use case appears.

## Critical files

| File | Reason |
|---|---|
| `crates/tc-otel-core/src/models.rs` | All three `from_*_entry` builders — primary surface for the attribute remap |
| `crates/tc-otel-ads/src/router.rs` | Lines 230–365: where `entry.hostname` gets the `format!("plc-{}", ...)` synthesis. Drop the hostname write here. |
| `crates/tc-otel-ads/src/listener.rs:179` | Same issue at the AMS-TCP receive path. |
| `crates/tc-otel-service/src/service.rs` | `TcOtelService::new` is the right place to read `gethostname` once and thread it down. |
| `crates/tc-otel-ads/src/transport/{tcp.rs,local_router.rs}` | Where the real peer IP is available — needs to be carried onto the entry as `source.address` value. |
| `crates/tc-otel-export/src/{metrics_proto,traces_proto,logs_proto}.rs` | P5.3: per-resource batch grouping. |
| `dist/local-router/config.json` | P5.4: switch logs endpoint. |
| `crates/tc-otel-export/src/lib.rs` doc-comment | D4: clarify "logs-only" gRPC receiver. |
| `Cargo.toml` (root) | new dep: `gethostname = "0.5"` for the `host.name` fix. ~50 KB. |

## Verification

Per-fix unit tests + one end-to-end protobuf-roundtrip per pillar
over a multi-resource batch (proves D3 fix and that mismapped
keys disappear).

End-to-end re-deployment to `172.18.129.178`:
- Wipe Victoria data dirs
- Restart everything
- Query VM: `select * from {__name__=~"tc.task..*"}` should show
  `tc.task.index`, `tc.task.name` resource labels, **no**
  `process.pid` / `process.command_line` anywhere.
- Query VL: `severity_text:"INFO"` returns logs; `level:"info"`
  returns nothing (key gone).
- Query VL: `host.name:"<actual hostname of the IPC>"` returns
  current logs; `host.name:"plc-172.28.41.37.1.1"` returns nothing
  for new ingests (old data with the synthetic hostname is
  grandfathered until VL retention expires).
- Verify resource cardinality: each `service.instance.id` value
  is `<app_name>@<ams_net_id>`, unique per PLC.

## User decisions taken (questions resolved)

1. **`service.instance.id`** = `format!("{app_name}@{ams_net_id}:{ams_source_port}")`
   for the full per-task identity. The user clarified the AMS
   source port is required to disambiguate multiple PLC tasks on
   the same NetId. Fallback chain documented in M3 above.
2. **`host.name`** = `gethostname::gethostname()` cached at
   `TcOtelService::new`, threaded into all dispatchers. Add
   `gethostname = "0.5"` to workspace deps (~50 KB).
3. **`plc.timestamp` removal verified**: source comment confirms
   the toplevel `LogRecord.timestamp` already carries the µs-precise
   DC time (`entry.clock_timestamp`); `plc.timestamp` log_attribute
   is the lower-resolution FILETIME (~100 ms). Dropping the
   FILETIME duplicate is safe — the µs precision the user requires
   is preserved by the standard OTLP `timestamp` field.
4. **PR split**: three PRs.
   - **PR-A**: P5.1 + P5.2 — *"OTel sem-conv attribute hardening"*
     (mismaps fixed: `process.pid`, `process.command_line`,
     `service.instance.id`, `host.name`, `source.address`, `level`;
     duplicates dropped: `plc.ams_net_id` / `plc.ams_source_port` at
     per-record scope, `plc.timestamp`).
   - **PR-B**: P5.3 — *"per-resource batch grouping in encoders"*
     (metrics_proto / traces_proto / logs_proto: hash records by
     resource attrs, emit one Resource block per group).
   - **PR-C**: P5.4 — *"switch dist logs endpoint to OTLP-Logs path"*
     (one-line `config.json` change + a comment in
     `dispatcher.rs::is_otlp_endpoint`).
