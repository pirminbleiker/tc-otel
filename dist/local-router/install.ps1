# Installs tc-otel + the full Victoria stack (Logs/Metrics/Traces) on a
# TwinCAT IPC. Each component runs as a Scheduled Task (RU SYSTEM,
# RL HIGHEST, ONSTART) so it survives SSH disconnect and reboots.
#
# Usage:
#   .\install.ps1                            # full install with downloads
#   .\install.ps1 -SkipFirewall              # don't open inbound TCP rules
#   .\install.ps1 -VlExe   C:\tmp\vl.exe     # use pre-downloaded binary
#   .\install.ps1 -VmExe   C:\tmp\vm.exe
#   .\install.ps1 -VtExe   C:\tmp\vt.exe
#   .\install.ps1 -SkipMetrics -SkipTraces   # logs only

[CmdletBinding()]
param(
    [string]$TcOtelDir   = "C:\tc-otel",
    [string]$VlDir       = "C:\victoria-logs",
    [string]$VmDir       = "C:\victoria-metrics",
    [string]$VtDir       = "C:\victoria-traces",

    [string]$VlVersion   = "v1.50.0",
    [string]$VmVersion   = "v1.142.0",
    [string]$VtVersion   = "v0.8.2",

    [string]$VlExe       = "",
    [string]$VmExe       = "",
    [string]$VtExe       = "",

    [int]   $VlPort      = 9428,
    [int]   $VmPort      = 8428,
    [int]   $VtPort      = 10428,

    [switch]$SkipFirewall,
    [switch]$SkipMetrics,
    [switch]$SkipTraces
)

$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path

if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Run this script in an Administrator PowerShell."
}

function Write-Step($msg) { Write-Host ">>> $msg" -ForegroundColor Cyan }

# Helper: download + extract a Victoria release zip → $TargetExe
function Get-VictoriaBinary {
    param(
        [string]$Repo,        # "VictoriaMetrics/VictoriaLogs" etc.
        [string]$Version,     # "v1.50.0"
        [string]$AssetPrefix, # "victoria-logs-windows-amd64"
        [string]$LocalCopy,   # optional pre-downloaded exe
        [string]$TargetExe    # final exe path
    )
    if ($LocalCopy -and (Test-Path $LocalCopy)) {
        Write-Step "Using pre-downloaded $LocalCopy"
        Copy-Item $LocalCopy $TargetExe -Force
        return
    }
    $packaged = Join-Path $here ([System.IO.Path]::GetFileName($TargetExe))
    if (Test-Path $packaged) {
        Write-Step "Using packaged $packaged"
        Copy-Item $packaged $TargetExe -Force
        return
    }
    $zipName = "$AssetPrefix-$Version.zip"
    $url     = "https://github.com/$Repo/releases/download/$Version/$zipName"
    Write-Step "Downloading $url"
    $tmp = "$env:TEMP\$zipName"
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest -Uri $url -OutFile $tmp -UseBasicParsing
    $extractDir = "$env:TEMP\vsetup-$([guid]::NewGuid())"
    Expand-Archive -LiteralPath $tmp -DestinationPath $extractDir -Force
    $exe = Get-ChildItem $extractDir -Filter "*.exe" -Recurse | Select-Object -First 1
    if (-not $exe) { throw "No .exe in $zipName" }
    Copy-Item $exe.FullName $TargetExe -Force
    Remove-Item $tmp, $extractDir -Recurse -Force -ErrorAction SilentlyContinue
}

# --- 1. Stop any prior install ------------------------------------------------
Write-Step "Stopping prior processes / Scheduled Tasks"
foreach ($p in @("tc-otel","victoria-logs","victoria-metrics","victoria-traces")) {
    Get-Process -Name $p -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
}
foreach ($t in @("tc-otel","VictoriaLogs","VictoriaMetrics","VictoriaTraces")) {
    & schtasks /Delete /TN $t /F 2>&1 | Out-Null
}

# --- 2. Layout ---------------------------------------------------------------
Write-Step "Creating directories"
foreach ($d in @($TcOtelDir, $VlDir, "$VlDir\data")) {
    New-Item -ItemType Directory -Force -Path $d | Out-Null
}
if (-not $SkipMetrics) {
    foreach ($d in @($VmDir, "$VmDir\data")) { New-Item -ItemType Directory -Force -Path $d | Out-Null }
}
if (-not $SkipTraces) {
    foreach ($d in @($VtDir, "$VtDir\data")) { New-Item -ItemType Directory -Force -Path $d | Out-Null }
}

# --- 3. tc-otel ---------------------------------------------------------------
Write-Step "Copying tc-otel"
Copy-Item "$here\tc-otel.exe"      "$TcOtelDir\tc-otel.exe" -Force
Copy-Item "$here\config.json"      "$TcOtelDir\config.json" -Force
Copy-Item "$here\run-tc-otel.bat"  "$TcOtelDir\run.bat"     -Force

# --- 4. Victoria binaries -----------------------------------------------------
Write-Step "Installing VictoriaLogs"
Get-VictoriaBinary -Repo "VictoriaMetrics/VictoriaLogs" -Version $VlVersion `
    -AssetPrefix "victoria-logs-windows-amd64" -LocalCopy $VlExe `
    -TargetExe "$VlDir\victoria-logs.exe"
