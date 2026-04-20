# Getting Started with tc-otel

Send logs, traces and metrics from your TwinCAT 3 PLC into Grafana (or any
OTLP backend) in under ten minutes.

## What you get

| Pillar | PLC API | Backend |
|--------|---------|---------|
| Logs | `F_Log(level, msg).WithAnyArg(x).CreateLog()` | Loki / Victoria-Logs / OTLP |
| Traces | `FB_Span.Begin(name) … End()` (W3C `traceparent` propagation) | Tempo / Jaeger / OTLP |
| Metrics | `FB_Metrics.Observe(value)` (oversampled, optional Welford min/max/mean/…) | Prometheus / VictoriaMetrics / OTLP |

All three flow through the same per-task ADS pipeline driven by a single
`PRG_TaskLog.Call()` per cycle.

## Prerequisites

- **TwinCAT 3** XAE 4024 or newer
- **Docker** for the tc-otel service and (optionally) the bundled
  Grafana / Tempo / Loki / Prometheus stack
- Network reachability between the PLC runtime and the host that runs
  tc-otel

## 1 · Bring up the stack

The fastest path is the bundled all-in-one observability stack:

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

## 2 · Add the ADS route to your PLC

In TwinCAT XAE → **SYSTEM › Routes › Add…**:

- **Route Name**: `tc-otel`
- **AMS Net ID**: the `receiver.ams_net_id` from your `config.json`
  (defaults to `0.0.0.0.1.1` in the bundled stack)
- **Address**: IP of the host running tc-otel
- **Transport**: `TCP_IP` (use MQTT only if you set up a broker — see
  [Traces Setup → MQTT](docs/traces-setup.md#mqtt-transport))

## 3 · Install the PLC library

In TwinCAT XAE:

1. **PLC › Library Repository › Install…** → pick `library/TcOtel.library`
2. In your PLC project, right-click **References › Add Library…** → search
   for **TcOtel** and add it

## 4 · Wire it up — one task, one Call

In every task that uses TcOtel, add these two lines:

```iecst
IF _TaskInfo[GETCURTASKINDEXEX()].FirstCycle THEN
    PRG_TaskLog.Init('127.0.0.1.1.1');   // tc-otel AMS net id
END_IF

PRG_TaskLog.Call();                       // pumps logs / spans / metrics
```

That's the whole transport. Now pick the pillar you need.

## 5 · Pick a pillar

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
| Local TCP dev | `examples/config/tcp.json` | tc-otel on the dev box, TwinCAT in a VM |
| Docker Compose | `examples/config/tcp-docker.json` | Bundled stack with Grafana + Tempo + Loki + VictoriaMetrics |
| MQTT | `examples/config/mqtt.json` | Multiple PLCs publishing through a broker |
| Minimal starter | `examples/config/minimal.json` | Bare-minimum reference config |

See [examples/README.md](examples/README.md) for details and
[examples/twincat/StaticRoutes.xml](examples/twincat/StaticRoutes.xml)
for an ADS static-route template.

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
