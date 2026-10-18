param (
    [string]$TargetProcess = "process_termination_loop",
    [int]$Kills = 10000,
    [int]$MinSleepMs = 0,
    [int]$MaxSleepMs = 4
)

$host.UI.RawUI.WindowTitle = "SNUBBER STRESS HARNESS"
Write-Host "==========================================================" -ForegroundColor Cyan
Write-Host "   SNUBBER STRESS HARNESS: FORCED PROCESS TERMINATION     " -ForegroundColor Cyan
Write-Host "==========================================================" -ForegroundColor Cyan
Write-Host "Target: $TargetProcess ($Kills termination cycles)"
Write-Host "Method: Stop-Process -Force (Windows TerminateProcess API)"
Write-Host "Interval: $MinSleepMs-$MaxSleepMs ms"
Write-Host "----------------------------------------------------------" -ForegroundColor DarkGray

$pidFile = "target/chaos_worker.pid"
$executed = 0
$lastKilledPid = 0
$sw = [System.Diagnostics.Stopwatch]::StartNew()

while ($executed -lt $Kills) {
    $targetPid = 0

    # 1. Fast lookup via published PID file
    if (Test-Path $pidFile) {
        try {
            $raw = [System.IO.File]::ReadAllText($pidFile).Trim()
            $parsed = 0
            if ([int]::TryParse($raw, [ref]$parsed) -and $parsed -ne $lastKilledPid) {
                $p = Get-Process -Id $parsed -ErrorAction SilentlyContinue
                if ($p -and $p.ProcessName -like "*$TargetProcess*") {
                    $targetPid = $parsed
                }
            }
        } catch {}
    }

    # 2. Fallback: query process by executable name
    if ($targetPid -eq 0) {
        $procs = Get-Process -Name $TargetProcess -ErrorAction SilentlyContinue
        if ($procs -and $procs.Count -ge 2) {
            $candidate = $procs | Sort-Object StartTime -Descending | Select-Object -First 1
            if ($candidate.Id -ne $lastKilledPid) {
                $targetPid = $candidate.Id
            }
        }
    }

    if ($targetPid -ne 0) {
        try {
            # Immediate process termination via OS kernel
            Stop-Process -Id $targetPid -Force -ErrorAction Stop
            $lastKilledPid = $targetPid
            $executed++

            if ($executed % 500 -eq 0 -or $executed -eq $Kills -or $executed -le 10) {
                $rate = [math]::Round($executed / [math]::Max($sw.Elapsed.TotalSeconds, 0.001), 1)
                Write-Host "[HARNESS] Progress: $executed / $Kills | Terminated PID: $targetPid | Rate: $rate terminations/sec" -ForegroundColor Green
            }
        } catch {}
    } else {
        [System.Threading.Thread]::Sleep(1)
    }

    if ($MaxSleepMs -gt 0) {
        $jitter = Get-Random -Minimum $MinSleepMs -Maximum $MaxSleepMs
        if ($jitter -gt 0) {
            [System.Threading.Thread]::Sleep($jitter)
        }
    }
}

$sw.Stop()
Write-Host "==========================================================" -ForegroundColor Green
Write-Host "   HARNESS COMPLETE: $executed terminations verified cleanly. " -ForegroundColor Green
Write-Host "   Duration: $($sw.Elapsed.ToString())                       " -ForegroundColor Green
Write-Host "==========================================================" -ForegroundColor Green
