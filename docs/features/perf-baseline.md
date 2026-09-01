# Performance baseline: CPU + memory benchmarks (Batch F)

## 1. Purpose

The user asked for benchmarks that measure the app's CPU and memory use,
so a later refactor of a hot path can be judged against a real baseline
rather than a guess. Two different kinds of measurement, on purpose:

1. **CPU micro-benchmarks** on a short list of named hot functions
   (buffer edit, directory scan, syntax tokenization) — fast, iterated
   thousands of times, comparable run-to-run.
2. **A whole-scenario peak-memory harness** — one deterministic synthetic
   workload (open a project, scan its tree, open and edit several
   files), reporting peak bytes allocated. Not a criterion-style
   micro-benchmark (criterion measures wall-clock/CPU of pure functions,
   not a process's memory footprint) — a separate, much simpler tool.

Both live in `crates/core` only. `ide-ui`'s hot paths (rendering,
`egui` layout) aren't in scope for this batch — they're not
`ide-core`-internal functions a `rust-core-dev` refactor would touch, and
`egui`'s own immediate-mode painting isn't meaningfully benchmarkable the
way a pure function is. If a future batch wants UI-frame-time
measurement, that's its own doc.

## 2. Interface / API

### 2.1 `crates/core/benches/core_benches.rs` (new, `criterion` harness)

Three benchmark groups, each with 2-3 named cases via `BenchmarkId`:

```rust
fn buffer_insert(c: &mut Criterion);   // Buffer::insert at start/middle/end
                                        // of a pre-populated buffer, for a
                                        // few payload sizes (1 char, 1 line,
                                        // 1KB paste).
fn buffer_delete(c: &mut Criterion);   // Buffer::delete over the same
                                        // shapes of range.
fn tokenize_rust(c: &mut Criterion);   // syntax::tokenize against a fixed
                                        // synthetic Rust source at a few
                                        // sizes (100/1,000/10,000 lines).
fn scan_tree(c: &mut Criterion);       // Project::scan_tree over a fixed
                                        // synthetic on-disk tree (built
                                        // once via a `criterion::
                                        // BenchmarkGroup` setup, not
                                        // per-iteration -- see §3.1).

criterion_group!(benches, buffer_insert, buffer_delete, tokenize_rust, scan_tree);
criterion_main!(benches);
```

`crates/core/Cargo.toml` gains:

```toml
[dev-dependencies]
criterion = "0.5"

[[bench]]
name = "core_benches"
harness = false
```

Run with `cargo bench -p ide-core`. Criterion writes its own historical
comparison data to `target/criterion/` and prints a
"Performance has improved/regressed by X%" line against the *previous*
run automatically — this is the actual baseline-comparison mechanism per
the user's request, not something this doc builds by hand (see §3.1 for
why reinventing that would be redundant).

### 2.2 `crates/core/examples/mem_scenario.rs` (new, plain binary)

```rust
#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;

// Zero-sized -- the counters below are separate `static` items, not
// struct fields, since `GlobalAlloc::alloc`/`dealloc` take `&self`, and
// a `static TrackingAllocator` value can't itself hold `&mut`-style
// state. Only `unsafe` this batch adds, required by `GlobalAlloc`'s own
// trait signature, confined to this example binary (§3.2/§4).
struct TrackingAllocator;

static CURRENT_BYTES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static PEAK_BYTES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn main() {
    // 1. Build a deterministic synthetic project tree (§3.2) in a
    //    tempdir.
    // 2. Project::open + scan_tree.
    // 3. Buffer::open every file the scan found, run a fixed batch of
    //    inserts/deletes on the largest few.
    // 4. Print `peak_bytes=<n> final_bytes=<n>` to stdout, then exit
    //    (tempdir drops, freeing everything -- final_bytes is a rough
    //    leak smell-test, not a hard leak detector).
}
```

Run with `cargo run -p ide-core --release --example mem_scenario`. Output
is one line to stdout; comparing two runs (before/after a refactor) is
the developer's own job (`diff`, or just reading two numbers) — no
historical-tracking mechanism is built for this one, unlike criterion's
built-in comparison, since building that ourselves for a single number
would be the kind of premature infrastructure this project's conventions
already warn against (see §6's rationale for not adding one).

### 2.3 `Makefile` targets

```makefile
bench:
	cargo bench -p ide-core

bench-mem:
	cargo run -p ide-core --release --example mem_scenario
