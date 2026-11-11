# Audio/video sync check: flashes a white window on a display and beeps at the same moment,
# three times, while recording that display; then compares when each shows up in the file.
param(
    [string]$Exe = "$PSScriptRoot\..\target\release\rewind.exe",
    [int]$Monitor = 1,
    # Seconds of recording before the flashes (use a few minutes to check for drift).
    [int]$Warmup = 2,
    [string]$Out = "$PSScriptRoot\..\test-sync"
)
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
$ff = "$env:APPDATA\media-downloader\bin\ffmpeg.exe"
$screens = @([System.Windows.Forms.Screen]::AllScreens | Sort-Object { -not $_.Primary })
$b = $screens[$Monitor].Bounds
if (Test-Path $Out) { Get-ChildItem $Out -Filter *.mp4 | Remove-Item -Force }

$p = Start-Process $Exe -ArgumentList @('cli', '--quiet', '--no-mic', '--buffer', '0', '--monitor', "$Monitor", '--out', "`"$Out`"", '--script', "0.3:rec,$($Warmup + 5):rec,$($Warmup + 5.5):quit") -PassThru -WindowStyle Hidden
Start-Sleep -Milliseconds ($Warmup * 1000)

$form = New-Object System.Windows.Forms.Form
$form.FormBorderStyle = 'None'; $form.StartPosition = 'Manual'; $form.TopMost = $true; $form.ShowInTaskbar = $false
$form.BackColor = [System.Drawing.Color]::White
$form.Bounds = $b
for ($i = 0; $i -lt 3; $i++) {
    $form.Show(); $form.Refresh()
    [console]::Beep(3000, 250)
    $form.Hide()
    [System.Windows.Forms.Application]::DoEvents()
    Start-Sleep -Milliseconds 900
}
$form.Dispose()
$p.WaitForExit()

$f = (Get-ChildItem $Out -Filter *.mp4 | Select-Object -First 1).FullName
# Moments when the picture turns white.
$v = & $ff -hide_banner -i $f -vf "scale=64:-1,signalstats,metadata=print:key=lavfi.signalstats.YAVG:file=-" -an -f null - 2>$null
$flashes = @(); $t = 0.0; $white = $false
foreach ($line in $v) {
    if ($line -match 'pts_time:([\d.]+)') { $t = [double]$Matches[1] }
    elseif ($line -match 'YAVG=([\d.]+)') {
        $now = [double]$Matches[1] -gt 200
        if ($now -and -not $white) { $flashes += $t }
        $white = $now
    }
}
# Moments when the 3 kHz tone starts in the system-sound track.
$a = & $ff -hide_banner -i $f -map 0:1 -af "bandpass=f=3000:width_type=h:w=100,silencedetect=noise=-30dB:d=0.1" -f null - 2>&1 | Out-String
$tones = @([regex]::Matches($a, 'silence_end: ([\d.]+)') | ForEach-Object { [double]$_.Groups[1].Value })

"flashes at: " + (($flashes | ForEach-Object { '{0:N3}' -f $_ }) -join ', ')
"tones at:   " + (($tones | ForEach-Object { '{0:N3}' -f $_ }) -join ', ')
foreach ($fl in $flashes) {
    $near = $tones | Sort-Object { [math]::Abs($_ - $fl) } | Select-Object -First 1
    if ($null -ne $near) { "sound is {0:N0} ms after the picture" -f (($near - $fl) * 1000) }
}
