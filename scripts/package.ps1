# Builds the Windows packages into artifacts/installer:
#   crabfarm-<version>-setup.exe, crabfarm-<version>-windows-x64.zip, SHA256SUMS.txt
# Build only: never installs crabfarm, never runs the setup, never changes PATH,
# autostart or the registry. (The installer lifecycle runs in CI only:
# scripts/test-installer.ps1.)
#
#   scripts\package.ps1 [-Version <x.y.z[-pre.N]>] [-MakeNsis <makensis.exe>] [-BinDir <folder>]
#
# -Version names the packages and the version the installer shows (Installed
# apps, the setup's title). It defaults to the version in Cargo.toml. A test
# package such as 1.0.0-rc.1 is built from a 1.0.0 tree this way: only the
# package name and the installer text change, the binaries still report the
# Cargo.toml version, and Cargo.toml itself is never touched.
#
# NSIS 3.13 (zlib licence, a build tool, not shipped): pass -MakeNsis or set
# CRABFARM_MAKENSIS to a makensis.exe; without either, scripts/fetch-nsis.ps1
# downloads the pinned portable ZIP into the repository-ignored artifacts/
# cache. Without -BinDir the release build runs first.
[CmdletBinding()]
param(
    [string]$Version,
    [string]$MakeNsis = $env:CRABFARM_MAKENSIS,
    [string]$BinDir,
    [int]$Jobs = 6
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$toml = Get-Content -LiteralPath (Join-Path $root 'Cargo.toml') -Raw
if ($toml -notmatch '(?m)^\[package\][\s\S]*?^version\s*=\s*"([^"]+)"') { throw 'No package version in Cargo.toml' }
if (!$Version) { $Version = $Matches[1] }
if ($Version -notmatch '^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z]+(?:\.[0-9A-Za-z]+)*)?$') { throw "Unexpected version: $Version" }
$numeric = "$($Matches[1]).$($Matches[2]).$($Matches[3]).0"
if (!$MakeNsis) {
    # No machine-specific default: fetch the pinned, checksum-verified NSIS into
    # the ignored artifacts/ cache (once; later runs reuse it).
    $cache = Join-Path $root 'artifacts/tools'
    $cached = Join-Path $cache 'nsis-3.13/makensis.exe'
    if (!(Test-Path -LiteralPath $cached -PathType Leaf)) {
        Write-Host "No -MakeNsis or CRABFARM_MAKENSIS: fetching the pinned NSIS into $cache"
        & (Join-Path $PSScriptRoot 'fetch-nsis.ps1') -Destination $cache | Out-Null
    }
    $MakeNsis = $cached
}
if (!(Test-Path -LiteralPath $MakeNsis -PathType Leaf)) {
    throw "NSIS compiler not found at '$MakeNsis'. Pass -MakeNsis, set CRABFARM_MAKENSIS, or run scripts/fetch-nsis.ps1 -Destination <folder>."
}
Write-Host "crabfarm $Version from commit $(& git -C $root rev-parse --short HEAD)"

function Invoke-BoundedCommand([string]$File, [string[]]$Arguments, [int]$Seconds) {
    $info = [Diagnostics.ProcessStartInfo]::new($File)
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.WorkingDirectory = $root
    foreach ($a in $Arguments) { $info.ArgumentList.Add($a) }
    $p = [Diagnostics.Process]::Start($info)
    Write-Host "Started PID $($p.Id): $(Split-Path -Leaf $File) (timeout $Seconds s)"
    $out = $p.StandardOutput.ReadToEndAsync()
    $err = $p.StandardError.ReadToEndAsync()
    try {
        if (!$p.WaitForExit($Seconds * 1000)) {
            $p.Kill($true) # only this process and its own children
            $p.WaitForExit()
            throw "$File timed out after $Seconds s"
        }
        $o = $out.GetAwaiter().GetResult(); if ($o) { Write-Host $o }
        $e = $err.GetAwaiter().GetResult(); if ($e) { Write-Host $e }
        if ($p.ExitCode) { throw "$File failed ($($p.ExitCode))" }
        return $o
    } finally { $p.Dispose() }
}

$cargo = (Get-Command cargo -ErrorAction Stop).Source
if (!$BinDir) {
    Invoke-BoundedCommand $cargo @('build', '--release', '--locked', '-j', "$Jobs") 3600 | Out-Null
    $meta = Invoke-BoundedCommand $cargo @('metadata', '--format-version', '1', '--no-deps') 120 | ConvertFrom-Json
    $BinDir = Join-Path $meta.target_directory 'release'
}
foreach ($exe in 'crabfarm.exe', 'crabfarmd.exe', 'cargo-crabfarm.exe') {
    if (!(Test-Path -LiteralPath (Join-Path $BinDir $exe) -PathType Leaf)) { throw "Missing $exe in $BinDir" }
}

$out = Join-Path $root 'artifacts/installer'
New-Item -ItemType Directory -Force -Path $out | Out-Null
$stage = Join-Path $out ('stage-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $stage | Out-Null
try {
    Copy-Item -LiteralPath (Join-Path $BinDir 'crabfarm.exe') -Destination $stage
    Copy-Item -LiteralPath (Join-Path $BinDir 'crabfarmd.exe') -Destination $stage
    Copy-Item -LiteralPath (Join-Path $BinDir 'cargo-crabfarm.exe') -Destination $stage
    Copy-Item -LiteralPath (Join-Path $root 'LICENSE') -Destination (Join-Path $stage 'LICENSE.txt')
    Copy-Item -LiteralPath (Join-Path $root 'docs/GETTING-STARTED.md') -Destination $stage
    Copy-Item -LiteralPath (Join-Path $root 'assets/crabfarm.ico') -Destination $stage

    $setup = Join-Path $out "crabfarm-$Version-setup.exe"
    Invoke-BoundedCommand $MakeNsis @('-V3', "-DVERSION=$Version", "-DVI_VERSION=$numeric", "-DSRC=$stage", "-DOUTFILE=$setup", (Join-Path $root 'installer/crabfarm.nsi')) 300 | Out-Null

    # The zip is the no-installer route: unpack anywhere, run `crabfarm path add` if wanted.
    $zip = Join-Path $out "crabfarm-$Version-windows-x64.zip"
    if (Test-Path -LiteralPath $zip) { Remove-Item -LiteralPath $zip }
    $zipDir = Join-Path $out "crabfarm-$Version-windows-x64"
    if (Test-Path -LiteralPath $zipDir) { Remove-Item -LiteralPath $zipDir -Recurse -Force }
    New-Item -ItemType Directory -Path $zipDir | Out-Null
    Copy-Item -Path (Join-Path $stage '*') -Destination $zipDir
    Remove-Item -LiteralPath (Join-Path $zipDir 'crabfarm.ico')
    Copy-Item -LiteralPath (Join-Path $root 'README.md') -Destination $zipDir
    Compress-Archive -LiteralPath $zipDir -DestinationPath $zip
    Remove-Item -LiteralPath $zipDir -Recurse -Force
} finally {
    Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue
}

$lines = foreach ($f in @($setup, $zip)) {
    $h = (Get-FileHash -LiteralPath $f -Algorithm SHA256).Hash.ToLowerInvariant()
    Write-Host ('{0:N0} bytes  {1}' -f (Get-Item -LiteralPath $f).Length, $f)
    "$h  $(Split-Path -Leaf $f)"
}
# sha256sum format with LF endings.
[IO.File]::WriteAllText((Join-Path $out 'SHA256SUMS.txt'), (($lines -join "`n") + "`n"))
Write-Host "Wrote $(Join-Path $out 'SHA256SUMS.txt')"
