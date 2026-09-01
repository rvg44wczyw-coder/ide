//! Peak-memory scenario harness (`docs/features/perf-baseline.md` §2.2).
//! Run with `cargo run -p ide-core --release --example mem_scenario`.
//! Prints one line: `peak_bytes=<n> final_bytes=<n>`. Comparing two runs
//! (before/after a refactor) is the developer's own job -- no historical
//! tracking is built for this single number.

use ide_core::buffer::Buffer;
use ide_core::project::{DirEntryKind, Project};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

// Zero-sized -- the counters live in the module-level statics below,
// since `GlobalAlloc::alloc`/`dealloc` take `&self`, not `&mut self`, and
// a `static TrackingAllocator` value can't itself hold mutable state.
// Only `unsafe` this file adds, required by `GlobalAlloc`'s own trait
// signature; confined to this example binary (never linked into
// `ide-core`'s library or `ide-ui`'s shipped `ide` binary).
struct TrackingAllocator;

static CURRENT_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl std::alloc::GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        let ptr = std::alloc::System.alloc(layout);
        if !ptr.is_null() {
            let new = CURRENT_BYTES.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK_BYTES.fetch_max(new, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        std::alloc::System.dealloc(ptr, layout);
        CURRENT_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;

/// Deterministic (no RNG, no timestamps) so `peak_bytes` stays a
/// meaningful baseline run-to-run.
fn synthetic_rust_source(lines: usize) -> String {
    let mut out = String::with_capacity(lines * 32);
    for i in 0..lines {
        out.push_str(&format!(
            "fn function_{i}(x: i32) -> i32 {{ x + {i} /* comment {i} */ }}\n"
        ));
    }
    out
}

fn build_synthetic_project(root: &Path) {
    for a in 0..5 {
        let dir_a = root.join(format!("dir_{a}"));
        fs::create_dir_all(&dir_a).unwrap();
        for b in 0..5 {
            let dir_b = dir_a.join(format!("sub_{b}"));
            fs::create_dir_all(&dir_b).unwrap();
            for f in 0..8 {
                fs::write(
                    dir_b.join(format!("file_{f}.rs")),
                    synthetic_rust_source(50),
                )
                .unwrap();
            }
        }
    }
    // A handful of larger "hot" files, matching a real project's mix of
    // mostly-small files plus a few big ones.
    for i in 0..5 {
        fs::write(
            root.join(format!("large_{i}.rs")),
            synthetic_rust_source(5_000),
        )
        .unwrap();
    }
}

fn flatten_files(entry: &ide_core::project::DirEntry, out: &mut Vec<std::path::PathBuf>) {
    match entry.kind {
        DirEntryKind::File => out.push(entry.path.clone()),
        DirEntryKind::Dir => {
            for child in &entry.children {
                flatten_files(child, out);
            }
        }
    }
}

fn main() {
    let dir = tempfile::tempdir().expect("create scenario tempdir");
    build_synthetic_project(dir.path());

    let project = Project::open(dir.path()).expect("open synthetic project");
    let tree = project.scan_tree();

    let mut files = Vec::new();
    flatten_files(&tree, &mut files);

    let mut buffers: Vec<Buffer> = files
        .iter()
        .map(|path| Buffer::open(path).expect("open synthetic file as a buffer"))
        .collect();

    // Fixed batch of edits on the largest few files -- exercises the
    // insert/delete path the CPU benchmarks also cover, but under this
    // harness's memory accounting instead of criterion's timing.
    for buffer in buffers.iter_mut().rev().take(5) {
        let mid = buffer.text().len() / 2;
        buffer.insert(mid, "// inserted by mem_scenario\n");
        let start = buffer.text().len() / 2;
        let end = (start + 40).min(buffer.text().len());
        buffer.delete(start..end);
    }

    let peak = PEAK_BYTES.load(Ordering::Relaxed);
    // Drop everything before the final read so `final_bytes` reflects
    // what's left once the scenario's own data is freed -- a rough leak
    // smell-test, not a hard leak detector.
    drop(buffers);
    drop(tree);
    drop(project);
    drop(dir);
    let final_bytes = CURRENT_BYTES.load(Ordering::Relaxed);

    println!("peak_bytes={peak} final_bytes={final_bytes}");
}
