# Traces — Wire Format & Propagation

Architecture and implementation details for distributed tracing in tc-otel. See [traces-setup.md](traces-setup.md) for the user-facing howto.

## Design Principles

1. **PLC-side API** matches the `opentelemetry`-SDK mental model: begin a span, add attributes/events, end the span. Nesting implicit via task stack.
2. **Deterministic and allocation-free on PLC**: no GUID generation, no dynamic strings, no variable-depth hot-path structures.
3. **tc-otel does the heavy lifting**: 128-bit trace_id and 64-bit span_id generation, parent-span resolution, attribute/event accumulation, OTLP serialisation.
4. **Same clock source as logs**: timestamps from PLC's `F_GetActualDcTime64()`, unchanged by tc-otel. Logs and spans align on Grafana time axis at nanosecond precision.
5. **Graceful degradation**: orphaned spans on tc-otel restart, fallback to ingest time if DC clock missing, fresh local_id space on PLC reconnect.

## Wire Format

All frames are little-endian with a 12-byte header:

```
+0x00  u8    event_type    1=BEGIN, 2=ATTR, 3=EVENT, 4=END
+0x01  u8    local_id      0..=254 task-local span handle
                           255 reserved for "no parent" sentinel
+0x02  u8    task_index    output of GETCURTASKINDEXEX
+0x03  u8    flags         per-event flags (see below)
+0x04  i64   dc_time       ns since DC epoch 2000-01-01 UTC
+0x0C  payload...
```

Flags (currently used):
- `BEGIN.flag_is_root (1<<0)` — parent_local_id ignored, tc-otel mints new trace_id
- `BEGIN.flag_has_external_parent (1<<1)` — payload contains W3C traceparent string; tc-otel parses for trace_id + parent_span_id
- `BEGIN.flag_sampled (1<<2)` — reserved for future PLC-side sampling opt-out; currently always treated as sampled

Unknown flags must be ignored by decoders.

### Responsibility Split

| Concern | PLC | Rust (tc-otel) |
|---------|-----|---|
| DC timestamps | generates per event | consumes verbatim |
| trace_id (128 bit) | — | generates on root span |
| span_id (64 bit) | — | generates on every BEGIN |
| Parent linkage | emits parent_local_id (u8) | maps to actual span_id |
| Attribute accumulation | emits as stream | holds in PendingSpan buffer |
| Event accumulation | emits as stream | holds in PendingSpan buffer |
| String storage | fixed STRING(N) buffers | String allocations OK |
| Serialisation | writes wire events | builds OTLP JSON |
| Timeout recovery | — | TTL sweeper evicts incomplete spans |
| W3C traceparent parsing | passes string through | parses on SPAN_BEGIN |

## Distributed Trace Propagation

Distributed tracing links spans across task boundaries, PLC boundaries, and transports so a logical operation (e.g., workpiece through production line, RPC call chain) renders as one tree in Tempo.

### W3C Traceparent Format

The primitive carrying the link is the W3C traceparent string:

```
00-<32-hex trace_id>-<16-hex parent_span_id>-01
  |         |              |                  |
  version   trace_id       producer's span    flags (sampled=1)
```

Example: `00-aaaabbbbccccddddeeeeffffgggghhhh-0123456789abcdef-01`

PLC producers format this string via `FB_Span.TraceParent()` property or `CurrentTraceParent()` function. Consumers accept it in `FB_Span.Begin()` overload with a traceparent parameter. tc-otel parses the string on SPAN_BEGIN with `flag_has_external_parent` set.

### Consumer-Side Machinery

When a span receives an external traceparent:

1. tc-otel decoder parses the 32-hex trace_id and 16-hex parent_span_id from the string.
2. New span is created with the **same trace_id** as the producer.
3. parent_span_id points back to the producer's span.
4. Tempo aggregates all spans with the same trace_id into one distributed trace.

### Producer-Side Machinery

When a span needs to propagate a link (e.g., before calling an RPC):

1. Call `FB_Span.TraceParent()` to format the current span's identifiers as a W3C string.
2. Pass the string to the consuming task/PLC/transport (in message, variable, MQTT topic, etc.).
3. Consumer's `FB_Span.Begin(traceparent:=...)` parses and links back.

### Instance-Based Tracer (Phase 6+)

Modern architecture flips ownership: `FB_Span` is the authoritative home of its own span state. The tracer role shrinks to coordination: RNG state, wire-emit glue, and optional nesting-aware "currently open" pointer.

- **Parallel controllers no longer race**: each `FB_Span` instance carries its own parent reference.
- **No fixed slot pool**: open span count limited only by memory, not a hard 255 cap.
- **Cross-task spans possible**: a span begun on motion task can end on safety task if both tasks cooperate via traceparent propagation.
- **Reduced bookkeeping**: `ST_SpanSlot` array eliminated; `FB_Span` instance is self-contained.

The tracer's per-task state now holds only:

- RNG seed for span_id generation.
- Pointer to the "currently open" span (for implicit parent-child nesting via task stack).
- Wire-emit glue to the ADS buffer.

## See Also

- [traces-setup.md](traces-setup.md) — User setup guide
- [Architecture](architecture.md) — System overview
- [push-metrics-wire-format.md](push-metrics-wire-format.md) — Metrics encoding
- [push-diagnostics-wire-format.md](push-diagnostics-wire-format.md) — Diagnostics encoding
