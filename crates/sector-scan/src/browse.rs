//! Live single-directory listing — the data layer for the file-explorer's
//! navigation (D16 / Phase E). Unlike the deep [`crate::walk`] scan (which
//! enumerates a whole subtree into a tree), this reads just ONE directory's
//! immediate contents on demand, fast, reflecting the filesystem right now.
//!
//! OS-agnostic `std::fs`, so it's testable on Linux; on Windows the sizes come
//! from the directory enumeration for free (see the scan-speed notes).

use std::path::Path;
use std::time::SystemTime;

/// One immediate child of a directory.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    /// File size in bytes. Directories report 0 here (their real weight needs a
    /// deep scan — that's the "City" mode's job).
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
    /// A symlink/reparse point (shown but not auto-traversed).
    pub is_symlink: bool,
    /// Hidden or system entry (Windows HIDDEN/SYSTEM attributes) — the explorer
    /// hides these by default.
    pub is_hidden: bool,
    pub readonly: bool,
}

/// Is this metadata a Windows reparse point (junction / mount point / symlink)?
#[cfg(windows)]
fn is_reparse_point(md: Option<&std::fs::Metadata>) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    md.map(|m| m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
        .unwrap_or(false)
}
#[cfg(not(windows))]
fn is_reparse_point(_md: Option<&std::fs::Metadata>) -> bool {
    false
}

/// Is this a hidden or system entry? (Windows HIDDEN | SYSTEM attributes.)
#[cfg(windows)]
fn is_hidden_entry(md: Option<&std::fs::Metadata>) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
    md.map(|m| m.file_attributes() & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0)
        .unwrap_or(false)
}
#[cfg(not(windows))]
fn is_hidden_entry(_md: Option<&std::fs::Metadata>) -> bool {
    false
}

/// List a directory's immediate entries. Per-entry errors (a file that vanished,
/// a permission glitch) are skipped rather than failing the whole listing; only
/// a failure to open the directory itself returns `Err`.
pub fn list_dir(path: &Path) -> std::io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for item in std::fs::read_dir(path)? {
        let Ok(dirent) = item else { continue };
        // `file_type`/`metadata` on a DirEntry do NOT follow symlinks.
        let Ok(ft) = dirent.file_type() else { continue };
        let md = dirent.metadata().ok();
        let is_dir = ft.is_dir();
        // Treat Windows junctions / mount points (reparse points) as symlinks too
        // — `FileType::is_symlink` misses them, and a junction pointing at an
        // ancestor would otherwise loop the folder tree. `DirEntry::metadata`
        // does not follow the reparse point, so these attributes are the link's.
        let is_symlink = ft.is_symlink() || is_reparse_point(md.as_ref());
        let is_hidden = is_hidden_entry(md.as_ref());
        let readonly = md.as_ref().map(|m| m.permissions().readonly()).unwrap_or(false);
        out.push(Entry {
            name: dirent.file_name().to_string_lossy().into_owned(),
            is_dir,
            size: if is_dir { 0 } else { md.as_ref().map(|m| m.len()).unwrap_or(0) },
            modified: md.as_ref().and_then(|m| m.modified().ok()),
            created: md.as_ref().and_then(|m| m.created().ok()),
            is_symlink,
            is_hidden,
            readonly,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_dir() -> std::path::PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "sector-browse-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn lists_files_and_dirs() {
        let root = unique_dir();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("a.txt"), vec![0u8; 123]).unwrap();
        fs::write(root.join("b.bin"), vec![0u8; 4096]).unwrap();

        let mut entries = list_dir(&root).unwrap();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(entries.len(), 3);

        let a = entries.iter().find(|e| e.name == "a.txt").unwrap();
        assert!(!a.is_dir);
        assert_eq!(a.size, 123);

        let sub = entries.iter().find(|e| e.name == "sub").unwrap();
        assert!(sub.is_dir);
        assert_eq!(sub.size, 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_dir_errors() {
        assert!(list_dir(std::path::Path::new("/no/such/sector/dir")).is_err());
    }
}
