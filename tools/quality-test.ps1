# Sharpness check: records a display for a few seconds, takes a screenshot of it at the same
# moment and compares a frame of the recording with the screenshot.
# Prints PSNR (higher = closer to the original; 40+ dB is visually clean) for brightness (Y)
# and colour (U, V), and writes a magnified side-by-side crop: screenshot left, recording right.
# Usage: quality-test.ps1 -Work <folder> [-Monitor 0] [-Label name] [-Args '--quality','18']
param(
    [Parameter(Mandatory)] [string]$Work,
    [int]$Monitor = 0,
    [string]$Label = 'test',
    [string[]]$Args = @(),
    [string]$Crop = '480:270:200:120',
    [string]$Exe = "$PSScriptRoot\..\target\release\rewind.exe"
)
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
$ff = "$env:APPDATA\media-downloader\bin\ffmpeg.exe"
$out = "$Work\$Label"
New-Item -ItemType Directory -Force $out | Out-Null
Get-ChildItem $out -Filter *.mp4 | Remove-Item -Force
$screens = @([System.Windows.Forms.Screen]::AllScreens | Sort-Object { -not $_.Primary })
$b = $screens[$Monitor].Bounds

$a = @('cli', '--quiet', '--no-cursor', '--no-sound', '--no-mic', '--buffer', '0', '--monitor', "$Monitor", '--out', "`"$out`"", '--script', '0.3:rec,4.3:rec,4.8:quit') + $Args
$p = Start-Process $Exe -ArgumentList $a -PassThru -WindowStyle Hidden
Start-Sleep -Milliseconds 3000
$bmp = New-Object System.Drawing.Bitmap $b.Width, $b.Height
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($b.Location, [System.Drawing.Point]::Empty, $b.Size)
$bmp.Save("$out\shot.png"); $g.Dispose(); $bmp.Dispose()
$p.WaitForExit()

$f = (Get-ChildItem $out -Filter *.mp4 | Select-Object -First 1).FullName
$info = (& $ff -hide_banner -i $f 2>&1 | Out-String)
$stream = ([regex]::Match($info, 'Video: (.+)')).Groups[1].Value
# PSNR in YUV 4:4:4 so that lost colour resolution shows up in U and V.
$r = & $ff -hide_banner -ss 2.6 -i $f -i "$out\shot.png" -frames:v 1 -lavfi "[0:v]scale=flags=bicubic:in_color_matrix=bt709:in_range=tv:out_color_matrix=bt709:out_range=tv,format=yuv444p[a];[1:v]scale=out_color_matrix=bt709:out_range=tv,format=yuv444p[b];[a][b]psnr" -f null - 2>&1 | Out-String
$m = [regex]::Match($r, 'PSNR y:([\d.inf]+) u:([\d.inf]+) v:([\d.inf]+)')
'{0,-18} Y {1,6} dB   U {2,6} dB   V {3,6} dB   {4:N1} Mbit/s   {5}' -f $Label, $m.Groups[1].Value, $m.Groups[2].Value, $m.Groups[3].Value, ((Get-Item $f).Length * 8 / 4 / 1e6), ($stream -replace ',\s*\d+ kb/s.*', '').Substring(0, [Math]::Min(60, $stream.Length))
& $ff -hide_banner -v error -y -ss 2.6 -i $f -i "$out\shot.png" -frames:v 1 -filter_complex "[1:v]crop=$Crop,scale=iw*3:ih*3:flags=neighbor[s];[0:v]crop=$Crop,scale=iw*3:ih*3:flags=neighbor[r];[s][r]hstack" "$out\compare.png"
