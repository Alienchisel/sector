//! NTFS USN Change Journal — cheap freshness detection for a cached local scan
//! (E6, slice 1). We record a *watermark* (the journal id + next-USN) when a
//! scan completes, then later ask whether the volume's journal has advanced past
//! it. This tells us "is the cached view still current?" without re-walking.
//!
//! Windows + local NTFS only. Network drives (SMB) have no USN journal — those
//! come back [`Freshness::Unknown`] and the caller falls back to "rescan to be
//! sure". Everything here is `#[cfg(windows)]`; other targets get stubs so the
//! rest of the workspace still builds and checks on Linux.

use std::path::Path;

/// A point-in-time marker for a volume's USN journal. Stored in the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsnMark {
    pub journal_id: u64,
    pub next_usn: i64,
}

impl UsnMark {
    /// A "no journal" marker (what we store when USN isn't available).
    pub const NONE: UsnMark = UsnMark { journal_id: 0, next_usn: 0 };

    pub fn is_some(&self) -> bool {
        self.journal_id != 0
    }
}

/// Whether a cached scan is still current, per the USN journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// The journal hasn't advanced since the watermark — nothing changed.
    Current,
    /// The journal advanced — something on the volume changed since the scan.
    Stale,
    /// Can't tell (not local NTFS, journal disabled/reset/wrapped, or an error).
    Unknown,
}

#[cfg(windows)]
mod imp {
    use super::{Freshness, UsnMark};
    use std::os::raw::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Component, Path, Prefix};

    type Handle = isize;
    const INVALID_HANDLE_VALUE: Handle = -1;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const OPEN_EXISTING: u32 = 3;
    const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00f4;
    const DRIVE_FIXED: u32 = 3;

    extern "system" {
        fn GetDriveTypeW(root: *const u16) -> u32;
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *const c_void,
            disposition: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
        fn DeviceIoControl(
            handle: Handle,
            code: u32,
            in_buf: *const c_void,
            in_size: u32,
            out_buf: *mut c_void,
            out_size: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
    }

    /// `USN_JOURNAL_DATA_V0` (the query result we care about).
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct UsnJournalDataV0 {
        usn_journal_id: u64,
        first_usn: i64,
        next_usn: i64,
        lowest_valid_usn: i64,
        max_usn: i64,
        maximum_size: u64,
        allocation_delta: u64,
    }

    /// A RAII wrapper so the volume handle is always closed.
    struct Volume(Handle);
    impl Drop for Volume {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    /// Open the volume that `path` lives on (e.g. `\\.\C:`), read-only. Only
    /// drive-letter paths — UNC / network locations return `None`.
    fn open_volume(path: &Path) -> Option<Volume> {
        let drive = path.components().find_map(|c| match c {
            Component::Prefix(p) => match p.kind() {
                Prefix::Disk(d) | Prefix::VerbatimDisk(d) => Some(d),
                _ => None,
            },
            _ => None,
        })?;
        // Only local FIXED volumes have a usable USN journal. Checking the drive
        // type first also avoids a blocking CreateFile on a mapped network drive
        // (which has a disk letter but no local volume device) — important since
        // this runs on the UI thread.
        let root: Vec<u16> = std::ffi::OsStr::new(&format!("{}:\\", drive as char))
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        if unsafe { GetDriveTypeW(root.as_ptr()) } != DRIVE_FIXED {
            return None;
        }
        let device = format!("\\\\.\\{}:", drive as char);
        let wide: Vec<u16> =
            std::ffi::OsStr::new(&device).encode_wide().chain(std::iter::once(0)).collect();
        // dwDesiredAccess = 0: enough for FSCTL_QUERY_USN_JOURNAL and obtainable
        // WITHOUT administrator (GENERIC_READ on a raw volume would need elevation
        // — SECTOR runs unprivileged, D8). Reading journal *records* (a later
        // slice) may need more; querying the watermark does not.
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                0,
            )
        };
        if h == INVALID_HANDLE_VALUE || h == 0 {
            None
        } else {
            Some(Volume(h))
        }
    }

    /// Query the volume's current journal data, or `None` if there's no active
    /// journal / not accessible.
    fn query(vol: &Volume) -> Option<UsnJournalDataV0> {
        let mut data = UsnJournalDataV0::default();
        let mut returned: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                vol.0,
                FSCTL_QUERY_USN_JOURNAL,
                std::ptr::null(),
                0,
                &mut data as *mut _ as *mut c_void,
                std::mem::size_of::<UsnJournalDataV0>() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(data)
    }

    pub fn query_mark(path: &Path) -> Option<UsnMark> {
        let vol = open_volume(path)?;
        let d = query(&vol)?;
        Some(UsnMark { journal_id: d.usn_journal_id, next_usn: d.next_usn })
    }

    pub fn freshness(path: &Path, mark: &UsnMark) -> Freshness {
        if !mark.is_some() {
            return Freshness::Unknown;
        }
        let Some(vol) = open_volume(path) else { return Freshness::Unknown };
        let Some(d) = query(&vol) else { return Freshness::Unknown };
        // A different journal id means it was deleted+recreated (USN reset); if
        // the journal has wrapped past our watermark we also can't reason about
        // it. Either way: unknown → the caller should rescan to be sure.
        if d.usn_journal_id != mark.journal_id || d.first_usn > mark.next_usn {
            return Freshness::Unknown;
        }
        match d.next_usn.cmp(&mark.next_usn) {
            std::cmp::Ordering::Greater => Freshness::Stale,
            std::cmp::Ordering::Equal => Freshness::Current,
            // Backwards without an id change shouldn't happen — treat a garbage
            // watermark as unknowable rather than falsely "current".
            std::cmp::Ordering::Less => Freshness::Unknown,
        }
    }
}

/// Query the current USN watermark for the volume containing `path`.
/// `None` if it's not a local NTFS drive with an active journal.
#[cfg(windows)]
pub fn query_mark(path: &Path) -> Option<UsnMark> {
    imp::query_mark(path)
}

/// Is a cached scan (taken at `mark`) still current for `path`'s volume?
#[cfg(windows)]
pub fn freshness(path: &Path, mark: &UsnMark) -> Freshness {
    imp::freshness(path, mark)
}

#[cfg(not(windows))]
pub fn query_mark(_path: &Path) -> Option<UsnMark> {
    None
}

#[cfg(not(windows))]
pub fn freshness(_path: &Path, _mark: &UsnMark) -> Freshness {
    Freshness::Unknown
}
