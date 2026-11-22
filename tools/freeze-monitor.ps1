# Freeze monitor: every few seconds appends one CSV row with what the system is doing
# (disk, paging, CPU, DPC, GPU, VibeZen and its agents, the busiest processes), so after
# a whole-system freeze the log shows what was saturated. Big gaps in `gap_s` mean the
# monitor itself was stalled - that is the freeze.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File tools\freeze-monitor.ps1
#
# Log: %LOCALAPPDATA%\VibeZen\freeze-monitor.csv (rotated to .old.csv at 20 MB).

param([int]$Every = 5)

$dir = Join-Path $env:LOCALAPPDATA 'VibeZen'
New-Item -ItemType Directory -Force $dir | Out-Null
$log = Join-Path $dir 'freeze-monitor.csv'

# one monitor at a time
$me = $PID
Get-CimInstance Win32_Process -Filter "Name='powershell.exe'" |
    Where-Object { $_.ProcessId -ne $me -and $_.CommandLine -match 'freeze-monitor\.ps1' } |
    ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }

if ((Test-Path $log) -and (Get-Item $log).Length -gt 20MB) {
    Move-Item $log (Join-Path $dir 'freeze-monitor.old.csv') -Force
}
$cols = 'time,gap_s,cpu_pct,dpc_pct,disk_busy_pct,disk_queue,disk_read_mbs,disk_write_mbs,hard_faults_s,avail_mb,gpu_max_pct,' +
        'vz_cpu_pct,vz_ws_mb,vz_handles,vz_io_mbs,vz_responding,agents,top_io,top_cpu'
if (-not (Test-Path $log)) { [IO.File]::AppendAllText($log, "$cols`r`n") }
[IO.File]::AppendAllText($log, "# monitor started $(Get-Date -Format s)`r`n")

$paths = @(
    '\Processor(_Total)\% Processor Time',
    '\Processor(_Total)\% DPC Time',
    '\PhysicalDisk(_Total)\% Idle Time',
    '\PhysicalDisk(_Total)\Current Disk Queue Length',
    '\PhysicalDisk(_Total)\Disk Read Bytes/sec',
    '\PhysicalDisk(_Total)\Disk Write Bytes/sec',
    '\Memory\Pages Input/sec',
    '\Memory\Available MBytes',
    '\Process(*)\IO Data Bytes/sec',
    '\Process(*)\% Processor Time',
    '\GPU Engine(*)\Utilization Percentage'
)
$cores = [Environment]::ProcessorCount
$last = Get-Date

while ($true) {
    $now = Get-Date
    $gap = ($now - $last).TotalSeconds
    $last = $now
    try {
        $s = (Get-Counter -Counter $paths -ErrorAction SilentlyContinue).CounterSamples
        $one = { param($m) ($s | Where-Object Path -like "*$m" | Select-Object -First 1).CookedValue }
        $io = $s | Where-Object { $_.Path -like '*\io data bytes/sec' -and $_.InstanceName -notin '_total', 'idle' }
        $cpu = $s | Where-Object { $_.Path -like '*\% processor time' -and $_.Path -like '*\process(*' -and $_.InstanceName -notin '_total', 'idle' }
        $topIo = $io | Sort-Object CookedValue -Descending | Select-Object -First 3 | ForEach-Object { '{0}={1:N1}MB/s' -f $_.InstanceName, ($_.CookedValue / 1MB) }
        $topCpu = $cpu | Sort-Object CookedValue -Descending | Select-Object -First 3 | ForEach-Object { '{0}={1:N0}%' -f $_.InstanceName, ($_.CookedValue / $cores) }
        $gpu = ($s | Where-Object Path -like '*utilization percentage' | Measure-Object CookedValue -Maximum).Maximum
        $vzIo = ($io | Where-Object InstanceName -eq 'vibezen' | Select-Object -First 1).CookedValue
        $vzCpu = ($cpu | Where-Object InstanceName -eq 'vibezen' | Select-Object -First 1).CookedValue

        $vz = Get-Process vibezen -ErrorAction SilentlyContinue | Select-Object -First 1
        $agents = 0
        if ($vz) {
            $agents = @(Get-CimInstance Win32_Process -Filter "ParentProcessId=$($vz.Id)").Count
        }
        $row = @(
            $now.ToString('s'), ('{0:F0}' -f $gap),
            ('{0:F0}' -f (& $one '% processor time')), ('{0:F1}' -f (& $one '% dpc time')),
            ('{0:F0}' -f (100 - (& $one '% idle time'))), ('{0:F1}' -f (& $one 'current disk queue length')),
            ('{0:F1}' -f ((& $one 'disk read bytes/sec') / 1MB)), ('{0:F1}' -f ((& $one 'disk write bytes/sec') / 1MB)),
            ('{0:F0}' -f (& $one 'pages input/sec')), ('{0:F0}' -f (& $one 'available mbytes')), ('{0:F0}' -f $gpu),
            ('{0:F0}' -f ($vzCpu / $cores)), $(if ($vz) { [int]($vz.WorkingSet64 / 1MB) } else { '' }), $(if ($vz) { $vz.HandleCount } else { '' }),
            ('{0:F1}' -f ($vzIo / 1MB)), $(if ($vz) { $vz.Responding } else { '' }), $agents,
            ('"' + ($topIo -join ' ') + '"'), ('"' + ($topCpu -join ' ') + '"')
        ) -replace ',', '.' # decimal commas of a localized format would break the CSV
        # (the quoted columns hold no commas: their separators are spaces)
        [IO.File]::AppendAllText($log, (($row -join ',') + "`r`n"))
    } catch {
        [IO.File]::AppendAllText($log, "# $(Get-Date -Format s) error: $($_.Exception.Message -replace '[\r\n]', ' ')`r`n")
    }
    Start-Sleep -Seconds $Every
}
