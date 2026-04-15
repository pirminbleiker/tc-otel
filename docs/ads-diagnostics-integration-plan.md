# ADS-over-MQTT Diagnostic Traffic — Analysis & Integration Plan

Status: draft, 2026-04-15
Scope: extract TwinCAT runtime diagnostics (task cycle stats, RT CPU usage, system latency, exceed counter) from the existing ADS-over-MQTT stream and expose them as OpenTelemetry metrics via tc-otel.

## 1. Premise

Capture work (see `captures/`) established:
- Engineering ↔ PLC uses **polling only** (ADS cmd 2/3/9). No notifications.
- Three diagnostic windows map cleanly to distinct AMS `(port, IG, IO)` triples.
- Payloads are fixed-size binary blocks — deterministic decode path.

We can therefore add a **passive observer** inside tc-otel: sniff the same AMS frames the IDE already pulls, decode the known triples, emit metrics. No new polling needed as long as an IDE session is active; optionally tc-otel can poll itself when no IDE is connected.

## 2. Analysis Phase (deepen what we have)

### 2.1 Structural decode of each payload

Target one capture per payload type, 60 s, multiple PLC load profiles (idle / mid / exceed).

| Payload | Size | Decode goal |
|---|---|---|
| `F200:0x100` | 4 B | Confirm u32 LE, monotonic, resets on write |
| `F200:0x000` (R0 default port) | 16 B | Split header vs per-call state; find timestamp field |
| `F200:0x000` **@port 350** | 16 B | Identify per-task sub-structure; correlate bytes vs CycleTime/ExecTime/CpuTicks |
| `IG=0x01 IO=0x0F` @port 200 | 1536 B | Map to (CPU-count × per-core record) + latency histogram bins |
| `IG=0x01 IO=0x0D` @port 200 | 24 B | RT config header (CPU mask, tick, etc.) |
| `IG=0x01 IO=0x01` @port 200 | 40 B | Device-info block (already partly known from AdsReadDeviceInfo) |
| `ReadWrite IG=0xC8 IO=0x0` @port 200 | 48 B write | RT-Usage session setup (filter/params) |
| `IG=0x01010004 IO=0x0C` @port 30 | 4 B | Task descriptor (`0x01010004` looks like PLC task offset; confirm via bit-fields) |
| `Write IG=0x01 IO=0x01` 968 B | batch | Periodic upload from PLC → IDE — likely event log / system message batch |

### 2.2 Correlation experiments

Run capture **while varying one input at a time**, then diff:
1. **Task cycle-time change** in XAE → look for byte shift in `F200:0` (port 350) 16-B response.
2. **Force an exceed** (busy-wait in PLC task) → confirm `F200:0x100` increments and which byte in `F200:0` stream flags it.
3. **Change CPU core count** in RT setup → confirm 1536 B grows/shrinks or is padded.
4. **Toggle logger verbosity** → confirm 968 B batch follows that.
5. **Multiple tasks** (add PlcTask2) → verify `0x01010004` IG becomes `0x01010008` or another offset delta.

Each experiment: one capture before/after, `diff_reqs.py` + byte-diff helper (to be added).

### 2.3 Tooling to add

- `captures/diff_bytes.py` — response-payload aligned diff for a given (IG, IO, rsz) key; highlight changing bytes across time.
- `captures/session.sh` — interactive: prompts for event labels during capture, annotates log with markers.
- `captures/replay.py` — replay a capture against a dev broker for repeatable parser tests.
- Extend `decode_ams.py`:
  - Add known IG table with named ports (200 / 300 / 350 / 30).
  - Emit JSON-lines output for pipe-to-analysis.

## 3. Metric Mapping

OTel naming follows `tc.ads.<subsystem>.<metric>`. All counters, gauges, histograms tagged with:
- `ams_net_id` (dest net ID — PLC identity)
- `ads_port` (200 / 300 / 350 / ...)
- `task_id` (when applicable)

