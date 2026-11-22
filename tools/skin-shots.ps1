# Screenshots of every settings tab and of the drawn cards (notification, recording pill) in both skins, for checking spacing by eye. No keys or clicks are sent:
# the page is driven through the "ui" test hook, the cards through --send commands.
# Usage: skin-shots.ps1 -Data <empty test data folder> -Out <folder for the PNGs>
param(
    [Parameter(Mandatory)] [string]$Data,
    [Parameter(Mandatory)] [string]$Out,
    [string]$Exe
)
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
if (-not $Exe) { $Exe = Join-Path $here '..\target\release\rewind.exe' }
$Exe = (Resolve-Path $Exe -ErrorAction Stop).Path
$env:REWIND_DEBUG = '1'
$env:REWIND_TOAST_CAPTURABLE = '1'
New-Item -ItemType Directory -Force "$Data\out", $Out | Out-Null
('{ "output_dir": "' + ("$Data\out" -replace '\\', '\\') + '", "buffer_on": true, "buffer_seconds": 600, "indicator": "always", "mark_presets": ["Bug", "Funny", "Epic", "Short"] }') | Out-File "$Data\settings.json" -Encoding utf8

function Send($c) { Start-Process $Exe -ArgumentList @('--data', "`"$Data`"", '--send', "`"$c`"") -Wait }
function Js($code) { Send "ui $code"; Start-Sleep -Milliseconds 600 }
function Shot($name, $class = 'RewindSettings') { & "$here\ui-shot.ps1" -Out "$Out\$name.png" -Margin 16 -Class $class | Out-Null }

$p = Start-Process $Exe -ArgumentList @('--data', "`"$Data`"") -PassThru
Start-Sleep 5
Send record; Start-Sleep 2; Send mark; Start-Sleep 1; Send record; Start-Sleep 3   # a recording with a marker for the list
foreach ($skin in 'standard', 'play') {
    Send show; Start-Sleep 3
    Js "set({skin: '$skin'})"
    Start-Sleep 1
    foreach ($tab in 'capture', 'video', 'sound', 'keys', 'files', 'system') {
        Js "{ const t = document.querySelector('[data-tab=$tab]'); t && t.click(); }"
        Shot "$skin-$tab"
    }

    Send save; Start-Sleep 1.2
    Shot "$skin-toast" 'RewindToast'
    # The marker title card is left out on purpose: opened from the keyboard it takes the focus,
    # opened from the gamepad it catches the keyboard for 3 s. Either way it would take what the
    # user is typing in another window. Shoot it only when nobody is at the keyboard.
    Shot "$skin-pill" 'RewindIndicator'
}
Send quit; Start-Sleep 2
Get-ChildItem $Out -Filter *.png | ForEach-Object Name
