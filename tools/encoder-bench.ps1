# Offline encoder benchmark: the same NVENC settings Rewind can use, applied to fixed sources,
# so that settings can be compared fairly (live screen tests change from run to run).
#   desktop: a screenshot, 1 s still, then scrolling 6 px per frame (text in motion), lossless
#   game:    a few seconds of high-motion gameplay (any high-bitrate recording)
# For each setting prints bitrate, PSNR (Y and colour) and SSIM against the source.
# Usage: encoder-bench.ps1 -Work <folder> -Game <gameplay.mp4> [-GameStart 90]
param(
    [Parameter(Mandatory)] [string]$Work,
    [Parameter(Mandatory)] [string]$Game,
    [double]$GameStart = 90,
    [string[]]$Only = @()
)
$ff = "$env:APPDATA\media-downloader\bin\ffmpeg.exe"
New-Item -ItemType Directory -Force $Work | Out-Null
Add-Type -AssemblyName System.Windows.Forms, System.Drawing

# ---- sources (made once)
$desk = "$Work\desktop.mkv"
if (-not (Test-Path $desk)) {
    $b = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
    $bmp = New-Object System.Drawing.Bitmap $b.Width, $b.Height
    $g = [System.Drawing.Graphics]::FromImage($bmp); $g.CopyFromScreen($b.Location, [System.Drawing.Point]::Empty, $b.Size)
    $bmp.Save("$Work\desktop.png"); $g.Dispose(); $bmp.Dispose()
    # 3 copies stacked = room to scroll; 60 still frames, then 6 px per frame for 4 s
    & $ff -hide_banner -v error -y -loop 1 -framerate 60 -t 5 -i "$Work\desktop.png" -filter_complex `
        "[0:v]split=3[a][b][c];[a][b][c]vstack=3,crop=$($b.Width):$($b.Height):0:'if(lt(n,60),0,(n-60)*6)'" `
        -c:v ffv1 -pix_fmt bgr0 $desk
}
$game = "$Work\game.mkv"
if (-not (Test-Path $game)) {
    Start-Process $ff -ArgumentList "-hide_banner -v error -y -ss $GameStart -t 5 -i `"$Game`" -an -c:v ffv1 -pix_fmt yuv420p `"$game`"" -NoNewWindow -Wait
}

# ---- settings to compare
$base = '-c:v hevc_nvenc -preset p4 -tune hq -rc vbr -b:v 0 -bf 0 -g 120 -forced-idr 1'
$better = '-c:v hevc_nvenc -preset p5 -tune hq -multipass qres -rc vbr -b:v 0 -spatial-aq 1 -temporal-aq 1 -aq-strength 8 -bf 3 -b_ref_mode middle -rc-lookahead 20 -g 120 -forced-idr 1'
$sets = [ordered]@{
    'now cq24 max50'        = "$base -cq 24 -maxrate 50M -bufsize 100M"
    'now cq20 max120'       = "$base -cq 20 -maxrate 120M -bufsize 240M"
    'tuned cq20 max120'     = "$better -cq 20 -maxrate 120M -bufsize 240M"
    'tuned cq17 max120'     = "$better -cq 17 -maxrate 120M -bufsize 240M"
    'tuned cq20 max50'      = "$better -cq 20 -maxrate 50M -bufsize 100M"
    'tuned cq20 444'        = "$better -cq 20 -maxrate 120M -bufsize 240M -profile:v rext"
    'cbr 50 (like SP/OBS)'  = '-c:v hevc_nvenc -preset p4 -tune hq -rc cbr -b:v 50M -bf 0 -g 120'
}

foreach ($src in @(@('desktop', $desk), @('game', $game))) {
    "--- $($src[0])"
    foreach ($name in $sets.Keys) {
        if ($Only.Count -and $Only -notcontains $name) { continue }
        $out = "$Work\$($src[0])-$($name -replace '[^\w]+', '_').mp4"
        $fmt = if ($name -match '444') { 'yuv444p' } else { 'nv12' }
        $sw = [Diagnostics.Stopwatch]::StartNew()
        $cmd = "-hide_banner -v error -y -i `"$($src[1])`" -vf scale=out_color_matrix=bt709:out_range=tv:flags=bicubic,format=$fmt -colorspace bt709 -color_primaries bt709 -color_trc bt709 -color_range tv $($sets[$name]) `"$out`""
        Start-Process $ff -ArgumentList $cmd -NoNewWindow -Wait
        $secs = $sw.Elapsed.TotalSeconds
        if (-not (Test-Path $out)) { "{0,-22} failed" -f $name; continue }
        # Frames are paired by number (setpts=N), not by timestamp: the containers round differently.
        $graph = "[0:v]setpts=N/60/TB,scale=in_color_matrix=bt709:in_range=tv:out_color_matrix=bt709:out_range=tv,format=yuv444p,split[a1][a2];[1:v]setpts=N/60/TB,scale=out_color_matrix=bt709:out_range=tv,format=yuv444p,split[b1][b2];[a1][b1]psnr;[a2][b2]ssim"
        $log = "$Work\cmp.txt"
        Start-Process $ff -ArgumentList "-hide_banner -i `"$out`" -i `"$($src[1])`" -lavfi `"$graph`" -f null -" -NoNewWindow -Wait -RedirectStandardError $log
        $r = Get-Content $log -Raw
        $p = [regex]::Match($r, 'PSNR y:([\d.]+) u:([\d.]+) v:([\d.]+)')
        $s = [regex]::Match($r, 'SSIM .*?All:([\d.]+)')
        $mbit = (Get-Item $out).Length * 8 / 5 / 1e6
        '{0,-22} {1,6:N1} Mbit/s   PSNR Y {2,5} U {3,5} V {4,5}   SSIM {5}   encode {6:N1} s' -f $name, $mbit, `
            ([double]$p.Groups[1].Value).ToString('N1'), ([double]$p.Groups[2].Value).ToString('N1'), ([double]$p.Groups[3].Value).ToString('N1'), $s.Groups[1].Value, $secs
    }
}
