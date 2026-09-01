.PHONY: all build release run test fmt fmt-fix clippy check ci clean install uninstall bench bench-mem

all: build

# Debug build of the whole workspace (binary: target/debug/ide).
build:
	cargo build --workspace --all-targets

# Optimized build (binary: target/release/ide).
release:
	cargo build --workspace --release

run:
	cargo run -p ide-ui

test:
	cargo test --workspace

fmt:
	cargo fmt --all -- --check

fmt-fix:
	cargo fmt --all

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

# Same four checks .github/workflows/ci.yml's "build" job runs, in the
# same order, so a failure here reproduces locally what CI would report.
check: fmt clippy build test

ci: check

clean:
	cargo clean

# Builds a release binary and installs it as `ide` into ~/.cargo/bin (on
# PATH for anyone using rustup) so it's runnable from any directory.
install:
	cargo install --path crates/ui --locked

uninstall:
	cargo uninstall ide-ui

# CPU micro-benchmarks (docs/features/perf-baseline.md); criterion prints
# a run-over-run percentage change against the previous `bench` run.
bench:
	cargo bench -p ide-core

# Peak-memory scenario harness; prints `peak_bytes=<n> final_bytes=<n>`
# for comparing before/after a refactor by eye.
bench-mem:
	cargo run -p ide-core --release --example mem_scenario
