# SNUBBER BENCH HARNESS — run Criterion benchmarks and export a
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
        cargo bench --bench admission_bench 2>&1 | ForEach-Object { Write-Host $_ }
        if ($LASTEXITCODE -ne 0) {
            throw "cargo bench failed with exit code $LASTEXITCODE"
        }
    } finally {
        Pop-Location
    }
}

if (-not (Test-Path $criterionDir)) {
    Write-Error "Criterion output not found at $criterionDir. Run without -SkipRun first."
    exit 1
}

# Pretty label map: criterion path fragments -> operation labels.
$labels = @{
    "guard_new_commit"             = "SnubberGuard::new + commit"
    "epoch_gate_observe"           = "EpochGate::observe"
    "classify_stall_ceiling"       = "classify_stall_ceiling"
    "worker_pulse_kick_watch"      = "WorkerPulse::kick + PulseWatch::observe"
    "isr_bridge_push_pop"          = "IsrBridge::try_push + try_pop"
    "isr_trip_latch_swap"          = "IsrBridge::trip + take_trips"
    "node_enter_commit"            = "node.enter + commit"
    "node_enter_auto_commit"       = "node.enter_auto + commit"
    "node_scoped_work_commit"      = "node.scoped (work + auto-commit)"
    "node_enter_critical_priority" = "node.enter_with_priority(Critical)"
    "node_transition"              = "node.transition"
    "gateway_roundtrip_send_recv"  = "gateway send -> recv roundtrip"
    "gateway_send_throttled_latch" = "gateway send (HWM/LWM throttled)"
    "conduit_check_directive"      = "TaskConduit::check_directive"
    "node_next_sequence"           = "node.next_sequence (auto ticket)"
    "node_stats_snapshot"          = "node.stats() snapshot"
    "stats_telemetry_math"         = "SnubberStats::shed_total + success_permille"
    "erased_enter_auto_commit"     = "enter_erased_auto"
}

$rows = [System.Collections.Generic.List[string]]::new()
$rows.Add("| Operation | Average Latency | Notes |")
$rows.Add("|---|---|---|")

$estimates = Get-ChildItem -Path $criterionDir -Recurse -Filter "estimates.json" |
    Where-Object {
        $rel = $_.FullName.Substring($criterionDir.Length + 1).Replace("\", "/")
        # Primary run estimate only: <group>/<function>/estimates.json
        # (skip baseline subfolders <group>/<function>/new|base/estimates.json).
        $rel -notmatch "(^|/)(new|base)/estimates\.json$"
    } | Sort-Object FullName

foreach ($est in $estimates) {
    $rel = $est.FullName.Substring($criterionDir.Length + 1).Replace("\", "/")
    $parts = $rel.Split("/")
    if ($parts.Count -lt 3) { continue }
    $key = $parts[$parts.Count - 2]   # function name sits before estimates.json
    $group = $parts[$parts.Count - 3]
    $label = if ($labels.ContainsKey($key)) { $labels[$key] } else { $key }
    try {
        $json = Get-Content $est.FullName -Raw | ConvertFrom-Json
        $meanNs = [double]$json.mean.point_estimate
    } catch {
        Write-Warning "Skipping unparsable estimate: $rel"
        continue
    }
    $layer = switch -Regex ($group) {
        "layer1_core"        { "Layer 1 (`core`)" }
        "layer1_5_hang_isr"  { "Layer 1.5 (`core`)" }
        "layer2_std"         { "Layer 2 (`std`)" }
        "layer2_telemetry"   { "Layer 2 (`std`)" }
        "layer3_alloc"       { "Layer 2 (`alloc`)" }
        default              { $group }
    }
    $ns = if ($meanNs -lt 1000) {
        "{0:F1} ns" -f $meanNs
    } else {
        "{0:F2} µs" -f ($meanNs / 1000.0)
    }
    $rows.Add("| ``$label`` | **$ns** | $group bench |")
}

if (-not (Test-Path $outDir)) { New-Item -ItemType Directory -Path $outDir -Force | Out-Null }
$rows | Set-Content -Path $outFile -Encoding utf8

Write-Host "" -ForegroundColor DarkGray
Write-Host "=== BENCH SNAPSHOT (ns/op mean point estimate) ===" -ForegroundColor Green
$rows | ForEach-Object { Write-Host $_ }
Write-Host "Snapshot written: $outFile" -ForegroundColor Cyan

if ($GitHub) {
    # GitHub Actions: emit a compact GHA step summary table.
    $summary = $env:GITHUB_STEP_SUMMARY
    if ($summary) {
        $rows | Add-Content -Path $summary -Encoding utf8
    }
}
