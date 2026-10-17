//! How much room is left on a volume.
//!
//! A download that runs out of disk part-way through fails in the least helpful
//! way: the transfer dies, the partial is discarded, and nothing tells the user
//! that freeing space is what would fix it. Knowing the figure in advance lets
//! the application say so instead.
//!
//! # Why this is not a crate
//!
//! Returning free space needs one platform call — `GetDiskFreeSpaceExW` on
//! Windows — and the whole surface is a single function. A dependency would add
//! a build to every platform for that. The declaration below is the documented
//! Win32 signature; on other platforms the answer is simply "unknown", which
//! every caller already has to handle because a volume can also be a network
//! share that declines to answer.

use std::path::Path;

/// Free bytes available to this process on the volume holding `path`.
///
/// `None` means the figure could not be obtained — an unsupported platform, a
/// path that does not exist yet, or a volume that will not say. Callers must
/// treat that as "unknown", never as "none available".
pub fn available_space(path: &Path) -> Option<u64> {
    platform::available_space(path)
}

#[cfg(windows)]
mod platform {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    #[link(name = "kernel32")]
    extern "system" {
        /// <https://learn.microsoft.com/windows/win32/api/fileapi/nf-fileapi-getdiskfreespaceexw>
        ///
        /// `lpFreeBytesAvailableToCaller` is the one that matters: on a volume
        /// with quotas it is the space *this* process may use, which is the
        /// question a download is asking.
        fn GetDiskFreeSpaceExW(
            directory_name: *const u16,
            free_bytes_available_to_caller: *mut u64,
            total_number_of_bytes: *mut u64,
            total_number_of_free_bytes: *mut u64,
        ) -> i32;
    }

    pub(super) fn available_space(path: &Path) -> Option<u64> {
        // The directory itself may not exist yet, and the call needs somewhere
        // on the volume. The nearest existing ancestor is the same volume.
        let mut probe = path;
        let wide = loop {
            let mut wide: Vec<u16> = probe.as_os_str().encode_wide().collect();
            if probe.exists() {
                wide.push(0);
                break wide;
            }
            probe = probe.parent()?;
        };

        let mut available: u64 = 0;
        let mut total: u64 = 0;
        let mut free: u64 = 0;
        // SAFETY: `wide` is a NUL-terminated UTF-16 path, and the three out
        // parameters point at initialised `u64`s that outlive the call. The
        // function writes at most four `u64`s through them.
        let ok = unsafe {
            GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, &mut free)
        };
        (ok != 0).then_some(available)
    }
}

#[cfg(not(windows))]
mod platform {
    use std::path::Path;

    pub(super) fn available_space(_path: &Path) -> Option<u64> {
        // No platform call is wired up here; the download path treats this the
        // same as a volume that will not report a figure.
        None
    }
}

/// Room a download should be allowed to occupy, given what it is expected to
/// need.
///
/// A download can briefly need more than the final file: a video whose sound
/// arrives separately is written as two tracks and then copied into one
/// combined file, so the peak is roughly double the media plus the output.
/// Multiply rather than add, so a small download is not refused by a fixed
/// overhead it will never use.
///
/// Deliberately generous. Refusing a download that would have fitted is worse
/// than starting one that turns out not to — the first is a failure the user
/// cannot work around, the second is a failure the transfer already handles.
pub fn required_headroom(expected_bytes: u64) -> u64 {
    expected_bytes.saturating_mul(3)
}

/// A short human-readable size, for messages a user reads.
pub fn describe_bytes(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("GB", 1024 * 1024 * 1024),
        ("MB", 1024 * 1024),
        ("KB", 1024),
        ("bytes", 1),
    ];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            if scale == 1 {
                return format!("{bytes} bytes");
            }
            return format!("{:.1} {unit}", bytes as f64 / scale as f64);
        }
    }
    "0 bytes".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_real_directory_reports_something_plausible() {
        let temp = tempfile::tempdir().unwrap();
        match available_space(temp.path()) {
            Some(bytes) => assert!(bytes > 0, "a writable volume has some room"),
            // Not a failure: the platform may not implement the call. What must
            // never happen is a wrong number, which is why `None` exists.
            None => eprintln!("NOTE: this platform did not report free space"),
        }
    }

    #[test]
    fn a_directory_that_does_not_exist_yet_is_answered_by_its_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("not").join("created").join("yet");
        assert_eq!(
            available_space(&nested),
            available_space(temp.path()),
            "the volume is the same one either way"
        );
    }

    #[test]
    fn a_path_with_no_existing_ancestor_reports_unknown() {
        // Nothing to ask, so nothing is claimed.
        assert_eq!(available_space(Path::new("")), None);
    }

    #[test]
    fn headroom_is_a_multiple_and_never_wraps() {
        assert_eq!(required_headroom(0), 0);
        assert_eq!(required_headroom(100), 300);
        assert_eq!(required_headroom(u64::MAX), u64::MAX, "saturating");
    }

    #[test]
    fn sizes_are_described_in_units_a_person_reads() {
        assert_eq!(describe_bytes(0), "0 bytes");
        assert_eq!(describe_bytes(512), "512 bytes");
        assert_eq!(describe_bytes(2048), "2.0 KB");
        assert_eq!(describe_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(describe_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }
}
