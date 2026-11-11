# End-to-end check of "settling into the system" and of self-update, without touching the
# real Start menu, autostart or GitHub:
#   REWIND_SYSTEM_TEST  sends the shortcut and the autostart entry to scratch places,
#   REWIND_UPDATE_FEED  replaces GitHub with a local folder holding a "release".
# Builds the program twice (the normal version, then a pretend 9.9.9) and restores the
# normal build at the end.
# Usage: update-test.ps1 -Work <empty folder>
param([Parameter(Mandatory)] [string]$Work)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$app = "$Work\app"; $feed = "$Work\feed"; $data = "$Work\data"; $sys = "$Work\system"
New-Item -ItemType Directory -Force $app, $feed, $data, $sys, "$data\out" | Out-Null
$exe = "$app\rewind.exe"
$testKey = 'HKCU:\Software\Rewind\TestRun'

function Build($version) {
    if ($version) { $env:REWIND_VERSION = $version } else { Remove-Item Env:\REWIND_VERSION -ErrorAction SilentlyContinue }
    Push-Location $root
    try { cmd /c "cargo build --release >nul 2>&1"; if ($LASTEXITCODE -ne 0) { throw "build failed ($version)" } } finally { Pop-Location }
    Remove-Item Env:\REWIND_VERSION -ErrorAction SilentlyContinue
}
function Check($name, $ok) { '{0}  {1}' -f $(if ($ok) { 'PASS' } else { 'FAIL' }), $name }
function Send($c) { Start-Process $exe -ArgumentList @('--data', "`"$data`"", '--send', "`"$c`"") -Wait }
function Js($code) { Send "ui $code"; Start-Sleep -Milliseconds 600 }
function Log { Get-Content "$data\rewind.log" -ErrorAction SilentlyContinue }
function Running { @(Get-Process rewind -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe }) }
function Release($tag, $ffmpeg, $sha) {
    $assets = 'rewind.exe', 'rewind.exe.sha256', 'update.json' | ForEach-Object {
        @{ name = $_; browser_download_url = "file:$feed\$_" }
    }
    @{ tag_name = $tag; html_url = 'https://github.com/ShuraProgerMain/rewind/releases'; assets = $assets } |
        ConvertTo-Json -Depth 4 | Out-File "$feed\latest.json" -Encoding ascii
    "{ `"ffmpeg`": `"$ffmpeg`" }" | Out-File "$feed\update.json" -Encoding ascii
    $sha | Out-File "$feed\rewind.exe.sha256" -Encoding ascii
}

# ---- two builds: the installed one and the "new release"
Build $null
Copy-Item "$root\target\release\rewind.exe" $exe -Force
$ff = Get-ChildItem "$root\deps\ffmpeg" -Directory | Select-Object -First 1
foreach ($dll in 'avcodec-62', 'avformat-62', 'avutil-60', 'swresample-6') { Copy-Item "$($ff.FullName)\bin\$dll.dll" $app -Force }
$installed = (Get-Item $exe).VersionInfo.ProductVersion
Build '9.9.9'
Copy-Item "$root\target\release\rewind.exe" "$feed\rewind.exe" -Force
$good = (Get-FileHash "$feed\rewind.exe" -Algorithm SHA256).Hash.ToLower()

