# End-to-end check of the settings window, driven without touching the real mouse or keyboard:
# the page is operated through scripts (needs REWIND_DEBUG), the area picker through posted
# window messages. Prints one line per check.
# Usage: ui-test.ps1 -Data <empty test data folder> [-Shots <folder for screenshots>]
param(
    [Parameter(Mandatory)] [string]$Data,
    [string]$Shots = $Data,
    [string]$Exe = "$PSScriptRoot\..\target\release\rewind.exe"
)
Add-Type -Namespace W -Name T -MemberDefinition @'
[DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern System.IntPtr FindWindowW(string cls, string title);
[DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern System.IntPtr FindWindowExW(System.IntPtr parent, System.IntPtr after, string cls, string title);
[DllImport("user32.dll")] public static extern bool PostMessageW(System.IntPtr h, uint msg, System.IntPtr w, System.IntPtr l);
[DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(System.IntPtr h, out uint pid);
'@
$null0 = [NullString]::Value
$env:REWIND_DEBUG = '1'
New-Item -ItemType Directory -Force "$Data\out" | Out-Null
('{ "output_dir": "' + ("$Data\out" -replace '\\', '\\') + '" }') | Out-File "$Data\settings.json" -Encoding utf8

function Send($c) { Start-Process $Exe -ArgumentList @('--data', "`"$Data`"", '--send', "`"$c`"") -Wait }
function Js($code) { Send "ui $code"; Start-Sleep -Milliseconds 500 }
function Setting { Get-Content "$Data\settings.json" -Raw | ConvertFrom-Json }
function Check($name, $ok) { '{0}  {1}' -f $(if ($ok) { 'PASS' } else { 'FAIL' }), $name }
function Shot($name) { & "$PSScriptRoot\ui-shot.ps1" -Out "$Shots\$name.png" -Margin 24 | Out-Null }
function WebViews { @(Get-Process msedgewebview2 -ErrorAction SilentlyContinue).Count }

$before = WebViews
$p = Start-Process $Exe -ArgumentList @('--data', "`"$Data`"") -PassThru
Start-Sleep 3
$p.Refresh(); $ramTray = $p.WorkingSet64 / 1MB
Send show; Start-Sleep 4
Check 'window opens on a second launch' ([W.T]::FindWindowW('RewindSettings', $null0) -ne [IntPtr]::Zero)
Shot 'page-capture'
$withWindow = WebViews
$p.Refresh(); $ramOpen = $p.WorkingSet64 / 1MB

# switches and segmented controls write the settings file
Js "document.querySelector('[data-toggle=cursor]').click()"
Check 'switch changes a setting (cursor off)' ((Setting).cursor -eq $false)
Js "rewind.show('video')"; Js "document.querySelector('[data-seg=fps][data-value=\`"30\`"]').click()"
Check 'segmented control changes a setting (30 fps)' ((Setting).fps -eq 30)
Shot 'page-video'
Js "document.querySelector('[data-seg=fps][data-value=\`"60\`"]').click()"

# hotkey field
Js "rewind.show('keys')"; Js "document.querySelector('[data-key=save_clip]').click()"
Shot 'page-keys-listening'
Js "document.dispatchEvent(new KeyboardEvent('keydown', {code:'F7', key:'F7', ctrlKey:true, shiftKey:true, bubbles:true}))"
Check 'hotkey field records a combination' ((Setting).hotkeys.save_clip -eq 'Ctrl+Shift+F7')
Shot 'page-keys'
Js "document.querySelector('[data-key=save_clip]').click()"
Js "document.dispatchEvent(new KeyboardEvent('keydown', {code:'F10', key:'F10', ctrlKey:true, altKey:true, bubbles:true}))"
Check 'hotkey restored' ((Setting).hotkeys.save_clip -eq 'Ctrl+Alt+F10')

# area picker: drag from (400,300) to (1680,1020) on the display
Js "rewind.show('capture')"; Js "cmd('pick_area')"
$pick = [W.T]::FindWindowW('RewindAreaPicker', $null0)
Check 'area picker appears' ($pick -ne [IntPtr]::Zero)
if ($pick -ne [IntPtr]::Zero) {
    $a = [IntPtr](300 * 65536 + 400); $b = [IntPtr](1020 * 65536 + 1680)
    [void][W.T]::PostMessageW($pick, 0x0201, [IntPtr]1, $a)   # left button down
    [void][W.T]::PostMessageW($pick, 0x0200, [IntPtr]1, $b)   # move
    Start-Sleep -Milliseconds 200
    [void][W.T]::PostMessageW($pick, 0x0202, [IntPtr]0, $b)   # up
    Start-Sleep 2
}
$crop = (Setting).crop
Check "area is stored ($($crop -join ','))" (($crop -join ',') -eq '400,300,1280,720')
Start-Sleep 2
Check 'recording restarts with the area' ((Get-Content "$Data\rewind.log" -Tail 3) -match '1280x720')
Shot 'page-area'
Js "cmd('clear_area')"
Check 'area is cleared' ($null -eq (Setting).crop)

# folder dialog: open it, then close it without choosing
Js "rewind.show('files')"; Js "cmd('pick_folder')"; Start-Sleep 2
$dlg = [IntPtr]::Zero; $found = [IntPtr]::Zero
do {
    $dlg = [W.T]::FindWindowExW([IntPtr]::Zero, $dlg, '#32770', $null0)
    $owner = 0; [void][W.T]::GetWindowThreadProcessId($dlg, [ref]$owner)
    if ($dlg -ne [IntPtr]::Zero -and $owner -eq $p.Id) { $found = $dlg }
} while ($dlg -ne [IntPtr]::Zero -and $found -eq [IntPtr]::Zero)
Check 'folder dialog opens' ($found -ne [IntPtr]::Zero)
if ($found -ne [IntPtr]::Zero) { [void][W.T]::PostMessageW($found, 0x0010, [IntPtr]0, [IntPtr]0); Start-Sleep 1 }
Shot 'page-files'

# commands from the page
Js "cmd('record')"; Start-Sleep 3
Shot 'page-recording'
Js "cmd('record')"; Start-Sleep 2
# No clip is saved in this test, so any video in the folder is the recording. (No Cyrillic
# here: Windows PowerShell 5 reads this file as ANSI.)
Check 'record button makes a file' (@(Get-ChildItem "$Data\out" -Recurse -Filter '*.mp4').Count -ge 1)

# closing releases the browser
Js "cmd('close')"; Start-Sleep 4
Check 'window closes' ([W.T]::FindWindowW('RewindSettings', $null0) -eq [IntPtr]::Zero)
$afterClose = WebViews
$p.Refresh(); $ramClosed = $p.WorkingSet64 / 1MB
Check "browser processes are gone (before $before, open $withWindow, closed $afterClose)" ($afterClose -le $before)
Send show; Start-Sleep 4
Check 'window opens again' ([W.T]::FindWindowW('RewindSettings', $null0) -ne [IntPtr]::Zero)
Send quit
Check 'program exits' ($p.WaitForExit(8000))
'memory of rewind.exe: tray only {0:N0} MB, window open {1:N0} MB, after closing {2:N0} MB' -f $ramTray, $ramOpen, $ramClosed
Get-Content "$Data\rewind.log" | Select-String 'error|rejected|not understood|fatal|PANIC' | ForEach-Object { "LOG: $($_.Line)" }
