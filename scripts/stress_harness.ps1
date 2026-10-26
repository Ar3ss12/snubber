[CmdletBinding()]
param(
    [string]$TargetProcess = "process_termination_loop",
    [int]$Kills = 10000,
    [int]$MinSleepMs = 0,
    [int]$MaxSleepMs = 0,
    [int]$PollSleepMs = 1,
    [int]$SampleEvery = 5,
    [switch]$Launch,
    [switch]$Build,
    [switch]$Aggressive,
    [switch]$RawKill,
    [int]$AmbushUs = 0,
    [switch]$HighPriority,
    [switch]$NoHighRes,
    [switch]$ArmedHold,
    [int]$MinAmbushPct = -1,
    [int]$MaxHandleGrowth = 16,
    [ValidateSet("release", "debug")]
    [string]$Profile = "release",
    [int]$StallTimeoutSec = 20,
    [int]$SupervisorWaitSec = 60,
    [double]$MaxGrowthMb = 2.0,
    [string]$ReportPath = "target/termination_report.json",
    [string]$StatsPath = "target/stress_stats.json"
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
if ($Kills -le 0) { throw "-Kills must be > 0" }
# Armed-hold workers never commit, so a low ambush rate means the killer
# is shooting during process startup instead of at an armed token.
$minAmbush = if ($MinAmbushPct -lt 0) { if ($ArmedHold) { 70 } else { 0 } } else { $MinAmbushPct }

# CI mode: GitHub Actions sets CI=true (or 1), same probe as the .sh
# harness. CI must own the supervisor: the RSS, handle-growth and exit
# gates are fed only by the launched path, so an external supervisor
# would leave every leak gate without data while the verdict still read
# PASS. The .sh flavor applies the identical rule.
$ciFlag = if ($env:CI) { $env:CI.ToLowerInvariant() } else { "" }
$inCi = ($ciFlag -eq "true" -or $ciFlag -eq "1")
if ($inCi -and -not $Launch) {
    Write-Host "[HARNESS] FAIL: CI requires -Launch: an external supervisor has no pid for the RSS/handle/exit gates, so the run would report without leak evidence" -ForegroundColor Red
    exit 1
}

function Resolve-RepoPath([string]$p) {
    if ([System.IO.Path]::IsPathRooted($p)) { return $p }
    return (Join-Path $repoRoot $p)
}

$reportFull = Resolve-RepoPath $ReportPath
$statsFull = Resolve-RepoPath $StatsPath
$pidFiles = @(
    (Join-Path $repoRoot "target/supervised_worker.pid"),
    (Join-Path $repoRoot "target/chaos_worker.pid")
)
$exeName = if ($env:OS -eq "Windows_NT") { "$TargetProcess.exe" } else { $TargetProcess }
$exe = Join-Path $repoRoot ("target/{0}/examples/{1}" -f $Profile, $exeName)
# Exact image name for managed-mode identity (ProcessName never carries the
# extension). The old `-like "*name*"` also matched unrelated images whose
# name merely contained the target string.
$exeBase = [System.IO.Path]::GetFileNameWithoutExtension($exeName)

# Creation time in universal ticks, or 0 when it cannot be read. Managed
# mode rejects a victim whose start time is not strictly newer than the
# last killed worker, exactly like the raw path's KillChecked.
function Get-StartTicks($proc) {
    if (-not $proc) { return [long]0 }
    try { return $proc.StartTime.ToUniversalTime().Ticks } catch { return [long]0 }
}
$isWindows = $env:OS -eq "Windows_NT"

$rawKiller = $null
if ($RawKill) {
    if (-not $isWindows) { throw "-RawKill is a Win32 path (OpenProcess/TerminateProcess) and is Windows-only" }
    if (-not ('SnubberHarness.Win32Killer' -as [type])) {
        Add-Type -Namespace SnubberHarness -Name Win32Killer -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("kernel32.dll", SetLastError = true)]
public static extern System.IntPtr OpenProcess(uint access, bool inherit, int pid);

[System.Runtime.InteropServices.DllImport("kernel32.dll", SetLastError = true)]
public static extern bool TerminateProcess(System.IntPtr handle, uint code);

[System.Runtime.InteropServices.DllImport("kernel32.dll", SetLastError = true)]
public static extern bool CloseHandle(System.IntPtr handle);

[System.Runtime.InteropServices.DllImport("kernel32.dll", SetLastError = true, CharSet = System.Runtime.InteropServices.CharSet.Unicode)]
public static extern bool QueryFullProcessImageNameW(System.IntPtr handle, uint flags, System.Text.StringBuilder buffer, ref uint size);

[System.Runtime.InteropServices.DllImport("kernel32.dll", SetLastError = true)]
public static extern bool GetProcessTimes(System.IntPtr handle, out long creation, out long exitTime, out long kernelTime, out long userTime);

public const uint PROCESS_TERMINATE = 0x0001;
public const uint PROCESS_QUERY_LIMITED_INFORMATION = 0x1000;

// 1 = terminated, 0 = gone/no access, 2 = foreign image / not ready,
// 3 = already killed (pid recycled or pid file not rewritten yet: creation <= lastCreation)
public static int KillChecked(int pid, string expectedExe, long lastCreation, out long creation) {
    creation = 0;
    System.IntPtr h = OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, false, pid);
    if (h == System.IntPtr.Zero) { return 0; }
    try {
        long exitTime, kernelTime, userTime;
        if (!GetProcessTimes(h, out creation, out exitTime, out kernelTime, out userTime)) { creation = 0; return 2; }
        if (creation <= lastCreation) { return 3; }
        var sb = new System.Text.StringBuilder(1024);
        uint size = 1024;
        if (!QueryFullProcessImageNameW(h, 0, sb, ref size)) { return 2; }
        string name = System.IO.Path.GetFileName(sb.ToString());
        if (!string.Equals(name, expectedExe, System.StringComparison.OrdinalIgnoreCase)) { return 2; }
        return TerminateProcess(h, 137) ? 1 : 0;
    } finally {
        CloseHandle(h);
    }
}
'@
    }
    $rawKiller = [SnubberHarness.Win32Killer]
}

