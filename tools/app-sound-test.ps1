# Checks "sound only from one program": two processes play different tones at the same time,
# 440 Hz from the target ("tonegame.exe") and 1000 Hz from another one ("tonenoise.exe").
# The recording must hold the first and not the second; a normal recording holds both.
# Usage: app-sound-test.ps1 -Work <folder>
param([Parameter(Mandatory)] [string]$Work, [string]$Exe = "$PSScriptRoot\..\target\release\rewind.exe")
$ff = "$env:APPDATA\media-downloader\bin\ffmpeg.exe"
New-Item -ItemType Directory -Force $Work | Out-Null
$ps = "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe"
foreach ($n in 'tonegame', 'tonenoise') { Copy-Item $ps "$Work\$n.exe" -Force }
foreach ($f in 440, 1000) {
    if (-not (Test-Path "$Work\tone$f.wav")) {
        & $ff -hide_banner -v error -y -f lavfi -i "sine=frequency=$($f):duration=7:sample_rate=48000" -af volume=0.25 -ac 2 "$Work\tone$f.wav"
    }
}
function Run($label, [string[]]$extra) {
    $out = "$Work\$label"
    if (Test-Path $out) { Get-ChildItem $out -Filter *.mp4 | Remove-Item -Force }
    $play = { param($exe, $wav) Start-Process $exe -ArgumentList '-NoProfile', '-Command', "(New-Object Media.SoundPlayer '$wav').PlaySync()" -WindowStyle Hidden -PassThru }
    $a = & $play "$Work\tonegame.exe" "$Work\tone440.wav"
    $b = & $play "$Work\tonenoise.exe" "$Work\tone1000.wav"
    Start-Sleep -Milliseconds 800
    $args = @('cli', '--quiet', '--no-mic', '--buffer', '0', '--out', "`"$out`"", '--script', '0.3:rec,4.3:rec,4.8:quit') + $extra
    Start-Process $Exe -ArgumentList $args -Wait -WindowStyle Hidden
    $a, $b | ForEach-Object { if (-not $_.HasExited) { Stop-Process $_ -Force } }
    $f = (Get-ChildItem $out -Filter *.mp4 | Select-Object -First 1).FullName
    $title = ((& $ff -hide_banner -i $f 2>&1 | Out-String) -split "`n" | Select-String 'handler_name' | Select-Object -Index 1).Line -replace '.*:\s*', ''
    $level = foreach ($hz in 440, 1000) {
        $r = & $ff -hide_banner -i $f -map 0:1 -af "bandpass=f=$($hz):width_type=h:w=60,volumedetect" -f null - 2>&1 | Out-String
        '{0} Hz {1,6} dB' -f $hz, [regex]::Match($r, 'mean_volume: ([-\d.]+)').Groups[1].Value
    }
    '{0,-22} track "{1}"   {2}' -f $label, $title.Trim(), ($level -join '   ')
}
Run 'all sound' @()
Run 'only tonegame.exe' @('--sound-app', 'tonegame.exe')
