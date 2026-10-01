.PHONY: check fmt-check clippy test

# Run the full local check (same commands as CI): fmt, clippy, then tests.
# Stops on the first failure.
check: fmt-check clippy test

fmt-check:
	cargo fmt --all -- --check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

test:
	cargo test --workspace