$timerRes = $false
if ($isWindows -and -not $NoHighRes) {
    try {
        Add-Type -Namespace SnubberHarness -Name Timer -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("winmm.dll")]
public static extern uint timeBeginPeriod(uint uPeriod);
[System.Runtime.InteropServices.DllImport("winmm.dll")]
public static extern uint timeEndPeriod(uint uPeriod);
'@
        $timerRes = ([SnubberHarness.Timer]::timeBeginPeriod(1) -eq 0)
    } catch { $timerRes = $false }
}

try { $host.UI.RawUI.WindowTitle = "SNUBBER STRESS HARNESS" } catch {}

Write-Host "==========================================================" -ForegroundColor Cyan
Write-Host "   SNUBBER STRESS HARNESS: FORCED PROCESS TERMINATION     " -ForegroundColor Cyan
Write-Host "==========================================================" -ForegroundColor Cyan
Write-Host ("Target   : {0}  ({1} kills requested)" -f $TargetProcess, $Kills)
Write-Host ("Method   : {0}" -f $(if ($RawKill) { "raw OpenProcess + image-verify + TerminateProcess (no Get-Process, no exit wait)" } elseif ($isWindows) { "Process.Kill() = TerminateProcess" } else { "SIGKILL" }))
Write-Host ("Workload : {0}" -f $(if ($Aggressive) { "spin pacing (precise 3 ms armed / 1 ms pace)" } else { "sleep pacing (default)" }))
Write-Host ("Ambush   : {0}   min true-ambush: {1}%" -f $(if ($ArmedHold) { "armed-hold worker (pid published only after arm)" } else { "free-running worker (3 ms armed / 1 ms pace)" }), $minAmbush)
if ($RawKill) {
    Write-Host ("Ambush   : 0-{0} us spin after PID publish   Priority: {1}" -f $AmbushUs, $(if ($HighPriority) { "High" } else { "Normal" }))
}
Write-Host ("Jitter   : {0}-{1} ms   Poll: {2} ms   Sample: every {3} kills" -f $MinSleepMs, $MaxSleepMs, $PollSleepMs, $SampleEvery)
Write-Host ("Profile  : {0}   Timer: {1}" -f $Profile, $(if ($timerRes) { "winmm 1 ms request" } else { "system default" }))
Write-Host ("Mode     : {0}" -f $(if ($Launch) { "launch supervisor + kill its workers" } else { "external supervisor (kill only)" }))
Write-Host "----------------------------------------------------------" -ForegroundColor DarkGray

$sup = $null
$supOutTask = $null
$supErrTask = $null
$supOut = Join-Path $repoRoot "target/stress_supervisor.out"
$supErr = Join-Path $repoRoot "target/stress_supervisor.err"
$completed = 0
$misses = 0
$notFoundPolls = 0
$stalePolls = 0
$duplicatePolls = 0
$killRetries = 0
$lastKilledPid = 0
$lastKilledCreation = [long]0
$lastManagedStartTicks = [long]0
$supPid = 0
$missStreak = 0
$overall = New-Object System.Diagnostics.Stopwatch
$latencies = New-Object 'System.Collections.Generic.List[double]'
$harnessProc = [System.Diagnostics.Process]::GetCurrentProcess()
if ($HighPriority) {
    try { $harnessProc.PriorityClass = [System.Diagnostics.ProcessPriorityClass]::High } catch {}
}
$harnessCpuStart = 0.0

# Resource accumulators (supervisor sampled periodically, workers sampled at kill time)
$supWsPeak = 0.0; $supWsSum = 0.0; $supWsFirst = -1.0; $supWsLast = 0.0
$supPrivPeak = 0.0; $supPrivSum = 0.0; $supPrivFirst = -1.0; $supPrivLast = 0.0
$supHandlePeak = 0; $supHandleFirst = -1; $supHandleLast = 0
$supThreadPeak = 0; $supThreadSum = 0
$supSamples = 0
$supCpuTimeLast = 0.0; $supTimeLast = 0.0
$supCpuPctSum = 0.0; $supCpuPctPeak = 0.0; $supCpuPctSamples = 0
$supCpuSeconds = 0.0
$wWsPeak = 0.0; $wWsSum = 0.0; $wPrivPeak = 0.0; $wPrivSum = 0.0
$wHandlePeak = 0; $wHandleSum = 0; $wThreadPeak = 0; $wThreadSum = 0; $wSamples = 0
$peakRate = 0.0; $windowKills = 0; $windowTime = 0.0

