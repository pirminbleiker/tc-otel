# Victoria stack on the IPC — UIs and queries

`install.ps1` deploys the full **VictoriaMetrics ecosystem** as
Scheduled Tasks alongside tc-otel: VictoriaLogs (logs),
VictoriaMetrics (metrics), VictoriaTraces (traces). All three accept
OTLP from tc-otel and ship with built-in query interfaces — no Grafana
or external collector required for the basic case.

## Endpoints summary

Run on the same IPC. Replace `<target>` with the IPC's IP from your
LAN if you query from another box (firewall rules are opened by
`install.ps1`).

| Pillar  | Backend         | Port  | Ingest path used by tc-otel                              | Built-in UI / query                                          | Status |
| ------- | --------------- | ----- | -------------------------------------------------------- | ------------------------------------------------------------ | ------ |
| Logs    | VictoriaLogs    | 9428  | `/insert/jsonline` *(JSONL)*                             | `http://<target>:9428/select/vmui/`                          | ✓ working |
| Metrics | VictoriaMetrics | 8428  | `/opentelemetry/v1/metrics` *(OTLP-Protobuf)*            | `http://<target>:8428/vmui/`                                 | ✓ working |
| Traces  | VictoriaTraces  | 10428 | `/insert/opentelemetry/v1/traces` *(OTLP-Protobuf)*      | Jaeger Query API at `http://<target>:10428/select/jaeger/api/...` | ✓ working |

`config.json` in this dist is pre-wired to these URLs at `127.0.0.1`
and uses the `protobuf` wire format on the OTLP endpoints (logs
still go via VL's JSONL fast path).

### Wire format (`format` field)

tc-otel's HTTP exporter speaks both OTLP-JSON and OTLP-Protobuf.
VictoriaMetrics' OTLP endpoint **only accepts protobuf**; VL and VT
accept either. The shipped config picks `protobuf` everywhere it
matters so all three backends ingest natively without an
OpenTelemetry-Collector sidecar in between.

```json
"export":  { "format": "json", ... },             // logs (VL JSONL fast path — format ignored)
"metrics": { "export_format": "protobuf", ... },  // VM requires this
"traces":  { "export": { "format": "protobuf", ... } }
```

If you flip a metrics endpoint to `format = "json"`, VictoriaMetrics
returns `HTTP 400 json encoding isn't supported for opentelemetry format`
— that's the deliberate VM design choice the protobuf path was added
to handle.

## Logs — VictoriaLogs VMUI

Open `http://<target>:9428/select/vmui/` in a browser.

Useful LogsQL queries against tc-otel data:

```
*                                            -- all logs
host.name:"plc-172.28.41.37.1.1"             -- one specific PLC
severity_text:"ERROR" | sort by (_time) desc -- errors, newest first
_msg:"connection lost"                       -- free text
service.name:"tc-otel" AND task.name:"PlcTask"
```

LogsQL reference: <https://docs.victoriametrics.com/victorialogs/logsql/>.

Stream-fields tc-otel emits:
`_msg`, `_time`, `host.name`, `severity_text`, `service.name`,
`task.name`, plus any `WithContext(...)` properties you set in the
PLC.

## Metrics — VictoriaMetrics VMUI

Open `http://<target>:8428/vmui/`.

The PromQL/MetricsQL query box accepts everything VMUI supports.
Examples for tc-otel-emitted metrics:

```
{__name__=~".*"}                             -- all metrics
motor_temperature_celsius                    -- raw user metric
rate(motor_temperature_celsius[1m])          -- 1-minute rate
plc_task_cycle_time_seconds{task="PlcTask"}  -- built-in diagnostics
```

Built-in dashboard editor in VMUI lets you save panels and queries.

## Traces — VictoriaTraces (Jaeger query API)

VictoriaTraces does **not** ship a UI. It speaks the Jaeger Query JSON
API, so you can either:

1. **Drop in the Jaeger UI binary** — point it at
   `http://<target>:10428/select/jaeger/`.
2. **Query the API directly** for ad-hoc inspection:

```bash
# List services emitting traces
curl http://<target>:10428/select/jaeger/api/services

# Find recent traces from tc-otel
curl 'http://<target>:10428/select/jaeger/api/traces?service=tc-otel&limit=20'

# Fetch one trace by ID
curl http://<target>:10428/select/jaeger/api/traces/<trace_id>
```

Cross-pillar correlation: tc-otel injects the W3C `traceparent`
into every log emitted while a span is open, so a `trace_id` in
VictoriaLogs maps 1:1 to a trace in VictoriaTraces.

## Optional: Grafana

If you want the unified Grafana dashboarding layer (separate from this
minimal Victoria-only setup):

| Data source plugin     | URL                          |
| ---------------------- | ---------------------------- |
| **VictoriaLogs**       | `http://<target>:9428`       |
| **Prometheus** *(VM is wire-compatible)* | `http://<target>:8428` |
| **Jaeger** *(VictoriaTraces speaks Jaeger API)* | `http://<target>:10428/select/jaeger` |

Add each as a separate data source in Grafana, and you can query
logs, metrics, and traces from a single dashboard.

## Storage & retention

Each backend stores its data in `C:\victoria-{logs,metrics,traces}\data`.
Default retention varies:

| Backend         | Default retention | Override flag                                    |
| --------------- | ----------------- | ------------------------------------------------ |
| VictoriaLogs    | 7 days            | `-retentionPeriod=30d`                           |
| VictoriaMetrics | 1 month           | `-retentionPeriod=2w` / `-retentionPeriod=12mo`  |
| VictoriaTraces  | 7 days            | `-retentionPeriod=30d`                           |

Edit the matching `run.bat` in the install directory and bounce the
task:

```powershell
schtasks /End /TN "VictoriaMetrics"; schtasks /Run /TN "VictoriaMetrics"
```

To cap disk usage instead of (or in addition to) time:

```
-storage.maxDiskSpaceUsageBytes=10GB
```

(Available on all three Victoria backends.)
