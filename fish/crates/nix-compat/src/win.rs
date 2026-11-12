//! Windows implementation of the POSIX surface used by fish.
//!
//! fish is written against a small, well-defined slice of POSIX: descriptor-numbered I/O, a
//! terminal with termios settings, process creation and reaping, signals for job control, a
//! readable-set poll, and a handful of filesystem attributes. Windows has equivalents for all of
//! it, but under different shapes. This module presents the POSIX shapes on top of the Win32 API
//! and the C runtime so that the rest of fish can stay in its original form.

#![allow(non_camel_case_types)]

pub type Result<T, E = Errno> = std::result::Result<T, E>;

pub use errno::{Errno, Errno as Error};

pub mod errno {
    /// The subset of POSIX error numbers that fish inspects. Values follow the Unix numbering so
    /// that error codes printed by fish look familiar.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
    #[repr(i32)]
    pub enum Errno {
        /// No error, or an error code we do not classify.
        UnknownErrno = 0,
        EPERM = 1,
        ENOENT = 2,
        ESRCH = 3,
        EINTR = 4,
        EIO = 5,
        ENXIO = 6,
        E2BIG = 7,
        ENOEXEC = 8,
        EBADF = 9,
        ECHILD = 10,
        EAGAIN = 11,
        ENOMEM = 12,
        EACCES = 13,
        EFAULT = 14,
        EBUSY = 16,
        EEXIST = 17,
        EXDEV = 18,
        ENODEV = 19,
        ENOTDIR = 20,
        EISDIR = 21,
        EINVAL = 22,
        ENFILE = 23,
        EMFILE = 24,
        ENOTTY = 25,
        EFBIG = 27,
        ENOSPC = 28,
        ESPIPE = 29,
        EROFS = 30,
        EPIPE = 32,
        ERANGE = 34,
        ENAMETOOLONG = 36,
        ENOSYS = 38,
        ENOTEMPTY = 39,
        ELOOP = 40,
        ENODATA = 61,
        ENOSTR = 60,
        ETIME = 62,
        EOPNOTSUPP = 95,
        ETIMEDOUT = 110,
        /// The POSIX spelling of EAGAIN; kept as a distinct variant because fish compares against
        /// both spellings.
        EWOULDBLOCK = 10035,
    }

    impl Errno {
        pub fn from_raw(err: i32) -> Self {
            use Errno::*;
            match err {
                1 => EPERM,
                2 => ENOENT,
                3 => ESRCH,
                4 => EINTR,
                5 => EIO,
                7 => E2BIG,
                8 => ENOEXEC,
                9 => EBADF,
                10 => ECHILD,
                11 => EAGAIN,
                12 => ENOMEM,
                13 => EACCES,
                14 => EFAULT,
                16 => EBUSY,
                17 => EEXIST,
                18 => EXDEV,
                19 => ENODEV,
                20 => ENOTDIR,
                21 => EISDIR,
                22 => EINVAL,
                24 => EMFILE,
                25 => ENOTTY,
                28 => ENOSPC,
                29 => ESPIPE,
                30 => EROFS,
                32 => EPIPE,
                34 => ERANGE,
                36 => ENAMETOOLONG,
                38 => ENOSYS,
                39 => ENOTEMPTY,
                40 => ELOOP,
                95 => EOPNOTSUPP,
                110 => ETIMEDOUT,
                10035 => EWOULDBLOCK,
                _ => UnknownErrno,
            }
        }

        pub fn last_raw() -> i32 {
            std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(0)
        }

        pub fn last() -> Self {
            Self::from_raw(Self::last_raw())
        }

        pub fn as_raw(self) -> i32 {
            self as i32
        }

        pub fn desc(self) -> &'static str {
            use Errno::*;
            match self {
                EPERM => "Operation not permitted",
                ENOENT => "No such file or directory",
                ESRCH => "No such process",
                EINTR => "Interrupted system call",
                EIO => "Input/output error",
                ENOEXEC => "Exec format error",
                EBADF => "Bad file descriptor",
                ECHILD => "No child processes",
                EAGAIN | EWOULDBLOCK => "Resource temporarily unavailable",
                ENOMEM => "Cannot allocate memory",
                EACCES => "Permission denied",
                EEXIST => "File exists",
                EINVAL => "Invalid argument",
                ENOTTY => "Inappropriate ioctl for device",
                ENOSPC => "No space left on device",
                EPIPE => "Broken pipe",
                ENOSYS => "Function not implemented",
                ENOTEMPTY => "Directory not empty",
                _ => "Unknown error",
            }
        }
    }

    impl std::fmt::Display for Errno {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.desc())
        }
    }

    impl std::error::Error for Errno {}

    impl From<std::io::Error> for Errno {
        fn from(err: std::io::Error) -> Self {
            use std::io::ErrorKind;
            match err.kind() {
                ErrorKind::NotFound => Errno::ENOENT,
                ErrorKind::PermissionDenied => Errno::EACCES,
                ErrorKind::AlreadyExists => Errno::EEXIST,
                ErrorKind::WouldBlock => Errno::EAGAIN,
                ErrorKind::InvalidInput => Errno::EINVAL,
                ErrorKind::InvalidData => Errno::EIO,
                ErrorKind::Interrupted => Errno::EINTR,
                ErrorKind::BrokenPipe => Errno::EPIPE,
                ErrorKind::TimedOut => Errno::ETIMEDOUT,
                ErrorKind::Unsupported => Errno::EOPNOTSUPP,
                _ => Errno::from_raw(err.raw_os_error().unwrap_or(0)),
            }
        }
    }

    impl From<Errno> for std::io::Error {
        fn from(err: Errno) -> Self {
            use std::io::ErrorKind;
            let kind = match err {
                Errno::EPERM | Errno::EACCES => ErrorKind::PermissionDenied,
                Errno::ENOENT => ErrorKind::NotFound,
                Errno::EEXIST => ErrorKind::AlreadyExists,
                Errno::EAGAIN | Errno::EWOULDBLOCK => ErrorKind::WouldBlock,
                Errno::EINVAL => ErrorKind::InvalidInput,
                Errno::EINTR => ErrorKind::Interrupted,
                Errno::EPIPE => ErrorKind::BrokenPipe,
                Errno::ETIMEDOUT => ErrorKind::TimedOut,
                Errno::EOPNOTSUPP => ErrorKind::Unsupported,
                _ => ErrorKind::Other,
            };
            std::io::Error::new(kind, err)
        }
    }
}

macro_rules! flagset {
    ($name:ident { $($flag:ident = $value:expr),* $(,)? }) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
        pub struct $name {
            bits: i32,
        }

        impl $name {
            $(pub const $flag: Self = Self { bits: $value };)*

            pub const fn empty() -> Self {
                Self { bits: 0 }
            }

            pub const fn from_bits_truncate(bits: i32) -> Self {
                Self { bits }
            }

            pub const fn from_bits_retain(bits: i32) -> Self {
                Self { bits }
            }

            pub const fn from_bits(bits: i32) -> Option<Self> {
                Some(Self { bits })
            }

            pub const fn bits(self) -> i32 {
                self.bits
            }

            pub const fn contains(self, other: Self) -> bool {
                (self.bits & other.bits) == other.bits
            }

            pub const fn is_empty(self) -> bool {
                self.bits == 0
            }

            pub fn insert(&mut self, other: Self) {
                self.bits |= other.bits;
            }

            pub fn remove(&mut self, other: Self) {
                self.bits &= !other.bits;
            }

            pub fn set(&mut self, other: Self, value: bool) {
                if value {
                    self.insert(other);
                } else {
                    self.remove(other);
                }
            }

            pub const fn union(self, other: Self) -> Self {
                Self {
                    bits: self.bits | other.bits,
                }
            }

            pub const fn difference(self, other: Self) -> Self {
                Self {
                    bits: self.bits & !other.bits,
                }
            }

            pub const fn complement(self) -> Self {
                Self { bits: !self.bits }
            }

            pub const fn intersects(self, other: Self) -> bool {
                (self.bits & other.bits) != 0
            }
        }

        impl std::ops::BitOr for $name {
            type Output = Self;
            fn bitor(self, rhs: Self) -> Self {
                self.union(rhs)
            }
        }

        impl std::ops::BitOrAssign for $name {
            fn bitor_assign(&mut self, rhs: Self) {
                self.insert(rhs);
            }
        }

        impl std::ops::BitAnd for $name {
            type Output = Self;
            fn bitand(self, rhs: Self) -> Self {
                Self {
                    bits: self.bits & rhs.bits,
                }
            }
        }

        impl std::ops::BitAndAssign for $name {
            fn bitand_assign(&mut self, rhs: Self) {
                self.bits &= rhs.bits;
            }
        }

        impl std::ops::Not for $name {
            type Output = Self;
            fn not(self) -> Self {
                Self { bits: !self.bits }
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, concat!(stringify!($name), "({:#x})"), self.bits)
            }
        }
    };
}

