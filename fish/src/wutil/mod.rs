pub mod dir_iter;
mod errors;
mod fileid;
mod hex_float;
#[macro_use]
pub mod printf;
pub mod wcstod;
pub mod wcstoi;

pub use errors::*;
pub use fileid::*;
pub use printf::{eprintf, fprintf, printf, sprintf};
pub use wcstoi::{
    fish_wcstoi, fish_wcstol, fish_wcstol_radix, fish_wcstoul, wcstoi, wcstoi_opts, wcstoi_partial,
};

use crate::{fds::BorrowedFdFile, flog, signal::SigChecker};
use fish_util::perror;
use fish_wcstringutil::join_strings;
use fish_widestring::{
    IntoCharIter, L, WExt as _, WString, bytes2wcstring, fish_reserved_codepoint, osstr2wcstring,
    str2bytes_callback, wcs2osstring, wstr,
};
use nix::unistd::{self, AccessFlags};
use fish_common::os::{AsRawFd, BorrowedFd, FromRawFd, RawFd};
use std::{
    fs::{self, canonicalize},
    io,
    path::PathBuf,
};

/// Convert a Windows path (as returned by the OS) into fish's internal representation: forward
/// slashes, without the verbatim `\\?\` prefix that `canonicalize` produces.
fn os_path_to_wstring(path: &std::path::Path) -> WString {
    let mut result = osstr2wcstring(path.as_os_str());
    #[cfg(windows)]
    {
        let verbatim_unc = L!("\\\\?\\UNC");
        let verbatim = L!("\\\\?\\");
        if result.starts_with(verbatim_unc) {
            result = WString::from(L!("\\\\")) + &result[verbatim_unc.char_count()..];
        } else if result.starts_with(verbatim) {
            result = result.slice_from(verbatim.char_count()).to_owned();
        }
        // On Unix a backslash is an ordinary filename character.
        result.replace(L!("\\"), L!("/"));
    }
    result
}

/// True when `c` separates path components.
///
/// Windows accepts both slashes in every file API, and the strings fish handles come from both
/// sides: `$PWD` carries backslashes, fish's own joins and user input usually use forward slashes.
/// Path logic therefore accepts either.
pub fn is_path_sep(c: char) -> bool {
    c == '/' || (cfg!(windows) && c == '\\')
}

/// Index of the first path separator in `s`.
pub fn find_path_sep(s: &wstr) -> Option<usize> {
    s.chars().position(is_path_sep)
}

/// Index of the last path separator in `s`.
pub fn rfind_path_sep(s: &wstr) -> Option<usize> {
    s.chars().rposition(is_path_sep)
}

/// Split `s` at either path separator, keeping empty components (a trailing separator yields a
/// final empty component, as POSIX path splitting does).
pub fn split_path_components(s: &wstr) -> Vec<&wstr> {
    let mut result = Vec::new();
    let mut start = 0;
    for (i, c) in s.chars().enumerate() {
        if is_path_sep(c) {
            result.push(s.slice_from(start).slice_to(i - start));
            start = i + 1;
        }
    }
    result.push(s.slice_from(start));
    result
}

/// True when the path is rooted, so the working directory must not be prepended.
///
/// A leading separator covers POSIX paths, Windows rooted paths (`\foo`), and UNC paths
/// (`\\server\share`). A drive designator (`C:\`, `C:/`, and drive-relative `C:foo`) is rooted
/// too: prepending `$PWD` to it can only produce an invalid path.
pub fn path_is_absolute(path: &wstr) -> bool {
    let n = path.char_count();
    if n == 0 {
        return false;
    }
    if is_path_sep(path.char_at(0)) {
        return true;
    }
    #[cfg(windows)]
    if path.char_at(0).is_ascii_alphabetic() && path.char_at(1) == ':' {
        return true;
    }
    false
}

/// True for the paths that `cd` resolves against the working directory itself: `.`, `..`, and
/// anything that starts with `./`, `../`, `.\` or `..\`.
pub fn path_is_dot_relative(path: &wstr) -> bool {
    if path == "." || path == ".." {
        return true;
    }
    if path.char_at(0) != '.' {
        return false;
    }
    let second = path.char_at(1);
    if is_path_sep(second) {
        return true;
    }
    second == '.' && path.char_count() > 2 && is_path_sep(path.char_at(2))
}

