# Getting Started with tc-otel

Receive telemetry from your TwinCAT 3 PLC and view logs in Grafana or Victoria-Logs.

## Prerequisites

- **TwinCAT 3** (XAE 4024 or later)
- **tc-otel service** (Docker, binary, or deb package)
- **A log backend** (Grafana Loki, Victoria-Logs, or OTEL Collector)

## Quick Start (5 minutes)

### 1. Start tc-otel

Using Docker with a minimal config:

```bash
mkdir tc-otel && cd tc-otel
cp /path/to/examples/config/minimal.json config.json

docker run -d --name tc-otel \
  -p 48898:48898 \
  -v $(pwd)/config.json:/etc/tc-otel/config.json:ro \
  ghcr.io/pirminbleiker/tc-otel:latest
```

Or download a binary from [releases](https://github.com/pirminbleiker/tc-otel/releases) and run `./tc-otel --config config.json`.

### 2. Add ADS Route

In TwinCAT XAE **SYSTEM > Routes**, add:
- **Route Name**: `tc-otel`
- **AMS Net ID**: from your `config.json` receiver.ams_net_id (e.g., `0.0.0.0.1.1`)
- **Address**: IP of the machine running tc-otel

### 3. Install PLC Library

In TwinCAT XAE:
1. **PLC > Library Repository > Install...**
2. Select `library/TcOtel.library`
3. Right-click project **References > Add Library...**
4. Search and add the library

### 4. Write a Log Message

Add this to any PLC program:

```iecst
F_Log(E_LogLevel.eInfo, 'Application started').CreateLog();
```

**Important:** Call `PRG_TaskLog.Call()` in each task that uses logging.

### 5. View Logs

Open Grafana (default: `http://localhost:3000`), go to **Explore > Loki**, and query:

```promql
{service_name="tc-otel"}
```

## Configuration Variants

See [examples/README.md](examples/README.md) for different deployment scenarios:
- **TCP**: direct AMS/TCP connection (local dev)
- **Docker Compose**: full stack with Grafana, Loki, Tempo, Prometheus
- **MQTT**: broker-based transport (multi-PLC, WAN-friendly)

## In-Depth Setup

For detailed instructions on ADS routes, MQTT transport, context propagation, and the full PLC API, see [docs/traces-setup.md](docs/traces-setup.md).

## Next Steps

- Explore [examples/](examples/) for configuration templates
- Read [docs/architecture.md](docs/architecture.md) for system design
- Check [CONTRIBUTING.md](CONTRIBUTING.md) if you want to contribute
