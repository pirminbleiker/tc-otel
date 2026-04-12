# Critical Implementation Review — Logs / Metrics / Traces Pillars

**Bead:** to-ynn
**Date:** 2026-04-12
**Reviewer:** tc_otel/polecats/capable
**Scope:** Logs pillar (stable), Metrics pillar (commit 740333e + branches polecat/capable-plc-system-metrics, polecat/capable/to-754.3@mntea4h2), Traces pillar (commits 547ebcd, ee5aa2d, a48834f).

**Files reviewed in depth:**
- `crates/tc-otel-export/src/exporter.rs` (1159 lines)
- `crates/tc-otel-export/src/grpc.rs` (996 lines, new in a48834f)
- `crates/tc-otel-export/src/mapping.rs`, `receiver.rs`, `error.rs`
- `crates/tc-otel-service/src/dispatcher.rs` (796 lines)
- `crates/tc-otel-service/src/service.rs`
- `crates/tc-otel-service/src/system_metrics.rs` (411 lines, new)
- `crates/tc-otel-service/src/cycle_time.rs`
- `crates/tc-otel-core/src/models.rs` (1763 lines)
- `crates/tc-otel-core/src/formatter.rs`
- `crates/tc-otel-ads/src/parser.rs` (2150 lines, type-4/5/6 arms)

---

## Severity Summary

| Severity | Count |
|----------|-------|
| HIGH     | 8     |
| MEDIUM   | 22    |
| LOW      | 14    |
| **Total**| **44**|

Each HIGH-severity finding has a separate fix bead created (see *Spawned beads* at the bottom).

---

## HIGH-SEVERITY FINDINGS

### H1. TLS bypass via substring match on 'otel-collector'
- **File:line:** `crates/tc-otel-export/src/exporter.rs:80-83`
- **Confirmed.** Code:
  ```rust
  let is_local = config.endpoint.contains("localhost")
      || config.endpoint.contains("127.0.0.1")
      || config.endpoint.contains("otel-collector")
      || config.endpoint.starts_with("http://");
  ```
  Any host whose name contains `otel-collector` (e.g. `evil-otel-collector.attacker.com`) is treated as local and TLS-only is disabled. Worse, any URL starting with `http://` opts out of TLS entirely — a misconfiguration silently downgrades to cleartext.
- **Fix:** Replace substring/scheme heuristics with explicit `allow_insecure` config flag (default false) plus URL parsing — strictly compare `host` against an allowlist.

### H2. TLS-only flag swallowed when client build fails
- **File:line:** `crates/tc-otel-export/src/exporter.rs:92-95`
- **Confirmed.** `builder.build().unwrap_or_else(|_| reqwest::Client::new())` silently falls back to a *default* client that does **not** enforce `https_only`. A bad system root store or a feature mismatch silently downgrades security.
- **Fix:** Propagate the build error (`?`) so misconfiguration fails loudly at startup.

### H3. trace_id / span_id JSON encoding for traces & logs
- **File:line:** `crates/tc-otel-export/src/exporter.rs:257-262`, `crates/tc-otel-core/src/models.rs:651, 691-731`
- OTLP-JSON requires `traceId`/`spanId` as **lowercase hex** (32 chars / 16 chars). The exporter emits whatever string is in `record.trace_id`. Round-trip through `LogRecord::from_log_entry` uses `[0u8;16]` sentinel to mean "no context" but that is also a **valid** W3C trace ID — ambiguity at the boundary.
- **Fix:** Centralise hex encoding in a `TraceId`/`SpanId` newtype that asserts length on construction and is serialised by a single helper. Switch the "no context" detector to `Option<TraceContext>` rather than zero-byte sentinel.

### H4. Shutdown handles SIGINT only — no SIGTERM
- **File:line:** `crates/tc-otel-service/src/service.rs:281` (also `:81` in dispatched dispatcher path)
- **Confirmed.** `tokio::signal::ctrl_c().await?;` is the only signal listener. Container orchestrators (k8s, systemd, docker stop) deliver **SIGTERM**; the service therefore never enters graceful drain in production deployments → in-flight batches are lost on every roll-out.
- **Fix:** `tokio::select!` over both `ctrl_c()` and `signal(SignalKind::terminate()).recv()`.