/// A path argument accepted by the Windows implementations of the path-taking POSIX calls.
///
/// This mirrors `nix::NixPath` for fish's call sites, but resolves straight to a `PathBuf`: fish
/// builds OS strings that already carry Windows UTF-16 (including unpaired surrogates), and routing
/// them through a `CStr` would corrupt those names.
pub trait NixPath {
    fn path(&self) -> std::path::PathBuf;

    fn is_empty(&self) -> bool {
        self.path().as_os_str().is_empty()
    }

    fn len(&self) -> usize {
        self.path().as_os_str().len()
    }
}

impl NixPath for str {
    fn path(&self) -> std::path::PathBuf {
        std::path::PathBuf::from(self)
    }
}

impl NixPath for std::ffi::OsStr {
    fn path(&self) -> std::path::PathBuf {
        std::path::PathBuf::from(self)
    }
}

impl NixPath for std::ffi::CStr {
    fn path(&self) -> std::path::PathBuf {
        // fish's narrow paths are UTF-8 (see `wcs2zstring`).
        let bytes = self.to_bytes();
        match std::str::from_utf8(bytes) {
            Ok(s) => std::path::PathBuf::from(s),
            Err(_) => std::path::PathBuf::from(String::from_utf8_lossy(bytes).into_owned()),
        }
    }
}

impl NixPath for std::path::Path {
    fn path(&self) -> std::path::PathBuf {
        self.to_path_buf()
    }
}

impl NixPath for std::path::PathBuf {
    fn path(&self) -> std::path::PathBuf {
        self.clone()
    }
}

pub mod fcntl {
    use super::NixPath;
    use super::errno::Errno;
    use super::Result;
    use fish_common::os::{AsRawFd, OwnedFd, RawFd};
    use std::ffi::{CStr, OsString};
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt as _;

    flagset!(OFlag {
        O_RDONLY = 0,
        O_WRONLY = 1,
        O_RDWR = 2,
        O_ACCMODE = 3,
        O_CREAT = 0o100,
        O_EXCL = 0o200,
        O_NOCTTY = 0o400,
        O_TRUNC = 0o1000,
        O_APPEND = 0o2000,
        O_NONBLOCK = 0o4000,
        O_DSYNC = 0o10000,
        O_DIRECTORY = 0o200000,
        O_NOFOLLOW = 0o400000,
        O_CLOEXEC = 0o2000000,
        O_SYNC = 0o4010000,
    });

    flagset!(FdFlag {
        FD_CLOEXEC = 1,
    });

    #[derive(Clone, Copy, Debug)]
    pub enum FcntlArg {
        F_GETFD,
        F_SETFD(FdFlag),
        F_GETFL,
        F_SETFL(OFlag),
        F_DUPFD_CLOEXEC(RawFd),
    }

    pub fn fcntl<Fd: AsRawFd>(fd: Fd, arg: FcntlArg) -> Result<RawFd> {
        use fish_common::os::{
            close_fd, duplicate_fd, fd_flags_get, fd_flags_set, FD_CLOEXEC_FLAG,
            O_NONBLOCK_FLAG,
        };
        let fd = fd.as_raw_fd();
        match arg {
            FcntlArg::F_GETFD => Ok(fd_flags_get(fd)),
            FcntlArg::F_SETFD(flags) => {
                let mut current = fd_flags_get(fd);
                current &= !FD_CLOEXEC_FLAG;
                if flags.contains(FdFlag::FD_CLOEXEC) {
                    current |= FD_CLOEXEC_FLAG;
                }
                fd_flags_set(fd, current);
                Ok(0)
            }
            FcntlArg::F_GETFL => Ok(fd_flags_get(fd) & O_NONBLOCK_FLAG),
            FcntlArg::F_SETFL(flags) => {
                let mut current = fd_flags_get(fd);
                current &= !O_NONBLOCK_FLAG;
                if flags.contains(OFlag::O_NONBLOCK) {
                    current |= O_NONBLOCK_FLAG;
                }
                fd_flags_set(fd, current);
                Ok(0)
            }
            FcntlArg::F_DUPFD_CLOEXEC(min_fd) => {
                // `_dup` only ever returns the lowest free descriptor. Callers use this to move
                // pipes out of the range scripts may write to, so park temporary duplicates in
                // the slots below `min_fd` until the real duplicate lands above it, then release
                // the temporaries. Probing slots with `_get_osfhandle` is not an option: the CRT
                // aborts the process for descriptors outside its table.
                let min_fd = min_fd.max(0);
                let mut parked = Vec::new();
                let result = loop {
                    match duplicate_fd(fd) {
                        Ok(newfd) if newfd >= min_fd => break newfd,
                        Ok(newfd) => {
                            if parked.len() >= 8192 {
                                for fd in parked {
                                    close_fd(fd);
                                }
                                return Err(Errno::EMFILE);
                            }
                            parked.push(newfd);
                        }
                        Err(err) => {
                            for fd in parked {
                                close_fd(fd);
                            }
                            return Err(Errno::from(err));
                        }
                    }
                };
                for fd in parked {
                    close_fd(fd);
                }
                fd_flags_set(result, fd_flags_get(result) | FD_CLOEXEC_FLAG);
                Ok(result)
            }
        }
    }

    pub fn open<P: ?Sized + NixPath>(
        path: &P,
        flags: OFlag,
        _mode: super::sys::stat::Mode,
    ) -> Result<OwnedFd> {
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        };
        let mut options = OpenOptions::new();
        let access = flags.bits() & OFlag::O_ACCMODE.bits();
        let read = access == OFlag::O_RDONLY.bits() || access == OFlag::O_RDWR.bits();
        let write = access == OFlag::O_WRONLY.bits() || access == OFlag::O_RDWR.bits();
        options.read(read).write(write);
        if flags.contains(OFlag::O_APPEND) {
            options.append(true);
        }
        if flags.contains(OFlag::O_TRUNC) {
            options.truncate(true);
        }
        if flags.contains(OFlag::O_CREAT) {
            if flags.contains(OFlag::O_EXCL) {
                options.create_new(true);
            } else {
                options.create(true);
            }
        }
        if flags.contains(OFlag::O_DIRECTORY) {
            options.custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
        }
        if flags.contains(OFlag::O_NOFOLLOW) {
            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
        }
        options
            .open(path.path())
            .map(OwnedFd::from)
            .map_err(Errno::from)
    }

    /// Like `open`, but the result can be used where fish expects a descriptor.
    pub fn wopen<P: ?Sized + NixPath>(
        path: &P,
        flags: OFlag,
        mode: super::sys::stat::Mode,
    ) -> Result<OwnedFd> {
        open(path, flags, mode)
    }

    /// Open with O_CLOEXEC; the flag only matters for fish's own bookkeeping on Windows.
    pub fn open_cloexec<P: ?Sized + NixPath>(
        path: &P,
        flags: OFlag,
        mode: super::sys::stat::Mode,
    ) -> Result<OwnedFd> {
        open(path, flags | OFlag::O_CLOEXEC, mode)
    }

    pub fn osstring_from_cstr(s: &CStr) -> OsString {
        OsString::from(String::from_utf8_lossy(s.to_bytes()).into_owned())
    }
}

pub mod unistd {
    use super::NixPath;
    use super::errno::Errno;
    use super::Result;
    use fish_common::os::{AsFd, AsRawFd, OwnedFd, RawFd};
    use std::ffi::OsString;
    use std::path::PathBuf;

    pub use super::fcntl::OFlag;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
    pub struct Pid(i32);

    impl Pid {
        pub const fn new(pid: i32) -> Self {
            Self(pid)
        }

        pub const fn from_raw(pid: i32) -> Self {
            Self(pid)
        }

        pub const fn as_raw(self) -> i32 {
            self.0
        }

        pub const fn as_pid_t(self) -> i32 {
            self.0
        }
    }

    impl std::fmt::Display for Pid {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    pub fn getpid() -> Pid {
        Pid(unsafe { libc::getpid() })
    }

    pub fn getppid() -> Pid {
        Pid(0)
    }

    pub fn getpgrp() -> Pid {
        getpid()
    }

    /// Windows has no process groups of the POSIX kind; fish's own pgroup tracking treats every
    /// job as sharing the shell's group.
    pub fn setpgid(_pid: Pid, _pgid: Pid) -> Result<()> {
        Ok(())
    }

    pub fn getpgid(pid: Pid) -> Result<Pid> {
        Ok(pid)
    }

    pub fn read<Fd: AsRawFd>(fd: Fd, buf: &mut [u8]) -> Result<usize> {
        fish_common::os::read_fd(fd.as_raw_fd(), buf).map_err(Errno::from)
    }

    pub fn write<Fd: AsRawFd>(fd: Fd, buf: &[u8]) -> Result<usize> {
        fish_common::os::write_fd(fd.as_raw_fd(), buf).map_err(Errno::from)
    }

    pub fn close(fd: RawFd) -> Result<()> {
        fish_common::os::close_fd(fd);
        Ok(())
    }

    pub fn pipe() -> Result<(OwnedFd, OwnedFd)> {
        fish_common::os::make_pipe().map_err(Errno::from)
    }

