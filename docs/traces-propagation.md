# Distributed trace propagation

This document explains how to link spans across task boundaries, PLC
boundaries, and external transports so a single logical operation
— a workpiece moving through a production line, an RPC call chain,
a message flowing across a bus — renders as one tree in Tempo.

Prerequisites: the base traces pipeline is set up per
[`traces-setup.md`](traces-setup.md). Consumer-side machinery (accepting
an incoming W3C traceparent on `Begin()`) has been in place since
Phase 1; Phase 5 added the producer side (`FB_Span.TraceParent`, and the task-global `CurrentTraceParent()`
formatting a real header, `F_HashWorkpieceId` for transport-less
handoffs, and the `FB_Span.Begin` overload so you rarely need to drop
down to the low-level tracer).

## Why propagate

By default every `FB_Span.Begin` opens a trace rooted in the local
PLC task. That is fine for a single-cycle operation but insufficient
when the logical unit of work spans multiple tasks, PLCs, or
transports. Propagation lets the producer of an event tell the
consumer "this is part of trace X, my span was Y" so the consumer's
new span links in as a child. Tempo then aggregates them as one
trace.

The primitive that carries the link is the W3C traceparent string:

    00-<32-hex trace_id>-<16-hex parent_span_id>-01
      |                  |                       |
      version            producer's span         flags (sampled=1)

tc-otel parses this string verbatim, so the PLC only has to produce
and consume the string — no binary encoding, no round-trip to the
backend.

## Primitive: `FB_Span.TraceParent`

Per-instance, safe to use when multiple `FB_Span` instances live on
the same task:

```pascal
VAR
    spn : FB_Span;
    sTp : STRING(60);
END_VAR

spn.Begin('process_step');
// ... work ...
sTp := spn.TraceParent;        // cites THIS instance's span
// ... hand `sTp` off to the next module (GVL, RPC, MQTT, etc.) ...
spn.End();
```

Returns the traceparent string formatted as `00-<trace_id>-<span_id>-01`.
Returns an empty string when the span is not open. Must be called
**before** `End()` — the slot is recycled on End.

Under the hood, trace_id and span_id are **minted on the PLC** at
`Begin()` time (xorshift64 seeded from `F_GetActualDcTime64()` XOR
task_index), stored in the slot associated with this FB_Span
instance, and shipped verbatim in the SPAN_BEGIN frame so tc-otel
honours the same bytes the PLC just returned. No return channel,
no round-trip latency.

### Task-global variant: `PRG_TaskLog.Span.CurrentTraceParent()`

There is also a task-scoped accessor:

```pascal
sTp := PRG_TaskLog.Span.CurrentTraceParent();
```

