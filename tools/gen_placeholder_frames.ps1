param(
  [string]$OutDir = "C:\Users\berat\Desktop\Omni Apps\omnidroid\assets\loading\frames",
  [int]$Width = 1280,
  [int]$Height = 800,
  [int]$Fps = 30,
  [int]$Frames = 60,
  [string]$Text = "LOADING"
)
# Placeholder loading animation: black bg, rotating arc + pulsing label.
# Replace assets/loading/frames with your own art, keep desc.txt format.
Add-Type -AssemblyName System.Drawing
$part = Join-Path $OutDir "part0"
New-Item -ItemType Directory -Force $part | Out-Null
Get-ChildItem $part -Filter *.png -ErrorAction SilentlyContinue | Remove-Item -Force

$cx = $Width / 2; $cy = $Height / 2
for ($i = 0; $i -lt $Frames; $i++) {
  $bmp = New-Object System.Drawing.Bitmap($Width, $Height)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
  $g.Clear([System.Drawing.Color]::Black)

  # rotating arc
  $startAngle = ($i * 360.0 / $Frames)
  $pen = New-Object System.Drawing.Pen([System.Drawing.Color]::FromArgb(255, 90, 200, 255), 10)
  $pen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
  $pen.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
  $r = 60
  $g.DrawArc($pen, [float]($cx - $r), [float]($cy - $r - 40), [float]($r*2), [float]($r*2), [float]$startAngle, 270.0)
  $pen.Dispose()

  # pulsing label
  $alpha = [int](150 + 105 * [math]::Sin($i * 2 * [math]::PI / $Frames))
  $brush = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb($alpha, 255, 255, 255))
  $font = New-Object System.Drawing.Font("Segoe UI", 28, [System.Drawing.FontStyle]::Bold)
  $sf = New-Object System.Drawing.StringFormat
  $sf.Alignment = [System.Drawing.StringAlignment]::Center
  $g.DrawString($Text, $font, $brush, [float]$cx, [float]($cy + 50), $sf)
  $brush.Dispose(); $font.Dispose()

  $g.Dispose()
  $name = Join-Path $part ("{0:0000}.png" -f $i)
  $bmp.Save($name, [System.Drawing.Imaging.ImageFormat]::Png)
  $bmp.Dispose()
}

# desc.txt: WIDTH HEIGHT FPS, then a single looping part.
$desc = "$Width $Height $Fps`nc 0 0 part0`n"
[System.IO.File]::WriteAllText((Join-Path $OutDir "desc.txt"), $desc)
Write-Output "generated $Frames frames + desc.txt in $OutDir"
