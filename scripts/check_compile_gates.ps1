# scripts/check_compile_gates.ps1
# Automated audit gate for snubber architectural invariants and AGENTS.md rules.

$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
$RepoRoot = Resolve-Path "$ScriptDir\.."

Write-Host "=== Snubber Architectural Invariant Audit ===" -ForegroundColor Cyan
$Failures = 0
$Skips = 0

# CI mode: GitHub Actions sets CI=true. A SKIP in a controlled
# environment is a hole in the audit, not a pass (parity with the .sh).
# Strict parity with the .sh `in_ci`: only "true"/"1" count as CI, so an
# unrecognised value can never silently turn SKIPs into hard failures.
$CiFlag = if ($env:CI) { $env:CI.ToLowerInvariant() } else { "" }
$InCi = ($CiFlag -eq "true" -or $CiFlag -eq "1")

# Precondition: a missing scanned path must fail, never yield an empty scan.
foreach ($Need in @("src\lib.rs", "src\node.rs", "src", "benches", "examples", "tests")) {
    if (-not (Test-Path (Join-Path $RepoRoot $Need))) {
        Write-Host "FAIL: required path $Need is missing -- gate scan would be empty." -ForegroundColor Red
        exit 1
    }
}

# Gate 1: Check #![forbid(unsafe_code)] in src/lib.rs
Write-Host "[1/7] Auditing unsafe code veto..." -NoNewline
# TrimStart the BOM: a UTF-8 BOM would otherwise defeat every `(?m)^#!`
# anchor below (false-FAIL that fails safe, but loud for nothing).
$LibContent = (Get-Content "$RepoRoot\src\lib.rs" -Raw).TrimStart([char]0xFEFF)
if ($LibContent -cmatch '(?m)^#!\[forbid\(unsafe_code\)\]') {
    Write-Host " OK (forbid(unsafe_code) active)" -ForegroundColor Green
} else {
    Write-Host " FAIL (missing #![forbid(unsafe_code)] in src/lib.rs)" -ForegroundColor Red
    $Failures++
}

# Scan all src/*.rs files for raw `unsafe ` blocks (recursive: pool,
# async_engine, and any future submodule included)
$SrcFiles = Get-ChildItem "$RepoRoot\src" -Recurse -Filter *.rs | ForEach-Object { $_.FullName }
$UnsafeMatches = Select-String -Path $SrcFiles -CaseSensitive -Pattern "\bunsafe\s*\{|\bunsafe\s+fn\b"
if ($UnsafeMatches) {
    Write-Host " FAIL: Found raw unsafe keyword in src/:" -ForegroundColor Red
    $UnsafeMatches | ForEach-Object { Write-Host "   $($_.Filename):$($_.LineNumber): $($_.Line.Trim())" }
    $Failures++
}

# Gate 2: Saturating arithmetic on counters in src/node.rs
Write-Host "[2/7] Auditing saturating counters in src/node.rs..." -NoNewline
# Every monotonic counter the node books -- parity with the .sh gate. A new
# bucket (rate/breaker/memory/evicted/preempted/implicit drops/trips) must
# not be writable with a bare `+=`.
$CounterPattern = 'in_flight|total_started|committed|explicit_aborts|implicit_drops|bulkhead_rejections|poison_recoveries|stale_epochs_rejected|rate_rejected|breaker_rejections|memory_rejections|evicted|preempted|breaker_trips|reported'
$CounterViolations = Select-String -Path "$RepoRoot\src\node.rs" -CaseSensitive -Pattern "(^|[^_a-zA-Z0-9])($CounterPattern)\s*\+=" |
    Where-Object { $_.Line -notmatch '^\s*//' }
if ($CounterViolations) {
    Write-Host " FAIL: Direct += found on counter:" -ForegroundColor Red
    $CounterViolations | ForEach-Object { Write-Host "   $($_.Filename):$($_.LineNumber): $($_.Line.Trim())" }
    $Failures++
} else {
    Write-Host " OK (saturating math preserved)" -ForegroundColor Green
}

