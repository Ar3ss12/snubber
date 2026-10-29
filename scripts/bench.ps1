# SNUBBER BENCH HARNESS -- run Criterion benchmarks and export a
# float-free telemetry snapshot (ns/op, mean point estimate) into
# target/bench/snapshot.md.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File scripts\bench.ps1            # build + run + export
#   powershell -ExecutionPolicy Bypass -File scripts\bench.ps1 -SkipRun   # export from existing run
#   powershell -ExecutionPolicy Bypass -File scripts\bench.ps1 -GitHub    # CI: fresh run, artifact output
param(
    [switch]$SkipRun,
    [switch]$GitHub
)

$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
$benchDir = Join-Path $root "target"
$criterionDir = Join-Path $benchDir "criterion"
$outDir = Join-Path $benchDir "bench"
$outFile = Join-Path $outDir "snapshot.md"

if (-not $SkipRun) {
    Write-Host "[bench] cargo bench --bench admission_bench (release) ..." -ForegroundColor Cyan
    Push-Location $root
    try {
        # PS 5.1 wraps native stderr lines in ErrorRecords; under Stop the
        # first cargo progress line would be a terminating error.
        $PrevEap = $ErrorActionPreference
        $ErrorActionPreference = "Continue"
        try {
            $CargoOut = @(& cargo bench --locked --bench admission_bench 2>&1)
            $CargoExit = $LASTEXITCODE
        } finally {
            $ErrorActionPreference = $PrevEap
        }
        if ($CargoExit -ne 0) {
            # Diagnostics belong on stderr, prefixed: folding them into
            # Write-Host made cargo failures look like ordinary output.
            $CargoOut | ForEach-Object { [Console]::Error.WriteLine($_) }
            throw "cargo bench failed with exit code $CargoExit"
        }
        $CargoOut | ForEach-Object { Write-Host $_ }
    } finally {
        Pop-Location
    }
}

if (-not (Test-Path $criterionDir)) {
    Write-Error "Criterion output not found at $criterionDir. Run without -SkipRun first."
    exit 1
}

# Pretty label map: criterion path fragments -> operation labels.
# Every bench in the suite is mapped: an unmapped key still exports, but
# under its snake_case function name, which reads as a different operation
# next to the mapped rows.
$labels = @{
    "guard_new_commit"                  = "SnubberGuard::new + commit"
    "epoch_gate_observe"                = "EpochGate::observe"
    "classify_stall_ceiling"            = "classify_stall_ceiling"
    "worker_pulse_kick_watch"           = "WorkerPulse::kick + PulseWatch::observe"
    "isr_bridge_push_pop"               = "IsrBridge::try_push + try_pop"
    "isr_trip_latch_swap"               = "IsrBridge::trip + take_trips"
    "node_enter_commit"                 = "node.enter + commit"
    "node_enter_auto_commit"            = "node.enter_auto + commit"
    "node_scoped_work_commit"           = "node.scoped (work + auto-commit)"
    "node_enter_critical_priority"      = "node.enter_with_priority(Critical)"
    "node_transition"                   = "node.transition"
    "node_passthrough_enter_commit"     = "node_passthrough.enter + commit"
    "node_passthrough_scoped_commit"    = "node_passthrough.scoped + commit"
    "conflate_publish_take"             = "ConflateSender::publish + take_latest"
    "conflate_publish_replace"          = "ConflateSender::publish (occupied slot)"
    "node_commit_enveloped_payload"     = "guard.commit_enveloped(64 B)"
    "async_enter_ready_poll"            = "enter_async_auto (ready poll)"
    "async_park_wake_admit_roundtrip"   = "enter_async_auto park/wake/admit"
    "gateway_roundtrip_send_recv"       = "gateway send -> recv roundtrip"
    "gateway_send_throttled_latch"      = "gateway send (HWM/LWM throttled)"
    "conduit_check_directive"           = "TaskConduit::check_directive"
    "node_next_sequence"                = "node.next_sequence (auto ticket)"
    "node_stats_snapshot"               = "node.stats() snapshot"
    "stats_telemetry_math"              = "SnubberStats::shed_total + success_rate_per_mille"
    "erased_enter_auto_commit"          = "probe_erased_auto + commit"
    "drain_feed_frame"                  = "PipeDrain::feed (one frame)"
    "drain_feed_256_frames"             = "PipeDrain::feed (256 frames)"
    "drain_sync_256_frames"             = "PipeDrain::drain_sync (256 frames)"
    "token_write_to"                    = "ProcessToken::write_to(&mut sink)"
    "state_digest_write_to"             = "StateDigest::write_to(&mut sink)"
    "friction_handler_arc_call"         = "friction_fn handler call (Arc<dyn Fn>)"
    "friction_wait_adaptive_arc"        = "friction_wait_ms (adaptive, Arc handler)"
    "friction_wait_adaptive_fn"         = "friction_wait_ms (adaptive, fn)"
    "node_name_as_str"                  = "NodeName::as_str"
    "node_name_clone_static"            = "NodeName::Static clone"
    "node_name_clone_owned"             = "NodeName::Owned clone"
    "process_token_parse"               = "ProcessToken::from_bytes"
    "process_token_to_bytes"            = "process_token.to_bytes()"
    "snapshot_v1_encode_crc"            = "snapshot_v1.encode()"
    "state_digest_new_crc"              = "state_digest.crc()"
    "static_node_enter_commit"          = "static_node.enter + commit"
    "expired_lanes_4_lanes"             = "pool.expired_lanes(lease) (4 lanes)"
    "lane_attach_reap_ambush"           = "lane.attach + note_death (ambush) -> PoolReap"
    "lane_attach_reap_escape"           = "lane.attach + note_death (escape) -> PoolReap"
    "namespace_task_pack"               = "namespace_task::pack"
    "namespace_task_unpack"             = "namespace_task::unpack (task_lane + task_local)"
    "note_memory_kb"                    = "lane.note_memory_kb"
    "report_snapshot_4_lanes"           = "pool.report() (4 lanes)"
    "contended_erased_holding_threads_6" = "enter_erased_holding_auto + commit (6 threads)"
    "contended_node_enter_threads_6"    = "node.enter + commit (6 threads)"
    "contended_node_scoped_threads_6"   = "node.scoped (6 threads)"
    "contended_gateway_mpsc_threads_6"  = "gateway send (6 threads)"
    "cross_thread_isr_bridge_push_pop"  = "IsrBridge::try_push + try_pop (2 threads)"
}

