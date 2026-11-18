# Installer lifecycle test. CI ONLY: it installs crabfarm for the current user,
# writes HKCU (Run key, user PATH, Installed apps) and %LOCALAPPDATA%, starts and
# stops nodes, and uninstalls again. It refuses to run anywhere but a GitHub-hosted
# runner (a disposable machine) and refuses to run when crabfarm is already
# installed there, so it can never touch a real profile.
#   scripts/test-installer.ps1 -Setup <crabfarm-x.y.z-setup.exe> -Version <x.y.z>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Setup,
    [Parameter(Mandatory)][string]$Version
)
$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true') { throw 'test-installer.ps1 runs only on disposable CI runners (GITHUB_ACTIONS=true).' }
if ($env:RUNNER_ENVIRONMENT -ne 'github-hosted') { throw 'test-installer.ps1 runs only on GitHub-hosted runners (RUNNER_ENVIRONMENT=github-hosted).' }

$dir = Join-Path $env:LOCALAPPDATA 'Programs\crabfarm'
$cli = Join-Path $dir 'crabfarm.exe'
$daemon = Join-Path $dir 'crabfarmd.exe'
$cargoSub = Join-Path $dir 'cargo-crabfarm.exe'
$key = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\crabfarm'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$data = Join-Path $env:APPDATA 'renner\crabfarm'
if ((Test-Path -LiteralPath $dir) -or (Test-Path -LiteralPath $key)) {
    throw "crabfarm is already installed here ($dir or the uninstall key exists); refusing to touch it."
}

function Invoke-BoundedProcess([string]$File, [string[]]$Arguments, [int]$Seconds = 120) {
    $p = Start-Process -FilePath $File -ArgumentList $Arguments -PassThru
    if (!$p.WaitForExit($Seconds * 1000)) { $p.Kill(); throw "$File $Arguments timed out" }
    return $p.ExitCode
}

function Invoke-CrabfarmCli([string[]]$Arguments) {
    $out = Join-Path $env:RUNNER_TEMP 'crabfarm-out.txt'
    $p = Start-Process -FilePath $cli -ArgumentList $Arguments -RedirectStandardOutput $out -NoNewWindow -PassThru
    if (!$p.WaitForExit(60000)) { $p.Kill(); throw "crabfarm $Arguments timed out" }
    if ($p.ExitCode) { throw "crabfarm $Arguments failed ($($p.ExitCode))" }
    return (Get-Content -LiteralPath $out -Raw)
}

