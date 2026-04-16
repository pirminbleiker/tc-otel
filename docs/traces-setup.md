# OpenTelemetry Traces Setup

## Why Push-Based Traces

PLC code naturally operates as a series of correlated operations: motion sequences,
recipe steps, state machine transitions, RPC handlers. Push-based traces make those
relationships first-class by emitting span Begin/Attribute/Event/End events into the
log batch pipeline. Traces capture:

- **Operation nesting** via span parent-child relationships, implicit on the PLC task stack.
- **Cycle-accurate timing** — every span boundary carries the same DC timestamp
  (`F_GetActualDcTime64()`) that log entries use, so logs and traces align on the Grafana
  time axis at nanosecond precision.
- **Cross-PLC correlation** via W3C `traceparent` propagation, so an RPC originating on
  PLC-A and handled by PLC-B appears as a single distributed trace.
- **Deterministic, allocation-free PLC side** — no span ID generation, no string
  allocation in the hot path, no randomness. Rust handles all ID minting and OTLP
  serialisation.

Traces replace the dormant legacy `AdsSpanEntry` format. For the wire format details,
see [OpenTelemetry Traces — Design](traces-design.md).

## Architecture

```
┌──────────────────────────────┐
│ PLC task cycle               │
│  PRG_TaskLog.Span.Begin(...)─┼─► aSpanSlot[ local_id ]
│  PRG_TaskLog.Span.Add*(...) ─┤   (fixed-size array)
│  PRG_TaskLog.Span.End(...) ──┘
└──────────────────┬───────────┘
                   │ ADSWRITE (non-blocking)
                   │ IG=0x4D42_43xx (trace events)
                   │ batched with log entries
                   ▼
┌──────────────────────────────────────────┐
│ tc-otel router (port 16150)              │
│  decode_v2_trace_event                   │
│  → SpanDispatcher                        │
└──────────────────┬───────────────────────┘
                   │
                   ▼
        ┌──────────────────────┐
        │  OTLP /v1/traces     │
        │                      │
        │  otel-collector      │
        └──────────────┬───────┘
                       │
                       ▼
          ┌─────────────────────────┐
          │ Tempo (shipped in       │
          │ docker-compose.         │
          │ observability.yml)      │
          └───────────┬─────────────┘
                      │
                      ▼
                 Grafana
```

The observability stack (Unit 4) provisions Tempo as the default backend alongside
tc-otel in one `docker compose` command. Any OTLP-compatible backend (Jaeger, Honeycomb,
DataDog) works by repointing the `[traces.export]` endpoint in AppSettings.

## Using Traces on the PLC

All trace code follows the Log4TC pattern: declare an `FB_Span` instance and
call its methods. The function block owns the span lifetime and guarantees
begin/end pairing even on abnormal exit.

**Pattern 1 — single-cycle operation (simplest):**

```structured-text
VAR
    spn : FB_Span;
END_VAR

spn.Begin('compute_recipe');
spn.AddInt('batch_id', nBatchId);
spn.AddString('profile', sProfileName);
(* ... work ... *)
IF bSuccess THEN
    spn.AddEvent('result_cached');
ELSE
    spn.MarkError('cache miss');
END_IF
spn.End();
```

Every call emits exactly one wire event into the ADS buffer. No allocation, no
RNG, no hidden state.

**Pattern 2 — multi-cycle operation (motion, recipe, state machine):**

Use Pattern 2 when an operation spans many cycles. The `FB_Span` instance lives
in the enclosing FB's state and co-exists with the underlying work.

```structured-text
FUNCTION_BLOCK FB_MotionStep
VAR
    fbMoveAbs     : MC_MoveAbsolute;
    spn           : FB_Span;
    _bExecutePrev : BOOL;
END_VAR

METHOD PUBLIC Execute : BOOL
VAR_INPUT
    bExecute : BOOL;
    fTarget  : LREAL;
END_VAR
    // Rising edge: open span and start motion
    IF bExecute AND NOT _bExecutePrev THEN
        spn.Begin('motion_move_abs');
        spn.AddReal('target', fTarget);
        fbMoveAbs(Execute := TRUE, Position := fTarget);
    END_IF

    // Motion runs across many cycles — no repeated Begin
    fbMoveAbs();

    // Completion edges close the span
    IF spn.IsOpen AND fbMoveAbs.Done THEN
        spn.AddEvent('target_reached');
        spn.End();
    ELSIF spn.IsOpen AND fbMoveAbs.Error THEN
        spn.AddInt('mc_error_id', DINT(fbMoveAbs.ErrorID));
        spn.MarkError('motion command failed');
        spn.End();
    END_IF

    _bExecutePrev := bExecute;
    Execute := TRUE;
END_METHOD
```

Nested spans work automatically: whichever span was opened most recently on the task
becomes the parent. No plumbing required.

**Pattern 3 — imperative via the low-level tracer:**

For test code or when `FB_Span` is inconvenient:

```structured-text
hSpan := PRG_TaskLog.Span.Begin('drive_setup', eKind := E_SpanKind.Internal);
IF NOT bOk THEN
    PRG_TaskLog.Span.SetError(hSpan, 'drive offline');
END_IF
PRG_TaskLog.Span.End(hSpan);
```

The caller is responsible for pairing Begin/End on all control-flow paths. Pattern 3
does not get the `FB_exit` safety net that `FB_Span` provides.

**Reference implementation:**

`log4tc_Tester.PRG_TestTraceApi` ships traces out of the box: a 1 Hz tick span
and a 5 s nested demo. Run it to confirm your setup and as a template for your
own traces.