### H5. Spawned tasks have no panic / restart guard
- **File:line:** `crates/tc-otel-service/src/service.rs:139, 156, 191, 196, 230, 268`
- All `tokio::spawn(...)` join-handles are dropped without `JoinHandle::await` or panic logging. A panic in the AMS server, dispatcher worker, or system-metrics task vanishes silently; the service keeps running with no data flowing.
- **Fix:** Wrap each task body in `async move { ... }.instrument(...)` plus a top-level `match handle.await` that logs `JoinError::is_panic()` and triggers an orderly shutdown (or supervised restart for AMS server).

### H6. Integer overflow / unbounded position writes in parser type-4/5/6 arms
- **File:line:** `crates/tc-otel-ads/src/parser.rs:398, 502, 671, 802` (analogous in earlier arms)
- `reader.pos = entry_start + entry_length` is computed from two attacker-controlled u16 values (with `as usize` widening). There is no check that `entry_start + entry_length <= reader.data.len()`. A malformed ADS frame can advance `pos` beyond the buffer, then subsequent `peek/read` operations behave on stale state and may panic in slicing.
- **Fix:** Use `checked_add` and validate against `reader.data.len()` *before* the assignment; return `AdsError::IncompleteMessage` on overflow.

### H7. MAX_STRING_LENGTH (65536) is unreachable for a u8 length prefix
- **File:line:** `crates/tc-otel-ads/src/parser.rs:11-13` and `:888` (read_string)
- `read_string` uses a **u8** length prefix (0–255) yet `MAX_STRING_LENGTH = 65_536`. The bound check therefore never fires; a wider parser (e.g. a future u16 length) will inherit a stale, oversized limit. More importantly, the documented "symmetric MAX_STRING_LENGTH/MAX_MESSAGE_SIZE on type-4/6" is *not* symmetric: the type-4/6 arms (`:635-661`) insert attribute keys/values without enforcing either constant.
- **Fix:** Pin `MAX_STRING_LENGTH = 255` and apply both limits inside the type-4 (span event attributes), type-5 (metric labels), and type-6 (extended attributes) arms.

### H8. Unbounded HashMap growth in cycle-time tracker
- **File:line:** `crates/tc-otel-service/src/cycle_time.rs:147, 174-189`
- `tasks: HashMap<TaskKey, TaskCycleState>` is keyed by `(plc, task_index)` and never evicted. PLCs that flap on/off, or test fixtures spinning new task IDs, grow the map indefinitely → real memory leak in long-running deployments.
- **Fix:** Track `last_seen: Instant` in `TaskCycleState`; periodically prune entries older than e.g. 1 hour from a maintenance task.

---

## MEDIUM-SEVERITY FINDINGS

### M1. Retry classification by string match `"HTTP 4"`
- **File:line:** `crates/tc-otel-export/src/exporter.rs:175`
- Confirmed. `!msg.contains("HTTP 4")` mis-classifies `429 Too Many Requests` as permanent and would wrongly retry e.g. `"HTTP 408"` if the format ever changes.
- **Fix:** Carry the numeric `StatusCode` on `OtelError::ExportFailed`; retry on `5xx`, `408`, `429`; do **not** retry on other `4xx`.

### M2. Silent drop on backpressure with no metric
- **File:line:** `crates/tc-otel-service/src/dispatcher.rs:60` (LogDispatcher::send) and `:65`
- `try_send` failures only emit a `tracing::warn!`. There is no counter, no Prom/OTEL metric, and operators cannot quantify or alert on data loss.
- **Fix:** Add `dropped_logs` / `dropped_metrics` `AtomicU64` to dispatcher stats and expose via `system_metrics.rs` as `tc_otel.dispatcher.dropped{kind="logs|metrics"}`.

### M3. Hot-path clone defeats buffer reuse
- **File:line:** `crates/tc-otel-service/src/dispatcher.rs:184` and `:82` (vs reusable `payload_buf`)
- Confirmed. `payload_buf` is reused across flushes to reduce allocations, but the body is sent via `body(payload.clone())`, allocating a fresh `String` per flush.
- **Fix:** `body(payload.as_bytes().to_vec())` is no better — instead clone *into* a long-lived `Bytes` buffer and `body(bytes.clone())` (cheap Arc clone), or use `reqwest::Body::wrap_stream`.

