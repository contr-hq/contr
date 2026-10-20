//! Free space of the file system that holds a path.

use std::io;
use std::path::Path;

/// Bytes the current user may still write on the file system that holds `path`.
#[cfg(unix)]
#[allow(
    clippy::useless_conversion,
    reason = "the block count is narrower than u64 on some Unix targets"
)]
pub fn free_bytes(path: &Path) -> io::Result<u64> {
    let stats = nix::sys::statvfs::statvfs(path)?;

    Ok(u64::from(stats.blocks_available()).saturating_mul(stats.fragment_size()))
}

/// Bytes the current user may still write on the volume that holds `path`.
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn free_bytes(path: &Path) -> io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut available = 0u64;
    // SAFETY: `wide` is NUL-terminated and outlives the call; the one output pointer is valid and the others are null.
    let succeeded = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &raw mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(available)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_space_on_an_existing_path() {
        let dir = tempfile::tempdir().unwrap();

        assert!(free_bytes(dir.path()).unwrap() > 0);
        assert!(free_bytes(&dir.path().join("missing")).is_err());
    }
}