    pub fn pipe2(_flags: OFlag) -> Result<(OwnedFd, OwnedFd)> {
        pipe()
    }

    pub fn dup(fd: RawFd) -> Result<RawFd> {
        fish_common::os::duplicate_fd(fd).map_err(Errno::from)
    }

    pub fn dup2(fd: RawFd, target: RawFd) -> Result<RawFd> {
        fish_common::os::duplicate_fd_to(fd, target).map_err(Errno::from)
    }

    pub fn mkdir<P: ?Sized + NixPath>(path: &P, _mode: super::sys::stat::Mode) -> Result<()> {
        std::fs::create_dir(path.path()).map_err(Errno::from)
    }

    pub fn rmdir<P: ?Sized + NixPath>(path: &P) -> Result<()> {
        std::fs::remove_dir(path.path()).map_err(Errno::from)
    }

    pub fn unlink<P: ?Sized + NixPath>(path: &P) -> Result<()> {
        std::fs::remove_file(path.path()).map_err(Errno::from)
    }

    /// Change the working directory to the directory represented by an open descriptor.
    ///
    /// Windows has no `fchdir`, but the final path of the handle can be recovered and made current.
    pub fn fchdir<Fd: AsFd>(fd: Fd) -> Result<()> {
        use std::os::windows::ffi::OsStringExt as _;
        use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;

        let handle = fish_common::os::handle_from_fd(fd.as_fd().as_raw_fd());
        if handle.is_null() {
            return Err(Errno::EBADF);
        }
        let mut buf: Vec<u16> = vec![0; 512];
        loop {
            let len = unsafe {
                GetFinalPathNameByHandleW(
                    handle.cast(),
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    0, // FILE_NAME_NORMALIZED | VOLUME_NAME_DOS
                )
            };
            if len == 0 {
                return Err(Errno::last());
            }
            if (len as usize) < buf.len() {
                buf.truncate(len as usize);
                break;
            }
            buf.resize(len as usize + 1, 0);
        }
        let path = PathBuf::from(OsString::from_wide(&buf));
        // GetFinalPathNameByHandleW always yields a verbatim path ("\\?\C:\..."). Making a verbatim
        // path the process working directory breaks relative path resolution and makes drive-less
        // rooted paths such as "/etc/fish" resolve as UNC server names, stalling for seconds per
        // lookup. Prefer the plain form and fall back when it cannot be represented (MAX_PATH).
        if let Some(plain) = strip_verbatim_prefix(&path) {
            if std::env::set_current_dir(&plain).is_ok() {
                return Ok(());
            }
        }
        std::env::set_current_dir(path).map_err(Errno::from)
    }

    pub fn getcwd() -> Result<PathBuf> {
        std::env::current_dir().map_err(Errno::from)
    }

    /// Convert a verbatim path ("\\?\C:\x" or "\\?\UNC\server\share") to the plain form.
    /// Returns None when the path is not a verbatim drive/UNC path and must stay verbatim.
    fn strip_verbatim_prefix(path: &std::path::Path) -> Option<PathBuf> {
        use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};

        const VERBATIM: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
        const UNC: [u16; 4] = [b'U' as u16, b'N' as u16, b'C' as u16, b'\\' as u16];

        let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if !wide.starts_with(&VERBATIM) {
            return None;
        }
        let rest = &wide[VERBATIM.len()..];
        let plain = if rest.starts_with(&UNC) {
            let mut plain = vec![b'\\' as u16, b'\\' as u16];
            plain.extend_from_slice(&rest[UNC.len()..]);
            plain
        } else if rest.len() >= 3 && rest[1] == b':' as u16 {
            rest.to_vec()
        } else {
            return None;
        };
        Some(PathBuf::from(OsString::from_wide(&plain)))
    }

    pub fn ttyname(fd: RawFd) -> Result<OsString> {
        if !fish_common::os::is_tty(fd) {
            return Err(Errno::ENOTTY);
        }
        let name = if fd == fish_common::os::STDIN_FILENO {
            "CONIN$"
        } else {
            "CONOUT$"
        };
        Ok(OsString::from(name))
    }

    pub fn gethostname() -> Result<OsString> {
        std::env::var_os("COMPUTERNAME").ok_or(Errno::ENOENT)
    }

    flagset!(AccessFlags {
        R_OK = 4,
        W_OK = 2,
        X_OK = 1,
        F_OK = 0,
    });

    pub fn access<P: ?Sized + NixPath>(path: &P, mode: AccessFlags) -> Result<()> {
        let path = path.path();
        let metadata = std::fs::metadata(&path).map_err(Errno::from)?;
        let readonly = metadata.permissions().readonly();
        if mode.contains(AccessFlags::W_OK) && readonly {
            return Err(Errno::EACCES);
        }
        if mode.contains(AccessFlags::X_OK) {
            let executable = metadata.is_dir()
                || path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|ext| {
                        matches!(
                            ext.to_ascii_lowercase().as_str(),
                            "exe" | "com" | "bat" | "cmd" | "ps1"
                        )
                    });
            if !executable {
                return Err(Errno::EACCES);
            }
        }
        Ok(())
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Whence {
        SeekSet,
        SeekCur,
        SeekEnd,
    }

    pub fn lseek<Fd: AsRawFd>(fd: Fd, offset: i64, whence: Whence) -> Result<i64> {
        let fd = fd.as_raw_fd();
        // The CRT's lseek accepts console and pipe descriptors and only fails once the position is
        // actually queried, with an errno that says nothing. POSIX reports ESPIPE for those.
        {
            use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_CHAR, FILE_TYPE_PIPE, GetFileType};
            let handle = fish_common::os::handle_from_fd(fd);
            if !handle.is_null() {
                let ty = unsafe { GetFileType(handle) };
                if ty == FILE_TYPE_PIPE || ty == FILE_TYPE_CHAR {
                    return Err(Errno::ESPIPE);
                }
            }
        }
        let (offset, origin) = match whence {
            Whence::SeekSet => (offset, libc::SEEK_SET),
            Whence::SeekCur => (offset, libc::SEEK_CUR),
            Whence::SeekEnd => (offset, libc::SEEK_END),
        };
        let ret = unsafe { libc::lseek64(fd, offset, origin) };
        if ret < 0 {
            Err(Errno::last())
        } else {
            Ok(ret)
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
    pub struct Uid(u32);

    impl Uid {
        pub fn effective() -> Self {
            Uid(account().id)
        }
        pub fn real() -> Self {
            Uid(account().id)
        }
        pub fn as_raw(self) -> u32 {
            self.0
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
    pub struct Gid(u32);

    impl Gid {
        pub fn effective() -> Self {
            Self(0)
        }
        pub fn real() -> Self {
            Self(0)
        }
        pub fn as_raw(self) -> u32 {
            self.0
        }
    }

    pub fn getuid() -> Uid {
        Uid(account().id)
    }

    pub fn geteuid() -> Uid {
        getuid()
    }

    pub fn getgid() -> Gid {
        Gid(0)
    }

    pub fn getegid() -> Gid {
        Gid(0)
    }

    /// The Windows account running this process. Windows has no passwd database and no numeric
    /// uid, so the fields fish needs are taken from the process token and the account database.
    pub(crate) struct Account {
        name: String,
        /// The RID of the token's user SID. It is the only real numeric identity Windows exposes;
        /// 0 is reserved for the LocalSystem-like accounts and never returned for a user SID.
        id: u32,
        dir: PathBuf,
    }

    pub(crate) fn account() -> &'static Account {
        static ACCOUNT: std::sync::OnceLock<Account> = std::sync::OnceLock::new();
        ACCOUNT.get_or_init(|| {
            let name = current_account_name()
                .or_else(|| std::env::var("USERNAME").ok())
                .unwrap_or_default();
            // Falling back to a non-zero id keeps an ordinary account from being mistaken for
            // root when the token cannot be queried.
            let id = current_account_id().unwrap_or(1000);
            let dir = std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(PathBuf::from)
                .unwrap_or_default();
            Account { name, id, dir }
        })
    }

    fn current_account_name() -> Option<String> {
        use windows_sys::Win32::System::WindowsProgramming::GetUserNameW;

        let mut len: u32 = 0;
        unsafe { GetUserNameW(std::ptr::null_mut(), &mut len) };
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u16; len as usize];
        if unsafe { GetUserNameW(buf.as_mut_ptr(), &mut len) } == 0 {
            return None;
        }
        buf.truncate((len as usize).saturating_sub(1));
        Some(String::from_utf16_lossy(&buf))
    }

    fn current_account_id() -> Option<u32> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Security::{
            GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_QUERY,
            TOKEN_USER, TokenUser,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        unsafe {
            let mut token = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return None;
            }
            let mut needed: u32 = 0;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
            let mut buf = vec![0u8; needed as usize];
            let ok = needed != 0
                && GetTokenInformation(
                    token,
                    TokenUser,
                    buf.as_mut_ptr().cast(),
                    needed,
                    &mut needed,
                ) != 0;
            CloseHandle(token);
            if !ok {
                return None;
            }
            let user = &*buf.as_ptr().cast::<TOKEN_USER>();
            let count = *GetSidSubAuthorityCount(user.User.Sid);
            if count == 0 {
                return None;
            }
            Some(*GetSidSubAuthority(user.User.Sid, u32::from(count) - 1))
        }
    }