/// Length in chars of a path's root, including its trailing separator: `/` is 1, `C:\` is 3,
/// `\\server\share\` is the whole UNC prefix. Returns 0 for a relative path.
pub fn path_root_len(path: &wstr) -> usize {
    let n = path.char_count();
    if n == 0 {
        return 0;
    }
    let first = path.char_at(0);
    #[cfg(windows)]
    {
        if first.is_ascii_alphabetic() && path.char_at(1) == ':' {
            return if n >= 3 && is_path_sep(path.char_at(2)) { 3 } else { 2 };
        }
        if first == '\\' && n >= 2 && path.char_at(1) == '\\' {
            // Verbatim paths (`\\?\C:\`, `\\.\device\`) keep their whole prefix.
            if n >= 4 && (path.char_at(2) == '?' || path.char_at(2) == '.') && is_path_sep(path.char_at(3)) {
                let mut i = 4;
                if i + 1 < n && path.char_at(i).is_ascii_alphabetic() && path.char_at(i + 1) == ':' {
                    i += 2;
                }
                if i < n && is_path_sep(path.char_at(i)) {
                    i += 1;
                }
                return i;
            }
            // UNC: \\server\share\
            let mut i = 2;
            while i < n && !is_path_sep(path.char_at(i)) {
                i += 1;
            }
            if i < n {
                i += 1;
            }
            while i < n && !is_path_sep(path.char_at(i)) {
                i += 1;
            }
            if i < n {
                i += 1;
            }
            return i;
        }
    }
    if is_path_sep(first) {
        let mut i = 0;
        while i < n && is_path_sep(path.char_at(i)) {
            i += 1;
        }
        return i;
    }
    0
}

/// Wide character version of stat().
pub fn wstat(file_name: &wstr) -> io::Result<fs::Metadata> {
    let tmp = wcs2osstring(file_name);
    fs::metadata(tmp)
}

/// Wide character version of lstat().
pub fn lwstat(file_name: &wstr) -> io::Result<fs::Metadata> {
    let tmp = wcs2osstring(file_name);
    fs::symlink_metadata(tmp)
}

/// Cover over fstat().
pub fn fstat(fd: impl AsRawFd) -> io::Result<fs::Metadata> {
    let file = unsafe { BorrowedFdFile::from_raw_fd(fd.as_raw_fd()) };
    file.metadata()
}

/// Whether the fd is a pipe or a console. `GetFileInformationByHandle` accepts only disk files,
/// so callers that just need to know whether this is a regular file ask here first.
pub fn fd_is_stream(fd: RawFd) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_TYPE_CHAR, FILE_TYPE_PIPE, GetFileType,
        };
        let handle = fish_common::os::handle_from_fd(fd);
        if handle.is_null() {
            return false;
        }
        let ty = unsafe { GetFileType(handle) };
        ty == FILE_TYPE_PIPE || ty == FILE_TYPE_CHAR
    }
    #[cfg(not(windows))]
    {
        let _ = fd;
        false
    }
}

/// Wide character version of access().
pub fn waccess(file_name: &wstr, amode: AccessFlags) -> nix::Result<()> {
    let tmp = wcs2osstring(file_name);
    nix::unistd::access(tmp.as_os_str(), amode)
}

/// Wide character version of unlink().
pub fn wunlink(file_name: &wstr) -> io::Result<()> {
    let tmp = wcs2osstring(file_name);
    fs::remove_file(tmp)
}

pub fn perror_nix(s: &str, e: nix::errno::Errno) {
    eprintf!("%s: %s\n", s, e.desc());
}

pub fn perror_io(s: &str, e: &io::Error) {
    eprintf!("%s: %s\n", s, e);
}

/// POSIX file-type predicates that have no direct Windows equivalent.
///
/// Windows' filesystem namespace has no block devices, FIFOs or sockets; named pipes live
/// outside it. Keeping the predicates explicit lets the builtins stay platform-neutral.
pub trait FileTypeExt {
    fn is_block_device(&self) -> bool;
    fn is_char_device(&self) -> bool;
    fn is_fifo(&self) -> bool;
    fn is_socket(&self) -> bool;
}

