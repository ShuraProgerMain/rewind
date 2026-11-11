# For each encoded file: average PSNR, the worst 1 % of frames and the single worst frame
# (luma, frames paired by number). For YUV sources only: the encoded file must hold the
# source's YUV values as they are (no colour conversion before encoding). Breakup in fast motion shows in the worst frames, not the average.
# Usage: worst-frames.ps1 -Source <lossless source> -Files a.mp4,b.mp4
param([Parameter(Mandatory)] [string]$Source, [Parameter(Mandatory)] [string[]]$Files)
$ff = "$env:APPDATA\media-downloader\bin\ffmpeg.exe"
$stats = Join-Path ([IO.Path]::GetTempPath()) 'rewind-psnr.txt'
foreach ($f in $Files) {
    # Both sides are compared as the YUV values they hold: no colour conversion that could
    # differ between them (an untagged source would otherwise be read as BT.601).
    $statsArg = $stats.Replace('\', '/').Replace(':', '\:')
    $g = "[0:v]setpts=N/60/TB,format=yuv420p[a];[1:v]setpts=N/60/TB,format=yuv420p[b];[a][b]psnr=stats_file='$statsArg'"
    Remove-Item $stats -ErrorAction SilentlyContinue
    Start-Process $ff -ArgumentList "-hide_banner -i `"$f`" -i `"$Source`" -lavfi `"$g`" -f null -" -NoNewWindow -Wait -RedirectStandardError "$stats.log"
    $y = Get-Content $stats | ForEach-Object { [double]([regex]::Match($_, 'psnr_y:([\d.]+)').Groups[1].Value) } | Sort-Object
    $low = $y[0..([Math]::Max(0, [int]($y.Count * 0.01) - 1))] | Measure-Object -Average
    '{0,-34} avg {1,5:N1} dB   worst 1% {2,5:N1} dB   worst frame {3,5:N1} dB   {4,6:N1} Mbit/s' -f (Split-Path $f -Leaf), ($y | Measure-Object -Average).Average, $low.Average, $y[0], ((Get-Item $f).Length * 8 / ($y.Count / 60) / 1e6)
}
