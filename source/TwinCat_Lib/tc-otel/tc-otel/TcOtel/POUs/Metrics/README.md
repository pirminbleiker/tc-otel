# FB_Metrics — Custom Oversampled Metrics for tc-otel

`FB_Metrics` is a per-metric PLC-side function block that captures value
changes of any IEC 61131-3 scalar (or `STRING` / `WSTRING`), buffers them
across cycles, and pushes a compact wire frame to tc-otel at a configurable
interval. It complements the per-task push-diagnostics collector
(`FB_TcOtelTaskDiag`) and the UI-driven custom metrics (`IO_PUSH_METRIC_BATCH`)
with a third option: **user-defined, instance-scoped, oversampled metrics
with optional trace-context correlation**.

## When to use which

| Need | Use |
| --- | --- |
| TwinCAT task cycle / exec time / RT-violation telemetry | `FB_TcOtelTaskDiag` (auto-wired) |
| Polled / notification-based scalar value via tc-otel UI config | UI custom metrics → `IO_PUSH_METRIC_BATCH` |
| Application-defined value, change-driven, optionally trace-correlated | **`FB_Metrics`** (this) |

## API at a glance

```pascal
VAR
    fbMotorTemp : FB_Metrics;
END_VAR

IF _TaskInfo[GETCURTASKINDEXEX()].FirstCycle THEN
    fbMotorTemp.Init('motor.temperature', 'celsius');
    fbMotorTemp.SetSampleIntervalMs(100);   // at most every 100 ms
    fbMotorTemp.SetPushIntervalMs(5000);    // flush every 5 s
END_IF

fbMotorTemp.Observe(rTemperature);          // every cycle is fine
PRG_TaskLog.Call();                         // pumps the per-task sender
```

That's it. tc-otel sees ~50 `motor.temperature` data points every 5 s,
unit `celsius`, with the AMS net id and task index attached as resource
attributes.

### Methods

| Method | Purpose |
| --- | --- |
| `Init(sName, sUnit, nStringSize)` | Set name + unit; compute stable `metric_id = FNV-1a(name)`. `nStringSize` only meaningful for `STRING`/`WSTRING` values (default 64 bytes). |
| `Observe(stArg : ANY)` | Capture one observation. First call locks the type — subsequent calls of a different `TypeClass` are dropped. |
| `Call()` | Cyclic push-window check. Only needed if `Observe` is called sporadically. |
| `Flush()` | Force-flush the buffered samples now. |
| `SetSampleIntervalMs(n)` | Min interval between captured samples. `0` = every change (default). |
| `SetPushIntervalMs(n)` | Push window in ms. Default `1000`. |
| `SetChangeDetect(b)` | Toggle byte-wise change suppression. Default `TRUE`. |
| `BindTracer(t)` | Attach an `FB_TcOtelTracer` — its innermost open span is snapshotted into every flushed frame. |
| `WithSpan(span)` | One-shot trace-context override for the next flush. |
| `SetAggregation(nMask)` | Enable Welford online aggregation (numeric metrics only). Bitmask of `E_MetricStat` values, or `0` for raw single-value sampling. See "Aggregation" below. |

### Properties

| Property | Description |
| --- | --- |
| `SampleCount : UDINT` | Samples currently buffered (since last flush). |
| `MetricId : UDINT` | FNV-1a hash of the metric name, stable per name. |

## Lifecycle

1. **First cycle**: `Init` once. Optionally tune `SetSampleIntervalMs` /
   `SetPushIntervalMs` / `SetChangeDetect` and bind a tracer.
2. **Every cycle**: call `Observe(value)`. The FB internally:
   * Locks the body schema on first non-zero-value observation.
   * Skips if rate-limit interval hasn't elapsed.
   * Skips if change-detect says "byte-identical to last sampled value".
   * Appends to the 8 KB body buffer (overflow → flag, drops further appends).
   * Triggers `_DoFlush` when the push window expires.
