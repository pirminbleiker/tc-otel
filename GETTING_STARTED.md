# Getting Started with tc-otel

This guide walks you through setting up tc-otel from scratch: installing the service, configuring your PLC, and viewing logs in a backend.

## Prerequisites

- **TwinCAT 3** (XAE 4024 or later) with a PLC project
- **tc-otel service** running on a reachable host (same machine or network)
- **An OTEL-compatible log backend** (e.g., Grafana Loki, Victoria-Logs, OTEL Collector)

## Step 1: Start the tc-otel Service

Choose one of the following methods:

### Option A: Docker (recommended)

```bash
# Create a config file
cat > config.json << 'EOF'
{
  "logging": { "log_level": "info", "format": "text" },
  "receiver": {
    "ams_net_id": "172.17.0.2.1.1",
    "ams_tcp_port": 48898,
    "ads_port": 16150
  },
  "export": {
    "endpoint": "http://loki:3100/otlp/v1/logs",
    "format": "otlp_http"
  },
  "service": {
    "worker_threads": null,
    "channel_capacity": 10000
  }
}
EOF

# Run the service
docker run -d --name tc-otel \
  -p 48898:48898 \
  -v $(pwd)/config.json:/etc/tc-otel/config.json:ro \
  ghcr.io/pirminbleiker/tc-otel:latest
```

### Option B: Binary

