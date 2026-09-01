.PHONY: all configure build release run run-tui test fmt fmt-fix clippy check ci clean install uninstall bench bench-mem

all: build

# One-time (or after a toolchain bump) environment setup: the pinned Rust
# toolchain + components (matches rust-toolchain.toml and CI's
# dtolnay/rust-toolchain@1.97.0 pin -- see that file's and ci.yml's own
# comments for why it's pinned, not `stable`), and Git LFS (crates/ui's
# font assets are LFS objects; a checkout/clone without `git lfs pull`
# leaves only ~130-byte pointer files, which fails ide-ui's compile-time
# TrueType assertion in crates/ui/src/theme/fonts.rs). Does not install a
# C/C++ toolchain (git2's vendored-libgit2/vendored-openssl build needs
# one) -- checked instead, since there's no single cross-platform install
# command for it: Xcode Command Line Tools on macOS, build-essential (or
# equivalent) on Linux, the MSVC Build Tools on Windows.
configure:
	rustup toolchain install 1.97.0 --component rustfmt --component clippy
	@command -v git-lfs >/dev/null 2>&1 || { \
		echo "git-lfs not found. Install it first, then re-run 'make configure':"; \
		echo "  macOS:   brew install git-lfs"; \
		echo "  Linux:   apt install git-lfs   (or your distro's equivalent)"; \
		echo "  Windows: winget install GitHub.GitLFS"; \
		exit 1; \
	}
	git lfs install --local
	git lfs pull
	@command -v cc >/dev/null 2>&1 || command -v cl >/dev/null 2>&1 || { \
		echo "No C/C++ compiler found on PATH -- git2's vendored libgit2/OpenSSL build needs one:"; \
		echo "  macOS:   xcode-select --install"; \
		echo "  Linux:   install build-essential (or your distro's equivalent, e.g. 'base-devel')"; \
		echo "  Windows: install the Visual Studio Build Tools (Desktop development with C++ workload)"; \
		exit 1; \
	}

# Debug build of the whole workspace (binary: target/debug/ide).
build:
	cargo build --workspace --all-targets

# Optimized build (binary: target/release/ide).
release:
	cargo build --workspace --release

run:
	cargo run -p ide-ui

# Terminal UI, via the unified binary's --tui flag. Pass a project
# directory the same way: make run-tui ARGS=path/to/project
run-tui:
	cargo run -p ide-ui -- --tui $(ARGS)

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
# Depends on `configure` so a first-time `make install` also fetches the
# pinned toolchain and the LFS font assets rather than failing partway
# through with an unhelpful compile error.
install: configure
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
