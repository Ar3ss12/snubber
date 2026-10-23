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
    if (Test-Path "$RepoRoot\.git\shallow") {
        Write-Host " SKIP (no git tag baseline; shallow clone — run: git fetch --tags --unshallow)"
    } else {
        Write-Host " SKIP (no git tag baseline)"
    }
    exit 0
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host " SKIP (cargo not installed)"
    exit 0
}
$PrevEap = $ErrorActionPreference
$ErrorActionPreference = "Continue"
& cargo semver-checks --version *> $null
$SemverExit = $LASTEXITCODE
$ErrorActionPreference = $PrevEap
if ($SemverExit -ne 0) {
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
    Write-Host " WARN (baseline $Baseline is HEAD: committed-but-untagged API changes are invisible to this gate)"
}

$Crate = (Select-String -Path "$RepoRoot\Cargo.toml" -Pattern '^name\s*=\s*"(.+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
Write-Host " baseline=$Baseline package=$Crate"
Push-Location $RepoRoot
try {
    & cargo semver-checks check-release --baseline-rev $Baseline --package $Crate
    exit $LASTEXITCODE
} finally {
    Pop-Location
}
