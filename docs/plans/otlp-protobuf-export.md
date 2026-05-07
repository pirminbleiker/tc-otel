# OTLP-Protobuf HTTP export — implementation plan

## Goal

Make tc-otel speak **OTLP-Protobuf over HTTP** (`Content-Type:
application/x-protobuf`) for logs, metrics and traces, so the
collector-less single-IPC deployment can talk **directly to
VictoriaMetrics** without a sidecar bridge.

## Why

VictoriaMetrics' `/opentelemetry/v1/metrics` ingest only accepts
Protobuf — by deliberate design, see
`dist/local-router/victoria-stack.md`. Today tc-otel's HTTP exporter
emits OTLP-JSON, which forces an OpenTelemetry Collector sidecar
(~380 MB Windows binary) on the IPC just to translate OTLP-JSON →
Prometheus remote_write. That's a poor footprint match for an
embedded controller.

VictoriaLogs and VictoriaTraces accept both encodings, but other
common backends (Datadog OTLP, Honeycomb, Tempo / Jaeger via OTLP,
Prometheus push gateway) have similar protobuf-only quirks. Native
protobuf support is the right long-term fix.

## Current state

| Component | Encoding | File |
|---|---|---|
| Logs HTTP export | OTLP-JSON | `crates/tc-otel-export/src/exporter.rs` (`build_otel_payload`, `send_with_retry`) |
| Metrics HTTP export | OTLP-JSON | `crates/tc-otel-export/src/exporter.rs` (`build_otel_metrics_payload`, `export_metrics_batch`) |
| Traces HTTP export | OTLP-JSON | `crates/tc-otel-export/src/exporter.rs` (`build_otel_traces_payload`, `export_traces_batch`) |
| Logs gRPC server | OTLP-Protobuf (incoming) | `crates/tc-otel-export/src/grpc.rs` — only **logs** types defined |
| Metric / trace gRPC | n/a | not implemented |

`grpc.rs` has hand-rolled prost-derived OTLP **logs** types
(`ResourceLogs`, `ScopeLogs`, `LogRecord`, `Resource`,
`InstrumentationScope`, `KeyValue`, `AnyValue`, …). It does **not**
have the metrics or traces proto types.

## Architecture

```
┌───────────────────┐          ┌─────────────────────────┐
│  MetricRecord     │  build_  │  ExportMetricsService   │
│  / TraceRecord    │  proto() │  Request (prost::Message)│
│  / LogRecord      │ ───────▶ │   (RFC: opentelemetry-  │
│  (existing types) │          │    proto crate)         │
└───────────────────┘          └────────────┬────────────┘
                                            │ encode_to_vec()
                                            ▼
                               ┌─────────────────────────┐
                               │ HTTP POST               │
                               │ Content-Type:           │
                               │   application/x-protobuf│
                               └─────────────────────────┘
```

The new export path is **format-pluggable**: each endpoint config
gets a `format: "json" | "protobuf"` field (default `"json"` for
backward compat). The OTLP exporter picks the encoder + Content-Type
at request time.

## Crate decision: `opentelemetry-proto` vs hand-rolled

**Use `opentelemetry-proto`** with feature `gen-tonic-messages` only
(skip `gen-tonic` to avoid pulling tonic generated client code we
don't need; we already use `tonic` from workspace for the existing
gRPC logs server).

| | `opentelemetry-proto` | hand-rolled |
|---|---|---|
| LoC added | ~5 (a `use` + map calls) | ~600 (Metric, Sum, Gauge, Histogram, Span, etc.) |
| Compile time impact | +~3-5 s clean build | ~+1 s |
| Binary size impact | +~250 KB stripped | +~50 KB |
| Maintenance | follows OTLP spec automatically | manual edits per OTLP version |
| Risk | new dep, version pin needed | bug surface in our own mapping |

The crate is small and well-maintained. The 250 KB delta is
negligible for a 12 MB tc-otel binary. Pick the crate.

