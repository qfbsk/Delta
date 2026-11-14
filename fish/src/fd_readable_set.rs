use std::time::Duration;

/// How long to sleep between readiness probes on handles that cannot be waited on.
#[cfg(windows)]
const PROBE_INTERVAL: Duration = Duration::from_millis(2);

/// Returned by `check_readable` when a cancellation signal arrived while waiting.
///
/// Unix select() reports this as an EINTR error. Windows has no signal-interrupted wait, so the
/// wait loop notices the pending cancellation and reports it with this value instead.
pub const SELECT_INTERRUPTED: i32 = -2;

#[derive(Clone, Copy)]
pub enum Timeout {
    Duration(Duration),
    Forever,
}

impl Timeout {
    pub const ZERO: Timeout = Timeout::Duration(Duration::ZERO);

    /// Convert from usecs to poll-friendly msecs.
    #[allow(unused)]
    fn as_poll_msecs(&self) -> std::ffi::c_int {
        match self {
            // Negative values mean wait forever in poll-speak.
            Timeout::Forever => -1 as std::ffi::c_int,
            Timeout::Duration(duration) => {
                assert!(
                    duration.as_millis() < std::ffi::c_int::MAX as _,
                    "Timeout too long but not forever!"
                );
                duration.as_millis() as std::ffi::c_int
            }
        }
    }
}

/// Returns `true` if the fd is or becomes readable within the given timeout.
/// This returns `false` if the waiting is interrupted by a signal.
pub fn is_fd_readable(fd: i32, timeout: Timeout) -> bool {
    FdReadableSet::is_fd_readable(fd, timeout)
}

/// Returns whether an fd is readable.
pub fn poll_fd_readable(fd: i32) -> bool {
    FdReadableSet::poll_fd_readable(fd)
}

#[cfg(windows)]
mod imp {
    use super::{PROBE_INTERVAL, Timeout};
    use fish_common::os::{RawFd, handle_from_fd};
    use std::time::Instant;
    use windows_sys::Win32::Foundation::{
        ERROR_BROKEN_PIPE, ERROR_INVALID_HANDLE, ERROR_PIPE_NOT_CONNECTED,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_TYPE_CHAR, FILE_TYPE_DISK, FILE_TYPE_PIPE, GetFileType,
    };
    use windows_sys::Win32::System::Console::GetNumberOfConsoleInputEvents;
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;

    /// Set when a probe saw a console window-size change, so the wait loop can report it as an
    /// interrupted select() once the current poll finishes.
    static WINCH_PENDING: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    // One entry per fd. `revents` records the outcome of the last probe, mirroring poll()'s
    // destructive update of its fd array.
    pub struct FdReadableSet {
        fds: Vec<RawFd>,
        revents: Vec<bool>,
    }

