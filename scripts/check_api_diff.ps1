# scripts/check_api_diff.ps1
# Public API semver gate. Compares the working tree against the latest
# reachable git tag with `cargo-semver-checks` when the tool is installed.
#
# Exit codes: 0 = clean or SKIP (tool/tag unavailable), 1 = breaking change.

param([string]$Baseline = "")

$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
$RepoRoot = Resolve-Path "$ScriptDir\.."
# CI mode: GitHub Actions sets CI=true. A SKIP in a controlled
# environment means the workflow is misconfigured (missing tags or
# tooling) -- that must fail the run, not silently skip the gate.
# Strict parity with the .sh `in_ci`: only "true"/"1" count as CI.
$CiFlag = if ($env:CI) { $env:CI.ToLowerInvariant() } else { "" }
$InCi = ($CiFlag -eq "true" -or $CiFlag -eq "1")

if (-not $Baseline) {
    # PS 5.1 wraps native stderr in ErrorRecords: under Stop, a
    # tag-less `git describe` writing to stderr would kill the script.
    $PrevEap = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    $Baseline = & git -C "$RepoRoot" describe --tags --abbrev=0 2>$null
    $GitExit = $LASTEXITCODE
    $ErrorActionPreference = $PrevEap
    if ($GitExit -ne 0) { $Baseline = "" }
}

Write-Host "[API] semver diff" -NoNewline
if (-not $Baseline) {
    if ($InCi) {
        Write-Host " FAIL (CI: no git tag baseline -- checkout must fetch tags and the repo must be tagged per release)" -ForegroundColor Red
        exit 1
    }
    if (Test-Path "$RepoRoot\.git\shallow") {
        Write-Host " SKIP (no git tag baseline; shallow clone -- run: git fetch --tags --unshallow)"
    } else {
        Write-Host " SKIP (no git tag baseline)"
    }
    exit 0
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    if ($InCi) {
        Write-Host " FAIL (CI: cargo not installed -- workflow must install the toolchain)" -ForegroundColor Red
        exit 1
    }
    Write-Host " SKIP (cargo not installed)"
    exit 0
}
$PrevEap = $ErrorActionPreference
$ErrorActionPreference = "Continue"
& cargo semver-checks --version *> $null
$SemverExit = $LASTEXITCODE
$ErrorActionPreference = $PrevEap
if ($SemverExit -ne 0) {
    if ($InCi) {
        Write-Host " FAIL (CI: cargo-semver-checks missing -- workflow must install it)" -ForegroundColor Red
        exit 1
    }
    Write-Host " SKIP (cargo-semver-checks not installed; cargo install cargo-semver-checks)"
    exit 0
}

# A baseline pointing at HEAD diffs an empty range: the gate would PASS
# trivially unless the worktree is dirty. Say so instead of passing silently.
$PrevEap = $ErrorActionPreference
$ErrorActionPreference = "Continue"
$BaselineCommit = & git -C "$RepoRoot" rev-parse "${Baseline}^{commit}" 2>$null
$HeadCommit = & git -C "$RepoRoot" rev-parse HEAD 2>$null
$ErrorActionPreference = $PrevEap
if ($BaselineCommit -and $BaselineCommit -eq $HeadCommit) {
    Write-Host " FAIL (baseline $Baseline is HEAD: committed-but-untagged API changes are invisible to this gate -- tag the release or pass an older baseline)" -ForegroundColor Red
    exit 1
}

$Crate = (Select-String -Path "$RepoRoot\Cargo.toml" -Pattern '^name\s*=\s*"(.+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
Write-Host " baseline=$Baseline package=$Crate"
Push-Location $RepoRoot
try {
    # cargo-semver-checks has no --locked flag: snapshot the lockfile and
    # fail if the tool rewrote it (the CI gate cannot see this otherwise).
    $LockPath = Join-Path $RepoRoot "Cargo.lock"
    $LockBefore = if (Test-Path $LockPath) { (Get-FileHash $LockPath -Algorithm SHA256).Hash } else { "" }
    & cargo semver-checks check-release --baseline-rev $Baseline --package $Crate
    $SemverExit = $LASTEXITCODE
    $LockAfter = if (Test-Path $LockPath) { (Get-FileHash $LockPath -Algorithm SHA256).Hash } else { "" }
    # An empty hash means the lockfile vanished: `"" -eq ""` would hide any
    # rewrite. Parity with check_api_diff.sh.
    if (-not $LockBefore -or -not $LockAfter) {
        Write-Host " FAIL (could not hash Cargo.lock: it is missing -- lock mutation would be invisible)" -ForegroundColor Red
        exit 1
    }
    if ($LockBefore -ne $LockAfter) {
        Write-Host " FAIL (cargo-semver-checks rewrote Cargo.lock; restore it and pin the toolchain)" -ForegroundColor Red
        exit 1
    }
    exit $SemverExit
} finally {
    Pop-Location
}
