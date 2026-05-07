# Removes Scheduled Tasks for tc-otel + Victoria backends.
# Default keeps binaries and data. Use -Purge to delete the on-disk dirs.

[CmdletBinding()]
param(
    [string]$TcOtelDir = "C:\tc-otel",
    [string]$VlDir     = "C:\victoria-logs",
    [string]$VmDir     = "C:\victoria-metrics",
    [string]$VtDir     = "C:\victoria-traces",
    [int]   $VlPort    = 9428,
    [int]   $VmPort    = 8428,
    [int]   $VtPort    = 10428,
    [switch]$Purge
)

if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Run this script in an Administrator PowerShell."
}

function Write-Step($msg) { Write-Host ">>> $msg" -ForegroundColor Cyan }

Write-Step "Stopping processes"
Get-Process -Name tc-otel,victoria-logs,victoria-metrics,victoria-traces -ErrorAction SilentlyContinue |
    Stop-Process -Force -ErrorAction SilentlyContinue

Write-Step "Removing Scheduled Tasks"
foreach ($t in @("tc-otel","VictoriaLogs","VictoriaMetrics","VictoriaTraces")) {
    & schtasks /Delete /TN $t /F 2>&1 | Out-Null
}

Write-Step "Closing firewall rules"
foreach ($r in @("VictoriaLogs $VlPort","VictoriaMetrics $VmPort","VictoriaTraces $VtPort")) {
    & netsh advfirewall firewall delete rule name=$r 2>&1 | Out-Null
}

if ($Purge) {
    Write-Step "Purging $TcOtelDir, $VlDir, $VmDir, $VtDir (data + logs)"
    foreach ($d in @($TcOtelDir, $VlDir, $VmDir, $VtDir)) {
        Remove-Item -Recurse -Force $d -ErrorAction SilentlyContinue
    }
} else {
    Write-Host "Binaries kept under $TcOtelDir, $VlDir, $VmDir, $VtDir. Use -Purge to delete." -ForegroundColor Yellow
}
Write-Host "Done." -ForegroundColor Green
