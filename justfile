# snubber task runner (https://github.com/casey/just).
#
# Every recipe is a thin wrapper over scripts/ or the documented cargo
# commands, so a human, CI and the snubber-release skill run the same
# sequence. Install: cargo install just (or a package manager).

# List the available recipes.
default:
	@just --list

# Fast gates: fmt, clippy, suite + doctests + badge, compile gates.
check:
	bash scripts/release.sh --fast

# The #[ignore] lanes in release mode (pool storms, breaker churn, termination).
release-lanes:
	cargo test --release --locked --test supervisor_pool_stress --test supervisor_pool_stress_lease --test supervisor_pool_stress_mixed --test stress_breaker_churn -- --ignored
	cargo test --release --locked --test supervisor_termination -- --ignored

# Full release sequence: fast gates + release lanes + identity + publish dry-run.
release:
	bash scripts/release.sh

# Package exactly what cargo publish would upload, without uploading.
package-dry:
	cargo publish --dry-run

# Upload to crates.io. Only on an explicit order, after `just release` is green.
publish:
	cargo publish
