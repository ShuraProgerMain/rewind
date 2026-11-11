# Live pipeline check on a known picture: shows an image 1:1 in a borderless window on a
# display, records that area with Rewind and compares a frame with the image itself.
# Then encodes the same image offline with FFmpeg's own colour conversion and the same
# encoder, so the two numbers show what the capture + GPU colour conversion step costs.
# Usage: pipeline-test.ps1 -Work <folder> -Image <png> [-Monitor 1] [-Label name] [-Args ...]
param(
    [Parameter(Mandatory)] [string]$Work,
    [Parameter(Mandatory)] [string]$Image,
    [int]$Monitor = 1,
    [string]$Label = 'live',
    [string[]]$Args = @(),
    [string]$Exe = "$PSScriptRoot\..\target\release\rewind.exe"
)
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
Add-Type -Namespace W -Name D -MemberDefinition '[DllImport("user32.dll")] public static extern bool SetProcessDPIAware();'
[void][W.D]::SetProcessDPIAware()
$ff = "$env:APPDATA\media-downloader\bin\ffmpeg.exe"
$out = "$Work\$Label"
New-Item -ItemType Directory -Force $out | Out-Null
Get-ChildItem $out -Filter *.mp4 | Remove-Item -Force

$img = [System.Drawing.Image]::FromFile($Image)
$screens = @([System.Windows.Forms.Screen]::AllScreens | Sort-Object { -not $_.Primary })
$b = $screens[$Monitor].Bounds
$form = New-Object System.Windows.Forms.Form
$form.FormBorderStyle = 'None'; $form.StartPosition = 'Manual'; $form.TopMost = $true; $form.ShowInTaskbar = $false
$form.Bounds = New-Object System.Drawing.Rectangle $b.X, $b.Y, $img.Width, $img.Height
$box = New-Object System.Windows.Forms.PictureBox
$box.Dock = 'Fill'; $box.SizeMode = 'Normal'; $box.Image = $img
$form.Controls.Add($box)
$form.Show(); $form.Refresh()
Start-Sleep -Milliseconds 800

$a = @('cli', '--quiet', '--no-cursor', '--no-sound', '--no-mic', '--buffer', '0', '--monitor', "$Monitor",
       '--crop', "0,0,$($img.Width),$($img.Height)", '--out', "`"$out`"", '--script', '0.3:rec,3.3:rec,3.8:quit') + $Args
$p = Start-Process $Exe -ArgumentList $a -PassThru -WindowStyle Hidden
while (-not $p.HasExited) { [System.Windows.Forms.Application]::DoEvents(); Start-Sleep -Milliseconds 50 }
$form.Close(); $img.Dispose()

$f = (Get-ChildItem $out -Filter *.mp4 | Select-Object -First 1).FullName
function Measure-Quality($video) {
    $g = "[0:v]trim=start_frame=150:end_frame=151,setpts=N/TB,scale=in_color_matrix=bt709:in_range=tv:out_color_matrix=bt709:out_range=tv,format=yuv444p,split[a1][a2];[1:v]scale=out_color_matrix=bt709:out_range=tv,format=yuv444p,split[b1][b2];[a1][b1]psnr;[a2][b2]ssim"
    Start-Process $ff -ArgumentList "-hide_banner -i `"$video`" -i `"$Image`" -lavfi `"$g`" -f null -" -NoNewWindow -Wait -RedirectStandardError "$out\cmp.txt"
    $r = Get-Content "$out\cmp.txt" -Raw
    $p = [regex]::Match($r, 'PSNR y:([\d.inf]+) u:([\d.inf]+) v:([\d.inf]+)')
    $s = [regex]::Match($r, 'SSIM .*?All:([\d.]+)')
    if (-not $p.Success) { return 'no comparison (see cmp.txt)' }
    'Y {0,5:N1} U {1,5:N1} V {2,5:N1}  SSIM {3}' -f [double]$p.Groups[1].Value, [double]$p.Groups[2].Value, [double]$p.Groups[3].Value, $s.Groups[1].Value
}
"{0,-24} live Rewind:      {1}" -f $Label, (Measure-Quality $f)
# The same picture, converted by FFmpeg (exact BT.709) and encoded by the same NVENC settings.
$ref = "$out\offline.mp4"
Start-Process $ff -ArgumentList "-hide_banner -v error -y -loop 1 -framerate 60 -t 3 -i `"$Image`" -vf scale=out_color_matrix=bt709:out_range=tv,format=nv12 -c:v hevc_nvenc -preset p4 -tune hq -rc vbr -b:v 0 -cq 20 -maxrate 100M -bf 0 `"$ref`"" -NoNewWindow -Wait
"{0,-24} offline (FFmpeg): {1}" -f '', (Measure-Quality $ref)
Start-Process $ff -ArgumentList "-hide_banner -v error -y -i `"$f`" -i `"$Image`" -filter_complex `"[0:v]trim=start_frame=150:end_frame=151,setpts=N/TB,crop=400:180:40:110,scale=iw*3:ih*3:flags=neighbor[r];[1:v]crop=400:180:40:110,scale=iw*3:ih*3:flags=neighbor[s];[s][r]hstack`" -frames:v 1 `"$out\compare.png`"" -NoNewWindow -Wait
