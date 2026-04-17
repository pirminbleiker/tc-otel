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
- **Push-based diagnostics** — Per-task cycle time, RT-violation detection, and custom metrics via active ADS writes; no polling needed
- **Log-to-trace correlation** — Logs automatically linked to trace context for fast root-cause analysis in Grafana
- **Multi-transport** — TCP (direct ADS route) or MQTT (publish-subscribe for fan-out and NAT traversal)
- **High performance** — Zero-allocation hot path, handles thousands of log/span/metric entries per second
- **OpenTelemetry native** — OTLP HTTP/gRPC export to any compatible backend
- **Cross-platform** — Linux (amd64/arm64), Windows, Docker; single static binary (~5 MB)

## Architecture

**System overview:**

```
┌─────────────────────────────────────────────────────────┐
│ TwinCAT PLC                                              │
│  • F_Log(level, msg)          → Logs (ADS WRITE)        │
│  • FB_Span.Begin/End          → Traces (ADS WRITE)      │
│  • PRG_TaskLog.Call()         → Metrics (ADS WRITE)     │
└────────────────────┬──────────────────────────────────────┘
                     │ ADS (via TCP or MQTT)
                     ▼
┌─────────────────────────────────────────────────────────┐
│ tc-otel Service (Rust)                                   │
│  • ADS Router (decode, dispatch)                         │
│  • Log/Trace/Metric processors                           │
│  • OTLP Exporter                                         │
└────────────────────┬──────────────────────────────────────┘
                     │ OTLP (HTTP/gRPC)
                     ▼
┌──────────────────────────────────────────────────────────┐
│ Your Observability Stack                                 │
│  • Grafana (UI)                                          │
│  • Tempo (traces)                                        │
│  • Loki / Victoria-Logs (logs)                           │
│  • Prometheus / Victoria-Metrics (metrics)               │
│  • Or: Datadog, Elastic, Honeycomb, etc.                │
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
4. Start logging:

```iecst
// Simple log
F_Log(E_LogLevel.eInfo, 'Machine started').CreateLog();

// With arguments
F_Log(E_LogLevel.eWarn, 'Temp {0}°C exceeds limit {1}°C')
    .WithAnyArg(fTemperature)
    .WithAnyArg(fTempLimit)
    .CreateLog();

// Distributed trace (spans)
VAR spn : FB_Span; END_VAR
spn.Begin('motion_step');
spn.AddInt('axis', 1);
spn.End();
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
| [Traces Setup](docs/traces-setup.md) | Push-based distributed tracing with span lifecycle (Phase 6 Stage 1) |
| [Traces Design](docs/traces-design.md) | Wire format, parent linkage, W3C propagation (Phase 5+) |
| [Traces Propagation](docs/traces-propagation.md) | Cross-PLC / cross-task `traceparent` handling |
| [Instance Tracer Design](docs/traces-instance-tracer-design.md) | `FB_TcOtelTracer` per-instance span tracking |
| [Push Diagnostics Setup](docs/push-diagnostics-setup.md) | Per-task cycle time, RT-violation metrics |
| [Push Metrics Wire Format](docs/push-metrics-wire-format.md) | Diagnostic batch serialization |
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

### Complete (Phase 1–6 Stage 1)
- Structured logging (v2 format, context properties)
- Distributed traces (w/ W3C `traceparent` propagation, cross-task, cross-PLC)
- Instance tracer (`FB_TcOtelTracer` for task-aware span tracking)
- Push-based diagnostics (per-task cycle time, RT violations)
- Log-to-trace correlation (Loki → Tempo via trace ID)
- TCP and MQTT transport
- OTLP HTTP/gRPC export

### In Progress / Planned
- Span sampling (Phase 7)
- Custom metric API enhancements
- Helm charts and deployment templates

## Contributing

TcOtel is open source under dual license: MIT or Apache-2.0. Contributions welcome!

For development setup, testing, and contribution guidelines, see [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Licensed under [MIT or Apache License 2.0](LICENSE).

Copyright (c) 2026 [Pirmin Bleiker](https://github.com/pirminbleiker)
