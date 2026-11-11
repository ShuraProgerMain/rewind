# Regression check: the program must exit even when its settings window is active and a
# lone Alt was pressed in it (Windows "menu mode" used to keep it alive forever).
# Usage: quit-test.ps1 -Data <test data folder> [-Rounds 4]
param(
    [Parameter(Mandatory)] [string]$Data,
    [int]$Rounds = 4,
    [string]$Exe = "$PSScriptRoot\..\target\release\rewind.exe"
)
New-Item -ItemType Directory -Force "$Data\out" | Out-Null
if (-not (Test-Path "$Data\settings.json")) {
    ('{ "output_dir": "' + ("$Data\out" -replace '\\', '\\') + '" }') | Out-File "$Data\settings.json" -Encoding ascii
}
function Send($c) { Start-Process $Exe -ArgumentList @('--data', "`"$Data`"", '--send', "`"$c`"") -Wait }
$failed = 0
for ($i = 1; $i -le $Rounds; $i++) {
    $p = Start-Process $Exe -ArgumentList @('--data', "`"$Data`"", '--background') -PassThru
    Start-Sleep 3
    Send show; Start-Sleep 4
    # Brings the window to the front with a tap on Alt, exactly the troublesome case.
    & "$PSScriptRoot\ui-shot.ps1" -Out "$Data\quit-test.png" -Margin 0 -Activate | Out-Null
    if ($i % 2 -eq 0) { Send record; Start-Sleep 2 }   # every other round: quit in the middle of a recording
    Send quit
    $ok = $p.WaitForExit(10000)
    if (-not $ok) { $failed++; Stop-Process $p -Force }
    '{0}  round {1}: exits after Alt in the settings window{2}' -f $(if ($ok) { 'PASS' } else { 'FAIL' }), $i, $(if ($i % 2 -eq 0) { ' (while recording)' } else { '' })
}
$log = Get-Content "$Data\rewind.log"
'starts in the journal: {0}, clean stops: {1}' -f @($log -match 'started Rewind').Count, @($log -match 'stopped$').Count
exit $failed