### 3.1 Realtime system (`port=200`, IG=0x01)

| Source bytes | Metric | Type | Unit |
|---|---|---|---|
| per-core %load (in 1536 B block) | `tc.rt.cpu.usage` | gauge | 1 (0–1) |
| latency histogram bins | `tc.rt.system_latency` | histogram | µs |
| core count | `tc.rt.cpu.cores` | gauge | 1 |

### 3.2 Task stats (`port=350`, IG=0xF200 IO=0)

| Source bytes | Metric | Type | Unit |
|---|---|---|---|
| current cycle | `tc.task.cycle_time` | gauge | µs |
| last exec | `tc.task.exec_time` | gauge | µs |
| exec total | `tc.task.cpu_time_total` | counter | µs |
| exceeds | `tc.task.cycle_exceed` | counter | 1 |

### 3.3 Exceed counter (`IG=0xF200 IO=0x100`)

| Metric | Type | Unit | Note |
|---|---|---|---|
| `tc.rt.exceed_counter` | counter | 1 | u32 LE; treat decrement (after reset) as counter restart |

### 3.4 System / device (`IG=0x01 IO=0x01/0x0D/0x06`, `IG=0x00F0`)

| Metric | Type | Note |
|---|---|---|
| `tc.device.state` | gauge | maps ADS state (Run / Config / Stop / Error) |
| `tc.device.info` (build/version) | resource attr | emitted once per connect, not a metric |

### 3.5 Logger batch upload (`Write IG=0x01 IO=0x01`, 968 B)

Defer until 2.1 decodes structure. If it is a log batch, emit as **OTel log signal**, not metrics.

## 4. Integration Plan (tc-otel)

### 4.1 Architecture fit

tc-otel today terminates AMS/TCP (port 48898) and republishes as MQTT (`AdsOverMqtt/...`). It already sees every frame. Adding a **frame observer** before republish is zero-overhead.

```
 PLC ──AMS/TCP──▶ tc-otel (router) ──MQTT──▶ IDE
                        │
                        ▼
                 frame observer ──▶ diagnostic decoder ──▶ OTel exporter
```

Key constraint (from memory: OS-independence, Docker-first): stay in Rust, no OS-specific hooks. Fits the existing crate split.

### 4.2 Phases

**P1 — Read-only observer (1–2 days)**
- [ ] Add `tc-otel-ads/src/diagnostics/mod.rs` — pure decode module, pub fn `try_decode(frame: &AmsFrame) -> Option<DiagEvent>`.
- [ ] Unit tests with captured payloads (`captures/*.log` as fixtures).
- [ ] Wire observer into `router.rs` dispatch (call on every response frame; ignore requests).
- [ ] Emit `DiagEvent` to existing metrics channel (check `tc-otel-core` for the batching primitive).

**P2 — Map to OTel metrics (1 day)**
- [ ] Add meters to `tc-otel-otlp` (or wherever OTel SDK lives). Reuse existing exporter.
- [ ] Attribute enrichment: resolve `ams_net_id` → display name via cached `/info` topic payloads.
- [ ] Cardinality audit: enforce allow-list of metrics; drop unknown IGs rather than tag-bloom.

**P3 — Self-poll fallback (optional, 1 day)**
- [ ] When no IDE session active for N seconds and config `diagnostics.self_poll=true`, tc-otel issues the same polls at configurable rate (min 1 Hz, max 10 Hz).
- [ ] Use separate AMS source port so replies are distinguishable from IDE.
- [ ] Rate-limit to avoid doubling PLC load when both IDE + tc-otel poll.

**P4 — Log-batch decoder (deferred)**
- [ ] Decode the 968 B write → structured log events via OTel log signal.
- [ ] Requires reverse-engineering batch format first (see 2.1).

### 4.3 Config surface

