# Instance-based tracer (Phase 6 design)

## Motivation

Phase 5 shipped a working trace pipeline but kept the Phase-1 slot
architecture: exactly one `FB_Log4TcTaskTracer` per task owning an
`ARRAY[0..254] OF ST_SpanSlot` + shared `aStack`. Every `FB_Span` on
the task funnels through that singleton — which means:

* **Parallel controllers race.** Two `FB_Span` instances open
  simultaneously (e.g. `fbMotion.spn` + `fbPump.spn`) push onto the
  same stack. The global "innermost open span" is whichever was
  pushed last — almost never what either controller expects.
  Phase 5.1 patches this with a per-instance `FB_Span.TraceParent`,
  but the underlying shared pool is still there.
* **Slot pool is fixed at 255.** Every open span anywhere on the
  task consumes one of those slots. Hitting the cap on a busy task
  silently drops spans.
* **No cross-task spans.** A span is bound to the task whose tracer
  created it. If a controller's logical operation starts on the
  motion task and finishes on the safety task, the span cannot
  span both.
* **Redundant bookkeeping.** The slot array duplicates what the
  `FB_Span` instance already carries: open/closed state, parent
  reference, identifiers.

Phase 6 flips the ownership: `FB_Span` becomes the authoritative
home of its own span state. The tracer shrinks to a coordination
role — RNG state, wire-emit glue, and an optional nesting-aware
"currently open" pointer. `ST_SpanSlot` disappears.

## Target architecture

```
FB_Span [n, one per logical operation]
  _trace_id       : ARRAY[0..15] OF BYTE
  _span_id        : ARRAY[0..7]  OF BYTE
  _parent_span    : POINTER TO FB_Span    // NULL = root, else linked-list
  _pTracer        : REFERENCE TO FB_Log4TcTracer   // or implicit task tracer
  _bOpen, _eStatus, _sMsg
  _kind           : E_SpanKind
  _name           : STRING(60)

FB_Log4TcTracer [n, one per controller/entity]
  _pInnermostOpen : POINTER TO FB_Span    // top of this tracer's nesting
  _nXorState      : ULINT                 // instance-local RNG
  _tracer_id      : USINT                 // optional, identifies this tracer
                                          // for debug/telemetry — not needed
                                          // on the wire

PRG_TaskLog [unchanged, per task]
  aTaskTracer[task_idx] : FB_Log4TcTaskTracer   // still exists as default
                                                // tracer for task-scoped use
```

Key flip: the tracer holds a pointer to FB_Span, not an integer slot
handle. FB_Span instances are stable memory addresses in TwinCAT
(unless online-change moves them, see "Risks" below). No dynamic
allocation in the tracer.

## Lifecycle

**Begin()** on an FB_Span (instance `self`, tracer `T`):

1. Seed T's RNG on first call from DC time XOR task_index.
2. Mint `self._trace_id` and `self._span_id` from T's RNG, unless:
   - an external `sTraceParent` is supplied → parse it, inherit
     `trace_id`, set `_parent_span_from_external_header`;
   - OR T's `_pInnermostOpen` is non-NULL → inherit `trace_id` from
     it, set `self._parent_span := T._pInnermostOpen`.
3. Set `self._bOpen := TRUE`.
4. T's `_pInnermostOpen := ADR(self)` so nested spans inside this
   one pick it up as parent.
5. Emit SPAN_BEGIN frame to current task's WriteBuffer (still
   carries `flag_local_ids` + the 24 bytes of trace_id/span_id).

**End()** on `self`:

1. If `self._eStatus = eError`, emit SetError first.
2. Emit SPAN_END frame.
3. Unlink from nesting: `T._pInnermostOpen := self._parent_span`.
4. `self._bOpen := FALSE`.

**TraceParent** property on FB_Span: format the W3C string from
`self._trace_id + self._span_id`. Still works after End() if we
decide to keep the bytes around — Phase 5.1 already discusses the
"must read before End()" caveat.

## Wire-format impact

All of this is on-PLC bookkeeping. The SPAN_BEGIN wire frame does
**not** need new fields:

* `local_id` header byte: becomes informational only. Can be left
  at 0 or used as a telemetry counter per tracer. tc-otel already
  keys on the pregenerated `span_id` when `flag_local_ids` is set.
* `parent_local_id` payload byte: same — can be left at 0xFF (no
  local parent). Parent relationship is conveyed via:
  * explicit `sTraceParent` (when crossing tracers / tasks / PLCs)
  * OR tc-otel's per-`span_id` lookup (for nested spans within one
    tracer, the frame can still set parent_span_id via the
    traceparent bytes).

**One change on the Rust side**: `SpanKey` migrates from
`(ams_net_id, task_index, local_id)` to `(ams_net_id, span_id)`.

The task_index is retained as a span attribute (for filtering in
Tempo) but is no longer part of the identity key. Span lookups
during ATTR / EVENT / END dispatch resolve by span_id only — which
is already the stable identifier Phase 5 ships.