    /// Windows has no poll(); readiness is observed per handle instead.
    ///
    /// - Pipes report the number of buffered bytes through PeekNamedPipe.
    /// - Console input handles report pending input records.
    /// - Disk files are always readable.
    ///
    /// None of these can be combined into a single wait, so a set is probed in a loop.
    fn probe(fd: RawFd) -> bool {
        if fd < 0 {
            return false;
        }
        let handle = handle_from_fd(fd);
        if handle.is_null() {
            return false;
        }
        let file_type = unsafe { GetFileType(handle) };
        match file_type {
            FILE_TYPE_PIPE => {
                let mut available: u32 = 0;
                let ok = unsafe {
                    PeekNamedPipe(
                        handle,
                        std::ptr::null_mut(),
                        0,
                        std::ptr::null_mut(),
                        &mut available,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 {
                    let err = std::io::Error::last_os_error().raw_os_error();
                    // A closed write end leaves nothing to read but must wake the reader.
                    matches!(
                        err,
                        Some(code)
                            if code == ERROR_BROKEN_PIPE as i32
                                || code == ERROR_PIPE_NOT_CONNECTED as i32
                                || code == ERROR_INVALID_HANDLE as i32
                    )
                } else {
                    available > 0
                }
            }
            FILE_TYPE_CHAR => {
                // The rest of a multi-byte character may be waiting in fish, not in the console.
                if fish_common::os::console_has_pending_input() {
                    return true;
                }
                // GetNumberOfConsoleInputEvents also counts key releases, modifier presses and
                // window-size changes. fish reads input as a byte stream and ReadFile skips those
                // records, so counting them would make a blocked reader out of a key that
                // produces no input. Discard them instead, stopping at the first key that does
                // yield a character, and report a resize while doing so.
                use windows_sys::Win32::System::Console::{
                    ENABLE_PROCESSED_INPUT, GetConsoleMode, INPUT_RECORD, KEY_EVENT,
                    LEFT_CTRL_PRESSED, PeekConsoleInputW, RIGHT_CTRL_PRESSED, ReadConsoleInputW,
                    WINDOW_BUFFER_SIZE_EVENT,
                };
                let mut count: u32 = 0;
                if unsafe { GetNumberOfConsoleInputEvents(handle, &mut count) } == 0 || count == 0 {
                    return false;
                }
                let mut mode = 0u32;
                let processed = unsafe { GetConsoleMode(handle, &mut mode) } != 0
                    && mode & ENABLE_PROCESSED_INPUT != 0;
                let mut char_ready = false;
                let mut resized = false;
                for _ in 0..32 {
                    let mut record: [INPUT_RECORD; 1] = unsafe { std::mem::zeroed() };
                    let mut got = 0u32;
                    if unsafe { PeekConsoleInputW(handle, record.as_mut_ptr(), 1, &mut got) } == 0
                        || got != 1
                    {
                        break;
                    }
                    let evt = record[0].EventType;
                    if evt == WINDOW_BUFFER_SIZE_EVENT as u16 {
                        resized = true;
                    } else if evt == KEY_EVENT as u16 {
                        let key = unsafe { &record[0].Event.KeyEvent };
                        let key_down = key.bKeyDown != 0;
                        let ch = unsafe { key.uChar.UnicodeChar };
                        let scan = key.wVirtualScanCode;
                        let vk = key.wVirtualKeyCode;
                        let ctrl_down =
                            key.dwControlKeyState & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED) != 0;
                        // With processed input on, ^C becomes a console event instead of data.
                        if key_down && ch != 0 && !(processed && ch == 0x03) {
                            // In virtual-terminal input mode conhost delivers Ctrl+Z as a plain
                            // character record, and the CRT's read reports that record as zero
                            // bytes instead of 0x1a. readb() reads zero bytes as "terminal
                            // closed", so take this record ourselves and queue the byte.
                            if vk == 0 && scan == 0 && ch == 0x1a {
                                let mut consumed = 0u32;
                                if unsafe {
                                    ReadConsoleInputW(handle, record.as_mut_ptr(), 1, &mut consumed)
                                } != 0
                                    && consumed == 1
                                {
                                    fish_common::os::console_queue_input_byte(0x1a);
                                }
                            }
                            char_ready = true;
                            break;
                        }
                        // ConPTY turns a terminal control byte it does not render as text into a
                        // key record that keeps the byte in the virtual-key field and has no
                        // character. ReadFile drops those, so consume the record and queue the
                        // byte for readb() instead.
                        if key_down && ch == 0 && scan == 0 && ctrl_down && (1..=0x1a).contains(&vk)
                        {
                            let mut consumed = 0u32;
                            if unsafe {
                                ReadConsoleInputW(handle, record.as_mut_ptr(), 1, &mut consumed)
                            } != 0
                                && consumed == 1
                            {
                                fish_common::os::console_queue_input_byte(vk as u8);
                                char_ready = true;
                            }
                            break;
                        }
                    }
                    let mut consumed = 0u32;
                    if unsafe { ReadConsoleInputW(handle, record.as_mut_ptr(), 1, &mut consumed) }
                        == 0
                        || consumed != 1
                    {
                        break;
                    }
                }
                if resized {
                    crate::signal::signal_winch();
                    WINCH_PENDING.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                char_ready
            }
            FILE_TYPE_DISK => true,
            _ => false,
        }
    }

    impl FdReadableSet {
        /// Construct an empty set.
        pub fn new() -> FdReadableSet {
            FdReadableSet {
                fds: Vec::new(),
                revents: Vec::new(),
            }
        }

        /// Reset back to an empty set.
        pub fn clear(&mut self) {
            self.fds.clear();
            self.revents.clear();
        }

        /// Add an fd to the set. The fd is ignored if negative (for convenience). The fd is also
        /// ignored if it's already in the set.
        pub fn add(&mut self, fd: RawFd) {
            if fd < 0 {
                return;
            }
            let pos = match self.fds.binary_search(&fd) {
                Ok(_) => return,
                Err(pos) => pos,
            };
            self.fds.insert(pos, fd);
            self.revents.insert(pos, false);
        }

        /// Returns `true` if the given `fd` has input available to read or has been HUP'd.
        /// Returns `false` if `fd` is negative or was not found in the set.
        pub fn test(&self, fd: RawFd) -> bool {
            match self.fds.binary_search(&fd) {
                Ok(pos) => self.revents[pos],
                Err(_) => false,
            }
        }

        /// Poll the set, waiting up to `timeout`. Note this destructively modifies the set.
        /// Returns the number of ready fds.
        pub fn check_readable(&mut self, timeout: Timeout) -> i32 {
            if self.fds.is_empty() {
                return 0;
            }
            let deadline = match timeout {
                Timeout::Forever => None,
                Timeout::Duration(duration) => Some(Instant::now() + duration),
            };
            loop {
                if crate::signal::signal_check_cancel() != 0 {
                    return super::SELECT_INTERRUPTED;
                }
                let mut count = 0;
                for (fd, ready) in self.fds.iter().zip(self.revents.iter_mut()) {
                    *ready = probe(*fd);
                    if *ready {
                        count += 1;
                    }
                }
                if WINCH_PENDING.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    return super::SELECT_INTERRUPTED;
                }
                if count > 0 {
                    return count;
                }
                let sleep_for = match deadline {
                    None => PROBE_INTERVAL,
                    Some(deadline) => {
                        let now = Instant::now();
                        if now >= deadline {
                            return 0;
                        }
                        PROBE_INTERVAL.min(deadline - now)
                    }
                };
                std::thread::sleep(sleep_for);
            }
        }

        /// Check if a single fd is readable, with a given timeout.
        /// Returns `true` if readable, `false` otherwise.
        pub fn is_fd_readable(fd: RawFd, timeout: Timeout) -> bool {
            if fd < 0 {
                return false;
            }
            let mut s = Self::new();
            s.add(fd);
            s.check_readable(timeout) > 0 && s.test(fd)
        }

        /// Check if a single fd is readable, without blocking.
        /// Returns `true` if readable, `false` if not.
        pub fn poll_fd_readable(fd: RawFd) -> bool {
            Self::is_fd_readable(fd, Timeout::ZERO)
        }
    }
}

#[cfg(windows)]
pub use imp::FdReadableSet;

#[cfg(unix)]
mod posix_imp {
    use super::Timeout;
    use std::ffi::c_int;
    use std::os::unix::prelude::*;

