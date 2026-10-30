# scripts/check_api_diff.ps1
# Public API semver gate. Compares the working tree against the latest
# reachable git tag with `cargo-semver-checks` when the tool is installed.
#
# Exit codes: 0 = clean or SKIP (tool/tag unavailable), 1 = breaking change,
# 2 = gate misconfiguration (no usable baseline, missing tool under CI, lock
# mutation), 3 = the verifier could not run (tool/toolchain error, not a
# verdict). Parity with check_api_diff.sh; Gate 6 names the class.

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

# Names declared by the `<!-- api-breaks ... -->` block inside the
# `## <version>` section of CHANGELOG.md: one token per line, `#` comments and
# blank lines skipped. This is the published declaration (CI checks out the
# repository and sees it); api-breaks\<version>.txt is a local audit copy.
# Parity with check_api_diff.sh `declared_breaks_block`.
function Get-DeclaredBreaks([string]$Version) {
    $Changelog = Join-Path $RepoRoot "CHANGELOG.md"
    if (Test-Path $Changelog) {
        $Inside = $false
        $Block = $false
        $Tokens = @()
        # The opener is anchored at the start of the line: prose may mention
        # the marker inline, and an unanchored match would read the rest of
        # the section as tokens looking for a terminator.
        foreach ($Line in (Get-Content $Changelog)) {
            if (-not $Inside) {
                if ($Line.StartsWith("## $Version")) { $Inside = $true }
                continue
            }
            if ($Line -match '^## ') { break }
            if (-not $Block) {
                if ($Line -match '^\s*<!--\s*api-breaks') { $Block = $true }
                continue
            }
            if ($Line -match '^\s*-->') { break }
            $Token = $Line.Trim()
            if ($Token -and -not $Token.StartsWith('#')) { $Tokens += $Token }
        }
        if ($Tokens.Count -gt 0) { return $Tokens }
    }
    $BreaksFile = Join-Path $RepoRoot "api-breaks\$Version.txt"
    if (Test-Path $BreaksFile) {
        return @(Get-Content $BreaksFile |
            Where-Object { $_ -match '\S' -and $_ -notmatch '^\s*#' } |
            ForEach-Object { $_.Trim() })
    }
    return @()
}

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
        exit 2
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
        exit 2
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
        exit 2
    }
    Write-Host " SKIP (cargo-semver-checks not installed; cargo install cargo-semver-checks)"
    exit 0
}

# A baseline pointing at HEAD diffs an empty range: the gate would PASS
# trivially unless the worktree is dirty. The one legitimate case is the
# RELEASE COMMIT itself -- the tag was just moved onto HEAD (that is what
# tagging a release means). There the meaningful diff is against the
# PREVIOUS tag; fall back to it instead of failing an honest release.
# Parity with check_api_diff.sh.
$PrevEap = $ErrorActionPreference
$ErrorActionPreference = "Continue"
$BaselineCommit = & git -C "$RepoRoot" rev-parse "${Baseline}^{commit}" 2>$null
$HeadCommit = & git -C "$RepoRoot" rev-parse HEAD 2>$null
$PreviousTag = ""
if ($BaselineCommit -and $BaselineCommit -eq $HeadCommit) {
    $PreviousTag = & git -C "$RepoRoot" describe --tags --abbrev=0 "${Baseline}^" 2>$null
}
$ErrorActionPreference = $PrevEap
if ($BaselineCommit -and $BaselineCommit -eq $HeadCommit) {
    if ($PreviousTag -and $PreviousTag -ne $Baseline) {
        Write-Host " note: $Baseline is HEAD (release commit) -- diffing against previous tag $PreviousTag"
        $Baseline = $PreviousTag
    } else {
        Write-Host " FAIL (baseline $Baseline is HEAD and no previous tag exists: committed-but-untagged API changes are invisible to this gate -- tag the release or pass an older baseline)" -ForegroundColor Red
        exit 2
    }
}