```jsonc
{
  "diagnostics": {
    "enabled": true,
    "observe_ide_traffic": true,      // passive decode
    "self_poll": false,               // P3
    "self_poll_interval_ms": 1000,
    "metrics": {
      "rt_usage": true,
      "task_stats": true,
      "exceed_counter": true
    }
  }
}
```

### 4.4 Testing

- **Unit**: parser against fixture payloads (hex strings from our captures).
- **Integration**: `docker-compose.test.yml` stack → run PLC project with known cycle times → assert metric values within tolerance at OTel collector output.
- **Regression**: before/after capture equivalence — tc-otel must not change bytes on passive observer path.

### 4.5 Risks

| Risk | Mitigation |
|---|---|
| Payload layout differs per TwinCAT version | Version-tag decode tables; emit `decode_miss` counter; log unknown structure sizes |
| IDE and tc-otel polling double PLC load | Passive-only by default; P3 gated by config |
| IG / port reuse across workflows | Allow-list rather than best-guess; unknown triples bypass diagnostic emit |
| Logger batch (968 B) is proprietary | Isolate to P4; ship without it |

## 5. Open Questions

1. Port 350 and port 30 need confirmation against Beckhoff constants — not in the standard `AMSPORT_*` list we normally see. **Partially resolved (see §7):** ports 340/350/351 are per-task runtime endpoints; port 852 is TC3 PLC runtime.
2. Is the 1536 B RT block a fixed CPU-count layout or variable? Needed before we can decode cleanly.
3. The 968 B batch direction: PLC → Engineering (via Write to engineering net ID). Confirms it's **not** a log the IDE sends — it's PLC-originated log/event batch, which is valuable for tc-otel. **Update:** target port is 16150 — tc-otel's own logger port. We already receive it.
4. Does each open window create a dedicated **invoke-id stream** or share the session? Affects whether we can demux by `invoke_id` range.
5. The 0x2A ReadWrite on port 852 is RPC-style, not SumUp. What method/object does it invoke? Response is nearly identical across calls — static handle query or diagnostic ping?

## 7. Confirmed Findings (60 s PLC-running capture)

### 7.1 Per-task port mapping

Each task owns its own AMS port. The IDE polls `IG=0xF200 IO=0 rsz=16` at **4.85 Hz per port**:

| Task (screenshot) | Port | Type-byte (payload[2]) |
|---|---|---|
| PlcTask   | **350** | `0x71` |
| PlcTask1  | **351** | `0x71` |
| I/O Idle  | **340** | `0x0b` |

**Implication:** the AMS port is a clean `task_id` dimension for metrics. No symbol lookup needed; tc-otel observer just keys on dst-port from the frame header.

### 7.2 16-byte task-stats payload (VERIFIED)

Verified against known cycle times (PlcTask 1 ms, PlcTask1 10 ms, I/O Idle 1 ms):

```
+0x00  u16  task cycle counter (wraps 65536; rate = 1000 / base_time_ms)
+0x02  u16  task type marker (0x71 PlcTask, 0x0b I/O Idle — constant)
+0x04  u32  CPU time accumulator, 100 ns units
+0x08  u32  Exec/Total time accumulator, 100 ns units
+0x0c  u32  reserved (always 0)
```

Measured vs expected:

| Task | Base | Expected ctr-rate | Measured | Δ |
|---|---|---|---|---|
| I/O Idle (port 340) | 1 ms | 1000/s | 999.8/s | <0.1 % |
| PlcTask (port 350) | 1 ms | 1000/s | 1003.3/s | <0.5 % |
| PlcTask1 (port 351) | 10 ms | 100/s | 100.0/s | exact |

Derived metrics per poll-window:

- **Average cycle time** (µs) = `Δ(+0x04) × 0.1 / Δ(+0x00 counter)`
- **CPU %** = `Δ(+0x04) × 100 ns / wallclock_Δ × 100`
- **Exec/Total %** = `Δ(+0x08) × 100 ns / wallclock_Δ × 100`
- **Cycle count in window** = Δ(+0x00 counter), unwrapped modulo 65536

