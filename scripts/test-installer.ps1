# Installer lifecycle test. CI ONLY: it installs crabfarm for the current user,
# writes HKCU (Run key, user PATH, Installed apps) and %LOCALAPPDATA%, starts and
# stops a node, and uninstalls again. It refuses to run outside GitHub Actions so
# it can never touch a real profile.
#   scripts/test-installer.ps1 -Setup <crabfarm-x.y.z-setup.exe> -Version <x.y.z>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Setup,
    [Parameter(Mandatory)][string]$Version
)
$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true') { throw 'test-installer.ps1 runs only on disposable CI runners.' }

$dir = Join-Path $env:LOCALAPPDATA 'Programs\crabfarm'
$cli = Join-Path $dir 'crabfarm.exe'
$daemon = Join-Path $dir 'crabfarmd.exe'
$key = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\crabfarm'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$data = Join-Path $env:APPDATA 'renner\crabfarm'

function Run([string]$File, [string[]]$Arguments, [int]$Seconds = 120) {
    $p = Start-Process -FilePath $File -ArgumentList $Arguments -PassThru
    if (!$p.WaitForExit($Seconds * 1000)) { $p.Kill(); throw "$File $Arguments timed out" }
    return $p.ExitCode
}

function Cli([string[]]$Arguments) {
    $out = Join-Path $env:RUNNER_TEMP 'crabfarm-out.txt'
    $p = Start-Process -FilePath $cli -ArgumentList $Arguments -RedirectStandardOutput $out -NoNewWindow -PassThru
    if (!$p.WaitForExit(60000)) { $p.Kill(); throw "crabfarm $Arguments timed out" }
    if ($p.ExitCode) { throw "crabfarm $Arguments failed ($($p.ExitCode))" }
    return (Get-Content -LiteralPath $out -Raw)
}

function Get-UserPath { (Get-ItemProperty -LiteralPath 'HKCU:\Environment').Path }
function Test-OnPath { ((Get-UserPath) -split ';') -contains $dir }
function Test-Autostart { $null -ne (Get-ItemProperty -LiteralPath $runKey -Name crabfarm -ErrorAction SilentlyContinue) }
function Test-NodeRunning {
    # The node's own lock: a second node on the directory refuses to start.
    $out = Cli @('service', 'status')
    return $out -match 'node: running'
}

function Assert-Installed([bool]$path, [bool]$autostart) {
    foreach ($f in 'crabfarm.exe', 'crabfarmd.exe', 'crabfarm.ico', 'LICENSE.txt', 'GETTING-STARTED.md', 'uninstall.exe') {
        if (!(Test-Path -LiteralPath (Join-Path $dir $f))) { throw "$f missing" }
    }
    $entry = Get-ItemProperty -LiteralPath $key
    if ($entry.DisplayVersion -ne $Version) { throw "DisplayVersion is '$($entry.DisplayVersion)', expected $Version" }
    if ($entry.Publisher -ne 'renner.dev') { throw "Publisher is '$($entry.Publisher)'" }
    if ((Test-OnPath) -ne $path) { throw "PATH entry expected=$path" }
    if ((Test-Autostart) -ne $autostart) { throw "Run key expected=$autostart" }
    if ($autostart) {
        $v = (Get-ItemProperty -LiteralPath $runKey).crabfarm
        if ($v -ne "`"$daemon`"") { throw "Run value is $v" }
    }
}

Write-Host '1. Silent install (both options on by default)'
if ((Run $Setup @('/S'))) { throw 'Setup failed' }
Assert-Installed $true $true
# A release build reports the Cargo.toml version, which equals the package
# version; a local test package may carry a suffix (-rc.1) in its name only.
$base = ($Version -split '-')[0]
$out = Cli @('--version')
if ($out.Trim() -notin @("crabfarm $Version", "crabfarm $base")) { throw "crabfarm --version printed '$($out.Trim())'" }

# No farm exists yet, so the node that autostart launched at install exited.
# `farm create` starts it (autostart is on and points at this install).
Cli @('farm', 'create', '--name', 'ci') | Out-Null
if (!(Test-Path -LiteralPath $data)) { throw "no data folder at $data after farm create" }
$deadline = (Get-Date).AddSeconds(60)
while (!(Test-NodeRunning) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 500 }
if (!(Test-NodeRunning)) { throw 'the node was not running after farm create' }

Write-Host '2. Silent reinstall over the running node (what an update does)'
if ((Run $Setup @('/S') 180)) { throw 'Reinstall failed' }
Assert-Installed $true $true
# PATH entry must not be duplicated.
if (((Get-UserPath) -split ';' | Where-Object { $_ -eq $dir }).Count -ne 1) { throw 'PATH entry duplicated by reinstall' }

Write-Host '3. Stop the node cleanly (farm data exists, so uninstall has something to keep)'
Cli @('node', '--stop') | Out-Null
if (Test-NodeRunning) { throw 'the node is still running after node --stop' }

Write-Host '4. Silent uninstall keeps the data, removes everything else'
if ((Run (Join-Path $dir 'uninstall.exe') @('/S'))) { throw 'Uninstall failed' }
$deadline = (Get-Date).AddSeconds(30)
while ((Test-Path -LiteralPath $cli) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 250 }
if (Test-Path -LiteralPath $cli) { throw 'crabfarm.exe still present after uninstall' }
if (Test-Path -LiteralPath $key) { throw 'Installed apps entry still present' }
if (Test-OnPath) { throw 'PATH entry still present' }
if (Test-Autostart) { throw 'Run key still present' }
if (!(Test-Path -LiteralPath $data)) { throw 'farm data was deleted without /DELETEDATA' }

Write-Host '5. Install without options, then uninstall with /DELETEDATA'
if ((Run $Setup @('/S', '/NOPATH', '/NOAUTOSTART'))) { throw 'Setup (no options) failed' }
Assert-Installed $false $false
if ((Run (Join-Path $dir 'uninstall.exe') @('/S', '/DELETEDATA'))) { throw 'Uninstall /DELETEDATA failed' }
$deadline = (Get-Date).AddSeconds(30)
while ((Test-Path -LiteralPath $data) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 250 }
if (Test-Path -LiteralPath $data) { throw 'farm data still present after /DELETEDATA' }
if (Test-Path -LiteralPath $key) { throw 'Installed apps entry still present' }
Write-Host 'Installer lifecycle passed.'
