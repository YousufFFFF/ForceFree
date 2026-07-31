//! Hard link accounting — the difference between "how big is this" and "how
//! much would deleting it give me back".
//!
//! pnpm and similar package managers keep one physical copy of a file in a
//! global store and hard link it into every project that needs it.
//! `metadata.len()` reports the full size for each link, so a naive sum claims
//! hundreds of megabytes that deletion would not return: dropping a link only
//! decrements the count, and the bytes stay put until the last one goes.
//!
//! **A link count above one is not enough to conclude anything.** Cargo hard
//! links its finished artifacts from `target/debug/deps/` up into
//! `target/debug/`, so those files have two links — but both are inside the
//! directory being deleted, and removing it frees every byte. pnpm's second link
//! lives in a store outside the project, and removing the project frees nothing.
//! The question is therefore not "how many links" but **"are they all inside the
//! thing we are about to delete"**, which needs file identity, not just a count:
//!
//! ```text
//! reclaimable  iff  link_count == times_this_file_appears_inside_the_target
//! ```
//!
//! Unix answers both parts from the `stat` the walk already performs, for free.
//! Windows exposes neither on stable std — `MetadataExt::number_of_links` and
//! `file_index` are unstable behind `windows_by_handle` (rust#63010) — so it
//! needs a handle and `GetFileInformationByHandle`. That handle is opened with
//! *zero* desired access: asking for read access makes Defender scan the file
//! and OneDrive hydrate cloud placeholders, which is enormously slower and quite
//! unnecessary when all we want is metadata.

use std::fs::Metadata;
use std::path::Path;

/// Identifies the physical file behind a path, so two hard links to the same
/// data compare equal. `(device, inode)` on Unix, `(volume serial, file index)`
/// on Windows.
pub type FileId = (u64, u64);

/// What we need to know about one file to decide whether deleting it frees
/// anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    /// Total hard links to this data, anywhere on the volume.
    pub links: u32,
    pub id: FileId,
}

/// Facts from metadata already in hand. `None` on platforms where std does not
/// expose them, which is the caller's signal to use [`probe`].
#[cfg(unix)]
pub fn from_metadata(meta: &Metadata) -> Option<Facts> {
    use std::os::unix::fs::MetadataExt;
    Some(Facts {
        links: meta.nlink() as u32,
        id: (meta.dev(), meta.ino()),
    })
}

#[cfg(not(unix))]
pub fn from_metadata(_meta: &Metadata) -> Option<Facts> {
    None
}

/// Facts by opening the file for metadata only.
///
/// `None` means we could not tell — a locked file, a permission failure, a cloud
/// placeholder that would not materialise. Callers must treat that as
/// uncertainty rather than as "not shared", or the unknown case silently
/// inflates the reclaimable figure.
#[cfg(windows)]
pub fn probe(path: &Path) -> Option<Facts> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();

    // Safety: `wide` is NUL-terminated and outlives the call; the handle is
    // closed on every path out; `info` is a POD struct we zero before use.
    unsafe {
        let handle = CreateFileW(
            wide.as_ptr(),
            // Zero access: metadata only. GENERIC_READ here would trigger
            // antivirus scanning and cloud-file hydration per file.
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            // Needed so directories can be opened too; harmless for files.
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        let ok = GetFileInformationByHandle(handle, &mut info);
        CloseHandle(handle);
        (ok != 0).then_some(Facts {
            links: info.nNumberOfLinks,
            id: (
                info.dwVolumeSerialNumber as u64,
                ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
            ),
        })
    }
}

#[cfg(not(windows))]
pub fn probe(_path: &Path) -> Option<Facts> {
    None
}

/// Would deleting this directory actually free this file's bytes?
///
/// Only if every link to it is inside the directory. `seen_inside` is how many
/// paths within the target resolved to this same physical file.
pub fn frees_bytes(links: u32, seen_inside: u32) -> bool {
    links <= seen_inside
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unlinked_file_always_frees_its_bytes() {
        assert!(frees_bytes(1, 1));
    }

    /// Cargo's case: two links, both inside `target/`, so deleting it frees the
    /// bytes even though the link count is above one.
    #[test]
    fn links_wholly_inside_the_target_still_free_bytes() {
        assert!(frees_bytes(2, 2));
    }

    /// pnpm's case: a second link lives in the global store, so removing the
    /// project returns nothing.
    #[test]
    fn a_link_outside_the_target_means_nothing_is_freed() {
        assert!(!frees_bytes(2, 1));
        assert!(!frees_bytes(9, 3));
    }
}