## Backward compatibility

### Existing code keeps working

The `FB_Log4TcTaskTracer` stays in place:

* `PRG_TaskLog.Span` still resolves to `aTaskTracer[GETCURTASKINDEXEX()]`.
* `FB_Span` used without explicit tracer binding still routes to
  the task tracer as today.
* `CurrentTraceParent()` remains for nested-call-chain use.

So a user who hasn't touched their code gets identical behaviour.

### New opt-in API

```pascal
VAR
    myTracer : FB_Log4TcTracer;       // controller-owned instance
    spn      : FB_Span;
END_VAR

spn.BindTracer(REF= myTracer);        // before first Begin
spn.Begin('controller_op');
// ... work ...
spn.End();
```

`BindTracer` sets `_pTracer` on the FB_Span. If unbound, the FB_Span
delegates to `PRG_TaskLog.Span` as today.

### Migration

`ST_SpanSlot` and the `aSlots[0..254]` array become dead weight.
They are kept behind `{attribute 'hide'}` for one release to
preserve ABI compatibility (downstream projects that happened to
reference them), then removed in the next minor version.

## Rust SpanDispatcher changes

```rust
// Before:
struct SpanKey { ams_net_id: AmsNetId, task_index: u8, local_id: u8 }

// After:
struct SpanKey { ams_net_id: AmsNetId, span_id: [u8; 8] }
```

The dispatcher's `on_begin`:

* Already receives pregenerated `span_id` in Phase 5. Use that as
  the insert key.
* Parent resolution order is unchanged: external traceparent first,
  pregenerated IDs next, then fresh mint. The `parent_local_id`-
  based lookup becomes legacy (kept for pre-Phase-5 PLC builds).

`on_attr`, `on_event`, `on_end` look up by span_id instead of the
triple. Wire frames carry span_id in the `pregenerated_span_id`
field (already wired up). No frame-format change.

TTL sweep is unchanged; it already iterates values.

## Cross-task spans

Once `SpanKey` is `span_id`-only, a span can legitimately span
multiple tasks. Controller A's FB_Log4TcTracer emits a BEGIN from
motion_task, an ATTR from safety_task, an END from motion_task —
tc-otel correlates them by span_id. The resulting span record
carries a single `task_index` (the one on BEGIN) and additional
attributes could record the tasks that contributed (follow-up work).

## API details

### New public FB

```pascal
FUNCTION_BLOCK FB_Log4TcTracer
VAR
    _pInnermostOpen : POINTER TO FB_Span;
    _nXorState      : ULINT;
    _bSeeded        : BOOL;
    _tracer_id      : USINT;       // debug/telemetry only
END_VAR

METHOD PUBLIC MintTraceId   : ARRAY[0..15] OF BYTE
METHOD PUBLIC MintSpanId    : ARRAY[0..7]  OF BYTE
METHOD INTERNAL OnBegin     (pSpan : POINTER TO FB_Span)
METHOD INTERNAL OnEnd       (pSpan : POINTER TO FB_Span)
PROPERTY PUBLIC CurrentTraceParent : STRING(60)
    // formats from _pInnermostOpen^ — per-tracer, not task-global
```

`MintTraceId` / `MintSpanId` expose the RNG for `FB_Span` to call
from inside its Begin. `OnBegin` / `OnEnd` hook the linked-list
maintenance (push / pop).

### Changed FB

```pascal
FUNCTION_BLOCK FB_Span
VAR
    _trace_id    : ARRAY[0..15] OF BYTE;
    _span_id     : ARRAY[0..7]  OF BYTE;
    _parent_span : POINTER TO FB_Span;
    _pTracer     : POINTER TO FB_Log4TcTracer;  // NULL → task tracer
    _bOpen       : BOOL;
    _eStatus     : E_SpanStatus;
    _sMsg        : STRING(80);
    _kind        : E_SpanKind;
    _name        : STRING(60);
END_VAR

METHOD PUBLIC BindTracer(pTracer : POINTER TO FB_Log4TcTracer)
METHOD PUBLIC Begin / End / AddInt / AddLInt / AddReal / AddBool / AddString / AddEvent / MarkError
PROPERTY PUBLIC TraceParent / IsOpen / Handle / Status
```

`Handle` becomes advisory — it returns `_tracer_id << 8 | _some_counter`
for logging but has no functional meaning. Can be deprecated.

### Deprecated / removed

* `ST_SpanSlot.TcDUT` — removed
* `FB_Log4TcTaskTracer.aSlots / aStack / nStackDepth / nNextLocalId`
  — removed (internal restructure; not public API)
* Slot-based `TraceParentFor(hSpan)` — becomes unused, superseded
  by per-instance `FB_Span.TraceParent`. Kept for one release for
  backward compat.

## Implementation plan

