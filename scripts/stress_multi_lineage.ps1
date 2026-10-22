#requires -Version 5.1
<#
SNUBBER MULTI-LINEAGE STORM HARNESS (Windows, raw Win32 kill path)

N independent supervisor processes, each with its own lineage:
  lane_i : own supervisor pid, own worker pid file, own pipe, own report.

Isolation without touching Rust: examples/process_termination_loop.rs writes
the worker pid to the RELATIVE path `target/supervised_worker.pid`, so every
lane runs in its own working directory and gets a private pid file:
  target/multi_lineage/lane_<i>/target/supervised_worker.pid

This measures cross-lineage contention (scheduler, handles, pipe drain under
parallel death storms), not the single-lineage reflex gate. The single-lineage
gate stays frozen in stress_harness.ps1.

Usage:
  powershell -ExecutionPolicy Bypass -File scripts/stress_multi_lineage.ps1 -Lines 4 -KillsPerLine 2500 -Aggressive

Exit code 0 = PASS, 1 = FAIL.
#>
[CmdletBinding()]
param(
    [int]$Lines = 4,
    [int]$KillsPerLine = 2500,
    [int]$PollSleepMs = 1,
    [int]$StallTimeoutSec = 20,
    [int]$SupervisorWaitSec = 60,
    [double]$MaxGrowthMb = 2.0,
    [switch]$Aggressive,
    [switch]$Build,
    [ValidateSet("release", "debug")]
    [string]$Profile = "release",
    [string]$TargetProcess = "process_termination_loop",
    [string]$LaneRoot = "target/multi_lineage",
    [string]$ReportPath = "target/multi_lineage_report.json",
    [string]$StatsPath = "target/multi_lineage_stats.json"
)

$ErrorActionPreference = "Stop"
if ($env:OS -ne "Windows_NT") { throw "Multi-lineage storm uses the raw Win32 kill path and is Windows-only" }
if ($Lines -lt 1 -or $Lines -gt 32) { throw "-Lines must be in 1..32" }
if ($KillsPerLine -le 0) { throw "-KillsPerLine must be > 0" }

