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
| `Flush()` | Force-flush the buffered samples now. No-op when empty; drops the window when sender is back-pressured. |
| `SetSampleIntervalMs(n)` | Min interval between captured samples. `0` = every change (default). |
| `SetPushIntervalMs(n)` | Push window in ms. Default `1000`. |
| `SetChangeDetect(b)` | Toggle byte-wise change suppression. Default `TRUE`. |
| `BindTracer(t)` | Attach a tracer; flushes snapshot its innermost-open span. Pass `0` to unbind. |
| `WithSpan(span)` | One-shot trace-context override for the very next flush — captures the span immediately, survives `End()`. |
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

OR the `E_MetricStat` bits to pick exactly the stats you want — no
hidden indirection:

```pascal
fbEnergy.SetAggregation(E_MetricStat.eSum);                                 // counter — 8 B/sample
fbTemp.SetAggregation(E_MetricStat.eMin OR E_MetricStat.eMax);              // envelope — 16 B
fbProcess.SetAggregation(E_MetricStat.eMin OR E_MetricStat.eMax
                          OR E_MetricStat.eMean);                           // standard — 24 B  ← recommended
fbCritical.SetAggregation(E_MetricStat.eMin OR E_MetricStat.eMax
                           OR E_MetricStat.eMean OR E_MetricStat.eSum
                           OR E_MetricStat.eCount OR E_MetricStat.eStdDev); // full — 48 B
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

When a tracer is bound and a span is open **at flush time**, the frame
carries the W3C trace_id / span_id of that span. tc-otel promotes these
to a native OTel Exemplar on every data point in the batch, so Grafana
Tempo renders "View trace" links directly from a metric anomaly.

### How FB_Metrics reads trace context

The correlation is **read-at-flush-time**, not remembered per sample.
The table shows what ends up on the emitted frame:

| Situation when `_DoFlush` runs                            | Trace on frame |
| --------------------------------------------------------- | -------------- |
| `WithSpan(span)` was called since the last flush          | that span (one-shot, cleared after) |
| Tracer bound, a span is currently `Begin`'d (not `End`'d) | innermost open span on the tracer |
| Tracer bound, no span open                                | none |
| `BindTracer(0)` / never bound                             | none |

A span that opens and closes *between* two flushes leaves **no** trace
on either frame. If you want a closed span's ID pinned on the next
emission, use `WithSpan(span)` just before the final `Observe` in the
window — it snapshots the span even after `End()` by the time flush
fires.

### Lifetime

`FB_TcOtelTracer` and `FB_Span` are FB instances, so their storage lives
with their declaring scope. You don't `Delete()` them. Their `FB_exit`
is a safety-net that force-ends any still-open spans with `eError`
during online change / program stop.

### Three ways to attach a trace to a metric window

**1. Bind a tracer, let innermost-at-flush-time decide.** Use when the
controller has a well-defined span that lives at least through one full
push window.

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
_temp.Observe(rTemperature);
// _spn still open when the 5s push window ends → frame carries its ID
PRG_TaskLog.Call();
```

**2. `WithSpan(span)` — one-shot override, survives `End()`.** Use when
you want a specific span (maybe a short-lived one) tagged on the next
frame regardless of whether it's still open at flush time.

```pascal
_spn.Begin('quick_op');
_temp.Observe(rValue);
_temp.WithSpan(_spn);   // next _DoFlush will pin _spn, even after End()
_spn.End();
// Some time later, the push window expires → frame ships with _spn's ID
```

**3. `Flush()` — force an immediate send.** Use when you absolutely need
the current window shipped *now*, e.g. right before an `End()` of a
one-shot operation. Runs the same A/B swap as a scheduled push.

```pascal
_spn.Begin('op');
_temp.Observe(rValue);
_temp.Flush();          // ships now while _spn is still open
_spn.End();
```

Caveat: `Flush()` is a no-op if the buffer is empty, and drops the
window (`_nDropWindow++`) if the sender's A/B is still busy shipping
the previous frame. For reliable correlation across back-pressure,
prefer (2).

### Unbinding

To stop attaching trace context to future frames, call
`fb.BindTracer(0)`. The internal reference becomes invalid and every
subsequent `_DoFlush` skips the trace-context path.

## Resource cost per instance

Phase 8 added A/B double-buffering so the sender can ship one frame while
`Observe` continues writing to the other half — no MEMCPY of the body.
Per-half = `HDR_RESERVE (256) + BODY_CAPACITY (8192)` = 8448 B.

| Item | Bytes |
| --- | --- |
| Frame A (header reserve + body) | 8448 |
| Frame B (header reserve + body) | 8448 |
| Header staging stack (transient, only during `_DoFlush`) | ~256 |
| Last-value cache | 64 |
| Header / counters / state | ~200 |
| **Total resident** | ~17.2 KB |

Memory is fixed at compile time — no dynamic allocation. Per-task sender
holds a small pointer ring (≤ 64 B) and no longer copies frames into a
staging buffer, so the sender itself shrank by the old 8 KB staging area
— net memory grows by ~8.4 KB per additional metric.

## Backpressure

Phase 8 replaced the drop-on-busy sender with a per-task dispatch ring
of `POINTER TO FB_Metrics` (depth 8). Each `_DoFlush` swaps `_eActiveBuf`,
stamps the frozen header into the pending half's header reserve (one ≤ 160 B
MEMCPY, body stays put), and calls `sender.Enqueue(THIS)`. The sender
drives ADSWRITE directly from the FB's own memory — zero body copy — and
calls back `_NotifyFrameSent` when the transfer completes so the half can
be reused.

Drop paths:

* `_nDropWindow` — a push window expired while the previous frame was still
  queued (sender backlog exceeds A/B = 2). The active half is reset and the
  window is lost. Should stay near zero in practice (one ADS roundtrip is
  much shorter than any reasonable push interval).
* `_nDropEmit` — the sender's dispatch ring was full (8 concurrent pending
  FBs on one task) or the sender was uninitialised. Also counts ADSWRITE
  error replies.
* `FB_TcOtelTaskMetrics.nFlushDropCount` — ring-overflow counter on the
  sender side; mirrors `_nDropEmit` per metric but aggregated per task.
* `FB_TcOtelTaskMetrics.nPendingHighWater` — peak queue depth since boot.
  Useful for tuning `SIZEOF_RING` if you genuinely have ≥ 8 metrics all
  flushing on the same cycle.

## Counters surfaced for ops

`FB_Metrics` instance:

* `_nFlushOk` — successful ADSWRITE completions
* `_nDropEmit` — sender ring full / ADSWRITE error / sender missing
* `_nDropAppend` — active body was full when a sample arrived
* `_nDropWindow` — window expired while previous frame still pending
* `_nWrongTypeDrops` — `Observe` called with a different `TypeClass` than locked