# Gate 3: Lock-hygiene audit in Phase A/C (no foreign calls / formatting under lock)
Write-Host "[3/7] Auditing zero-panic mutex discipline in src/node.rs..." -NoNewline
# Extract each lock-path body from its signature line to the closing brace
# at the SIGNATURE's indent. Parity with the .sh gate: all three functions
# are scanned, not only record_locked. Select-String is line-based and
# cannot span lines, so a single-pattern check would only see signatures.
function Get-FnBody {
    param([string]$Path, [string]$Needle)
    $InFn = $false
    $SigIndent = 0
    $Body = New-Object System.Collections.Generic.List[string]
    foreach ($Line in Get-Content $Path) {
        if (-not $InFn -and $Line.Contains($Needle)) {
            $InFn = $true
            if ($Line -match '^(\s*)') { $SigIndent = $Matches[1].Length }
        }
        if ($InFn) {
            $Body.Add($Line)
            if ($Line -match '^(\s*)\}\s*$' -and $Matches[1].Length -eq $SigIndent) { break }
        }
    }
    return ,$Body
}
$NodePath = "$RepoRoot\src\node.rs"
$Gate3Bodies = [ordered]@{
    enter_inner         = Get-FnBody -Path $NodePath -Needle 'fn enter_inner<'
    enter_async_attempt = Get-FnBody -Path $NodePath -Needle 'fn enter_async_attempt<'
    record_locked       = Get-FnBody -Path $NodePath -Needle 'fn record_locked('
}
$Gate3Missing = @()
foreach ($Name in $Gate3Bodies.Keys) {
    $Body = $Gate3Bodies[$Name]
    if ($Body.Count -eq 0 -or -not $Body[0].Contains("fn $Name")) { $Gate3Missing += $Name }
}
$SyncBodies = @($Gate3Bodies["enter_inner"]) + @($Gate3Bodies["record_locked"])
$AsyncBody = @($Gate3Bodies["enter_async_attempt"])
if ($Gate3Missing.Count -gt 0) {
    Write-Host " FAIL: could not extract lock-path bodies from src\node.rs: $($Gate3Missing -join ', ') (renamed or moved?)" -ForegroundColor Red
    $Failures++
} else {
    $SyncHits = $SyncBodies | Select-String -CaseSensitive -Pattern 'format!|println!|eprintln!|panic!|\.unwrap\(\)|\.expect\('
    # enter_async_attempt legitimately uses `expect` for the pre-lock
    # rollback.take(); everything else must stay non-panicking.
    $AsyncHits = $AsyncBody | Select-String -CaseSensitive -Pattern 'format!|println!|eprintln!|panic!|\.unwrap\(\)'
    if ($SyncHits) {
        Write-Host " FAIL: panicking/formatting call inside the sync lock path:" -ForegroundColor Red
        $SyncHits | ForEach-Object { Write-Host "   body line $($_.LineNumber): $($_.Line.Trim())" }
        $Failures++
    } elseif ($AsyncHits) {
        Write-Host " FAIL: panicking/formatting call inside the async lock path:" -ForegroundColor Red
        $AsyncHits | ForEach-Object { Write-Host "   body line $($_.LineNumber): $($_.Line.Trim())" }
        $Failures++
    } else {
        Write-Host " OK (no format!/panic! in enter_inner, enter_async_attempt, record_locked)" -ForegroundColor Green
    }
}

