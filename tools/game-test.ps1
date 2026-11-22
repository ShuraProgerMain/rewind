# End-to-end check of "record the game's window?": a full-screen window in front (fake-game.ps1)
# gets an offer; "toast_action 10" stands for "Записывать окно", 11 for "Не спрашивать". When
# the game closes the recording goes back to the screen, without an error. The fake game covers
# the primary display for a few seconds at a time; no keys or clicks are sent.
# Usage: game-test.ps1 -Data <empty test data folder>
param(
    [Parameter(Mandatory)] [string]$Data,
    [string]$Exe
)
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
if (-not $Exe) { $Exe = Join-Path $here '..\target\release\rewind.exe' }
$Exe = (Resolve-Path $Exe -ErrorAction Stop).Path
Add-Type -Namespace W -Name N -MemberDefinition @'
[DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern System.IntPtr FindWindowW(string cls, string title);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(System.IntPtr h);
'@
$env:REWIND_DEBUG = '1'
New-Item -ItemType Directory -Force "$Data\out" | Out-Null
('{ "output_dir": "' + ("$Data\out" -replace '\\', '\\') + '", "buffer_on": true, "buffer_seconds": 30 }') | Out-File "$Data\settings.json" -Encoding utf8

function Send($c) { Start-Process $Exe -ArgumentList @('--data', "`"$Data`"", '--send', "`"$c`"") -Wait }
function Check($name, $ok) { '{0}  {1}' -f $(if ($ok) { 'PASS' } else { 'FAIL' }), $name }
function Settings { Get-Content "$Data\settings.json" -Raw | ConvertFrom-Json }
function Offers { @(Select-String -Path "$Data\rewind.log" -Pattern 'offering its window' -SimpleMatch).Count }
# Windows may give the new window the focus a little later: wait for the offer to appear.
function WaitOffers($n) { foreach ($i in 1..40) { if ((Offers) -ge $n) { break }; Start-Sleep -Milliseconds 250 } }
function Game($secs) { Start-Process powershell -ArgumentList @('-NoProfile', '-WindowStyle', 'Hidden', '-File', "`"$here\fake-game.ps1`"", '-Title', '"Fake Game"', '-Seconds', $secs) -PassThru }

$p = Start-Process $Exe -ArgumentList @('--data', "`"$Data`"") -PassThru
Start-Sleep 5

# --- a game comes to the front: the offer appears; "record the window" switches to it
$g = Game 12
WaitOffers 1
Check "a full-screen window gets the offer ($(Offers))" ((Offers) -eq 1)
$toast = [W.N]::FindWindowW('RewindToast', [NullString]::Value)
Check 'the offer is on screen' ($toast -ne [IntPtr]::Zero -and [W.N]::IsWindowVisible($toast))
Send 'toast_action 10'; Start-Sleep 2
$s = Settings
Check "the game window becomes the source ($($s.source.type) $($s.source.title))" ($s.source.type -eq 'window' -and $s.source.title -eq 'Fake Game')
Check "the screen is kept to come back to ($($s.game_return.source.type))" ($s.game_return.source.type -eq 'primary')
Send record; Start-Sleep 3   # recording when the game closes
Check 'no second offer for the same game while it runs' ((Offers) -eq 1)

# --- the game closes: back to the screen, no error
$g.WaitForExit(); Start-Sleep 3
$s = Settings
Check "after the game closes the screen is recorded again ($($s.source.type))" ($s.source.type -eq 'primary' -and -not $s.game_return)
$errors = @(Select-String -Path "$Data\rewind.log" -Pattern 'error:' -SimpleMatch).Count
Check "closing the game is not an error ($errors)" ($errors -eq 0)
Check 'the log says where it went back' ([bool](Select-String -Path "$Data\rewind.log" -Pattern 'back to Primary' -SimpleMatch))
$saved = Select-String -Path "$Data\rewind.log" -Pattern 'recording saved: (.+) \(' | Select-Object -Last 1
$file = if ($saved) { $saved.Matches[0].Groups[1].Value } else { '' }
Check "a recording running when the game closes is saved ($(Split-Path -Leaf $file))" ($file -and (Test-Path -LiteralPath $file))

# --- launched again: asked again; "do not ask" remembers the program
$g = Game 6
WaitOffers 2
Check "a new launch of the game is asked about again ($(Offers))" ((Offers) -eq 2)
Send 'toast_action 11'; Start-Sleep 1
$never = (Settings).game_never -join ','
Check "'do not ask' remembers the program ($never)" ($never -eq 'powershell.exe')
$g.WaitForExit(); Start-Sleep 1
$g = Game 5
Start-Sleep 4
Check "...and it is not asked about any more ($(Offers))" ((Offers) -eq 2)
$g.WaitForExit()

Send quit; Start-Sleep 2
Check 'quits' ($p.HasExited)
$panics = @(Select-String -Path "$Data\rewind.log" -Pattern 'panic' -SimpleMatch).Count
Check "no panics in the log ($panics)" ($panics -eq 0)