# The user PATH exactly as stored: unexpanded text and registry type.
function Get-RawUserPath {
    $k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment')
    try {
        if ($k.GetValueNames() -notcontains 'Path') { return @($null, 'absent') }
        return @($k.GetValue('Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames), $k.GetValueKind('Path').ToString())
    } finally { $k.Dispose() }
}
function Get-UserPath { (Get-RawUserPath)[0] }
function Test-OnPath { ((Get-UserPath) -split ';') -contains $dir }
function Assert-PathUntouched($before) {
    $now = Get-RawUserPath
    if ($now[1] -ne $before[1]) { throw "user PATH registry type changed: '$($before[1])' -> '$($now[1])'" }
    if ($now[0] -cne $before[0]) {
        # A disposable runner: the values are shown so a mismatch can be fixed.
        throw "user PATH value differs from before the install`nbefore ($($before[1])): [$($before[0])]`nafter  ($($now[1])): [$($now[0])]"
    }
}
function Test-Autostart { $null -ne (Get-ItemProperty -LiteralPath $runKey -Name crabfarm -ErrorAction SilentlyContinue) }
function Test-NodeRunning {
    # The node's own lock: a second node on the directory refuses to start.
    $out = Invoke-CrabfarmCli @('service', 'status')
    return $out -match 'node: running'
}
function Wait-ForCondition([scriptblock]$Condition, [int]$Seconds, [string]$What) {
    $deadline = (Get-Date).AddSeconds($Seconds)
    while (!(& $Condition) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 250 }
    if (!(& $Condition)) { throw "timed out waiting for: $What" }
}

function Assert-Installed([bool]$path, [bool]$autostart) {
    foreach ($f in 'crabfarm.exe', 'crabfarmd.exe', 'cargo-crabfarm.exe', 'crabfarm.ico', 'LICENSE.txt', 'GETTING-STARTED.md', 'uninstall.exe') {
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

# The uninstaller copies itself to %TEMP% and relaunches, so its own exit code
# says nothing: wait for the outcome instead (the uninstall key goes last).
function Assert-Uninstalled {
    Wait-ForCondition { !(Test-Path -LiteralPath $key) } 90 'the uninstall key to disappear'
    Wait-ForCondition { !(Test-Path -LiteralPath $dir) } 30 'the install folder to disappear'
    foreach ($f in $cli, $daemon, $cargoSub) {
        if (Test-Path -LiteralPath $f) { throw "$f still present after uninstall" }
    }
    if (Test-OnPath) { throw 'PATH entry still present' }
    if (Test-Autostart) { throw 'Run key still present' }
}

$pathBefore = Get-RawUserPath

Write-Host '1. Silent install (both options on by default)'
if ((Invoke-BoundedProcess $Setup @('/S'))) { throw 'Setup failed' }
Assert-Installed $true $true
# A release build reports the Cargo.toml version, which equals the package
# version; a local test package may carry a suffix (-rc.1) in its name only.
$base = ($Version -split '-')[0]
$out = Invoke-CrabfarmCli @('--version')
if ($out.Trim() -notin @("crabfarm $Version", "crabfarm $base")) { throw "crabfarm --version printed '$($out.Trim())'" }
# `cargo crabfarm` reaches the same program: cargo-crabfarm drops the name cargo adds.
$p = Start-Process -FilePath $cargoSub -ArgumentList @('crabfarm', '--version') -RedirectStandardOutput (Join-Path $env:RUNNER_TEMP 'sub-out.txt') -NoNewWindow -PassThru
if (!$p.WaitForExit(60000)) { $p.Kill(); throw 'cargo-crabfarm timed out' }
if ($p.ExitCode) { throw "cargo-crabfarm crabfarm --version failed ($($p.ExitCode))" }

# No farm exists yet, so the node that autostart launched at install exited.
# `farm create` starts it (autostart is on and points at this install).
Invoke-CrabfarmCli @('farm', 'create', '--name', 'ci') | Out-Null
if (!(Test-Path -LiteralPath $data)) { throw "no data folder at $data after farm create" }
Wait-ForCondition { Test-NodeRunning } 60 'the node to run after farm create'

Write-Host '2. Silent reinstall over the running node (what an update does)'
if ((Invoke-BoundedProcess $Setup @('/S') 180)) { throw 'Reinstall failed' }
Assert-Installed $true $true
# PATH entry must not be duplicated.
if (((Get-UserPath) -split ';' | Where-Object { $_ -eq $dir }).Count -ne 1) { throw 'PATH entry duplicated by reinstall' }

Write-Host '3. Silent uninstall WITH the node running (the uninstaller stops it itself); data stays'
# The reinstall's autostart step started the node again.
Wait-ForCondition { Test-NodeRunning } 60 'the node to be running before the uninstall'
if ((Invoke-BoundedProcess (Join-Path $dir 'uninstall.exe') @('/S'))) { throw 'Uninstall failed' }
Assert-Uninstalled
$still = Get-Process -Name crabfarmd -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $daemon }
if ($still) { throw 'the node was still running after the uninstall' }
if (!(Test-Path -LiteralPath $data)) { throw 'farm data was deleted without /DELETEDATA' }
Assert-PathUntouched $pathBefore

Write-Host '4. Upgrade while crabfarm.exe is in use by a running process (the installer renames it aside)'
if ((Invoke-BoundedProcess $Setup @('/S', '/NOPATH', '/NOAUTOSTART'))) { throw 'Setup (no options) failed' }
Assert-Installed $false $false
$holderHome = Join-Path $env:RUNNER_TEMP 'crabfarm-holder'
$env:CRABFARM_HOME = $holderHome
$holder = $null
try {
    # A second node with its own data directory: it runs the installed
    # crabfarm.exe but is not the node the installer stops.
    Invoke-CrabfarmCli @('farm', 'create', '--name', 'holder') | Out-Null
    $holder = Start-Process -FilePath $cli -ArgumentList @('node') -PassThru -WindowStyle Hidden
    Start-Sleep -Seconds 5
    if ($holder.HasExited) { throw 'the process that should hold crabfarm.exe exited' }
    Write-Host "  holder PID $($holder.Id) runs $cli"
    Remove-Item Env:CRABFARM_HOME
    if ((Invoke-BoundedProcess $Setup @('/S', '/NOPATH', '/NOAUTOSTART') 180)) { throw 'Upgrade over an in-use crabfarm.exe failed' }
    if ($holder.HasExited) { throw 'the upgrade ended the process that held crabfarm.exe' }
    if (!(Get-ChildItem -LiteralPath $dir -Filter 'crabfarm.exe.old*')) { throw 'crabfarm.exe was not moved aside (the in-use path was not exercised)' }
    Assert-Installed $false $false
    $out = Invoke-CrabfarmCli @('--version')
    if ($out.Trim() -notin @("crabfarm $Version", "crabfarm $base")) { throw "upgraded crabfarm --version printed '$($out.Trim())'" }
} finally {
    $env:CRABFARM_HOME = $holderHome
    if ($holder -and !$holder.HasExited) {
        try { Invoke-CrabfarmCli @('node', '--stop') | Out-Null } catch { Write-Host "node --stop failed: $_" }
        if (!$holder.WaitForExit(30000)) { $holder.Kill() }
    }
    Remove-Item Env:CRABFARM_HOME -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $holderHome -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host '5. Uninstall with /DELETEDATA removes the data and the renamed leftovers'
if ((Invoke-BoundedProcess (Join-Path $dir 'uninstall.exe') @('/S', '/DELETEDATA'))) { throw 'Uninstall /DELETEDATA failed' }
Assert-Uninstalled
Wait-ForCondition { !(Test-Path -LiteralPath $data) } 30 'the farm data to be deleted by /DELETEDATA'
Assert-PathUntouched $pathBefore
Write-Host 'Installer lifecycle passed.'