### M4. Hot-path allocations in `to_otlp_attributes`
- **File:line:** `crates/tc-otel-export/src/exporter.rs:233-240`
- Each record allocates a fresh `Vec<serde_json::Value>` and per-key `json!({...})` Maps. Under 2000-record batches this is the dominant allocation site.
- **Fix:** Pre-size with `Vec::with_capacity(attrs.len())`; consider serialising directly via `serde::Serializer` rather than building a `Value` tree, or pre-cache attribute serialisation per (key, type) when keys are static.

### M5. Receiver does not parse incoming OTLP-JSON
- **File:line:** `crates/tc-otel-export/src/receiver.rs:82`
- `// TODO: Parse OTEL LogsData format` — handler accepts the request and emits a hardcoded test log entry. Anyone wiring a real receiver pipeline against this gets *fake* data.
- **Fix:** Implement OTLP-JSON `LogsData` parsing or remove the handler; in either case do not emit synthetic data on the production code path.

### M6. Receiver / gRPC silent drop on channel-full
- **File:line:** `crates/tc-otel-export/src/receiver.rs:84` and `crates/tc-otel-export/src/grpc.rs:373-376`
- `try_send` mirrors the same loss-without-metric pattern as M2 but on the *receive* path.
- **Fix:** Surface dropped-record counts as a metric and reply `RESOURCE_EXHAUSTED` (gRPC) / `503` (HTTP) so upstream collectors retry.

### M7. CUMULATIVE temporality hardcoded
- **File:line:** `crates/tc-otel-export/src/exporter.rs:378-387`
- Sum metrics emit `aggregationTemporality: 2` (CUMULATIVE) with no configuration. Datadog and most OTEL pipelines for short-cycle PLC counters expect DELTA.
- **Fix:** Make temporality configurable per metric class (`config.metrics.temporality`), or infer from `MetricKind`.

### M8. Histogram `bucketCounts` and `count` stringification inconsistent
- **File:line:** `crates/tc-otel-export/src/exporter.rs:392-395`
- `count` and `bucketCounts` are stringified, `sum` is left as a number. Per OTLP-JSON spec all uint64 fields must be JSON strings — currently consistent — but it would be easy to forget. Centralise.
- **Fix:** Single `int64_to_otlp_string()` helper used everywhere uint64/int64 is emitted (attributes too).

### M9. JSON `intValue` produced via `i.to_string()` but i32/u32 fields use bare numbers
- **File:line:** `crates/tc-otel-core/src/models.rs:196, 216, 222, 484, 504, 510, 749, 776, 780, 795`
- Many models emit `serde_json::Value::Number(i32.into())` for fields that the OTLP spec requires to be uint/int strings (esp. `task_index`, `task_cycle_counter`).
- **Fix:** Mirror exporter's int64-as-string helper at the model boundary, or only at the exporter (and never serialise these models to a wire format directly).

### M10. Receiver / gRPC: missing parsing of incoming OTLP-JSON `traceId` length
- **File:line:** `crates/tc-otel-export/src/exporter.rs:257-262`, `crates/tc-otel-core/src/models.rs:410-425, 697-704`
- No assertion that `trace_id` is exactly 32 hex chars / `span_id` 16 hex chars before emission. A malformed ID is silently emitted; downstream rejects the whole batch.
- **Fix:** Validate at the model constructor boundary.

### M11. Severity-number mapping is a 6-level lattice, not OTEL 1–24
- **File:line:** `crates/tc-otel-core/src/models.rs:582-590`
- Only `{1,5,9,13,17,21}` are produced. Inputs at 2–4, 6–8, … fall through.
- **Fix:** Map enums to canonical OTEL number directly; do not narrow to 6 buckets.

### M12. Cycle-time variance uses `n` not `n-1` (Bessel correction)
- **File:line:** `crates/tc-otel-service/src/cycle_time.rs:104-110`
- Sample stddev divides by `n`. Bias is meaningful for the smaller rolling windows used in alarms.
- **Fix:** Divide by `(n-1).max(1.0)`.

### M13. system_metrics.rs naming diverges from OTEL semantic conventions
- **File:line:** `crates/tc-otel-service/src/system_metrics.rs:137-140`
- `service.memory.rss` is not in the OTEL semconv — closest is `process.runtime.rust.memory.rss` or `process.memory.usage`.
- **Fix:** Adopt `process.*` names; document any deliberate deviations in the spec exception list.