impl FileTypeExt for fs::FileType {
    fn is_block_device(&self) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::FileTypeExt::is_block_device(self)
        }
        #[cfg(windows)]
        {
            false
        }
    }

    fn is_char_device(&self) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::FileTypeExt::is_char_device(self)
        }
        #[cfg(windows)]
        {
            false
        }
    }

    fn is_fifo(&self) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::FileTypeExt::is_fifo(self)
        }
        #[cfg(windows)]
        {
            false
        }
    }

    fn is_socket(&self) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::FileTypeExt::is_socket(self)
        }
        #[cfg(windows)]
        {
            false
        }
    }
}

/// The POSIX permission bits of a file.
///
/// Windows has no mode word; the read-only attribute is its only permission bit. Report rwx for
/// writable files and r-x for read-only ones, which is what POSIX tools expect to see.
pub fn permissions_mode(perms: &fs::Permissions) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        perms.mode()
    }
    #[cfg(windows)]
    {
        if perms.readonly() { 0o555 } else { 0o777 }
    }
}

/// The POSIX permission bits of a file's metadata.
pub fn metadata_mode(md: &fs::Metadata) -> u32 {
    permissions_mode(&md.permissions())
}

/// The owning user id, reported as 0 on Windows.
pub fn metadata_uid(md: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        md.uid()
    }
    #[cfg(windows)]
    {
        let _ = md;
        0
    }
}

/// The owning group id, reported as 0 on Windows.
pub fn metadata_gid(md: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        md.gid()
    }
    #[cfg(windows)]
    {
        let _ = md;
        0
    }
}

/// Whether two paths refer to the same file. Windows exposes identity only through handles, so
/// this opens both files and compares volume serial number plus file index.
pub fn paths_have_same_identity(path1: &wstr, path2: &wstr) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        match (wstat(path1), wstat(path2)) {
            (Ok(s1), Ok(s2)) => s1.ino() == s2.ino() && s1.dev() == s2.dev(),
            _ => false,
        }
    }
    #[cfg(windows)]
    {
        let id1 = fish_common::os::file_identity(&wcs2osstring(path1));
        let id2 = fish_common::os::file_identity(&wcs2osstring(path2));
        matches!((id1, id2), (Ok(a), Ok(b)) if a == b)
    }
}

/// Wide character version of getcwd().
pub fn wgetcwd() -> WString {
    match std::env::current_dir() {
        Ok(cwd) => os_path_to_wstring(&cwd),
        Err(e) => {
            flog!(error, "std::env::current_dir() failed with error:", e);
            WString::new()
        }
    }
}

/// Wide character version of readlink().
pub fn wreadlink(file_name: &wstr) -> Option<WString> {
    let _ = lwstat(file_name).ok()?;
    match fs::read_link(wcs2osstring(file_name)) {
        Ok(target) => Some(osstr2wcstring(target)),
        Err(e) => {
            perror_io("readlink", &e);
            None
        }
    }
}

/// Wide character realpath. The last path component does not need to be valid.
pub fn wrealpath(pathname: &wstr) -> io::Result<WString> {
    if pathname.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty path"));
    }

    // Strip trailing slashes, except for a drive root like "C:/" and the filesystem root.
    let mut path = pathname.to_owned();
    while path.char_count() > 1 && path.ends_with(L!("/")) {
        if path.char_count() == 3 && path.as_char_slice()[1] == ':' {
            break;
        }
        path = path.slice_to(path.char_count() - 1).to_owned();
    }

    let os_path = PathBuf::from(wcs2osstring(&path));
    let real_path = match canonicalize(&os_path) {
        Ok(result) => result,
        Err(_) => {
            // Check if everything up to the last path component is valid.
            let pathsep_idx = path.chars().rposition(|c| c == '/');

            if pathsep_idx == Some(0) {
                // Absolute path with a single component; it does not need conversion.
                os_path
            } else {
                let narrow_result = if let Some(pathsep_idx) = pathsep_idx {
                    // Only call realpath() on the portion up to the last component.
                    canonicalize(PathBuf::from(wcs2osstring(&path[..pathsep_idx])))?
                } else {
                    // If there is no "/", this is a file in $PWD, so give the realpath to that.
                    canonicalize(".")?
                };

                let mut real_path = narrow_result;
                let last_component = pathsep_idx.map_or(0, |idx| idx + 1);
                if last_component < path.char_count() {
                    real_path.push(wcs2osstring(&path.slice_from(last_component)));
                }
                real_path
            }
        }
    };

    Ok(os_path_to_wstring(&real_path))
}

