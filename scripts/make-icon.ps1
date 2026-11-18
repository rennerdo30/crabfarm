# Writes assets/crabfarm.ico (16, 24, 32, 48, 64 and 256 px, PNG entries)
# from assets/logo.svg, so the installer icon is the same artwork as the
# website and README. The SVG is rendered once at 512 px by headless Microsoft
# Edge (transparent background) and scaled down with high-quality resampling.
# Windows only; the result is committed, run this only to change the artwork.
#   scripts\make-icon.ps1
[CmdletBinding()]
param(
    [string]$Svg = (Join-Path (Split-Path -Parent $PSScriptRoot) 'assets/logo.svg'),
    [string]$Output = (Join-Path (Split-Path -Parent $PSScriptRoot) 'assets/crabfarm.ico')
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
$sizes = 16, 24, 32, 48, 64, 256
$master = 512

$edge = @(
    "${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe",
    "$env:ProgramFiles\Microsoft\Edge\Application\msedge.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $edge) { throw 'Microsoft Edge not found; it renders the SVG.' }

$work = Join-Path ([IO.Path]::GetTempPath()) ("crabfarm-icon-" + [guid]::NewGuid().ToString('n'))
New-Item -ItemType Directory -Path $work | Out-Null
try {
    $svgUri = ([Uri](Resolve-Path $Svg).Path).AbsoluteUri
    $html = Join-Path $work 'icon.html'
    Set-Content -Path $html -Encoding utf8 -Value "<!doctype html><html><body style='margin:0;background:transparent'><img src='$svgUri' width='$master' height='$master' style='display:block'></body></html>"
    $png = Join-Path $work 'master.png'
    $args = @('--headless=new', '--disable-gpu', '--hide-scrollbars', '--default-background-color=00000000',
              "--user-data-dir=$(Join-Path $work 'profile')", "--window-size=$master,$master",
              "--screenshot=$png", ([Uri]$html).AbsoluteUri)
    # Only this Edge process (recorded by its handle) is ever stopped.
    $p = Start-Process -FilePath $edge -ArgumentList $args -PassThru -WindowStyle Hidden
    if (-not $p.WaitForExit(60000)) { $p.Kill(); throw 'Edge did not finish rendering within 60 s.' }
    if (-not (Test-Path $png)) { throw 'Edge produced no screenshot.' }

    $src = [Drawing.Bitmap]::FromFile($png)
    $entries = foreach ($s in $sizes) {
        $bmp = [Drawing.Bitmap]::new($s, $s, [Drawing.Imaging.PixelFormat]::Format32bppArgb)
        $g = [Drawing.Graphics]::FromImage($bmp)
        $g.InterpolationMode = 'HighQualityBicubic'
        $g.PixelOffsetMode = 'HighQuality'
        $g.SmoothingMode = 'AntiAlias'
        $g.Clear([Drawing.Color]::Transparent)
        $g.DrawImage($src, 0, 0, $s, $s)
        $g.Dispose()
        $ms = [IO.MemoryStream]::new()
        $bmp.Save($ms, [Drawing.Imaging.ImageFormat]::Png)
        $bmp.Dispose()
        , @($s, $ms.ToArray())
    }
    $src.Dispose()

    # ICONDIR + ICONDIRENTRY table + PNG payloads.
    $out = [IO.MemoryStream]::new()
    $w = [IO.BinaryWriter]::new($out)
    $w.Write([uint16]0); $w.Write([uint16]1); $w.Write([uint16]$entries.Count)
    $offset = 6 + 16 * $entries.Count
    foreach ($e in $entries) {
        $s = $e[0]; $data = $e[1]
        $dim = if ($s -ge 256) { 0 } else { $s }
        $w.Write([byte]$dim); $w.Write([byte]$dim); $w.Write([byte]0); $w.Write([byte]0)
        $w.Write([uint16]1); $w.Write([uint16]32)
        $w.Write([uint32]$data.Length); $w.Write([uint32]$offset)
        $offset += $data.Length
    }
    foreach ($e in $entries) { $w.Write([byte[]]$e[1]) }
    $w.Flush()
    [IO.File]::WriteAllBytes($Output, $out.ToArray())
    Write-Host "wrote $Output ($($sizes -join ', ') px)"
}
finally {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
