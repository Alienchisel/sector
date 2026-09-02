//! Benchmark the concurrent walker at several thread counts.
//!
//!   cargo run --release --example scan_bench -- <path> [threads,csv]
//!
//! e.g. cargo run --release --example scan_bench -- /mnt/nas/manga 1,4,8,16,32,64,1
//!
//! The trailing repeat of a thread count (…,64,1) helps spot CIFS/OS caching:
//! if the second `1` is much faster than the first, later runs are riding a warm
//! cache and the raw numbers are optimistic.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use sector_core::human_size;
use sector_scan::{scan, ScanOptions};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = PathBuf::from(args.next().unwrap_or_else(|| {
        eprintln!("usage: scan_bench <path> [threads,csv]");
        std::process::exit(2);
    }));
    let thread_list: Vec<usize> = args
        .next()
        .unwrap_or_else(|| "1,4,8,16,32,64,1".to_string())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    println!("target: {}", path.display());
    // We only stat metadata, never read file contents, so the meaningful
    // throughput is entries enumerated per second — not bytes/s.
    println!(
        "{:>8}  {:>9}  {:>10}  {:>12}  {:>12}  {:>13}",
        "threads", "elapsed", "dirs", "files", "apparent", "entries/s"
    );

    let cancel = AtomicBool::new(false);
    for &threads in &thread_list {
        let t0 = Instant::now();
        let (_tree, stats) = scan(&path, &ScanOptions { threads }, &cancel, None);
        let secs = t0.elapsed().as_secs_f64();
        let entries_per_s = (stats.dirs + stats.files) as f64 / secs;
        println!(
            "{:>8}  {:>8.2}s  {:>10}  {:>12}  {:>12}  {:>13.0}{}",
            threads,
            secs,
            stats.dirs,
            stats.files,
            human_size(stats.bytes),
            entries_per_s,
            if stats.errors > 0 { format!("  ({} errors)", stats.errors) } else { String::new() },
        );
    }
}