/// Given an input path, "normalize" it:
/// 1. Collapse multiple /s into a single /, except maybe at the beginning.
/// 2. .. goes up a level.
/// 3. Remove /./ in the middle.
pub fn normalize_path(path: &wstr, allow_leading_double_slashes: bool) -> WString {
    // Keep the root verbatim: POSIX `/` (collapsed from runs of slashes, `//` kept when allowed),
    // a Windows drive (`C:\`) or a UNC/verbatim prefix. Everything after it is component-wise.
    let root_len = path_root_len(path);
    let mut root = WString::new();
    if root_len > 0 {
        let first = path.char_at(0);
        #[cfg(windows)]
        let verbatim = first == '\\' && root_len >= 2 && path.char_at(1) == '\\';
        #[cfg(not(windows))]
        let verbatim = false;
        if verbatim {
            root.push_utfstr(path.slice_to(root_len));
        } else if cfg!(windows) && path.char_at(1) == ':' {
            // Drive root: keep `C:` plus at most one separator.
            root.push_utfstr(path.slice_to(2));
            if root_len > 2 {
                root.push(path.char_at(2));
            }
        } else {
            // Runs of leading slashes collapse to one, or to two when `//` is allowed. On Windows
            // a leading `//` names a UNC share, so it always survives.
            let keep_two = if cfg!(windows) {
                root_len == 2
            } else {
                allow_leading_double_slashes && root_len == 2
            };
            root.push(if is_path_sep(first) { first } else { '/' });
            if keep_two {
                root.push(if is_path_sep(first) { first } else { '/' });
            }
        }
    }
    let sep = if root.is_empty() {
        '/'
    } else {
        let last = root.char_at(root.char_count() - 1);
        if is_path_sep(last) { last } else { '/' }
    };

    let rest = if root_len > 0 { path.slice_from(root_len) } else { path };
    let comps = split_path_components(rest);
    let mut new_comps = Vec::new();
    for comp in comps {
        if comp.is_empty() || comp == "." {
            continue;
        } else if comp != ".." {
            new_comps.push(comp);
        } else if !new_comps.is_empty() && new_comps.last().unwrap() != ".." {
            // '..' with a real path component, drop that path component.
            new_comps.pop();
        } else if root_len == 0 {
            // We underflowed the .. and are a relative (not absolute) path.
            new_comps.push(L!(".."));
        }
    }
    let mut result = root;
    for (i, comp) in new_comps.iter().enumerate() {
        if i > 0 {
            result.push(sep);
        }
        result.push_utfstr(comp);
    }
    // Ensure ./ normalizes to . and not empty.
    if result.is_empty() {
        result.push('.');
    }
    result
}

/// Given an input path `path` and a working directory `wd`, do a "normalizing join" in a way
/// appropriate for cd. That is, return effectively wd + path while resolving leading ../s from
/// path. The intent here is to allow 'cd' out of a directory which may no longer exist, without
/// allowing 'cd' into a directory that may not exist; see #5341.
pub fn path_normalize_for_cd(wd: &wstr, path: &wstr) -> WString {
    use std::collections::VecDeque;

    // Fast paths.
    if path.is_empty() {
        return wd.to_owned();
    } else if path_is_absolute(path) {
        return path.to_owned();
    }

    // The working directory is absolute and ends with a separator; tolerate a caller that left it
    // off (the root of a drive is simply `C:`).
    let wd_owned;
    let wd = if wd.is_empty() || is_path_sep(wd.char_at(wd.char_count() - 1)) {
        wd
    } else {
        wd_owned = {
            let mut w = wd.to_owned();
            w.push(if cfg!(windows) { '\\' } else { '/' });
            w
        };
        &wd_owned
    };

    if !path_is_dot_relative(path) {
        return wd.to_owned() + path;
    }

    // Keep the root (drive, UNC share, or `/`) as it is; only the components below it move.
    let root_len = path_root_len(wd);
    let sep = if root_len > 0 && is_path_sep(wd.char_at(root_len - 1)) {
        wd.char_at(root_len - 1)
    } else if cfg!(windows) {
        '\\'
    } else {
        '/'
    };
    let root = wd.slice_to(root_len);
    let mut wd_comps: VecDeque<&wstr> = split_path_components(wd.slice_from(root_len))
        .into_iter()
        // Remove empty segments from wd_comps, especially leading/trailing empties.
        .filter(|p| !p.is_empty())
        .collect();
    let path_comps = split_path_components(path);

    // Erase leading . and .. components from path_comps, popping from wd_comps as we go.
    let mut erased = 0;
    while erased < path_comps.len() {
        let comp = path_comps[erased];
        #[allow(clippy::if_same_then_else)]
        if comp.is_empty() || comp == "." {
            erased += 1;
        } else if comp == ".." && wd_comps.pop_back().is_some() {
            erased += 1;
        } else {
            break;
        }
    }

    // Append un-erased elements to wd_comps and join them under the root.
    let mut paths = wd_comps;
    paths.extend(path_comps[erased..].iter().copied());
    let mut result = WString::with_capacity(wd.char_count() + path.char_count());
    result.push_utfstr(root);
    for (i, p) in paths.iter().enumerate() {
        if i > 0 {
            result.push(sep);
        }
        result.push_utfstr(p);
    }
    if result.is_empty() {
        result.push(sep);
    }
    result
}