**Stage 1 — Rust side (no PLC changes):**
1. Add a second code path in SpanDispatcher keyed by `span_id`.
2. Use it when `flag_local_ids` is set; keep the old `(task, local_id)`
   path as fallback.
3. Unit + integration tests.
4. Merge. Both keying modes coexist in tc-otel.

**Stage 2 — PLC introduction (opt-in):**
1. Add `FB_Log4TcTracer` as a new public FB. Internally uses the
   instance-local storage described above.
2. Add `FB_Span.BindTracer` method.
3. `FB_Span.Begin/End/...` gains a branch: if `_pTracer` set, route
   through it; else fall back to `PRG_TaskLog.Span` (task tracer).
4. Tester adds Scenario 4 demonstrating two parallel `FB_Log4TcTracer`
   instances on one task, each with its own nested span chain.
5. Merge.

**Stage 3 — Internal refactor (invisible to users):**
1. Refactor `FB_Log4TcTaskTracer` to use the same instance-owned
   span-state model. Drop `aSlots` / `aStack` / `nStackDepth` /
   `nNextLocalId`.
2. `ST_SpanSlot.TcDUT` hidden (attribute) but kept.
3. Rust side drops the legacy `(task, local_id)` fallback.
4. Merge.

**Stage 4 — Cleanup:**
1. Delete `ST_SpanSlot.TcDUT`.
2. Delete `TraceParentFor(hSpan)` in favour of `FB_Span.TraceParent`
   everywhere.
3. Bump library minor version.

Each stage is independently mergeable and keeps the pipeline
working. Roll back any stage without breaking the ones before.

## Risks

**R1 — Pointer stability across online change.**
TwinCAT's online change may relocate FBs. A tracer's
`_pInnermostOpen` pointer could dangle. Mitigation: span TTL on
tc-otel side flushes any stranded span after 10 s; online changes
that happen during an open span finalise it with `status=TimedOut`.
Acceptable for real production — online changes are rare events
and coincidence with an open span is rarer still.

Alternative: each `FB_Log4TcTracer.OnBegin` walks back any dangling
`_parent_span` before linking. Adds one cycle per Begin, worth it
only if online-change-during-span proves common.

**R2 — Two `BindTracer` calls with different tracers.**
User error. Document: "Bind once, before first Begin. Rebinding
while the FB is open is undefined." Add a guard in `BindTracer`
that asserts `NOT _bOpen`.

**R3 — `FB_Log4TcTracer` declared in an obvious place.**
Users will want it as a VAR on their controller FB. Cycle-accurate
destruction semantics apply (`FB_exit` cascades). Document
lifecycle.

**R4 — Rust `SpanKey` migration race.**
During Stage 1 / 2, tc-otel has two lookup paths. A malformed or
pre-Phase-5 emitter could produce events that match both a
`(task, local_id)` key AND (by collision) a `span_id` key. Extremely
unlikely but possible. Mitigation: prefer the span_id path when
`flag_local_ids` is set; fall back to legacy only when the flag is
absent. Document this ordering in the dispatcher.

**R5 — Existing `FB_Log4TcTaskTracer` deep internals are
referenced.**
No public API references `aSlots` etc. today. Scan the codebase
before Stage 3 to be sure.

## Open questions (for next round)

1. Should `FB_Log4TcTracer` be **per-task** (one instance per
   physical task) or **per-logical-entity** (one per controller FB,
   shared across tasks)? The latter is the interesting case and
   what this doc assumes, but it requires the cross-task-span
   support (automatic via Stage 1's `span_id` keying).
2. Does `FB_Span` need an explicit relationship link ("this span
   links to that other span without being its child") à la OTel
   span links? Useful for batch processing where one operation
   covers many input spans. Defer unless asked.
3. Do we expose `FB_Log4TcTracer.MintTraceId` / `MintSpanId`
   publicly? A user who wants to derive deterministic IDs from
   domain data (Pattern D) benefits — they can inject their own
   hash into `FB_Span.BeginWithIds(...)`. Strong use case, low
   surface. Proposed: yes, add an overload
   `FB_Span.Begin(sName, eKind, trace_id_override, span_id_override)`.

## Non-goals for Phase 6

* **Sampling.** `flag_sampled` bit stays reserved. Phase 7.
* **Span links.** Deferred.
* **Grafana service-map panel.** Phase 7 UX work.
* **tc-otel-side clock correction across PLCs.** Deferred — real
  deployments need this only at scale.

## Acceptance criteria

* Two `FB_Log4TcTracer` instances on one task produce independent
  trace trees, verified live in Tempo.
* Cross-task span works: a tracer called from task A's cycle and
  task B's cycle produces one span record in Tempo with the
  correct timeline.
* The Phase-5 live tester (three scenarios) still passes unchanged.
* `ST_SpanSlot.TcDUT` deleted; workspace-wide grep for the type
  returns zero results.
