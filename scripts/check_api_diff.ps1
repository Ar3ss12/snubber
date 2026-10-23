# scripts/check_api_diff.ps1
# Public API semver gate. Compares the working tree against the latest
# reachable git tag with `cargo-semver-checks` when the tool is installed.
#
# Exit codes: 0 = clean or SKIP (tool/tag unavailable), 1 = breaking change.

param([string]$Baseline = "")

$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
$RepoRoot = Resolve-Path "$ScriptDir\.."

if (-not $Baseline) {
    $Baseline = (git -C "$RepoRoot" describe --tags --abbrev=0 2>$null)
    if ($LASTEXITCODE -ne 0) { $Baseline = "" }
}

Write-Host "[API] semver diff" -NoNewline
if (-not $Baseline) {
    Write-Host " SKIP (no git tag baseline)"
    exit 0
}

$tool = Get-Command cargo-semver-checks -ErrorAction SilentlyContinue
if (-not $tool) {
    & cargo semver-checks --version *> $null
    if ($LASTEXITCODE -ne 0) {
        Write-Host " SKIP (cargo-semver-checks not installed; cargo install cargo-semver-checks)"
        exit 0
    }
}

Write-Host " baseline=$Baseline"
Push-Location $RepoRoot
try {
    & cargo semver-checks check-release --baseline-rev $Baseline
    exit $LASTEXITCODE
} finally {
    Pop-Location
}