### M14. system_metrics.rs uses non-standard unit strings
- **File:line:** `crates/tc-otel-service/src/system_metrics.rs:42-56`
- Units like `"{tasks}"`, `"{cycles}"` violate UCUM; OTEL convention is `"1"` for dimensionless counts.
- **Fix:** Use `"1"` plus a descriptive `unit_name` attribute if disambiguation is needed.

### M15. Process RSS read silently fails on non-Linux
- **File:line:** `crates/tc-otel-service/src/system_metrics.rs:149-154`
- `process_rss_bytes()` is `Option`-typed and silently `None` outside Linux. There is no log; operators on Windows/macOS see *missing* metrics rather than a clear "unsupported" signal.
- **Fix:** Use `sysinfo` for portability or log `info!` once at startup when RSS is unavailable.

### M16. Config reload not atomic across endpoint+batch_size
- **File:line:** `crates/tc-otel-service/src/dispatcher.rs:373-389` (`MetricDispatcher::batch_worker`)
- Endpoint change rebuilds the exporter; batch_size is applied on a separate branch. A reload that changes both can briefly mix new endpoint with old batch settings.
- **Fix:** Diff a snapshot of `(endpoint, batch_size, timeout)` and rebuild atomically in one branch.

### M17. Reqwest client not rebuilt on endpoint change
- **File:line:** `crates/tc-otel-service/src/dispatcher.rs:82-86, 141-148`
- `reqwest::Client` keeps connection pools keyed by host. After endpoint change the old pool is leaked until idle-eviction.
- **Fix:** Rebuild client whenever endpoint or auth headers change.

### M18. Batch not cleared on flush error
- **File:line:** `crates/tc-otel-service/src/dispatcher.rs:105-111`
- If `flush_batch()` returns `Err`, the buffer is **not** cleared; the loop keeps appending to a growing batch until OOM.
- **Fix:** Always `batch.clear()` after `flush_batch()` (success or failure). The records are already lost — accumulating them merely amplifies the loss.

### M19. Metric dispatcher not flushed on shutdown
- **File:line:** `crates/tc-otel-service/src/service.rs:191-217`
- Shutdown drops the receiver; the worker exits without a final flush. Last batch is lost.
- **Fix:** Have the worker observe a shutdown channel and flush remaining buffer before exit.

### M20. Span event attribute count limit shared with span attribute limit
- **File:line:** `crates/tc-otel-ads/src/parser.rs:649-661`
- `MAX_SPAN_ATTRIBUTES = 64` is reused for *each* event, multiplied by `MAX_SPAN_EVENTS = 128` → up to 8K attribute allocations per malformed span.
- **Fix:** Add `MAX_SPAN_EVENT_ATTRIBUTES = 16`; bound per-event load.

### M21. FILETIME underflow falls back to `Utc::now()` masking corruption
- **File:line:** `crates/tc-otel-ads/src/parser.rs:916-918`
- If the PLC sends a timestamp before the FILETIME epoch, the code substitutes the host's wall clock. Downstream cannot detect that the timestamp was synthesised.
- **Fix:** Return `AdsError::InvalidTimestamp` (and warn-log); let the dispatcher decide whether to drop or pass through.

### M22. Histogram bucket bounds not validated for sorting / contiguity
- **File:line:** `crates/tc-otel-ads/src/parser.rs:755-771`
- Bucket bounds are read as f64 without ascending-sorted check. OTLP requires sorted bounds; out-of-order bounds break quantile estimation downstream.
- **Fix:** Validate `bounds[i] > bounds[i-1]` after read.

---

## LOW-SEVERITY FINDINGS

### L1. Auth header re-expanded on every send
- `crates/tc-otel-export/src/exporter.rs:192-194, 319-321` — `expand_env_vars()` per attempt. Cache once at startup.

### L2. Empty trace/span checks don't validate hex shape
- `crates/tc-otel-export/src/exporter.rs:257, 260` — `is_empty()` does not detect a zero-filled string of correct length.

### L3. Severity text always cloned
- `crates/tc-otel-export/src/exporter.rs:252` — accept `&str` and use `serde_json::Value::String(s.to_string())` only when needed; or pre-intern common severities.

