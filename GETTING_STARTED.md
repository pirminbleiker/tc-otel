# Getting Started with tc-otel

Send logs, traces, and metrics from your TwinCAT 3 PLC into a
queryable backend.

## What you get

| Pillar  | PLC API                                                                     | Backend choices                                  |
| ------- | --------------------------------------------------------------------------- | ------------------------------------------------ |
| Logs    | `F_Log(level, msg).WithAnyArg(x).CreateLog()`                               | VictoriaLogs / Loki / any OTLP receiver          |
| Traces  | `FB_Span.Begin(name) … End()` (W3C `traceparent` propagation)               | VictoriaTraces / Tempo / Jaeger / any OTLP       |
| Metrics | `FB_Metrics.Observe(value)` (oversampled, optional Welford min/max/mean/…) | VictoriaMetrics / Prometheus / any OTLP          |

All three flow through the same per-task ADS pipeline driven by a single
`PRG_TaskLog.Call()` per cycle.

## Pick a deployment shape

| Deployment | When to use | Quickstart |
| ---------- | ----------- | ---------- |
| **On the IPC alongside TwinCAT** *(recommended for single PLCs)* | tc-otel + the full Victoria stack run on the **same** Windows IPC as the PLC. No network hop, no port conflict, persistent on-box logs/traces/metrics. | [§A below](#a--quickstart-on-the-ipc-local_router-transport) |
| **Separate Linux/Docker host** | Centralised collection from many remote PLCs. Bundled Grafana/Tempo/Loki/Prometheus stack via Docker Compose. | [§B below](#b--quickstart-on-a-separate-docker-host) |

Both paths share the same PLC code. Pick the deployment shape, wire
the PLC, then jump to the [§ Pick a pillar](#pick-a-pillar) section.

---

## A · Quickstart on the IPC (local_router transport)

This is the typical industrial setup: tc-otel runs *on the TwinCAT
IPC itself*. Logs / traces / (metrics) live on the IPC's local disk
and are queryable from any Grafana box on the LAN — no separate
collection server, no Docker.

### How it works

tc-otel cannot bind TCP/48898 because TwinCAT's `TcAmsRouter` already
owns it. Instead it acts as a **client** of the local router: outbound
TCP to `127.0.0.1:48898`, an AMS/TCP `PortConnect` handshake registers
AMS port 16150, and the router fans every frame addressed to
`<localNetId>:16150` back over the same socket. No `StaticRoutes.xml`
edit, no separate AMS NetId.

The deployment package at [`dist/local-router/`](dist/local-router/)
provisions tc-otel **and** the full Victoria stack (VictoriaLogs +
VictoriaMetrics + VictoriaTraces) as Windows Scheduled Tasks under
SYSTEM, all on the IPC. See its
[`README.md`](dist/local-router/README.md),
[`architecture.md`](dist/local-router/architecture.md) and
[`router-reference.md`](dist/local-router/router-reference.md).

### A.1 Build tc-otel (one-time, on a dev box)

```bash
cargo build --release -p tc-otel-service
cp target/release/tc-otel.exe dist/local-router/
```

### A.2 Copy `dist/local-router/` to the IPC

`scp -r`, network share, USB stick — anything. End up with the
directory at e.g. `C:\deploy\local-router\` on the IPC.

### A.3 Run the installer (on the IPC, admin PowerShell)

```powershell
cd C:\deploy\local-router
.\install.ps1
```

The installer:

1. Lays out `C:\tc-otel\`, `C:\victoria-logs\`, `C:\victoria-metrics\`,
   `C:\victoria-traces\`
2. Downloads VictoriaLogs / VictoriaMetrics / VictoriaTraces from
   GitHub releases (skip with `-VlExe / -VmExe / -VtExe <path>` if
   pre-staged)
3. Registers four Scheduled Tasks (`tc-otel`, `VictoriaLogs`,
   `VictoriaMetrics`, `VictoriaTraces`), all `RU SYSTEM`
   `RL HIGHEST` `SC ONSTART` — they survive SSH disconnect and reboot
4. Opens Windows Firewall for the Victoria UI ports
5. Starts everything and verifies

After install, tc-otel logs **"registered with local AMS router:
netId=… port=16150"** — that's the success line.

### A.4 Tell the PLC where to send

In every task that uses TcOtel:

```iecst
IF _TaskInfo[GETCURTASKINDEXEX()].FirstCycle THEN
    PRG_TaskLog.Init('127.0.0.1.1.1');   // local NetId — the local router resolves it
END_IF

PRG_TaskLog.Call();                       // pumps logs / spans / metrics
```

No `StaticRoutes.xml` change required. tc-otel registered itself as a
local AMS port, so frames addressed to the PLC's own NetId on
port 16150 reach it directly through the router.

### A.5 Open the UIs

| | URL |
|---|---|
| VictoriaLogs (logs) | `http://<ipc>:9428/select/vmui/` |
| VictoriaTraces (Jaeger query API) | `http://<ipc>:10428/select/jaeger/api/services` |
| VictoriaMetrics (metrics) | `http://<ipc>:8428/vmui/` |
| tc-otel local web UI | `http://127.0.0.1:8080` *(IPC-local only)* |

> **Wire format:** the shipped config picks `format = "protobuf"` on the
> metrics + traces OTLP endpoints so VictoriaMetrics / VictoriaTraces
> ingest natively without an OpenTelemetry-Collector sidecar in
> between. Logs go to VictoriaLogs via its JSONL fast path. Set
> `format = "json"` on any pillar to switch back to OTLP-JSON if you
> point at a different OTLP backend that prefers JSON. See
> [`dist/local-router/victoria-stack.md`](dist/local-router/victoria-stack.md).

If you want a unified Grafana dashboard layer in front of these,
add VictoriaLogs / Prometheus / Jaeger data sources pointing at the
same URLs. See `dist/local-router/victoria-stack.md`.

Skip ahead to [§ Pick a pillar](#pick-a-pillar) for the PLC API.

---

## B · Quickstart on a separate Docker host

For multi-PLC / centralised collection: tc-otel and the dashboards run
on a separate Linux box, PLCs reach them over the LAN.

```bash
git clone https://github.com/pirminbleiker/tc-otel.git
cd tc-otel
docker compose -f docker-compose.observability.yml up -d
```

This starts:

- `tc-otel` — the ADS receiver / OTLP exporter (port `48898`)
- Grafana on `http://localhost:3000` (admin / admin) with provisioned
  dashboards, Tempo, Loki, Prometheus / VictoriaMetrics datasources
- An OpenTelemetry collector on `4317` / `4318`

If you only want the service, run it standalone:

```bash
docker run -d --name tc-otel \
  -p 48898:48898 \
  -v $(pwd)/examples/config/minimal.json:/etc/tc-otel/config.json:ro \
  ghcr.io/pirminbleiker/tc-otel:latest
```

Other deployment recipes (TCP, MQTT, OTel-Collector-only) live under
[`examples/`](examples/README.md).

### B.1 Add the ADS route on the PLC

In TwinCAT XAE → **SYSTEM › Routes › Add…**:

- **Route Name**: `tc-otel`
- **AMS Net ID**: the `receiver.ams_net_id` from your `config.json`
  (defaults to `0.0.0.0.1.1` in the bundled stack)
- **Address**: IP of the host running tc-otel
- **Transport**: `TCP_IP` (use MQTT only if you set up a broker — see
  [Traces Setup → MQTT](docs/traces-setup.md#mqtt-transport))

### B.2 Install the PLC library

In TwinCAT XAE:

1. **PLC › Library Repository › Install…** → pick `library/TcOtel.library`
2. In your PLC project, right-click **References › Add Library…** → search
   for **TcOtel** and add it

### B.3 Wire it up — one task, one Call

In every task that uses TcOtel, add these two lines:

```iecst
IF _TaskInfo[GETCURTASKINDEXEX()].FirstCycle THEN
    PRG_TaskLog.Init('127.0.0.1.1.1');   // local NetId for on-IPC, tc-otel NetId for remote
END_IF

PRG_TaskLog.Call();                       // pumps logs / spans / metrics
```

That's the whole transport. Now pick the pillar you need.

---

## Pick a pillar

### Logs

```iecst
F_Log(E_LogLevel.eInfo, 'Application started').CreateLog();

F_Log(E_LogLevel.eWarn, 'Temp {0}°C exceeds limit {1}°C')
    .WithAnyArg(fTemperature)
    .WithAnyArg(fTempLimit)
    .CreateLog();
```

In Grafana **Explore › Loki** (or VictoriaLogs):

```
{service_name="tc-otel"}
```

### Traces

Bind the span to a tracer once, then call `Begin`/`End` around the work:

```iecst
VAR
    tracer : FB_TcOtelTracer;
    spn    : FB_Span;
END_VAR

IF _TaskInfo[GETCURTASKINDEXEX()].FirstCycle THEN
    spn.BindTracer(tracer);
END_IF

spn.Begin('cycle');
spn.AddInt('axis', 1);
// ... do work ...
spn.End();
```

In Grafana **Explore › Tempo** → search by service name `tc-otel`. Logs
emitted while a span is open carry the same trace_id, so the trace view
links straight to the matching log lines.

### Metrics — `FB_Metrics`

`FB_Metrics` is the per-instance push API for any IEC scalar value
(`BOOL`, all int/REAL widths, `STRING`, `WSTRING`, ENUM/discrete). One
FB per logical metric, observed every cycle, flushed in batches at a
configurable push interval.

```iecst
VAR fbTemp : FB_Metrics; END_VAR

IF _TaskInfo[GETCURTASKINDEXEX()].FirstCycle THEN
    fbTemp.Init('motor.temperature', 'celsius');
    fbTemp.SetSampleIntervalMs(50);     // sample at most every 50 ms
    fbTemp.SetPushIntervalMs(5000);     // ship a batch every 5 s
END_IF

fbTemp.Observe(rTemperature);            // every cycle is fine
```

This emits `motor_temperature_celsius` with ~100 samples per 5 s window
and the AMS net id + task index attached as resource attributes.

**Aggregation (recommended for fast signals)** — at sample intervals
larger than the task cycle, raw sampling drops every value between
ticks. `SetAggregation` folds every `Observe` into a Welford on-line
aggregator and ships the chosen statistics per sample tick instead:

```iecst
fbTemp.SetAggregation(
    E_MetricStat.eMin OR E_MetricStat.eMax OR E_MetricStat.eMean);
// → ships motor_temperature_min_celsius / _max_celsius / _mean_celsius
```

Other useful combinations — OR the bits you want:

| Goal | Mask |
|------|------|
| Counter / totalizer (energy, parts, distance) | `eSum` |
| Peak / under-spec envelope | `eMin OR eMax` |
| Stability / jitter | `eMin OR eMax OR eMean OR eStdDev` |

See the [FB_Metrics reference](source/TwinCat_Lib/tc-otel/tc-otel/TcOtel/POUs/Metrics/README.md)
for the full method list, wire format, trace-context correlation
(`BindTracer` / `WithSpan`) and per-instance memory cost.

A pre-built Grafana dashboard with raw and aggregated demo signals
ships at
[`observability/grafana/dashboards/tc-otel-fb-metrics.json`](observability/grafana/dashboards/tc-otel-fb-metrics.json) —
auto-loaded by the bundled docker-compose stack.

### Per-task diagnostics (no code needed)

`PRG_TaskLog.Init(...)` automatically attaches the per-task diagnostic
collector — cycle time, exec time, RT-violation count, exceed counter.
The companion dashboard is
[`observability/grafana/dashboards/tc-otel-diagnostics.json`](observability/grafana/dashboards/tc-otel-diagnostics.json).
Tune the aggregation window via `PRG_TaskLog.InitDiag(…)` or via tc-otel
through ADS symbol writes against `PRG_TaskLog.aTaskDiagConfig[n]`.

## Configuration variants

| Scenario | File | When to use |
|----------|------|-------------|
| **On-IPC (local_router)** | [`dist/local-router/config.json`](dist/local-router/config.json) | tc-otel **on the same Windows IPC** as TwinCAT, full Victoria stack alongside |
| Local TCP dev | `examples/config/tcp.json` | tc-otel on the dev box, TwinCAT in a VM |
| Docker Compose | `examples/config/tcp-docker.json` | Bundled stack with Grafana + Tempo + Loki + VictoriaMetrics |
| MQTT | `examples/config/mqtt.json` | Multiple PLCs publishing through a broker |
| Minimal starter | `examples/config/minimal.json` | Bare-minimum reference config |

See [examples/README.md](examples/README.md) for details and
[examples/twincat/StaticRoutes.xml](examples/twincat/StaticRoutes.xml)
for an ADS static-route template (only needed for the non-IPC
deployment shapes).

## Troubleshooting

- **No data in Grafana?** Check `docker logs tc-otel` for ADS handshake
  messages and confirm the PLC route shows green in TwinCAT XAE.
- **Metrics flat-line for a few seconds at the live edge?** That's pipeline
  latency — the bundled dashboards default the time range to
  `now-1m-30s … now-30s` to skip it.
- **`PRG_TaskLog.Call()` not pumping?** It must run in the **same task**
  whose telemetry you want shipped — call it once per cycle in every task
  that uses TcOtel.

## Next steps

- [Architecture](docs/architecture.md) — layered design and extension points
- [FB_Metrics reference](source/TwinCat_Lib/tc-otel/tc-otel/TcOtel/POUs/Metrics/README.md) — full API, wire format, aggregation
- [Traces Setup](docs/traces-setup.md) — cross-task / cross-PLC propagation
- [Push Diagnostics Setup](docs/push-diagnostics-setup.md) — per-task metrics
- [CONTRIBUTING.md](CONTRIBUTING.md) — building from source, running tests
