# OpenTelemetry Traces — Design

Status: proposal, not yet implemented. Replaces the currently-dormant
`AdsSpanEntry` one-shot span wire format (parser exists but router drops
the result — see `crates/tc-otel-ads/src/router.rs:176`).

## Goals

1. PLC-side API matches the mental model of `opentelemetry`-SDK users:
   begin a span, add attributes / events, end the span. Nesting implicit
   via a per-task stack.
2. PLC stays deterministic and allocation-free. No GUID generation, no
   dynamic strings, no variable-depth structures in the hot path.
3. tc-otel does the heavy lifting: 128-bit `trace_id` and 64-bit
   `span_id` generation, parent-span resolution, attribute/event
   accumulation, OTLP serialisation.
4. Timestamps come from the PLC (`F_GetActualDcTime64()`), unchanged by
   tc-otel. Same clock source as log entries — logs and spans line up
   on the Grafana time axis at the nanosecond.
5. Graceful on tc-otel restart (partial spans orphaned), graceful on
   DC-clock absence (fall back to ingest time), graceful on PLC
   reconnect (fresh `local_id` space).

## Architecture

### Split

```
┌───────────────────────────────────────────────────────────────┐
│ PLC (per task)                                                │
│                                                               │
│   PRG_TaskLog.Span.Begin('op')  ─┐                            │
│   PRG_TaskLog.Span.AddInt(...)   ├─► aSpanSlot[ local_id ]    │
│   PRG_TaskLog.Span.AddEvent(...) │   (fixed-size array)       │
│   PRG_TaskLog.Span.End(h)       ─┘                            │
│                                                               │
│   Every method emits ONE wire event into the existing         │
│   ADS write buffer (same path the logger uses):               │
│     SPAN_BEGIN | SPAN_ATTR | SPAN_EVENT | SPAN_END            │
└──────────────────────┬────────────────────────────────────────┘
                       │ ADSWRITE to tc-otel (batched with logs)
                       ▼
┌───────────────────────────────────────────────────────────────┐
│ tc-otel (Rust)                                                │
│                                                               │
│   router → new SpanEventDispatcher                            │
│     HashMap<SpanKey, PendingSpan>                             │
│       SpanKey = (ams_net_id, task_index, local_id)            │
│       PendingSpan { trace_id, span_id, parent_span_id,        │
│                     name, start_time, attrs, events,          │
│                     deadline }                                │
│                                                               │
│   On SPAN_BEGIN:  new trace_id if root, resolve parent else,  │
│                   new span_id, insert into map                │
│   On SPAN_ATTR:   look up, push to attrs Vec                  │
│   On SPAN_EVENT:  look up, push to events Vec                 │
│   On SPAN_END:    look up, finalise, hand off to existing     │
│                   TraceRecord → OTLP /v1/traces pipeline      │
│                                                               │
│   TTL-cleaner evicts pending spans whose deadline passed      │
│   → emits them with status=Error("incomplete")                │
└───────────────────────────────────────────────────────────────┘
```

### Responsibility summary

| Concern | PLC | Rust |
|---|---|---|
| DC timestamps | generates (per event) | consumes verbatim |
| `trace_id` (128 bit) | — | generates on root span |
| `span_id` (64 bit) | — | generates on every BEGIN |
| Parent linkage | emits `parent_local_id` (u8) | maps to actual `span_id` |
| Attribute accumulation | emits as stream | holds in `PendingSpan` |
| Event accumulation | emits as stream | holds in `PendingSpan` |
| String storage | fixed `STRING(N)` buffers | `String` allocations OK |
| Serialisation | writes fixed wire events | builds OTLP JSON |
| Timeout recovery | — | TTL sweeper |
| W3C `traceparent` parsing | passes string through | parses on `SPAN_BEGIN` |

## Wire Format

All integers little-endian. All frames share a 12-byte header:

