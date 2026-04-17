# Push-Diagnostics — Per-Task Oversampling Batches

## Overview

Each task that calls `PRG_TaskLog.Call()` gets a per-task push-diagnostic
collector (`FB_TcOtelTaskDiag`). Every cycle the collector captures one
`ST_PushDiagSample` (`cycle_count`, `exec_time_us`, `dc_time`, `flags`)
into a 1024-entry ring buffer. When the configured aggregation window
elapses the ring plus pre-computed aggregates (`min/max/avg exec_time`,
exceed/rtv counts) are flushed as a single batch frame via `ADSWRITE` to
tc-otel (`IG=16#4D424301`, `IO=0`).

Aggregation and oversampling together deliver cycle-exact information
to tc-otel without a separate FB call-site: the magic runs inside
`PRG_TaskLog.Call()` just like the logger.

## Files

- **FB_TcOtelTaskDiag.TcPOU** — per-task collector (ring + window flush + ADSWRITE)
- **ST_PushDiagBatchHeader.TcDUT** — 80-byte batch header
- **ST_PushDiagSample.TcDUT** — 24-byte per-cycle sample
- **ST_PushDiagConfig.TcDUT** — 8-byte per-task config slot (runtime-writable)

## Wire Format

All data little-endian (x86). `{attribute 'pack_mode' := '1'}` on every
struct guarantees the on-wire layout matches the Rust decoder in
`tc-otel-ads/src/diagnostics_push.rs`.

- **Batch** (`IG=0x4D424301`, `IO=0`): 80-byte header + N × 24-byte samples
- Maximum samples per batch: **1024** (ring depth)
- Worst-case frame size: **24,656 bytes** (well below the 8 MB ADS limit)
- Wire version: **2** (v1 = legacy snapshot/edge format, removed)
- Event type in header: **10** (batch)

### Per-Cycle Flags

| Bit | Constant                   | Meaning                                  |
| --- | -------------------------- | ---------------------------------------- |
| 0   | `SAMPLE_FLAG_CYCLE_EXCEED` | `CycleTimeExceeded` observed this cycle  |
| 1   | `SAMPLE_FLAG_RT_VIOLATION` | `RTViolation` observed this cycle        |
| 2   | `SAMPLE_FLAG_FIRST_CYCLE`  | First cycle after task start             |
| 3   | `SAMPLE_FLAG_OVERFLOW`     | Ring overflowed before this sample       |

Each sample carries `cycle_count` + `dc_time`, so tc-otel can correlate
a flagged cycle 1:1 with the log entry produced on the same cycle.

## Runtime-Writable Configuration

The per-task config slot lives at `PRG_TaskLog.aTaskDiagConfig[n]` and
is a plain `ST_PushDiagConfig` (`window_ms`, `enabled`, `divider`).
tc-otel reaches it via ADS SymbolByName writes — no custom PLC-side
handler is needed. The collector reads its config every cycle, so
changes take effect on the next flush.

## Usage

There's no separate FB to instantiate — the logger's init wires
everything up. Call `InitDiag` if you need a non-default window or
a tc-otel port other than 16150.

```structured-text
IF _TaskInfo[GETCURTASKINDEXEX()].FirstCycle THEN
    PRG_TaskLog.Init('127.0.0.1.1.1');
    // Optional: override defaults (window=100ms, enabled=TRUE, divider=1)
    PRG_TaskLog.InitDiag(
        sAmsNetId := '127.0.0.1.1.1',
        nPort     := 16150,
        nWindowMs := 250,
        bEnabled  := TRUE,
        nDivider  := 1
    );
END_IF

// Each cycle:
PRG_TaskLog.Call();
```

## Dependencies

- TwinCAT 3.1 PLC Runtime
- `Tc2_System` (for `ADSWRITE`, `MEMCPY`)
- `Tc2_Utilities` (for `GETCURTASKINDEXEX`)
- tc-otel v2+ (batch wire format)