```

## 3. Behaviour

### 3.1 Why `scan_tree`'s synthetic tree is built once, not per-iteration

`Project::scan_tree(&self)` takes `&self` and doesn't mutate the
`Project`/filesystem, and calling it repeatedly is idempotent — so the
benchmark needs exactly one setup step, not criterion's `iter_batched`-
style per-iteration re-setup (which exists for benchmarks whose closure
*consumes or mutates* its input). The benchmark function builds one
synthetic on-disk tree via a `tempfile::tempdir()` created once at the
very top of the function (same shape `crates/core/src/project.rs`'s own
`scan_tree_...` tests already use), calls `Project::open` on it once, and
then `c.bench_function(...)` calls `project.scan_tree()` repeatedly
inside the timed closure — this measures the scan itself, not
directory-creation or `Project::open` noise. Fixed shape: 200 files
across a 3-level-deep directory tree (matches the kind of project size a
real small-to-medium Rust crate has), plus one symlink-cycle and one
escaping-symlink case included permanently (not toggled) so the benchmark
also stays an implicit regression guard against `scan_tree`'s own
documented `O(unique real directories)` complexity claim — a future
change that accidentally reintroduces the `O(branch^levels)` blowup
`docs/security-findings/editor-shell-project-scan-2026-08-16.md` fixed
would show up as a sudden, large regression in criterion's own
percentage-change output, not just a silently slower number nobody
notices.

Reusing criterion's own regression-detection for this, instead of a
dedicated perf-regression test with a hard-coded time threshold, is a
deliberate choice: a hard-coded "must complete under Nms" assertion is
notoriously flaky across different machines/CI runners, where criterion's
run-over-run percentage comparison is inherently relative to whatever
machine it's running on.

### 3.2 Deterministic synthetic fixtures

Both `core_benches.rs` and `mem_scenario.rs` generate their test data
programmatically from a fixed (not random) scheme — file `N` in directory
`D` always gets the exact same generated content and path shape run to
run. This is load-bearing: criterion's regression percentage and
`mem_scenario`'s peak-bytes number are only meaningful baselines if the
workload itself never changes between runs. No `rand`/timestamp-seeded
generation anywhere in either file.

`mem_scenario`'s `TrackingAllocator` (§2.2): wraps `std::alloc::System`,
incrementing an `AtomicUsize` current-bytes counter on every `alloc` (and
`fetch_max`-ing a separate peak-bytes counter), decrementing current-bytes
on every `dealloc`. `Ordering::Relaxed` throughout — this scenario runs
single-threaded by design (a deterministic sequential workload, no reason
to parallelize a benchmark), so the counters have no cross-thread
ordering requirement beyond atomicity itself.

## 4. Constraints & invariants

- Both benchmark files depend only on `ide-core`'s own already-public API
  (`Buffer`, `Project`, `syntax::{tokenize, RUST}`) — no new `pub` surface
  is added to the library itself, only new files under `benches/`/
  `examples/`, which Cargo treats as separate compilation targets, not
  library code.
- `TrackingAllocator`'s `unsafe impl GlobalAlloc` is the only `unsafe` this
  batch introduces, and it's confined to `examples/mem_scenario.rs` — it
  never becomes part of `ide-core`'s library `unsafe` surface (there is
  none today) or `ide-ui`'s binary (a `#[global_allocator]` only applies
  to the binary it's declared in; `cargo run --example` builds a
  completely separate binary from `ide`/`ide-ui`'s own `main.rs`).
- Both fixtures are deterministic (§3.2) — a run against unmodified code
  today and the same run next year should report the same numbers,
  modulo whatever a real code change actually did.
- Neither benchmark touches the network, spawns a subprocess, or reads
  any real user project — both build and tear down their own tempdir.

## 5. Examples

```bash
# CPU: run once to establish a baseline, make a change, run again to see
# criterion's own before/after percentage.
cargo bench -p ide-core

# Memory: run before a refactor, note peak_bytes, run again after.
cargo run -p ide-core --release --example mem_scenario
# peak_bytes=18874368 final_bytes=131072
```

## 6. Dependencies & integration points

New dependency: `criterion = "0.5"`, **dev-dependency only** for
`ide-core` (never a normal `[dependencies]` entry — it's not linked into
`ide-core`'s library or `ide-ui`'s binary, only into the `cargo bench`
harness), the same shape `egui_kittest` already has in `crates/ui/
Cargo.toml`. Needs adding to CLAUDE.md's dependency-approval table before
this role implements it.

No new dependency for the memory harness — `TrackingAllocator` is
hand-written against `std::alloc`, deliberately not pulling in a crate
like `dhat`/`jemalloc-ctl` for this: the ask is a single before/after
peak-bytes number for comparing refactors, which a ~20-line
`GlobalAlloc` wrapper delivers directly, and a heavier profiling
dependency would be solving a different (and not-yet-asked-for) problem
(per-allocation-site attribution, flamegraphs) this batch doesn't need.

Integration points: `Buffer` (`crates/core/src/buffer.rs`), `Project`
(`crates/core/src/project.rs`), `syntax::{tokenize, RUST}`
(`crates/core/src/syntax.rs`) — all pre-existing public API, unmodified
by this batch.

Not security-sensitive: `benches/`/`examples/` aren't on CLAUDE.md's
security-sensitive-paths list, and this batch doesn't touch any file that
is (no `git/`, no `project/` logic changes, no UI/subprocess code). No
`hacker` pass expected.

## 7. Diagram

Skipped — this is two small, independent measurement tools around
already-documented public functions, not a new protocol or component
relationship a diagram would clarify.

## Revision notes

1. §3.1 previously offered two alternative setup mechanisms
   ("`criterion::BenchmarkGroup`'s setup phase, or an outer
   `tempfile::tempdir()`...") for the `scan_tree` benchmark. Since
   `scan_tree` is idempotent and non-mutating, `iter_batched`-style
   per-iteration re-setup (what `BenchmarkGroup`'s setup phase is *for*)
   is unnecessary machinery here — reworded to specify the single,
   simpler, strictly-correct approach: one `tempfile::tempdir()` and one
   `Project::open` at the top of the function, reused by every iteration
   inside the timed closure.
2. §2.2 clarified that `TrackingAllocator`'s current/peak-bytes counters
   are separate module-level `static AtomicUsize` items, not struct
   fields — the original snippet showed a bare `struct TrackingAllocator;`
   with only a comment gesturing at "two AtomicUsize counters" without
   saying where they live, which would have been ambiguous to implement
   against (a struct-field design doesn't actually work here, since
   `GlobalAlloc::alloc`/`dealloc` take `&self`).