```
+0x00  u8    event_type    1=BEGIN, 2=ATTR, 3=EVENT, 4=END
+0x01  u8    local_id      0..=254 task-local span handle
                           255 reserved for "no parent" sentinel
+0x02  u8    task_index    matches GETCURTASKINDEXEX output
+0x03  u8    flags         per-event flags (see below)
+0x04  i64   dc_time       ns since DC epoch 2000-01-01 UTC
+0x0C  payload...
```

`flags` bits (currently used):
- `BEGIN.flag_is_root (1<<0)`  — parent_local_id field should be ignored, this is the top-level span (tc-otel mints a new trace_id)
- `BEGIN.flag_has_external_parent (1<<1)` — payload contains a W3C `traceparent` string that tc-otel must parse for trace_id + parent_span_id
- `BEGIN.flag_sampled (1<<2)` — reserved for a future PLC-side sampling opt-out; currently always treated as 1 by the decoder. Keep the bit defined now so the wire format does not have to change later when / if volume reduction becomes necessary.

Unknown flags must be ignored by the decoder.

### SPAN_BEGIN (event_type=1)

```
+0x0C  u8    parent_local_id   0xFF = no parent, else local_id of parent span
+0x0D  u8    kind              0=Internal, 1=Server, 2=Client, 3=Producer, 4=Consumer
+0x0E  u8    name_len          0..127
+0x0F  u8    reserved
+0x10  char  name[name_len]
+...   (if flag_has_external_parent is set)
       u8    traceparent_len
       char  traceparent[traceparent_len]   W3C "00-<trace>-<span>-01"
```

Typical size: ~28 bytes for a 16-char span name. ~95 bytes with a
W3C header.

### SPAN_ATTR (event_type=2)

```
+0x0C  u8    value_type        1=i64, 2=f64, 3=bool, 4=string
+0x0D  u8    key_len           0..31
+0x0E  u8    value_len         0..127 (string only, ignored otherwise)
+0x0F  u8    reserved
+0x10  char  key[key_len]
+...   value payload
         i64    → 8 bytes
         f64    → 8 bytes
         bool   → 1 byte
         string → value_len bytes
```

Typical size: ~30-50 bytes. Attribute's own dc_time is carried in the
header purely for debug/ordering; it is **not** exported to OTLP since
OTel attributes have no timestamp.

### SPAN_EVENT (event_type=3)

```
+0x0C  u8    name_len          0..31
+0x0D  u8    attr_count        0..4  (inline mini-attributes)
+0x0E  u16   reserved
+0x10  char  name[name_len]
+...   attr_count × inline-attr
         u8    value_type   (1=i64, 2=bool; string events use SPAN_ATTR pre-emit)
         u8    key_len
         u8    value_len
         u8    reserved
         char  key[key_len]
         ...   value payload
```

Events ship their own DC timestamp in the frame header — maps directly
to `Event.timeUnixNano` in OTLP.

### SPAN_END (event_type=4)

```
+0x0C  u8    status            0=Unset, 1=Ok, 2=Error
+0x0D  u8    msg_len           0..127
+0x0E  u16   reserved
+0x10  char  status_msg[msg_len]
```

Frame header's `dc_time` = end_time.

## PLC API

The PLC side exposes two layers. User code interacts with **`FB_Span`**
only (OOP object, methods + properties). `FB_Log4TcTaskTracer` is the
low-level primitive `FB_Span` is built on — publicly reachable via
`PRG_TaskLog.Span` for advanced use cases, but not the recommended user
surface.

### Enums

```pascal
TYPE E_SpanKind :
(
    Internal := 0,   // non-remote, internal-to-task operation
    Server   := 1,   // handling an inbound RPC / request
    Client   := 2,   // making an outbound RPC / request
    Producer := 3,   // enqueueing onto an async queue / bus
    Consumer := 4    // dequeueing from an async queue / bus
) USINT;
END_TYPE

TYPE E_SpanStatus :
(
    Unset := 0,
    Ok    := 1,
    Error := 2
) USINT;
END_TYPE
```

### User-facing API: `FB_Span`