('{ "output_dir": "' + ("$data\out" -replace '\\', '\\') + '", "buffer_on": false }') | Out-File "$data\settings.json" -Encoding ascii
$env:REWIND_SYSTEM_TEST = $sys; $env:REWIND_UPDATE_FEED = $feed; $env:REWIND_DEBUG = '1'
try {
    Start-Process $exe -ArgumentList @('--data', "`"$data`"", '--background')
    Start-Sleep 3
    Check "starts as version $installed" ((Log) -match "started Rewind $installed")

    # ---- settling into the system
    $lnk = "$sys\Rewind.lnk"
    $target = if (Test-Path $lnk) { (New-Object -ComObject WScript.Shell).CreateShortcut($lnk).TargetPath } else { '' }
    Check 'Start menu shortcut points to the exe' ($target -eq $exe)
    Send show; Start-Sleep 4
    Js "rewind.show('system')"
    Js "document.querySelector('[data-toggle=autostart]').click()"
    $run = (Get-ItemProperty $testKey -ErrorAction SilentlyContinue).Rewind
    Check 'autostart entry is written' ($run -eq "`"$exe`" --background")
    & "$PSScriptRoot\ui-shot.ps1" -Out "$Work\system-tab.png" -Margin 0 | Out-Null

    # ---- a release whose checksum does not match is refused
    Release 'v9.9.9' 'avcodec-62' ('0' * 64)
    Js "cmd('check_update')"; Start-Sleep 2
    Check 'bad checksum: nothing is staged' (-not (Test-Path "$exe.new"))
    Check 'bad checksum: reported in the journal' ((Log) -match 'does not match its checksum')

    # ---- a good release is downloaded and waits for the next start
    Release 'v9.9.9' 'avcodec-62' "$good  rewind.exe"
    Js "cmd('check_update')"; Start-Sleep 3
    Check 'update is downloaded next to the exe' ((Test-Path "$exe.new") -and ((Get-Content "$exe.new.version" -Raw).Trim() -eq '9.9.9'))
    & "$PSScriptRoot\ui-shot.ps1" -Out "$Work\update-ready.png" -Margin 0 | Out-Null
    $before = (Running | Select-Object -First 1).Id
    Js "cmd('restart_update')"; Start-Sleep 8
    $now = Running | Select-Object -First 1
    Check 'restarts into the new version' ($now -and $now.Id -ne $before -and ((Log) -match 'started Rewind 9\.9\.9'))
    Check 'exe on disk is the new one' ((Get-FileHash $exe -Algorithm SHA256).Hash.ToLower() -eq $good)
    Check 'no leftovers (.new, .old)' (-not (Test-Path "$exe.new") -and -not (Test-Path "$exe.old") -and -not (Test-Path "$exe.new.version"))
    $target = (New-Object -ComObject WScript.Shell).CreateShortcut($lnk).TargetPath
    Check 'shortcut and autostart survived the update' (($target -eq $exe) -and ((Get-ItemProperty $testKey).Rewind -eq "`"$exe`" --background"))

    # ---- a release that needs other FFmpeg DLLs is only announced
    Release 'v10.0.0' 'avcodec-99' "$good  rewind.exe"
    Send show; Start-Sleep 4
    Js "cmd('check_update')"; Start-Sleep 2
    Check 'different DLL set: not downloaded, user is told' ((-not (Test-Path "$exe.new")) -and ((Log) -match 'version 10\.0\.0 is out but needs the full archive'))
    Js "rewind.show('system')"
    & "$PSScriptRoot\ui-shot.ps1" -Out "$Work\update-manual.png" -Margin 0 | Out-Null

    # ---- leaving the system
    Js "cmd('remove_system')"
    Check 'removed from the system' ((-not (Test-Path $lnk)) -and ($null -eq (Get-ItemProperty $testKey -ErrorAction SilentlyContinue).Rewind))
    Send quit; Start-Sleep 3
    Check 'program exits' ((Running).Count -eq 0)
    Log | Select-String 'error|fatal|PANIC|not applied' | ForEach-Object { "LOG: $($_.Line)" }
} finally {
    Remove-Item Env:\REWIND_SYSTEM_TEST, Env:\REWIND_UPDATE_FEED, Env:\REWIND_DEBUG -ErrorAction SilentlyContinue
    Running | ForEach-Object { Stop-Process $_ -Force }
    Remove-Item $testKey -Recurse -Force -ErrorAction SilentlyContinue
    Build $null   # leave the normal version in target\
    'normal build restored: ' + (Get-Item "$root\target\release\rewind.exe").VersionInfo.ProductVersion
}