Download the latest binary from the [releases page](https://github.com/pirminbleiker/tc-otel/releases):

```bash
# Linux
curl -L https://github.com/pirminbleiker/tc-otel/releases/latest/download/tc-otel-linux-amd64 -o tc-otel
chmod +x tc-otel
./tc-otel --config config.json
```

```powershell
# Windows
Invoke-WebRequest -Uri "https://github.com/pirminbleiker/tc-otel/releases/latest/download/tc-otel-windows-amd64.exe" -OutFile "tc-otel.exe"
.\tc-otel.exe --config config.json
```

### Option C: Debian/Ubuntu package

```bash
curl -L https://github.com/pirminbleiker/tc-otel/releases/latest/download/tc-otel_amd64.deb -o tc-otel.deb
sudo dpkg -i tc-otel.deb

# Edit the configuration
sudo nano /etc/tc-otel/config.json

# Start the service
sudo systemctl enable --now tc-otel
```

### Verify the service is running

```bash
# Check logs
docker logs tc-otel
# or
journalctl -u tc-otel -f
```

You should see:
```
tc-otel starting: AMS/TCP :48898 (Net ID 172.17.0.2.1.1), export -> http://loki:3100/otlp/v1/logs
```

## Step 2: Install the PLC Library

1. Open TwinCAT XAE (Visual Studio)
2. Go to **PLC > Library Repository > Install...**
3. Select `library/Log4TC.library` from this repository
4. In your PLC project, right-click **References** > **Add Library...**
5. Search for `tc-otel` and add it

## Step 3: Add an ADS Route

The PLC needs an ADS route to the tc-otel service:

1. Open **TwinCAT XAE** > **SYSTEM** > **Routes**
2. Click **Add Route...**
3. Configure:
   - **Route Name**: `tc-otel`
   - **AMS Net ID**: The `ams_net_id` from your config (e.g., `172.17.0.2.1.1`)
   - **Transport Type**: TCP/IP
   - **Address**: IP address of the machine running tc-otel
   - **Connection Timeout**: 5 seconds

Alternatively, you can add the route via `adsrouter` command line or through the PLC program.

## Step 4: Write Your First Log Message

Create a new PLC program or add to an existing one:

```iecst
PROGRAM MAIN
VAR
    bInit : BOOL;
END_VAR
```

```iecst
// Body
IF NOT bInit THEN
    bInit := TRUE;
    F_Log(E_LogLevel.eInfo, 'Application started').CreateLog();
END_IF

// Log with positional argument {0}
F_Log(E_LogLevel.eInfo, 'Cycle count: {0}')
    .WithAnyArg(_TaskInfo[GETCURTASKINDEXEX()].CycleCount)
    .CreateLog();
```

> **Important:** You must call `PRG_TaskLog.Call()` in each task that uses logging. Add it to your task's program cycle.

### Activate and run

1. Build the PLC project (Ctrl+Shift+B)
2. Activate configuration (Ctrl+Shift+F4)
3. Start the PLC (F5 or green Play button)

## Step 5: View Your Logs

Depending on your backend:

### Grafana + Loki

1. Open Grafana (default: `http://localhost:3000`)
2. Go to **Explore** > select **Loki** data source
3. Query: `{service_name="tc-otel"}`

### Victoria-Logs

```bash
curl "http://localhost:9428/select/logsql/query?query=*&limit=10"
```

### OTEL Collector (debug)

If using the included `otel-collector-config.yml` with the debug exporter, check the collector's stdout for log output.

## Configuration Reference

### Full config.json example

```json
{
  "logging": {
    "log_level": "info",
    "format": "text",
    "output_path": null
  },
  "receiver": {
    "host": "0.0.0.0",
    "http_port": 4318,
    "grpc_port": 4317,
    "max_body_size": 4194304,
    "request_timeout_secs": 30,
    "ams_net_id": "0.0.0.0.1.1",
    "ams_tcp_port": 48898,
    "ads_port": 16150
  },
  "export": {
    "endpoint": "http://localhost:3100/otlp/v1/logs",
    "batch_size": 2000,
    "flush_interval_ms": 1000,
    "timeout_secs": 10,
    "max_retries": 3
  },
  "outputs": [],
  "service": {
    "name": "tc-otel",
    "display_name": "tc-otel - TwinCAT OpenTelemetry Service",
    "worker_threads": null,
    "channel_capacity": 50000,
    "shutdown_timeout_secs": 30
  }
}
```

### Key settings

| Setting | Default | Description |
|---------|---------|-------------|
| `receiver.ams_net_id` | `0.0.0.0.1.1` | AMS Net ID the service listens on |
| `receiver.ams_tcp_port` | `48898` | AMS/TCP port for PLC connections |
| `receiver.ads_port` | `16150` | ADS port within AMS |
| `export.endpoint` | `victoria-logs:9428/...` | URL of the OTEL-compatible log endpoint |
| `export.batch_size` | `2000` | Flush after this many records |
| `export.flush_interval_ms` | `1000` | Flush interval in milliseconds |
| `service.channel_capacity` | `50000` | Internal message buffer size |

## PLC API Overview

The API uses a fluent builder pattern. Every log call **must** end with `.CreateLog()` to emit the message.

### F_Log -- Simple task-scoped logging

```iecst
// Basic message
F_Log(E_LogLevel.eInfo, 'Machine started').CreateLog();

// With positional arguments ({0}, {1}, ...)
F_Log(E_LogLevel.eWarn, 'Temperature {0} exceeds limit {1}')
    .WithAnyArg(fTemperature)
    .WithAnyArg(fTempLimit)
    .CreateLog();

// WithAnyArg accepts any IEC 61131-3 type (INT, REAL, STRING, LREAL, etc.)
F_Log(E_LogLevel.eDebug, 'Axis {0} at position {1}')
    .WithAnyArg(sAxisName)
    .WithAnyArg(fPosition)
    .CreateLog();
```

### FB_Log -- Persistent named logger

```iecst
VAR
    fbLog : FB_Log;  // Logger name derived from instance path
END_VAR

// Log methods: Trace, Debug, Info, Warn, Error, Fatal
fbLog.Info('Motor started').CreateLog();

// With arguments
fbLog.Warn('Speed {0} exceeds limit {1}')
    .WithAnyArg(fSpeed)
    .WithAnyArg(fMaxSpeed)
    .CreateLog();

// Override logger name
fbLog.Name := 'Drives.Motor1';

// Add persistent context (appears on every log from this logger)
fbLog.LoggerContext.AddString('machine', 'Line1');
fbLog.LoggerContext.AddDInt('stationId', nStationId);
```

### FB_ScopedContext -- Nested scope context

```iecst
VAR
    fbCtx : FB_ScopedContext;
END_VAR

// Add context properties
fbCtx.ContextBuilder.AddString('recipe', sRecipeName);
fbCtx.ContextBuilder.AddDInt('batchId', nBatchId);

// Push context onto task stack
fbCtx.Begin();

// All logs within this scope include the context
F_Log(E_LogLevel.eInfo, 'Processing step {0}').WithAnyArg(nStep).CreateLog();

// Pop context when done
fbCtx.End();
```

### F_Context -- Task-scoped context

```iecst
// Add context to the current task (persists across cycles)
F_Context().AddString('operator', sOperatorName);
F_Context().AddDInt('orderId', nOrderId);
```

### PRG_TaskLog -- Required in every task

```iecst
// Must be called once per cycle in each task that uses logging
PRG_TaskLog.Call();

// Optional: add task-level context
PRG_TaskLog.TaskContextBuilder.AddString('taskName', 'PlcTask');
```

### Log levels

| Level | Enum value | Use for |
|-------|------------|---------|
| Trace | `E_LogLevel.eTrace` | Detailed diagnostic info |
| Debug | `E_LogLevel.eDebug` | Development debugging |
| Info | `E_LogLevel.eInfo` | Normal operation events |
| Warn | `E_LogLevel.eWarn` | Unexpected but handled situations |
| Error | `E_LogLevel.eError` | Failures requiring attention |
| Fatal | `E_LogLevel.eFatal` | Unrecoverable errors |

### Context builder methods

All methods on `I_ContextBuilder` (returned by `F_Context()`, `fbLog.LoggerContext`, `fbCtx.ContextBuilder`):

| Method | Type |
|--------|------|
| `AddBool(sName, bValue)` | BOOL |
| `AddInt(sName, nValue)` | INT |
| `AddDInt(sName, nValue)` | DINT |
| `AddUDInt(sName, nValue)` | UDINT |
| `AddLint(sName, nValue)` | LINT |
| `AddReal(sName, fValue)` | REAL |
| `AddLReal(sName, fValue)` | LREAL |
| `AddString(sName, sValue)` | T_MaxString |
| `Clear()` | -- |
| `Remove(sName)` | -- |

## Docker Compose Example

For a full stack with Grafana and Loki:

```yaml
services:
  tc-otel:
    image: ghcr.io/pirminbleiker/tc-otel:latest
    ports:
      - "48898:48898"
    volumes:
      - ./config.json:/etc/tc-otel/config.json:ro

  loki:
    image: grafana/loki:latest
    ports:
      - "3100:3100"

  grafana:
    image: grafana/grafana:latest
    ports:
      - "3000:3000"
    environment:
      - GF_AUTH_ANONYMOUS_ENABLED=true
      - GF_AUTH_ANONYMOUS_ORG_ROLE=Admin
```

## Troubleshooting

### No logs appearing

1. **Check the ADS route**: Ensure the route is active in TwinCAT System Manager
2. **Check the service**: `docker logs tc-otel` or `journalctl -u tc-otel`
3. **Check the endpoint**: Ensure your backend is reachable from the service
4. **Check the PLC**: Add `F_Log(E_LogLevel.eError, 'TEST').CreateLog();` and verify it triggers

### Connection refused on port 48898

- The service is not running or not listening on the correct interface
- Check `receiver.host` in config (use `0.0.0.0` for all interfaces)
- Check firewall rules

### High CPU usage

- This was addressed in recent versions with zero-alloc optimizations
- Ensure you're running the latest release
- Check `service.channel_capacity` -- increase if buffer fills up

## Next Steps

- Browse the [PLC examples](source/TwinCat_Examples/) for more patterns
- Read the [source code](crates/) to understand the service internals
- [Contribute](CONTRIBUTING.md) to the project