    #[derive(Clone, Debug)]
    pub struct User {
        pub name: String,
        pub uid: Uid,
        pub gid: Gid,
        pub dir: PathBuf,
        pub shell: PathBuf,
    }

    impl User {
        pub fn from_name(name: &str) -> Result<Option<Self>> {
            // Only the account running the shell can be resolved; that is the one fish uses to
            // populate $USER and $HOME.
            let account = account();
            if !account.name.is_empty() && name.eq_ignore_ascii_case(&account.name) {
                Ok(Some(Self::from_account(account)))
            } else {
                Ok(None)
            }
        }

        pub fn from_uid(uid: Uid) -> Result<Option<Self>> {
            let account = account();
            if uid.0 == account.id {
                Ok(Some(Self::from_account(account)))
            } else {
                Ok(None)
            }
        }

        fn from_account(account: &Account) -> Self {
            Self {
                name: account.name.clone(),
                uid: Uid(account.id),
                gid: Gid(0),
                dir: account.dir.clone(),
                shell: PathBuf::new(),
            }
        }
    }

    pub fn mkfifo<P: ?Sized + NixPath>(_path: &P, _mode: super::sys::stat::Mode) -> Result<()> {
        Err(Errno::EOPNOTSUPP)
    }

    pub use crate::sys::termios::{tcgetpgrp, tcsetpgrp};
}

pub mod sys {
    pub mod stat {
        flagset!(Mode {
            S_IRWXU = 0o700,
            S_IRUSR = 0o400,
            S_IWUSR = 0o200,
            S_IXUSR = 0o100,
            S_IRWXG = 0o070,
            S_IRGRP = 0o040,
            S_IWGRP = 0o020,
            S_IXGRP = 0o010,
            S_IRWXO = 0o007,
            S_IROTH = 0o004,
            S_IWOTH = 0o002,
            S_IXOTH = 0o001,
            S_ISUID = 0o4000,
            S_ISGID = 0o2000,
            S_ISVTX = 0o1000,
        });

        pub fn umask(_mask: Mode) -> Mode {
            // Windows file permissions do not have a umask. Report "no mask" so callers do not
            // accidentally restrict their own files.
            Mode::empty()
        }
    }

    pub mod statfs {
        use super::super::Result;
        use std::ffi::CStr;

        /// Placeholder for the Linux-only statfs interface; remote-filesystem detection has its
        /// own branch on Windows.
        pub struct Statfs {
            f_type: u64,
        }

        impl Statfs {
            pub fn filesystem_type(&self) -> (u64,) {
                (self.f_type,)
            }
        }

        pub fn statfs(_path: &CStr) -> Result<Statfs> {
            Ok(Statfs { f_type: 0 })
        }
    }

    pub mod time {
        use std::time::Duration;

        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
        pub struct TimeVal {
            pub tv_sec: i64,
            pub tv_usec: i64,
        }

        impl TimeVal {
            pub const fn new(sec: i64, usec: i64) -> Self {
                Self {
                    tv_sec: sec,
                    tv_usec: usec,
                }
            }
            pub const fn tv_sec(&self) -> i64 {
                self.tv_sec
            }
            pub const fn tv_usec(&self) -> i64 {
                self.tv_usec
            }
        }

        impl From<Duration> for TimeVal {
            fn from(d: Duration) -> Self {
                Self::new(d.as_secs() as i64, d.subsec_micros() as i64)
            }
        }

        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
        pub struct TimeSpec {
            pub tv_sec: i64,
            pub tv_nsec: i64,
        }

        impl TimeSpec {
            pub const fn new(sec: i64, nsec: i64) -> Self {
                Self {
                    tv_sec: sec,
                    tv_nsec: nsec,
                }
            }
            pub const fn tv_sec(&self) -> i64 {
                self.tv_sec
            }
            pub const fn tv_nsec(&self) -> i64 {
                self.tv_nsec
            }
            pub const fn from_duration(duration: Duration) -> Self {
                Self::new(duration.as_secs() as i64, duration.subsec_nanos() as i64)
            }
            pub const fn from_secs(secs: i64) -> Self {
                Self::new(secs, 0)
            }
            pub fn to_duration(&self) -> Duration {
                Duration::new(self.tv_sec as u64, self.tv_nsec as u32)
            }
        }
    }

    pub mod select {
        use super::super::errno::Errno;
        use super::super::Result;
        use super::signal::SigSet;
        use super::time::TimeSpec;
        use std::time::{Duration, Instant};

        #[derive(Default)]
        pub struct FdSet {
            fds: Vec<i32>,
        }

        impl FdSet {
            pub fn new() -> Self {
                Self::default()
            }
            pub fn insert<Fd: fish_common::os::AsFd>(&mut self, fd: Fd) {
                use fish_common::os::AsRawFd as _;
                let fd = fd.as_fd().as_raw_fd();
                if !self.fds.contains(&fd) {
                    self.fds.push(fd);
                }
            }
            pub fn remove(&mut self, fd: i32) {
                self.fds.retain(|f| *f != fd);
            }
            pub fn contains(&self, fd: i32) -> bool {
                self.fds.contains(&fd)
            }
            pub fn fds(&self) -> impl Iterator<Item = i32> + '_ {
                self.fds.iter().copied()
            }
        }

        /// Whether a descriptor has data waiting.
        pub fn fd_readable(fd: i32) -> bool {
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_TYPE_PIPE, GetFileType,
            };
            use windows_sys::Win32::System::Console::GetConsoleMode;
            use windows_sys::Win32::System::Pipes::PeekNamedPipe;
            use windows_sys::Win32::System::Threading::WaitForSingleObject;

            let handle = fish_common::os::handle_from_fd(fd);
            if handle.is_null() {
                return false;
            }
            let handle = handle.cast();

            // Console input handles signal when input records are available. A multi-byte
            // character whose leading byte was already read leaves no record behind, though;
            // its remaining bytes are waiting in fish.
            let mut mode = 0u32;
            if unsafe { GetConsoleMode(handle, &mut mode) } != 0 {
                return fish_common::os::console_has_pending_input()
                    || unsafe { WaitForSingleObject(handle, 0) } == 0;
            }

