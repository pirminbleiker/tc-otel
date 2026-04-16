# tc-otel

[![CI](https://github.com/pirminbleiker/tc-otel/actions/workflows/build.yml/badge.svg)](https://github.com/pirminbleiker/tc-otel/actions/workflows/build.yml)
[![Release](https://github.com/pirminbleiker/tc-otel/actions/workflows/release.yml/badge.svg)](https://github.com/pirminbleiker/tc-otel/actions/workflows/release.yml)
[![GitHub License](https://img.shields.io/github/license/pirminbleiker/tc-otel)](LICENSE)
[![GitHub Downloads](https://img.shields.io/github/downloads/pirminbleiker/tc-otel/total)](https://github.com/pirminbleiker/tc-otel/releases)

**OpenTelemetry for Beckhoff TwinCAT 3 PLCs -- logs, metrics, and traces.**

tc-otel bridges TwinCAT 3 PLCs to the OpenTelemetry ecosystem. It collects telemetry data from PLCs via ADS and exports it as standard OTLP to any compatible backend -- Grafana, Datadog, Elastic, Honeycomb, and more.

## Architecture

**High-level system overview:**

```
                           ┌─────────────────────────────────────────┐
                           │      Swappable Transport Layer           │
                           │                                           │
  TwinCAT PLC              │ TCP (direct ADS route)   MQTT Broker     │  tc-otel Service
 +----------+              │ +----────────────────+   +──────────+    │ +----------------+
 |   Logs   |              │ | Point-to-point     |   | Multi-   |    │ | ADS Receiver   |
 | Metrics  | ----[ADS]----|>| Direct IP route    |   | consumer |----+-→| Processor      |
 |  Traces  |              │ | port 48898         |   | fan-out  |    │ | OTEL Exporter  |
 +----------+              │ +----────────────────+   +──────────+    │ +----------------+
                           │                                           │                    ↓
                           └─────────────────────────────────────────┘                    OTLP
                                                                                      HTTP/gRPC
                                                                                          ↓
                                                                            +-----------------+
                                                                            | Grafana Loki    |
                                                                            | Prometheus      |
                                                                            | Jaeger / Tempo  |
                                                                            +-----------------+
```

The system has two components:

1. **PLC Library** (`library/`) -- A TwinCAT 3 library providing the telemetry API
2. **Service** (`crates/`) -- A Rust service that receives ADS data and exports via OpenTelemetry

For detailed architecture information including the layered design, protocol vs. transport separation, and how to extend the system with new commands or transports, see [Architecture Guide](docs/architecture.md).

### Transport Options

tc-otel supports multiple transports for ADS communication between PLC and service. Choose the one that fits your deployment:

| Transport                    | When to use                                                                     | Requires                                                                                    |
|------------------------------|---------------------------------------------------------------------------------|----------------------------------------------------------------------------------------------|
| **TCP (direct ADS route)**   | Point-to-point communication, low latency requirements, existing TwinCAT static routes | Direct IP connectivity between PLC and tc-otel service, ADS route configured via TwinCAT engineering |
| **MQTT (ADS-over-MQTT)**     | Multiple consumers, NAT/firewall traversal, broker fan-out, unidirectional clients | MQTT broker (mosquitto, EMQX, HiveMQ, etc.), `RemoteConnections/Mqtt` configured in PLC `StaticRoutes.xml` |

For MQTT setup details, see [MQTT Transport Setup](GETTING_STARTED.md#mqtt-transport-setup) in the Getting Started guide.

## Features

- **Simple PLC API** -- `F_Log(E_LogLevel.eInfo, 'Motor {0} started').WithAnyArg(sMotorName).CreateLog()`
- **Structured logging** -- Positional placeholders `{0}`, `{1}` with any IEC 61131-3 type
- **Context properties** -- Attach metadata at task, scope, and logger level
- **Push-based diagnostics** -- Real-time task cycle time and RT-violation monitoring via active ADS writes; see [Push Diagnostics Setup](docs/push-diagnostics-setup.md)
- **OpenTelemetry native** -- Exports via OTLP HTTP/gRPC to any OTEL-compatible backend
- **High performance** -- Zero-alloc hot path, handles thousands of entries per second
- **Cross-platform** -- Linux (amd64/arm64), Windows, Docker
- **Minimal footprint** -- Single static binary, ~5 MB

## Roadmap

tc-otel aims to provide **full OpenTelemetry observability** for TwinCAT PLCs across all three OTEL pillars:

### Logs (available now)
- [x] Structured log messages from PLC via ADS
- [x] Message templates with typed arguments
- [x] Context properties (task, scope, logger)
- [x] OTLP HTTP/gRPC export
- [x] Victoria-Logs / Grafana Loki / Elastic integration
- [x] Log level filtering

### Metrics (planned)
- [ ] PLC variable sampling as OTEL metrics (gauges, counters, histograms)
- [ ] Task cycle time metrics (jitter, min/max/avg)
- [ ] PLC CPU and memory utilization
- [ ] ADS connection health metrics
- [ ] Custom metric definitions via config (map PLC symbols to metric names)
- [ ] Prometheus / OTEL Collector / Datadog export

### Traces (planned)
- [ ] Motion sequence tracing (start/end spans for axis movements)
- [ ] Recipe execution spans
- [ ] State machine transition traces
- [ ] Distributed tracing across multiple PLCs
- [ ] Correlation of logs within trace context
- [ ] Jaeger / Grafana Tempo / Zipkin export

### Infrastructure (planned)
- [ ] Auto-discovery of PLC symbols via ADS browse
- [ ] Hot-reload configuration without restart
- [ ] Web UI for status and diagnostics
- [ ] Helm chart for Kubernetes deployment
- [ ] Grafana dashboard templates

## Quick Start

### 1. Install the service

**Docker (recommended):**
```bash
docker run -d --name tc-otel \
  -p 48898:48898 \
  -v ./config.json:/etc/tc-otel/config.json:ro \
  ghcr.io/pirminbleiker/tc-otel:latest
```

**Binary download:**
```bash
# Linux (amd64)
curl -L https://github.com/pirminbleiker/tc-otel/releases/latest/download/tc-otel-linux-amd64 -o tc-otel
chmod +x tc-otel

# Or install the .deb package
curl -L https://github.com/pirminbleiker/tc-otel/releases/latest/download/tc-otel_amd64.deb -o tc-otel.deb
sudo dpkg -i tc-otel.deb
```

### 2. Configure

Create a `config.json` (see [config.example.json](config.example.json)):
```json
{
  "logging": { "log_level": "info", "format": "text" },
  "receiver": {
    "ams_net_id": "0.0.0.0.1.1",
    "ams_tcp_port": 48898,
    "ads_port": 16150
  },
  "export": {
    "endpoint": "http://localhost:3100/otlp/v1/logs",
    "batch_size": 2000,
    "flush_interval_ms": 1000
  },
  "outputs": [],
  "service": {
    "channel_capacity": 50000
  }
}
```

### 3. Add the PLC library

1. Install `library/Log4TC.library` into your TwinCAT library repository
2. Add a reference in your PLC project
3. Start logging:

```iecst
// Simple log (must call CreateLog to emit)
F_Log(E_LogLevel.eInfo, 'Machine started').CreateLog();

// With arguments (positional: {0}, {1})
F_Log(E_LogLevel.eWarn, 'Temperature {0} exceeds limit {1}')
    .WithAnyArg(fTemperature)
    .WithAnyArg(fTempLimit)
    .CreateLog();
```

### 4. Add an ADS route

Add a route from your PLC to the tc-otel service (AMS Net ID from config, transport TCP).

See [GETTING_STARTED.md](GETTING_STARTED.md) for the full walkthrough.

## Installation Options

| Method | Platforms | Notes |
|--------|-----------|-------|
| **Docker** | linux/amd64, linux/arm64 | `ghcr.io/pirminbleiker/tc-otel` |
| **Binary** | Linux amd64/arm64, Windows x64 | Single executable, no dependencies |
| **.deb package** | Debian/Ubuntu amd64/arm64 | Includes systemd service unit |
| **Build from source** | Any Rust target | See [CONTRIBUTING.md](CONTRIBUTING.md) |

## Documentation

- [Getting Started](GETTING_STARTED.md) -- Full setup walkthrough
- [Contributing](CONTRIBUTING.md) -- Development setup, building, testing
- [Changelog](CHANGELOG.md) -- Release history
- [Security Policy](SECURITY.md) -- Reporting vulnerabilities
- [PLC Examples](source/TwinCat_Examples/) -- TwinCAT example projects

## Building from Source

```bash
# Prerequisites: Rust 1.75+
cargo build --release -p tc-otel-service

# Run tests
cargo test --all

# Run benchmarks
cargo bench
```

## License

Licensed under [Apache License 2.0](LICENSE).

Copyright (c) 2026 [Pirmin Bleiker](https://github.com/pirminbleiker)
