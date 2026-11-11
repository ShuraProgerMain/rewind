# Builds the release exe and puts it, with the FFmpeg DLLs it needs, into the test folder.
# The test folder gets its own `data` folder, so its settings and journal stay separate
# from an installed copy.
param([string]$Dest = 'E:\tools\Rewind test')
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
Push-Location $root
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw 'build failed' }
    New-Item -ItemType Directory -Force $Dest, "$Dest\data" | Out-Null
    # Let a running test copy finish its files and release the exe.
    $exe = Join-Path $Dest 'rewind.exe'
    if (Test-Path $exe) {
        Start-Process $exe -ArgumentList '--send', 'quit' -Wait
        Start-Sleep -Milliseconds 800
    }
    Copy-Item "$root\target\release\rewind.exe" $Dest -Force
    $ff = Get-ChildItem "$root\deps\ffmpeg" -Directory | Select-Object -First 1
    foreach ($dll in 'avcodec-62', 'avformat-62', 'avutil-60', 'swresample-6') {
        Copy-Item "$($ff.FullName)\bin\$dll.dll" $Dest -Force
    }
    "test build ready: $Dest"
} finally {
    Pop-Location
}