function Sample-Supervisor {
    if (-not $script:sup) { return }
    try {
        $script:sup.Refresh()
        if ($script:sup.HasExited) { return }
        $now = $script:overall.Elapsed.TotalSeconds
        $ws = $script:sup.WorkingSet64 / 1MB
        $pv = $script:sup.PrivateMemorySize64 / 1MB
        $hc = [int]$script:sup.HandleCount
        $th = [int]($script:sup.Threads.Count)
        $cpu = $script:sup.TotalProcessorTime.TotalSeconds

        if ($script:supSamples -eq 0) {
            $script:supWsFirst = $ws
            $script:supPrivFirst = $pv
            $script:supHandleFirst = $hc
        }
        $script:supWsLast = $ws
        $script:supPrivLast = $pv
        $script:supHandleLast = $hc

        if ($ws -gt $script:supWsPeak) { $script:supWsPeak = $ws }
        if ($pv -gt $script:supPrivPeak) { $script:supPrivPeak = $pv }
        if ($hc -gt $script:supHandlePeak) { $script:supHandlePeak = $hc }
        if ($th -gt $script:supThreadPeak) { $script:supThreadPeak = $th }

        $script:supWsSum += $ws
        $script:supPrivSum += $pv
        $script:supThreadSum += $th
        $script:supSamples++

        if ($script:supTimeLast -gt 0 -and $now -gt $script:supTimeLast) {
            $pct = 100.0 * ($cpu - $script:supCpuTimeLast) / ($now - $script:supTimeLast)
            if ($pct -lt 0) { $pct = 0 }
            $script:supCpuPctSum += $pct
            $script:supCpuPctSamples++
            if ($pct -gt $script:supCpuPctPeak) { $script:supCpuPctPeak = $pct }
        }
        $script:supTimeLast = $now
        $script:supCpuTimeLast = $cpu
        $script:supCpuSeconds = $cpu
    } catch {}
}

function Sample-Worker([System.Diagnostics.Process]$p) {
    try {
        $ws = $p.WorkingSet64 / 1MB
        $pv = $p.PrivateMemorySize64 / 1MB
        $hc = [int]$p.HandleCount
        $th = [int]($p.Threads.Count)

        if ($ws -gt $script:wWsPeak) { $script:wWsPeak = $ws }
        if ($pv -gt $script:wPrivPeak) { $script:wPrivPeak = $pv }
        if ($hc -gt $script:wHandlePeak) { $script:wHandlePeak = $hc }
        if ($th -gt $script:wThreadPeak) { $script:wThreadPeak = $th }

        $script:wWsSum += $ws
        $script:wPrivSum += $pv
        $script:wHandleSum += $hc
        $script:wThreadSum += $th
        $script:wSamples++
    } catch {}
}

# Sub-millisecond spin (Stopwatch QPC), used to land kills inside the
# worker's arm/write/commit window instead of after it has settled.
function Spin-Micros([int]$us) {
    if ($us -le 0) { return }
    $freq = [System.Diagnostics.Stopwatch]::Frequency
    $deadline = [System.Diagnostics.Stopwatch]::GetTimestamp() + [int64](($us * $freq) / 1000000)
    while ([System.Diagnostics.Stopwatch]::GetTimestamp() -lt $deadline) { }
}