$rows = [System.Collections.Generic.List[string]]::new()
$rows.Add("| Operation | Average Latency | Notes |")
$rows.Add("|---|---|---|")

$estimates = Get-ChildItem -Path $criterionDir -Recurse -Filter "estimates.json" |
    Where-Object {
        $rel = $_.FullName.Substring($criterionDir.Length + 1).Replace("\", "/")
        # Criterion 0.5 layout: <group>/<function>/new/estimates.json holds
        # the latency in ns. change/ holds a relative delta, base/ the old
        # run; reading those published 0.1 "ns" tables. Accept ONLY new/.
        $rel -match "(^|/)new/estimates\.json$"
    } | Sort-Object FullName

foreach ($est in $estimates) {
    # $rel is relative to $criterionDir: <group>/<function>/new/estimates.json
    $rel = $est.FullName.Substring($criterionDir.Length + 1).Replace("\", "/")
    $parts = $rel.Split("/")
    if ($parts.Count -lt 4) { continue }
    $key = $parts[$parts.Count - 3]
    $group = $parts[$parts.Count - 4]
    $label = if ($labels.ContainsKey($key)) { $labels[$key] } else { $key }
    try {
        $json = Get-Content $est.FullName -Raw | ConvertFrom-Json
        $meanNs = [double]$json.mean.point_estimate
    } catch {
        # An unparsable estimate must fail the export, not drop a row: a
        # partial table would still publish as a green run.
        Write-Error "Cannot parse bench estimate $rel : $_"
        exit 1
    }
    if (-not ($meanNs -ge 0.0)) {
        Write-Error "Non-finite bench mean in $rel : $meanNs"
        exit 1
    }
    $ns = if ($meanNs -lt 1000) {
        "{0:F1} ns" -f $meanNs
    } else {
        "{0:F2} us" -f ($meanNs / 1000.0)
    }
    $rows.Add("| ``$label`` | **$ns** | $group bench |")
}

if ($rows.Count -le 2) {
    # Header-only snapshot means the export saw no estimates: publishing it
    # would zero out the README table with a green CI. Fail instead.
    Write-Error "No bench estimates found under $criterionDir -- refusing to write an empty snapshot."
    exit 1
}

if (-not (Test-Path $outDir)) { New-Item -ItemType Directory -Path $outDir -Force | Out-Null }
# BOM-less UTF-8: PS 5.1 `Set-Content -Encoding utf8` writes a BOM that
# makes the snapshot differ byte-wise from the .sh flavor (and upsets
# naive markdown diffs).
$Utf8NoBom = New-Object System.Text.UTF8Encoding($false)
$SnapshotText = ($rows -join "`r`n") + "`r`n"
$OutFilePath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($outFile)
[System.IO.File]::WriteAllText($OutFilePath, $SnapshotText, $Utf8NoBom)

Write-Host "" -ForegroundColor DarkGray
Write-Host "=== BENCH SNAPSHOT (ns/op mean point estimate) ===" -ForegroundColor Green
$rows | ForEach-Object { Write-Host $_ }
Write-Host "Snapshot written: $outFile" -ForegroundColor Cyan

if ($GitHub) {
    # GitHub Actions: emit a compact GHA step summary table.
    $summary = $env:GITHUB_STEP_SUMMARY
    if ($summary) {
        $rows | Add-Content -Path $summary -Encoding utf8
    } else {
        # Strict parity with bench.sh's in_ci: only "true"/"1" count as CI.
        $CiFlag = if ($env:CI) { $env:CI.ToLowerInvariant() } else { "" }
        if ($CiFlag -eq "true" -or $CiFlag -eq "1") {
            # In CI the whole point of -GitHub is the published summary; a
            # silent no-op reported a green bench job that showed nothing.
            Write-Error "-GitHub in CI but GITHUB_STEP_SUMMARY is unset: the snapshot would never reach the step summary"
            exit 1
        }
        Write-Host "[bench] WARN: -GitHub without GITHUB_STEP_SUMMARY (not CI) -- snapshot left at $outFile" -ForegroundColor Yellow
    }
}