$Crate = (Select-String -Path "$RepoRoot\Cargo.toml" -Pattern '^name\s*=\s*"(.+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
# `--release-type auto` classifies a 0.x -> 0.y bump as MAJOR and then
# runs zero checks -- a vacuous green. Inside 0.x the compatible unit is
# the minor bump, so name it explicitly. Parity with check_api_diff.sh.
$CurrentVersion = (Select-String -Path "$RepoRoot\Cargo.toml" -Pattern '^version\s*=\s*"(.+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
$ReleaseType = if ($CurrentVersion -like "0.*") { "minor" } else { "auto" }
Write-Host " baseline=$Baseline package=$Crate release-type=$ReleaseType"
Push-Location $RepoRoot
$SemverLog = [System.IO.Path]::GetTempFileName()
try {
    # cargo-semver-checks has no --locked flag: snapshot the lockfile and
    # fail if the tool rewrote it (the CI gate cannot see this otherwise).
    $LockPath = Join-Path $RepoRoot "Cargo.lock"
    $LockBefore = if (Test-Path $LockPath) { (Get-FileHash $LockPath -Algorithm SHA256).Hash } else { "" }
    $PrevEap = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    & cargo semver-checks check-release --baseline-rev $Baseline --package $Crate --release-type $ReleaseType 2>&1 |
        Tee-Object -FilePath $SemverLog
    $SemverExit = $LASTEXITCODE
    $ErrorActionPreference = $PrevEap
    $LockAfter = if (Test-Path $LockPath) { (Get-FileHash $LockPath -Algorithm SHA256).Hash } else { "" }
    # An empty hash means the lockfile vanished: `"" -eq ""` would hide any
    # rewrite. Parity with check_api_diff.sh.
    if (-not $LockBefore -or -not $LockAfter) {
        Write-Host " FAIL (could not hash Cargo.lock: it is missing -- lock mutation would be invisible)" -ForegroundColor Red
        exit 2
    }
    if ($LockBefore -ne $LockAfter) {
        Write-Host " FAIL (cargo-semver-checks rewrote Cargo.lock; restore it and pin the toolchain)" -ForegroundColor Red
        exit 2
    }
    if ($SemverExit -eq 0) { exit 0 }
    # 0.48.x exits 1 for both a violation and a tool error; 0.50+ split 100
    # (lint failure) from 101 (tool error). The `--- failure <lint>` heading
    # is printed by both generations, so it decides the legacy mixed case.
    $LogText = if (Test-Path $SemverLog) { [string](Get-Content $SemverLog -Raw) } else { "" }
    if ($SemverExit -eq 100 -or $LogText -match '(?m)^--- failure ') {
        # A 0.x minor bump MAY break, but only by declaration. Parity with
        # check_api_diff.sh: the CHANGELOG `<!-- api-breaks -->` block is the
        # published channel (CI can see it), api-breaks\<version>.txt a local
        # audit copy; an undeclared break is red, a stale entry is red, and
        # tests/doc_sync.rs ties each name to the CHANGELOG PROSE.
        $BreaksFile = Join-Path $RepoRoot "api-breaks\$CurrentVersion.txt"
        # Every "Failed in:" entry counts, not only those whose item kind is a
        # bare lowercase word: a removed inherent method is printed as
        # `HandleWatch::leaks, previously in file ...` with no kind prefix, and
        # a new field of an exhaustively constructible struct arrives as
        # `field SnapshotV1.tape in <path>:<line>` with no "previously" clause.
        $Reported = @($LogText -split "`r?`n" |
            ForEach-Object {
                if ($_ -match '^  (\S.*), previously in file .*$') {
                    ($Matches[1] -replace '^[a-z_]+ ', '')
                }
                elseif ($_ -match '^  (field \S+) in .*\.rs:[0-9]+$') {
                    ($Matches[1] -replace '^[a-z_]+ ', '')
                }
            } |
            Where-Object { $_ } |
            Sort-Object -Unique)
        if ($Reported.Count -eq 0) {
            Write-Host " verdict: cargo-semver-checks reported a semver violation (exit $SemverExit) and no affected item could be parsed"
            exit 1
        }
        $Declared = @(Get-DeclaredBreaks $CurrentVersion)
        if ($Declared.Count -eq 0) {
            Write-Host " verdict: breaking change vs $Baseline with no declared breaks -- add the names below to the"
            Write-Host "         ``<!-- api-breaks ... -->`` block in the CHANGELOG.md $CurrentVersion section"
            if (Test-Path $BreaksFile) {
                Write-Host "         (api-breaks\$CurrentVersion.txt exists but declares nothing)"
            }
            $Reported | ForEach-Object { Write-Host "   undeclared: $_" }
            exit 1
        }
        $Undeclared = $false
        foreach ($item in $Reported) {
            $hit = $false
            foreach ($token in $Declared) { if ($item.Contains($token)) { $hit = $true } }
            if (-not $hit) {
                Write-Host " FAIL (undeclared API break: $item -- add it to the <!-- api-breaks --> block in CHANGELOG.md $CurrentVersion)"
                $Undeclared = $true
            }
        }
        foreach ($token in $Declared) {
            $hit = $false
            foreach ($item in $Reported) { if ($item.Contains($token)) { $hit = $true } }
            if (-not $hit) {
                Write-Host " FAIL (the api-breaks declaration names '$token' but the diff no longer breaks it -- prune it from CHANGELOG.md $CurrentVersion)"
                $Undeclared = $true
            }
        }
        if ($Undeclared) { exit 1 }
        Write-Host " verdict: $($Reported.Count) affected item(s) vs $Baseline, all declared in the CHANGELOG.md $CurrentVersion api-breaks block"
        exit 0
    }
    Write-Host " FAIL (verifier could not run: cargo-semver-checks exit $SemverExit -- tool/toolchain error, not an API verdict)" -ForegroundColor Red
    exit 3
} finally {
    Remove-Item $SemverLog -ErrorAction SilentlyContinue
    Pop-Location
}