            let file_type = unsafe { GetFileType(handle) };
            if file_type == FILE_TYPE_PIPE {
                let mut avail = 0u32;
                let ok = unsafe {
                    PeekNamedPipe(handle, std::ptr::null_mut(), 0, std::ptr::null_mut(), &mut avail, std::ptr::null_mut())
                };
                // A broken pipe reads as end-of-file.
                ok == 0 || avail > 0
            } else {
                // Regular files are always readable.
                true
            }
        }

        /// Wait until at least one descriptor in the read set is ready, or the timeout expires.
        ///
        /// `nfds`, the write/except sets and the signal mask are accepted for signature
        /// compatibility; fish only ever waits for readability on its input fd.
        pub fn pselect<'a, N, R, W, E, T, S>(
            _nfds: N,
            readfds: R,
            _writefds: W,
            _exceptfds: E,
            timeout: T,
            _sigmask: S,
        ) -> Result<i32>
        where
            N: Into<Option<i32>>,
            R: Into<Option<&'a mut FdSet>>,
            W: Into<Option<&'a mut FdSet>>,
            E: Into<Option<&'a mut FdSet>>,
            T: Into<Option<&'a TimeSpec>>,
            S: Into<Option<&'a SigSet>>,
        {
            let Some(readfds) = readfds.into() else {
                return Ok(0);
            };
            let timeout = timeout.into();
            let deadline = timeout.map(|t| Instant::now() + t.to_duration());
            loop {
                let ready = readfds.fds().filter(|fd| fd_readable(*fd)).count();
                if ready > 0 {
                    return Ok(ready as i32);
                }
                if let Some(deadline) = deadline {
                    let now = Instant::now();
                    if now >= deadline {
                        return Ok(0);
                    }
                    let remaining = deadline - now;
                    std::thread::sleep(remaining.min(Duration::from_millis(1)));
                } else {
                    return Err(Errno::ENOSYS);
                }
            }
        }
    }

    pub mod signal {
        use super::super::errno::Errno;
        use super::super::Result;
        use super::super::unistd::Pid;

        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[repr(i32)]
        pub enum Signal {
            SIGHUP = 1,
            SIGINT = 2,
            SIGQUIT = 3,
            SIGILL = 4,
            SIGTRAP = 5,
            SIGABRT = 6,
            SIGBUS = 7,
            SIGFPE = 8,
            SIGKILL = 9,
            SIGUSR1 = 10,
            SIGSEGV = 11,
            SIGUSR2 = 12,
            SIGPIPE = 13,
            SIGALRM = 14,
            SIGTERM = 15,
            SIGSTKFLT = 16,
            SIGCHLD = 17,
            SIGCONT = 18,
            SIGSTOP = 19,
            SIGTSTP = 20,
            SIGTTIN = 21,
            SIGTTOU = 22,
            SIGURG = 23,
            SIGXCPU = 24,
            SIGXFSZ = 25,
            SIGVTALRM = 26,
            SIGPROF = 27,
            SIGWINCH = 28,
            SIGIO = 29,
            SIGPWR = 30,
            SIGSYS = 31,
        }

        impl Signal {
            pub fn as_str(self) -> &'static str {
                match self {
                    Signal::SIGHUP => "SIGHUP",
                    Signal::SIGINT => "SIGINT",
                    Signal::SIGQUIT => "SIGQUIT",
                    Signal::SIGILL => "SIGILL",
                    Signal::SIGTRAP => "SIGTRAP",
                    Signal::SIGABRT => "SIGABRT",
                    Signal::SIGBUS => "SIGBUS",
                    Signal::SIGFPE => "SIGFPE",
                    Signal::SIGKILL => "SIGKILL",
                    Signal::SIGUSR1 => "SIGUSR1",
                    Signal::SIGSEGV => "SIGSEGV",
                    Signal::SIGUSR2 => "SIGUSR2",
                    Signal::SIGPIPE => "SIGPIPE",
                    Signal::SIGALRM => "SIGALRM",
                    Signal::SIGTERM => "SIGTERM",
                    Signal::SIGCHLD => "SIGCHLD",
                    Signal::SIGCONT => "SIGCONT",
                    Signal::SIGSTOP => "SIGSTOP",
                    Signal::SIGTSTP => "SIGTSTP",
                    Signal::SIGTTIN => "SIGTTIN",
                    Signal::SIGTTOU => "SIGTTOU",
                    Signal::SIGWINCH => "SIGWINCH",
                    _ => "SIGUNKNOWN",
                }
            }
        }

        impl std::fmt::Display for Signal {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.as_str())
            }
        }

        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
        pub struct SigSet {
            bits: u64,
        }

        impl SigSet {
            pub fn empty() -> Self {
                Self { bits: 0 }
            }
            pub fn all() -> Self {
                Self { bits: u64::MAX }
            }
            pub fn thread_get_mask() -> Result<Self> {
                Ok(Self::empty())
            }
            pub fn thread_set_mask(&mut self) -> Result<()> {
                Ok(())
            }
            pub fn thread_swap_mask(&self, _how: SigmaskHow) -> Result<Self> {
                Ok(Self::empty())
            }
            pub fn add(&mut self, sig: Signal) {
                self.add_raw(sig as i32);
            }
            pub fn add_raw(&mut self, sig: i32) {
                self.bits |= 1u64 << sig;
            }
            pub fn remove(&mut self, sig: Signal) {
                self.bits &= !(1 << (sig as u32));
            }
            pub fn contains(&self, sig: Signal) -> bool {
                self.bits & (1 << (sig as u32)) != 0
            }
        }

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum SigmaskHow {
            SIG_BLOCK,
            SIG_UNBLOCK,
            SIG_SETMASK,
        }

        /// Windows has no per-thread signal masks in the POSIX sense; fish's blocking is emulated
        /// by the console handler and the topic monitor, so masking is a no-op.
        pub fn sigprocmask(
            _how: SigmaskHow,
            _set: Option<&SigSet>,
            _oldset: Option<&mut SigSet>,
        ) -> Result<()> {
            Ok(())
        }

        flagset!(SaFlags {
            SA_NOCLDSTOP = 1,
            SA_NOCLDWAIT = 2,
            SA_SIGINFO = 4,
            SA_RESTART = 0x10000000,
            SA_ONSTACK = 0x08000000,
            SA_RESETHAND = -2147483648,
        });

        #[derive(Clone, Copy, Debug)]
        pub enum SigHandler {
            SigDfl,
            SigIgn,
            SigAction(extern "C" fn(i32)),
        }

        #[derive(Clone, Copy, Debug)]
        pub struct SigAction {
            pub handler: SigHandler,
            pub flags: SaFlags,
            pub mask: SigSet,
        }

        impl SigAction {
            pub fn new(handler: SigHandler, flags: SaFlags, mask: SigSet) -> Self {
                Self {
                    handler,
                    flags,
                    mask,
                }
            }
        }

        /// Deliver a signal to a process.
        pub fn kill<T: Into<Option<Signal>>>(pid: Pid, sig: T) -> Result<()> {
            match sig.into() {
                Some(sig) => super::super::process::signal_process(pid.as_raw(), sig),
                // Signal 0 is a liveness probe; a registered child is alive until reaped.
                None => {
                    if super::super::process::child_exists(pid.as_raw()) {
                        Ok(())
                    } else {
                        Err(Errno::ESRCH)
                    }
                }
            }
        }

        /// Deliver a signal to a process group.
        pub fn killpg<T: Into<Option<Signal>>>(pgid: Pid, sig: T) -> Result<()> {
            match sig.into() {
                Some(sig) => super::super::process::signal_process_group(pgid.as_raw(), sig),
                None => Ok(()),
            }
        }

        pub fn raise(sig: Signal) -> Result<()> {
            super::super::process::signal_process(unsafe { libc::getpid() }, sig)
        }
    }

    pub mod termios {
        use super::super::errno::Errno;
        use super::super::unistd::Pid;
        use super::super::Result;
        use fish_common::os::AsFd;

        /// The number of control characters, matching Linux's `NCCS`.
        pub const NCCS: usize = 32;

        /// The value which disables a control character.
        pub const _POSIX_VDISABLE: u8 = 0;

        flagset!(InputFlags {
            IGNBRK = 0o1,
            BRKINT = 0o2,
            IGNPAR = 0o4,
            PARMRK = 0o10,
            INPCK = 0o20,
            ISTRIP = 0o40,
            INLCR = 0o100,
            IGNCR = 0o200,
            ICRNL = 0o400,
            IUCLC = 0o1000,
            IXON = 0o2000,
            IXANY = 0o4000,
            IXOFF = 0o10000,
            IMAXBEL = 0o20000,
            IUTF8 = 0o40000,
        });

        flagset!(OutputFlags {
            OPOST = 0o1,
            OLCUC = 0o2,
            ONLCR = 0o4,
            OCRNL = 0o10,
            ONOCR = 0o20,
            ONLRET = 0o40,
        });

        flagset!(ControlFlags {
            CSIZE = 0o60,
            CS5 = 0o0,
            CS6 = 0o20,
            CS7 = 0o40,
            CS8 = 0o60,
            CSTOPB = 0o100,
            CREAD = 0o200,
            PARENB = 0o400,
            PARODD = 0o1000,
            HUPCL = 0o2000,
            CLOCAL = 0o4000,
        });

        flagset!(LocalFlags {
            ISIG = 0o1,
            ICANON = 0o2,
            XCASE = 0o4,
            ECHO = 0o10,
            ECHOE = 0o20,
            ECHOK = 0o40,
            ECHONL = 0o100,
            NOFLSH = 0o200,
            TOSTOP = 0o400,
            ECHOCTL = 0o1000,
            ECHOPRT = 0o2000,
            ECHOKE = 0o4000,
            FLUSHO = 0o10000,
            PENDIN = 0o40000,
            IEXTEN = 0o100000,
        });

        /// Indices into the control character array.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[repr(usize)]
        pub enum SpecialCharacterIndices {
            VINTR = 0,
            VQUIT = 1,
            VERASE = 2,
            VKILL = 3,
            VEOF = 4,
            VTIME = 5,
            VMIN = 6,
            VSWTC = 7,
            VSTART = 8,
            VSTOP = 9,
            VSUSP = 10,
            VEOL = 11,
            VREPRINT = 12,
            VDISCARD = 13,
            VWERASE = 14,
            VLNEXT = 15,
            VEOL2 = 16,
        }

        pub const VINTR: SpecialCharacterIndices = SpecialCharacterIndices::VINTR;
        pub const VQUIT: SpecialCharacterIndices = SpecialCharacterIndices::VQUIT;
        pub const VERASE: SpecialCharacterIndices = SpecialCharacterIndices::VERASE;
        pub const VKILL: SpecialCharacterIndices = SpecialCharacterIndices::VKILL;
        pub const VEOF: SpecialCharacterIndices = SpecialCharacterIndices::VEOF;
        pub const VTIME: SpecialCharacterIndices = SpecialCharacterIndices::VTIME;
        pub const VMIN: SpecialCharacterIndices = SpecialCharacterIndices::VMIN;
        pub const VSTART: SpecialCharacterIndices = SpecialCharacterIndices::VSTART;
        pub const VSTOP: SpecialCharacterIndices = SpecialCharacterIndices::VSTOP;
        pub const VSUSP: SpecialCharacterIndices = SpecialCharacterIndices::VSUSP;
        pub const VEOL: SpecialCharacterIndices = SpecialCharacterIndices::VEOL;
        pub const VREPRINT: SpecialCharacterIndices = SpecialCharacterIndices::VREPRINT;
        pub const VDISCARD: SpecialCharacterIndices = SpecialCharacterIndices::VDISCARD;
        pub const VWERASE: SpecialCharacterIndices = SpecialCharacterIndices::VWERASE;
        pub const VLNEXT: SpecialCharacterIndices = SpecialCharacterIndices::VLNEXT;
        pub const VEOL2: SpecialCharacterIndices = SpecialCharacterIndices::VEOL2;

        impl SpecialCharacterIndices {
            pub const fn as_usize(self) -> usize {
                self as usize
            }
        }

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum SetArg {
            TCSANOW,
            TCSADRAIN,
            TCSAFLUSH,
        }

        /// A terminal mode set.
        ///
        /// On Windows the input and output halves of a terminal have separate mode words. This
        /// struct keeps the POSIX fields and converts them on the way in and out of the console.
        #[derive(Clone, Debug, Eq, PartialEq)]
        pub struct Termios {
            pub input_flags: InputFlags,
            pub output_flags: OutputFlags,
            pub control_flags: ControlFlags,
            pub local_flags: LocalFlags,
            pub control_chars: [u8; NCCS],
        }

        impl Default for Termios {
            fn default() -> Self {
                Self {
                    input_flags: InputFlags::empty(),
                    output_flags: OutputFlags::empty(),
                    control_flags: ControlFlags::CS8 | ControlFlags::CREAD,
                    local_flags: LocalFlags::empty(),
                    control_chars: default_control_chars(),
                }
            }
        }

        fn default_control_chars() -> [u8; NCCS] {
            let mut cc = [_POSIX_VDISABLE; NCCS];
            cc[SpecialCharacterIndices::VINTR as usize] = 3; // ^C
            cc[SpecialCharacterIndices::VQUIT as usize] = 28; // ^\
            // Unix terminals send DEL for backspace; fish's key canonicalization
            // checks VERASE to decide whether DEL is backspace or delete.
            cc[SpecialCharacterIndices::VERASE as usize] = 127; // DEL
            cc[SpecialCharacterIndices::VKILL as usize] = 21; // ^U
            cc[SpecialCharacterIndices::VEOF as usize] = 4; // ^D
            cc[SpecialCharacterIndices::VSTART as usize] = 17; // ^Q
            cc[SpecialCharacterIndices::VSTOP as usize] = 19; // ^S
            cc[SpecialCharacterIndices::VSUSP as usize] = 26; // ^Z
            cc[SpecialCharacterIndices::VREPRINT as usize] = 18; // ^R
            cc[SpecialCharacterIndices::VWERASE as usize] = 23; // ^W
            cc[SpecialCharacterIndices::VLNEXT as usize] = 22; // ^V
            cc[SpecialCharacterIndices::VMIN as usize] = 1;
            cc[SpecialCharacterIndices::VTIME as usize] = 0;
            cc
        }

        use windows_sys::Win32::System::Console::{
            ENABLE_ECHO_INPUT, ENABLE_EXTENDED_FLAGS, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
            ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
            ENABLE_VIRTUAL_TERMINAL_PROCESSING, ENABLE_WINDOW_INPUT, ENABLE_WRAP_AT_EOL_OUTPUT,
            DISABLE_NEWLINE_AUTO_RETURN,
        };

        /// Read the console mode word for a descriptor, or `ENOTTY` when it is not a console.
        fn get_mode(fd: i32, input: bool) -> Result<u32> {
            use windows_sys::Win32::System::Console::GetConsoleMode;
            let handle = fish_common::os::handle_from_fd(fd);
            if handle.is_null() {
                return Err(Errno::EBADF);
            }
            let mut mode = 0u32;
            if unsafe { GetConsoleMode(handle.cast(), &mut mode) } == 0 {
                return Err(if input {
                    Errno::ENOTTY
                } else {
                    Errno::ENOTTY
                });
            }
            Ok(mode)
        }

        fn set_mode(fd: i32, mode: u32) -> Result<()> {
            use windows_sys::Win32::System::Console::SetConsoleMode;
            let handle = fish_common::os::handle_from_fd(fd);
            if handle.is_null() {
                return Err(Errno::EBADF);
            }
            if unsafe { SetConsoleMode(handle.cast(), mode) } == 0 {
                return Err(Errno::last());
            }
            Ok(())
        }

        /// Input descriptors are the standard input (or a duplicate of it); the shell only owns
        /// one console, so output settings ride along with the input descriptor.
        fn is_input_fd(fd: i32) -> bool {
            fd == fish_common::os::STDIN_FILENO
        }

        pub fn tcgetattr<Fd: AsFd>(fd: Fd) -> Result<Termios> {
            let fd = fd.as_fd().as_raw_fd();
            let in_mode = get_mode(fd, true)?;
            let out_mode = if is_input_fd(fd) {
                get_mode(fish_common::os::STDOUT_FILENO, false).unwrap_or(0)
            } else {
                get_mode(fd, false)?
            };

            let mut t = Termios::default();
            if in_mode & ENABLE_PROCESSED_INPUT != 0 {
                t.local_flags.insert(LocalFlags::ISIG);
            }
            if in_mode & ENABLE_LINE_INPUT != 0 {
                t.local_flags.insert(LocalFlags::ICANON);
            }
            if in_mode & ENABLE_ECHO_INPUT != 0 {
                t.local_flags.insert(LocalFlags::ECHO);
            }
            if out_mode != 0 {
                t.output_flags.insert(OutputFlags::OPOST);
                if out_mode & DISABLE_NEWLINE_AUTO_RETURN == 0 {
                    t.output_flags.insert(OutputFlags::ONLCR);
                }
            }
            Ok(t)
        }

        pub fn tcsetattr<Fd: AsFd>(fd: Fd, _action: SetArg, termios: &Termios) -> Result<()> {
            let fd = fd.as_fd().as_raw_fd();

            let mut in_mode = ENABLE_EXTENDED_FLAGS
                | ENABLE_WINDOW_INPUT
                | ENABLE_VIRTUAL_TERMINAL_INPUT;
            if termios.local_flags.contains(LocalFlags::ISIG) {
                in_mode |= ENABLE_PROCESSED_INPUT;
            }
            if termios.local_flags.contains(LocalFlags::ICANON) {
                in_mode |= ENABLE_LINE_INPUT;
            }
            if termios.local_flags.contains(LocalFlags::ECHO) {
                in_mode |= ENABLE_ECHO_INPUT;
            }

            let mut out_mode =
                ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING | ENABLE_WRAP_AT_EOL_OUTPUT;
            if !termios.output_flags.contains(OutputFlags::ONLCR) {
                out_mode |= DISABLE_NEWLINE_AUTO_RETURN;
            }

            // The input settings go to the descriptor itself; the output settings always belong to
            // the console's output handle, which fish reaches through stdout.
            set_mode(fd, in_mode)?;
            let _ = set_mode(fish_common::os::STDOUT_FILENO, out_mode);
            Ok(())
        }

        /// The foreground process group.
        ///
        /// Windows has no such concept; the shell is always the owner of its console.
        pub fn tcgetpgrp<Fd: AsFd>(fd: Fd) -> Result<Pid> {
            if fish_common::os::is_tty(fd.as_fd().as_raw_fd()) {
                Ok(super::super::unistd::getpgrp())
            } else {
                Err(Errno::ENOTTY)
            }
        }

        pub fn tcsetpgrp<Fd: AsFd>(fd: Fd, _pgrp: Pid) -> Result<()> {
            if fish_common::os::is_tty(fd.as_fd().as_raw_fd()) {
                Ok(())
            } else {
                Err(Errno::ENOTTY)
            }
        }
    }

    pub mod wait {
        use super::super::errno::Errno;
        use super::super::unistd::Pid;
        use super::super::Result;
        use super::signal::Signal;

        flagset!(WaitPidFlag {
            WNOHANG = 1,
            WUNTRACED = 2,
            WCONTINUED = 8,
        });

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum WaitStatus {
            Exited(Pid, i32),
            Signaled(Pid, Signal, bool),
            Stopped(Pid, Signal),
            Continued(Pid),
            StillAlive,
        }

        pub fn waitpid<P: Into<Option<Pid>>>(
            pid: P,
            flags: Option<WaitPidFlag>,
        ) -> Result<WaitStatus> {
            let pid = pid.into().ok_or(Errno::ECHILD)?;
            super::super::process::wait_for_child(
                pid,
                flags.is_some_and(|f| f.contains(WaitPidFlag::WNOHANG)),
            )
        }
    }

    pub mod resource {
        use super::super::Result;
        use super::time::TimeVal;

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        #[repr(i32)]
        pub enum Resource {
            RLIMIT_CPU,
            RLIMIT_FSIZE,
            RLIMIT_DATA,
            RLIMIT_STACK,
            RLIMIT_CORE,
            RLIMIT_RSS,
            RLIMIT_NPROC,
            RLIMIT_NOFILE,
            RLIMIT_MEMLOCK,
            RLIMIT_AS,
            RLIMIT_LOCKS,
            RLIMIT_SIGPENDING,
            RLIMIT_MSGQUEUE,
            RLIMIT_NICE,
            RLIMIT_RTPRIO,
            RLIMIT_RTTIME,
        }

        pub type Rlim = u64;
        pub type rlim_t = Rlim;
        pub const RLIM_INFINITY: Rlim = u64::MAX;

        /// Resource limits are not a Windows concept; report "unlimited" and ignore changes.
        pub fn getrlimit(_resource: Resource) -> Result<(Rlim, Rlim)> {
            Ok((RLIM_INFINITY, RLIM_INFINITY))
        }

        pub fn setrlimit(_resource: Resource, _soft: Rlim, _hard: Rlim) -> Result<()> {
            Ok(())
        }

        #[derive(Clone, Copy, Debug, Default)]
        pub struct RUsageRaw {
            pub ru_utime: TimeVal,
            pub ru_stime: TimeVal,
            pub ru_maxrss: i64,
            pub ru_nsignals: i64,
        }

        /// CPU usage of the shell or its children.
        #[derive(Clone, Copy, Debug, Default)]
        pub struct Usage(RUsageRaw);

        impl Usage {
            pub fn user_time(&self) -> TimeVal {
                self.0.ru_utime
            }
            pub fn system_time(&self) -> TimeVal {
                self.0.ru_stime
            }
            pub fn max_rss(&self) -> i64 {
                self.0.ru_maxrss
            }
            pub fn signals(&self) -> i64 {
                self.0.ru_nsignals
            }
            pub fn as_mut(&mut self) -> &mut RUsageRaw {
                &mut self.0
            }
        }

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum UsageWho {
            RUSAGE_SELF,
            RUSAGE_CHILDREN,
            RUSAGE_THREAD,
        }

        fn process_times() -> RUsageRaw {
            use windows_sys::Win32::Foundation::FILETIME;
            use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

            fn to_timeval(ft: FILETIME) -> TimeVal {
                // FILETIME counts 100ns intervals.
                let ticks = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
                let micros = ticks / 10;
                TimeVal::new((micros / 1_000_000) as i64, (micros % 1_000_000) as i64)
            }

            let mut creation = FILETIME::default();
            let mut exit = FILETIME::default();
            let mut kernel = FILETIME::default();
            let mut user = FILETIME::default();
            let ok = unsafe {
                GetProcessTimes(
                    GetCurrentProcess(),
                    &mut creation,
                    &mut exit,
                    &mut kernel,
                    &mut user,
                )
            };
            if ok == 0 {
                return RUsageRaw::default();
            }
            RUsageRaw {
                ru_utime: to_timeval(user),
                ru_stime: to_timeval(kernel),
                ru_maxrss: 0,
                ru_nsignals: 0,
            }
        }

        pub fn getrusage(who: UsageWho) -> Result<Usage> {
            let raw = match who {
                UsageWho::RUSAGE_SELF | UsageWho::RUSAGE_THREAD => process_times(),
                // Children's CPU time is not tracked; the delta is clamped to zero by fish.
                UsageWho::RUSAGE_CHILDREN => RUsageRaw::default(),
            };
            Ok(Usage(raw))
        }
    }

    pub mod mman {
        use super::super::errno::Errno;
        use super::super::Result;
        use fish_common::os::AsHandle;
        use std::ffi::c_void;
        use std::num::NonZeroUsize;
        use std::ptr::NonNull;
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MapViewOfFile, PAGE_READONLY,
            PAGE_READWRITE, UnmapViewOfFile,
        };

        flagset!(ProtFlags {
            PROT_NONE = 0,
            PROT_READ = 1,
            PROT_WRITE = 2,
            PROT_EXEC = 4,
        });

        flagset!(MapFlags {
            MAP_SHARED = 1,
            MAP_PRIVATE = 2,
            MAP_ANON = 0x20,
            MAP_ANONYMOUS = 0x20,
            MAP_FIXED = 0x10,
            MAP_POPULATE = 0x8000,
        });

        fn page_protection(prot: ProtFlags) -> u32 {
            if prot.contains(ProtFlags::PROT_WRITE) {
                PAGE_READWRITE
            } else {
                PAGE_READONLY
            }
        }

        fn map_access(prot: ProtFlags) -> u32 {
            if prot.contains(ProtFlags::PROT_WRITE) {
                FILE_MAP_WRITE | FILE_MAP_READ
            } else {
                FILE_MAP_READ
            }
        }

        /// Map a region of a file.
        ///
        /// # Safety
        /// The caller must ensure the returned mapping is not used after `munmap`.
        pub unsafe fn mmap<F: AsHandle>(
            _addr: Option<NonZeroUsize>,
            length: NonZeroUsize,
            prot: ProtFlags,
            _flags: MapFlags,
            f: F,
            offset: i64,
        ) -> Result<NonNull<c_void>> {
            let handle = f.as_handle();
            if handle.is_null() {
                return Err(Errno::EBADF);
            }
            let len = length.get() as u64;
            let mapping = unsafe {
                CreateFileMappingW(
                    handle.cast(),
                    std::ptr::null(),
                    page_protection(prot),
                    (len >> 32) as u32,
                    len as u32,
                    std::ptr::null(),
                )
            };
            if mapping.is_null() {
                return Err(Errno::from(std::io::Error::last_os_error()));
            }
            let off = offset as u64;
            let ptr = unsafe {
                MapViewOfFile(
                    mapping,
                    map_access(prot),
                    (off >> 32) as u32,
                    off as u32,
                    length.get(),
                )
            };
            unsafe { CloseHandle(mapping) };
            NonNull::new(ptr.Value.cast()).ok_or_else(|| Errno::from(std::io::Error::last_os_error()))
        }

        /// Map anonymous memory.
        ///
        /// # Safety
        /// The caller must ensure the returned mapping is not used after `munmap`.
        pub unsafe fn mmap_anonymous(
            _addr: Option<NonZeroUsize>,
            length: NonZeroUsize,
            prot: ProtFlags,
            _flags: MapFlags,
        ) -> Result<NonNull<c_void>> {
            let len = length.get() as u64;
            let mapping = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    std::ptr::null(),
                    page_protection(prot),
                    (len >> 32) as u32,
                    len as u32,
                    std::ptr::null(),
                )
            };
            if mapping.is_null() {
                return Err(Errno::from(std::io::Error::last_os_error()));
            }
            let ptr = unsafe { MapViewOfFile(mapping, map_access(prot), 0, 0, length.get()) };
            unsafe { CloseHandle(mapping) };
            NonNull::new(ptr.Value.cast()).ok_or_else(|| Errno::from(std::io::Error::last_os_error()))
        }

        /// # Safety
        /// `addr` must have been returned by `mmap`.
        pub unsafe fn munmap(addr: NonNull<c_void>, _length: usize) -> Result<()> {
            let ok = unsafe {
                UnmapViewOfFile(windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: addr.as_ptr(),
                })
            };
            if ok == 0 {
                Err(Errno::last())
            } else {
                Ok(())
            }
        }
    }
}

