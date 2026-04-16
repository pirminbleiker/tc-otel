# FB_PushDiag - Diagnostics Push Function Block

## Overview

`FB_PushDiag` is a TwinCAT 3 Function Block that pushes real-time PLC diagnostics to **tc-otel** (TwinCAT OpenTelemetry gateway) via ADS. It monitors all PLC tasks, detects cycle-time violations and real-time violations, and sends:

1. **Snapshot events** (IO=0) — periodic batched task state (version 1, little-endian)
2. **Edge events** (IO=1 or 2) — immediate per-task alerts when violations occur

Data is marshalled to binary format (IG=16#4D424301) and sent via `ADSWRITE` to tc-otel's ADS endpoint.

## Files

- **FB_PushDiag.TcPOU** — Main function block
- **ST_PushDiagHeader.TcDUT** — Snapshot packet header (16 bytes)
- **ST_PushDiagTaskRecord.TcDUT** — Per-task record in snapshot (72 bytes)
- **ST_PushDiagEdge.TcDUT** — Edge event packet (44 bytes)

## Wire Format

All data is **little-endian** (x86). Use `{attribute 'pack_mode' := '1'}` to ensure tight packing (no alignment padding).

### Snapshot Packet (IO=0)
- Header: 16 bytes
- Per-task records: 72 bytes each (count via `num_tasks`)
- Total: 16 + 72×N bytes

### Edge Event Packet (IO=1 cycle-exceed, IO=2 rt-violation)
- 44 bytes fixed size
- Sent immediately on violation edge detection

## How to Import into TwinCAT 3 Project

1. In Visual Studio, open your TwinCAT 3 PLC project.
2. Right-click **POUs** (or target folder) → **Import**.
3. Select `FB_PushDiag.TcPOU` and confirm import.
4. Right-click **Data Types** → **Import**.
5. Select all three `.TcDUT` files and confirm.
6. Verify no compilation errors (checkAllObjects).

## Sample Usage

Add this to your PLC `MAIN` or equivalent:

```structured-text
VAR
    fbPushDiag : FB_PushDiag;
    stTcOtelNetId : T_AmsNetId := (
        ipaddr := [192, 168, 1, 100],
        netid := 1,
        port := 1
    );
END_VAR

// Each cycle:
fbPushDiag(
    sTcOtelNetId := stTcOtelNetId,
    nTcOtelPort := 16150,       // tc-otel ADS port (adjust as needed)
    nSnapshotIntervalMs := 1000, // Send snapshot every 1000 ms
    bEnabled := TRUE
);
```

## Configuration Integration

Once `FB_PushDiag` is active and tc-otel receives edge events (IO=1,2), you can **disable or remove** the **poll-based diagnostics** from tc-otel's config:

```yaml
diagnostics:
  targets:
    - netid: "192.168.1.1.1.1"
      poll_interval_ms: 0  # Disable polling (or remove this target entirely)
      # tc-otel auto-detects push mode and stops polling once edge events arrive
```

Polling adds ADS load. Push-mode diagnostics are much more efficient.

## Notes

- **Source intent:** These files are intended as source for integration into the **mbc_log4tc** library package.
- **Task info access:** The FB uses `VAR_EXTERNAL` to access `TwinCAT_SystemInfoVarList.PlcAppSystemInfo.TaskInfo[]` array (standard TwinCAT runtime exposure).
- **Non-blocking ADS:** `ADSWRITE` calls are non-blocking. The FB handles `BUSY` and `ERR` states gracefully without blocking the cycle.
- **Edge detection:** Rising-edge detection of `CycleTimeExceeded` and `RTViolation` flags ensures one edge event per violation, preventing duplicate storms.

## Dependencies

- TwinCAT 3.1 PLC Runtime
- `Tc2_System` library (for `ADSWRITE`, system time)
- tc-otel v1+ (ADS receiver on port 16150 by default)

## License & Integration

Intended for inclusion in **mbc_log4tc** library distribution. Follow mbc_log4tc licensing terms.
