# Traces — Wire Format & Propagation

Architecture and implementation details for distributed tracing in tc-otel. See [traces-setup.md](traces-setup.md) for the user-facing howto.

## Design Principles

1. **PLC-side API** matches the `opentelemetry`-SDK mental model: begin a span, add attributes/events, end the span. Nesting implicit via task stack.
2. **Deterministic and allocation-free on PLC**: no GUID generation, no dynamic strings, no variable-depth hot-path structures.
3. **tc-otel does the heavy lifting**: 128-bit trace_id and 64-bit span_id generation, parent-span resolution, attribute/event accumulation, OTLP serialisation.
4. **Same clock source as logs**: timestamps from PLC's `F_GetActualDcTime64()`, unchanged by tc-otel. Logs and spans align on Grafana time axis at nanosecond precision.
5. **Graceful degradation**: orphaned spans on tc-otel restart, fallback to ingest time if DC clock missing, fresh local_id space on PLC reconnect.

## Wire Format

All frames are little-endian. Frame layout depends on type.

### SPAN_BEGIN Frame (event_type=5) — Phase 6 Stage 3+

12-byte header + payload with always-embedded trace_id/span_id:

```
+0x00  u8    event_type    5 = SPAN_BEGIN
+0x01  u8    local_id      1-byte local_id (transport counter; semantic meaning deprecated in Stage 3)
+0x02  u8    task_index    output of GETCURTASKINDEXEX
+0x03  u8    flags         per-event flags (see below)
+0x04  i64   dc_time       ns since DC epoch 2000-01-01 UTC
+0x0C  payload:
         8B  parent_span_id   [0u8;8] = root; else parent's 8B span_id
         u8  kind             E_SpanKind: 0=INTERNAL, 1=SERVER, 2=CLIENT, ...
         u8  name_len         length of span name (0..127)
         u16 reserved         unused
         16B trace_id         PLC-minted 128-bit trace_id (always present, Stage 3+)
         8B  span_id          PLC-minted 64-bit span_id (always present, Stage 3+)
       name[name_len]
       [1B tp_len + traceparent] if flag_has_external_parent (bit 1) set
```

**Key changes in Stage 3:**
- `parent_local_id` (1B) → `parent_span_id` (8B) for span-id-keyed parent lookup (no 1B→8B slot translation)
- `trace_id` and `span_id` now **always** embedded (no `flag_local_ids` gating)
- `flag_local_ids` flag removed; reserve space for future flags

### SPAN_ATTR Frame (event_type=6) — Phase 6 Stage 3+

Extended 20-byte header carrying 8-byte span_id:

```
+0x00  u8    event_type    6 = SPAN_ATTR
+0x01  u8    span_id[0]    First byte of 8-byte span_id
+0x02  u8    task_index    output of GETCURTASKINDEXEX
+0x03  u8    flags         unused (always 0)
+0x04  i64   dc_time       ns since DC epoch 2000-01-01 UTC
+0x0C  7B    span_id[1..7] Remaining 7 bytes of span_id
+0x13  payload:
         u8  value_type    1=i64, 2=f64, 3=bool, 4=string
         u8  key_len       length of attribute key (0..31)
         u8  value_len     length of value (depends on type)
         u8  reserved
       key[key_len]
       value[value_len]
```

### SPAN_EVENT Frame (event_type=7) — Phase 6 Stage 3+

Extended 20-byte header carrying 8-byte span_id:

```
+0x00  u8    event_type    7 = SPAN_EVENT
+0x01  u8    span_id[0]    First byte of 8-byte span_id
+0x02  u8    task_index    output of GETCURTASKINDEXEX
+0x03  u8    flags         unused (always 0)
+0x04  i64   dc_time       ns since DC epoch 2000-01-01 UTC
+0x0C  7B    span_id[1..7] Remaining 7 bytes of span_id
+0x13  payload:
         u8  name_len      length of event name (0..31)
         u8  attr_count    number of inline attributes (0..4)
         u16 reserved      unused
       name[name_len]
       [for each attr: value_type(1) + key_len(1) + _reserved(1) + key + value(8)]
```

### SPAN_END Frame (event_type=8) — Phase 6 Stage 3+

Extended 20-byte header carrying 8-byte span_id:

```
+0x00  u8    event_type    8 = SPAN_END
+0x01  u8    span_id[0]    First byte of 8-byte span_id
+0x02  u8    task_index    output of GETCURTASKINDEXEX
+0x03  u8    flags         unused (always 0)
+0x04  i64   dc_time       ns since DC epoch 2000-01-01 UTC
+0x0C  7B    span_id[1..7] Remaining 7 bytes of span_id
+0x13  payload:
         u8  status        0=Unset, 1=Ok, 2=Error
         u8  msg_len       length of status message (0..127)
         u16 reserved
       msg[msg_len]
```

### Flags (BEGIN frame only)

- `flag_has_external_parent (1<<1)` — payload contains W3C traceparent string; tc-otel parses for trace_id + parent_span_id. Stage 3+: trace_id/span_id always present; external traceparent overrides wire trace_id
- `flag_sampled (1<<2)` — reserved for future PLC-side sampling opt-out; currently always treated as sampled

**Retired in Stage 3:**
- `flag_is_root (1<<0)` — no longer needed (parent_span_id=[0u8;8] indicates root)
- `flag_local_ids (1<<3)` — no longer needed (trace_id/span_id always present, never gated)

Unknown flags must be ignored by decoders.

### Migration: Phase 2 to Phase 6 Stage 3

- **Phase 2 (legacy)**: SPAN_BEGIN carries parent_local_id, flags gate trace_id/span_id; SPAN_ATTR/EVENT/END use 1-byte local_id, 12-byte header
- **Phase 6 Stage 3 (current)**: SPAN_BEGIN always carries 8B parent_span_id and always-embedded trace_id/span_id; SPAN_ATTR/EVENT/END use 8-byte span_id, 20-byte header
- **Breaking change**: PLC and tc-otel must redeploy together atomically. Mismatch → corrupted spans, protocol violations.
- **Side effect**: `ST_SpanSlot`, `aSlots`, `aStack` arrays retired. `FB_Span` owns state; task tracer holds only RNG + `_pInnermost` chain pointer.

### Responsibility Split

| Concern | PLC | Rust (tc-otel) |
|---------|-----|---|
| DC timestamps | generates per event | consumes verbatim |
| trace_id (128 bit) | mints via xorshift64 RNG; emits on SPAN_BEGIN | override only if W3C traceparent supplied |
| span_id (64 bit) | mints via xorshift64 RNG; emits on SPAN_BEGIN | accepts PLC-minted value; no regeneration |
| Parent linkage | emits parent_span_id (8B, all-zero=root) | looks up pending span by span_id; inherit trace_id or treat as orphan |
| Attribute accumulation | emits as stream via SPAN_ATTR | holds in PendingSpan buffer |
| Event accumulation | emits as stream via SPAN_EVENT | holds in PendingSpan buffer |
| String storage | fixed STRING(N) buffers | String allocations OK |
| Serialisation | writes wire frames | builds OTLP JSON |
| Timeout recovery | — | TTL sweeper evicts incomplete spans |
| W3C traceparent parsing | passes string through; option to override parent | parses on SPAN_BEGIN if flag_has_external_parent |
| Span ownership (Stage 3+) | FB_Span instance holds trace_id/span_id bytes | task tracer RNG + _pInnermost chain only |

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
