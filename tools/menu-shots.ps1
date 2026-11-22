# Screenshots of the open selection menus (what to record, sound source, buffer length) in
# both skins. No keys or clicks: the page opens each menu through the "ui" test hook.
# Usage: menu-shots.ps1 -Data <empty test data folder> -Out <folder for the PNGs>
param(
    [Parameter(Mandatory)] [string]$Data,
    [Parameter(Mandatory)] [string]$Out,
    [string]$Exe
)
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
if (-not $Exe) { $Exe = Join-Path $here '..\target\release\rewind.exe' }
$Exe = (Resolve-Path $Exe -ErrorAction Stop).Path
$env:REWIND_DEBUG = '1'
New-Item -ItemType Directory -Force "$Data\out", $Out | Out-Null
('{ "output_dir": "' + ("$Data\out" -replace '\\', '\\') + '", "buffer_on": false }') | Out-File "$Data\settings.json" -Encoding utf8

function Send($c) { Start-Process $Exe -ArgumentList @('--data', "`"$Data`"", '--send', "`"$c`"") -Wait }
function Js($code) { Send "ui $code"; Start-Sleep -Milliseconds 600 }
function Shot($name) { & "$here\ui-shot.ps1" -Out "$Out\$name.png" -Margin 16 | Out-Null }

$p = Start-Process $Exe -ArgumentList @('--data', "`"$Data`"") -PassThru
Start-Sleep 5
Send show; Start-Sleep 3
foreach ($skin in 'standard', 'play') {
    Js "set({skin: '$skin'})"
    foreach ($m in @(@('capture', 'source'), @('capture', 'length'), @('sound', 'from'))) {
        Js "{ closeMenu(); document.querySelector('[data-tab=$($m[0])]').click(); }"
        Js "openMenu(document.querySelector('[data-popup=$($m[1])]'))"
        Shot "$skin-menu-$($m[1])"
    }
    Js "closeMenu()"
}
Send quit; Start-Sleep 2
Get-ChildItem $Out -Filter *.png | ForEach-Object Name