3. **`PRG_TaskLog.Call()`**: pumps the per-task `FB_TcOtelTaskMetrics` sender
   that owns the `ADSWRITE` to tc-otel.
4. **Online change / FB destruction**: `FB_exit` flushes the partial window
   so the last samples don't disappear.

## Body schemas (locked on first Observe)

| `TypeClass` | `body_schema` | Wire `sample_size` | Body content |
| --- | --- | --- | --- |
| `BOOL` | `eBool` (1) | 1 | `N × BYTE` (0 / 1) |
| `SINT` … `LREAL` (incl. unsigned) | `eNumeric` (2) | 8 | `N × LREAL` (widened) |
| `BYTE`, `WORD`, `DWORD`, `LWORD`, `ENUM` | `eDiscrete` (3) | native | `N × raw` |
| `STRING` | `eString` (4) | `nStringSize` | `N × fixed-size UTF-8` (zero-padded) |
| `WSTRING` | `eWstring` (5) | `nStringSize × 2` | `N × fixed-size UTF-16LE` (zero-padded) |

The receiver knows how to slice the body from `(body_schema, sample_size,
sample_count)`. Numeric types collapse to LREAL on the PLC so the wire is
uniform 8-byte floats regardless of the source.

**OTel mapping note**: `STRING` / `WSTRING` metrics are decoded but not
emitted as OTel metrics (which are numeric). They're reserved for the
events pipeline. `BOOL` / `Discrete` map to `0.0` / `1.0` / unsigned-int
gauges.

## Aggregation (Phase 7)

Raw sampling at `SetSampleIntervalMs(N>1ms)` drops every value between
ticks — a 10 ms peak in a 50 ms-sampled signal can vanish entirely.
`SetAggregation(nMask)` solves this by folding **every** `Observe` call
into a Welford online aggregator (~6 LREAL ops per call). At each
sample tick the aggregator's snapshot is written to the body buffer
and the aggregator resets, so no observation is ever dropped — only
its detail is summarized into the selected stats.

Bitmask values from `E_MetricStat`:

| Bit     | Stat       | Use case                                              |
| ------- | ---------- | ----------------------------------------------------- |
| `eMin`  | min        | trough / under-spec detection                         |
| `eMax`  | max        | peak / over-spec detection                            |
| `eMean` | mean       | central tendency, typical level                       |
| `eSum`  | sum        | totalizer (energy, parts produced, distance)          |
| `eCount`| count      | observations folded — gap / drop detection            |
| `eStdDev`| stddev    | jitter / stability (controller tuning, vibration)    |

Convenience constants in `GVL_MetricAggregation`:

```pascal
fbEnergy.SetAggregation(E_MetricStat.eSum);                        // counter — 8 B/sample
fbTemp.SetAggregation(GVL_MetricAggregation.cMetricEnvelope);      // min+max  — 16 B
fbProcess.SetAggregation(GVL_MetricAggregation.cMetricStandard);   // min+max+mean — 24 B  ← recommended
fbCritical.SetAggregation(GVL_MetricAggregation.cMetricFull);      // all six — 48 B
```

Wire impact: `body_schema = eNumericAggregated (6)`, `sample_size =
popcount(mask) * 8`, body holds the chosen stats per sample in
**bit-index order** (Min first if set, then Max, then Mean, Sum, Count,
StdDev). The receiver expands each aggregated sample into N separate
OTel gauge metrics with `.min` / `.max` / `.mean` / `.sum` / `.count` /
`.stddev` suffixes — dashboards plot them independently.

Aggregation is **numeric-only**. On BOOL / STRING / WSTRING / discrete
metrics the mask is held but ignored, with a warn-log on `SetAggregation`.
Switching aggregation on/off mid-stream is supported but the in-flight
body is not retroactively converted; call `SetAggregation` once at
startup before the first `Observe`.

Welford online formula (B. P. Welford, *Technometrics 4(3), 1962*) keeps
mean and sum-of-squared-deltas exact in single pass; stddev derives
from `sqrt(SumSq / (n-1))` only at flush time, never per `Observe`.

