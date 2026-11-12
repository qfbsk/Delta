//! Minimal platform layer for file descriptors and the standard streams.
//!
//! On Unix this just re-exports the standard library's fd types.
//!
//! On Windows there are no file descriptors at the kernel level, but the C runtime offers
//! descriptor numbers (`_open`, `_pipe`, ...) that wrap Win32 handles. fish's whole I/O model is
//! built around small integer descriptors (0 = stdin, 1 = stdout, 2 = stderr, -1 = invalid), so we
//! keep that model and implement the few operations fish needs directly on top of the CRT. The
//! underlying kernel object is reached with `_get_osfhandle` when Win32 APIs (readiness, console
//! modes, process creation) are required.

#[cfg(unix)]
pub use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};

#[cfg(unix)]
pub use std::fs::File;

#[cfg(windows)]
pub type RawFd = i32;

pub const STDIN_FILENO: RawFd = 0;
pub const STDOUT_FILENO: RawFd = 1;
pub const STDERR_FILENO: RawFd = 2;

// Signal numbers, in POSIX numbering. Unix takes them from libc; Windows has no POSIX signals,
// but fish's signal table, `kill` builtin and scripts speak these numbers, and the console
// control handler maps its events onto them.
#[cfg(unix)]
pub use libc::{
    SIGABRT, SIGALRM, SIGBUS, SIGCHLD, SIGCONT, SIGFPE, SIGHUP, SIGILL, SIGINT, SIGIOT, SIGIO,
    SIGKILL, SIGPIPE, SIGPROF, SIGQUIT, SIGSEGV, SIGSTOP, SIGSYS, SIGTERM, SIGTRAP, SIGTSTP,
    SIGTTIN, SIGTTOU, SIGURG, SIGUSR1, SIGUSR2, SIGVTALRM, SIGWINCH, SIGXCPU, SIGXFSZ,
};

#[cfg(windows)]
mod signal_numbers {
    pub const SIGHUP: i32 = 1;
    pub const SIGINT: i32 = 2;
    pub const SIGQUIT: i32 = 3;
    pub const SIGILL: i32 = 4;
    pub const SIGTRAP: i32 = 5;
    pub const SIGABRT: i32 = 6;
    pub const SIGIOT: i32 = 6;
    pub const SIGBUS: i32 = 7;
    pub const SIGFPE: i32 = 8;
    pub const SIGKILL: i32 = 9;
    pub const SIGUSR1: i32 = 10;
    pub const SIGSEGV: i32 = 11;
    pub const SIGUSR2: i32 = 12;
    pub const SIGPIPE: i32 = 13;
    pub const SIGALRM: i32 = 14;
    pub const SIGTERM: i32 = 15;
    pub const SIGCHLD: i32 = 17;
    pub const SIGCONT: i32 = 18;
    pub const SIGSTOP: i32 = 19;
    pub const SIGTSTP: i32 = 20;
    pub const SIGTTIN: i32 = 21;
    pub const SIGTTOU: i32 = 22;
    pub const SIGURG: i32 = 23;
    pub const SIGXCPU: i32 = 24;
    pub const SIGXFSZ: i32 = 25;
    pub const SIGVTALRM: i32 = 26;
    pub const SIGPROF: i32 = 27;
    pub const SIGWINCH: i32 = 28;
    pub const SIGIO: i32 = 29;
    pub const SIGSYS: i32 = 31;
}

#[cfg(windows)]
pub use signal_numbers::*;

#[cfg(unix)]
pub use libc::off_t;
/// Windows offsets are always 64-bit in the emulated POSIX layer.
#[cfg(windows)]
pub type off_t = i64;

#[cfg(not(windows))]
pub use unix_impl::*;

#[cfg(windows)]
pub use windows_impl::*;

#[cfg(not(windows))]
mod unix_impl {
    use std::io;

    use super::*;

    pub fn is_tty(fd: RawFd) -> bool {
        unsafe { libc::isatty(fd) != 0 }
    }