try {
    if ($Launch) {
        if ($Build -or -not (Test-Path $exe)) {
            Write-Host "[BUILD] cargo build --example $TargetProcess ($Profile)..." -ForegroundColor Yellow
            $cargoArgs = @("build", "--locked", "--example", $TargetProcess)
            if ($Profile -eq "release") { $cargoArgs = @("build", "--locked", "--release", "--example", $TargetProcess) }
            & cargo @cargoArgs
            if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
        }
        if (-not (Test-Path $exe)) { throw "supervisor binary not found: $exe (use -Build)" }

        foreach ($f in $pidFiles) { Remove-Item $f -ErrorAction SilentlyContinue }
        Remove-Item $reportFull -ErrorAction SilentlyContinue

        $supArgs = "--target-kills $Kills --report-path `"$ReportPath`""
        if ($Aggressive) { $supArgs = "$supArgs --spin" }
        if ($ArmedHold) { $supArgs = "$supArgs --armed-hold" }

        $psi = New-Object System.Diagnostics.ProcessStartInfo
        $psi.FileName = $exe
        $psi.Arguments = $supArgs
        $psi.WorkingDirectory = $repoRoot
        $psi.UseShellExecute = $false
        $psi.RedirectStandardOutput = $true
        $psi.RedirectStandardError = $true
        $psi.CreateNoWindow = $true
        $sup = New-Object System.Diagnostics.Process
        $sup.StartInfo = $psi
        $null = $sup.Start()
        $supOutTask = $sup.StandardOutput.ReadToEndAsync()
        $supErrTask = $sup.StandardError.ReadToEndAsync()
        $supPid = $sup.Id
        Write-Host "[LAUNCH] supervisor pid=$supPid stdout=$supOut" -ForegroundColor DarkGray

        $readyDeadline = (Get-Date).AddSeconds(10)
        while (-not (Test-Path $pidFiles[0]) -and (Get-Date) -lt $readyDeadline -and -not $sup.HasExited) {
            Start-Sleep -Milliseconds 20
        }
        if ($sup.HasExited) { throw "supervisor exited before publishing a worker pid (exit=$($sup.ExitCode))" }
    }

    $overall.Start()
    $harnessCpuStart = $harnessProc.TotalProcessorTime.TotalSeconds
    if ($Launch) { Sample-Supervisor }

    $lastSuccessAt = Get-Date
    while ($completed -lt $Kills) {
        $targetPid = 0

        foreach ($f in $pidFiles) {
            if (-not (Test-Path $f)) { continue }
            try {
                $raw = [System.IO.File]::ReadAllText($f).Trim()
                $parsed = 0
                if ([int]::TryParse($raw, [ref]$parsed) -and $parsed -gt 4 -and $parsed -ne $supPid -and $parsed -ne $PID) {
                    if ($RawKill) { $targetPid = $parsed; break }
                    $p = Get-Process -Id $parsed -ErrorAction SilentlyContinue
                    $startTicks = Get-StartTicks $p
                    if ($p -and $p.ProcessName -eq $exeBase -and $startTicks -gt $lastManagedStartTicks) {
                        $targetPid = $parsed
                        break
                    }
                }
            } catch {}
        }

        if ($targetPid -eq 0) {
            if (-not $RawKill) {
                if ($Launch -and $sup -and -not $sup.HasExited) {
                    # The supervisor owns the pid file; skip the expensive process scan
                    # in the hot respawn window and only fall back if it stays missing.
                    $missStreak++
                    if ($missStreak -ge 200) {
                        $missStreak = 0
                        $procs = @(Get-Process -Name $exeBase -ErrorAction SilentlyContinue |
                            Sort-Object StartTime -Descending)
                        foreach ($p in $procs) {
                            if ($p.Id -ne $lastKilledPid -and $p.Id -ne $supPid -and $p.Id -ne $PID `
                                -and (Get-StartTicks $p) -gt $lastManagedStartTicks) { $targetPid = $p.Id; break }
                        }
                    }
                } else {
                    $procs = @(Get-Process -Name $exeBase -ErrorAction SilentlyContinue |
                        Sort-Object StartTime -Descending)
                    foreach ($p in $procs) {
                        if ($p.Id -ne $lastKilledPid -and $p.Id -ne $supPid -and $p.Id -ne $PID `
                            -and (Get-StartTicks $p) -gt $lastManagedStartTicks) { $targetPid = $p.Id; break }
                    }
                }
            } else {
                $missStreak++
            }
        }

        if ($targetPid -eq 0) {
            if ($Launch -and $sup.HasExited) { break }
            if (((Get-Date) - $lastSuccessAt).TotalSeconds -gt $StallTimeoutSec) { break }
            $notFoundPolls++
            if ($PollSleepMs -gt 0) { Start-Sleep -Milliseconds $PollSleepMs }
            else { [System.Threading.Thread]::Sleep(0) }
            continue
        }

        $killSw = [System.Diagnostics.Stopwatch]::StartNew()
        $retry = $false
        try {
            $exited = $false
            if ($RawKill) {
                if ($AmbushUs -gt 0) {
                    Spin-Micros (Get-Random -Minimum 0 -Maximum ($AmbushUs + 1))
                }
                $created = [long]0
                $verdictCode = $rawKiller::KillChecked($targetPid, $exeName, $lastKilledCreation, [ref]$created)
                $killSw.Stop()
                $exited = ($verdictCode -eq 1)
                if ($exited) {
                    $lastKilledCreation = $created
                } elseif ($verdictCode -eq 3) {
                    $duplicatePolls++
                    $retry = $true
                } else {
                    # 2 = torn pid-file read / foreign image, 0 = gone or refused kill.
                    # Never blacklist the pid here: a live child that refused the kill
                    # must stay retryable, or the supervisor waits on it forever.
                    if ($verdictCode -eq 2) { $stalePolls++ } else { $killRetries++ }
                    $retry = $true
                }
            } else {
                $proc = Get-Process -Id $targetPid -ErrorAction SilentlyContinue
                $startTicks = Get-StartTicks $proc
                if (-not $proc -or $proc.ProcessName -ne $exeBase -or $startTicks -le $lastManagedStartTicks) {
                    # Wrong image or a recycled pid: never kill, keep retrying.
                    if ($proc) { $stalePolls++ }
                    $retry = $true
                } else {
                    Sample-Worker $proc
                    $proc.Kill()
                    $exited = $proc.WaitForExit(250)
                    $killSw.Stop()
                    if ($exited) {
                        $lastManagedStartTicks = $startTicks
                    } else {
                        $killRetries++
                        $retry = $true
                    }
                }
            }
            if ($exited) {
                $lastKilledPid = $targetPid
                $completed++
                $missStreak = 0
                $sample = if ($RawKill) { $killSw.Elapsed.TotalMilliseconds * 1000.0 } else { $killSw.Elapsed.TotalMilliseconds }
                $latencies.Add($sample)
                $lastSuccessAt = Get-Date

                if (($completed % 500) -eq 0) {
                    $nowT = $overall.Elapsed.TotalSeconds
                    $windowRate = ($completed - $windowKills) / [math]::Max($nowT - $windowTime, 0.001)
                    if ($windowRate -gt $peakRate) { $peakRate = $windowRate }
                    $windowKills = $completed
                    $windowTime = $nowT
                }

                if ($completed -le 10 -or $completed % 500 -eq 0 -or $completed -eq $Kills) {
                    $rate = $completed / [math]::Max($overall.Elapsed.TotalSeconds, 0.001)
                    $unit = if ($RawKill) { "us" } else { "ms" }
                    Write-Host ("[HARNESS] {0}/{1} | pid={2} | {3:N1} kills/s | {4:N2} {5}" -f `
                        $completed, $Kills, $targetPid, $rate, $sample, $unit) -ForegroundColor Green
                }
            } elseif ($retry) {
                # Vanished/gone/duplicate child: keep polling the same slot; the
                # supervisor rewrites the pid once the live child is actually dead.
                if ($Launch -and $sup.HasExited) { break }
                if (((Get-Date) - $lastSuccessAt).TotalSeconds -gt $StallTimeoutSec) { break }
                $notFoundPolls++
                if ($PollSleepMs -gt 0) { Start-Sleep -Milliseconds $PollSleepMs }
                else { [System.Threading.Thread]::Sleep(0) }
            } else {
                $misses++
            }
        } catch {
            $killSw.Stop()
            $misses++
            if ($Launch -and $sup.HasExited) { break }
            if (((Get-Date) - $lastSuccessAt).TotalSeconds -gt $StallTimeoutSec) { break }
            [System.Threading.Thread]::Sleep(0)
        }

        if ($SampleEvery -gt 0 -and $completed -gt 0 -and ($completed % $SampleEvery) -eq 0) {
            Sample-Supervisor
        }

        if ($MaxSleepMs -gt $MinSleepMs) {
            $jitter = Get-Random -Minimum $MinSleepMs -Maximum ($MaxSleepMs + 1)
            if ($jitter -gt 0) { Start-Sleep -Milliseconds $jitter }
        }
    }
} finally {
    if ($overall.IsRunning) { $overall.Stop() }
    if ($Launch -and $sup) {
        if (-not $sup.HasExited) {
            $null = $sup.WaitForExit($SupervisorWaitSec * 1000)
            if (-not $sup.HasExited) { try { $sup.Kill() } catch {} }
        }
        try { if ($supOutTask) { [System.IO.File]::WriteAllText($supOut, $supOutTask.GetAwaiter().GetResult()) } } catch {}
        try { if ($supErrTask) { [System.IO.File]::WriteAllText($supErr, $supErrTask.GetAwaiter().GetResult()) } } catch {}
    }
    if ($timerRes) { try { $null = [SnubberHarness.Timer]::timeEndPeriod(1) } catch {} }
}