/// Wide character version of dirname().
pub fn wdirname(mut path: &wstr) -> &wstr {
    // Do not use system-provided dirname (#7837).
    // On Mac it's not thread safe, and will error for paths exceeding PATH_MAX.
    // This follows OpenGroup dirname recipe.

    // 1: Double-slash stays.
    if path == "//" {
        return path;
    }

    // 2: All slashes => return slash.
    if !path.is_empty() && path.chars().all(is_path_sep) {
        return path.slice_to(1);
    }

    // 3: Trim trailing slashes.
    while path.as_char_slice().last().is_some_and(|&c| is_path_sep(c)) {
        path = path.slice_to(path.char_count() - 1);
    }

    // 4: No slashes left => return period.
    let Some(last_slash) = rfind_path_sep(path) else {
        return L!(".");
    };

    // 5: Remove trailing non-slashes.
    path = path.slice_to(last_slash + 1);

    // 6: Skip as permitted.
    // 7: Remove trailing slashes again. On Windows a drive or UNC root must keep
    // its separator: `E:` is drive-relative, not the root of E:.
    if cfg!(windows)
        && path_root_len(path) == path.char_count()
        && path.as_char_slice().iter().any(|&c| !is_path_sep(c))
    {
        return path;
    }
    while path.as_char_slice().last().is_some_and(|&c| is_path_sep(c)) {
        path = path.slice_to(path.char_count() - 1);
    }

    // 8: Empty => return slash.
    if path.is_empty() {
        return L!("/");
    }
    path
}

/// Wide character version of basename().
pub fn wbasename(mut path: &wstr) -> &wstr {
    // This follows OpenGroup basename recipe.
    // 1: empty => allowed to return ".". This is what system impls do.
    if path.is_empty() {
        return L!(".");
    }

    // 2: Skip as permitted.
    // 3: All slashes => return slash.
    if !path.is_empty() && path.chars().all(is_path_sep) {
        return path.slice_to(1);
    }

    // 4: Remove trailing slashes.
    while path.as_char_slice().last().is_some_and(|&c| is_path_sep(c)) {
        path = path.slice_to(path.char_count() - 1);
    }

    // 5: Remove up to and including last slash.
    if let Some(last_slash) = rfind_path_sep(path) {
        path = path.slice_from(last_slash + 1);
    }
    path
}