`FB_Span` is pure OOP — no `VAR_INPUT` / `VAR_OUTPUT`, no body call.
User code declares one `FB_Span` per logical operation and drives it
through method calls. The FB owns the span lifetime, enforces
begin/end pairing, provides a `FB_exit` safety net for abnormal
teardown (online change, program stop, FB deletion).

```pascal
FUNCTION_BLOCK FB_Span
VAR
    _hSpan   : USINT;                             // opaque slot on the low-level tracer
    _bOpen   : BOOL;
    _eStatus : E_SpanStatus := E_SpanStatus.Ok;
    _sMsg    : STRING(80);
END_VAR
(* body empty *)

// Begin a new span. Safe to call again while a previous span is still
// open: the old one is force-ended with status=Error before the new
// one starts.
METHOD PUBLIC Begin : BOOL
VAR_INPUT
    sName : STRING(60);
    eKind : E_SpanKind := E_SpanKind.Internal;
END_VAR

// End the current span. No-op when no span is open.
METHOD PUBLIC End : BOOL

// Typed attribute setters. All ignore the call when no span is open.
METHOD PUBLIC AddInt    : BOOL  VAR_INPUT sKey: STRING(30); nVal: DINT;   END_VAR
METHOD PUBLIC AddLInt   : BOOL  VAR_INPUT sKey: STRING(30); nVal: LINT;   END_VAR
METHOD PUBLIC AddReal   : BOOL  VAR_INPUT sKey: STRING(30); fVal: LREAL;  END_VAR
METHOD PUBLIC AddBool   : BOOL  VAR_INPUT sKey: STRING(30); bVal: BOOL;   END_VAR
METHOD PUBLIC AddString : BOOL  VAR_INPUT sKey: STRING(30); sVal: STRING(120); END_VAR

// Timestamped event on the currently-open span.
METHOD PUBLIC AddEvent  : BOOL  VAR_INPUT sName: STRING(30); END_VAR

// Flag the span as errored; message is flushed by End().
METHOD PUBLIC MarkError : BOOL  VAR_INPUT sMsg: STRING(80); END_VAR

// Read-only state. No setters, no fluent chaining.
PROPERTY PUBLIC IsOpen : BOOL       GET: IsOpen := _bOpen;
PROPERTY PUBLIC Handle : USINT      GET: Handle := _hSpan;
PROPERTY PUBLIC Status : E_SpanStatus GET: Status := _eStatus;

// Safety net: the runtime calls this when the FB instance is destroyed
// (online change with structural rebuild, program stop, manual
// deletion). Closes any still-open span as Error so tc-otel does not
// wait out the TTL.
METHOD FB_exit : BOOL
VAR_INPUT
    bInCopyCode : BOOL;
END_VAR
```

Every call delegates to the low-level tracer and emits exactly one
wire event. No allocation, no string copy beyond fixed buffers, no
RNG — all ID minting happens on the Rust side.

### Low-level tracer (internal primitive)

`FB_Log4TcTaskTracer` is the actual wire emitter and is reachable via
`PRG_TaskLog.Span` as a `REFERENCE TO FB_Log4TcTaskTracer`. Direct use
is reserved for code that cannot own a `FB_Span` instance (e.g.
one-off imperative code in a test harness, or cross-FB span handles
that are passed around deliberately).

```pascal
{attribute 'no_explicit_call' := 'no direct call necessary'}
PROGRAM PRG_TaskLog
VAR
    aTaskLogger       : ARRAY[1..N] OF FB_Log4TcTask;
    aTaskDiag         : ARRAY[1..N] OF FB_Log4TcTaskDiag;
    aTaskDiagConfig   : ARRAY[1..N] OF ST_PushDiagConfig;

    // NEW:
    aTaskTracer       : ARRAY[1..N] OF FB_Log4TcTaskTracer;
END_VAR

METHOD PUBLIC Call
    aTaskLogger[nIdx].Call();
    aTaskDiag[nIdx].Call();
    aTaskTracer[nIdx].Call();   // flushes pending span events into the ADS buffer
END_METHOD

// Access point — identical convenience pattern as TaskContextBuilder
PROPERTY PUBLIC Span : REFERENCE TO FB_Log4TcTaskTracer
    GET: Span REF= aTaskTracer[GETCURTASKINDEXEX()];
```