$supExit = $null
if ($Launch -and $sup) {
    try { $supExit = $sup.ExitCode } catch { $supExit = $null }
}
$report = $null
if (Test-Path $reportFull) {
    try { $report = Get-Content -Raw $reportFull | ConvertFrom-Json } catch { $report = $null }
}

try {
$sorted = @($latencies.ToArray() | Sort-Object)
function Get-Pctl([double[]]$values, [double]$p) {
    if ($values.Count -eq 0) { return 0 }
    $idx = [int][math]::Ceiling($p * $values.Count) - 1
    if ($idx -lt 0) { $idx = 0 }
    if ($idx -ge $values.Count) { $idx = $values.Count - 1 }
    return $values[$idx]
}
$avgLat = if ($latencies.Count -gt 0) { ($latencies | Measure-Object -Average).Average } else { 0 }
$durationMs = $overall.Elapsed.TotalMilliseconds
$durationSec = [math]::Max($overall.Elapsed.TotalSeconds, 0.001)
$rate = $completed / $durationSec
$harnessProc.Refresh()
$harnessCpuSeconds = [math]::Max($harnessProc.TotalProcessorTime.TotalSeconds - $harnessCpuStart, 0)
$harnessAvgPct = 100.0 * $harnessCpuSeconds / $durationSec

$supWsAvg = 0.0; $supPrivAvg = 0.0; $supThreadAvg = 0.0; $supAvgPct = 0.0
if ($supSamples -gt 0) {
    $supWsAvg = $supWsSum / $supSamples
    $supPrivAvg = $supPrivSum / $supSamples
    $supThreadAvg = $supThreadSum / $supSamples
    $supAvgPct = 100.0 * $supCpuSeconds / $durationSec
}
$wWsAvg = 0.0; $wPrivAvg = 0.0; $wHandleAvg = 0.0; $wThreadAvg = 0.0
if ($wSamples -gt 0) {
    $wWsAvg = $wWsSum / $wSamples
    $wPrivAvg = $wPrivSum / $wSamples
    $wHandleAvg = $wHandleSum / $wSamples
    $wThreadAvg = $wThreadSum / $wSamples
}

$checks = New-Object 'System.Collections.Generic.List[object]'
$ambush = 0; $escape = 0; $stillborn = 0
# A missing or unparsable report proves nothing: the report checks below
# would never run, so the run must fail in every mode.
$checks.Add([pscustomobject]@{ Name = "report present and parsable"; Ok = [bool]$report })
if ($report) {
    if ($report.PSObject.Properties['ambush_kills']) {
        $ambush = [int]$report.ambush_kills
        $escape = [int]$report.escape_kills
        $stillborn = [int]$report.stillborn_kills
    } else {
        # Older supervisor binary: no per-class split, all rescues are ambushes.
        $ambush = [int]$report.tokens_rescued
    }
}
$ambushPct = if ($completed -gt 0) { 100.0 * $ambush / $completed } else { 0 }
if ($report) {
    $checks.Add([pscustomobject]@{ Name = "killed == requested"; Ok = ($completed -eq $Kills) })
    $checks.Add([pscustomobject]@{ Name = "reaped == kills"; Ok = ([int]$report.total_reaped -eq $completed) })
    # Parity with stress_multi_lineage.py: a report that disagrees about
    # armed-hold proves the supervisor ran a different workload.
    $checks.Add([pscustomobject]@{
        Name = "armed_hold matches"
        Ok   = ([bool]$report.armed_hold -eq [bool]$ArmedHold)
    })
    $checks.Add([pscustomobject]@{ Name = "rescue + clean == reaped"; Ok = (([int]$report.tokens_rescued + [int]$report.clean_settles) -eq [int]$report.total_reaped) })
    if ($report.PSObject.Properties['ambush_kills']) {
        $checks.Add([pscustomobject]@{ Name = "ambush + escape + stillborn == reaped"; Ok = (($ambush + $escape + $stillborn) -eq [int]$report.total_reaped) })
    }
    if ($minAmbush -gt 0) {
        $checks.Add([pscustomobject]@{ Name = ("true ambush rate {0:N1}% >= {1}%" -f $ambushPct, $minAmbush); Ok = ($ambushPct -ge $minAmbush) })
    }
    $checks.Add([pscustomobject]@{ Name = "final_generation == kills + 1"; Ok = ([int]$report.final_generation -eq $completed + 1) })
}
if ($Launch) {
    # Kept outside the report block: a wedged supervisor must fail the run
    # even when no report was written.
    $checks.Add([pscustomobject]@{ Name = "supervisor exit == 0"; Ok = ($supExit -eq 0) })
}
if ($supSamples -gt 0) {
    # A shrinking measurement is not negative growth: clamp like the
    # python tracker's growth() so sign noise cannot buy slack on the
    # leak gate.
    $privGrowth = [math]::Max(0.0, $supPrivLast - $supPrivFirst)
    $handleGrowth = [math]::Max(0, $supHandleLast - $supHandleFirst)
    $checks.Add([pscustomobject]@{
        Name = ("supervisor private growth {0:+0.00;-0.00;0.00} MB <= {1:N1} MB" -f $privGrowth, $MaxGrowthMb)
        Ok   = ($privGrowth -le $MaxGrowthMb)
    })
    $checks.Add([pscustomobject]@{
        Name = ("supervisor handle growth {0:+0;-0;0} <= {1}" -f $handleGrowth, $MaxHandleGrowth)
        Ok   = ($handleGrowth -le $MaxHandleGrowth)
    })
} elseif ($Launch) {
    # Zero sampling turns the leak gates into silent no-ops (e.g. the
    # sampler's catch{} swallowed every poll): a run with no samples
    # proves nothing and must fail, not pass.
    $checks.Add([pscustomobject]@{ Name = "sampler collected supervisor samples"; Ok = $false })
}

$failedChecks = @($checks | Where-Object { -not $_.Ok })
# One verdict formula across all three harnesses (mirrors
# stress_multi_lineage.py): a SKIP is a hole, not a pass. Checks may
# carry Ok = $null (platform-gated); a skipped check fails the verdict
# and is printed as SKIP, never silently promoted to OK.
$skippedChecks = @($checks | Where-Object { $null -eq $_.Ok })
$verdict = if ($completed -eq $Kills -and $failedChecks.Count -eq 0 -and $skippedChecks.Count -eq 0 -and $report) { "PASS" } else { "FAIL" }

Write-Host "==========================================================" -ForegroundColor Cyan
Write-Host "   STRESS RESULTS" -ForegroundColor Cyan
Write-Host "==========================================================" -ForegroundColor Cyan
Write-Host ("Kills completed : {0} / {1}" -f $completed, $Kills)
Write-Host ("Misses          : {0}   (not-found polls: {1}, stale/foreign pid polls: {2}, duplicate-pid polls: {3}, dispatch retries: {4})" -f `
    $misses, $notFoundPolls, $stalePolls, $duplicatePolls, $killRetries)
Write-Host ("Duration        : {0}" -f $overall.Elapsed.ToString("hh\:mm\:ss\.fff"))
Write-Host ("Rate            : {0:N1} kills/s avg | {1:N1} kills/s peak (500-kill window)" -f $rate, $peakRate)
$latLabel = if ($RawKill) { "Kill dispatch us" } else { "Kill latency ms " }
Write-Host ("{0} : min {1:N2} | p50 {2:N2} | p95 {3:N2} | p99 {4:N2} | p99.9 {5:N2} | max {6:N2} | avg {7:N2}" -f `
    $latLabel, (Get-Pctl $sorted 0.0), (Get-Pctl $sorted 0.50), (Get-Pctl $sorted 0.95), (Get-Pctl $sorted 0.99), `
    (Get-Pctl $sorted 0.999), (Get-Pctl $sorted 1.0), $avgLat)
if ($report) {
    $rescueRate = if ([int]$report.total_reaped -gt 0) { 100.0 * [int]$report.tokens_rescued / [int]$report.total_reaped } else { 0 }
    Write-Host ("Supervisor      : reaped={0} tokens_rescued={1} clean_settles={2} rescue_rate={3:N1}% final_gen={4} elapsed_ms={5} exit={6}" -f `
        $report.total_reaped, $report.tokens_rescued, $report.clean_settles, $rescueRate, `
        $report.final_generation, $report.elapsed_ms, $supExit)
    Write-Host ("Kill classes    : true_ambush={0} ({1:N1}%) clean_escape={2} stillborn={3} | armed_hold={4}" -f `
        $ambush, $ambushPct, $escape, $stillborn, [bool]$ArmedHold)
}
if ($supSamples -gt 0) {
    Write-Host ("Supervisor res  : ws peak {0:N2} MB avg {1:N2} MB growth {2:+0.00;-0.00;0.00} MB | private peak {3:N2} MB growth {4:+0.00;-0.00;0.00} MB | handles peak {5} growth {6:+0;-0;0} | threads peak {7}" -f `
        $supWsPeak, $supWsAvg, ($supWsLast - $supWsFirst), $supPrivPeak, ($supPrivLast - $supPrivFirst), `
        $supHandlePeak, ($supHandleLast - $supHandleFirst), $supThreadPeak)
    Write-Host ("CPU             : supervisor avg {0:N1}% peak {1:N1}% ({2:N1}s cpu) | harness avg {3:N1}% ({4:N1}s cpu)" -f `
        $supAvgPct, $supCpuPctPeak, $supCpuSeconds, $harnessAvgPct, $harnessCpuSeconds)
}
if ($wSamples -gt 0) {
    Write-Host ("Worker res      : ws peak {0:N2} MB avg {1:N2} MB | private peak {2:N2} MB avg {3:N2} MB | handles peak {4} avg {5:N1} | threads peak {6} avg {7:N1} (samples {8})" -f `
        $wWsPeak, $wWsAvg, $wPrivPeak, $wPrivAvg, $wHandlePeak, $wHandleAvg, $wThreadPeak, $wThreadAvg, $wSamples)
}
if ($supSamples -gt 0) {
    Write-Host ("Harness CPU     : {0:N1}% avg ({1:N1}s) | timer resolution: {2}" -f $harnessAvgPct, $harnessCpuSeconds, $(if ($timerRes) { "winmm 1 ms" } else { "default" }))
}
foreach ($c in $checks) {
    $state = if ($null -ne $c.Ok) { if ($c.Ok) { "OK  " } else { "FAIL" } } else { "SKIP" }
    $color = if ($null -ne $c.Ok -and $c.Ok) { "Green" } elseif ($null -eq $c.Ok) { "Yellow" } else { "Red" }
    Write-Host ("Check           : {0} {1}" -f $state, $c.Name) -ForegroundColor $color
}
if ($skippedChecks.Count -gt 0) {
    Write-Host ("[skip accounting] {0} check(s) skipped: platform-gated" -f $skippedChecks.Count) -ForegroundColor Yellow
}
$verdictColor = if ($verdict -like "PASS*") { "Green" } else { "Red" }
Write-Host ("Verdict         : {0}" -f $verdict) -ForegroundColor $verdictColor
Write-Host ("Stats JSON      : {0}" -f $statsFull)
Write-Host ("Report JSON     : {0}" -f $reportFull)
if ($Launch -and $supExit -ne 0) {
    Write-Host "--- supervisor stderr (tail) ---" -ForegroundColor DarkYellow
    if (Test-Path $supErr) { Get-Content $supErr -Tail 10 | Write-Host }
}
Write-Host "==========================================================" -ForegroundColor $verdictColor