Pin version in `Cargo.toml`:

```toml
opentelemetry-proto = { version = "0.27", default-features = false, features = ["gen-tonic-messages"] }
```

(Match whatever opentelemetry version the `tonic 0.14`-based stack
already pulls; 0.27 is current at time of writing.)

## Phased delivery

### Phase 1 — Metrics (the immediate VM unblocker)

1. Add `opentelemetry-proto` dep; `mod metrics_proto;` in
   `tc-otel-export`.
2. Implement `fn build_metrics_proto(records: &[MetricRecord])
   -> Vec<u8>` that produces an `ExportMetricsServiceRequest` and
   `prost::Message::encode_to_vec()`s it.
3. Add `format` field to `ExportConfig` and per-pillar overrides:
   ```rust
   #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
   #[serde(rename_all = "lowercase")]
   pub enum WireFormat {
       #[default]
       Json,
       Protobuf,
   }
   ```
4. Update `OtelExporter` to take a `WireFormat` for each pillar; in
   `send_metrics_with_retry` pick body / Content-Type accordingly.
5. **Mapping** `MetricRecord.kind` ↔ OTLP:
   - `Gauge`     → `metric.data = Some(Gauge(...))`, single
     `NumberDataPoint`
   - `Sum`       → `metric.data = Some(Sum(SumDataPoint{
     aggregation_temporality: CUMULATIVE,
     is_monotonic: <record.is_monotonic>, ...}))`
   - `Histogram` → `metric.data =
     Some(Histogram(HistogramDataPoint{ bucket_counts, explicit_bounds,
     count, sum, ... }))`. tc-otel only emits explicit-bucket
     histograms, no exponential — keep it simple.
6. **Exemplars**: when `trace_id` / `span_id` are non-empty hex,
   attach as a single `Exemplar` on the data point. Required for
   trace ↔ metric correlation in dashboards.
7. **Resource + Scope** mapping: convert
   `MetricRecord.resource_attributes` to `Resource.attributes`;
   tc-otel doesn't set a Scope today — emit a single
   `InstrumentationScope { name = "tc-otel", version = env!(...) }`.
8. Tests:
   - `roundtrip_gauge_protobuf`: build payload, decode with
     `opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest::decode`,
     assert structure.
   - `histogram_buckets_align`: verify bound/count vector alignment
     follows OTLP rules (`bounds.len() == counts.len() - 1`).
   - `protobuf_format_sets_content_type`: integration test against a
     mock HTTP receiver that records `Content-Type` header.
   - `vm_acceptance_test`: optional — gated behind
     `--ignored`-style cargo test feature; spins up VM via
     `testcontainers` and asserts ingest success.

### Phase 2 — Traces

Same shape as Phase 1 but for `TraceRecord` →
`ExportTraceServiceRequest`:
- `Span` mapping: each tc-otel `TraceRecord` becomes one `Span`.
- `Span.kind` from `TraceRecord.kind` (Internal/Client/Server/...).
- `events` from `TraceRecord.events: Vec<TraceEventRecord>`.
- `status` from `TraceRecord.status_code` + `status_message`.
- **Test target**: VictoriaTraces — already accepts JSON, must keep
  doing so. New protobuf path is additive.

### Phase 3 — Logs

Same shape, mapping `LogRecord` →
`ExportLogsServiceRequest`. **Lower priority**: VictoriaLogs handles
JSONL natively (`/insert/jsonline`), and the existing `Loki / Tempo /
collector` recipes use OTLP-JSON happily. Only add this when a
specific user backend demands it. (Could even be a separate PR.)

### Phase 4 — Config UX & defaults

After Phases 1-3 ship:
- Auto-detect: when `endpoint` contains `/opentelemetry/v1/metrics`
  (the VM canonical path), default `format = "protobuf"` even if not
  configured.
- Update `dist/local-router/config.json` to flip
  `metrics.export_enabled = true` with `format = "protobuf"`,
  pointing direct at VM. Drop the otelcol bridge from the dist
  entirely.