It returns the traceparent for the **innermost span on the task's
span stack**, regardless of which FB_Span instance owns it. Use
only for nested-span patterns where you are sure you are inside
a single active span chain (e.g. a helper function reading "the
span my caller opened").

**Do not use it with parallel FB_Span instances on the same task.**
If `FB_Motion.spn` and `FB_Pump.spn` both hold open spans,
`CurrentTraceParent()` returns whichever was pushed most recently
— which is almost never what the caller expects. Use
`<instance>.TraceParent` instead.

## Pattern A — same PLC, two tasks, via GVL

The simplest case: a producer task opens a span, a consumer task
picks up after it.

```pascal
VAR_GLOBAL  // in GVL_Work
    sTraceParent : STRING(60);
END_VAR

// --- Producer task A ---
spnProducer.Begin('enqueue_job', eKind := E_SpanKind.eClient);
// ... work ...
GVL_Work.sTraceParent := spnProducer.TraceParent;
spnProducer.End();

// --- Consumer task B, some cycles later ---
spnConsumer.Begin(
    sName        := 'process_job',
    eKind        := E_SpanKind.eServer,
    sTraceParent := GVL_Work.sTraceParent);
// ... work ...
spnConsumer.End();
```

Tempo renders: Client span → Server span, same trace, correct parent.
This is the pattern the live test `PRG_TestTracePropagation` Scenario
1 exercises.

## Pattern B — two PLCs via ADS-RPC

Expose an ADS-RPC method that takes the traceparent as a parameter.
`STRING(60)` fits comfortably in the default RPC parameter budget.

Server side (PLC B):

```pascal
METHOD PUBLIC ProcessOrder
VAR_INPUT
    nOrderId     : UDINT;
    sTraceParent : STRING(60);
END_VAR

spnOrder.Begin(
    sName        := 'process_order',
    eKind        := E_SpanKind.eServer,
    sTraceParent := sTraceParent);
spnOrder.AddLInt('order.id', UDINT_TO_LINT(nOrderId));
// ... work ...
spnOrder.End();
```

Client side (PLC A):

```pascal
spnSubmit.Begin('submit_order', eKind := E_SpanKind.eClient);
sTp := spnSubmit.TraceParent;
fbAdsRpc(
    sNetId := '172.16.4.1.1.1',
    nPort  := 851,
    sPath  := 'PLC_PRG.fbOrderServer.ProcessOrder',
    // ... parameter serialisation with nOrderId + sTp ...
);
spnSubmit.End();
```

Tempo renders one trace with the producer's Client span (PLC A) and
the consumer's Server span (PLC B) linked by parent_span_id. Both
PLCs must export to the same tc-otel instance (or to a federated
Tempo backend).

## Pattern C — MQTT or other bus

Embed the traceparent as a field in the message payload, or as a
segment in the topic. Consumer parses it back out and passes it to
`Begin()`.

Payload example (JSON over MQTT):

```json
{
  "workpiece": "WP-1234",
  "operation": "label_print",
  "traceparent": "00-aabb…-cdef…-01"
}
```

Consumer on the subscriber PLC reads the field and calls `Begin(sName,
eKind, sTraceParent := sParsedFromPayload)`. Transport-agnostic — the
PLC doesn't care whether the string arrived via MQTT, OPC UA,
Fieldbus, RFID, Ethernet/IP, or a human typing it in.

## Pattern D — transport-less handoff via hashed identifiers

When the physical product flows through stations via conveyor or
tray and the only shared identifier is a scanned barcode / RFID ID
(no data bus), `F_TraceParentFromString` builds a complete
traceparent by hashing two domain identifiers into the trace_id and
parent_span_id positions:

```pascal
F_TraceParentFromString(
    sTraceId  : STRING(80);          // hashed → 32-hex trace_id
    sParentId : STRING(80) := '';    // hashed → 16-hex span_id,
                                     // or all-zero when empty
) : STRING(60)
```

### Aggregation-only (siblings under one trace_id)

The simplest case — every station sharing the same workpiece ID
ends up in the same trace, but spans are all siblings (no
parent-child linkage):

```pascal
VAR
    sWp  : STRING(40) := 'WP-1234';
    sTp  : STRING(60);
END_VAR

sTp := F_TraceParentFromString(sTraceId := sWp);

spnStation.Begin(
    sName        := 'station_weigh',
    eKind        := E_SpanKind.eInternal,
    sTraceParent := sTp);
spnStation.AddString('workpiece_id', sWp);
// ... work ...
spnStation.End();
```

Every PLC that hashes the same `sTraceId` produces the same 32-hex
trace_id. Tempo aggregates their spans under one trace. Adequate
for "what happened to WP-1234?" queries; less good for "which
station fed which?" visualisations.

### Parent-child linkage (full tree)

To get a real tree in Tempo, both producer and consumer must build
their traceparent via `F_TraceParentFromString` using consistent
identifiers. The producer passes its own `sParentId` so its minted
span_id is the hash of that identifier (not the random xorshift
default), and the consumer cites the same identifier:

```pascal
// Producer station:
sTpProducer := F_TraceParentFromString(
    sTraceId  := 'WP-1234',
    sParentId := 'upstream-feeder');      // its own station-id
spnProducer.Begin(
    sName        := 'station_weigh',
    sTraceParent := sTpProducer);
// ... work ...
spnProducer.End();

// Consumer station, same workpiece, cites the producer:
sTpConsumer := F_TraceParentFromString(
    sTraceId  := 'WP-1234',
    sParentId := 'station_weigh');        // the producer's id
spnConsumer.Begin(
    sName        := 'station_inspect',
    sTraceParent := sTpConsumer);
```

### Symmetry rule

`F_TraceParentFromString` only produces a working parent-child link
when BOTH sides hash the same `sParentId` via this helper. If the
producer calls `FB_Span.Begin(sName := '...')` without a
`sTraceParent`, its span_id is a random xorshift draw; the
consumer's `parent_span_id` then refers to a span the producer
never emitted, and Tempo renders it as an orphaned child. The
chain works only when:

1. Producer and consumer agree on the `sParentId` string (same
   domain identifier on both sides).
2. Producer feeds its own `F_TraceParentFromString` output to
   `FB_Span.Begin(sTraceParent := ...)` — so its emitted span_id
   equals the consumer's expected `parent_span_id`.

For pure aggregation (siblings under a trace_id) these rules don't
apply — leave `sParentId` empty on both sides.

## Pattern E — per-controller isolation via `FB_Log4TcTracer`

A and D keep each FB_Span's span_id consistent across the producer
and consumer. Pattern E solves a different problem: **two logically
independent controllers on the same PLC task**.

By default every `FB_Span` in the process funnels through the
task-global tracer (`PRG_TaskLog.Span`). Its `aStack` records
nesting but it is shared state — if `FB_Motion` opens `spnOuter`
and before it ends `FB_Pump` opens `spnUnrelated`, the two spans
end up stacked together. Any nested `FB_Span.Begin` on either
controller thereafter picks the wrong parent, and
`PRG_TaskLog.Span.CurrentTraceParent()` returns whichever FB
pushed most recently.

Phase 6 Stage 1 adds `FB_Log4TcTracer` — an instance-scoped
tracer that keeps its own open-span list. Bind an `FB_Span` to
one of these via `FB_Span.BindTracer(ADR(tracer))` before the
first `Begin()`, and:

* Nested `FB_Span.Begin` under the same tracer automatically picks
  the tracer's innermost span as parent (via the empty-traceparent
  auto-chain).
