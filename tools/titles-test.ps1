# End-to-end check of preset marker titles and the search on the Records tab.
# Gamepad: "pad 6" acts as the combination for "marker with title" (a script cannot press a
# gamepad); the card must not take the focus, the same combination steps through the presets
# and the highlighted one is taken after 3 s. Keyboard: arrows in the title field pick a
# preset. Text stays ASCII (PowerShell 5 reads scripts as ANSI), so the presets are English.
# Usage: titles-test.ps1 -Data <empty test data folder>
param(
    [Parameter(Mandatory)] [string]$Data,
    [string]$Exe
)
if (-not $Exe) { $Exe = Join-Path $PSScriptRoot '..\target\release\rewind.exe' }
$Exe = (Resolve-Path $Exe -ErrorAction Stop).Path
Add-Type -Namespace W -Name N -MemberDefinition @'
[DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern System.IntPtr FindWindowW(string cls, string title);
[DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern System.IntPtr FindWindowExW(System.IntPtr parent, System.IntPtr after, string cls, string title);
[DllImport("user32.dll")] public static extern System.IntPtr GetForegroundWindow();
[DllImport("user32.dll")] public static extern bool IsWindowVisible(System.IntPtr h);
[DllImport("user32.dll")] public static extern bool PostMessageW(System.IntPtr h, uint msg, System.IntPtr w, System.IntPtr l);
'@
$env:REWIND_DEBUG = '1'
$env:REWIND_TOAST_CAPTURABLE = '1'   # so the card shows up in the screenshots
New-Item -ItemType Directory -Force "$Data\out" | Out-Null
('{ "output_dir": "' + ("$Data\out" -replace '\\', '\\') + '", "folders_by_app": false, "mark_presets": ["Bug", "Funny", "Epic", "Short"] }') | Out-File "$Data\settings.json" -Encoding utf8

function Send($c) { Start-Process $Exe -ArgumentList @('--data', "`"$Data`"", '--send', "`"$c`"") -Wait }
function Js($code) { Send "ui $code"; Start-Sleep -Milliseconds 500 }
function Check($name, $ok) { '{0}  {1}' -f $(if ($ok) { 'PASS' } else { 'FAIL' }), $name }
function Note { [W.N]::FindWindowW('RewindNote', [NullString]::Value) }
function PageLog($pattern) { (Select-String -Path "$Data\rewind.log" -Pattern $pattern | Select-Object -Last 1).Matches[0].Groups[1].Value }

$p = Start-Process $Exe -ArgumentList @('--data', "`"$Data`"") -PassThru
Start-Sleep 5
# a recording without markers, so the search has something to leave out
Send record; Start-Sleep 2; Send record; Start-Sleep 2

# --- gamepad: open the card, step once (Bug -> Funny), let the countdown pick it
$before = [W.N]::GetForegroundWindow()
Send 'pad 6'; Start-Sleep -Milliseconds 700
$note = Note
Check 'gamepad: the title card appears' ($note -ne [IntPtr]::Zero -and [W.N]::IsWindowVisible($note))
Check 'gamepad: the card does not take the focus from the game' ([W.N]::GetForegroundWindow() -ne $note)
& "$PSScriptRoot\ui-shot.ps1" -Out "$Data\pad-card.png" -Margin 20 -Class RewindNote | Out-Null
Send 'pad 6'; Start-Sleep 4.5
Check 'gamepad: the card closes by itself after the countdown' (-not [W.N]::IsWindowVisible($note))

# --- gamepad: step past the last preset to "no title"
Send 'pad 6'; Start-Sleep -Milliseconds 500
foreach ($i in 1..4) { Send 'pad 6'; Start-Sleep -Milliseconds 300 }
Start-Sleep 4.5

# --- keyboard: arrows pick a preset (Down x3 = Epic), Enter keeps it
Send mark_note; Start-Sleep 1
$note = Note
$edit = [W.N]::FindWindowExW($note, [IntPtr]::Zero, 'Edit', [NullString]::Value)
foreach ($i in 1..3) { [void][W.N]::PostMessageW($edit, 0x0100, [IntPtr]0x28, [IntPtr]::Zero); Start-Sleep -Milliseconds 150 }
& "$PSScriptRoot\ui-shot.ps1" -Out "$Data\key-card.png" -Margin 20 -Class RewindNote | Out-Null
[void][W.N]::PostMessageW($edit, 0x0100, [IntPtr]0x0D, [IntPtr]::Zero); Start-Sleep 1

Send save; Start-Sleep 3
$clip = Get-ChildItem "$Data\out" -Filter *.mp4 | Sort-Object CreationTime | Select-Object -Last 1
$marks = @((Get-Content -LiteralPath ([IO.Path]::ChangeExtension($clip.FullName, 'markers.json')) -Raw -Encoding UTF8 | ConvertFrom-Json).markers)
$titles = ($marks | ForEach-Object { if ($_.title) { $_.title } else { '-' } }) -join ', '
Check "titles: gamepad picked Funny, then no title, keyboard picked Epic ($titles)" ($titles -eq 'Funny, -, Epic')

# --- presets are edited on the Keys tab
Send show; Start-Sleep 4
Js "document.querySelector('[data-tab=keys]').click()"
& "$PSScriptRoot\ui-shot.ps1" -Out "$Data\keys.png" -Margin 20 | Out-Null
Js "{ const f = document.querySelector('[data-presets]'); f.value = 'Alpha,  Beta ,, Gamma'; f.dispatchEvent(new Event('change', {bubbles: true})); }"
Start-Sleep 1
$presets = ((Get-Content "$Data\settings.json" -Raw | ConvertFrom-Json).mark_presets) -join '|'
Check "presets are saved from the Keys tab ($presets)" ($presets -eq 'Alpha|Beta|Gamma')

# --- search on the Records tab
Js "document.querySelector('[data-tab=files]').click()"
Start-Sleep 1
function Search($text) {
    Js "{ const f = document.querySelector('[data-search]'); f.focus(); f.value = '$text'; f.dispatchEvent(new Event('input', {bubbles: true})); }"
    Js "send({t: 'log', text: 'found ' + document.querySelectorAll('[data-file]').length + ' focus ' + ('search' in (document.activeElement.dataset || {}))})"
    PageLog 'found (\d+ focus \w+)'
}
$all = Search ''
$r = Search 'epic'
Check "searching a marker title finds the clip ($r of $all)" ($r -like '1 focus*')
Check '...and the search field keeps the focus while typing' ($r -like '*focus true')
& "$PSScriptRoot\ui-shot.ps1" -Out "$Data\search.png" -Margin 20 | Out-Null
$r = Search 'nothing-like-this'
Js "send({t: 'log', text: 'empty ' + !!document.querySelector('.empty')})"
Check "a search with no match says so ($r)" ($r -like '0 *' -and (PageLog 'empty (\w+)') -eq 'true')
$r = Search ''
Check "clearing the search shows everything again ($r)" ($r -eq $all)

Send quit; Start-Sleep 2
Check 'quits' ($p.HasExited)
$panics = @(Select-String -Path "$Data\rewind.log" -Pattern 'panic' -SimpleMatch).Count
Check "no panics in the log ($panics)" ($panics -eq 0)
