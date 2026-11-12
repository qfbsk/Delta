pub mod config_paths;
mod environment;
mod r#impl;
mod var;

pub use environment::*;
pub use r#impl::is_read_only;
pub use var::*;

use fish_widestring::ToCString;
use std::sync::{Mutex, atomic::AtomicUsize};

/// Limit `read` to 1 GiB (bytes, not wide chars) by default. This can be overridden with the
/// `fish_read_limit` variable.
pub const DEFAULT_READ_BYTE_LIMIT: usize = 1024 * 1024 * 1024;

/// The actual `read` limit in effect, defaulting to [`DEFAULT_READ_BYTE_LIMIT`] but overridable
/// with `$fish_read_limit`.
pub static READ_BYTE_LIMIT: AtomicUsize = AtomicUsize::new(DEFAULT_READ_BYTE_LIMIT);

static SETENV_LOCK: Mutex<()> = Mutex::new(());

/// Sets an environment variable after obtaining a lock, to try and improve the safety of
/// environment variables.
///
/// As values could contain non-unicode characters, they must first be converted from &wstr to a
/// `CString` with [`fish_widestring::wcs2zstring()`].
pub fn setenv_lock<S1: ToCString, S2: ToCString>(name: S1, value: S2, overwrite: bool) {
    let name_c = name.to_cstring();
    let value_c = value.to_cstring();
    let name_str = String::from_utf8_lossy(name_c.to_bytes());
    let value_str = String::from_utf8_lossy(value_c.to_bytes());
    let name = std::ffi::OsStr::new(name_str.as_ref());
    let value = std::ffi::OsStr::new(value_str.as_ref());
    let _lock = SETENV_LOCK.lock();
    if !overwrite && std::env::var_os(name).is_some() {
        return;
    }
    // Safety: callers hold SETENV_LOCK, which serializes access to the process environment.
    unsafe {
        std::env::set_var(name, value);
    }
    // The UCRT keeps its own copy of the environment, and calls like tzset() read that copy
    // rather than the process environment block.
    #[cfg(windows)]
    unsafe {
        libc::putenv_s(name_c.as_ptr(), value_c.as_ptr());
    }
}

/// Unsets an environment variable after obtaining a lock, to try and improve the safety of
/// environment variables.
pub fn unsetenv_lock<S: ToCString>(name: S) {
    let name_c = name.to_cstring();
    let name = std::ffi::OsStr::new(String::from_utf8_lossy(name_c.to_bytes()).as_ref()).to_owned();
    let _lock = SETENV_LOCK.lock();
    // Safety: callers hold SETENV_LOCK, which serializes access to the process environment.
    unsafe {
        std::env::remove_var(&name);
    }
    #[cfg(windows)]
    unsafe {
        libc::putenv_s(name_c.as_ptr(), c"".as_ptr());
    }
}