/// Write a wide string to a file descriptor. This avoids doing any additional allocation.
/// Returns nothing when interrupted by ctrl-c or HUP.
pub fn unescape_bytes_and_write_to_fd(input: impl IntoCharIter, fd: RawFd) -> Option<usize> {
    // Accumulate data in a local buffer.
    let mut accum = [0u8; 512];
    let mut accumlen = 0;
    let accum_capacity = accum.len();

    // Helper to perform a write to 'fd', looping as necessary.
    // Return true on success, false on error.
    let mut total_written = 0;

    fn do_write(
        sigcheck: &mut SigChecker,
        fd: RawFd,
        total_written: &mut usize,
        mut buf: &[u8],
    ) -> bool {
        while !buf.is_empty() {
            let amt = match unistd::write(unsafe { BorrowedFd::borrow_raw(fd) }, buf) {
                Ok(amt) => amt,
                Err(err) => {
                    // Some of our builtins emit multiple screens worth of data sent to a pager (the primary
                    // example being the `history` builtin) and receiving SIGINT should be considered normal and
                    // non-exceptional (user request to abort via Ctrl-C), meaning we shouldn't print an error.
                    //
                    // We have two options here: we can either return false without setting errored_ to
                    // true (*this* write will be silently aborted but the onus is on the caller to check
                    // the return value and skip future calls to `append()`) or we can flag the entire
                    // output stream as errored, causing us to both return false and skip any future writes.
                    // We're currently going with the latter, especially seeing as no callers currently
                    // check the result of `append()` (since it was always a void function before).
                    match err {
                        nix::errno::Errno::EINTR => {
                            if !sigcheck.check() {
                                continue;
                            }
                        }
                        nix::errno::Errno::EPIPE => (),
                        _ => perror("write"),
                    }
                    return false;
                }
            };
            *total_written += amt;
            assert!(amt <= buf.len(), "Wrote more than requested");
            buf = &buf[amt..];
        }
        true
    }

    // Helper to flush the accumulation buffer.
    let flush_accum = |sigcheck: &mut SigChecker,
                       total_written: &mut usize,
                       accum: &[u8],
                       accumlen: &mut usize| {
        if !do_write(sigcheck, fd, total_written, &accum[..*accumlen]) {
            return false;
        }
        *accumlen = 0;
        true
    };

    let mut sigcheck = SigChecker::new_sighupintterm();
    let mut success = str2bytes_callback(input, |buff: &[u8]| {
        if buff.len() + accumlen > accum_capacity {
            // We have to flush.
            if !flush_accum(&mut sigcheck, &mut total_written, &accum, &mut accumlen) {
                return false;
            }
        }
        if buff.len() + accumlen <= accum_capacity {
            // Accumulate more.
            accum[accumlen..(accumlen + buff.len())].copy_from_slice(buff);
            accumlen += buff.len();
            true
        } else {
            // Too much data to even fit, just write it immediately.
            do_write(&mut sigcheck, fd, &mut total_written, buff)
        }
    });
    // Flush any remaining.
    if success {
        success = flush_accum(&mut sigcheck, &mut total_written, &accum, &mut accumlen);
    }
    if success { Some(total_written) } else { None }
}

const PUA1_START: char = '\u{E000}';
const PUA1_END: char = '\u{F900}';
// const PUA2_START: char = '\u{F0000}';
// const PUA2_END: char = '\u{FFFFE}';
// const PUA3_START: char = '\u{100000}';
// const PUA3_END: char = '\u{10FFFE}';

/// Return one if the code point is in a Unicode private use area.
pub(crate) fn fish_is_pua(c: char) -> bool {
    PUA1_START <= c && c < PUA1_END
}

/// We need this because there are too many implementations that don't return the proper answer for
/// some code points. See issue #3050.
pub fn fish_iswalnum(c: char) -> bool {
    !fish_reserved_codepoint(c) && !fish_is_pua(c) && c.is_alphanumeric()
}

/// Given that `cursor` is a pointer into `base`, return the offset in characters.
/// This emulates C pointer arithmetic:
///    `wstr_offset_in(cursor, base)` is equivalent to C++ `cursor - base`.
pub fn wstr_offset_in(cursor: &wstr, base: &wstr) -> usize {
    let cursor = cursor.as_slice();
    let base = base.as_slice();
    // cursor may be a zero-length slice at the end of base,
    // which base.as_ptr_range().contains(cursor.as_ptr()) will reject.
    let base_range = base.as_ptr_range();
    let curs_range = cursor.as_ptr_range();
    assert!(
        base_range.start <= curs_range.start && curs_range.end <= base_range.end,
        "cursor should be a subslice of base"
    );
    let offset = unsafe { cursor.as_ptr().offset_from(base.as_ptr()) };
    assert!(offset >= 0, "offset should be non-negative");
    offset as usize
}