$mode = if ($Launch) { "launch" } else { "external" }
$workload = if ($Aggressive) { "spin" } else { "sleep" }
$killMode = if ($RawKill) { "raw" } else { "managed" }
$latencyKind = if ($RawKill) { "dispatch" } else { "exit" }
$latStats = [ordered]@{
    min   = [math]::Round((Get-Pctl $sorted 0.0), 3)
    p50   = [math]::Round((Get-Pctl $sorted 0.50), 3)
    p95   = [math]::Round((Get-Pctl $sorted 0.95), 3)
    p99   = [math]::Round((Get-Pctl $sorted 0.99), 3)
    p999  = [math]::Round((Get-Pctl $sorted 0.999), 3)
    max   = [math]::Round((Get-Pctl $sorted 1.0), 3)
    avg   = [math]::Round($avgLat, 3)
    count = $latencies.Count
}
$latMsJson = $null
$latUsJson = $null
if ($RawKill) { $latUsJson = $latStats } else { $latMsJson = $latStats }
$supRes = $null
if ($supSamples -gt 0) {
    $supRes = [ordered]@{
        samples        = $supSamples
        working_set_mb = [ordered]@{
            peak   = [math]::Round($supWsPeak, 3)
            avg    = [math]::Round($supWsAvg, 3)
            first  = [math]::Round($supWsFirst, 3)
            last   = [math]::Round($supWsLast, 3)
            growth = [math]::Round($supWsLast - $supWsFirst, 3)
        }
        private_mb     = [ordered]@{
            peak   = [math]::Round($supPrivPeak, 3)
            avg    = [math]::Round($supPrivAvg, 3)
            first  = [math]::Round($supPrivFirst, 3)
            last   = [math]::Round($supPrivLast, 3)
            growth = [math]::Round($supPrivLast - $supPrivFirst, 3)
        }
        handles        = [ordered]@{
            peak   = $supHandlePeak
            first  = $supHandleFirst
            last   = $supHandleLast
            growth = $supHandleLast - $supHandleFirst
        }
        threads        = [ordered]@{
            peak = $supThreadPeak
            avg  = [math]::Round($supThreadAvg, 2)
        }
        cpu_percent    = [ordered]@{
            avg  = [math]::Round($supAvgPct, 2)
            peak = [math]::Round($supCpuPctPeak, 2)
        }
        cpu_seconds    = [math]::Round($supCpuSeconds, 2)
    }
}
$workerRes = $null
if ($wSamples -gt 0) {
    $workerRes = [ordered]@{
        samples        = $wSamples
        working_set_mb = [ordered]@{
            peak = [math]::Round($wWsPeak, 3)
            avg  = [math]::Round($wWsAvg, 3)
        }
        private_mb     = [ordered]@{
            peak = [math]::Round($wPrivPeak, 3)
            avg  = [math]::Round($wPrivAvg, 3)
        }
        handles        = [ordered]@{
            peak = $wHandlePeak
            avg  = [math]::Round($wHandleAvg, 2)
        }
        threads        = [ordered]@{
            peak = $wThreadPeak
            avg  = [math]::Round($wThreadAvg, 2)
        }
    }
}