Low-level methods exist in a handle-based form because the primitive
has no per-span state of its own — each method takes a `hSpan : USINT`
slot identifier returned by `Begin`:

```pascal
METHOD PUBLIC Begin : USINT
VAR_INPUT
    sName        : STRING(60);
    eKind        : E_SpanKind := E_SpanKind.Internal;
    sTraceParent : STRING(60) := '';        // optional W3C traceparent
END_VAR

METHOD PUBLIC AddInt     : BOOL   VAR_INPUT  hSpan: USINT; sKey: STRING(30); nVal: DINT;   END_VAR
METHOD PUBLIC AddLInt    : BOOL   VAR_INPUT  hSpan: USINT; sKey: STRING(30); nVal: LINT;   END_VAR
METHOD PUBLIC AddReal    : BOOL   VAR_INPUT  hSpan: USINT; sKey: STRING(30); fVal: LREAL;  END_VAR
METHOD PUBLIC AddBool    : BOOL   VAR_INPUT  hSpan: USINT; sKey: STRING(30); bVal: BOOL;   END_VAR
METHOD PUBLIC AddString  : BOOL   VAR_INPUT  hSpan: USINT; sKey: STRING(30); sVal: STRING(120); END_VAR
METHOD PUBLIC AddEvent   : BOOL   VAR_INPUT  hSpan: USINT; sName: STRING(30); END_VAR
METHOD PUBLIC SetError   : BOOL   VAR_INPUT  hSpan: USINT; sMsg: STRING(120) := ''; END_VAR
METHOD PUBLIC End        : BOOL   VAR_INPUT  hSpan: USINT; END_VAR
METHOD PUBLIC CurrentTraceParent : STRING(60)
```

### Slot allocation

`local_id` is `nNextLocalId` incremented on every `Begin`, wrapping
0..254 (255 reserved). Rust treats `(ams_net_id, task_index, local_id)`
as the key and is tolerant of reuse: a new `SPAN_BEGIN` with the same
key invalidates any still-pending earlier span under that key (emits
it as timed-out first). Wraps are harmless in practice because a task
can't have more than 16 spans in flight (nesting cap), and the
254-wide space gives >>16× headroom before the next reuse.

## Usage patterns

Three patterns for different use cases. All produce identical wire
output; pick the one that fits the operation's shape.

### Pattern 1 — single-cycle OOP (recommended default)

Operation fits in one PLC cycle:

```pascal
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

### Pattern 2 — multi-cycle OOP with FB composition (motion, recipes, state machines)

Operation spans many cycles. `FB_Span` instance lives in the enclosing
FB's state. `Begin` fires once on rising edge; `End` fires once on the
completion edge of the underlying work:

```pascal
FUNCTION_BLOCK FB_MotionStep
VAR
    fbMoveAbs     : MC_MoveAbsolute;
    spn           : FB_Span;      // span lifetime co-exists with motion
    _bExecutePrev : BOOL;
END_VAR

METHOD PUBLIC Execute : BOOL
VAR_INPUT
    bExecute : BOOL;
    fTarget  : LREAL;
END_VAR
    // Rising edge: open span, start motion
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

PROPERTY PUBLIC Busy  : BOOL  GET: Busy  := fbMoveAbs.Busy;
PROPERTY PUBLIC Done  : BOOL  GET: Done  := fbMoveAbs.Done;
PROPERTY PUBLIC Error : BOOL  GET: Error := fbMoveAbs.Error;
```

Users of `FB_MotionStep` call `Execute(...)` every cycle — the span
tracks the full motion from accept to done automatically. If the
enclosing FB instance is destroyed mid-motion (online change, program
stop), `FB_Span.FB_exit` closes the span with
`status=Error('FB instance destroyed while open')` — tc-otel sees the
end marker rather than waiting out the TTL.

Nested multi-cycle spans — inner FB_Span inside an outer FB_Span — use
the task's implicit parent stack: whichever span was `Begin`-ed most
recently and not yet ended becomes the parent. No handle plumbing
between FBs needed:

```pascal
VAR
    spnRecipe : FB_Span;   // outer: many cycles
    spnStep   : FB_Span;   // inner: also many cycles, parent = spnRecipe
