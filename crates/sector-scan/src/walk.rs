//! A concurrent directory walker.
//!
//! # Design (see docs/DECISIONS.md D7)
//!
//! A producer/consumer pipeline that keeps parallel I/O separate from
//! tree-building:
//!
//! - A pool of **worker threads** each `read_dir` one directory and `stat` its
//!   entries. This is the slow, latency-bound part over SMB, and running many
//!   directories at once is what hides that latency.
//! - A single **consumer** (the calling thread) owns the [`Tree`] and builds it
//!   serially — cheap, no locking. It assigns a [`NodeId`] to every entry and
//!   queues each discovered subdirectory back to the pool with its new id.
//!
//! Concurrency here is *per-directory*: N directories are walked at once, but the
//! files within one directory are `stat`ed sequentially by the worker that owns
//! it. That is a good fit for wide trees (a NAS collection of many folders). If
//! benchmarks show a wide-but-shallow layout starving the pool, per-file `stat`
//! parallelism is the next refinement — but measure first.
//!
//! Symlinks are **not** followed (we `lstat` via `DirEntry::metadata`, which does
//! not traverse), so cycles can't trap the walk and we don't double-count.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crossbeam_channel::unbounded;
use sector_core::{NodeId, NodeKind, Tree};

/// Options controlling a scan.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Number of worker threads walking directories concurrently. For SMB this
    /// wants to be high (latency hiding); for a local SSD, near the core count.
    pub threads: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        ScanOptions { threads }
    }
}

/// Live, lock-free progress counters. Share a reference across the scan and read
/// it from another thread (e.g. the UI) while the scan runs. All fields are
/// monotonic during a scan.
#[derive(Debug, Default)]
pub struct Progress {
    pub dirs: AtomicU64,
    pub files: AtomicU64,
    pub bytes: AtomicU64,
    pub errors: AtomicU64,
}

/// Final tally of a completed (or cancelled) scan.
#[derive(Debug, Clone)]
pub struct ScanStats {
    pub dirs: u64,
    pub files: u64,
    pub bytes: u64,
    /// Entries or directories that could not be read (permissions, disconnects).
    /// A scan tolerates these and keeps going rather than aborting.
    pub errors: u64,
    pub elapsed: Duration,
    pub cancelled: bool,
}

// ---- internal pipeline messages -------------------------------------------

/// A directory the pool still needs to walk, and where to attach its contents.
struct DirJob {
    path: PathBuf,
    parent: NodeId,
}

/// One entry discovered inside a directory.
struct WalkEntry {
    name: String,
    /// File apparent size in bytes; 0 for directories.
    size: u64,
    /// `Some(child_path)` if this entry is a directory to recurse into.
    dir_path: Option<PathBuf>,
}

/// A worker's result for one directory.
struct DirResult {
    parent: NodeId,
    entries: Vec<WalkEntry>,
    errors: u64,
}

/// Read a single directory (no recursion). Runs on a worker thread. Returns the
/// entries found and a count of unreadable items; the caller attaches `parent`.
fn read_one_dir(path: &Path) -> (Vec<WalkEntry>, u64) {
    let mut entries = Vec::new();
    let mut errors = 0u64;

    let rd = match std::fs::read_dir(path) {
        Ok(rd) => rd,
        // Whole directory unreadable (permissions, vanished, disconnect).
        Err(_) => return (entries, 1),
    };

    for item in rd {
        let dirent = match item {
            Ok(d) => d,
            Err(_) => {
                errors += 1;
                continue;
            }
        };
        let name = dirent.file_name().to_string_lossy().into_owned();

        // `file_type()` and `metadata()` on a DirEntry do NOT follow symlinks.
        let ft = match dirent.file_type() {
            Ok(t) => t,
            Err(_) => {
                errors += 1;
                continue;
            }
        };

        if ft.is_dir() {
            entries.push(WalkEntry {
                name,
                size: 0,
                dir_path: Some(dirent.path()),
            });
        } else {
            // Files and symlinks are leaves (we don't traverse symlinks). Use
            // the entry's own (l)stat size; on error, count it and record 0.
            let size = match dirent.metadata() {
                Ok(m) => m.len(),
                Err(_) => {
                    errors += 1;
                    0
                }
            };
            entries.push(WalkEntry {
                name,
                size,
                dir_path: None,
            });
        }
    }

    (entries, errors)
}

/// Scan `root` into a fresh [`Tree`], using `opts.threads` concurrent workers.
///
/// - `cancel`: set to `true` from any thread to stop early; the partially built
///   tree is returned with `stats.cancelled = true`.
/// - `progress`: optional live counters updated as the scan runs.
///
/// `subtree_size` is valid on every node when this returns (maintained
/// incrementally — see [`scan_into`]).
pub fn scan(
    root: &Path,
    opts: &ScanOptions,
    cancel: &AtomicBool,
    progress: Option<&Progress>,
) -> (Tree, ScanStats) {
    let tree = Mutex::new(Tree::new(root.to_string_lossy().into_owned()));
    let stats = scan_into(root, &tree, opts, cancel, progress);
    (tree.into_inner().unwrap(), stats)
}