## Runtime Configuration

Traces are configured in the `[traces]` section of AppSettings:

```toml
[traces]
enabled = false
span_ttl_secs = 10
max_pending_spans = 1024

[traces.export]
endpoint = "http://otel-collector:4318/v1/traces"
batch_size = 100
flush_interval_ms = 1000
```

| Field | Meaning |
|-------|---------|
| `enabled` | When FALSE (default), span events are decoded but not exported. Useful for circuit-breaking during troubleshooting. |
| `span_ttl_secs` | Orphaned-span eviction threshold. If a span does not emit an END event within this duration, it is flushed with status=`TimedOut`. Default 10 s. |
| `max_pending_spans` | Hard limit on concurrent incomplete spans across all connected PLCs. When exceeded, the oldest span is evicted. Default 1024. Raise if you have many concurrent operations. |
| `endpoint` | OTLP HTTP receiver. Default points to the otel-collector container. Can be any URL (Honeycomb, DataDog, self-hosted Jaeger, etc.). |
| `batch_size` | Maximum spans per OTLP request. Tuning has minimal effect on latency; use if your backend has request-size limits. |
| `flush_interval_ms` | Maximum wait before sending a partial batch. Lower values increase latency-sensitivity; higher values improve compression. |

## Wire Format

Span events use the v2 log protocol with dedicated event types. Each event carries
a 12-byte header (event type, local_id, task_index, flags, dc_time) followed by
event-specific payload. Typical frames are 20–50 bytes for Begin and Add operations,
larger for Event frames with inline attributes.

Full byte-level layout, payload schemas, and bandwidth calculations are in
[OpenTelemetry Traces — Design](traces-design.md#wire-format).

## Observability Stack

Once Unit 4 (observability) lands, `docker-compose.observability.yml` brings up
the full stack in one command:

```bash
docker compose -f docker-compose.observability.yml up -d
```

This starts:
- **Tempo** (trace backend, port 3200)
- **Grafana** (dashboards, port 3000)
- **tc-otel** (log + metric + trace router, ports 6831, 16150, 4317)
- **otel-collector** (OTLP receiver, port 4318)

**Query Tempo directly:**

```bash
# Search for traces (returns trace IDs)
curl -s http://localhost:3200/api/search \
  --data-urlencode 'q=span_name="compute_recipe"' | jq .

# Fetch a single trace by ID
curl -s http://localhost:3200/api/traces/<trace_id> | jq .
```

**In Grafana:**

The `Tempo` datasource is pre-configured. Hover over a span on a dashboard, click
the `Trace ID` link to jump to the full trace in the Tempo explorer. The trace view
shows all spans, parent-child nesting, timing, attributes, and events.

**Log-to-trace linking:**

Tempo's `tracesToLogsV2` integration lets you jump from a span back to the logs
produced on the same cycle (same `dc_time`). Logs and traces share the same clock.

## Troubleshooting

### Spans marked `TimedOut` in Tempo

A span's END event never arrived, or tc-otel was restarted mid-trace.

- **On the PLC side:** Confirm the span's lifecycle is correct. Use `FB_Span` — its
  `FB_exit` safety net closes the span if the FB is destroyed. If using Pattern 3
  (imperative), verify every code path has a matching Begin/End pair, including
  error paths.
- **tc-otel restart:** If tc-otel restarts, pending spans are orphaned. A new `Begin`
  call with the same `local_id` will flush the stale entry as `TimedOut` first.
  Raise `span_ttl_secs` if your operations are long-running (> 10 s default).

### Duration shows as very small or zero

The DC clock is not synchronised.

- Check `F_GetActualDcTime64()` on the PLC — if it returns 0, tc-otel falls back to
  ingest-time (`Utc::now()`), losing nanosecond precision. This is a fallback path
  only; configure EtherCAT Distributed Clock (DC) for proper cycle-accurate timing.
- Without DC, `start_time` and `end_time` are both wall-clock-near-now, so duration
  is millisecond-level at best. This does not affect parent-child relationships or
  attributes.

### `max_pending_spans` exhausted (warnings in tc-otel logs)

Too many concurrent spans are open.

```
warn: max_pending_spans (1024) exhausted, evicting oldest span
```

- Raise `max_pending_spans` in AppSettings (default 1024).
- Reduce the span concurrency on the PLC — end long-running spans earlier, or lower
  the number of operations in flight.
- Typical case: a task spawns many independent work items without tracking their
  completion. Use a state machine or a completion counter instead.

### No traces arriving at Tempo

Checklist:

1. **Configuration:** `[traces].enabled = true` in AppSettings.
2. **Otel-collector routing:** Inspect the otel-collector `config.yaml` — the
   `traces:` pipeline should forward to Tempo's gRPC endpoint
   (`tempo:4317` in docker-compose, or your self-hosted URL).
3. **Tempo health:** `curl http://localhost:3200/ready`. Must return HTTP 200.
4. **Wire visibility:** For MQTT transport, sniff the broker:
   ```bash
   docker exec mosquitto mosquitto_sub -t 'AdsOverMqtt/#' -v | grep -i span
   ```
   If no `span` messages appear, the PLC is not emitting events. Check PLC logs,
   ADS route to tc-otel, and confirm `PRG_TaskLog.Call()` is wired into a task.
5. **Router logs:** `docker logs tc-otel | grep -i "span\|trace"`. Look for decode
   errors or dispatcher warnings.