Copy-Item "$here\run-victorialogs.bat" "$VlDir\run.bat" -Force

if (-not $SkipMetrics) {
    Write-Step "Installing VictoriaMetrics"
    Get-VictoriaBinary -Repo "VictoriaMetrics/VictoriaMetrics" -Version $VmVersion `
        -AssetPrefix "victoria-metrics-windows-amd64" -LocalCopy $VmExe `
        -TargetExe "$VmDir\victoria-metrics.exe"
    Copy-Item "$here\run-victoriametrics.bat" "$VmDir\run.bat" -Force
}

if (-not $SkipTraces) {
    Write-Step "Installing VictoriaTraces"
    Get-VictoriaBinary -Repo "VictoriaMetrics/VictoriaTraces" -Version $VtVersion `
        -AssetPrefix "victoria-traces-windows-amd64" -LocalCopy $VtExe `
        -TargetExe "$VtDir\victoria-traces.exe"
    Copy-Item "$here\run-victoriatraces.bat" "$VtDir\run.bat" -Force
}

# --- 5. Scheduled tasks -------------------------------------------------------
# Use Register-ScheduledTask (not schtasks.exe) so ExecutionTimeLimit can be
# set to 0 = unlimited. schtasks /Create defaults to 72h and would kill the
# task after 3 days.
Write-Step "Registering Scheduled Tasks"
function Register-Task($name, $batPath) {
    $action    = New-ScheduledTaskAction    -Execute "cmd.exe" -Argument "/c `"$batPath`""
    $trigger   = New-ScheduledTaskTrigger   -AtStartup
    $principal = New-ScheduledTaskPrincipal -UserId "SYSTEM" -RunLevel Highest
    $settings  = New-ScheduledTaskSettingsSet `
        -ExecutionTimeLimit (New-TimeSpan -Seconds 0) `
        -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) `
        -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable
    Register-ScheduledTask -TaskName $name -Action $action -Trigger $trigger `
        -Principal $principal -Settings $settings -Force | Out-Null
}
Register-Task "VictoriaLogs"    "$VlDir\run.bat"
if (-not $SkipMetrics) { Register-Task "VictoriaMetrics" "$VmDir\run.bat" }
if (-not $SkipTraces)  { Register-Task "VictoriaTraces"  "$VtDir\run.bat" }
Register-Task "tc-otel" "$TcOtelDir\run.bat"

# --- 6. Firewall --------------------------------------------------------------
if (-not $SkipFirewall) {
    Write-Step "Opening Windows Firewall (inbound TCP)"
    function Open-Port($name, $port) {
        & netsh advfirewall firewall delete rule name=$name 2>&1 | Out-Null
        & netsh advfirewall firewall add rule name=$name `
            dir=in action=allow protocol=TCP localport=$port | Out-Null
    }
    Open-Port "VictoriaLogs $VlPort"    $VlPort
    if (-not $SkipMetrics) { Open-Port "VictoriaMetrics $VmPort" $VmPort }
    if (-not $SkipTraces)  { Open-Port "VictoriaTraces $VtPort"  $VtPort }
}

# --- 7. Start ----------------------------------------------------------------
Write-Step "Starting Victoria backends"
& schtasks /Run /TN "VictoriaLogs" | Out-Null
if (-not $SkipMetrics) { & schtasks /Run /TN "VictoriaMetrics" | Out-Null }
if (-not $SkipTraces)  { & schtasks /Run /TN "VictoriaTraces"  | Out-Null }
Start-Sleep -Seconds 4

Write-Step "Starting tc-otel"
& schtasks /Run /TN "tc-otel" | Out-Null
Start-Sleep -Seconds 5

# --- 8. Verify ---------------------------------------------------------------
Write-Step "Process check"
Get-Process -Name tc-otel,victoria-logs,victoria-metrics,victoria-traces -ErrorAction SilentlyContinue |
    Format-Table Id, ProcessName, WS -AutoSize

Write-Step "Listening sockets"
$wantPorts = @(8080, $VlPort)
if (-not $SkipMetrics) { $wantPorts += $VmPort }
if (-not $SkipTraces)  { $wantPorts += $VtPort }
Get-NetTCPConnection -State Listen -ErrorAction SilentlyContinue |
    Where-Object { $_.LocalPort -in $wantPorts } |
    Format-Table LocalAddress, LocalPort -AutoSize

Write-Step "tc-otel registration line"
$log = "$TcOtelDir\tcotel-stderr.log"
if (Test-Path $log) {
    Select-String -Path $log -Pattern "registered with local AMS router" |
        Select-Object -Last 1
}

Write-Host ""
$h = hostname
Write-Host "Done." -ForegroundColor Green
Write-Host "  Logs    UI:        http://${h}:$VlPort/select/vmui/"
if (-not $SkipMetrics) { Write-Host "  Metrics UI (VMUI):  http://${h}:$VmPort/vmui/" }
if (-not $SkipTraces)  { Write-Host "  Traces  Jaeger API: http://${h}:$VtPort/select/jaeger/api/services" }
Write-Host "  tc-otel Web UI:    http://127.0.0.1:8080"
