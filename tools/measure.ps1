# Starts a program, samples its CPU / memory and the GPU for N seconds, prints a summary.
# Usage: measure.ps1 -Exe path -ArgList 'cli','--quiet' -Seconds 30 -Label name
param(
    [Parameter(Mandatory)] [string]$Exe,
    [string[]]$ArgList = @(),
    [int]$Seconds = 30,
    [string]$Label = 'run'
)
$p = Start-Process -FilePath $Exe -ArgumentList $ArgList -PassThru -WindowStyle Hidden
Start-Sleep 4   # let it settle
$p.Refresh()
$t0 = $p.TotalProcessorTime
$sw = [Diagnostics.Stopwatch]::StartNew()
$gpu = @(); $enc = @(); $ws = @(); $priv = @()
for ($i = 0; $i -lt $Seconds; $i++) {
    $g = (nvidia-smi --query-gpu=utilization.gpu,utilization.encoder --format=csv,noheader,nounits) -split ','
    $gpu += [int]$g[0]; $enc += [int]$g[1]
    $p.Refresh()
    $ws += $p.WorkingSet64; $priv += $p.PrivateMemorySize64
    Start-Sleep -Milliseconds 900
}
$p.Refresh()
$cpu = ($p.TotalProcessorTime - $t0).TotalSeconds / $sw.Elapsed.TotalSeconds
if (-not $p.HasExited) { Stop-Process $p -Force }
$cores = [Environment]::ProcessorCount
"{0,-10} CPU {1,5:N2}% of all cores ({2,5:N1}% of one) | GPU {3,4:N1}% | NVENC {4,4:N1}% | RAM {5,4:N0} MB working set, {6,4:N0} MB private" -f `
    $Label, ($cpu / $cores * 100), ($cpu * 100), ($gpu | Measure-Object -Average).Average, ($enc | Measure-Object -Average).Average, `
    (($ws | Measure-Object -Maximum).Maximum / 1MB), (($priv | Measure-Object -Maximum).Maximum / 1MB)