    /// A modest wrapper around select() or poll().
    /// This allows accumulating a set of fds and then seeing if they are readable.
    /// This only handles readability.
    /// Apple's `man poll`: "The poll() system call currently does not support devices."
    #[cfg(apple)]
    pub struct FdReadableSet {
        // The underlying fdset and nfds value to pass to select().
        fdset_: libc::fd_set,
        nfds_: c_int,
    }

    #[cfg(apple)]
    impl FdReadableSet {
        /// Construct an empty set.
        pub fn new() -> FdReadableSet {
            let mut fdset_ = std::mem::MaybeUninit::uninit();
            let fdset_ = unsafe {
                libc::FD_ZERO(fdset_.as_mut_ptr());
                fdset_.assume_init()
            };
            FdReadableSet { fdset_, nfds_: 0 }
        }

        /// Reset back to an empty set.
        pub fn clear(&mut self) {
            self.nfds_ = 0;
            unsafe {
                libc::FD_ZERO(&mut self.fdset_);
            }
        }

        /// Add an fd to the set. The fd is ignored if negative (for convenience).
        pub fn add(&mut self, fd: RawFd) {
            if fd >= (libc::FD_SETSIZE as RawFd) {
                //flogf!(error, "fd %d too large for select()", fd);
                return;
            }
            if fd >= 0 {
                unsafe { libc::FD_SET(fd, &mut self.fdset_) };
                self.nfds_ = std::cmp::max(self.nfds_, fd + 1);
            }
        }

        /// Returns `true` if the given `fd` is marked as set, in our set. Returns `false` if `fd` is
        /// negative.
        pub fn test(&self, fd: RawFd) -> bool {
            fd >= 0 && unsafe { libc::FD_ISSET(fd, &self.fdset_) }
        }

