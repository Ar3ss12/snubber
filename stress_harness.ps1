param (
    [string]$TargetProcess = "chaos_satellite",
    [int]$Kills = 10000,
    [int]$MinSleepMs = 0,
    [int]$MaxSleepMs = 4
)

$host.UI.RawUI.WindowTitle = "SNUBBER CHAOS SNIPER (TERMINAL 2)"
Write-Host "==========================================================" -ForegroundColor Red
Write-Host "   SNUBBER CHAOS SNIPER: HIGH-FREQUENCY TERMINATEPROCESS  " -ForegroundColor Red
Write-Host "==========================================================" -ForegroundColor Red
Write-Host "Ціль: розстріляти воркерів $Kills разів (до першого збою)" -ForegroundColor Yellow
Write-Host "Метод: Stop-Process -Id <pid> -Force (Windows TerminateProcess)" -ForegroundColor Yellow
Write-Host "Затримка: $MinSleepMs-$MaxSleepMs ms (максимальна кулеметна черга)" -ForegroundColor Yellow
Write-Host "----------------------------------------------------------" -ForegroundColor DarkGray

$pidFile = "target/chaos_worker.pid"
$executed = 0
$lastKilledPid = 0
$sw = [System.Diagnostics.Stopwatch]::StartNew()

while ($executed -lt $Kills) {
    $targetPid = 0

    # 1. Швидкий пошук через опублікований PID-файл (sub-millisecond)
    if (Test-Path $pidFile) {
        try {
            $raw = [System.IO.File]::ReadAllText($pidFile).Trim()
            $parsed = 0
            if ([int]::TryParse($raw, [ref]$parsed) -and $parsed -ne $lastKilledPid) {
                # Перевіримо, чи процес ще живий
                $p = Get-Process -Id $parsed -ErrorAction SilentlyContinue
                if ($p -and $p.ProcessName -like "*$TargetProcess*") {
                    $targetPid = $parsed
                }
            }
        } catch {}
    }

    # 2. Фолбек: прямий пошук процесу за іменем
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
            # ВДАР КУВАЛДОЮ: миттєве стирання процесу на рівні ядра Windows
            Stop-Process -Id $targetPid -Force -ErrorAction Stop
            $lastKilledPid = $targetPid
            $executed++

            # Регулярна телеметрія снайпера
            if ($executed % 100 -eq 0 -or $executed -eq $Kills -or $executed -le 10) {
                $rate = [math]::Round($executed / [math]::Max($sw.Elapsed.TotalSeconds, 0.001), 1)
                Write-Host "[CHAOS SNIPER] Убито: $executed / $Kills | Розстріляно PID: $targetPid | Швидкість: $rate kills/sec" -ForegroundColor Cyan
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
Write-Host "   СНАЙПЕР ФІНІШУВАВ: $executed успішних системних вбивств!  " -ForegroundColor Green
Write-Host "   Час: $($sw.Elapsed.ToString())                         " -ForegroundColor Green
Write-Host "==========================================================" -ForegroundColor Green
