# End-to-end check of recording from a capture card: a short recording in each mode, then the
# file is checked (size, frame rate, the card's sound track, the picture moves, sound and
# picture have the same length). Needs the card plugged in with a picture on it (a console).
# Opens no windows and sends no keys.
# Usage: card-test.ps1 -Data <empty test data folder> [-Card "fifine Video Capture"]
param(
    [Parameter(Mandatory)] [string]$Data,
    [string]$Card = 'fifine Video Capture',
    [string]$Exe
)
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
if (-not $Exe) { $Exe = Join-Path $here '..\target\release\rewind.exe' }
$Exe = (Resolve-Path $Exe -ErrorAction Stop).Path
$probe = Get-ChildItem (Join-Path $here '..\deps\ffmpeg') -Recurse -Filter ffprobe.exe | Select-Object -First 1 -ExpandProperty FullName
# A full (GPL) ffmpeg for the mpdecimate filter; the project's LGPL one lacks it.
$ffmpeg = @("$env:APPDATA\media-downloader\bin\ffmpeg.exe", (Join-Path (Split-Path $probe) 'ffmpeg.exe')) | Where-Object { Test-Path $_ } | Select-Object -First 1
$env:REWIND_DEBUG = '1'
New-Item -ItemType Directory -Force "$Data\out" | Out-Null

function Send($c) { Start-Process $Exe -ArgumentList @('--data', "`"$Data`"", '--send', "`"$c`"") -Wait }
function Check($name, $ok) { '{0}  {1}' -f $(if ($ok) { 'PASS' } else { 'FAIL' }), $name }
function Settings($mode) {
    $s = @{ output_dir = "$Data\out"; buffer_on = $false; microphone = $false; system_sound = $true; notifications = $false
        source = @{ type = 'card'; name = $Card; mode = $mode; label = 'Nintendo Switch' } }
    $s | ConvertTo-Json -Depth 4 | Out-File "$Data\settings.json" -Encoding utf8
}
function Newest { Get-ChildItem "$Data\out" -Recurse -Filter *.mp4 | Sort-Object LastWriteTime | Select-Object -Last 1 }
function Streams($f) { & $probe -v error -show_entries stream=codec_type,width,height,avg_frame_rate,duration:stream_tags=handler_name -of json $f.FullName | ConvertFrom-Json }

foreach ($mode in '1080p30', '720p60') {
    Settings $mode
    $p = Start-Process $Exe -ArgumentList @('--data', "`"$Data`"") -PassThru
    Start-Sleep 5
    Send record; Start-Sleep 10; Send record; Start-Sleep 4
    Send quit; Start-Sleep 2
    $f = Newest
    $want = @{ '1080p30' = @(1920, 1080, 30); '720p60' = @(1280, 720, 60) }[$mode]
    Check "$mode`: a recording is saved under the card's name ($($f.Directory.Name)\$($f.Name))" ($f -and $f.Directory.Name -eq 'Nintendo Switch')
    if (-not $f) { continue }
    $st = (Streams $f).streams
    $v = $st | Where-Object codec_type -eq 'video' | Select-Object -First 1
    $a = $st | Where-Object codec_type -eq 'audio' | Select-Object -First 1
    $rate = if ($v.avg_frame_rate -match '(\d+)/(\d+)') { [math]::Round([double]$matches[1] / [double]$matches[2]) } else { 0 }
    Check "$mode`: picture $($v.width)x$($v.height) at $rate fps" ($v.width -eq $want[0] -and $v.height -eq $want[1] -and $rate -eq $want[2])
    Check "$mode`: the card's sound is a track ($($a.tags.handler_name))" ($a -and $a.tags.handler_name -eq 'Sound: Nintendo Switch')
    $vd = [double]$v.duration; $ad = [double]$a.duration
    Check ("$mode`: sound and picture are as long ({0:N2} s / {1:N2} s)" -f $vd, $ad) ([math]::Abs($vd - $ad) -lt 0.3 -and $vd -gt 8)
    # A moving picture: count frames that differ from the one before.
    $out = (& $ffmpeg -hide_banner -i $f.FullName -vf mpdecimate -an -f null - 2>&1) -join "`n"
    $kept = ([regex]::Matches($out, 'frame=\s*(\d+)') | Select-Object -Last 1).Groups[1].Value
    Check "$mode`: the picture changes ($kept distinct frames)" ([int]$kept -gt 20)
    $vol = (& $ffmpeg -hide_banner -i $f.FullName -map 0:a:0 -af volumedetect -f null - 2>&1 | Select-String 'max_volume: (-?[\d.]+) dB').Matches[0].Groups[1].Value
    "      loudest sound in the recording: $vol dB (silence is -91)"
}
$errors = @(Select-String -Path "$Data\rewind.log" -Pattern 'error:|panic' ).Count
Check "no errors or panics in the log ($errors)" ($errors -eq 0)
Select-String -Path "$Data\rewind.log" -Pattern 'capture on' | ForEach-Object { '      ' + $_.Line }
