# Removes Scheduled Tasks for tc-otel and VictoriaLogs.
# By default keeps binaries and data. Use -Purge to delete everything.

[CmdletBinding()]
param(
    [string]$TcOtelDir = "C:\tc-otel",
    [string]$VlDir     = "C:\victoria-logs",
    [int]   $VlPort    = 9428,
    [switch]$Purge
)

if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Run this script in an Administrator PowerShell."
}

function Write-Step($msg) { Write-Host ">>> $msg" -ForegroundColor Cyan }

Write-Step "Stopping processes"
Get-Process -Name tc-otel,victoria-logs -ErrorAction SilentlyContinue |
    Stop-Process -Force -ErrorAction SilentlyContinue

Write-Step "Removing Scheduled Tasks"
foreach ($t in @("tc-otel", "VictoriaLogs")) {
    & schtasks /Delete /TN $t /F 2>&1 | Out-Null
}

Write-Step "Closing firewall rule TCP/$VlPort"
& netsh advfirewall firewall delete rule name="VictoriaLogs $VlPort" 2>&1 | Out-Null

if ($Purge) {
    Write-Step "Purging $TcOtelDir and $VlDir (data + logs)"
    Remove-Item -Recurse -Force $TcOtelDir -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $VlDir     -ErrorAction SilentlyContinue
} else {
    Write-Host "Binaries kept under $TcOtelDir and $VlDir. Use -Purge to delete." -ForegroundColor Yellow
}

Write-Host "Done." -ForegroundColor Green
