$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

Add-Type -AssemblyName System.Drawing

$SafeMarginRatio = 0.03
$repoRoot = Split-Path -Parent $PSScriptRoot
$iconsDir = Join-Path $repoRoot 'src-tauri\icons'
$masterPath = Join-Path $iconsDir 'icon.png'
$masterTempPath = Join-Path $iconsDir 'icon.refresh.png'
$faviconPath = Join-Path $repoRoot 'static\favicon.png'
$requiredDesktopIcons = @(
    '32x32.png',
    '128x128.png',
    '128x128@2x.png',
    'icon.ico',
    'icon.icns'
)

function Get-AlphaBounds {
    param([System.Drawing.Bitmap]$Bitmap)

    $minX = $Bitmap.Width
    $minY = $Bitmap.Height
    $maxX = -1
    $maxY = -1
    for ($y = 0; $y -lt $Bitmap.Height; $y++) {
        for ($x = 0; $x -lt $Bitmap.Width; $x++) {
            if ($Bitmap.GetPixel($x, $y).A -gt 8) {
                if ($x -lt $minX) { $minX = $x }
                if ($x -gt $maxX) { $maxX = $x }
                if ($y -lt $minY) { $minY = $y }
                if ($y -gt $maxY) { $maxY = $y }
            }
        }
    }
    if ($maxX -lt 0) {
        throw 'icon.png has no visible pixels'
    }
    [System.Drawing.Rectangle]::FromLTRB($minX, $minY, $maxX + 1, $maxY + 1)
}

function New-HighQualityBitmap {
    param(
        [System.Drawing.Image]$Source,
        [int]$Width,
        [int]$Height,
        [System.Drawing.Rectangle]$Destination
    )

    $output = [System.Drawing.Bitmap]::new(
        $Width,
        $Height,
        [System.Drawing.Imaging.PixelFormat]::Format32bppArgb
    )
    $graphics = [System.Drawing.Graphics]::FromImage($output)
    try {
        $graphics.Clear([System.Drawing.Color]::Transparent)
        $graphics.CompositingMode = [System.Drawing.Drawing2D.CompositingMode]::SourceCopy
        $graphics.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
        $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
        $graphics.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
        $graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::HighQuality
        $graphics.DrawImage(
            $Source,
            $Destination,
            0,
            0,
            $Source.Width,
            $Source.Height,
            [System.Drawing.GraphicsUnit]::Pixel
        )
    }
    finally {
        $graphics.Dispose()
    }
    $output
}

function Save-TrayFrame {
    param(
        [System.Drawing.Bitmap]$Avatar,
        [string]$Path,
        [int]$PulseRadius
    )

    $frame = [System.Drawing.Bitmap]::new(
        44,
        44,
        [System.Drawing.Imaging.PixelFormat]::Format32bppArgb
    )
    $graphics = [System.Drawing.Graphics]::FromImage($frame)
    try {
        $graphics.Clear([System.Drawing.Color]::Transparent)
        $graphics.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
        $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
        $graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
        $graphics.DrawImage($Avatar, [System.Drawing.Rectangle]::new(1, 1, 42, 42))
        if ($PulseRadius -gt 0) {
            $white = [System.Drawing.SolidBrush]::new([System.Drawing.Color]::FromArgb(230, 255, 255, 255))
            $red = [System.Drawing.SolidBrush]::new([System.Drawing.Color]::FromArgb(255, 244, 67, 73))
            try {
                $graphics.FillEllipse($white, 34 - $PulseRadius, 34 - $PulseRadius, 2 * ($PulseRadius + 2), 2 * ($PulseRadius + 2))
                $graphics.FillEllipse($red, 36 - $PulseRadius, 36 - $PulseRadius, 2 * $PulseRadius, 2 * $PulseRadius)
            }
            finally {
                $white.Dispose()
                $red.Dispose()
            }
        }
        $frame.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
    }
    finally {
        $graphics.Dispose()
        $frame.Dispose()
    }
}

$loaded = [System.Drawing.Bitmap]::FromFile($masterPath)
try {
    $source = $loaded.Clone(
        [System.Drawing.Rectangle]::new(0, 0, $loaded.Width, $loaded.Height),
        [System.Drawing.Imaging.PixelFormat]::Format32bppArgb
    )
}
finally {
    $loaded.Dispose()
}

try {
    $bounds = Get-AlphaBounds -Bitmap $source
    $cropped = $source.Clone($bounds, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    try {
        $canvasSize = 512
        $margin = [Math]::Round($canvasSize * $SafeMarginRatio)
        $targetSize = $canvasSize - (2 * $margin)
        $fitted = New-HighQualityBitmap -Source $cropped -Width $canvasSize -Height $canvasSize `
            -Destination ([System.Drawing.Rectangle]::new($margin, $margin, $targetSize, $targetSize))
        try {
            $fitted.Save($masterTempPath, [System.Drawing.Imaging.ImageFormat]::Png)
        }
        finally {
            $fitted.Dispose()
        }
    }
    finally {
        $cropped.Dispose()
    }
}
finally {
    $source.Dispose()
}
Move-Item -LiteralPath $masterTempPath -Destination $masterPath -Force

Push-Location $repoRoot
try {
    & npm.cmd run tauri -- icon 'src-tauri/icons/icon.png' --output 'src-tauri/icons'
    if ($LASTEXITCODE -ne 0) {
        throw "Tauri icon generation failed with exit code $LASTEXITCODE"
    }
}
finally {
    Pop-Location
}

$master = [System.Drawing.Bitmap]::FromFile($masterPath)
try {
    $favicon = New-HighQualityBitmap -Source $master -Width 128 -Height 128 `
        -Destination ([System.Drawing.Rectangle]::new(0, 0, 128, 128))
    try {
        $favicon.Save($faviconPath, [System.Drawing.Imaging.ImageFormat]::Png)
    }
    finally {
        $favicon.Dispose()
    }
    Save-TrayFrame -Avatar $master -Path (Join-Path $iconsDir 'tray-logo-idle.png') -PulseRadius 0
    $pulseRadii = @(4, 5, 6, 5, 4, 5)
    for ($i = 0; $i -lt $pulseRadii.Count; $i++) {
        Save-TrayFrame -Avatar $master `
            -Path (Join-Path $iconsDir "tray-logo-rec-$i.png") `
            -PulseRadius $pulseRadii[$i]
    }
}
finally {
    $master.Dispose()
}

foreach ($name in $requiredDesktopIcons) {
    $path = Join-Path $iconsDir $name
    if (-not (Test-Path -LiteralPath $path)) {
        throw "Missing generated icon: $name"
    }
}

Write-Host 'Refreshed system icons from src-tauri/icons/icon.png with 3% safe margins.'