    pub fn read_fd(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let ret = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if ret >= 0 {
                return Ok(ret as usize);
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }

    pub fn write_fd(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
        loop {
            let ret = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
            if ret >= 0 {
                return Ok(ret as usize);
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }

    pub fn close_fd(fd: RawFd) {
        unsafe {
            libc::close(fd);
        }
    }

    pub fn duplicate_fd(fd: RawFd) -> io::Result<RawFd> {
        let ret = unsafe { libc::dup(fd) };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(ret)
        }
    }

    pub fn duplicate_fd_to(fd: RawFd, target: RawFd) -> io::Result<RawFd> {
        let ret = unsafe { libc::dup2(fd, target) };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(ret)
        }
    }

    pub fn make_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
        let mut fds = [0 as RawFd; 2];
        let ret = unsafe { libc::pipe(fds.as_mut_ptr()) };
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }
        unsafe {
            Ok((
                OwnedFd::from_raw_fd(fds[0]),
                OwnedFd::from_raw_fd(fds[1]),
            ))
        }
    }
}

#[cfg(windows)]
mod windows_impl {
    use std::{
        collections::HashMap,
        io::{self, Read, Seek, Write},
        marker::PhantomData,
        os::windows::io::{IntoRawHandle, RawHandle},
        sync::Mutex,
    };

