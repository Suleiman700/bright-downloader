//! Free-space queries, so a download fails up front instead of half way.

use std::path::Path;

/// Bytes available to this user on the volume holding `path`. The path need
/// not exist yet; the nearest existing ancestor is asked. `None` if unknown.
pub fn available_space(path: &Path) -> Option<u64> {
    let existing = path.ancestors().find(|p| p.exists())?;
    query(existing)
}

#[cfg(windows)]
fn query(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut free = 0u64;
    // SAFETY: `wide` is NUL-terminated and outlives the call; null pointers
    // are allowed for the totals we do not need.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
    (ok != 0).then_some(free)
}

#[cfg(not(windows))]
fn query(_dir: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn reports_space_for_missing_subfolder() {
        let dir = tempfile::tempdir().unwrap();
        let free = available_space(&dir.path().join("not").join("yet")).unwrap();
        assert!(free > 0);
    }
}
