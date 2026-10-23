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
$InCi = [bool]$env:CI

# Gate 1: Check #![forbid(unsafe_code)] in src/lib.rs
Write-Host "[1/6] Auditing unsafe code veto..." -NoNewline
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
Write-Host "[2/6] Auditing saturating counters in src/node.rs..." -NoNewline
# Counter increment check: flag direct `+= 1` on known counter fields
$CounterViolations = Select-String -Path "$RepoRoot\src\node.rs" -CaseSensitive -Pattern "\b(in_flight|total_started|committed|bulkhead_rejections|poison_recoveries|stale_epochs_rejected)\s*\+="
if ($CounterViolations) {
    Write-Host " FAIL: Direct += found on counter:" -ForegroundColor Red
    $CounterViolations | ForEach-Object { Write-Host "   $($_.Filename):$($_.LineNumber): $($_.Line.Trim())" }
    $Failures++
} else {
    Write-Host " OK (saturating math preserved)" -ForegroundColor Green
}

# Gate 3: Lock-hygiene audit in Phase A/C (no foreign calls / formatting under lock)
Write-Host "[3/6] Auditing zero-panic mutex discipline in src/node.rs..." -NoNewline
# Extract the FULL `fn record_locked` body (signature line to the first
# method-indent closing brace) and forbid panicking/formatting calls
# inside it. Select-String is line-based and cannot span lines, so the
# previous single-pattern check only ever saw the signature line.
$NodeLines = Get-Content "$RepoRoot\src\node.rs"
$InFn = $false
$SigIndent = 0
$FnBody = New-Object System.Collections.Generic.List[string]
foreach ($Line in $NodeLines) {
    if (-not $InFn -and $Line -match '^(\s*)fn record_locked\(') {
        $InFn = $true
        $SigIndent = $Matches[1].Length
    }
    if ($InFn) {
        $FnBody.Add($Line)
        # Closing brace at the SIGNATURE's own indent (survives the fn
        # moving one indent deeper; the old fixed `^    \}$` anchor ran
        # to EOF and failed safe but loud).
        if ($Line -match '^(\s*)\}\s*$' -and $Matches[1].Length -eq $SigIndent) { break }
    }
}
# A gate that scans nothing must fail, not pass: a renamed/moved
# record_locked would otherwise yield an empty body and a silent PASS.
if ($FnBody.Count -eq 0 -or ($FnBody[0] -notmatch 'fn record_locked\(')) {
    Write-Host " FAIL: could not extract the fn record_locked body from src\node.rs (renamed or moved?)" -ForegroundColor Red
    $Failures++
} else {
    $FormatUnderLock = $FnBody | Select-String -CaseSensitive -Pattern 'format!|println!|eprintln!|panic!|\.unwrap\(\)|\.expect\('
    if ($FormatUnderLock) {
        Write-Host " FAIL: panicking/formatting call inside record_locked" -ForegroundColor Red
        $FormatUnderLock | ForEach-Object { Write-Host "   body line $($_.LineNumber): $($_.Line.Trim())" }
        $Failures++
    } else {
        Write-Host " OK (no format! under lock)" -ForegroundColor Green
    }
}

# Gate 4: Zero-metaphor scan across codebase (AGENTS.md Section 14)
Write-Host "[4/6] Auditing forbidden metaphors..." -NoNewline
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
    $Found = Select-String -Path $ScanFiles -Pattern $Pat
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
Write-Host "[5/6] Auditing true no_std on bare metal..." -NoNewline
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
    try {
        cargo check --quiet --no-default-features --target $BareMetalTarget
        if ($LASTEXITCODE -eq 0) {
            cargo check --quiet --no-default-features --features alloc --target $BareMetalTarget
        }
        if ($LASTEXITCODE -eq 0) {
            Write-Host " OK (core and alloc build for $BareMetalTarget)" -ForegroundColor Green
        } else {
            Write-Host " FAIL (no_std build failed for $BareMetalTarget)" -ForegroundColor Red
            $Failures++
        }
    } finally {
        Pop-Location
    }
}

# Gate 6: Public API semver diff (cargo-semver-checks when installed)
Write-Host "[6/6] Auditing public API against the last release..." -NoNewline
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
    # Parity with check_api_diff.ps1: a tag equal to HEAD hides every
    # committed-but-untagged API change; warn instead of trivially passing.
    $HeadRev = & git -C "$RepoRoot" rev-parse HEAD 2>$null
    $TagRev = & git -C "$RepoRoot" rev-parse "$ApiBaseline^{}" 2>$null
    if ($HeadRev -and $TagRev -and $HeadRev -eq $TagRev) {
        Write-Host " WARN (baseline $ApiBaseline == HEAD; committed-but-untagged API changes are invisible)" -ForegroundColor Yellow
    }
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
        $Crate = (Select-String -Path "$RepoRoot\Cargo.toml" -Pattern '^name *= *"([^"]+)"').Matches[0].Groups[1].Value
        Push-Location $RepoRoot
        try {
            $PrevEap = $ErrorActionPreference
            $ErrorActionPreference = "Continue"
            & cargo semver-checks check-release --baseline-rev $ApiBaseline --package $Crate *> $null
            $SemverExit = $LASTEXITCODE
            $ErrorActionPreference = $PrevEap
            if ($SemverExit -eq 0) {
                Write-Host " OK (no breaking change vs $ApiBaseline)" -ForegroundColor Green
            } else {
                Write-Host " FAIL (breaking API change vs ${ApiBaseline}: run scripts/check_api_diff.ps1)" -ForegroundColor Red
                $Failures++
            }
        } finally {
            Pop-Location
        }
    }
}

Write-Host "---------------------------------------------"
if ($Skips -gt 0) {
    # Never a PASS-looking summary over a reduced audit: the skip count
    # is on its own line, next to the verdict.
    Write-Host "$Skips gate(s) SKIPPED -- the audit ran at reduced coverage." -ForegroundColor Yellow
}
if ($Failures -eq 0) {
    Write-Host "All invariant gates PASSED." -ForegroundColor Green
    exit 0
} else {
    Write-Host "$Failures invariant check(s) FAILED." -ForegroundColor Red
    exit 1
}