END_VAR

IF bRecipeStart THEN
    spnRecipe.Begin('recipe_brew');
END_IF

IF bStepStart THEN
    spnStep.Begin('step_heat');   // auto-parent = spnRecipe
END_IF

IF bStepDone THEN  spnStep.End();  END_IF
IF bRecipeDone THEN spnRecipe.End(); END_IF
```

### Pattern 3 — imperative via the low-level tracer

When `FB_Span` composition is inconvenient (ad-hoc test code,
cross-FB handles passed deliberately):

```pascal
hSpan := PRG_TaskLog.Span.Begin('drive_setup');
IF NOT bOk THEN
    PRG_TaskLog.Span.SetError(hSpan, 'drive offline');
END_IF
PRG_TaskLog.Span.End(hSpan);
```

This is the same imperative API as before, still supported. It is
explicit about the handle (`hSpan`), and does not get the `FB_exit`
safety net — the caller is responsible for pairing `Begin` with `End`
on every control-flow path.

### Cross-task / cross-PLC propagation

The calling side (producer) reads the current traceparent string:

```pascal
VAR
    spn     : FB_Span;
    sParent : STRING(60);
END_VAR

spn.Begin('send_command', eKind := E_SpanKind.Client);
sParent := PRG_TaskLog.Span.CurrentTraceParent();   // W3C string
// ... ship sParent alongside the RPC payload ...
spn.End();
```

The receiving side (consumer) uses the low-level tracer to thread the
external parent into a new span — `FB_Span` currently has no input for
this, so Pattern 3 is required here:

```pascal
hSpan := PRG_TaskLog.Span.Begin(
    sName        := 'handle_command',
    eKind        := E_SpanKind.Server,
    sTraceParent := sIncomingParent
);
// ...
PRG_TaskLog.Span.End(hSpan);
```

If cross-process propagation becomes common, `FB_Span.Begin` can gain
an optional `sTraceParent` parameter without breaking existing
callers.

## Rust Dispatcher

### New module

`crates/tc-otel-service/src/span_dispatcher.rs`

### Types

```rust
struct SpanKey {
    ams_net_id: AmsNetId,
    task_index: u8,
    local_id: u8,
}

struct PendingSpan {
    trace_id: [u8; 16],
    span_id: [u8; 8],
    parent_span_id: Option<[u8; 8]>,
    name: String,
    kind: SpanKind,
    start_time: DateTime<Utc>,
    attrs: HashMap<String, AttrValue>,
    events: Vec<SpanEvent>,
    deadline: Instant,   // now() + span_ttl
}

struct SpanDispatcher {
    pending: HashMap<SpanKey, PendingSpan>,
    // for parent-lookup on new BEGIN, indexed same way:
    // pending.get(&parent_key).map(|p| p.span_id) → parent_span_id
    trace_tx: mpsc::Sender<TraceRecord>,
    span_ttl: Duration,   // config; default 10s
    max_pending: usize,   // config; default 1024 spans across all PLCs
    rng: rand::rngs::ThreadRng,
}
```

### Event handling

```rust
impl SpanDispatcher {
    fn on_begin(&mut self, key: SpanKey, parent_local: Option<u8>,
                name: String, kind: SpanKind, dc_time: i64,
                traceparent: Option<&str>) {
        // Trace-id: external > parent > new
        let (trace_id, parent_span_id) = if let Some(tp) = traceparent {
            parse_traceparent(tp)  // -> ([u8;16], [u8;8])
        } else if let Some(plocal) = parent_local {
            let pkey = SpanKey { local_id: plocal, ..key };
            match self.pending.get(&pkey) {
                Some(p) => (p.trace_id, Some(p.span_id)),
                None    => (self.new_trace_id(), None),  // orphan — treat as root
            }
        } else {
            (self.new_trace_id(), None)
        };
        let span_id = self.new_span_id();
        // LRU / limit check
        if self.pending.len() >= self.max_pending {
            self.evict_oldest();
        }
        // If key already present, flush the stale entry as timed-out
        if let Some(stale) = self.pending.remove(&key) {
            self.finalise_timed_out(stale);
        }
        self.pending.insert(key, PendingSpan { trace_id, span_id,
            parent_span_id, name, kind,
            start_time: dc_time_to_datetime(dc_time),
            attrs: HashMap::new(), events: Vec::new(),
            deadline: Instant::now() + self.span_ttl,
        });
    }