# Gate 4: Zero-metaphor scan across codebase (AGENTS.md Section 14)
Write-Host "[4/7] Auditing forbidden metaphors..." -NoNewline
# Scope: src + benches + examples + tests -- public examples are as
# user-visible as the docs the rule protects (parity with the .sh gate).
# Excludes binary header magic terminology ("magic bytes / magic number") which is technical standard.
$ForbiddenPatterns = @(
    "\bwizard\b",
    "\bspaceship\b",
    "\bbulletproof\b",
    "\bpanacea\b",
    "\bsilver\s+bullet\b"
)
$ScanFiles = @($SrcFiles)
foreach ($Dir in @("benches", "examples", "tests")) {
    $Path = Join-Path $RepoRoot $Dir
    if (Test-Path $Path) {
        $ScanFiles += (Get-ChildItem $Path -Recurse -Filter *.rs | ForEach-Object { $_.FullName })
    }
}
$MetaphorMatches = @()
foreach ($Pat in $ForbiddenPatterns) {
    # CaseSensitive keeps parity with the POSIX gate's grep (no -i): a
    # metaphor spelled in mixed case must fail here the same way it does
    # under bash.
    $Found = Select-String -Path $ScanFiles -CaseSensitive -Pattern $Pat
    if ($Found) { $MetaphorMatches += $Found }
}
if ($MetaphorMatches.Count -gt 0) {
    Write-Host " FAIL: Found informal metaphors in src/ benches/ examples/ tests/:" -ForegroundColor Red
    $MetaphorMatches | ForEach-Object { Write-Host "   $($_.Filename):$($_.LineNumber): $($_.Line.Trim())" }
    $Failures++
} else {
    Write-Host " OK (technical language strictly adhered)" -ForegroundColor Green
}

