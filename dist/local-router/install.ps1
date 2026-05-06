# Installs tc-otel + VictoriaLogs on a TwinCAT target as Scheduled Tasks.
# Run from this directory in an Administrator PowerShell.
#
#   .\install.ps1                                # full install with download
#   .\install.ps1 -VlExe C:\tmp\victoria-logs.exe  # skip download, use given exe
#   .\install.ps1 -SkipFirewall                  # don't open TCP/9428 inbound

[CmdletBinding()]
param(
    [string]$TcOtelDir = "C:\tc-otel",
    [string]$VlDir     = "C:\victoria-logs",
    [string]$VlVersion = "v1.50.0",
    [string]$VlExe     = "",            # optional: pre-downloaded victoria-logs exe
    [int]   $VlPort    = 9428,
    [switch]$SkipFirewall
)

$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path

if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Run this script in an Administrator PowerShell."
}

function Write-Step($msg) { Write-Host ">>> $msg" -ForegroundColor Cyan }

# --- 1. Stop anything that might already be running ---------------------------
Write-Step "Stopping prior tc-otel / VictoriaLogs processes and tasks"
foreach ($p in @("tc-otel", "victoria-logs")) {
    Get-Process -Name $p -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
}
foreach ($t in @("tc-otel", "VictoriaLogs")) {
    & schtasks /Delete /TN $t /F 2>&1 | Out-Null
}

# --- 2. Lay out directories ---------------------------------------------------
Write-Step "Creating $TcOtelDir and $VlDir"
New-Item -ItemType Directory -Force -Path $TcOtelDir | Out-Null
New-Item -ItemType Directory -Force -Path $VlDir     | Out-Null
New-Item -ItemType Directory -Force -Path "$VlDir\data" | Out-Null

# --- 3. Place tc-otel ---------------------------------------------------------
Write-Step "Copying tc-otel.exe + config.json"
Copy-Item "$here\tc-otel.exe" "$TcOtelDir\tc-otel.exe" -Force
Copy-Item "$here\config.json" "$TcOtelDir\config.json" -Force
Copy-Item "$here\run-tc-otel.bat" "$TcOtelDir\run.bat" -Force

# --- 4. Place VictoriaLogs ----------------------------------------------------
$vlTarget = "$VlDir\victoria-logs.exe"
if ($VlExe -and (Test-Path $VlExe)) {
    Write-Step "Copying VictoriaLogs from $VlExe"
    Copy-Item $VlExe $vlTarget -Force
}
elseif (Test-Path "$here\victoria-logs.exe") {
    Write-Step "Copying VictoriaLogs from package"
    Copy-Item "$here\victoria-logs.exe" $vlTarget -Force
}
else {
    Write-Step "Downloading VictoriaLogs $VlVersion (Windows amd64)"
    $url = "https://github.com/VictoriaMetrics/VictoriaLogs/releases/download/$VlVersion/victoria-logs-windows-amd64-$VlVersion.zip"
    $tmpZip = "$env:TEMP\vl-$VlVersion.zip"
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest -Uri $url -OutFile $tmpZip -UseBasicParsing
    Expand-Archive -LiteralPath $tmpZip -DestinationPath "$env:TEMP\vl-extract" -Force
    $extracted = Get-ChildItem "$env:TEMP\vl-extract" -Filter "victoria-logs-*-prod.exe" |
                 Select-Object -First 1
    Copy-Item $extracted.FullName $vlTarget -Force
    Remove-Item $tmpZip, "$env:TEMP\vl-extract" -Recurse -Force -ErrorAction SilentlyContinue
}
Copy-Item "$here\run-victorialogs.bat" "$VlDir\run.bat" -Force

# --- 5. Scheduled tasks (run as SYSTEM, ONSTART trigger) ----------------------
Write-Step "Registering Scheduled Tasks (run as SYSTEM, trigger ONSTART)"
& schtasks /Create /TN "VictoriaLogs" `
    /TR "cmd /c $VlDir\run.bat" `
    /SC ONSTART /RU SYSTEM /RL HIGHEST /F | Out-Null
& schtasks /Create /TN "tc-otel" `
    /TR "cmd /c $TcOtelDir\run.bat" `
    /SC ONSTART /RU SYSTEM /RL HIGHEST /F | Out-Null

# --- 6. Firewall --------------------------------------------------------------
if (-not $SkipFirewall) {
    Write-Step "Opening Windows Firewall: TCP/$VlPort inbound"
    & netsh advfirewall firewall delete rule name="VictoriaLogs $VlPort" 2>&1 | Out-Null
    & netsh advfirewall firewall add rule name="VictoriaLogs $VlPort" `
        dir=in action=allow protocol=TCP localport=$VlPort | Out-Null
}

# --- 7. Start ----------------------------------------------------------------
Write-Step "Starting VictoriaLogs"
& schtasks /Run /TN "VictoriaLogs" | Out-Null
Start-Sleep -Seconds 3

Write-Step "Starting tc-otel"
& schtasks /Run /TN "tc-otel" | Out-Null
Start-Sleep -Seconds 5

# --- 8. Verify ---------------------------------------------------------------
Write-Step "Process check"
Get-Process -Name tc-otel,victoria-logs -ErrorAction SilentlyContinue |
    Format-Table Id, ProcessName, WS -AutoSize

Write-Step "Listening sockets"
$listening = Get-NetTCPConnection -State Listen -ErrorAction SilentlyContinue |
    Where-Object { $_.LocalPort -in @($VlPort, 8080) }
$listening | Format-Table LocalAddress, LocalPort -AutoSize

Write-Step "tc-otel registration line"
$log = "$TcOtelDir\tcotel-stderr.log"
if (Test-Path $log) {
    Select-String -Path $log -Pattern "registered with local AMS router" |
        Select-Object -Last 1
}

Write-Host ""
Write-Host "Done." -ForegroundColor Green
Write-Host "  VictoriaLogs UI:    http://$(hostname):$VlPort/select/vmui/"
Write-Host "  Grafana datasource: http://$(hostname):$VlPort"
Write-Host "  tc-otel Web UI:     http://127.0.0.1:8080"