pub mod fs {
    use super::errno::Errno;
    use super::Result;
    use std::fs::Metadata;
    use std::path::Path;

    pub trait MetadataExt {
        fn uid(&self) -> u32;
        fn gid(&self) -> u32;
        fn mode(&self) -> u32;
        fn ino(&self) -> u64;
        fn dev(&self) -> u64;
        fn nlink(&self) -> u64;
        fn rdev(&self) -> u64;
        fn size(&self) -> u64;
    }

    impl MetadataExt for Metadata {
        fn uid(&self) -> u32 {
            0
        }
        fn gid(&self) -> u32 {
            0
        }
        fn mode(&self) -> u32 {
            const S_IFDIR: u32 = 0o040000;
            const S_IFREG: u32 = 0o100000;
            let kind = if self.is_dir() { S_IFDIR } else { S_IFREG };
            let bits = if self.permissions().readonly() {
                0o444
            } else {
                0o666
            };
            kind | bits
        }
        fn ino(&self) -> u64 {
            // `Metadata` does not expose the file index on stable Rust. Identity-sensitive paths
            // in fish go through `wutil::FileId`, which opens the file by name and asks Win32.
            0
        }
        fn dev(&self) -> u64 {
            0
        }
        fn nlink(&self) -> u64 {
            1
        }
        fn rdev(&self) -> u64 {
            0
        }
        fn size(&self) -> u64 {
            Metadata::len(self)
        }
    }