# Gate 5: TRUE no_std - crate attribute + bare-metal target compile
Write-Host "[5/7] Auditing true no_std on bare metal..." -NoNewline
$BareMetalTarget = "thumbv7em-none-eabihf"
if ($LibContent -cnotmatch '(?m)^#!\[cfg_attr\(not\(feature = "std"\), no_std\)\]') {
    Write-Host " FAIL (missing #![cfg_attr(not(feature = `"std`"), no_std)] in src/lib.rs)" -ForegroundColor Red
    $Failures++
} elseif (-not (Get-Command rustup -ErrorAction SilentlyContinue)) {
    if ($InCi) {
        Write-Host " FAIL (CI: rustup missing -- workflow must install the toolchain)" -ForegroundColor Red
        $Failures++
    } else {
        Write-Host " SKIP (rustup not installed)" -ForegroundColor Yellow
        $Skips++
    }
} elseif (-not ((rustup target list --installed) -contains $BareMetalTarget)) {
    if ($InCi) {
        Write-Host " FAIL (CI: bare-metal target missing -- workflow must run: rustup target add $BareMetalTarget)" -ForegroundColor Red
        $Failures++
    } else {
        Write-Host " SKIP (target not installed: rustup target add $BareMetalTarget)" -ForegroundColor Yellow
        $Skips++
    }
} else {
    Push-Location $RepoRoot
    $PrevRustFlags = $env:RUSTFLAGS
    try {
        # -D warnings: the core-only lane is a product guarantee, not a courtesy.
        $env:RUSTFLAGS = "-D warnings"
        cargo check --locked --quiet --no-default-features --target $BareMetalTarget
        if ($LASTEXITCODE -eq 0) {
            cargo check --locked --quiet --no-default-features --features alloc --target $BareMetalTarget
        }
        if ($LASTEXITCODE -eq 0) {
            Write-Host " OK (core and alloc build for $BareMetalTarget)" -ForegroundColor Green
        } else {
            Write-Host " FAIL (no_std build failed for $BareMetalTarget)" -ForegroundColor Red
            $Failures++
        }
    } finally {
        $env:RUSTFLAGS = $PrevRustFlags
        Pop-Location
    }
}

# Gate 6: Public API semver diff (cargo-semver-checks when installed)
Write-Host "[6/7] Auditing public API against the last release..." -NoNewline
# PS 5.1 wraps native stderr lines in ErrorRecords: under
# $ErrorActionPreference = "Stop" a SKIP-path `git describe` writing to
# stderr would kill the script. Run native calls with Continue locally.
$PrevEap = $ErrorActionPreference
$ErrorActionPreference = "Continue"
$ApiBaseline = & git -C "$RepoRoot" describe --tags --abbrev=0 2>$null
$GitExit = $LASTEXITCODE
$ErrorActionPreference = $PrevEap
if ($GitExit -ne 0 -or -not $ApiBaseline) {
    if ($InCi) {
        Write-Host " FAIL (CI: no git tag baseline -- checkout must fetch tags and the repo must be tagged per release)" -ForegroundColor Red
        $Failures++
    } else {
        Write-Host " SKIP (no git tag baseline)" -ForegroundColor Yellow
        $Skips++
    }
} else {
    $PrevEap = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    & cargo semver-checks --version *> $null
    $SemverInstalled = ($LASTEXITCODE -eq 0)
    $ErrorActionPreference = $PrevEap
    if (-not $SemverInstalled) {
        if ($InCi) {
            Write-Host " FAIL (CI: cargo-semver-checks missing -- workflow must install it)" -ForegroundColor Red
            $Failures++
        } else {
            Write-Host " SKIP (cargo-semver-checks not installed)" -ForegroundColor Yellow
            $Skips++
        }
    } else {
        # Delegate to the standalone gate (parity with the .sh flavor): one
        # implementation of the semver rules, the release-commit fallback and
        # the Cargo.lock guard, so CI exercises check_api_diff.ps1 on every
        # Windows run. No local baseline==HEAD pre-check: duplicating that
        # rule here is how the Windows gate drifted from .sh at v0.6.10.
        $ApiDiffScript = Join-Path $ScriptDir "check_api_diff.ps1"
        & powershell -NoProfile -ExecutionPolicy Bypass -File $ApiDiffScript $ApiBaseline
        if ($LASTEXITCODE -eq 0) {
            Write-Host " OK (no breaking change vs $ApiBaseline)" -ForegroundColor Green
        } else {
            Write-Host " FAIL (public API gate vs ${ApiBaseline}: see the API diff output above)" -ForegroundColor Red
            $Failures++
        }
    }
}

# Gate 7: Settle paths must drop guards, never forget them.
# `mem::forget(guard)` skips drop glue for EVERY field: on the old
# `TrackedGuard` settle path it stranded one `Arc<Mutex<NodeInner>>` per
# transaction (+24 MB over 10k kills; fixed in 0.5.2, commit e890122).
# A disarmed guard drops cleanly -- its Drop sees settled/None and does
# nothing -- so forgetting buys nothing and silently rots back into that leak.
Write-Host "[7/7] Auditing settle paths for mem::forget..." -NoNewline
$ForgetMatches = Select-String -Path $SrcFiles -CaseSensitive -Pattern 'mem::forget' |
    Where-Object { $_.Line -notmatch '^\s*//' }
if ($ForgetMatches) {
    Write-Host " FAIL: mem::forget in src/ bypasses Drop for every field:" -ForegroundColor Red
    $ForgetMatches | ForEach-Object { Write-Host "   $($_.Filename):$($_.LineNumber): $($_.Line.Trim())" }
    $Failures++
} else {
    Write-Host " OK (settled guards drop; no mem::forget)" -ForegroundColor Green
}

Write-Host "---------------------------------------------"
# Cross-language parity fingerprint: CI runs this and check_compile_gates.sh
# on the same tree and requires both lines to match.
$CounterCount = $CounterPattern.Split('|').Count
Write-Host "PARITY: gates=7 counters=$CounterCount functions=$($Gate3Bodies.Count) metaphors=$($ForbiddenPatterns.Count)"
if ($Skips -gt 0) {
    # A skip is a hole, not a pass: reduced coverage must fail the gate.
    Write-Host "$Skips gate(s) SKIPPED -- coverage reduced, audit NOT complete." -ForegroundColor Yellow
    Write-Host "Re-run with the missing tool available (CI provisions every tool)." -ForegroundColor Yellow
    exit 1
}
if ($Failures -eq 0) {
    Write-Host "All invariant gates PASSED." -ForegroundColor Green
    exit 0
} else {
    Write-Host "$Failures invariant check(s) FAILED." -ForegroundColor Red
    exit 1
}
