# Writes assets/crabfarm.ico (16, 24, 32, 48, 64 and 256 px, PNG entries) by
# drawing the same crab as website/favicon.svg with System.Drawing, so the
# installer icon matches the website. Windows only; the result is committed,
# run this only to change the artwork.
#   scripts\make-icon.ps1
[CmdletBinding()]
param([string]$Output = (Join-Path (Split-Path -Parent $PSScriptRoot) 'assets/crabfarm.ico'))
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
$sizes = 16, 24, 32, 48, 64, 256

function New-CrabPng([int]$size) {
    $bmp = [Drawing.Bitmap]::new($size, $size, [Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'
    $g.Clear([Drawing.Color]::Transparent)
    $k = $size / 32.0
    function P([double]$x, [double]$y) { [Drawing.PointF]::new([single]($x * $k), [single]($y * $k)) }
    # Rounded square, radius 7 of 32.
    $r = 7 * $k; $d = 2 * $r; $s = [double]$size
    $bg = [Drawing.Drawing2D.GraphicsPath]::new()
    $bg.AddArc(0, 0, $d, $d, 180, 90); $bg.AddArc($s - $d, 0, $d, $d, 270, 90)
    $bg.AddArc($s - $d, $s - $d, $d, $d, 0, 90); $bg.AddArc(0, $s - $d, $d, $d, 90, 90)
    $bg.CloseFigure()
    $g.FillPath([Drawing.SolidBrush]::new([Drawing.ColorTranslator]::FromHtml('#b8420f')), $bg)
    $pen = [Drawing.Pen]::new([Drawing.Color]::White, [single](2.4 * $k))
    $pen.StartCap = 'Round'; $pen.EndCap = 'Round'; $pen.LineJoin = 'Round'
    foreach ($m in 1, -1) {
        $x = { param($v) if ($m -eq 1) { $v } else { 32 - $v } }
        $claw = [Drawing.Drawing2D.GraphicsPath]::new()
        $claw.AddBezier((P (& $x 9) 12.5), (P (& $x 5.8) 11.9), (P (& $x 4) 9.5), (P (& $x 4.6) 6.5))
        $g.DrawPath($pen, $claw)
        $claw2 = [Drawing.Drawing2D.GraphicsPath]::new()
        $claw2.AddBezier((P (& $x 4.6) 6.5), (P (& $x 7.2) 6.6), (P (& $x 9.1) 8.4), (P (& $x 9.5) 10.9))
        $g.DrawPath($pen, $claw2)
        $g.DrawLine($pen, (P (& $x 12.5) 24.5), (P (& $x 10.5) 27.5))
        $g.FillEllipse([Drawing.Brushes]::White, [single]((& $x 13.2) * $k - 1.3 * $k), [single](17.5 * $k - 1.3 * $k), [single](2.6 * $k), [single](2.6 * $k))
    }
    $g.DrawEllipse($pen, [single](8 * $k), [single](13 * $k), [single](16 * $k), [single](11 * $k))
    $ms = [IO.MemoryStream]::new()
    $bmp.Save($ms, [Drawing.Imaging.ImageFormat]::Png)
    $g.Dispose(); $bmp.Dispose()
    , $ms.ToArray()
}

$entries = foreach ($size in $sizes) { , (New-CrabPng $size) }
$ms = [IO.MemoryStream]::new()
$w = [IO.BinaryWriter]::new($ms)
$w.Write([uint16]0); $w.Write([uint16]1); $w.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $dim = if ($sizes[$i] -ge 256) { 0 } else { $sizes[$i] }
    $w.Write([byte]$dim); $w.Write([byte]$dim); $w.Write([byte]0); $w.Write([byte]0)
    $w.Write([uint16]1); $w.Write([uint16]32)
    $w.Write([uint32]$entries[$i].Length); $w.Write([uint32]$offset)
    $offset += $entries[$i].Length
}
foreach ($bytes in $entries) { $w.Write($bytes) }
$w.Flush()
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Output) | Out-Null
[IO.File]::WriteAllBytes($Output, $ms.ToArray())
Write-Host "Wrote $Output ($($ms.Length) bytes)"
