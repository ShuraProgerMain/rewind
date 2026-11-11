# What each NVENC option costs while recording live: NVENC load, Rewind's memory, CPU.
# Uses REWIND_ENCODER_OPTS to switch options off one group at a time.
# Usage: cost-bench.ps1 -Work <folder> [-Monitor 0] [-Seconds 15]
param([Parameter(Mandatory)] [string]$Work, [int]$Monitor = 0, [int]$Seconds = 15)
$exe = "$PSScriptRoot\..\target\release\rewind.exe"
$off = 'multipass=disabled;spatial-aq=0;temporal-aq=0;rc-lookahead=0;b_ref_mode=disabled;bf=0'
$variants = [ordered]@{
    'old (p4, nothing extra)'      = "preset=p4;$off"
    'p5'                           = "preset=p5;$off"
    'p4 + B-frames'                = 'preset=p4;multipass=disabled;spatial-aq=0;temporal-aq=0;rc-lookahead=0'
    'p4 + B + AQ'                  = 'preset=p4;multipass=disabled;rc-lookahead=0'
    'p4 + B + AQ + lookahead'      = 'preset=p4;multipass=disabled'
    'p4 + B + AQ + qres'           = 'preset=p4;rc-lookahead=0'
    'full (p5 qres B AQ lookahead)' = ''
}
foreach ($name in $variants.Keys) {
    $env:REWIND_ENCODER_OPTS = $variants[$name]
    & "$PSScriptRoot\measure.ps1" -Label $name -Seconds $Seconds -Exe $exe -ArgList @('cli', '--quiet', '--monitor', "$Monitor", '--buffer', '600', '--out', "`"$Work\cost`"")
}
Remove-Item Env:\REWIND_ENCODER_OPTS
