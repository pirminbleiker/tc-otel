# TcOtel

[![CI](https://github.com/pirminbleiker/tc-otel/actions/workflows/build.yml/badge.svg)](https://github.com/pirminbleiker/tc-otel/actions/workflows/build.yml)
[![Release](https://github.com/pirminbleiker/tc-otel/actions/workflows/release.yml/badge.svg)](https://github.com/pirminbleiker/tc-otel/actions/workflows/release.yml)
[![GitHub License](https://img.shields.io/github/license/pirminbleiker/tc-otel)](LICENSE)
[![GitHub Downloads](https://img.shields.io/github/downloads/pirminbleiker/tc-otel/total)](https://github.com/pirminbleiker/tc-otel/releases)

**Full-stack observability for Beckhoff TwinCAT 3 PLCs — structured logs, distributed traces, and push-based metrics via OpenTelemetry.**

TcOtel provides a complete observability pipeline for TwinCAT PLCs: Push logs, traces, and metrics from your PLC code via ADS to a Rust service (tc-otel) that exports them as standard OTLP to any backend you choose — Grafana, Datadog, Elastic, Honeycomb, or your own OTLP collector.

## Features

- **Structured logging** — `F_Log(E_LogLevel.eInfo, 'Motor {0} started').WithAnyArg(sMotorName).CreateLog()` with typed placeholders
- **Distributed traces** — W3C `traceparent` propagation across tasks and multiple PLCs; span lifecycle (Begin/Attribute/Event/End) emitted as OTLP to Grafana Tempo or Jaeger
- **Oversampled application metrics** — `FB_Metrics` per-instance push of any IEC scalar (BOOL, all int/REAL widths, STRING, ENUM) at task-cycle rate, with optional Welford on-line aggregation (min/max/mean/sum/count/stddev) so peaks between sample ticks aren't lost
- **Push-based diagnostics** — Per-task cycle time, exec time, and RT-violation detection via active ADS writes; no polling needed
- **UI-driven custom metrics** — Select PLC symbols in the web UI and register them as OTLP metrics at runtime (poll or ADS notification), no PLC rebuild required; see [`docs/custom-metrics-client.md`](docs/custom-metrics-client.md)
- **Log-to-trace correlation** — Logs and metrics automatically carry trace context so you can jump from anomaly to span in one click
- **Multi-transport** — TCP (direct ADS route) or MQTT (publish-subscribe for fan-out and NAT traversal)
- **High performance** — Zero-allocation hot path, A/B double-buffered metric pipeline, handles thousands of log/span/metric entries per second
- **OpenTelemetry native** — OTLP HTTP/gRPC export to any compatible backend
- **Cross-platform** — Linux (amd64/arm64), Windows, Docker; single static binary (~5 MB)

## Architecture

**System overview:**

```
┌──────────────────────────────────────────────────────────┐
│ TwinCAT PLC                                              │
│  • F_Log(level, msg)          → Logs (ADS WRITE)         │
│  • FB_Span.Begin/End          → Traces (ADS WRITE)       │
│  • FB_Metrics.Observe(value)  → Metrics (ADS WRITE)      │
│  • PRG_TaskLog.Call()         → drives all three         │
└────────────────────┬─────────────────────────────────────┘
                     │ ADS (via TCP or MQTT)
                     ▼
┌──────────────────────────────────────────────────────────┐
│ tc-otel Service (Rust)                                   │
│  • ADS Router (decode, dispatch)                         │
│  • Log/Trace/Metric processors                           │
│  • OTLP Exporter                                         │
└────────────────────┬─────────────────────────────────────┘
                     │ OTLP (HTTP/gRPC)
                     ▼
┌──────────────────────────────────────────────────────────┐
│ Your Observability Stack                                 │
│  • Grafana (UI)                                          │
│  • Tempo (traces)                                        │
│  • Loki / Victoria-Logs (logs)                           │
│  • Prometheus / Victoria-Metrics (metrics)               │
│  • Or: Datadog, Elastic, Honeycomb, etc.                 │
└──────────────────────────────────────────────────────────┘
```

For detailed architecture, layered design, and extension points, see [Architecture Guide](docs/architecture.md).

### Transport Options

| Transport | When to use | Setup |
|-----------|-------------|-------|
| **TCP (direct ADS route)** | Point-to-point, low latency, existing TwinCAT infrastructure | Direct IP connectivity, ADS route via TwinCAT |
| **MQTT** | Multiple consumers, NAT/firewall, broker fan-out, edge gateways | MQTT broker (mosquitto/EMQX), `StaticRoutes.xml` config |

See [MQTT Transport Setup](GETTING_STARTED.md#mqtt-transport-setup) and [Traces Setup](docs/traces-setup.md) for examples.

## Quick Start

### Option A: Docker Compose (all-in-one stack)

```bash
# Clone the repo
git clone https://github.com/pirminbleiker/tc-otel.git
cd tc-otel

# Start the full observability stack (Grafana + Tempo + Loki + Prometheus + tc-otel)
docker compose -f docker-compose.observability.yml up -d

# Verify tc-otel is running
docker logs -f tc-otel
```

Visit Grafana at `http://localhost:3000` (admin/admin).

### Option B: tc-otel service only

**Docker:**
```bash
docker run -d --name tc-otel \
  -p 48898:48898 \
  -v ./config.json:/etc/tc-otel/config.json:ro \
  ghcr.io/pirminbleiker/tc-otel:latest
```

**Binary (Linux/Windows):**
```bash
curl -L https://github.com/pirminbleiker/tc-otel/releases/latest/download/tc-otel-linux-amd64 -o tc-otel
chmod +x tc-otel
./tc-otel --config config.json
```

### 2. Install the PLC Library

1. Go to **PLC > Library Repository > Install...**
2. Select `library/TcOtel.library` from this repo
3. In your PLC project, add **TcOtel** reference
4. In every task that uses TcOtel, drive the per-task pipeline once per cycle:

```iecst
IF _TaskInfo[GETCURTASKINDEXEX()].FirstCycle THEN
    PRG_TaskLog.Init('127.0.0.1.1.1');   // first cycle: tc-otel AMS net id
END_IF
PRG_TaskLog.Call();                   // every cycle: pumps logs / spans / metrics
```

5. Emit telemetry — pick the pillar(s) you need:

```iecst
// --- Logs: structured + typed args ---------------------------------
F_Log(E_LogLevel.eWarn, 'Temp {0}°C exceeds limit {1}°C')
    .WithAnyArg(fTemperature)
    .WithAnyArg(fTempLimit)
    .CreateLog();

// --- Traces: bind a tracer once, then Begin/End ---------------------
VAR
    tracer : FB_Trace;
    spn    : FB_Span;
END_VAR
IF _TaskInfo[GETCURTASKINDEXEX()].FirstCycle THEN
    spn.BindTracer(tracer);          // first cycle
END_IF
spn.Begin('motion_step');
spn.AddInt('axis', 1);
spn.End();

// --- Metrics: per-instance FB, oversampled, optional aggregation ----
VAR fbTemp : FB_Metrics; END_VAR
IF _TaskInfo[GETCURTASKINDEXEX()].FirstCycle THEN
    fbTemp.Init('motor.temperature', 'celsius');
    fbTemp.SetSampleIntervalMs(50);
    fbTemp.SetPushIntervalMs(5000);
    fbTemp.SetAggregation(                              // optional
        E_MetricStat.eMin OR E_MetricStat.eMax OR E_MetricStat.eMean);
END_IF
fbTemp.Observe(rTemperature);    // every cycle is fine
```

### 3. Configure ADS Route

Add a route from your PLC to tc-otel:
- **Route Name:** `tc-otel`
- **AMS Net ID:** from your `config.json` (e.g., `172.17.0.2.1.1`)
- **Transport:** TCP
- **Address:** IP of machine running tc-otel

See [GETTING_STARTED.md](GETTING_STARTED.md) for detailed steps.

## Documentation

| Document | Purpose |
|----------|---------|
| [Getting Started](GETTING_STARTED.md) | Install, configure, and run tc-otel + PLC library |
| [Architecture](docs/architecture.md) | Layered design, protocol/transport separation, extension points |
| [Traces Setup](docs/traces-setup.md) | Push-based distributed tracing — `FB_Span` lifecycle, W3C `traceparent` propagation, MQTT setup |
| [Traces Internals](docs/traces-internals.md) | Wire format, parent linkage, instance-tracer LIFO chain |
| [FB_Metrics — Oversampled App Metrics](source/TwinCat_Lib/tc-otel/tc-otel/TcOtel/POUs/Metrics/README.md) | Per-instance metric FB: sample-interval, push-interval, Welford aggregation, trace correlation |
| [Push Diagnostics Setup](docs/push-diagnostics-setup.md) | Per-task cycle time, RT-violation metrics |
| [Push Diagnostics Wire Format](docs/push-diagnostics-wire-format.md) | Per-task diagnostic batch serialization |
| [Push Metrics Wire Format](docs/push-metrics-wire-format.md) | `FB_Metrics` aggregate batch serialization |
| [Contributing](CONTRIBUTING.md) | Development setup, building, testing |
| [Changelog](CHANGELOG.md) | Release history and milestones |
| [Security Policy](SECURITY.md) | Reporting vulnerabilities |

## Project Structure

```
.
├── crates/                          # Rust workspace
│   ├── tc-otel-core                # Shared types (LogEntry, MetricEntry, etc.)
│   ├── tc-otel-ads                 # ADS protocol: router, parser, frame codec
│   ├── tc-otel-export              # OTLP exporters (HTTP/gRPC)
│   ├── tc-otel-service             # Main service: config, CLI, transport orchestration
│   ├── tc-otel-benches             # Benchmarks
│   └── tc-otel-integration-tests    # End-to-end tests
├── source/TwinCat_Lib/TcOtel/       # TwinCAT PLC library (TcOtel)
│   └── TcOtel/                      # FB_Log, FB_Span, FB_TcOtelTaskTracer, PRG_TaskLog
├── docs/                            # Architecture, design, setup guides
├── observability/                   # Docker/k8s configs for Tempo, Loki, Prometheus, Grafana
├── scripts/                         # Build and deployment utilities
├── docker-compose.*.yml             # All-in-one stacks (observability, test)
├── config.*.example.json            # Configuration templates
└── tests/                           # Integration test fixtures
```

## Installation Options

| Method | Platforms | Notes |
|--------|-----------|-------|
| **Docker Compose** | linux/amd64, linux/arm64 | Full stack: tc-otel + Grafana + Tempo + Loki + Prometheus |
| **Docker Service Only** | linux/amd64, linux/arm64 | `ghcr.io/pirminbleiker/tc-otel` |
| **Binary** | Linux amd64/arm64, Windows x64 | Single executable, no dependencies |
| **.deb package** | Debian/Ubuntu amd64/arm64 | Includes systemd service unit |
| **Build from source** | Any Rust target | Rust 1.75+, see [Contributing](CONTRIBUTING.md) |

## Building from Source

```bash
# Prerequisites: Rust 1.75+, Docker (for integration tests)
cargo build --release -p tc-otel-service

# Run tests
cargo test --workspace

# Run integration tests
cargo test --package tc-otel-integration-tests --test '*'

# Build PLC library
# (Requires TwinCAT 3, XAE 4024+)
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for full development setup.

## Status

### Complete
- Structured logging (v2 format, context properties)
- Distributed traces with W3C `traceparent` propagation (cross-task, cross-PLC)
- Per-instance tracer (`FB_Trace`) for isolated span trees per controller
- Push-based diagnostics (per-task cycle time, exec time, RT violations)
- **Application metrics (`FB_Metrics`)** — oversampled per-instance push, all IEC scalars + STRING/WSTRING, Welford on-line aggregation (min/max/mean/sum/count/stddev), A/B double-buffered sender (no drops on busy)
- Log- and metric-to-trace correlation (trace_id/span_id stamped at flush)
- TCP and MQTT transport
- OTLP HTTP/gRPC export

### Planned
- Promote trace_id/span_id to native OTLP exemplar fields (one-click jump from metric point to span in Grafana)
- Helm charts and deployment templates

## Contributing

TcOtel is open source under dual license: MIT or Apache-2.0. Contributions welcome!

For development setup, testing, and contribution guidelines, see [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Licensed under [MIT or Apache License 2.0](LICENSE).

Copyright (c) 2026 [Pirmin Bleiker](https://github.com/pirminbleiker)