### 7.3 RT-Usage + System Latency payload (24 B, not 1536)

Request `Read IG=0x01 IO=0x0F rsz=1536` on **port 200** (`AMSPORT_R0_REALTIME`) — IDE over-allocates; actual response is **24 bytes**. Rate ~2.5 Hz with window open.

| Offset | Field | Unit | Screenshot match (CPU 4 %, Latency 272 µs) |
|---|---|---|---|
| +0x00 | reserved (0) | — | — |
| +0x04 | peak/transient latency tracker | µs (?) | peaks 0–191 |
| +0x08 | **System Latency** | µs | avg 258 µs, matches 272 µs shown |
| +0x0c | reserved (0) | — | — |
| +0x10 | **CPU Usage** | % | avg 5.7 %, matches 4 % shown |
| +0x14 | scale / max (100) | % | constant 100 |

Both plots in the Real-Time window come from this single 24-byte block. Ready for metric emit.

### 7.4 PLC runtime RPC channel

- Port **852** (AMSPORT_R0_PLC_TC3)
- `ReadWrite IG=0x2A IO=0x2A wsz=44 rsz=65536`, 4.5 Hz
- 44-byte write body is **identical every call** (no per-call context) → static method handle invocation
- 52-byte response has quasi-static header, first 32 bytes identical across samples

Not a SumUpRead. Looks like a **TcRpc / ObjectServer-style** method call to a fixed handle. Defer decoding to after P1 (not critical for first metric set).

### 7.4 Logger batch target

The 968 B periodic Write `IG=1 IO=1` flows to **port 16150** — tc-otel's configured `ads_port`. tc-otel already receives it as part of its existing log ingest path. Good news: no new transport work for log-signal integration (§3.5 / P4).

### 7.5 Traffic mix (60 s, 3 windows open, PLC running)

| Class | Rate |
|---|---|
| Task stats polls (3 tasks × 4.85 Hz) | 14.6 Hz |
| TC3 PLC RPC | 4.5 Hz |
| Logger batch (968 B) | 2 Hz |
| Route/session keepalive | ~4 Hz |
| Misc state polls | ~5 Hz |
| **Total IDE → PLC requests** | **~30 req/s** |

Steady-state load is ~30 req/s per connected engineering client per PLC. Two open IDE sessions double this. Design consideration for P3 self-poll: stay well below this.

## 8. Revised Immediate Next Steps

1. **Correlation experiment for task-stats**: set PlcTask cycle time to a known value (e.g. 1 ms) and re-capture 30 s. Match +0x00 / +0x04 / +0x08 fields against known cycle + timestamp. Produces final decoder for §3.2 metrics.
2. **RT-Usage while running**: 60 s capture with RT-Usage window open (PLC in Run) → 1536 B block layout.
3. **Exceed correlation**: deliberate cycle-overrun (busy-wait in PLC) → confirm which field in 16 B block flags it.
4. Add **port → task name** mapping via `AdsReadDeviceInfo` on each detected task port (tc-otel can probe once per connect to build the label map).
5. Then draft `tc-otel-ads/src/diagnostics/mod.rs` skeleton.

## 6. Immediate Next Steps

1. Implement `diff_bytes.py`, re-capture 60 s PlcTask (PLC in Run, known cycle time), aligned-decode the 16 B block.
2. Re-capture RT-Usage with known CPU-core count, align 1536 B → per-core record size.
3. Force exceed → confirm counter semantics.
4. Draft `diagnostics/mod.rs` skeleton with stub decoders + fixture tests.

Cross-references: `captures/decode_ams.py`, `captures/diff_reqs.py`, `captures/baseline_idle.log`, `captures/exceed_counter.log`, `captures/rt_usage.log`, `captures/plctask.log`.