/// Scan `root` into a **shared** tree that another thread can read *while the
/// scan runs* — the basis of the live "discovery" render (D12).
///
/// `tree` must already contain its root node (e.g. `Tree::new("C:")`); entries
/// are inserted under [`Tree::ROOT`]. Only this call's consumer touches the tree
/// (workers just do I/O), so the lock is contended only with the reader (UI).
/// The lock is taken once per directory (coarse granularity), and inserts use
/// [`Tree::add_child_propagating`] so `subtree_size` is always current for a
/// mid-scan reader — no `recompute_sizes` pass.
pub fn scan_into(
    root: &Path,
    tree: &Mutex<Tree>,
    opts: &ScanOptions,
    cancel: &AtomicBool,
    progress: Option<&Progress>,
) -> ScanStats {
    let start = Instant::now();

    let (job_tx, job_rx) = unbounded::<DirJob>();
    let (res_tx, res_rx) = unbounded::<DirResult>();

    job_tx
        .send(DirJob {
            path: root.to_path_buf(),
            parent: Tree::ROOT,
        })
        .expect("job channel open");
    let mut pending: u64 = 1; // outstanding directory jobs (consumer-only counter)

    let mut dirs = 0u64;
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut errors = 0u64;

    let n_workers = opts.threads.max(1);

    std::thread::scope(|scope| {
        // Workers: pull directory jobs, stat their entries, return results.
        for _ in 0..n_workers {
            let job_rx = job_rx.clone();
            let res_tx = res_tx.clone();
            scope.spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    // On cancel, still return an (empty) result so the consumer's
                    // `pending` accounting terminates cleanly.
                    let (entries, errors) = if cancel.load(Ordering::Relaxed) {
                        (Vec::new(), 0)
                    } else {
                        read_one_dir(&job.path)
                    };
                    let result = DirResult {
                        parent: job.parent,
                        entries,
                        errors,
                    };
                    if res_tx.send(result).is_err() {
                        break; // consumer gone
                    }
                }
            });
        }
        drop(res_tx); // workers hold their own clones

        // Consumer: fold each directory's entries into the shared tree.
        while pending > 0 {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let result = match res_rx.recv() {
                Ok(r) => r,
                Err(_) => break, // all workers exited
            };

            // Insert this directory's entries under one lock, collecting the
            // subdirectories to enqueue (after unlocking).
            let mut new_dirs: Vec<(PathBuf, NodeId)> = Vec::new();
            {
                let mut t = tree.lock().expect("tree mutex");
                for e in result.entries {
                    match e.dir_path {
                        Some(child_path) => {
                            let id =
                                t.add_child_propagating(result.parent, e.name, NodeKind::Dir, 0);
                            dirs += 1;
                            if let Some(p) = progress {
                                p.dirs.fetch_add(1, Ordering::Relaxed);
                            }
                            new_dirs.push((child_path, id));
                        }
                        None => {
                            t.add_child_propagating(result.parent, e.name, NodeKind::File, e.size);
                            files += 1;
                            bytes += e.size;
                            if let Some(p) = progress {
                                p.files.fetch_add(1, Ordering::Relaxed);
                                p.bytes.fetch_add(e.size, Ordering::Relaxed);
                            }
                        }
                    }
                }
                if result.errors > 0 {
                    errors += result.errors;
                    if let Some(p) = progress {
                        p.errors.fetch_add(result.errors, Ordering::Relaxed);
                    }
                }
            } // unlock before enqueuing more work

            for (path, parent) in new_dirs {
                job_tx
                    .send(DirJob { path, parent })
                    .expect("job channel open");
                pending += 1;
            }
            pending -= 1;
        }

        drop(job_tx); // idle workers' recv() returns Err -> they exit; scope joins
    });

    ScanStats {
        dirs,
        files,
        bytes,
        errors,
        elapsed: start.elapsed(),
        cancelled: cancel.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Build a small tree on the local (fast) fs and scan it. Each call gets a
    /// UNIQUE directory: `cargo test` runs tests in parallel, so a shared path
    /// would let one test delete/recreate the fixture while another scans it.
    fn make_fixture() -> tempdir::TempTree {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let unique = SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join(format!("sector-walk-test-{}-{}", std::process::id(), unique));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::write(root.join("a/one.bin"), vec![0u8; 100]).unwrap();
        fs::write(root.join("a/two.bin"), vec![0u8; 200]).unwrap();
        fs::write(root.join("a/b/three.bin"), vec![0u8; 50]).unwrap();
        tempdir::TempTree(root)
    }

    // Tiny inline temp-dir guard (no external dev-dependency needed).
    mod tempdir {
        use std::path::PathBuf;
        pub struct TempTree(pub PathBuf);
        impl std::ops::Deref for TempTree {
            type Target = std::path::Path;
            fn deref(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for TempTree {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn scans_counts_and_sizes() {
        let fixture = make_fixture();
        let cancel = AtomicBool::new(false);
        for threads in [1usize, 4, 16] {
            let opts = ScanOptions { threads };
            let (tree, stats) = scan(&fixture, &opts, &cancel, None);
            assert_eq!(stats.files, 3, "threads={threads}");
            assert_eq!(stats.dirs, 3, "a, a/b, empty (threads={threads})"); // a, b, empty
            assert_eq!(stats.bytes, 350, "threads={threads}");
            assert!(!stats.cancelled);
            // Root subtree size must equal total bytes after aggregation.
            assert_eq!(tree.node(Tree::ROOT).subtree_size, 350, "threads={threads}");
        }
    }

    #[test]
    fn progress_matches_stats() {
        let fixture = make_fixture();
        let cancel = AtomicBool::new(false);
        let progress = Progress::default();
        let (_tree, stats) = scan(&fixture, &ScanOptions { threads: 8 }, &cancel, Some(&progress));
        assert_eq!(progress.files.load(Ordering::Relaxed), stats.files);
        assert_eq!(progress.bytes.load(Ordering::Relaxed), stats.bytes);
        assert_eq!(progress.dirs.load(Ordering::Relaxed), stats.dirs);
    }
}