    pub trait PermissionsExt {
        fn mode(&self) -> u32;
        fn set_mode(&mut self, mode: u32);
    }

    impl PermissionsExt for std::fs::Permissions {
        fn mode(&self) -> u32 {
            if self.readonly() { 0o444 } else { 0o666 }
        }
        fn set_mode(&mut self, mode: u32) {
            self.set_readonly(mode & 0o222 == 0);
        }
    }

    pub trait OpenOptionsExt {
        fn mode(&mut self, mode: u32) -> &mut Self;
    }

    impl OpenOptionsExt for std::fs::OpenOptions {
        fn mode(&mut self, _mode: u32) -> &mut Self {
            self
        }
    }

    pub trait DirBuilderExt {
        fn mode(&mut self, mode: u32) -> &mut Self;
    }

    impl DirBuilderExt for std::fs::DirBuilder {
        fn mode(&mut self, _mode: u32) -> &mut Self {
            self
        }
    }

    pub fn symlink(original: &Path, link: &Path) -> Result<()> {
        let is_dir = std::fs::metadata(original)
            .map(|m| m.is_dir())
            .unwrap_or(false);
        let result = if is_dir {
            std::os::windows::fs::symlink_dir(original, link)
        } else {
            std::os::windows::fs::symlink_file(original, link)
        };
        result.map_err(Errno::from)
    }

