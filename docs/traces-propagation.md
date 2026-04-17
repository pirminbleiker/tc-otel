# Distributed trace propagation

This document explains how to link spans across task boundaries, PLC
boundaries, and external transports so a single logical operation
— a workpiece moving through a production line, an RPC call chain,
a message flowing across a bus — renders as one tree in Tempo.

Prerequisites: the base traces pipeline is set up per
[`traces-setup.md`](traces-setup.md). Consumer-side machinery (accepting
an incoming W3C traceparent on `Begin()`) has been in place since
Phase 1; Phase 5 added the producer side (`CurrentTraceParent()`
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

## Primitive: `CurrentTraceParent()`

```pascal
VAR
    sTp : STRING(60);
END_VAR

sTp := PRG_TaskLog.Span.CurrentTraceParent();
```

Returns the traceparent string for the **innermost currently-open
span** on this task, formatted as `00-<trace_id>-<span_id>-01`.
Returns an empty string when no span is open.

Call it **before** `End()` on the span you want to cite — the slot
is recycled on End, so after that it will point at whatever the next
caller opened.

Example — capturing the traceparent right before End:

```pascal
spn.Begin('process_step');
// ... work ...
sTp := PRG_TaskLog.Span.CurrentTraceParent();
// ... hand `sTp` off to the next module (GVL, RPC, MQTT, etc.) ...
spn.End();
```

Underneath, the trace_id and span_id are **minted on the PLC** at
`Begin()` time (xorshift64 seeded from `F_GetActualDcTime64()` XOR
task_index), stored in the slot, and shipped verbatim in the
SPAN_BEGIN frame so tc-otel honours the same bytes the PLC just
returned. No return channel, no round-trip latency.

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
GVL_Work.sTraceParent := PRG_TaskLog.Span.CurrentTraceParent();
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
sTp := PRG_TaskLog.Span.CurrentTraceParent();
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

## Pattern D — transport-less handoff (workpiece-centric)

When the physical product flows through stations via conveyor or
tray and the only shared identifier is a scanned barcode / RFID
ID, use `F_TraceParentFromWorkpieceId` to derive a complete
traceparent from the identifier:

```pascal
VAR
    sWp  : STRING(40) := 'WP-1234';
    sTp  : STRING(60);
END_VAR

sTp := F_TraceParentFromWorkpieceId(sWorkpieceId := sWp);

spnStation.Begin(
    sName        := 'station_weigh',
    eKind        := E_SpanKind.eInternal,
    sTraceParent := sTp);
spnStation.AddString('workpiece_id', sWp);
// ... work ...
spnStation.End();
```

Every PLC that scans the same workpiece ID produces the same 32-hex
trace_id. Tempo aggregates their spans under one trace.

### With an upstream sender span_id

If the previous station did open a span and shipped its span_id
alongside the piece (16 hex chars), pass it as `sSenderSpanId` to
get a full parent-child link:

```pascal
sTp := F_TraceParentFromWorkpieceId(
    sWorkpieceId  := sWp,
    sSenderSpanId := sUpstreamSpanIdHex);

spnStation.Begin(
    sName        := 'station_inspect',
    eKind        := E_SpanKind.eServer,
    sTraceParent := sTp);
```

With `sSenderSpanId` empty (the default), all-zero span_id is
substituted — Tempo treats the consumer span as root within the
trace. This is the "stations are siblings under the common
workpiece trace_id" mode: adequate for "what happened to WP-1234"
queries; less good for "which station fed which" visualisations.

The helper rejects malformed `sSenderSpanId` (anything other than
exactly 16 hex chars) and falls back to all-zero, so callers don't
need to validate.

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
