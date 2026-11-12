# End-to-end check of markers: in clips and recordings, written into the videos as chapters
# (what DaVinci Resolve shows as clip markers), titles from the title box (also when typed
# after the file was saved, which rewrites the chapters), the Records tab, and the marker
# file following the video when it is renamed, trimmed and deleted.
# Text stays ASCII (PowerShell 5 reads scripts as ANSI).
# Usage: marks-test.ps1 -Data <empty test data folder>
param(
    [Parameter(Mandatory)] [string]$Data,
    [string]$Exe
)
if (-not $Exe) { $Exe = Join-Path $PSScriptRoot '..\target\release\rewind.exe' }
$Exe = (Resolve-Path $Exe -ErrorAction Stop).Path
$Probe = (Get-ChildItem (Join-Path $PSScriptRoot '..\deps\ffmpeg') -Recurse -Filter ffprobe.exe | Select-Object -First 1).FullName
$env:REWIND_DEBUG = '1'
New-Item -ItemType Directory -Force "$Data\out" | Out-Null
('{ "output_dir": "' + ("$Data\out" -replace '\\', '\\') + '", "folders_by_app": false }') | Out-File "$Data\settings.json" -Encoding utf8

function Send($c) { Start-Process $Exe -ArgumentList @('--data', "`"$Data`"", '--send', "`"$c`"") -Wait }
function Js($code) { Send "ui $code"; Start-Sleep -Milliseconds 500 }
function Check($name, $ok) { '{0}  {1}' -f $(if ($ok) { 'PASS' } else { 'FAIL' }), $name }
function Videos { @(Get-ChildItem "$Data\out" -Filter *.mp4 | Sort-Object CreationTime) }
function Marks($video) {
    $j = [IO.Path]::ChangeExtension($video.FullName, 'markers.json')
    if (Test-Path -LiteralPath $j) { (Get-Content -LiteralPath $j -Raw -Encoding UTF8 | ConvertFrom-Json).markers } else { @() }
}
# Chapters and tracks inside a video, as editors see them.
function Inside($video) {
    $json = & $Probe -v error -show_chapters -show_streams -of json $video.FullName 2>$null | Out-String
    $bytes = [Text.Encoding]::Default.GetBytes($json)   # ffprobe writes UTF-8; PowerShell read it as ANSI
    [Text.Encoding]::UTF8.GetString($bytes) | ConvertFrom-Json
}
function Starts($info) { ($info.chapters | ForEach-Object { '{0:N2}' -f [double]$_.start_time }) -join ', ' }

$p = Start-Process $Exe -ArgumentList @('--data', "`"$Data`"") -PassThru
Start-Sleep 6   # let the buffer fill a little

# --- a recording without markers stays as it was: no chapters, no extra track
Send record; Start-Sleep 3
Send record; Start-Sleep 3
$plain = (Videos)[-1]
$i = Inside $plain
Check "recording without markers has no chapters ($(@($i.chapters).Count)) and no extra track ($(@($i.streams).Count) tracks)" (@($i.chapters).Count -eq 0 -and @($i.streams | Where-Object codec_type -eq 'data').Count -eq 0)

# --- a clip from the buffer with two markers, one titled in the box
Send mark; Start-Sleep 1
Send mark_note; Start-Sleep 1
Send 'note_submit Texture bug'; Start-Sleep -Milliseconds 500
Send save; Start-Sleep 3
$clip = (Videos)[-1]
$m = @(Marks $clip)
Check "clip has 2 markers ($($m.Count))" ($m.Count -eq 2)
Check 'title from the box is stored' (@($m | Where-Object { $_.title -eq 'Texture bug' }).Count -eq 1)
'      clip markers at: ' + (($m | ForEach-Object { '{0:N2}' -f $_.t }) -join ', ')
Check 'markers lie near the end of the clip (pressed just before saving)' ($m.Count -eq 2 -and ($m[1].t - $m[0].t) -gt 0.6 -and ($m[1].t - $m[0].t) -lt 2.5)
$i = Inside $clip
'      clip chapters at: ' + (Starts $i)
Check "clip has 3 chapters: start + 2 markers ($(@($i.chapters).Count))" (@($i.chapters).Count -eq 3)
Check 'chapters start at the markers' (@($i.chapters).Count -eq 3 -and [double]$i.chapters[0].start_time -eq 0 -and [math]::Abs([double]$i.chapters[1].start_time - $m[0].t) -lt 0.02 -and [math]::Abs([double]$i.chapters[2].start_time - $m[1].t) -lt 0.02)
Check 'the titled chapter carries its title' (@($i.chapters | Where-Object { $_.tags.title -eq 'Texture bug' }).Count -eq 1)
Check "both sound tracks are still there ($(@($i.streams | Where-Object codec_type -eq 'audio').Count))" (@($i.streams | Where-Object codec_type -eq 'audio').Count -eq 2)
Check 'no files next to the clip but the marker file' (@(Get-ChildItem "$Data\out" -Filter ($clip.BaseName + '*')).Count -eq 2)

# --- a recording: a marker at about 3 s, and one titled only after the recording ended
# (Each --send starts a process, which may take the focus and close the title box, so the
# title is typed before the next command; a title typed later is tested on the page below.)
Send record; Start-Sleep 3
Send mark; Start-Sleep 2
Send mark_note; Start-Sleep 1
Send 'note_submit Late title'; Start-Sleep 1
Send record; Start-Sleep 3
$rec = (Videos)[-1]
$dateBefore = $rec.LastWriteTime
$m = @(Marks $rec)
'      recording markers at: ' + (($m | ForEach-Object { '{0:N2} {1}' -f $_.t, $_.title }) -join ', ')
Check "recording has 2 markers ($($m.Count))" ($m.Count -eq 2)
# each --send takes about a second to start, so the press lands at 3-5 s
Check 'first marker 3-5 s in' ($m.Count -ge 1 -and $m[0].t -gt 2.8 -and $m[0].t -lt 5.2)
Check 'title typed while recording reaches the marker file' (@($m | Where-Object { $_.title -eq 'Late title' }).Count -eq 1)
$i = Inside $rec
'      recording chapters at: ' + (Starts $i) + ' / ' + (($i.chapters | ForEach-Object { $_.tags.title }) -join ' | ')
Check 'recording has chapters at its markers' (@($i.chapters).Count -eq 3 -and [math]::Abs([double]$i.chapters[1].start_time - $m[0].t) -lt 0.02)
Check '...and is written into the video at once' (@($i.chapters | Where-Object { $_.tags.title -eq 'Late title' }).Count -eq 1)
Check 'no temporary files left' (@(Get-ChildItem "$Data\out" -Filter '*rewind-tmp*').Count -eq 0)

# --- the Records tab
Send show; Start-Sleep 4
Js "document.querySelector('[data-tab=files]').click()"
Start-Sleep 1
Js "document.querySelector('[data-op=marks]').click()"
& "$PSScriptRoot\ui-shot.ps1" -Out "$Data\files-marks.png" -Margin 24 | Out-Null
Js "const f = document.querySelector('[data-mark-title]'); f.focus(); f.value = 'Typed on the page'; f.blur();"
Start-Sleep 3
$m = @(Marks $rec)
Check 'title edited on the page is saved' (@($m | Where-Object { $_.title -eq 'Typed on the page' }).Count -eq 1)
Check 'the video keeps its date after the rewrite' ((Get-Item -LiteralPath $rec.FullName).LastWriteTime -eq $dateBefore)
$i = Inside $rec
Check '...and written into the video' (@($i.chapters | Where-Object { $_.tags.title -eq 'Typed on the page' }).Count -eq 1)
Check "...without growing extra tracks ($(@($i.streams).Count) tracks)" (@($i.streams | Where-Object codec_type -eq 'data').Count -eq 1 -and @($i.streams | Where-Object codec_type -eq 'audio').Count -eq 2)

# trim view shows the markers
Js "document.querySelector('[data-mop=go]').click()"
Start-Sleep 2
Js "send({t: 'log', text: 'ticks: ' + document.querySelectorAll('#tl-marks .mk').length + ' at ' + T.t.toFixed(2)})"
$line = Select-String -Path "$Data\rewind.log" -Pattern 'ticks: (\d+) at ([\d.]+)' | Select-Object -Last 1
Check "trim view shows the markers and starts at one ($($line.Matches[0].Value))" ($line -and $line.Matches[0].Groups[1].Value -eq '2')
Js "T.a = 1; T.b = 4.5; saveTrim()"
Start-Sleep 2
$cut = (Videos)[-1]
$m = @(Marks $cut)
$i = Inside $cut
'      trimmed: ' + $cut.Name + ', markers at ' + (($m | ForEach-Object { '{0:N2}' -f $_.t }) -join ', ') + ', chapters at ' + (Starts $i)
Check 'trimmed copy keeps the marker inside it, shifted' ($m.Count -eq 1 -and $m[0].t -gt 2 -and $m[0].t -lt 4.5)
Check '...also as a chapter' (@($i.chapters).Count -eq 2 -and [math]::Abs([double]$i.chapters[1].start_time - $m[0].t) -lt 0.02)

# deleting a marker on the page takes its chapter out of the video
$before = @((Inside $rec).chapters).Count
Js "document.querySelector('[data-tab=files]').click()"
$recPath = $rec.FullName -replace '\\', '\\\\'
$firstId = (Marks $rec)[0].id
Js "send({t: 'mark', op: 'delete', path: '$recPath', id: $firstId})"
Start-Sleep 3
Check "deleting a marker removes its chapter ($before -> $(@((Inside $rec).chapters).Count))" (@((Inside $rec).chapters).Count -eq $before - 1)

# rename and delete carry the marker file along
Js "send({t: 'file', op: 'rename', path: '$recPath', name: 'renamed-rec'})"
Start-Sleep 1
Check 'rename moves the marker file' ((Test-Path "$Data\out\renamed-rec.markers.json") -and -not (Test-Path -LiteralPath ([IO.Path]::ChangeExtension($rec.FullName, 'markers.json'))))
$newPath = "$Data\out\renamed-rec.mp4" -replace '\\', '\\\\'
Js "send({t: 'file', op: 'delete', path: '$newPath'})"
Start-Sleep 2
Check 'delete takes the marker file to the Recycle Bin too' (-not (Test-Path "$Data\out\renamed-rec.mp4") -and -not (Test-Path "$Data\out\renamed-rec.markers.json"))

# nothing to mark with the buffer off
Send 'ui cmd("close")'; Start-Sleep 1
Send buffer; Start-Sleep 1
Send mark; Start-Sleep 1
Check 'no marker without buffer or recording' ((Select-String -Path "$Data\rewind.log" -Pattern 'marked ' -SimpleMatch).Count -eq 4)

Send quit; Start-Sleep 2
Check 'quits' ($p.HasExited)
$panics = @(Select-String -Path "$Data\rewind.log" -Pattern 'panic' -SimpleMatch).Count
Check "no panics in the log ($panics)" ($panics -eq 0)
