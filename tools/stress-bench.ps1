# Worst-case motion: a game frame zoomed and panned fast, with fresh grain in every frame
# (like foliage, rain or particles). This is where recordings break into blocks when the
# bitrate runs out. Compares settings by average and worst-frame quality.
# Usage: stress-bench.ps1 -Work <folder> -Game <gameplay.mp4> [-At 120] [-Grain 14]
param(
    [Parameter(Mandatory)] [string]$Work,
    [Parameter(Mandatory)] [string]$Game,
    [double]$At = 120,
    [int]$Grain = 14,
    [string[]]$Only = @()
)
$ff = "$env:APPDATA\media-downloader\bin\ffmpeg.exe"
New-Item -ItemType Directory -Force $Work | Out-Null
$src = "$Work\stress-g$Grain.mkv"
if (-not (Test-Path $src)) {
    Start-Process $ff -ArgumentList "-hide_banner -v error -y -ss $At -i `"$Game`" -frames:v 1 `"$Work\frame.png`"" -NoNewWindow -Wait
    # 5 s at 60 fps: zoom from 1.0 to 1.6 while panning, plus per-frame grain, scaled to 2560x1440.
    $vf = "scale=5120:-2,zoompan=z='1+0.002*on':x='iw/2-(iw/zoom/2)+on*6':y='ih/2-(ih/zoom/2)':d=300:s=2560x1440:fps=60,noise=alls=$($Grain):allf=t,format=yuv420p"
    Start-Process $ff -ArgumentList "-hide_banner -v error -y -loop 1 -i `"$Work\frame.png`" -vf `"$vf`" -frames:v 300 -c:v ffv1 `"$src`"" -NoNewWindow -Wait
}
$base = '-c:v hevc_nvenc -preset p4 -tune hq -rc vbr -b:v 0 -bf 0 -g 120 -forced-idr 1'
$tuned = '-c:v hevc_nvenc -preset p5 -tune hq -multipass qres -rc vbr -b:v 0 -spatial-aq 1 -temporal-aq 1 -aq-strength 8 -bf 3 -b_ref_mode middle -rc-lookahead 20 -g 120 -forced-idr 1'
$sets = [ordered]@{
    'cbr50 (SP/OBS style)' = '-c:v hevc_nvenc -preset p4 -tune hq -rc cbr -b:v 50M -bf 0 -g 120'
    'now cq24 max50'       = "$base -cq 24 -maxrate 50M -bufsize 100M"
    'now cq20 max120'      = "$base -cq 20 -maxrate 120M -bufsize 240M"
    'tuned cq24 max50'     = "$tuned -cq 24 -maxrate 50M -bufsize 100M"
    'tuned cq20 max80'     = "$tuned -cq 20 -maxrate 80M -bufsize 160M"
    'tuned cq20 max120'    = "$tuned -cq 20 -maxrate 120M -bufsize 240M"
    'tuned cq20 max200'    = "$tuned -cq 20 -maxrate 200M -bufsize 400M"
    'h264 tuned cq20 max120' = ($tuned -replace 'hevc_nvenc', 'h264_nvenc') + ' -cq 20 -maxrate 120M -bufsize 240M'
    'av1 tuned cq28 max120'  = ($tuned -replace 'hevc_nvenc', 'av1_nvenc') + ' -cq 28 -maxrate 120M -bufsize 240M'
}
$files = @()
foreach ($name in $sets.Keys) {
    if ($Only.Count -and $Only -notcontains $name) { continue }
    $out = "$Work\stress-g$Grain-$($name -replace '[^\w]+', '_').mp4"
    Start-Process $ff -ArgumentList "-hide_banner -v error -y -i `"$src`" -pix_fmt nv12 $($sets[$name]) `"$out`"" -NoNewWindow -Wait
    if (Test-Path $out) { $files += $out } else { "$name failed" }
}
& "$PSScriptRoot\worst-frames.ps1" -Source $src -Files $files
