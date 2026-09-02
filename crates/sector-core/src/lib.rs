//! sector-core — the OS-agnostic heart of SECTOR.
//!
//! Pure logic only: no GUI, no Windows APIs. This is what gets developed and
//! unit-tested on the Linux VM for fast feedback, and compiled into the app on
//! Windows. The real content (arena tree, treemap layout) lands in Steps 1–2;
//! for Step 0 this holds just enough to prove the workspace and the test loop.

pub mod filetype;
pub mod tree;
pub mod treemap;
pub use filetype::{categorize, FileCategory};
pub use tree::{CacheStats, Node, NodeId, NodeKind, Tree};
pub use treemap::{layout, layout_partial, LayoutOptions, Rect, Tile};

/// Display name of the application.
pub const APP_NAME: &str = "SECTOR";

/// Format a byte count the way Windows Explorer presents it: 1024-based steps
/// labelled KB/MB/GB/TB. This is *apparent* size (see docs/DECISIONS.md D10).
///
/// We match Explorer's familiar labels (KB, not KiB) even though the base is
/// 1024, because SECTOR is a Windows tool and should read like one.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_have_no_decimal() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1023), "1023 B");
    }

    #[test]
    fn kilobytes() {
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
    }

    #[test]
    fn larger_units() {
        assert_eq!(human_size(1024u64.pow(2)), "1.0 MB");
        assert_eq!(human_size(1024u64.pow(3)), "1.0 GB");
        assert_eq!(human_size(3 * 1024u64.pow(4)), "3.0 TB");
    }

    #[test]
    fn caps_at_terabytes() {
        // Petabyte-scale values still read in TB rather than overflowing units.
        assert_eq!(human_size(2 * 1024u64.pow(5)), "2048.0 TB");
    }
}