* `tracer.CurrentTraceParent` returns THIS tracer's innermost
  span, ignoring every other tracer on the same task.
* Two controllers each with their own tracer instance are fully
  isolated — no cross-contamination even when they interleave
  Begin / End calls.

Wire emission still goes through the task's log buffer; `local_id`
allocation is still task-wide (one shared counter). What Stage 1
changes is *parent resolution only*. The Rust side (`SpanDispatcher`)
is untouched — it keys spans by `(task_index, local_id)` as before.

```pascal
FUNCTION_BLOCK FB_MotionController
VAR
    _tracer  : FB_Log4TcTracer;      // one tracer per controller
    _spnMove : FB_Span;
    _spnSub  : FB_Span;
    _bInit   : BOOL;
END_VAR

IF NOT _bInit THEN
    _spnMove.BindTracer(ADR(_tracer));
    _spnSub.BindTracer(ADR(_tracer));
    _bInit := TRUE;
END_IF

_spnMove.Begin('move_to_pos');
// ... outer work ...
_spnSub.Begin('check_bounds');     // auto-nests under _spnMove,
                                   // regardless of any FB_Pump
                                   // span open in a sibling FB
// ...
_spnSub.End();
// ...
_spnMove.End();
```

Use this when:
* Multiple independent controllers live on the same PLC task.
* You care about correct parent-child nesting per controller.
* `tracer.CurrentTraceParent` is the right scope for your outbound
  propagation — not "whatever is innermost on the whole task".

Don't use it when:
* Your controller has exactly one `FB_Span` at a time → default
  task tracer is fine, simpler.
* You want task-level nesting across multiple helper functions in
  one call chain → default task tracer's `aStack` is exactly that.

`PRG_TestTracerInstance` in the `log4Tc_Tester` project exercises
this pattern with two parallel controllers in an interleaved
Begin/End sequence.

## Cross-PLC clock synchronisation

Span `startTimeUnixNano` comes from `F_GetActualDcTime64()`, which
returns EtherCAT Distributed Clock time on the local segment. Two
PLCs on **different** EtherCAT segments have **independent** DC
masters and may drift by µs to ms against each other.

### Recommendations

1. **Run NTP or PTP at the OS layer** on every PLC that participates
   in a shared trace. A standard `ntpd` setup brings system-clock
   drift to roughly 1 ms across the factory floor — well inside
   Tempo's tolerance. This is the practical minimum for cross-PLC
   tracing and is what we document.
2. `TwinCAT_SystemInfoVarList._AppInfo.StartTime` is NTP-synchronised
   and can be used as an offset anchor if sub-ms audit precision is
   ever required in addition to trace rendering.
3. For higher precision, connect both PLCs' EtherCAT segments to the
   same DC master via a bridge device. Rare in practice.

### How Tempo renders drift

Tempo and Grafana handle small negative gaps between parent-end and
child-start gracefully — the tree still renders correctly, the gap
just shows as a tiny negative duration in the waterfall. Drift in
the µs range is invisible to the human eye in the UI.

### Offset correction in tc-otel (future)

If a deployment reports drift large enough to break analysis, a
tc-otel-side correction pipeline (learn per-PLC offset from event
arrival timestamps, apply before OTLP export) can be added as a
Phase 6 follow-up. Not currently implemented.

## Encoding alternatives

The canonical form is the 55-character W3C traceparent string. If a
transport has tight byte budgets (small Fieldbus frames, 64-byte
RFID tags), the same information fits in 25 binary bytes:

| Offset | Bytes | Field |
|-------:|------:|-------|
|      0 |     1 | version = `0x00` |
|      1 |    16 | trace_id |
|     17 |     8 | span_id |

Custom parse on the consumer is simple — reconstruct the W3C string
and pass it to `Begin()`. Ship W3C in v1 unless the transport forces
otherwise; every extra encoding is a compatibility burden.

## Cardinality guidance

OpenTelemetry best practice: span **names** and attribute **keys**
must be drawn from finite, enumerable sets. Attribute **values** may
vary unboundedly.

Bad — unbounded span names explode storage:

```pascal
// DON'T
spn.Begin(CONCAT('process_order_', DINT_TO_STRING(nOrderId)));
```

Good — fixed name, identifier as an attribute:

```pascal
// DO
spn.Begin('process_order');
spn.AddInt('order.id', nOrderId);
```

Same principle for attribute keys. `workpiece.id` is fine; so is
`machine.zone`. Avoid constructing keys like `'temp_' + sSensorName`
— use `'temperature'` and add `'sensor.name'` as a separate
attribute instead.

## Live test

`PRG_TestTracePropagation.TcPOU` in the `log4Tc_Tester` project
cycles through three scenarios on independent cadences:

* Scenario 1 — Pattern A (explicit handoff, producer→consumer, every 3 s)
* Scenario 2 — Pattern D (hash-based, three stations sharing a
  workpiece-derived trace_id, every 5 s)
* Scenario 3 — Chained producer → consumer → consumer (every 7 s)

Load the tester, watch Tempo Explore, and filter by span name
`order_submit`, `station_weigh`, or `chain_A` to inspect each
scenario's trace structure.