$repoRoot = Split-Path -Parent $PSScriptRoot
function Resolve-RepoPath([string]$p) {
    if ([System.IO.Path]::IsPathRooted($p)) { return $p }
    return (Join-Path $repoRoot $p)
}
$laneRootFull = Resolve-RepoPath $LaneRoot
$reportFull = Resolve-RepoPath $ReportPath
$statsFull = Resolve-RepoPath $StatsPath
if (-not $laneRootFull.StartsWith((Join-Path $repoRoot "target"), [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "-LaneRoot must live under target/ (refusing to clean: $laneRootFull)"
}
$exe = Join-Path $repoRoot ("target/{0}/examples/{1}.exe" -f $Profile, $TargetProcess)
$exeName = "$TargetProcess.exe"
$totalTarget = $Lines * $KillsPerLine

if ($Build -or -not (Test-Path $exe)) {
    Write-Host "[BUILD] cargo build --example $TargetProcess ($Profile)..." -ForegroundColor Yellow
    $cargoArgs = @("build", "--example", $TargetProcess)
    if ($Profile -eq "release") { $cargoArgs = @("build", "--release", "--example", $TargetProcess) }
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
}
if (-not (Test-Path $exe)) { throw "supervisor binary not found: $exe (use -Build)" }

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
// 3 = already killed (pid recycled or pid file not rewritten: creation <= lastCreation)
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
$killer = [SnubberHarness.Win32Killer]

$timerRes = $false
try {
    Add-Type -Namespace SnubberHarness -Name Timer -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("winmm.dll")]
public static extern uint timeBeginPeriod(uint uPeriod);
[System.Runtime.InteropServices.DllImport("winmm.dll")]
public static extern uint timeEndPeriod(uint uPeriod);
'@
    $timerRes = ([SnubberHarness.Timer]::timeBeginPeriod(1) -eq 0)
} catch { $timerRes = $false }

try { $host.UI.RawUI.WindowTitle = "SNUBBER MULTI-LINEAGE STORM ($Lines lanes)" } catch {}

Write-Host "==========================================================" -ForegroundColor Cyan
Write-Host "   SNUBBER MULTI-LINEAGE STORM: N INDEPENDENT LINEAGES   " -ForegroundColor Cyan
Write-Host "==========================================================" -ForegroundColor Cyan
Write-Host ("Lines     : {0}   kills/lane: {1}   total: {2}" -f $Lines, $KillsPerLine, $totalTarget)
Write-Host ("Model     : {0} supervisor processes, one pipe each, separate pid files" -f $Lines)
Write-Host ("Workload  : {0}" -f $(if ($Aggressive) { "spin pacing (3 ms armed / 1 ms pace)" } else { "sleep pacing" }))
Write-Host ("Kill path : raw OpenProcess + image verify + creation-time identity + TerminateProcess, {0} runspace killer(s)" -f $Lines)
Write-Host ("Timer     : {0}" -f $(if ($timerRes) { "winmm 1 ms request" } else { "system default" }))
Write-Host "----------------------------------------------------------" -ForegroundColor DarkGray

# Clean slate for this run (lane root is generated state under target/)
if (Test-Path $laneRootFull) { Remove-Item -Recurse -Force $laneRootFull }
New-Item -ItemType Directory -Force $laneRootFull | Out-Null
Remove-Item $reportFull, $statsFull -ErrorAction SilentlyContinue

$lanes = New-Object 'System.Collections.Generic.List[object]'
for ($i = 0; $i -lt $Lines; $i++) {
    $laneDir = Join-Path $laneRootFull ("lane_{0}" -f $i)
    $laneTarget = Join-Path $laneDir "target"
    New-Item -ItemType Directory -Force $laneTarget | Out-Null
    $lanes.Add([pscustomobject]@{
        Index     = $i
        Dir       = $laneDir
        PidFile   = (Join-Path $laneTarget "supervised_worker.pid")
        Report    = (Join-Path $laneDir "termination_report.json")
        OutFile   = (Join-Path $laneDir "supervisor.out")
        ErrFile   = (Join-Path $laneDir "supervisor.err")
        Proc      = $null
        OutTask   = $null
        ErrTask   = $null
        Exit      = $null
        Kill      = $null
        ReportObj = $null
        WsPeak    = 0.0; WsFirst = -1.0; WsLast = 0.0
        PrivPeak  = 0.0; PrivFirst = -1.0; PrivLast = 0.0
        HandlePeak = 0; HandleFirst = -1; HandleLast = 0
        ThreadPeak = 0; Samples = 0
    })
}

$lanesFinalized = $false
try {
    foreach ($lane in $lanes) {
        $supArgs = "--target-kills $KillsPerLine --report-path `"$($lane.Report)`""
        if ($Aggressive) { $supArgs = "$supArgs --spin" }
        $psi = New-Object System.Diagnostics.ProcessStartInfo
        $psi.FileName = $exe
        $psi.Arguments = $supArgs
        $psi.WorkingDirectory = $lane.Dir
        $psi.UseShellExecute = $false
        $psi.RedirectStandardOutput = $true
        $psi.RedirectStandardError = $true
        $psi.CreateNoWindow = $true
        $p = New-Object System.Diagnostics.Process
        $p.StartInfo = $psi
        $null = $p.Start()
        $lane.Proc = $p
        $lane.OutTask = $p.StandardOutput.ReadToEndAsync()
        $lane.ErrTask = $p.StandardError.ReadToEndAsync()
        Write-Host ("[LAUNCH] lane={0} supervisor_pid={1} dir={2}" -f $lane.Index, $p.Id, $lane.Dir) -ForegroundColor DarkGray
    }

    $readyDeadline = (Get-Date).AddSeconds(10)
    foreach ($lane in $lanes) {
        while (-not (Test-Path $lane.PidFile) -and (Get-Date) -lt $readyDeadline -and -not $lane.Proc.HasExited) {
            Start-Sleep -Milliseconds 20
        }
        if ($lane.Proc.HasExited) {
            throw ("lane {0} supervisor exited before publishing a worker pid (exit={1})" -f $lane.Index, $lane.Proc.ExitCode)
        }
    }

    $shared = [hashtable]::Synchronized(@{})
    foreach ($lane in $lanes) { $shared[$lane.Index] = 0 }
    # Every lane runs the same exe; a recycled pid can point at another lane's
    # supervisor. Killers must never terminate a supervisor.
    $supSet = [hashtable]::Synchronized(@{})
    foreach ($lane in $lanes) { $supSet[[int]$lane.Proc.Id] = $true }

    $killerScript = {
        param(
            [int]$Lane,
            [string]$PidFile,
            [int]$SupPid,
            [string]$ExpectedExe,
            [int]$StallSec,
            [int]$PollSleepMs,
            [hashtable]$Shared,
            [hashtable]$SupSet
        )
        $completed = 0
        $misses = 0
        $stale = 0
        $duplicates = 0
        $retries = 0
        $stalled = $false
        $lastCreation = [long]0
        $lastSuccess = Get-Date
        $lastSupCheck = Get-Date
        $lat = New-Object 'System.Collections.Generic.List[double]'
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        try {
            while ($true) {
                if (((Get-Date) - $lastSupCheck).TotalMilliseconds -ge 250) {
                    $lastSupCheck = Get-Date
                    if (-not (Get-Process -Id $SupPid -ErrorAction SilentlyContinue)) { break }
                }

                $targetPid = 0
                try {
                    if (Test-Path $PidFile) {
                        # Share-all read: ReadAllText opens with FileShare.Read and
                        # makes the supervisor's fs::write fail with a sharing
                        # violation; the example ignores that error, so the pid
                        # file would keep a dead pid and the lane would wedge.
                        $fs = [System.IO.File]::Open($PidFile, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, ([System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete))
                        try {
                            $sr = New-Object System.IO.StreamReader($fs)
                            try { $raw = $sr.ReadToEnd().Trim() } finally { $sr.Dispose() }
                        } finally { $fs.Dispose() }
                        $parsed = 0
                        if ([int]::TryParse($raw, [ref]$parsed) -and $parsed -gt 4 -and $parsed -ne $SupPid -and $parsed -ne $PID -and -not $SupSet.ContainsKey($parsed)) {
                            $targetPid = $parsed
                        }
                    }
                } catch {}

                if ($targetPid -eq 0) {
                    $misses++
                    if (((Get-Date) - $lastSuccess).TotalSeconds -gt $StallSec) { $stalled = $true; break }
                    [System.Threading.Thread]::Sleep($PollSleepMs)
                    continue
                }

                $killSw = [System.Diagnostics.Stopwatch]::StartNew()
                $created = [long]0
                $verdict = [SnubberHarness.Win32Killer]::KillChecked($targetPid, $ExpectedExe, $lastCreation, [ref]$created)
                $killSw.Stop()

                switch ($verdict) {
                    1 {
                        $lastCreation = $created
                        $completed++
                        $lat.Add($killSw.Elapsed.TotalMilliseconds * 1000.0)
                        $lastSuccess = Get-Date
                        $Shared[$Lane] = $completed
                    }
                    3 { $duplicates++; [System.Threading.Thread]::Sleep(1) }
                    2 { $stale++; [System.Threading.Thread]::Sleep(1) }
                    default {
                        # 0 = gone or refused: never blacklist, keep retrying this lane
                        $retries++
                        if (((Get-Date) - $lastSuccess).TotalSeconds -gt $StallSec) { $stalled = $true; break }
                        [System.Threading.Thread]::Sleep($PollSleepMs)
                    }
                }
                if ($stalled) { break }
            }
        } catch {
            return [pscustomobject]@{ Lane = $Lane; Error = $_.Exception.Message; Completed = $completed; Stalled = $stalled; Misses = $misses; Stale = $stale; Duplicates = $duplicates; Retries = $retries; ElapsedMs = $sw.Elapsed.TotalMilliseconds; LatUs = $lat.ToArray() }
        }
        [pscustomobject]@{
            Lane       = $Lane
            Error      = $null
            Completed  = $completed
            Stalled    = $stalled
            Misses     = $misses
            Stale      = $stale
            Duplicates = $duplicates
            Retries    = $retries
            ElapsedMs  = [math]::Round($sw.Elapsed.TotalMilliseconds, 1)
            LatUs      = $lat.ToArray()
        }
    }

    $workers = New-Object 'System.Collections.Generic.List[object]'
    foreach ($lane in $lanes) {
        $rs = [runspacefactory]::CreateRunspace([initialsessionstate]::CreateDefault2())
        $rs.Open()
        $ps = [powershell]::Create()
        $ps.Runspace = $rs
        $null = $ps.AddScript($killerScript)
        $null = $ps.AddArgument($lane.Index)
        $null = $ps.AddArgument($lane.PidFile)
        $null = $ps.AddArgument([int]$lane.Proc.Id)
        $null = $ps.AddArgument($exeName)
        $null = $ps.AddArgument($StallTimeoutSec)
        $null = $ps.AddArgument($PollSleepMs)
        $null = $ps.AddArgument($shared)
        $null = $ps.AddArgument($supSet)
        $handle = $ps.BeginInvoke()
        $workers.Add([pscustomobject]@{ Lane = $lane; PS = $ps; Handle = $handle; RS = $rs })
    }
    Write-Host ("[STORM] {0} killers armed, {1} supervisors live" -f $workers.Count, $Lines) -ForegroundColor DarkGray

    $overall = [System.Diagnostics.Stopwatch]::StartNew()
    $allLat = New-Object 'System.Collections.Generic.List[double]'
    $rateWindow = New-Object 'System.Collections.Generic.List[double]'
    $lastTotal = 0
    $lastSampleAt = 0.0
    $peakRate = 0.0
    $nextPrintMs = 0

    while ($true) {
        $done = $true
        foreach ($w in $workers) { if (-not $w.Handle.IsCompleted) { $done = $false; break } }

        $total = 0
        foreach ($lane in $lanes) { $total += [int]$shared[$lane.Index] }
        foreach ($lane in $lanes) {
            $p = $lane.Proc
            try {
                $p.Refresh()
                if (-not $p.HasExited) {
                    $ws = $p.WorkingSet64 / 1MB
                    $pv = $p.PrivateMemorySize64 / 1MB
                    $hc = [int]$p.HandleCount
                    $th = [int]($p.Threads.Count)
                    if ($lane.Samples -eq 0) { $lane.WsFirst = $ws; $lane.PrivFirst = $pv; $lane.HandleFirst = $hc }
                    $lane.WsLast = $ws; $lane.PrivLast = $pv; $lane.HandleLast = $hc
                    if ($ws -gt $lane.WsPeak) { $lane.WsPeak = $ws }
                    if ($pv -gt $lane.PrivPeak) { $lane.PrivPeak = $pv }
                    if ($hc -gt $lane.HandlePeak) { $lane.HandlePeak = $hc }
                    if ($th -gt $lane.ThreadPeak) { $lane.ThreadPeak = $th }
                    $lane.Samples++
                }
            } catch {}
        }

        $now = $overall.Elapsed.TotalSeconds
        if ($now - $lastSampleAt -ge 0.25) {
            $bucket = ($total - $lastTotal) / [math]::Max($now - $lastSampleAt, 0.001)
            if ($bucket -gt $peakRate) { $peakRate = $bucket }
            $rateWindow.Add($bucket)
            $lastTotal = $total
            $lastSampleAt = $now
        }
        if ($overall.Elapsed.TotalMilliseconds -ge $nextPrintMs) {
            $nextPrintMs += 1000
            $rate = $total / [math]::Max($now, 0.001)
            Write-Host ("[STORM] {0}/{1} | {2:N1} kills/s aggregate | lanes: {3}" -f $total, $totalTarget, $rate, (($lanes | ForEach-Object { "{0}={1}" -f $_.Index, [int]$shared[$_.Index] }) -join " ")) -ForegroundColor Green
        }

        if ($done) { break }
        Start-Sleep -Milliseconds 100
    }
    $overall.Stop()

    foreach ($w in $workers) {
        try {
            $out = $w.PS.EndInvoke($w.Handle)
            $res = @($out) | Select-Object -Last 1
            $w.Lane.Kill = $res
            if ($res) { foreach ($v in $res.LatUs) { $allLat.Add($v) } }
        } catch {
            $w.Lane.Kill = [pscustomobject]@{ Lane = $w.Lane.Index; Error = $_.Exception.Message; Completed = 0; Stalled = $true; Misses = 0; Stale = 0; Duplicates = 0; Retries = 0; ElapsedMs = 0; LatUs = @() }
        } finally {
            try { $w.PS.Dispose() } catch {}
            try { $w.RS.Close(); $w.RS.Dispose() } catch {}
        }
    }

    foreach ($lane in $lanes) {
        if (-not $lane.Proc.HasExited) { $null = $lane.Proc.WaitForExit($SupervisorWaitSec * 1000) }
        if (-not $lane.Proc.HasExited) {
            Write-Host ("[TIMEOUT] lane={0} supervisor did not exit, terminating" -f $lane.Index) -ForegroundColor Red
            try { $lane.Proc.Kill() } catch {}
            $null = $lane.Proc.WaitForExit(5000)
            $lane.Exit = $null
        } else {
            $lane.Exit = $lane.Proc.ExitCode
        }
        try { if ($lane.OutTask) { [System.IO.File]::WriteAllText($lane.OutFile, $lane.OutTask.GetAwaiter().GetResult()) } } catch {}
        try { if ($lane.ErrTask) { [System.IO.File]::WriteAllText($lane.ErrFile, $lane.ErrTask.GetAwaiter().GetResult()) } } catch {}
        if (Test-Path $lane.Report) {
            try { $lane.ReportObj = Get-Content -Raw $lane.Report | ConvertFrom-Json } catch { $lane.ReportObj = $null }
        }
    }
    $lanesFinalized = $true
} finally {
    if (-not $lanesFinalized) {
        # Abort path: never leave supervisors (and their spinning workers) behind.
        foreach ($lane in $lanes) {
            if ($lane.Proc -and -not $lane.Proc.HasExited) {
                Write-Host ("[CLEANUP] lane={0} killing supervisor pid={1}" -f $lane.Index, $lane.Proc.Id) -ForegroundColor DarkYellow
                try { $lane.Proc.Kill() } catch {}
                try { $null = $lane.Proc.WaitForExit(5000) } catch {}
            }
            try {
                if (Test-Path $lane.PidFile) {
                    $childPid = 0
                    $fs = [System.IO.File]::Open($lane.PidFile, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, ([System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete))
                    try {
                        $sr = New-Object System.IO.StreamReader($fs)
                        try { $rawPid = $sr.ReadToEnd().Trim() } finally { $sr.Dispose() }
                    } finally { $fs.Dispose() }
                    if ([int]::TryParse($rawPid, [ref]$childPid) -and $childPid -gt 4 -and -not $supSet.ContainsKey($childPid)) {
                        $created = [long]0
                        $null = $killer::KillChecked($childPid, $exeName, [long]0, [ref]$created)
                    }
                }
            } catch {}
        }
    }
    if ($timerRes) { try { $null = [SnubberHarness.Timer]::timeEndPeriod(1) } catch {} }
}

$durationSec = [math]::Max($overall.Elapsed.TotalSeconds, 0.001)
$completed = 0
$totalRescued = 0
$totalClean = 0
foreach ($lane in $lanes) {
    if ($lane.ReportObj) {
        $completed += [int]$lane.ReportObj.total_reaped
        $totalRescued += [int]$lane.ReportObj.tokens_rescued
        $totalClean += [int]$lane.ReportObj.clean_settles
    }
}
$rate = $completed / $durationSec

$checks = New-Object 'System.Collections.Generic.List[object]'
foreach ($lane in $lanes) {
    $tag = "lane {0}" -f $lane.Index
    if ($lane.ReportObj) {
        $checks.Add([pscustomobject]@{ Name = "$tag reaped == $KillsPerLine"; Ok = ([int]$lane.ReportObj.total_reaped -eq $KillsPerLine) })
        $checks.Add([pscustomobject]@{ Name = "$tag rescue + clean == reaped"; Ok = (([int]$lane.ReportObj.tokens_rescued + [int]$lane.ReportObj.clean_settles) -eq [int]$lane.ReportObj.total_reaped) })
        $checks.Add([pscustomobject]@{ Name = "$tag final_generation == kills + 1"; Ok = ([int]$lane.ReportObj.final_generation -eq $KillsPerLine + 1) })
        $checks.Add([pscustomobject]@{ Name = "$tag deadlock == false"; Ok = (-not [bool]$lane.ReportObj.deadlock) })
    } else {
        $checks.Add([pscustomobject]@{ Name = "$tag report present"; Ok = $false })
    }
    $checks.Add([pscustomobject]@{ Name = "$tag supervisor exit == 0"; Ok = ($lane.Exit -eq 0) })
    $checks.Add([pscustomobject]@{ Name = "$tag killer reported no error/stall"; Ok = ($lane.Kill -and -not $lane.Kill.Error -and -not $lane.Kill.Stalled) })
    if ($lane.Samples -gt 0) {
        $growth = $lane.PrivLast - $lane.PrivFirst
        $checks.Add([pscustomobject]@{ Name = ("$tag private growth {0:+0.00;-0.00;0.00} MB <= {1:N1} MB" -f $growth, $MaxGrowthMb); Ok = ($growth -le $MaxGrowthMb) })
    }
}
$checks.Add([pscustomobject]@{ Name = ("aggregate reaped {0} == {1}" -f $completed, $totalTarget); Ok = ($completed -eq $totalTarget) })
$checks.Add([pscustomobject]@{ Name = "aggregate rescue + clean == reaped"; Ok = (($totalRescued + $totalClean) -eq $completed) })
$checks.Add([pscustomobject]@{ Name = "aggregate no cross-lineage accounting drift"; Ok = (($totalRescued + $totalClean) -eq $totalTarget) })

$failedChecks = @($checks | Where-Object { -not $_.Ok })
$verdict = if ($completed -eq $totalTarget -and $failedChecks.Count -eq 0) { "PASS" } else { "FAIL" }

$sorted = @($allLat.ToArray() | Sort-Object)
function Get-Pctl([double[]]$values, [double]$p) {
    if ($values.Count -eq 0) { return 0 }
    $idx = [int][math]::Ceiling($p * $values.Count) - 1
    if ($idx -lt 0) { $idx = 0 }
    if ($idx -ge $values.Count) { $idx = $values.Count - 1 }
    return $values[$idx]
}

Write-Host "==========================================================" -ForegroundColor Cyan
Write-Host "   MULTI-LINEAGE STORM RESULTS" -ForegroundColor Cyan
Write-Host "==========================================================" -ForegroundColor Cyan
Write-Host ("Kills completed : {0} / {1} across {2} lineages" -f $completed, $totalTarget, $Lines)
Write-Host ("Duration        : {0}" -f $overall.Elapsed.ToString("hh\:mm\:ss\.fff"))
Write-Host ("Aggregate rate  : {0:N1} kills/s avg | {1:N1} kills/s peak (250 ms window)" -f $rate, $peakRate)
Write-Host ("Dispatch us     : min {0:N1} | p50 {1:N1} | p95 {2:N1} | p99 {3:N1} | p99.9 {4:N1} | max {5:N1}" -f `
    (Get-Pctl $sorted 0.0), (Get-Pctl $sorted 0.50), (Get-Pctl $sorted 0.95), (Get-Pctl $sorted 0.99), (Get-Pctl $sorted 0.999), (Get-Pctl $sorted 1.0))
Write-Host ("Accounting      : tokens_rescued={0} clean_settles={1} rescue_rate={2:N1}%" -f $totalRescued, $totalClean, $(if ($completed -gt 0) { 100.0 * $totalRescued / $completed } else { 0 }))
foreach ($lane in $lanes) {
    $k = $lane.Kill
    $r = $lane.ReportObj
    $rateL = if ($k -and $k.ElapsedMs -gt 0) { 1000.0 * $k.Completed / $k.ElapsedMs } else { 0 }
    $privGrowth = if ($lane.PrivFirst -ge 0) { $lane.PrivLast - $lane.PrivFirst } else { 0 }
    Write-Host ("Lane {0} : sup={1} exit={2} killed={3} {4:N1} kills/s | reaped={5} rescued={6} clean={7} final_gen={8} | ws_peak={9:N2} MB priv_growth={10:+0.00;-0.00;0.00} MB handles_peak={11} threads_peak={12} | stale={13} dup={14} retries={15} miss_polls={16}" -f `
        $lane.Index, $(if ($lane.Proc) { $lane.Proc.Id } else { 0 }), $lane.Exit, $(if ($k) { $k.Completed } else { 0 }), $rateL, `
        $(if ($r) { $r.total_reaped } else { 0 }), $(if ($r) { $r.tokens_rescued } else { 0 }), $(if ($r) { $r.clean_settles } else { 0 }), $(if ($r) { $r.final_generation } else { 0 }), `
        $lane.WsPeak, $privGrowth, $lane.HandlePeak, $lane.ThreadPeak, `
        $(if ($k) { $k.Stale } else { 0 }), $(if ($k) { $k.Duplicates } else { 0 }), $(if ($k) { $k.Retries } else { 0 }), $(if ($k) { $k.Misses } else { 0 }))
    if ($k -and $k.Error) { Write-Host ("           killer error: {0}" -f $k.Error) -ForegroundColor Red }
}
foreach ($c in $checks) {
    $color = if ($c.Ok) { "Green" } else { "Red" }
    Write-Host ("Check           : {0} {1}" -f $(if ($c.Ok) { "OK  " } else { "FAIL" }), $c.Name) -ForegroundColor $color
}
$verdictColor = if ($verdict -eq "PASS") { "Green" } else { "Red" }
Write-Host ("Verdict         : {0}" -f $verdict) -ForegroundColor $verdictColor
Write-Host ("Report JSON     : {0}" -f $reportFull)
Write-Host ("Stats JSON      : {0}" -f $statsFull)
Write-Host ("Lane artifacts  : {0}" -f $laneRootFull)
Write-Host "==========================================================" -ForegroundColor $verdictColor

$laneStats = @()
foreach ($lane in $lanes) {
    $k = $lane.Kill
    $laneStats += [ordered]@{
        index            = $lane.Index
        supervisor_pid   = $(if ($lane.Proc) { $lane.Proc.Id } else { 0 })
        supervisor_exit  = $lane.Exit
        killed           = $(if ($k) { $k.Completed } else { 0 })
        stale            = $(if ($k) { $k.Stale } else { 0 })
        duplicates       = $(if ($k) { $k.Duplicates } else { 0 })
        retries          = $(if ($k) { $k.Retries } else { 0 })
        miss_polls       = $(if ($k) { $k.Misses } else { 0 })
        stalled          = $(if ($k) { [bool]$k.Stalled } else { $true })
        error            = $(if ($k) { $k.Error } else { $null })
        elapsed_ms       = $(if ($k) { $k.ElapsedMs } else { 0 })
        report           = $lane.ReportObj
        resources        = [ordered]@{
            ws_peak_mb     = [math]::Round($lane.WsPeak, 3)
            private_growth_mb = [math]::Round($(if ($lane.PrivFirst -ge 0) { $lane.PrivLast - $lane.PrivFirst } else { 0 }), 3)
            handles_peak   = $lane.HandlePeak
            threads_peak   = $lane.ThreadPeak
            samples        = $lane.Samples
        }
    }
}

$stats = [ordered]@{
    timestamp_utc      = (Get-Date).ToUniversalTime().ToString("o")
    model              = "multi_lineage"
    lines              = $Lines
    kills_per_line     = $KillsPerLine
    kills_requested    = $totalTarget
    kills_completed    = $completed
    workload           = $(if ($Aggressive) { "spin" } else { "sleep" })
    timer_resolution   = $timerRes
    duration_ms        = [math]::Round($overall.Elapsed.TotalMilliseconds, 1)
    rate_per_sec       = [math]::Round($rate, 1)
    peak_rate_per_sec  = [math]::Round($peakRate, 1)
    tokens_rescued     = $totalRescued
    clean_settles      = $totalClean
    dispatch_us        = [ordered]@{
        min   = [math]::Round((Get-Pctl $sorted 0.0), 1)
        p50   = [math]::Round((Get-Pctl $sorted 0.50), 1)
        p95   = [math]::Round((Get-Pctl $sorted 0.95), 1)
        p99   = [math]::Round((Get-Pctl $sorted 0.99), 1)
        p999  = [math]::Round((Get-Pctl $sorted 0.999), 1)
        max   = [math]::Round((Get-Pctl $sorted 1.0), 1)
        count = $allLat.Count
    }
    max_growth_mb      = $MaxGrowthMb
    lanes              = $laneStats
    checks             = $checks.ToArray()
    verdict            = $verdict
}
foreach ($f in @($statsFull, $reportFull)) {
    $dir = Split-Path -Parent $f
    if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir | Out-Null }
}
$stats | ConvertTo-Json -Depth 8 | Set-Content -Encoding UTF8 $statsFull
$stats | ConvertTo-Json -Depth 8 | Set-Content -Encoding UTF8 $reportFull

if ($verdict -eq "PASS") { exit 0 } else { exit 1 }
