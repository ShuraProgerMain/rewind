# End-to-end check of screenshots and gamepad combinations. A script cannot press gamepad
# buttons, so learning and firing are driven through the test hooks ("pad_learned MASK",
# "pad ID"); the real XInput reader still runs while combinations are set.
# Usage: pad-test.ps1 -Data <empty test data folder>
param(
    [Parameter(Mandatory)] [string]$Data,
    [string]$Exe
)
# PowerShell 5 leaves $PSScriptRoot empty inside parameter defaults.
if (-not $Exe) { $Exe = Join-Path $PSScriptRoot '..\target\release\rewind.exe' }
$Exe = (Resolve-Path $Exe -ErrorAction Stop).Path
Add-Type -AssemblyName System.Drawing, System.Windows.Forms
$env:REWIND_DEBUG = '1'
New-Item -ItemType Directory -Force "$Data\out" | Out-Null
('{ "output_dir": "' + ("$Data\out" -replace '\\', '\\') + '" }') | Out-File "$Data\settings.json" -Encoding utf8

function Send($c) { Start-Process $Exe -ArgumentList @('--data', "`"$Data`"", '--send', "`"$c`"") -Wait }
function Js($code) { Send "ui $code"; Start-Sleep -Milliseconds 500 }
function Setting { Get-Content "$Data\settings.json" -Raw | ConvertFrom-Json }
function Check($name, $ok) { '{0}  {1}' -f $(if ($ok) { 'PASS' } else { 'FAIL' }), $name }
function Pngs { @(Get-ChildItem "$Data\out" -Recurse -Filter *.png) }

$p = Start-Process $Exe -ArgumentList @('--data', "`"$Data`"") -PassThru
Start-Sleep 3

# screenshot by command, with the buffer running
$t0 = Get-Date
Send screenshot; Start-Sleep 2
$shots = Pngs
Check 'screenshot saved as PNG' ($shots.Count -eq 1)
if ($shots.Count) {
    $img = [System.Drawing.Image]::FromFile($shots[0].FullName)
    $size = "$($img.Width)x$($img.Height)"; $img.Dispose()
    Check "screenshot has the screen size ($size)" ($size -eq "$([System.Windows.Forms.Screen]::PrimaryScreen.Bounds.Width)x$([System.Windows.Forms.Screen]::PrimaryScreen.Bounds.Height)")
    "      file: $($shots[0].Name), $([math]::Round($shots[0].Length / 1MB, 2)) MB"
}

# learning a combination in the settings window
Send show; Start-Sleep 4
Js "document.querySelector('[data-tab=keys]').click()"
Js "document.querySelector('[data-pad=save_clip]').click()"
Send 'pad_learned 4096'; Start-Sleep -Milliseconds 600      # A alone
Check 'one button is refused' (-not (Setting).pad.save_clip)
Send 'pad_learned 960'; Start-Sleep -Milliseconds 600       # LB+RB+L3+R3
Check 'combination learned (LB+RB+L3+R3)' ((Setting).pad.save_clip -eq 'LB+RB+L3+R3')
& "$PSScriptRoot\ui-shot.ps1" -Out "$Data\keys.png" -Margin 24 | Out-Null

# the same combination for another action moves there
Js "document.querySelector('[data-pad=screenshot]').click()"
& "$PSScriptRoot\ui-shot.ps1" -Out "$Data\keys-learning.png" -Margin 24 | Out-Null
Send 'pad_learned 960'; Start-Sleep -Milliseconds 600
$s = Setting
Check 'a combination belongs to one action' ($s.pad.screenshot -eq 'LB+RB+L3+R3' -and $s.pad.save_clip -eq '')

# Esc cancels, Backspace clears
Js "document.querySelector('[data-pad=record]').click()"
Js "document.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape'}))"
Send 'pad_learned 12288'; Start-Sleep -Milliseconds 600     # A+B after cancelling: ignored
Check 'Esc cancels learning' (-not (Setting).pad.record)

# the combination fires its action
Send 'pad 4'; Start-Sleep 2
Check 'combination takes a screenshot' ((Pngs).Count -eq 2)

# the screenshots are listed on the Records tab
Js "document.querySelector('[data-tab=files]').click()"
Start-Sleep 1
Js "rewind && send({t: 'log', text: 'shots listed: ' + document.querySelectorAll('[data-file]').length + ', trim buttons: ' + document.querySelectorAll('[data-op=trim]').length})"
$line = Select-String -Path "$Data\rewind.log" -Pattern 'shots listed: (\d+), trim buttons: (\d+)' | Select-Object -Last 1
Check "screenshots listed without a trim button ($($line.Matches[0].Value))" ($line -and $line.Matches[0].Groups[1].Value -eq '2' -and $line.Matches[0].Groups[2].Value -eq '0')
& "$PSScriptRoot\ui-shot.ps1" -Out "$Data\files.png" -Margin 24 | Out-Null
Send 'ui cmd("close")'; Start-Sleep 1

# cost of watching the gamepad (a combination is set, so the reader polls)
$p.Refresh(); $cpu0 = $p.TotalProcessorTime.TotalMilliseconds
Send buffer; Start-Sleep 1   # buffer off: what is left is the tray and the pad reader
$p.Refresh(); $cpu1 = $p.TotalProcessorTime.TotalMilliseconds
Start-Sleep 10
$p.Refresh(); $cpu2 = $p.TotalProcessorTime.TotalMilliseconds
'      idle with the pad reader: {0:N1} ms CPU in 10 s ({1:N3} % of one core)' -f ($cpu2 - $cpu1), (($cpu2 - $cpu1) / 100)
Check 'pad reader costs under 0.5 % of a core' (($cpu2 - $cpu1) -lt 50)

Send quit; Start-Sleep 2
Check 'quits' ($p.HasExited)
$panics = @(Select-String -Path "$Data\rewind.log" -Pattern 'panic' -SimpleMatch).Count
Check "no panics in the log ($panics)" ($panics -eq 0)