### L4. Connection pooling not configured
- `crates/tc-otel-export/src/exporter.rs:85-86` — defaults are fine but should be documented; consider `pool_idle_timeout` for long-lived deployments.

### L5. Atomic counters use `Relaxed` — fine for diagnostics but document
- `crates/tc-otel-service/src/service.rs:161, 176, 179, 201` — call out as best-effort diagnostics.

### L6. Channel capacity sizing has no documentation
- `crates/tc-otel-service/src/dispatcher.rs:32, 260` — README/config docs should explain "≥ batch_size × concurrency".

### L7. Shutdown branch in dispatcher does not drain log_rx
- `crates/tc-otel-service/src/service.rs:182-186` — extend the shutdown branch to drain `log_rx` until empty, then exit.

### L8. `formatter.rs::write_value` is recursively unbounded
- `crates/tc-otel-core/src/formatter.rs:174-182` — add `MAX_NESTING_DEPTH = 32`.

### L9. LogRecord serialization makes an ephemeral key per arg
- `crates/tc-otel-core/src/models.rs:763-764, 800-802` — interning `arg.0..arg.N` cuts allocations.

### L10. `panic!` in test on assertion failure
- `crates/tc-otel-ads/src/parser.rs:1507` — use `assert!(matches!(...))`.

### L11. `peek().unwrap()` after the loop guard
- `crates/tc-otel-ads/src/parser.rs:67` — semantically safe, syntactically fragile; replace with `?`.

### L12. Formatter positional placeholder offset
- `crates/tc-otel-core/src/formatter.rs:50-51` — clarify whether `{0}` indexes args[0] or args[1] and document.

### L13. `Kind::from_u8` already validates — the second `ok_or` is dead
- `crates/tc-otel-ads/src/parser.rs:707-710` — drop the redundant check; mark enum `#[non_exhaustive]`.

### L14. Attribute keys not validated against OTEL semconv naming
- `crates/tc-otel-ads/src/parser.rs:635-661` — PLC could inject `trace_id` or other reserved names; sanitise on insert.

---

## OTEL Spec Adherence Summary

| Area | Status | Notes |
|------|--------|-------|
| OTLP-JSON int64 → string | Partial | exporter does this for attributes (good); histogram counts (good); `models.rs::*Record` direct serialisation does **not** (M9). Centralise. |
| trace_id 16B / span_id 8B lowercase hex | Encoded but **not validated**; emitter trusts caller (H3). |
| Metric temporality | Hardcoded CUMULATIVE (M7). |
| Severity number mapping | 6 buckets, not 24 (M11). |
| Semantic-convention names | `service.memory.rss` and `plc.tasks.*` deviate (M13, M14). |
| TLS for non-localhost endpoints | Bypass via substring + scheme (H1, H2). |

---

## Test-coverage gaps

- No round-trip OTLP collector schema validation (e.g. against `opentelemetry-proto`'s JSON schemas).
- No end-to-end trace_id propagation test from PLC parser → models → exporter payload → fixture collector.
- No fault-injection on dispatcher backpressure (channel-full, retry-storm) — silent drops are not asserted-against.
- No bench for `to_otlp_attributes` at 2000-batch (M4).
- No integration test that SIGTERM triggers graceful shutdown (H4).

Recommended additions: (a) `tc-otel-integration-tests` case asserting OTLP-JSON validity against a recorded schema, (b) loom or property-based test for dispatcher backpressure semantics.

---

## Spawned beads (one per HIGH finding)

| HIGH | Bead   | Title |
|------|--------|-------|
| H1   | to-vp0 | fix: enforce TLS via explicit allow_insecure flag, drop substring matching |
| H2   | to-jqq | fix: propagate reqwest client build error instead of silent fallback |
| H3   | to-lxt | fix: introduce TraceId/SpanId newtypes with hex validation; remove zero-byte sentinel |
| H4   | to-ehp | fix: handle SIGTERM in graceful shutdown |
| H5   | to-lfo | fix: panic-guard or supervise tokio::spawn'd tasks in service.rs |
| H6   | to-mps | fix: bounds-check entry_start+entry_length writes in parser type-4/5/6 arms |
| H7   | to-i92 | fix: align MAX_STRING_LENGTH with read_string prefix and enforce MAX_* in type-4/6 arms |
| H8   | to-rc3 | fix: TTL-evict stale tasks in cycle-time tracker (memory leak) |
