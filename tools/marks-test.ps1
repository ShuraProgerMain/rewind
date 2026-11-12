# End-to-end check of markers: in clips and recordings, titles from the title box (also when
# typed after the file was saved), the Records tab, and the files following the video when
# it is renamed, trimmed and deleted. Text stays ASCII (PowerShell 5 reads scripts as ANSI).
# Usage: marks-test.ps1 -Data <empty test data folder>
param(
    [Parameter(Mandatory)] [string]$Data,
    [string]$Exe
)
if (-not $Exe) { $Exe = Join-Path $PSScriptRoot '..\target\release\rewind.exe' }
$Exe = (Resolve-Path $Exe -ErrorAction Stop).Path
$env:REWIND_DEBUG = '1'
New-Item -ItemType Directory -Force "$Data\out" | Out-Null
('{ "output_dir": "' + ("$Data\out" -replace '\\', '\\') + '", "folders_by_app": false }') | Out-File "$Data\settings.json" -Encoding utf8

function Send($c) { Start-Process $Exe -ArgumentList @('--data', "`"$Data`"", '--send', "`"$c`"") -Wait }
function Js($code) { Send "ui $code"; Start-Sleep -Milliseconds 500 }
function Check($name, $ok) { '{0}  {1}' -f $(if ($ok) { 'PASS' } else { 'FAIL' }), $name }
function Videos { @(Get-ChildItem "$Data\out" -Filter *.mp4 | Sort-Object LastWriteTime) }
function Marks($video) {
    $j = [IO.Path]::ChangeExtension($video.FullName, 'markers.json')
    if (Test-Path -LiteralPath $j) { (Get-Content -LiteralPath $j -Raw -Encoding UTF8 | ConvertFrom-Json).markers } else { @() }
}

$p = Start-Process $Exe -ArgumentList @('--data', "`"$Data`"") -PassThru
Start-Sleep 6   # let the buffer fill a little

# --- a clip from the buffer with two markers, one titled in the box
Send mark; Start-Sleep 1
Send mark_note; Start-Sleep 1
Send 'note_submit Texture bug'; Start-Sleep -Milliseconds 500
Send save; Start-Sleep 3
$clip = (Videos)[-1]
$m = @(Marks $clip)
Check "clip has 2 markers ($($m.Count))" ($m.Count -eq 2)
Check 'title from the box is stored' (@($m | Where-Object { $_.title -eq 'Texture bug' }).Count -eq 1)
$dur = 0; if ($m.Count) { $dur = ($m | Measure-Object t -Maximum).Maximum }
'      clip markers at: ' + (($m | ForEach-Object { '{0:N2}' -f $_.t }) -join ', ')
Check 'markers lie near the end of the clip (pressed just before saving)' ($m.Count -eq 2 -and ($m[1].t - $m[0].t) -gt 0.6 -and ($m[1].t - $m[0].t) -lt 2.5)
$fcp = [IO.Path]::ChangeExtension($clip.FullName, 'otio')
Check 'timeline file written next to the clip' (Test-Path -LiteralPath $fcp)
if (Test-Path -LiteralPath $fcp) {
    $x = Get-Content -LiteralPath $fcp -Raw -Encoding UTF8 | ConvertFrom-Json
    $mk = @($x.tracks.children[0].children[0].markers)
    Check "timeline has 2 markers, titled ($($mk.name -join ' | '))" ($mk.Count -eq 2 -and ($mk.name -contains 'Texture bug'))
    Check "timeline has the video and both sound tracks ($($x.tracks.children.Count) tracks)" ($x.tracks.children.Count -eq 3)
}

# --- a recording: a marker at about 3 s, and one titled only after the recording ended
Send record; Start-Sleep 3
Send mark; Start-Sleep 2
Send mark_note; Start-Sleep 1
Send record; Start-Sleep 3
Send 'note_submit Late title'; Start-Sleep 1
$rec = (Videos)[-1]
$m = @(Marks $rec)
'      recording markers at: ' + (($m | ForEach-Object { '{0:N2} {1}' -f $_.t, $_.title }) -join ', ')
Check "recording has 2 markers ($($m.Count))" ($m.Count -eq 2)
# each --send takes about a second to start, so the press lands at 3-5 s
Check 'first marker 3-5 s in' ($m.Count -ge 1 -and $m[0].t -gt 2.8 -and $m[0].t -lt 5.2)
Check 'title typed after saving reaches the file' (@($m | Where-Object { $_.title -eq 'Late title' }).Count -eq 1)

# --- the Records tab
Send show; Start-Sleep 4
Js "document.querySelector('[data-tab=files]').click()"
Start-Sleep 1
Js "document.querySelector('[data-op=marks]').click()"
& "$PSScriptRoot\ui-shot.ps1" -Out "$Data\files-marks.png" -Margin 24 | Out-Null
Js "const f = document.querySelector('[data-mark-title]'); f.focus(); f.value = 'Typed on the page'; f.blur();"
Start-Sleep 1
$m = @(Marks $rec)
Check 'title edited on the page is saved' (@($m | Where-Object { $_.title -eq 'Typed on the page' }).Count -eq 1)

# trim view shows the markers
Js "document.querySelector('[data-mop=go]').click()"
Start-Sleep 2
Js "send({t: 'log', text: 'ticks: ' + document.querySelectorAll('#tl-marks .mk').length + ' at ' + T.t.toFixed(2)})"
$line = Select-String -Path "$Data\rewind.log" -Pattern 'ticks: (\d+) at ([\d.]+)' | Select-Object -Last 1
Check "trim view shows the markers and starts at one ($($line.Matches[0].Value))" ($line -and $line.Matches[0].Groups[1].Value -eq '2')
& "$PSScriptRoot\ui-shot.ps1" -Out "$Data\trim-marks.png" -Margin 24 | Out-Null
Js "T.a = 1; T.b = 4.5; saveTrim()"
Start-Sleep 2
$cut = Get-ChildItem "$Data\out" -Filter '*(*' -Include *.mp4 -Recurse | Select-Object -First 1
if (-not $cut) { $cut = (Videos | Where-Object { $_.Name -notin @($clip.Name, $rec.Name) })[-1] }
$m = @(Marks $cut)
'      trimmed: ' + $cut.Name + ', markers at ' + (($m | ForEach-Object { '{0:N2}' -f $_.t }) -join ', ')
Check 'trimmed copy keeps the marker inside it, shifted' ($m.Count -eq 1 -and $m[0].t -gt 2 -and $m[0].t -lt 4.5)

# rename and delete carry the marker files along
$recPath = $rec.FullName -replace '\\', '\\\\'
Js "send({t: 'file', op: 'rename', path: '$recPath', name: 'renamed-rec'})"
Start-Sleep 1
Check 'rename moves the marker files' ((Test-Path "$Data\out\renamed-rec.markers.json") -and (Test-Path "$Data\out\renamed-rec.otio") -and -not (Test-Path -LiteralPath ([IO.Path]::ChangeExtension($rec.FullName, 'markers.json'))))
$newPath = "$Data\out\renamed-rec.mp4" -replace '\\', '\\\\'
Js "send({t: 'file', op: 'delete', path: '$newPath'})"
Start-Sleep 2
Check 'delete takes the marker files to the Recycle Bin too' (-not (Test-Path "$Data\out\renamed-rec.mp4") -and -not (Test-Path "$Data\out\renamed-rec.markers.json") -and -not (Test-Path "$Data\out\renamed-rec.otio"))

# nothing to mark with the buffer off
Send 'ui cmd("close")'; Start-Sleep 1
Send buffer; Start-Sleep 1
Send mark; Start-Sleep 1
Check 'no marker without buffer or recording' ((Select-String -Path "$Data\rewind.log" -Pattern 'marked ' -SimpleMatch).Count -eq 4)

Send quit; Start-Sleep 2
Check 'quits' ($p.HasExited)
$panics = @(Select-String -Path "$Data\rewind.log" -Pattern 'panic' -SimpleMatch).Count
Check "no panics in the log ($panics)" ($panics -eq 0)
