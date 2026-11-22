# A clip being saved must not hold up the running recording or "stop", and pressing "save"
# again meanwhile must not queue another clip. The clip is made slow on purpose
# (REWIND_TEST_SLOW_SAVE_MS), as a buffer of several gigabytes would be. Opens no windows, sends no keys.
# Usage: save-test.ps1 -Data <empty test data folder>
param([Parameter(Mandatory)] [string]$Data, [string]$Exe)
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
if (-not $Exe) { $Exe = Join-Path $here '..\target\release\rewind.exe' }
$Exe = (Resolve-Path $Exe -ErrorAction Stop).Path
New-Item -ItemType Directory -Force "$Data\out" | Out-Null
$env:REWIND_TEST_SLOW_SAVE_MS = '15000'   # as long as a multi-gigabyte clip would take
('{ "output_dir": "' + ("$Data\out" -replace '\\', '\\') + '", "buffer_on": true, "buffer_seconds": 120, "quality": 14, "max_mbit": 300, "notifications": false, "microphone": false }') | Out-File "$Data\settings.json" -Encoding utf8
function Send($c) { Start-Process $Exe -ArgumentList @('--data', "`"$Data`"", '--send', "`"$c`"") -Wait }
function Check($name, $ok) { '{0}  {1}' -f $(if ($ok) { 'PASS' } else { 'FAIL' }), $name }
function At($pattern) { $l = Select-String -Path "$Data\rewind.log" -Pattern $pattern -SimpleMatch | Select-Object -Last 1; if ($l) { [datetime]::ParseExact($l.Line.Substring(0, 19), 'yyyy-MM-dd HH:mm:ss', $null) } }

$p = Start-Process $Exe -ArgumentList @('--data', "`"$Data`"") -PassThru
Start-Sleep 20          # something in the buffer
Send record; Start-Sleep 3
Send save; Start-Sleep -Milliseconds 300
Send save; Send save    # pressed again while the clip is written
Send record             # stop the recording right away
Start-Sleep 20
Send quit; Start-Sleep 5

$saving = At 'saving a clip'; $recSaved = At 'recording saved'; $clipSaved = At 'clip saved'
$ms = (Select-String -Path "$Data\rewind.log" -Pattern 'clip saved: .*, (\d+) ms' | Select-Object -Last 1).Matches[0].Groups[1].Value
Check "the recording stops while the clip is written (stop $recSaved, clip $clipSaved, clip took $ms ms)" ($recSaved -and $clipSaved -and ($clipSaved - $recSaved).TotalSeconds -ge 8)
$busy = @(Select-String -Path "$Data\rewind.log" -Pattern 'still being written' -SimpleMatch).Count
$clips = @(Get-ChildItem "$Data\out" -Recurse -Filter *.mp4).Count - 1   # all but the recording (names are Cyrillic; PowerShell 5 reads this script as ANSI)
Check "presses during the save are refused ($busy), one clip is made ($clips)" ($busy -ge 1 -and $clips -eq 1)
Check 'quits' ($p.HasExited)
Select-String -Path "$Data\rewind.log" -Pattern 'GPU scheduling|frames:' | Select-Object -First 3 | ForEach-Object { '      ' + $_.Line }
