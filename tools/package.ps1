# Builds Rewind and assembles everything a release needs:
#   dist\Rewind\                      the program as the user gets it (exe + FFmpeg DLLs + licences)
#   dist\release\Rewind-<ver>-win64.zip   the same folder zipped: the first download
#   dist\release\rewind.exe              the exe alone: what a running copy downloads to update itself
#   dist\release\rewind.exe.sha256       its checksum, verified before the update is accepted
#   dist\release\update.json             which FFmpeg DLLs that exe needs
# Run locally to look at the result, or by the release workflow on GitHub.
param([string]$Out)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
if (-not $Out) { $Out = Join-Path $root 'dist' }

Push-Location $root
try {
    cmd /c "cargo build --release 2>&1"
    if ($LASTEXITCODE -ne 0) { throw 'build failed' }
} finally { Pop-Location }

$exe = Join-Path $root 'target\release\rewind.exe'
$version = (Get-Item $exe).VersionInfo.ProductVersion
# The FFmpeg folder: the one the build used (FFMPEG_DIR on the build server), else deps\.
$ffmpeg = if ($env:FFMPEG_DIR) { $env:FFMPEG_DIR } else { (Get-ChildItem (Join-Path $root 'deps\ffmpeg') -Directory | Select-Object -First 1).FullName }
$codec = Get-ChildItem (Join-Path $ffmpeg 'bin') -Filter 'avcodec-*.dll' | Select-Object -First 1
if (-not $codec) { throw "no FFmpeg DLLs in $ffmpeg\bin" }
$set = $codec.BaseName   # e.g. avcodec-62: the exe and the DLLs must agree on it
$built = (Select-String -Path (Join-Path $root 'src\updater.rs') -Pattern 'FFMPEG_SET: &str = "(.+)"').Matches[0].Groups[1].Value
if ($set -ne $built) { throw "FFmpeg DLLs are $set but the program expects $built (update FFMPEG_SET in src\updater.rs)" }

if (Test-Path $Out) { Get-ChildItem $Out -Recurse -File | Remove-Item -Force }
$app = Join-Path $Out 'Rewind'
$release = Join-Path $Out 'release'
New-Item -ItemType Directory -Force $app, (Join-Path $app 'licenses'), $release | Out-Null

Copy-Item $exe $app
foreach ($name in 'avcodec', 'avformat', 'avutil', 'swresample') {
    Get-ChildItem (Join-Path $ffmpeg 'bin') -Filter "$name-*.dll" | Copy-Item -Destination $app
}
Copy-Item (Join-Path $root 'LICENSE') (Join-Path $app 'licenses\Rewind-MIT.txt')
Copy-Item (Join-Path $ffmpeg 'LICENSE.txt') (Join-Path $app 'licenses\FFmpeg-LGPL.txt')
Copy-Item (Join-Path $root 'ui\OFL.txt') (Join-Path $app 'licenses\Inter-OFL.txt')
@'
Rewind is distributed under the MIT licence (Rewind-MIT.txt).

It uses these components, which keep their own licences:

FFmpeg (avcodec, avformat, avutil, swresample DLLs) - LGPL v2.1 or later, see FFmpeg-LGPL.txt.
  The DLLs are unmodified builds from https://github.com/BtbN/FFmpeg-Builds (lgpl-shared).
  Source code: https://ffmpeg.org. You may replace the DLLs with your own build of the same version.

Inter font - SIL Open Font License 1.1, see Inter-OFL.txt. https://rsms.me/inter/
'@ | Out-File (Join-Path $app 'licenses\README.txt') -Encoding ascii

$zip = Join-Path $release "Rewind-$version-win64.zip"
Compress-Archive -Path $app -DestinationPath $zip -Force
Copy-Item $exe $release
(Get-FileHash (Join-Path $release 'rewind.exe') -Algorithm SHA256).Hash.ToLower() | Out-File (Join-Path $release 'rewind.exe.sha256') -Encoding ascii
"{ `"ffmpeg`": `"$set`" }" | Out-File (Join-Path $release 'update.json') -Encoding ascii

"Rewind $version packaged (FFmpeg set $set):"
Get-ChildItem $release | ForEach-Object { '  {0,-34} {1,8:N1} MB' -f $_.Name, ($_.Length / 1MB) }