- `examples/config/*.json` — set sensible per-backend defaults.
- `GETTING_STARTED.md` and `dist/local-router/victoria-stack.md`:
  remove the "metrics disabled" caveat.

## Test matrix

| Backend | Format | Path |
|---|---|---|
| VictoriaLogs (current) | JSON | `/insert/jsonline` |
| VictoriaLogs (OTLP) | JSON or Protobuf | `/insert/opentelemetry/v1/logs` |
| VictoriaMetrics | **Protobuf** | `/opentelemetry/v1/metrics` |
| VictoriaTraces | JSON or Protobuf | `/insert/opentelemetry/v1/traces` |
| OTel-Collector | either | `:4318/v1/{logs,metrics,traces}` |
| Tempo / Jaeger / Datadog OTLP | Protobuf preferred | varies |

CI integration tests should hit at least one JSON and one Protobuf
backend per pillar, gated via `testcontainers` feature.

## Risks and mitigations

1. **Version pinning** of `opentelemetry-proto` against the existing
   `tonic 0.14` and `prost` workspace versions — could trigger a
   small dep tree refresh. Verify with `cargo tree -d` early.
2. **Behavioral change** for existing users who silently relied on
   JSON. Default `format = "json"` keeps them on the old path; the
   new behaviour is explicit opt-in unless the auto-detect (Phase 4)
   fires.
3. **MetricStat ↔ OTLP mismatch**: tc-otel's
   `eMin / eMax / eMean / eStdDev / eSum` Welford bits don't have
   direct OTLP analogues. Pragmatic mapping in `MetricMapper`:
   each enabled stat becomes a separate OTLP metric (`name_min`,
   `name_max`, …) of kind `Gauge` (`Sum` for `eSum`). Already what
   the current JSON path does — keep semantics identical.
4. **Histogram alignment**: OTLP spec demands strict ordering and
   `bounds.len() == counts.len() - 1`. tc-otel's existing JSON
   builder honours that; copy the same invariant assertions to the
   protobuf builder and unit-test them.
5. **Exemplar size** — OTLP allows multiple Exemplars per data
   point but tc-otel only ever has one (the most-recent
   trace_id/span_id pair). Single-element `Vec` is fine.

## Out of scope for this plan

- Switching the gRPC server side to use `opentelemetry-proto` types.
  It works fine with the hand-rolled types in `grpc.rs`; refactor
  later if the duplication becomes painful.
- Compression (`gzip` / `zstd`) in the HTTP exporter. A separate
  follow-up — VM and most backends support `Content-Encoding: gzip`
  out of the box, expected ~5-10× shrink on metric batches but
  orthogonal to format.
- New PLC API. PLC code keeps emitting the same wire frames; the
  change is entirely in tc-otel-export.

## Branch & PR plan

| Step | Branch | Outcome |
|---|---|---|
| 1 | `feat/otlp-protobuf-export` | Phase 1: metrics protobuf path, behind `format = "protobuf"`. Tests + dist update for VM direct. |
| 2 | `feat/otlp-protobuf-traces` | Phase 2: traces protobuf path. |
| 3 | `feat/otlp-protobuf-logs` | Phase 3: logs protobuf path. |
| 4 | `feat/otlp-protobuf-defaults` | Phase 4: auto-detect + config flip in dist + docs cleanup. |

Phase 1 alone unblocks the on-IPC VM ingest — that's the
high-value first ship.

## Open questions

- Pin `opentelemetry-proto` to 0.27 or take latest (0.30)? Latest is
  preferred unless it pulls a major opentelemetry rewrite. Decide
  during Phase 1.
- Should the auto-detect (Phase 4) be opt-out-able? Probably yes via
  `format = "json"` explicit, which already overrides everything.
- Streaming export (chunked transfer) vs. batched POSTs? Out of
  scope for now — current batched POST is fine at expected
  throughputs.