        /// Call `select()`. Note this destructively modifies the set. Returns the result of
        /// `select()`.
        pub fn check_readable(&mut self, timeout: Timeout) -> c_int {
            let null = std::ptr::null_mut();
            let mut tvs;
            let timeout = match timeout {
                Timeout::Forever => std::ptr::null_mut(),
                Timeout::Duration(duration) => {
                    tvs = libc::timeval {
                        tv_sec: duration.as_secs() as libc::time_t,
                        tv_usec: duration.subsec_micros() as libc::suseconds_t,
                    };
                    &mut tvs
                }
            };
            unsafe { libc::select(self.nfds_, &mut self.fdset_, null, null, timeout) }
        }

        /// Check if a single fd is readable, with a given timeout.
        /// Returns `true` if readable, `false` otherwise.
        pub fn is_fd_readable(fd: RawFd, timeout: Timeout) -> bool {
            if fd < 0 {
                return false;
            }
            let mut s = Self::new();
            s.add(fd);
            let res = s.check_readable(timeout);
            res > 0 && s.test(fd)
        }

        /// Check if a single fd is readable, without blocking.
        /// Returns `true` if readable, `false` if not.
        pub fn poll_fd_readable(fd: RawFd) -> bool {
            Self::is_fd_readable(fd, Timeout::ZERO)
        }
    }

    #[cfg(not(apple))]
    pub struct FdReadableSet {
        pollfds_: Vec<libc::pollfd>,
    }

    #[cfg(not(apple))]
    impl FdReadableSet {
        /// Construct an empty set.
        pub fn new() -> FdReadableSet {
            FdReadableSet {
                pollfds_: Vec::new(),
            }
        }

        /// Reset back to an empty set.
        pub fn clear(&mut self) {
            self.pollfds_.clear();
        }

        #[inline]
        fn pollfd_get_fd(pollfd: &libc::pollfd) -> RawFd {
            pollfd.fd
        }

        /// Add an fd to the set. The fd is ignored if negative (for convenience). The fd is also
        /// ignored if it's already in the set.
        pub fn add(&mut self, fd: RawFd) {
            if fd < 0 {
                return;
            }
            let pos = match self.pollfds_.binary_search_by_key(&fd, Self::pollfd_get_fd) {
                Ok(_) => return,
                Err(pos) => pos,
            };

            self.pollfds_.insert(
                pos,
                libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
            );
        }

        /// Returns `true` if the given `fd` has input available to read or has been HUP'd.
        /// Returns `false` if `fd` is negative or was not found in the set.
        pub fn test(&self, fd: RawFd) -> bool {
            // If a pipe is widowed with no data, Linux sets POLLHUP but not POLLIN, so test for both.
            if let Ok(pos) = self.pollfds_.binary_search_by_key(&fd, Self::pollfd_get_fd) {
                let pollfd = &self.pollfds_[pos];
                debug_assert_eq!(pollfd.fd, fd);
                return pollfd.revents & (libc::POLLIN | libc::POLLHUP) != 0;
            }
            false
        }

        fn do_poll(fds: &mut [libc::pollfd], timeout: Timeout) -> c_int {
            let count = fds.len();
            assert!(count <= libc::nfds_t::MAX as usize, "count too big");
            unsafe {
                libc::poll(
                    fds.as_mut_ptr(),
                    count as libc::nfds_t,
                    timeout.as_poll_msecs(),
                )
            }
        }

        /// Call poll(). Note this destructively modifies the set. Return the result of poll().
        pub fn check_readable(&mut self, timeout: Timeout) -> c_int {
            if self.pollfds_.is_empty() {
                return 0;
            }
            Self::do_poll(&mut self.pollfds_, timeout)
        }

        /// Check if a single fd is readable, with a given timeout.
        /// Return true if `fd` is our set and is readable, `false` otherwise.
        pub fn is_fd_readable(fd: RawFd, timeout: Timeout) -> bool {
            if fd < 0 {
                return false;
            }
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ret = Self::do_poll(std::slice::from_mut(&mut pfd), timeout);
            ret > 0 && (pfd.revents & libc::POLLIN) != 0
        }

        /// Check if a single fd is readable, without blocking.
        /// Return true if readable, false if not.
        pub fn poll_fd_readable(fd: RawFd) -> bool {
            Self::is_fd_readable(fd, Timeout::ZERO)
        }
    }
}

#[cfg(unix)]
pub use posix_imp::FdReadableSet;
