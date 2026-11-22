# The source picker (live pictures of displays and windows) in both skins, then picks a
# display through it and checks the setting. No keys or clicks: the "ui" test hook drives it.
# Usage: picker-shots.ps1 -Data <empty test data folder> -Out <folder for the PNGs>
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
function Check($name, $ok) { '{0}  {1}' -f $(if ($ok) { 'PASS' } else { 'FAIL' }), $name }
function PageLog($pattern) { (Select-String -Path "$Data\rewind.log" -Pattern $pattern | Select-Object -Last 1).Matches[0].Groups[1].Value }

$p = Start-Process $Exe -ArgumentList @('--data', "`"$Data`"") -PassThru
Start-Sleep 5
Send show; Start-Sleep 3
foreach ($skin in 'standard', 'play') {
    Js "set({skin: '$skin'})"
    Js "document.querySelector('[data-cmd=picker]').click()"
    Start-Sleep 3   # the pictures arrive
    & "$here\ui-shot.ps1" -Out "$Out\$skin-picker.png" -Margin 16 | Out-Null
    Js "send({t: 'log', text: 'pics $skin ' + [...document.querySelectorAll('.src img')].filter((i) => i.naturalWidth > 0).length + ' of ' + document.querySelectorAll('.src').length})"
    $r = PageLog "pics $skin (\d+ of \d+)"
    Check "$skin`: the picker shows pictures ($r)" ($r -match '^[1-9]')
    Js "closePicker()"
}
# pick the second display (or the first window) through a card
Js "document.querySelector('[data-cmd=picker]').click()"
Js "{ const c = document.querySelectorAll('.src')[1]; window.__want = c.dataset.src; c.click(); }"
Start-Sleep 1
$src = (Get-Content "$Data\settings.json" -Raw | ConvertFrom-Json).source
Check "a card picks the source ($($src.type) $($src.device)$($src.title))" ($src.type -ne 'primary')
Js "send({t: 'log', text: 'closed ' + !document.getElementById('picker')})"
Check 'and the picker closes' ((PageLog 'closed (\w+)') -eq 'true')
Send quit; Start-Sleep 2