    pub fn symlink_file(original: &Path, link: &Path) -> Result<()> {
        std::os::windows::fs::symlink_file(original, link).map_err(Errno::from)
    }

    pub fn symlink_dir(original: &Path, link: &Path) -> Result<()> {
        std::os::windows::fs::symlink_dir(original, link).map_err(Errno::from)
    }

    pub fn fchown<F>(_fd: F, _uid: Option<u32>, _gid: Option<u32>) -> Result<()> {
        Err(Errno::EOPNOTSUPP)
    }
}

pub mod ffi {
    use std::ffi::OsStr;

    pub trait OsStrExt {
        fn as_bytes(&self) -> &[u8];
    }

    impl OsStrExt for OsStr {
        fn as_bytes(&self) -> &[u8] {
            self.as_encoded_bytes()
        }
    }
}

pub mod io {
    pub type RawFd = i32;
}

pub mod prelude {
    pub use super::fcntl::{FcntlArg, OFlag};
    pub use super::sys::wait::WaitStatus;
    pub use super::unistd::Pid;
    pub use fish_common::os::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
}

/// Top-level `isatty`, matching the convenience re-export in later nix versions.
pub fn isatty(fd: i32) -> bool {
    fish_common::os::is_tty(fd)
}

mod process {
    use super::errno::Errno;
    use super::sys::signal::Signal;
    use super::sys::wait::WaitStatus;
    use super::unistd::Pid;
    use super::Result;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
    };

    struct ChildEntry {
        handle: HANDLE,
        /// Whether this process was created with CREATE_NEW_PROCESS_GROUP, which decides how
        /// console control events can be targeted at it.
        new_group: bool,
    }

    // SAFETY: kernel handles are plain values; they are only used with thread-safe Win32 calls
    // while the table lock is held.
    unsafe impl Send for ChildEntry {}

    static CHILDREN: Mutex<Option<HashMap<i32, ChildEntry>>> = Mutex::new(None);

    fn with_children<R>(f: impl FnOnce(&mut HashMap<i32, ChildEntry>) -> R) -> R {
        let mut guard = CHILDREN.lock().unwrap();
        f(guard.get_or_insert_with(HashMap::new))
    }

    /// Remember a spawned process so that waitpid/kill can find it.
    pub fn register_child(pid: i32, handle: isize, new_group: bool) {
        let handle = handle as HANDLE;
        with_children(|children| {
            children.insert(pid, ChildEntry { handle, new_group });
        });
    }

    fn remove_child(pid: i32) {
        with_children(|children| {
            if let Some(entry) = children.remove(&pid) {
                unsafe { CloseHandle(entry.handle) };
            }
        });
    }

    pub fn child_handle(pid: i32) -> Option<isize> {
        with_children(|children| children.get(&pid).map(|c| c.handle as isize))
    }

    pub fn child_exists(pid: i32) -> bool {
        with_children(|children| children.contains_key(&pid))
    }

    fn handle_for(pid: i32) -> Option<HANDLE> {
        with_children(|children| children.get(&pid).map(|c| c.handle))
    }

    fn in_own_group(pid: i32) -> bool {
        with_children(|children| children.get(&pid).is_some_and(|c| c.new_group))
    }

    pub fn signal_process(pid: i32, sig: Signal) -> Result<()> {
        use windows_sys::Win32::System::Console::{
            CTRL_BREAK_EVENT, CTRL_C_EVENT, GenerateConsoleCtrlEvent,
        };
        if let Some(handle) = handle_for(pid) {
            return match sig {
                // Console control events can only be aimed at a process group, so only processes
                // created in their own group can receive them; others just get terminated.
                Signal::SIGINT | Signal::SIGQUIT if in_own_group(pid) => {
                    // Ctrl+C is delivered to the whole console by the console host, which includes
                    // the shell; Ctrl+Break can be addressed to a single group.
                    if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid as u32) } != 0 {
                        Ok(())
                    } else {
                        Err(Errno::last())
                    }
                }
                Signal::SIGCONT => Ok(()),
                Signal::SIGSTOP | Signal::SIGTSTP | Signal::SIGTTIN | Signal::SIGTTOU => {
                    // Windows has no way to stop a process for job control.
                    Err(Errno::EOPNOTSUPP)
                }
                _ => {
                    if unsafe { TerminateProcess(handle, 1) } != 0 {
                        Ok(())
                    } else {
                        Err(Errno::last())
                    }
                }
            };
        }
        // The process is not our child; open it for termination if possible.
        let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid as u32) };
        if handle.is_null() {
            return Err(Errno::ESRCH);
        }
        let ok = unsafe { TerminateProcess(handle, 1) };
        unsafe { CloseHandle(handle) };
        if ok != 0 {
            Ok(())
        } else {
            Err(Errno::last())
        }
    }

    pub fn signal_process_group(pgid: i32, sig: Signal) -> Result<()> {
        use windows_sys::Win32::System::Console::{
            CTRL_BREAK_EVENT, CTRL_C_EVENT, GenerateConsoleCtrlEvent,
        };
        let event = match sig {
            Signal::SIGINT => CTRL_C_EVENT,
            Signal::SIGQUIT => CTRL_BREAK_EVENT,
            _ => CTRL_BREAK_EVENT,
        };
        // Processes created with CREATE_NEW_PROCESS_GROUP use their pid as the group id.
        if unsafe { GenerateConsoleCtrlEvent(event, pgid as u32) } != 0 {
            Ok(())
        } else {
            Err(Errno::last())
        }
    }

    fn exit_code_of(handle: HANDLE) -> Option<i32> {
        let mut code: u32 = 0;
        let ok = unsafe { GetExitCodeProcess(handle, &mut code) };
        if ok != 0 && code != STILL_ACTIVE as u32 {
            Some(code as i32)
        } else {
            None
        }
    }

    /// `STATUS_CONTROL_C_EXIT`, the exit code Windows gives a console process terminated by
    /// Ctrl+C (or Ctrl+Break before the default handler is replaced).
    const STATUS_CONTROL_C_EXIT: u32 = 0xC000_013A;

    /// Report a finished child the way wait() does on Unix.
    ///
    /// Windows puts abnormal termination into the exit code as an NTSTATUS, while Unix reports it
    /// as death by signal. Fish relies on that split: exit codes are 8 bits. Ctrl+C is the case
    /// users see, so it becomes SIGINT; anything else keeps the low 8 bits, which is what Unix
    /// stores for an exit code in the first place.
    fn wait_status_from_exit_code(pid: Pid, code: i32) -> WaitStatus {
        if code as u32 == STATUS_CONTROL_C_EXIT {
            return WaitStatus::Signaled(pid, Signal::SIGINT, false);
        }
        WaitStatus::Exited(pid, code & 0xFF)
    }

    pub fn wait_for_child(pid: Pid, nohang: bool) -> Result<WaitStatus> {
        let handle = match handle_for(pid.as_raw()) {
            Some(h) => h,
            None => return Err(Errno::ECHILD),
        };

        if nohang {
            if let Some(code) = exit_code_of(handle) {
                remove_child(pid.as_raw());
                return Ok(wait_status_from_exit_code(pid, code));
            }
            return Ok(WaitStatus::StillAlive);
        }

        let ret = unsafe { WaitForSingleObject(handle, u32::MAX) };
        if ret != 0 {
            return Err(Errno::from(std::io::Error::last_os_error()));
        }
        let code = exit_code_of(handle).unwrap_or(0);
        remove_child(pid.as_raw());
        Ok(wait_status_from_exit_code(pid, code))
    }
}

pub use process::{child_handle, child_exists, register_child};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errno_roundtrip() {
        assert_eq!(Errno::from_raw(2), Errno::ENOENT);
        assert_eq!(Errno::ENOENT.as_raw(), 2);
        assert_eq!(Errno::from_raw(12345), Errno::UnknownErrno);
    }

    #[test]
    fn termios_defaults() {
        let t = termios::Termios::default();
        assert_eq!(
            t.control_chars[termios::SpecialCharacterIndices::VERASE as usize],
            8
        );
        assert_eq!(t.control_chars[termios::SpecialCharacterIndices::VINTR as usize], 3);
    }
}
