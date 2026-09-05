//! Filesystem enumeration into a [`sector_core::Tree`].
//!
//! [`walk::scan`] is a concurrent directory walker built on `std::fs`, so it is
//! OS-agnostic — testable and benchmarkable on Linux — and it is the real scan
//! path for mapped network drives / NAS (D7), whose bottleneck is SMB latency,
//! not disk throughput. Windows-only fast paths (MFT/USN) come later.

pub mod browse;
pub mod usn;
pub mod walk;

pub use browse::{list_dir, stat_entry, Entry};
pub use usn::{freshness, query_mark, Freshness, UsnMark};
pub use walk::{scan, scan_into, Progress, ScanOptions, ScanStats};