$stats = [ordered]@{
    timestamp_utc      = (Get-Date).ToUniversalTime().ToString("o")
    target_process     = $TargetProcess
    profile            = $Profile
    mode               = $mode
    workload           = $workload
    kill_mode          = $killMode
    ambush_us          = $AmbushUs
    timer_resolution   = $timerRes
    kills_requested    = $Kills
    kills_completed    = $completed
    max_growth_mb      = $MaxGrowthMb
    max_handle_growth  = $MaxHandleGrowth
    armed_hold         = [bool]$ArmedHold
    min_ambush_pct     = $minAmbush
    ambush_kills       = $ambush
    escape_kills       = $escape
    stillborn_kills    = $stillborn
    ambush_pct         = [math]::Round($ambushPct, 1)
    misses             = $misses
    not_found_polls    = $notFoundPolls
    stale_pid_polls    = $stalePolls
    duplicate_pid_polls = $duplicatePolls
    dispatch_retries   = $killRetries
    duration_ms        = [math]::Round($durationMs, 1)
    rate_per_sec       = [math]::Round($rate, 1)
    peak_rate_per_sec  = [math]::Round($peakRate, 1)
    latency_kind       = $latencyKind
    kill_latency_ms    = $latMsJson
    dispatch_us        = $latUsJson
    supervisor         = $report
    supervisor_exit    = $supExit
    resources          = [ordered]@{
        supervisor = $supRes
        workers    = $workerRes
        harness    = [ordered]@{
            cpu_percent_avg = [math]::Round($harnessAvgPct, 2)
            cpu_seconds     = [math]::Round($harnessCpuSeconds, 2)
        }
    }
    checks             = $checks.ToArray()
    verdict            = $verdict
}
$statsDir = Split-Path -Parent $statsFull
if ($statsDir -and -not (Test-Path $statsDir)) { New-Item -ItemType Directory -Path $statsDir | Out-Null }
$stats | ConvertTo-Json -Depth 6 | Set-Content -Encoding UTF8 $statsFull
} catch {
    Write-Host ("[REPORT ERROR] {0}" -f $_.Exception.Message) -ForegroundColor Red
    Write-Host $_.InvocationInfo.PositionMessage
    exit 1
}

if ($verdict -like "PASS*") { exit 0 } else { exit 1 }