    use super::{RawFd, STDIN_FILENO};
    use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType};
    use windows_sys::Win32::System::Console::{GetConsoleCP, SetConsoleCP};
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;

    const O_BINARY: i32 = 0x8000;

    // FD_CLOEXEC has no Windows equivalent: child processes inherit handles explicitly, not
    // through the CRT descriptor table. We still record the flag because fish uses it to reason
    // about which descriptors should not leak into commands.
    const FD_CLOEXEC: i32 = 1;
    // The POSIX value; fish compares F_GETFL results against libc::O_NONBLOCK.
    const O_NONBLOCK: i32 = 0o4000;

    static FD_FLAGS: Mutex<Option<HashMap<RawFd, i32>>> = Mutex::new(None);

    /// The UCRT reports failures through errno and leaves GetLastError stale, so build the error
    /// from errno. std's Windows errors carry Win32 codes, so translate the common POSIX codes.
    fn crt_error() -> io::Error {
        unsafe extern "C" {
            fn _errno() -> *mut i32;
        }
        let errno = unsafe { *_errno() };
        let win32 = match errno {
            4 => return io::Error::from(io::ErrorKind::Interrupted),
            11 => return io::Error::from(io::ErrorKind::WouldBlock),
            2 => 2,   // ENOENT
            9 => 6,   // EBADF
            13 => 5,  // EACCES
            17 => 80, // EEXIST
            20 => 267,
            21 => 5,
            22 => 87,  // EINVAL
            28 => 112, // ENOSPC
            29 => 132, // ESPIPE
            32 => 109, // EPIPE
            _ => errno,
        };
        io::Error::from_raw_os_error(win32)
    }

    fn with_flags<R>(f: impl FnOnce(&mut HashMap<RawFd, i32>) -> R) -> R {
        let mut guard = FD_FLAGS.lock().unwrap();
        f(guard.get_or_insert_with(HashMap::new))
    }

    pub fn fd_flags_get(fd: RawFd) -> i32 {
        with_flags(|flags| flags.get(&fd).copied().unwrap_or(0))
    }

    pub fn fd_flags_set(fd: RawFd, value: i32) {
        with_flags(|flags| {
            if value == 0 {
                flags.remove(&fd);
            } else {
                flags.insert(fd, value);
            }
        });
    }

    pub const FD_CLOEXEC_FLAG: i32 = FD_CLOEXEC;
    pub const O_NONBLOCK_FLAG: i32 = O_NONBLOCK;

    unsafe extern "C" {
        fn _setmode(fd: i32, mode: i32) -> i32;
    }

    /// Set a stream to binary mode so the CRT does not translate CR/LF bytes.
    pub fn set_binary(fd: RawFd) {
        if fd >= 0 {
            unsafe {
                _setmode(fd, O_BINARY);
            }
        }
    }

    /// Anything that refers to a Win32 kernel object.
    ///
    /// Win32 APIs take handles, not descriptors; this bridges descriptors, `File`s and the fd
    /// wrapper types so callers can hand any of them to handle-based APIs.
    pub trait AsHandle {
        fn as_handle(&self) -> RawHandle;
    }

    impl AsHandle for File {
        fn as_handle(&self) -> RawHandle {
            handle_from_fd(self.fd)
        }
    }

    impl<T: AsHandle + ?Sized> AsHandle for &T {
        fn as_handle(&self) -> RawHandle {
            (**self).as_handle()
        }
    }

    impl AsHandle for RawFd {
        fn as_handle(&self) -> RawHandle {
            handle_from_fd(*self)
        }
    }

    impl AsHandle for BorrowedFd<'_> {
        fn as_handle(&self) -> RawHandle {
            handle_from_fd(self.fd)
        }
    }

    impl AsHandle for OwnedFd {
        fn as_handle(&self) -> RawHandle {
            handle_from_fd(self.fd)
        }
    }

    /// The Win32 handle underlying a CRT descriptor, or a null pointer for an invalid fd.
    pub fn handle_from_fd(fd: RawFd) -> RawHandle {
        if fd < 0 {
            return std::ptr::null_mut();
        }
        let raw = unsafe { libc::get_osfhandle(fd) };
        if raw == -1 {
            std::ptr::null_mut()
        } else {
            raw as RawHandle
        }
    }

    pub fn is_tty(fd: RawFd) -> bool {
        // Console handles are recognized by GetFileType == FILE_TYPE_CHAR, which is also true for
        // the NUL device; isatty on top of that excludes NUL and character devices.
        // The UCRT returns an arbitrary nonzero value (0x40 for a console), not 1.
        fd >= 0 && unsafe { libc::isatty(fd) != 0 }
    }

    /// Return the (volume serial number, file index) pair identifying the file at `path`.
    /// Follows reparse points. Stands in for the POSIX device/inode pair, which Windows only
    /// exposes through a handle.
    pub fn file_identity(path: &std::ffi::OsStr) -> io::Result<(u64, u64)> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };

        let wide: Vec<u16> = path.encode_wide().chain(std::iter::once(0)).collect();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let result = file_identity_from_handle(handle);
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(handle);
        }
        result
    }

    /// Return the (volume serial number, file index) pair identifying an open handle.
    pub fn file_identity_from_handle(handle: RawHandle) -> io::Result<(u64, u64)> {
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        if handle.is_null() {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        Ok((u64::from(info.dwVolumeSerialNumber), index))
    }

    /// Return the identity of the file an open `File` refers to.
    pub fn file_identity_of(file: &File) -> io::Result<(u64, u64)> {
        file_identity_from_handle(handle_from_fd(file.as_raw_fd()))
    }

    /// Bytes conhost still owes for the character whose leading byte was already returned.
    ///
    /// A console read converts one character at a time. When the read buffer is smaller than the
    /// character's encoding, conhost hands out the leading byte and keeps the rest pending until
    /// the next read - but peeking the input queue in between (which the readiness probes do)
    /// discards that remainder. So the tail is read immediately, before any probe can run, and
    /// served from here.
    static CONSOLE_CHAR_TAIL: Mutex<Vec<u8>> = Mutex::new(Vec::new());

    /// The console input code page inherited at startup, restored when a child owns the console.
    static CONSOLE_CODEPAGE: Mutex<Option<u32>> = Mutex::new(None);

    /// Make console input UTF-8, the encoding of fish's input stream.
    ///
    /// The inherited code page is the system ANSI one, so conhost would otherwise convert console
    /// characters to something fish cannot decode (GBK on Chinese Windows). Called whenever fish
    /// takes the terminal back, since a child may have run chcp.
    pub fn console_input_utf8() {
        if !is_tty(STDIN_FILENO) {
            return;
        }
        let mut saved = CONSOLE_CODEPAGE.lock().unwrap_or_else(|e| e.into_inner());
        if saved.is_none() {
            *saved = Some(unsafe { GetConsoleCP() });
        }
        unsafe { SetConsoleCP(65001) };
    }

    /// Give a child the code page the console had before fish changed it.
    pub fn console_input_restore() {
        if let Some(cp) = *CONSOLE_CODEPAGE.lock().unwrap_or_else(|e| e.into_inner()) {
            unsafe { SetConsoleCP(cp) };
        }
    }

    /// Whether a character tail read ahead by [`console_read_char_tail`] is still waiting.
    pub fn console_has_pending_input() -> bool {
        !CONSOLE_CHAR_TAIL
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// Queue a control byte that the console delivered as a key record without a character.
    ///
    /// ConPTY represents a terminal control byte it does not turn into text (^U and ^W on this
    /// conhost) as a key-down record whose virtual-key code is the byte itself and whose character
    /// is zero. The CRT's text read skips such records, so the input layer hands the byte over
    /// here instead.
    pub fn console_queue_input_byte(byte: u8) {
        CONSOLE_CHAR_TAIL
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(byte);
    }

    /// Take one byte of the character tail read ahead by [`console_read_char_tail`].
    pub fn console_take_pending_input() -> Option<u8> {
        let mut tail = CONSOLE_CHAR_TAIL.lock().unwrap_or_else(|e| e.into_inner());
        if tail.is_empty() {
            None
        } else {
            Some(tail.remove(0))
        }
    }

    /// After reading `lead`, read the remaining bytes of its character while no input-queue probe
    /// can run. Does nothing unless the console input code page is UTF-8 and `lead` starts a
    /// multi-byte character.
    pub fn console_read_char_tail(fd: RawFd, lead: u8) {
        if unsafe { GetConsoleCP() } != 65001 {
            return;
        }
        let len = match lead {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return,
        };
        let mut tail = [0u8; 3];
        let mut got = 0;
        while got < len - 1 {
            match read_fd(fd, &mut tail[got..got + 1]) {
                Ok(1) => got += 1,
                _ => break,
            }
        }
        if got > 0 {
            CONSOLE_CHAR_TAIL
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend_from_slice(&tail[..got]);
        }
    }

    pub fn read_fd(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
        if fd < 0 {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let mut len: u32 = buf.len().min(u32::MAX as usize) as u32;
        if len == 0 {
            return Ok(0);
        }
        // The CRT has no non-blocking mode for pipes, so honor fish's O_NONBLOCK bookkeeping
        // here: report WouldBlock when the pipe is empty instead of entering a blocking read.
        // This is what lets the fd monitor drain a pipe until EAGAIN.
        if fd_flags_get(fd) & O_NONBLOCK != 0 {
            let handle = handle_from_fd(fd);
            if !handle.is_null() && unsafe { GetFileType(handle) } == FILE_TYPE_PIPE {
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
                if ok != 0 {
                    if available == 0 {
                        return Err(io::Error::from(io::ErrorKind::WouldBlock));
                    }
                    len = len.min(available);
                }
                // If PeekNamedPipe failed the pipe is broken or the handle invalid; fall through
                // so the actual read reports EOF or the error.
            }
        }
        let ret = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), len) };
        if ret < 0 {
            Err(crt_error())
        } else {
            Ok(ret as usize)
        }
    }

    pub fn write_fd(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
        if fd < 0 {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let len: u32 = buf.len().min(u32::MAX as usize) as u32;
        let ret = unsafe { libc::write(fd, buf.as_ptr().cast(), len) };
        if ret < 0 {
            // The UCRT reports a pipe whose reader went away as EINVAL, but POSIX code (and
            // fish's output writer) expects EPIPE here and treats it as a normal shutdown race.
            let last = io::Error::last_os_error();
            if matches!(last.raw_os_error(), Some(109 | 232 | 233)) {
                return Err(io::Error::from(io::ErrorKind::BrokenPipe));
            }
            Err(crt_error())
        } else {
            Ok(ret as usize)
        }
    }

    pub fn close_fd(fd: RawFd) {
        if fd >= 0 {
            unsafe {
                libc::close(fd);
            }
            fd_flags_set(fd, 0);
        }
    }

    pub fn duplicate_fd(fd: RawFd) -> io::Result<RawFd> {
        let ret = unsafe { libc::dup(fd) };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            set_binary(ret);
            Ok(ret)
        }
    }

    pub fn duplicate_fd_to(fd: RawFd, target: RawFd) -> io::Result<RawFd> {
        let ret = unsafe { libc::dup2(fd, target) };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            set_binary(ret);
            Ok(ret)
        }
    }

    /// Create an anonymous pipe, returning (read, write) descriptors in binary mode.
    pub fn make_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
        let mut fds = [0 as RawFd; 2];
        let ret = unsafe { libc::pipe(fds.as_mut_ptr(), 0, O_BINARY) };
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }
        set_binary(fds[0]);
        set_binary(fds[1]);
        unsafe {
            Ok((
                OwnedFd::from_raw_fd(fds[0]),
                OwnedFd::from_raw_fd(fds[1]),
            ))
        }
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct BorrowedFd<'a> {
        fd: RawFd,
        _phantom: PhantomData<&'a RawFd>,
    }

    impl<'a> BorrowedFd<'a> {
        pub const unsafe fn borrow_raw(fd: RawFd) -> Self {
            Self {
                fd,
                _phantom: PhantomData,
            }
        }

        pub fn as_raw_fd(self) -> RawFd {
            self.fd
        }

        pub fn is_valid(self) -> bool {
            self.fd >= 0
        }
    }

    impl AsRawFd for BorrowedFd<'_> {
        fn as_raw_fd(&self) -> RawFd {
            self.fd
        }
    }

    impl<'a> AsFd for BorrowedFd<'a> {
        fn as_fd(&self) -> BorrowedFd<'_> {
            *self
        }
    }

    #[derive(Debug)]
    pub struct OwnedFd {
        fd: RawFd,
    }

    impl OwnedFd {
        pub fn as_fd(&self) -> BorrowedFd<'_> {
            unsafe { BorrowedFd::borrow_raw(self.fd) }
        }

        pub fn try_clone(&self) -> io::Result<Self> {
            Ok(unsafe { Self::from_raw_fd(duplicate_fd(self.fd)?) })
        }

        pub fn into_raw(self) -> RawFd {
            let fd = self.fd;
            std::mem::forget(self);
            fd
        }
    }

    impl AsRawFd for OwnedFd {
        fn as_raw_fd(&self) -> RawFd {
            self.fd
        }
    }

    impl AsFd for OwnedFd {
        fn as_fd(&self) -> BorrowedFd<'_> {
            unsafe { BorrowedFd::borrow_raw(self.fd) }
        }
    }

    impl FromRawFd for OwnedFd {
        unsafe fn from_raw_fd(fd: RawFd) -> Self {
            Self { fd }
        }
    }

    impl IntoRawFd for OwnedFd {
        fn into_raw_fd(self) -> RawFd {
            self.into_raw()
        }
    }

    impl Drop for OwnedFd {
        fn drop(&mut self) {
            close_fd(self.fd);
        }
    }

    /// A file handle exposed the way POSIX code expects: as a descriptor that owns the underlying
    /// Win32 handle. The descriptor is the only owner, so `as_raw_fd()` can lend it out without
    /// duplicating. A duplicate would outlive the `File` and keep byte-range locks alive, which
    /// made the history file deadlock on itself.
    #[derive(Debug)]
    pub struct File {
        fd: RawFd,
    }

    impl File {
        pub fn try_clone(&self) -> io::Result<Self> {
            Ok(Self {
                fd: duplicate_fd(self.fd)?,
            })
        }

        /// Metadata through a borrowed wrapper around the same handle, so the descriptor stays
        /// owned by `self`.
        pub fn metadata(&self) -> io::Result<std::fs::Metadata> {
            use std::mem::ManuallyDrop;
            use std::os::windows::io::FromRawHandle;
            let handle = handle_from_fd(self.fd);
            if handle.is_null() {
                return Err(io::Error::from(io::ErrorKind::InvalidInput));
            }
            let file = ManuallyDrop::new(unsafe { std::fs::File::from_raw_handle(handle) });
            file.metadata()
        }
    }

    impl Drop for File {
        fn drop(&mut self) {
            close_fd(self.fd);
        }
    }

    impl Read for File {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            read_fd(self.fd, buf)
        }
    }

    impl Read for &File {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            read_fd(self.fd, buf)
        }
    }

    impl Write for File {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            write_fd(self.fd, buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Write for &File {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            write_fd(self.fd, buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn seek_fd(fd: RawFd, pos: io::SeekFrom) -> io::Result<u64> {
        unsafe extern "C" {
            fn _lseeki64(fd: i32, offset: i64, origin: i32) -> i64;
        }
        let (offset, origin) = match pos {
            io::SeekFrom::Start(n) => (n as i64, libc::SEEK_SET),
            io::SeekFrom::End(n) => (n, libc::SEEK_END),
            io::SeekFrom::Current(n) => (n, libc::SEEK_CUR),
        };
        let ret = unsafe { _lseeki64(fd, offset, origin) };
        if ret < 0 {
            Err(crt_error())
        } else {
            Ok(ret as u64)
        }
    }

    impl Seek for File {
        fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
            seek_fd(self.fd, pos)
        }
    }

    impl Seek for &File {
        fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
            seek_fd(self.fd, pos)
        }
    }

    impl AsRawFd for File {
        fn as_raw_fd(&self) -> RawFd {
            self.fd
        }
    }

    impl AsFd for File {
        fn as_fd(&self) -> BorrowedFd<'_> {
            unsafe { BorrowedFd::borrow_raw(self.fd) }
        }
    }

    impl FromRawFd for File {
        unsafe fn from_raw_fd(fd: RawFd) -> Self {
            Self { fd }
        }
    }

    impl IntoRawFd for File {
        fn into_raw_fd(self) -> RawFd {
            let fd = self.fd;
            std::mem::forget(self);
            fd
        }
    }

    impl From<OwnedFd> for File {
        fn from(owned: OwnedFd) -> Self {
            unsafe { Self::from_raw_fd(owned.into_raw()) }
        }
    }

    impl From<std::fs::File> for File {
        fn from(file: std::fs::File) -> Self {
            Self::from(OwnedFd::from(file))
        }
    }

    impl From<File> for OwnedFd {
        fn from(file: File) -> Self {
            unsafe { Self::from_raw_fd(file.into_raw_fd()) }
        }
    }

    /// A `std::fs::File` opened elsewhere (e.g. `std::fs::OpenOptions`) transfers its handle into
    /// the descriptor world.
    impl From<std::fs::File> for OwnedFd {
        fn from(file: std::fs::File) -> Self {
            let handle = file.into_raw_handle();
            let fd = unsafe { libc::open_osfhandle(handle as isize, O_BINARY) };
            if fd < 0 {
                unsafe {
                    windows_sys::Win32::Foundation::CloseHandle(handle.cast());
                }
                panic!("failed to convert file handle to a descriptor");
            }
            unsafe { Self::from_raw_fd(fd) }
        }
    }

    impl IntoRawFd for std::fs::File {
        fn into_raw_fd(self) -> RawFd {
            OwnedFd::from(self).into_raw()
        }
    }

    /// Wrap a descriptor in a `File` without taking ownership of the descriptor. The caller must
    /// keep the file inside a `ManuallyDrop` (or otherwise prevent its destructor from closing the
    /// descriptor).
    pub unsafe fn file_from_fd(fd: RawFd) -> File {
        unsafe { File::from_raw_fd(fd) }
    }

    pub trait AsRawFd {
        fn as_raw_fd(&self) -> RawFd;
    }

    impl AsRawFd for i32 {
        fn as_raw_fd(&self) -> RawFd {
            *self
        }
    }

    impl<T: AsRawFd + ?Sized> AsRawFd for &T {
        fn as_raw_fd(&self) -> RawFd {
            (**self).as_raw_fd()
        }
    }

    pub trait AsFd {
        fn as_fd(&self) -> BorrowedFd<'_>;
    }

    impl AsFd for i32 {
        fn as_fd(&self) -> BorrowedFd<'_> {
            unsafe { BorrowedFd::borrow_raw(*self) }
        }
    }

    impl<T: AsFd + ?Sized> AsFd for &T {
        fn as_fd(&self) -> BorrowedFd<'_> {
            (**self).as_fd()
        }
    }

    pub trait IntoRawFd {
        fn into_raw_fd(self) -> RawFd;
    }

    impl IntoRawFd for i32 {
        fn into_raw_fd(self) -> RawFd {
            self
        }
    }

    pub trait FromRawFd {
        unsafe fn from_raw_fd(fd: RawFd) -> Self;
    }

    impl FromRawFd for i32 {
        unsafe fn from_raw_fd(fd: RawFd) -> Self {
            fd
        }
    }
}
