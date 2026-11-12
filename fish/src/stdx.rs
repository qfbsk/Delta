/// This module contains tests that assert the functionality and behavior of the rust standard
/// library, to ensure we can safely use its abstractions to perform low-level operations.
#[cfg(test)]
mod tests {
    use std::fs::File;

    const TEST_FILE: &str = "test_file_for_fd_cloexec";

    #[cfg(unix)]
    #[test]
    fn test_fd_cloexec() {
        use nix::fcntl::{FcntlArg, fcntl};

        // Just open a file. Any file.
        let file = File::create(TEST_FILE).unwrap();
        assert_eq!(
            fcntl(&file, FcntlArg::F_GETFD).unwrap() & libc::FD_CLOEXEC,
            libc::FD_CLOEXEC
        );
        let _ = std::fs::remove_file(TEST_FILE);
    }

    #[cfg(windows)]
    #[test]
    fn test_handle_not_inheritable() {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Foundation::{
            GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT,
        };

        // Just open a file. Any file.
        let file = File::create(TEST_FILE).unwrap();
        let mut flags = 0u32;
        let ok = unsafe { GetHandleInformation(file.as_raw_handle() as HANDLE, &mut flags) };
        assert_ne!(ok, 0);
        assert_eq!(
            flags & HANDLE_FLAG_INHERIT,
            0,
            "std opened an inheritable handle"
        );
        let _ = std::fs::remove_file(TEST_FILE);
    }
}
