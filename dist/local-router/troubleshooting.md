# Troubleshooting

## tc-otel log files

`run-tc-otel.bat` redirects to:

- `C:\tc-otel\tcotel-stdout.log`
- `C:\tc-otel\tcotel-stderr.log` *(this is where tracing output lives)*

`run-victorialogs.bat` redirects to:

- `C:\victoria-logs\vl-stdout.log`
- `C:\victoria-logs\vl-stderr.log` *(this is where VL's own log lives)*

## tc-otel doesn't connect to the router

Look for this line in `tcotel-stderr.log`:

```
INFO tc_otel_ads::transport::local_router: registered with local AMS router: netId=... port=16150
```

If you see only:

```
WARN tc_otel_ads::transport::local_router: local-router connect failed: ... — retrying in 5s
```

Verify the TwinCAT router is actually up:

```powershell
Get-Service -Name "TcSystemServiceUm" -ErrorAction SilentlyContinue
Get-NetTCPConnection -LocalPort 48898 -State Listen
```

If 48898 isn't listening, TwinCAT itself is not running.

## "PortConnect: short response" or `cmd 0x...`

Either you're talking to the wrong router (something else owns 48898)
or the router rejected the registration. Run the standalone probe:

```powershell
py probe_port_connect.py 5
```

It sends just the `PortConnect` and prints the raw reply. A healthy
target prints something like:

```
[L] Registered: netId=172.28.41.37.1.1 port=16150
[S] Registered: netId=172.28.41.37.1.1 port=45453
[L] >> FRAME: target=172.28.41.37.1.1:16150 src=172.28.41.37.1.1:45453 cmd=0x0003 err=0x0
```

## PLC sends to the wrong NetId

After this deployment, the PLC must call `PRG_TaskLog.Init('')` (empty
string = local). If it was previously calling `Init('172.28.41.37.1.2')`
(or any other remote NetId) the writes go to the old NetId, never reach
the local router's port 16150, and tc-otel sits idle.

Quick check from the PLC side: the in-PLC `TcOtelInfo.aTaskInfo[i].bConnected`
flag is `TRUE` once writes succeed. If it's stuck `FALSE` and
`nSendLogBufferErrorCount` keeps climbing, the PLC's `sAmsNetId` is wrong.

## VictoriaLogs not reachable from Grafana

```powershell
# On target — port open and listening?
Get-NetTCPConnection -LocalPort 9428 -State Listen

# Firewall rule present?
netsh advfirewall firewall show rule name="VictoriaLogs 9428"
```

If the rule is missing, re-run `install.ps1` (it's idempotent) or:

```powershell
netsh advfirewall firewall add rule name="VictoriaLogs 9428" `
    dir=in action=allow protocol=TCP localport=9428
```

Then try from another box on the same LAN:

```powershell
Test-NetConnection -ComputerName <target> -Port 9428
```

## `Batch export error: HTTP error: error sending request for url ...`

tc-otel can't POST to its `export.endpoint`. With this dist that
endpoint is `http://127.0.0.1:9428/insert/jsonline`, so it means the
local VictoriaLogs task isn't running:

```powershell
schtasks /Query /TN "VictoriaLogs" /V /FO LIST | Select-String "Status|Last Run"
schtasks /Run   /TN "VictoriaLogs"
```

## Channel full / dropped frames

If `tcotel-stderr.log` shows lines like

```
DEBUG tc_otel_ads::router: trace-event channel full, dropping from ...
```

the PLC is producing faster than the dispatcher drains. Bump
`service.channel_capacity` in `config.json` (default in this dist:
`50000`; reasonable for high-volume PLCs: `200000`–`500000`), then
restart the task:

```powershell
schtasks /End /TN "tc-otel"; schtasks /Run /TN "tc-otel"
```

## Logs persistence + retention

- VL stores at `C:\victoria-logs\data`. Default retention 7d.
- Override via `-retentionPeriod=` flag in `C:\victoria-logs\run.bat`.
- Disk-full → VL drops oldest data automatically; check
  `vl-stderr.log` for warnings.