    fn on_attr(&mut self, key: SpanKey, k: String, v: AttrValue) {
        if let Some(p) = self.pending.get_mut(&key) {
            p.attrs.insert(k, v);
        }
        // silently drop if span not found — race after TTL eviction
    }

    fn on_event(&mut self, key: SpanKey, name: String, dc_time: i64,
                attrs: Vec<(String, AttrValue)>) {
        if let Some(p) = self.pending.get_mut(&key) {
            p.events.push(SpanEvent {
                time: dc_time_to_datetime(dc_time),
                name, attrs,
            });
        }
    }

    fn on_end(&mut self, key: SpanKey, status: SpanStatus, msg: String, dc_time: i64) {
        let Some(mut p) = self.pending.remove(&key) else { return };
        let end_time = dc_time_to_datetime(dc_time);
        let record = TraceRecord::from_pending(p, end_time, status, msg);
        let _ = self.trace_tx.try_send(record);
    }

    fn sweep_timed_out(&mut self) {
        let now = Instant::now();
        let expired: Vec<SpanKey> = self.pending.iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(k, _)| *k).collect();
        for k in expired {
            let Some(p) = self.pending.remove(&k) else { continue };
            self.finalise_timed_out(p);
        }
    }
}
```

### Wire-up in `service.rs`

A parallel channel to the existing `log_tx` / `metric_tx`:

```rust
let (trace_tx, mut trace_rx) = mpsc::channel::<TraceRecord>(256);

let ads_router = AdsRouter::new(...)
    .with_push_sender(push_tx)
    .with_trace_sender(trace_tx);  // NEW

// New task: forwards TraceRecord to TraceDispatcher → OTLP /v1/traces
let trace_dispatcher = TraceDispatcher::new(&self.settings).await?;
tokio::spawn(async move {
    while let Some(record) = trace_rx.recv().await {
        if let Err(e) = trace_dispatcher.dispatch(record).await {
            tracing::error!("trace dispatch error: {e}");
        }
    }
});

// Periodic TTL sweep
let sweeper_span_disp = span_dispatcher.clone();
tokio::spawn(async move {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick.tick().await;
        sweeper_span_disp.lock().await.sweep_timed_out();
    }
});
```

### Router change — extend the v2 log protocol

**Decision**: multiplex trace events into the existing log-batch
pipeline by adding four new `nType` values to the v2 protocol, rather
than introducing a new ADS index group. The PLC's outbound buffer
stays single-path, and `parser.rs::parse_all` already demuxes by type
byte.

New `nType` values:

| nType | Event        | Parser routes into |
|-------|--------------|--------------------|
| 5     | SPAN_BEGIN   | `spans` vec on `ParseResult` |
| 6     | SPAN_ATTR    | same vec (per-local_id ordering) |
| 7     | SPAN_EVENT   | same vec |
| 8     | SPAN_END     | same vec |

Parser extension plan: add `parse_v2_span_event_from_reader` that
reads one of the 4 new frames into a `TraceWireEvent` enum variant,
and route from `parse_v2_from_reader`'s type-byte dispatch. The router
then forwards the vec on `trace_tx` the same way it forwards logs on
`log_tx` today.

Legacy single-shot `AdsSpanEntry` (type 5 in the current parser) is
retired; its tests migrate to the new event-stream form.

## Configuration

New `AppSettings` section:

```toml
[traces]
enabled = true
span_ttl_secs = 10
max_pending = 1024

