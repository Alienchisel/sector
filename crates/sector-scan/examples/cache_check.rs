//! Quick check: scan a path, save + reload the cache, report size and timing.
//!   cargo run --release --example cache_check -- <path>
use std::sync::atomic::AtomicBool;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use sector_core::{CacheStats, Tree};
use sector_scan::{scan, ScanOptions};

fn main() {
    let path = std::env::args().nth(1).expect("usage: cache_check <path>");
    let cancel = AtomicBool::new(false);
    let (tree, stats) = scan(path.as_ref(), &ScanOptions { threads: 16 }, &cancel, None);
    println!("scanned {} nodes ({} files)", tree.len(), stats.files);

    let cs = CacheStats {
        dirs: stats.dirs,
        files: stats.files,
        bytes: stats.bytes,
        saved_unix: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
    };
    let out = std::env::temp_dir().join("sector_cache_check.bin");

    let t0 = Instant::now();
    tree.save_cache(&out, cs).unwrap();
    let save_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let file_bytes = std::fs::metadata(&out).unwrap().len();

    let t1 = Instant::now();
    let (r, _rs) = Tree::load_cache(&out).unwrap();
    let load_ms = t1.elapsed().as_secs_f64() * 1000.0;

    println!(
        "cache: {:.1} MB · save {save_ms:.0}ms · load {load_ms:.0}ms · {} bytes/node",
        file_bytes as f64 / 1_048_576.0,
        file_bytes / tree.len().max(1) as u64,
    );
    assert_eq!(r.len(), tree.len());
    assert_eq!(r.node(Tree::ROOT).subtree_size, tree.node(Tree::ROOT).subtree_size);
    assert_eq!(r.node(Tree::ROOT).file_count, tree.node(Tree::ROOT).file_count);
    println!("reload verified (nodes, size, file_count match)");
    let _ = std::fs::remove_file(&out);
}
