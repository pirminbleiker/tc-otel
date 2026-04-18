# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/), and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.0.11] - 2026-04-18

### Added
- **Traces pipeline end-to-end** — OpenTelemetry trace emission from PLC through tc-otel to VictoriaTraces, with W3C traceparent propagation across tasks, PLCs, and transport boundaries (GVL, ADS-RPC, MQTT, barcode/RFID).
- **Instance-scoped `FB_TcOtelTracer`** — public tracer API with per-instance `_pInnermost` chain, eliminating cross-PRG parent contamination when multiple controllers trace on the same task.
- **`FB_Span.BindTracer(tracer)`** — single public entry for binding a span to its owning tracer. Required before first `Begin()`.
- **`F_Log(...).WithSpan(spn)`** — explicit log-to-trace correlation; no more task-global auto-inheritance that cross-contaminated unrelated logs.
- **`F_HashTraceIdFromString(sId)`** — deterministic trace_id derivation for transport-less workpiece tracing (Pattern D: "station_weigh → station_inspect → station_label" without any writable medium on the workpiece).
- VictoriaTraces replaces Tempo as the trace backend; Jaeger UI added as alternative trace frontend.
- `otel-collector` pipelines route traces → victoria-traces, logs → victoria-logs, metrics → victoria-metrics; `spanmetrics` and `servicegraph` connectors produce RED metrics and service-graph series.
- Dispatcher-side `pending_by_span_id` secondary index + `log4tc.orphan_reason` attribute for debuggable parent-lookup failures.
- `tc_otel_span_orphan_total` Prometheus counter for orphan-span rate monitoring.

### Changed
- **Renamed PLC library `log4tc` → `TC-OTel`** (namespace `TcOtel`, folder `source/TwinCat_Lib/tc-otel/`). Library placeholder, FB prefixes (`FB_TcOtelTracer`, `FB_TcOtelTaskTracer`, `FB_TcOtelTask`, `FB_TcOtelTaskDiag`), DUTs, GVLs, tester project — all migrated.
- **Wire format rewritten** (Phase 6 Stage 3): BEGIN carries `parent_span_id(8)` instead of `parent_local_id(1)`; trace_id + span_id always embedded (no more `flag_local_ids` gating); ATTR/EVENT/END frames carry 8-byte span_id.
- **FB_Span owns trace_id + span_id state** directly (`_abTraceId`, `_abSpanId`); `ST_SpanSlot` / `aSlots` / `aStack` / `nStackDepth` / `nNextLocalId` retired from `FB_TcOtelTaskTracer`.
- **Public API uses REFERENCE / VAR_IN_OUT**, no POINTER. Callers write `spn.BindTracer(myTracer)` and `log.WithSpan(spn)` — no more `ADR(...)` boilerplate.
- `FB_TcOtelTaskTracer` privatized to internal wire transport (`METHOD INTERNAL`); users route through `FB_TcOtelTracer`.
- Repo cleanup for OSS-readiness: consolidated example configs under `examples/`, condensed docs (traces-internals + traces-setup as single source of truth), trimmed GETTING_STARTED to a real 5-minute quick-start, CODEOWNERS added.
- README refreshed to reflect Phases 1–6 Stage 1 state.

### Fixed
- Bound-tracer child spans now inherit parent's trace_id (regression introduced by instance-first refactor).
- PLC emits monotonic `local_id` per span so Rust-dispatcher `SpanKey` stays unique across concurrently-open spans (prevents every new BEGIN flushing the previous as "timed-out").
- `FB_Span.Begin` without `BindTracer` now logs an internal error and returns FALSE instead of silently mis-attributing the span.
- `span_dispatcher` orphan WARN demoted to DEBUG; orphan span carries `log4tc.orphan_reason` attribute for downstream queryability.
- XTI project files (`Log4TC.xti`, `log4Tc_SmokeTest.xti`, `log4Tc_Tester.xti`) renamed and their internal project/path references updated so TwinCAT can load the renamed library.
- AMS router service name (`b"tc-otel"`) for `ADS_CMD_READ_DEVICE_INFO`.

### Removed
- `ST_SpanSlot.TcDUT` — state moved into FB_Span.
- Task-tracer `aSlots`, `aStack`, `nStackDepth`, `nNextLocalId` (legacy slot bookkeeping).
- `PRG_TaskLog.Span` public entry point — users declare their own `FB_TcOtelTracer` instead.
- `CopyIdsTo` method — obsolete after instance-first refactor.
- Root-level debris: `.beads/`, `.runtime/`, `captures/`, `tests/` (legacy), `tc31-StaticRoutes.xml` (moved to `examples/twincat/`).
- Tempo — replaced by VictoriaTraces.

## [0.0.10]

### Added
- V2 protocol support with simplified TwinCAT API
- V2 protocol support with simplified TwinCAT API
- Direct Victoria-Logs export with batched async worker
- OpenTelemetry (OTLP) export pipeline with HTTP and gRPC support
- AMS/TCP server implementation (port 48898)
- Multi-platform Docker support (amd64/arm64)
- Debian package (.deb) releases
- GitHub Actions CI/CD (build, benchmark, release, security)
- Comprehensive benchmark suite (ADS parser, OTEL conversion, end-to-end)
- Security tests (connection limits, message limits)

### Changed
- Rewrote service from C#/.NET to Rust for performance and cross-platform support
- Replaced NLog/Graylog/InfluxDB output plugins with unified OTEL export
- Removed Beckhoff licensing requirement -- fully open source
- Zero-alloc hot path optimizations (CPU usage reduced from 116% to 0.05%)

### Fixed
- Timestamps before Unix epoch now accepted (PLC RTC not synced)
- Numeric placeholders `{0}`, `{1}` correctly map to 1-based PLC arguments
- TwinCAT-conform formatting for TIME, LTIME, DATE, DT, TOD types
- Parse all log entries per ADS buffer, not just the first one
- Use PLC timestamp for log ordering instead of receive time
- AMS/TCP Write responses include correct header

### Removed
- Windows-only .NET service (replaced by cross-platform Rust service)
- NLog, Graylog, InfluxDB, SQL output plugins (replaced by OTEL)
- Beckhoff license mechanism
- Azure Pipelines CI/CD (replaced by GitHub Actions)
- DocFx documentation system (replaced by markdown docs)