#[cfg(test)]
mod tests {
    use super::{
        normalize_path, unescape_bytes_and_write_to_fd, wbasename, wdirname, wstr_offset_in,
    };
    use crate::{prelude::*, tests::prelude::*};
    use fish_common::os::{AsRawFd as _, File as FishFile};
    use fish_widestring::bytes2wcstring;
    use nix::fs::OpenOptionsExt as _;
    use rand::RngExt as _;
    use std::{
        fs::OpenOptions,
        io::{Read as _, Seek as _},
    };

    mod test_path_normalize_for_cd {
        use super::super::path_normalize_for_cd;
        use fish_widestring::L;

        #[test]
        fn relative_path() {
            let wd = L!("/home/user/");
            let path = L!("projects");
            assert_eq!(
                path_normalize_for_cd(wd, path),
                L!("/home/user/projects"),
                "Normalized path for ({wd}, {path})"
            );
        }

        #[test]
        fn absolute_path() {
            let wd = L!("/home/user/");
            let path = L!("/etc");
            assert_eq!(
                path_normalize_for_cd(wd, path),
                L!("/etc"),
                "Normalized path for ({wd}, {path})"
            );
        }

        #[test]
        fn parent_directory() {
            let wd = L!("/home/user/projects/");
            let path = L!("../docs");
            assert_eq!(
                path_normalize_for_cd(wd, path),
                L!("/home/user/docs"),
                "Normalized path for ({wd}, {path})"
            );
        }

        #[test]
        fn current_directory() {
            let wd = L!("/home/user/");
            let path = L!("./");
            assert_eq!(
                path_normalize_for_cd(wd, path),
                L!("/home/user"),
                "Normalized path for ({wd}, {path})"
            );
        }

        #[test]
        fn nested_parent_directory() {
            let wd = L!("/home/user/projects/");
            let path = L!("../../");
            assert_eq!(
                path_normalize_for_cd(wd, path),
                L!("/home"),
                "Normalized path for ({wd}, {path})"
            );
        }

        #[test]
        fn complex_path() {
            let wd = L!("/home/user/projects/");
            let path = L!("./../other/projects/./.././../docs");
            assert_eq!(
                path_normalize_for_cd(wd, path),
                L!("/home/user/other/projects/./.././../docs"),
                "Normalized path for ({wd}, {path})"
            );
        }

        #[test]
        fn root_directory() {
            let wd = L!("/");
            let path = L!("..");
            assert_eq!(
                path_normalize_for_cd(wd, path),
                L!("/.."),
                "Normalized path for ({wd}, {path})"
            );
        }

        #[test]
        fn up_to_root_directory() {
            let wd = L!("/foo/");
            let path = L!("..");
            assert_eq!(
                path_normalize_for_cd(wd, path),
                L!("/"),
                "Normalized path for ({wd}, {path})"
            );
        }

        #[test]
        fn empty_path() {
            let wd = L!("/home/user/");
            let path = L!("");
            assert_eq!(
                path_normalize_for_cd(wd, path),
                L!("/home/user/"),
                "Normalized path for ({wd}, {path})"
            );
        }

        #[test]
        fn trailing_slash() {
            let wd = L!("/home/user/projects/");
            let path = L!("docs/");
            assert_eq!(
                path_normalize_for_cd(wd, path),
                L!("/home/user/projects/docs/"),
                "Normalized path for ({wd}, {path})"
            );
        }
    }

    #[test]
    fn test_normalize_path() {
        fn norm_path(path: &wstr) -> WString {
            normalize_path(path, true)
        }
        assert_eq!(norm_path(L!("")), ".");
        assert_eq!(norm_path(L!("..")), "..");
        assert_eq!(norm_path(L!("./")), ".");
        assert_eq!(norm_path(L!("./.")), ".");
        assert_eq!(norm_path(L!("/")), "/");
        assert_eq!(norm_path(L!("//")), "//");
        assert_eq!(norm_path(L!("///")), "/");
        assert_eq!(norm_path(L!("////")), "/");
        assert_eq!(norm_path(L!("/.///")), "/");
        assert_eq!(norm_path(L!(".//")), ".");
        assert_eq!(norm_path(L!("/.//../")), "/");
        assert_eq!(norm_path(L!("////abc")), "/abc");
        assert_eq!(norm_path(L!("/abc")), "/abc");
        assert_eq!(norm_path(L!("/abc/")), "/abc");
        assert_eq!(norm_path(L!("/abc/..def/")), "/abc/..def");
        assert_eq!(norm_path(L!("//abc/../def/")), "//def");
        assert_eq!(norm_path(L!("abc/../abc/../abc/../abc")), "abc");
        assert_eq!(norm_path(L!("../../")), "../..");
        assert_eq!(norm_path(L!("foo/./bar")), "foo/bar");
        assert_eq!(norm_path(L!("foo/../")), ".");
        assert_eq!(norm_path(L!("foo/../foo")), "foo");
        assert_eq!(norm_path(L!("foo/../foo/")), "foo");
        assert_eq!(norm_path(L!("foo/././bar/.././baz")), "foo/baz");
    }