[traces.export]
endpoint = "http://otel-collector:4318/v1/traces"
batch_size = 100
flush_interval_ms = 1000
```

## Error handling matrix

| Situation | Behaviour |
|---|---|
| ATTR/EVENT/END arrives before BEGIN (or after TTL eviction) | drop silently, tracing::debug log |
| BEGIN with duplicate key while previous still pending | finalise previous as `status=TimedOut`, start new |
| BEGIN references unknown `parent_local_id` | treat as root span (new trace_id), log warn |
| BEGIN carries malformed `traceparent` | log warn, treat as root |
| TTL expires without END | emit with `status=TimedOut("span did not end within N s")` |
| Max pending spans exceeded | evict oldest (LRU), emit with status=TimedOut |
| tc-otel restart | pending lost, new spans from PLC start cleanly |
| DC-clock = 0 | `dc_time_to_datetime` falls back to `Utc::now()` — duration measurement invalid that span only |

## Metrics (so the dispatcher is observable)

Emitted by the dispatcher itself as regular `MetricEntry`:

- `tc.traces.spans_started_total` (counter, per task_name)
- `tc.traces.spans_completed_total` (counter)
- `tc.traces.spans_timed_out_total` (counter) — the must-alert signal
- `tc.traces.spans_dropped_total` (counter, `reason=unknown_key|buffer_full`)
- `tc.traces.pending_spans` (gauge) — current size of pending map

## Implementation phases

**Phase 1 — Rust scaffold + tests.** SpanDispatcher, wire decoder,
dispatcher unit tests with synthetic event streams. No PLC yet.
No live integration. Deliverable: `cargo test` proves assembly works.

**Phase 2 — PLC FB.** Ships together:
* `FB_Log4TcTaskTracer` — the low-level wire emitter exposed via
  `PRG_TaskLog.Span` (Pattern 3 imperative callers)
* `FB_Span` — the primary user-facing OOP object with methods,
  properties and the `FB_exit` safety net (Patterns 1 and 2)
* `E_SpanKind`, `E_SpanStatus` enums
* Wire emitter reuses the existing `FB_LogEntry` buffer path.

Integration-test by driving the Phase-1 dispatcher with a synthetic
PLC replay harness.

**Phase 3 — Router wire-up.** Route `IG_TRACE_EVENT` (or new log
types) through the dispatcher. tc-otel now emits real OTLP traces.
Existing integration tests for AdsSpanEntry are retired.

**Phase 4 — Observability.** Add Tempo or Jaeger to
`docker-compose.observability.yml`, point the otel-collector `traces:`
pipeline at it (currently file-only). Grafana datasource for the
trace backend, dashboard links from log rows / exceed events to span
views via `trace_id`.

**Phase 5 — Cross-PLC / cross-task propagation.** Verify W3C
traceparent round-trip end-to-end. Document how users pass the header
over ADS-RPC / MQTT / their transport of choice.

## Decisions taken

1. **Transport**: extend v2 log protocol with nType 5-8 for
   BEGIN/ATTR/EVENT/END. No separate ADS index group. Keeps the PLC's
   outbound buffer pipeline single-path.
2. **Span completion semantics**: OTLP-conformant full span emitted
   on END only. No partial records.
3. **Sampling**: no head-side decision on the PLC in phase 1. Every
   span is emitted. If wire volume becomes a problem, add Rust-side
   sampling (trace-id-ratio, driven by `[traces].sample_rate` config)
   later — PLC code unaffected. The `flag_sampled` bit in the BEGIN
   frame is reserved now so this can be introduced without a wire
   break.

## Open questions

1. Cardinality guidance for `name` / `key` values — OTel best
   practice says names and keys should be finite, values can vary.
   Worth a short how-to in the setup doc when the feature ships.
