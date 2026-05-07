# tc-otel local-router deployment

Self-contained on-IPC deployment for tc-otel **on the same machine as
TwinCAT** with the full Victoria stack (logs + metrics + traces) right
next to it. After install you have:

| Pillar  | Backend         | Port  | UI                                              | Status |
| ------- | --------------- | ----- | ----------------------------------------------- | ------ |
| Logs    | VictoriaLogs    | 9428  | `http://<target>:9428/select/vmui/`             | ✓ working |
| Traces  | VictoriaTraces  | 10428 | Jaeger API `http://<target>:10428/select/jaeger/api/...` | ✓ working |
| Metrics | VictoriaMetrics | 8428  | `http://<target>:8428/vmui/`                    | ⚠ ingest disabled — VM only accepts OTLP-protobuf, tc-otel emits OTLP-JSON. See [`victoria-stack.md`](victoria-stack.md) for workarounds |

All four binaries (tc-otel + 3× Victoria) run as Windows Scheduled
Tasks under SYSTEM, auto-start on boot, and survive SSH disconnect.

## How tc-otel reaches TwinCAT (the local-router transport)

Instead of binding TCP/48898 (already owned by `TcAmsRouter`), tc-otel
acts as a **client** of the local router: outbound TCP to
`127.0.0.1:48898`, AMS/TCP `PortConnect` (cmd `0x1000`) registers AMS
port 16150 with the router, and ADS frames addressed to
`<localNetId>:16150` are delivered over the same socket.

No `StaticRoutes.xml` edit. No port conflict. No separate AMS NetId —
the router hands tc-otel the local NetId during `PortConnect`. PLC
code uses `PRG_TaskLog.Init('127.0.0.1.1.1')` and writes via the
standard `Tc2_System.ADSWRITE`. See [`architecture.md`](architecture.md)
for the full protocol picture and [`router-reference.md`](router-reference.md)
for the deep dive (commands, ports, performance).

## Files in this directory

> **Build prerequisite:** the binary `tc-otel.exe` is `.gitignore`d. Build it
> with `cargo build --release -p tc-otel-service` and copy the resulting
> `target/release/tc-otel.exe` into this directory before running `install.ps1`.

| File | Purpose |
|---|---|
| `tc-otel.exe` | tc-otel service binary (build artifact, see note above) |
| `config.json` | tc-otel config — transport `local_router`, export to local VictoriaLogs |
| `install.ps1` | One-shot installer for the target (admin) — sets up tc-otel + VictoriaLogs as Scheduled Tasks |
| `uninstall.ps1` | Removes scheduled tasks and files |
| `start.ps1` / `stop.ps1` | Manual task control |
| `run-tc-otel.bat` | Wrapper that the `tc-otel` Scheduled Task runs |
| `run-victorialogs.bat` | Wrapper that the `VictoriaLogs` Scheduled Task runs |
| `run-victoriametrics.bat` | Wrapper that the `VictoriaMetrics` Scheduled Task runs |
| `run-victoriatraces.bat` | Wrapper that the `VictoriaTraces` Scheduled Task runs |
| `probe_port_connect.py` | Diagnostic — verify the local router accepts PortConnect for port 16150 |
| `remote_deploy.py` | *Optional* — push this dist to a target over SSH and run `install.ps1` remotely |
| `victoria-stack.md` | Endpoints, query examples, retention for VL/VM/VT |
| `architecture.md` | Protocol details + why this approach |
| `router-reference.md` | Deep dive: full AMS/TCP command set, port table, dispatch flow, performance numbers |
| `troubleshooting.md` | Common issues and fixes |

## Install (target machine, run as Administrator)

```powershell
# Copy this entire folder to the target (e.g. C:\deploy\local-router)
# Open an admin PowerShell prompt:
cd C:\deploy\local-router
.\install.ps1
```

### Remote install over SSH (alternative)

If the target has OpenSSH server enabled and you'd rather push from a
dev box, install paramiko (`py -m pip install paramiko`) and:

```powershell
py remote_deploy.py --host 172.21.101.62 --user Administrator --password 1
```

That uploads this whole directory to `C:\deploy\local-router` on the
target and runs `install.ps1` with admin elevation.

The installer:

1. Creates `C:\tc-otel\` and `C:\victoria-logs\`
2. Copies `tc-otel.exe` + `config.json` to `C:\tc-otel\`
3. Downloads VictoriaLogs `v1.50.0` (Windows amd64, ~6 MB zip / ~16 MB exe) into `C:\victoria-logs\` *(skip with `-VlExe <path>` if you have it locally)*
4. Writes `run.bat` launchers with stdout/stderr redirection
5. Creates Scheduled Tasks **`tc-otel`** and **`VictoriaLogs`** (run as `SYSTEM`, `RL HIGHEST`, trigger `ONSTART`)
6. Opens Windows Firewall TCP/9428 inbound (so Grafana on another box can query VL)
7. Runs both tasks
8. Verifies `:9428` is listening and tc-otel registered with the router

After install, both services start automatically on every boot.

## What the PLC needs

In the TwinCAT project that uses the `TcOtel` library:

```pascal
IF _TaskInfo[GETCURTASKINDEXEX()].FirstCycle THEN
    // Empty NetId = route to the local AMS router (default).
    // tc-otel registers there as port 16150.
    PRG_TaskLog.Init('');
END_IF
```

If the PLC was previously configured with a remote NetId for tc-otel
(e.g. `'172.28.41.37.1.2'`), change it to `''` (empty) — tc-otel now
shares the local TwinCAT NetId.

## Verify it's running

```powershell
# Both processes alive?
tasklist | findstr "tc-otel victoria"

# tc-otel registered with the router?
Get-Content C:\tc-otel\tcotel-stderr.log -Tail 20
# Look for: "registered with local AMS router: netId=... port=16150"

# VictoriaLogs receiving data?
Invoke-RestMethod "http://127.0.0.1:9428/select/logsql/query" `
    -Method POST -Body "query=*&start=5m" |
    Select-Object -First 5
```

The probe script can sanity-check the AMS router protocol independently
(no tc-otel involvement):

```powershell
py probe_port_connect.py 5
# Connects, sends PortConnect, reports localNetId + assigned port,
# does a self-test write to verify routing works.
```

## URLs

| | URL |
|---|---|
| VictoriaLogs Web UI | `http://<target>:9428/select/vmui/` |
| Grafana data source | `http://<target>:9428` |
| tc-otel local Web UI | `http://127.0.0.1:8080` |

See `grafana-setup.md` for the data source plugin and example queries.

## Uninstall

```powershell
.\uninstall.ps1            # removes tasks + binaries, keeps logs/data
.\uninstall.ps1 -Purge     # also removes C:\tc-otel and C:\victoria-logs
```