    #[test]
    fn test_wdirname_wbasename() {
        // path, dir, base
        struct Test(&'static wstr, &'static wstr, &'static wstr);
        let testcases: &[Test] = &[
            Test(L!(""), L!("."), L!(".")),
            Test(L!("foo//"), L!("."), L!("foo")),
            Test(L!("foo//////"), L!("."), L!("foo")),
            Test(L!("/////foo"), L!("/"), L!("foo")),
            Test(L!("//foo/////bar"), L!("//foo"), L!("bar")),
            Test(L!("foo/////bar"), L!("foo"), L!("bar")),
            // Examples given in XPG4.2.
            Test(L!("/usr/lib"), L!("/usr"), L!("lib")),
            Test(L!("usr"), L!("."), L!("usr")),
            Test(L!("/"), L!("/"), L!("/")),
            Test(L!("."), L!("."), L!(".")),
            Test(L!(".."), L!("."), L!("..")),
        ];

        for tc in testcases {
            let Test(path, tc_dir, tc_base) = *tc;
            let dir = wdirname(path);
            assert_eq!(
                dir, tc_dir,
                "\npath: {:?}, dir: {:?}, tc.dir: {:?}",
                path, dir, tc_dir
            );

            let base = wbasename(path);
            assert_eq!(
                base, tc_base,
                "\npath: {:?}, base: {:?}, tc.base: {:?}",
                path, base, tc_base
            );
        }

        // Ensure strings which greatly exceed PATH_MAX still work (#7837).
        const PATH_MAX: usize = 4096;
        let mut longpath = WString::new();
        longpath.reserve(PATH_MAX * 2 + 10);
        while longpath.char_count() <= PATH_MAX * 2 {
            longpath.push_str("/overlong");
        }
        let last_slash = longpath.chars().rposition(|c| c == '/').unwrap();
        let longpath_dir = &longpath[..last_slash];
        assert_eq!(wdirname(&longpath), longpath_dir);
        assert_eq!(wbasename(&longpath), L!("overlong"));
    }

    #[test]
    #[serial]
    fn test_wwrite_to_fd() {
        test_init();
        let temp_file = fish_tempfile::new_file().unwrap();
        let mut rng = rand::rng();
        let sizes = [1, 2, 3, 5, 13, 23, 64, 128, 255, 4096, 4096 * 2];
        for &size in &sizes {
            // fish's descriptor-backed file on Windows; the identity conversion on Unix.
            let mut file = FishFile::from(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o666)
                    .open(temp_file.path())
                    .unwrap(),
            );
            let mut input = Vec::new();
            for _i in 0..size {
                input.push(rng.random());
            }

            let amt =
                unescape_bytes_and_write_to_fd(&bytes2wcstring(&input), file.as_raw_fd()).unwrap();
            assert_eq!(amt, input.len());

            file.seek(std::io::SeekFrom::Start(0)).unwrap();

            let mut contents = vec![];
            file.read_to_end(&mut contents).unwrap();
            assert_eq!(&contents, &input);
        }
    }

    #[test]
    fn test_wstr_offset_in() {
        use fish_widestring::L;
        let base = L!("hello world");
        assert_eq!(wstr_offset_in(&base[6..], base), 6);
        assert_eq!(wstr_offset_in(&base[0..], base), 0);
        assert_eq!(wstr_offset_in(&base[6..], &base[6..]), 0);
        assert_eq!(wstr_offset_in(&base[base.len()..], base), base.len());
    }
}
