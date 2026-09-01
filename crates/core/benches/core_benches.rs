//! CPU micro-benchmarks (`docs/features/perf-baseline.md`). Run with
//! `cargo bench -p ide-core`; criterion writes its own historical
//! comparison data to `target/criterion/` and reports a run-over-run
//! percentage change automatically.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use ide_core::buffer::Buffer;
use ide_core::project::{DirEntryKind, Project};
use ide_core::syntax::{self, RUST};
use std::fs;
use std::path::Path;

/// Deterministic (no RNG, no timestamps) so criterion's regression
/// percentage stays meaningful run-to-run (`docs/features/
/// perf-baseline.md` §3.2).
fn synthetic_rust_source(lines: usize) -> String {
    let mut out = String::with_capacity(lines * 32);
    for i in 0..lines {
        out.push_str(&format!(
            "fn function_{i}(x: i32) -> i32 {{ x + {i} /* comment {i} */ }}\n"
        ));
    }
    out
}

fn buffer_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("buffer_insert");
    for (name, payload) in [
        ("one_char", "x".to_string()),
        (
            "one_line",
            "let value = compute_something(42);\n".to_string(),
        ),
        ("one_kb_paste", "x".repeat(1024)),
    ] {
        group.bench_with_input(BenchmarkId::from_parameter(name), &payload, |b, payload| {
            b.iter(|| {
                let mut buffer = Buffer::untitled();
                buffer.insert(0, &synthetic_rust_source(200));
                let mid = buffer.text().len() / 2;
                buffer.insert(mid, payload);
            });
        });
    }
    group.finish();
}

fn buffer_delete(c: &mut Criterion) {
    let mut group = c.benchmark_group("buffer_delete");
    for (name, delete_len) in [("one_char", 1usize), ("one_line", 40), ("one_kb", 1024)] {
        group.bench_with_input(
            BenchmarkId::from_parameter(name),
            &delete_len,
            |b, &delete_len| {
                b.iter(|| {
                    let mut buffer = Buffer::untitled();
                    buffer.insert(0, &synthetic_rust_source(200));
                    let start = buffer.text().len() / 2;
                    let end = (start + delete_len).min(buffer.text().len());
                    buffer.delete(start..end);
                });
            },
        );
    }
    group.finish();
}

fn tokenize_rust(c: &mut Criterion) {
    let mut group = c.benchmark_group("tokenize_rust");
    for lines in [100usize, 1_000, 10_000] {
        let source = synthetic_rust_source(lines);
        group.bench_with_input(BenchmarkId::from_parameter(lines), &source, |b, source| {
            b.iter(|| syntax::tokenize(source, &RUST));
        });
    }
    group.finish();
}

/// 3 levels deep, 200 files total, plus one symlink cycle and one
/// escaping symlink kept permanently -- doubles as an implicit
/// regression guard against `scan_tree`'s documented `O(unique real
/// directories)` complexity (`docs/features/perf-baseline.md` §3.1).
///
/// `outside` must be a directory the caller owns (its own separate
/// `tempfile::tempdir()`), not a path derived from `root`'s parent --
/// `root`'s parent is the shared OS temp directory, and writing directly
/// into it would leave litter behind with no `TempDir` to clean it up.
fn build_synthetic_tree(root: &Path, outside: &Path) {
    fs::write(outside.join("secret.txt"), "not part of the project").unwrap();

    let mut file_count = 0;
    for a in 0..5 {
        let dir_a = root.join(format!("dir_{a}"));
        fs::create_dir_all(&dir_a).unwrap();
        for b in 0..5 {
            let dir_b = dir_a.join(format!("sub_{b}"));
            fs::create_dir_all(&dir_b).unwrap();
            for f in 0..8 {
                fs::write(
                    dir_b.join(format!("file_{f}.rs")),
                    synthetic_rust_source(20),
                )
                .unwrap();
                file_count += 1;
            }
        }
    }
    assert_eq!(
        file_count, 200,
        "synthetic tree shape drifted from the documented 200 files"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink(root, root.join("dir_0/cycle_link")).unwrap();
        symlink(outside, root.join("dir_0/escape_link")).unwrap();
    }
}

fn scan_tree(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    build_synthetic_tree(dir.path(), outside.path());
    let project = Project::open(dir.path()).unwrap();

    c.bench_function("scan_tree", |b| {
        b.iter(|| {
            let tree = project.scan_tree();
            assert_eq!(tree.kind, DirEntryKind::Dir);
        });
    });
}

criterion_group!(
    benches,
    buffer_insert,
    buffer_delete,
    tokenize_rust,
    scan_tree
);
criterion_main!(benches);