## Wire format

Frames go via `ADSWRITE` to:

* **Index group**: `0x4D424301` (`IG_PUSH_DIAG`)
* **Index offset**: `2` (`IO_PUSH_METRIC_AGG`)

Layout (LE, `pack_mode := 1`):

```
+0x00  version           u8   = 2
+0x01  event_type        u8   = 21 (PUSH_METRIC_AGG)
+0x02  flags             u8   bit0 = has_trace_ctx
                              bit1 = ring_overflowed
+0x03  body_schema       u8   E_MetricBodySchema
+0x04  sample_size       u32
+0x08  sample_count      u32
+0x0C  metric_id         u32  = FNV-1a(name)
+0x10  task_index        u8
+0x11  stat_mask         u8   E_MetricStat bitmask (0 = raw / non-aggregated)
+0x12  reserved          [2]
+0x14  cycle_count_start u32
+0x18  cycle_count_end   u32
+0x1C  reserved          u32
+0x20  dc_time_start     i64  ns since DC epoch (2000-01-01 UTC)
+0x28  dc_time_end       i64
+0x30  name_len          u8   ≤ 63
+0x31  unit_len          u8   ≤ 15
+0x32  reserved          [2]
= 0x34 (52 B fixed header)

if flags.has_trace_ctx:
+0x34  trace_id          [16]
+0x44  span_id           [8]
        (+24 B)

then:
  name (UTF-8, name_len bytes)
  unit (UTF-8, unit_len bytes)
  body (sample_count × sample_size bytes)
```

Total frame = 52 + (24 if trace ctx) + name_len + unit_len + body. A
typical "100 ms / 5 s" gauge frame is ~450 B (50 × LREAL = 400 B body +
~50 B header overhead).

## Trace-context correlation

When a tracer is bound and a span is open at flush time, the frame includes
the W3C trace_id / span_id of the innermost open span. tc-otel attaches
these as `trace_id` / `span_id` attributes on every metric data point in
the batch — backends that filter by attributes can jump from a metric
anomaly to the trace.

```pascal
VAR
    _tracer : FB_TcOtelTracer;
    _spn    : FB_Span;
    _temp   : FB_Metrics;
END_VAR

IF FirstCycle THEN
    _spn.BindTracer(_tracer);
    _temp.Init('motor.temperature', 'celsius');
    _temp.BindTracer(_tracer);
END_IF

_spn.Begin('cycle');
_temp.Observe(rTemperature);   // these samples will carry _spn's trace_id when flushed
_spn.End();

PRG_TaskLog.Call();
```

For a one-shot override use `WithSpan(specificSpan)` — consumed by the
very next flush, then cleared.

## Resource cost per instance

| Item | Bytes |
| --- | --- |
| Header / state | ~140 |
| Body buffer | 8192 |
| Last-value cache | 64 |
| Frame staging stack (transient, only during `_DoFlush`) | ~256 |
| **Total resident** | ~8.4 KB |

Memory is fixed at compile time — no dynamic allocation. For low-rate
metrics this is more than enough; if you have many high-rate metrics on
a single task, plan for the cumulative footprint.

## Backpressure

The per-task `FB_TcOtelTaskMetrics` sender is single-flight: while one
frame is being shipped via `ADSWRITE`, further `Emit` calls return FALSE
and increment a drop counter. `FB_Metrics` accepts the drop and tries
again on its next push window — losing one window is preferable to
per-instance queueing memory cost. Inspect `FB_TcOtelTaskMetrics`
counters (`nFlushOkCount`, `nFlushErrorCount`, `nFlushDropCount`) on the
online view if you suspect backpressure.

## Counters surfaced for ops

`FB_Metrics` instance:

* `_nFlushOk` — successful Emit calls
* `_nDropEmit` — sender returned FALSE (busy / oversize)
* `_nDropAppend` — body buffer was full when a sample arrived
* `_nWrongTypeDrops` — `Observe` called with a different `TypeClass` than locked
