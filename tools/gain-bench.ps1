# What each NVENC option gains: bitrate and quality on fixed sources at the same cq.
# Pairs with cost-bench.ps1 (what each option costs live).
# Usage: gain-bench.ps1 -Work <folder with desktop.mkv, game.mkv, stress-g0.mkv, stress-g4.mkv> [-Cq 20]
param([Parameter(Mandatory)] [string]$Work, [int]$Cq = 20)
$ff = "$env:APPDATA\media-downloader\bin\ffmpeg.exe"
$common = "-c:v hevc_nvenc -tune hq -rc vbr -b:v 0 -cq $Cq -maxrate 100M -bufsize 200M -g 120 -forced-idr 1"
$variants = [ordered]@{
    'old (p4)'               = '-preset p4 -bf 0'
    'p4 + B'                 = '-preset p4 -bf 3 -b_ref_mode middle'
    'p4 + B + AQ'            = '-preset p4 -bf 3 -b_ref_mode middle -spatial-aq 1 -temporal-aq 1 -aq-strength 8'
    'p4 + B + lookahead'     = '-preset p4 -bf 3 -b_ref_mode middle -rc-lookahead 20'
    'p5 + B'                 = '-preset p5 -bf 3 -b_ref_mode middle'
    'full'                   = '-preset p5 -multipass qres -bf 3 -b_ref_mode middle -spatial-aq 1 -temporal-aq 1 -aq-strength 8 -rc-lookahead 20'
}
foreach ($src in 'game', 'stress-g0', 'stress-g4') {
    "--- $src"
    $files = @()
    foreach ($name in $variants.Keys) {
        $out = "$Work\gain-$src-$($name -replace '[^\w]+', '_').mp4"
        Start-Process $ff -ArgumentList "-hide_banner -v error -y -i `"$Work\$src.mkv`" -pix_fmt nv12 $common $($variants[$name]) `"$out`"" -NoNewWindow -Wait
        $files += $out
    }
    & "$PSScriptRoot\worst-frames.ps1" -Source "$Work\$src.mkv" -Files $files | ForEach-Object { $_ -replace "gain-$src-", '' -replace '\.mp4', '' }
}
