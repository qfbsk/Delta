//! Directory utilities. This library contains functions for locating configuration directories,
//! for testing if a command with a given name can be found in the PATH, and various other
//! path-related issues.

use crate::env::{EnvMode, EnvSetMode, EnvStack, Environment, FALLBACK_PATH};
use crate::expand::expand_tilde;
use crate::flog::{flog, flogf};
use crate::prelude::*;
pub use crate::wutil::{path_is_absolute, path_is_dot_relative};
use crate::wutil::{
    is_path_sep, normalize_path, path_normalize_for_cd, path_root_len, waccess, wdirname, wstat,
};
use cfg_if::cfg_if;
use fish_widestring::{HOME_DIRECTORY, str2wcstring, wcs2osstring, wcs2zstring};
use nix::errno::Errno;
use nix::unistd::{AccessFlags, access};
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::LazyLock;

pub struct ValidatedPath<'a> {
    pub path: &'a wstr,
    pub ok: bool,
}

/// Returns the user configuration directory for fish. If the directory or one of its parents
/// doesn't exist, they are first created.
pub fn path_get_config() -> ValidatedPath<'static> {
    CONFIG_DIRECTORY.validated_path()
}

/// Returns the user data directory for fish. If the directory or one of its parents doesn't exist,
/// they are first created.
///
/// Volatile files presumed to be local to the machine, such as the fish_history will be stored in this directory.
pub fn path_get_data() -> ValidatedPath<'static> {
    DATA_DIRECTORY.validated_path()
}

/// Returns the user cache directory for fish. If the directory or one of its parents doesn't exist,
/// they are first created.
///
/// Volatile files presumed to be local to the machine such as all the
/// generated_completions, will be stored in this directory.
pub fn path_get_cache() -> ValidatedPath<'static> {
    CACHE_DIRECTORY.validated_path()
}

#[derive(Clone, Copy, PartialEq)]
pub enum DirRemoteness {
    /// directory status is unknown
    Unknown,
    /// directory is known local
    Local,
    /// directory is known remote
    Remote,
}

/// Return the remoteness of the fish data directory.
/// This will be remote for filesystems like NFS, SMB, etc.
pub fn path_get_data_remoteness() -> DirRemoteness {
    DATA_DIRECTORY.remoteness
}

/// Emit any errors if config directories are missing.
/// Use the given environment stack to ensure this only occurs once.
pub fn path_emit_config_directory_messages(vars: &EnvStack) {
    for (base_directory, which_dir, custom_error_msg, xdg_var) in [
        (
            &*DATA_DIRECTORY,
            L!("data"),
            wgettext!("can not save history"),
            L!("XDG_DATA_HOME"),
        ),
        (
            &*CONFIG_DIRECTORY,
            L!("config"),
            wgettext!("can not save universal variables or functions"),
            L!("XDG_CONFIG_HOME"),
        ),
        (
            &*CACHE_DIRECTORY,
            L!("cache"),
            wgettext!("can not save caching data"),
            L!("XDG_CACHE_HOME"),
        ),
    ] {
        if let Some(error) = &base_directory.err {
            maybe_issue_path_warning(
                which_dir,
                custom_error_msg,
                base_directory.used_xdg,
                xdg_var,
                &base_directory.path,
                error,
                vars,
            );
        }
        if base_directory.remoteness == DirRemoteness::Remote {
            flog!(path, which_dir, "path appears to be on a network volume");
        }
    }
}

/// We separate this from path_create() for two reasons. First it's only caused if there is a
/// problem, and thus is not central to the behavior of that function. Second, we only want to issue
/// the message once. If the current shell starts a new fish shell (e.g., by running `fish -c` from
/// a function) we don't want that subshell to issue the same warnings.
fn maybe_issue_path_warning(
    which_dir: &wstr,
    custom_error_msg: &wstr,
    using_xdg: bool,
    xdg_var: &wstr,
    path: &wstr,
    error: &std::io::Error,
    vars: &EnvStack,
) {
    let warning_var_name = L!("_FISH_WARNED_").to_owned() + which_dir;
    if vars
        .getf(&warning_var_name, EnvMode::GLOBAL_EXPORTED)
        .is_some()
    {
        return;
    }
    vars.set_one(
        &warning_var_name,
        EnvSetMode::new_at_early_startup(EnvMode::GLOBAL_EXPORTED),
        L!("1").to_owned(),
    );

    flog!(error, custom_error_msg);
    if path.is_empty() {
        flog!(
            warning_path,
            wgettext_fmt!("Unable to locate the %s directory.", which_dir)
        );
        flog!(
            warning_path,
            wgettext_fmt!(
                "Please set the %s or HOME environment variable before starting fish.",
                xdg_var
            )
        );
    } else {
        let env_var = if using_xdg { xdg_var } else { L!("HOME") };
        flog!(
            warning_path,
            wgettext_fmt!(
                "Unable to locate %s directory derived from $%s: '%s'.",
                which_dir,
                env_var,
                path
            )
        );
        flog!(
            warning_path,
            wgettext_fmt!("The error was '%s'.", error.to_string())
        );
        flog!(
            warning_path,
            wgettext_fmt!(
                "Please set $%s to a directory where you have write access.",
                env_var
            )
        );
    }
    eprintf!("\n");
}

/// Finds the path of an executable named `cmd`, by looking in $PATH taken from `vars`.
/// Returns the path if found, none if not.
pub fn path_get_path(cmd: &wstr, vars: &dyn Environment) -> Option<WString> {
    let result = path_try_get_path(cmd, vars);
    if result.err.is_some() {
        None
    } else {
        Some(result.path)
    }
}

/// Finds the path of an executable named `cmd`, by looking in $PATH taken from `vars`.
/// On success, err will be 0 and the path is returned.
/// On failure, we return the "best path" with err set appropriately.
/// For example, if we find a non-executable file, we will return its path and EACCESS.
/// If no candidate path is found, path will be empty and err will be set to ENOENT.
/// Possible err values are taken from access().
pub struct GetPathResult {
    pub err: Option<Errno>,
    pub path: WString,
}
impl GetPathResult {
    fn new(err: Option<Errno>, path: WString) -> Self {
        Self { err, path }
    }
}

pub fn path_try_get_path(cmd: &wstr, vars: &dyn Environment) -> GetPathResult {
    if let Some(path) = vars.get(L!("PATH")) {
        path_get_path_core(cmd, path.as_list())
    } else {
        path_get_path_core(cmd, &FALLBACK_PATH)
    }
}

fn path_check_executable(path: &wstr) -> Result<(), std::io::Error> {
    if waccess(path, AccessFlags::X_OK).is_err() {
        return Err(std::io::Error::last_os_error());
    }

    let buff = wstat(path)?;

    if buff.file_type().is_file() {
        Ok(())
    } else {
        Err(ErrorKind::PermissionDenied.into())
    }
}

/// Return all the paths that match the given command.
pub fn path_get_paths(cmd: &wstr, vars: &dyn Environment) -> Vec<WString> {
    flogf!(path, "path_get_paths('%s')", cmd);
    let mut paths = vec![];

    // If the command has a slash, it must be an absolute or relative path and thus we don't bother
    // looking for matching commands in the PATH var.
    if command_has_path_component(cmd) && path_check_executable(cmd).is_ok() {
        paths.push(cmd.to_owned());
        return paths;
    }

    let Some(path_var) = vars.get(L!("PATH")) else {
        return paths;
    };
    for path in path_var.as_list() {
        if path.is_empty() {
            continue;
        }
        let mut proposed = path.clone();
        append_path_component(&mut proposed, cmd);
        for candidate in command_candidate_names(&proposed) {
            if path_check_executable(&candidate).is_ok() {
                paths.push(candidate);
            }
        }
    }

    paths
}

/// True if `cmd` denotes a path instead of a bare command name.
#[cfg(not(windows))]
pub fn command_has_path_component(cmd: &wstr) -> bool {
    cmd.contains('/')
}
#[cfg(windows)]
pub fn command_has_path_component(cmd: &wstr) -> bool {
    cmd.contains('/') || cmd.contains('\\')
}

/// The file names to try for a command found in a PATH directory.
/// Windows resolves bare names through PATHEXT, so `cat` may live at `cat.exe`.
#[cfg(not(windows))]
fn command_candidate_names(proposed: &wstr) -> Vec<WString> {
    vec![proposed.to_owned()]
}
#[cfg(windows)]
fn command_candidate_names(proposed: &wstr) -> Vec<WString> {
    let mut names = vec![proposed.to_owned()];
    let pathext =
        std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned());
    for ext in pathext.split(';').filter(|ext| !ext.is_empty()) {
        let mut name = proposed.to_owned();
        name.push_utfstr(&str2wcstring(ext));
        names.push(name);
    }
    names
}

fn path_get_path_core<S: AsRef<wstr>>(cmd: &wstr, pathsv: &[S]) -> GetPathResult {
    let noent_res = GetPathResult::new(Some(Errno::ENOENT), WString::new());
    // Test if the given path can be executed.
    // Return 0 on success, an errno value on failure.
    let test_path = |path: &wstr| -> Result<(), Errno> {
        let narrow = wcs2zstring(path);
        access(narrow.as_c_str(), AccessFlags::X_OK)?;
        let Ok(md) = std::fs::metadata(wcs2osstring(path)) else {
            return Err(Errno::last());
        };
        if md.is_file() {
            Ok(())
        } else {
            Err(Errno::EACCES)
        }
    };

    if cmd.is_empty() {
        return noent_res;
    }

    // Commands cannot contain NUL byte.
    if cmd.contains('\0') {
        return noent_res;
    }

    // If the command has a slash, it must be an absolute or relative path and thus we don't bother
    // looking for a matching command.
    if command_has_path_component(cmd) {
        return GetPathResult::new(test_path(cmd).err(), cmd.to_owned());
    }

    let mut best = noent_res;
    for next_path in pathsv {
        let next_path: &wstr = next_path.as_ref();
        if next_path.is_empty() {
            continue;
        }
        let mut proposed_path = next_path.to_owned();
        append_path_component(&mut proposed_path, cmd);
        for candidate in command_candidate_names(&proposed_path) {
            match test_path(&candidate) {
                Ok(()) => {
                    // We found one.
                    return GetPathResult::new(None, candidate);
                }
                Err(err) => {
                    if err != Errno::ENOENT && best.err == Some(Errno::ENOENT) {
                        // Keep the first *interesting* error and path around.
                        // ENOENT isn't interesting because not having a file is the normal case.
                        // Ignore if the parent directory is already inaccessible.
                        if waccess(wdirname(&candidate), AccessFlags::X_OK).is_ok() {
                            best = GetPathResult::new(Some(err), candidate);
                        }
                    }
                }
            }
        }
    }
    best
}

/// Returns the full path of the specified directory, using the CDPATH variable as a list of base
/// directories for relative paths.
///
/// \param dir The name of the directory.
/// \param wd The working directory. The working directory must end with a slash.
/// \param vars The environment variables to use (for the CDPATH variable)
/// Return the command, or none if no path resolves to a directory.
pub fn path_get_cdpath(dir: &wstr, wd: &wstr, vars: &dyn Environment) -> Option<WString> {
    if dir.is_empty() {
        return None;
    }
    assert_eq!(wd.chars().next_back(), Some('/'));
    let paths = path_apply_cdpath(dir, wd, vars);

    for a_dir in paths {
        if let Ok(md) = wstat(&a_dir) {
            if md.is_dir() {
                return Some(a_dir);
            }
        }
    }

    None
}

/// Returns the given directory with all CDPATH components applied.
pub fn path_apply_cdpath(dir: &wstr, wd: &wstr, env_vars: &dyn Environment) -> Vec<WString> {
    let mut paths = vec![];
    if path_is_absolute(dir) {
        // Absolute path.
        paths.push(dir.to_owned());
    } else if path_is_dot_relative(dir) {
        // Path is relative to the working directory.
        paths.push(path_normalize_for_cd(wd, dir));
    } else {
        // Respect CDPATH.
        let mut cdpathsv = vec![];
        if let Some(cdpaths) = env_vars.get(L!("CDPATH")) {
            cdpathsv = cdpaths.as_list().to_vec();
        }
        // Always append $PWD
        cdpathsv.push(L!(".").to_owned());
        for path in cdpathsv {
            let mut abspath = WString::new();
            // We want to return an absolute path (see issue 6220)
            if !path_is_absolute(&path) && path.char_at(0) != '~' {
                abspath = wd.to_owned();

                // Do not add a second slash if `wd` already ends with one
                // (typically, when it's the root directory).
                // This could result in unwanted paths (e.g. `//<path>`, which
                // on Windows, is a remote directory).
                if !abspath
                    .as_char_slice()
                    .last()
                    .is_some_and(|&c| is_path_sep(c))
                {
                    abspath.push('/');
                }
            }
            abspath.push_utfstr(&path);

            expand_tilde(&mut abspath, env_vars);
            if abspath.is_empty() {
                continue;
            }
            abspath = normalize_path(&abspath, true);

            let mut whole_path = abspath;
            append_path_component(&mut whole_path, dir);
            paths.push(whole_path);
        }
    }
    paths
}

/// Returns the path resolved as an implicit cd command, or none() if none. This requires it to
/// start with one of the allowed prefixes (., .., ~) and resolve to a directory.
pub fn path_as_implicit_cd(path: &wstr, wd: &wstr, vars: &dyn Environment) -> Option<WString> {
    let mut exp_path = path.to_owned();
    expand_tilde(&mut exp_path, vars);
    if path_is_absolute(&exp_path)
        || path_is_dot_relative(&exp_path)
        || exp_path.ends_with('/')
        || (cfg!(windows) && exp_path.ends_with('\\'))
        || exp_path == ".."
    {
        // These paths can be implicit cd, so see if you cd to the path. Note that a single period
        // cannot (that's used for sourcing files anyways).
        return path_get_cdpath(&exp_path, wd, vars);
    }
    None
}

/// Remove double slashes and trailing slashes from a path, e.g. transform foo//bar/ into foo/bar.
/// The string is modified in-place.
pub fn path_make_canonical(path: &mut WString) {
    // A drive root (`E:\`) and a leading UNC prefix (`\\server\share`) are part of the path's
    // identity: `E:` alone names a drive-relative directory, and UNC carries a meaningful double
    // separator. Keep either one exactly as written.
    let root_len = if cfg!(windows) && !path.starts_with(L!("/")) {
        path_root_len(path)
    } else {
        0
    };
    let chars: &mut [char] = path.as_char_slice_mut();

    // Turn runs of slashes into a single slash.
    let mut written = root_len;
    let mut prev_was_slash = false;
    for read in root_len..chars.len() {
        let c = chars[read];
        let is_slash = c == '/';
        if prev_was_slash && is_slash {
            continue;
        }
        // This is either the first slash in a run, or not a slash at all.
        chars[written] = c;
        written += 1;
        prev_was_slash = is_slash;
    }
    if written > root_len.max(1) {
        path.truncate(written - usize::from(prev_was_slash));
    }
}

/// Check if two paths are equivalent, which means to ignore runs of multiple slashes (or trailing
/// slashes).
pub fn paths_are_equivalent(p1: &wstr, p2: &wstr) -> bool {
    let p1 = p1.as_char_slice();
    let p2 = p2.as_char_slice();

    if p1 == p2 {
        return true;
    }

    // Ignore trailing slashes after the first character.
    let mut len1 = p1.len();
    let mut len2 = p2.len();
    while len1 > 1 && is_path_sep(p1[len1 - 1]) {
        len1 -= 1;
    }
    while len2 > 1 && is_path_sep(p2[len2 - 1]) {
        len2 -= 1;
    }

    // Start walking
    let mut idx1 = 0;
    let mut idx2 = 0;
    while idx1 < len1 && idx2 < len2 {
        let c1 = p1[idx1];
        let c2 = p2[idx2];

        // If the characters are different, the strings are not equivalent. On Windows the two
        // separators are interchangeable.
        if c1 != c2 && !(is_path_sep(c1) && is_path_sep(c2)) {
            break;
        }

        idx1 += 1;
        idx2 += 1;

        // If the character was a slash, walk forwards until we hit the end of the string, or a
        // non-slash. Note the first condition is invariant within the loop.
        while is_path_sep(c1) && p1.get(idx1).is_some_and(|&c| is_path_sep(c)) {
            idx1 += 1;
        }
        while is_path_sep(c2) && p2.get(idx2).is_some_and(|&c| is_path_sep(c)) {
            idx2 += 1;
        }
    }

    // We matched if we consumed all of the characters in both strings.
    idx1 == len1 && idx2 == len2
}

pub fn path_is_valid(path: &wstr, working_directory: &wstr) -> bool {
    // Some special paths are always valid.
    if path.is_empty() {
        false
    } else if path == "." || path == "./" || (cfg!(windows) && path == ".\\") {
        true
    } else if path == ".." || path == "../" || (cfg!(windows) && path == "..\\") {
        !working_directory.is_empty()
            && !(path_is_absolute(working_directory)
                && path_root_len(working_directory) == working_directory.char_count())
    } else if !path_is_absolute(path) {
        // Prepend the working directory. Note that we know path is not empty here.
        let mut tmp = working_directory.to_owned();
        tmp.push_utfstr(path);
        waccess(&tmp, AccessFlags::F_OK).is_ok()
    } else {
        // Simple check.
        waccess(path, AccessFlags::F_OK).is_ok()
    }
}

/// Returns whether the two paths refer to the same file.
pub fn paths_are_same_file(path1: &wstr, path2: &wstr) -> bool {
    if paths_are_equivalent(path1, path2) {
        return true;
    }

    crate::wutil::paths_have_same_identity(path1, path2)
}

/// If the given path looks like it's relative to the working directory, then prepend that working
/// directory. This operates on unescaped paths only (so a ~ means a literal ~).
pub fn path_apply_working_directory(path: &wstr, working_directory: &wstr) -> WString {
    if path.is_empty() || working_directory.is_empty() {
        return path.to_owned();
    }

    // We're going to make sure that if we want to prepend the wd, that the string has no leading
    // "/".
    let prepend_wd = !path_is_absolute(path) && path.char_at(0) != HOME_DIRECTORY;

    if !prepend_wd {
        // No need to prepend the wd, so just return the path we were given.
        return path.to_owned();
    }

    // Remove up to one "./".
    let mut path_component = path.to_owned();
    if path_component.starts_with("./") {
        path_component.replace_range(0..2, L!(""));
    }

    // Removing leading /s.
    while path_component.starts_with("/") {
        path_component.replace_range(0..1, L!(""));
    }

    // Construct and return a new path.
    let mut new_path = working_directory.to_owned();
    append_path_component(&mut new_path, &path_component);
    new_path
}

/// The following type wraps up a user's "base" directories, corresponding (conceptually if not
/// actually) to XDG spec.
struct BaseDirectory {
    /// the path where we attempted to create the directory.
    path: WString,
    /// whether we actually created the directory if it didn't exist
    created: bool,
    /// whether the dir is remote
    remoteness: DirRemoteness,
    /// the error code if creating the directory failed, or 0 on success.
    err: Option<std::io::Error>,
    /// whether an XDG variable was used in resolving the directory.
    used_xdg: bool,
}

impl BaseDirectory {
    fn validated_path(&self) -> ValidatedPath<'_> {
        ValidatedPath {
            path: &self.path,
            ok: self.err.is_none(),
        }
    }
}

/// Attempt to get a base directory, creating it if necessary. If a variable named `xdg_var` is
/// set, use that directory; otherwise use the path `non_xdg_homepath` rooted in $HOME. Return the
/// result; see the base_directory_t fields.
#[cfg_attr(test, allow(unused_variables), allow(unreachable_code))]
fn make_base_directory(xdg_var: &wstr, non_xdg_homepath: &wstr) -> BaseDirectory {
    #[cfg(test)]
    // If running under `cargo test`, contain ourselves to the build directory and do not try to use
    // the actual $HOME or $XDG_XXX directories. This prevents the tests from failing and/or stops
    // the tests polluting the user's actual $HOME if a sandbox environment has not been set up.
    {
        use crate::common::BUILD_DIR;
        use fish_widestring::osstr2wcstring;
        use std::path::PathBuf;

        let mut build_dir = PathBuf::from(BUILD_DIR);
        build_dir.push("fish-test-home");

        let err = std::fs::create_dir_all(&build_dir).err();

        return BaseDirectory {
            path: osstr2wcstring(build_dir),
            created: false,
            remoteness: DirRemoteness::Unknown,
            used_xdg: false,
            err,
        };
    }

    // The vars we fetch must be exported. Allowing them to be universal doesn't make sense and
    // allowing that creates a lock inversion that deadlocks the shell since we're called before
    // uvars are available.
    let vars = EnvStack::globals();

    let mut path = WString::new();
    let used_xdg;
    if let Some(xdg_dir) = vars.getf_unless_empty(xdg_var, EnvMode::GLOBAL_EXPORTED) {
        path = xdg_dir.as_string() + L!("/fish");
        used_xdg = true;
    } else {
        if let Some(home) = vars.getf_unless_empty(L!("HOME"), EnvMode::GLOBAL_EXPORTED) {
            path = home.as_string() + non_xdg_homepath;
        }
        used_xdg = false;
    }

    enum Success {
        AlreadyExists,
        CreatedDir,
    }
    let result = {
        let path = PathBuf::from(wcs2osstring(&path));
        use std::io::ErrorKind::{AlreadyExists, NotFound};
        let mut recursive = false;
        loop {
            break match create_dir(&path, 0o700, recursive) {
                Ok(()) => Ok(Success::CreatedDir),
                Err(e) if e.kind() == AlreadyExists && path.is_dir() => Ok(Success::AlreadyExists),
                Err(e) if e.kind() == NotFound => {
                    recursive = true;
                    continue;
                }
                Err(e) => Err(e),
            };
        }
    };
    let remoteness = if result.is_err() {
        DirRemoteness::Unknown
    } else {
        path_remoteness(&path)
    };

    BaseDirectory {
        path,
        created: matches!(result, Ok(Success::CreatedDir)),
        remoteness,
        err: result.err(),
        used_xdg,
    }
}

// Like std::fs::create_dir_all, but new directories are created using the given mode (e.g. 0o700).
fn create_dir<P: AsRef<std::path::Path>>(
    path: P,
    mode: u32,
    recursive: bool,
) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .recursive(recursive)
            .mode(mode)
            .create(path.as_ref())
    }
    #[cfg(windows)]
    {
        // Windows directory permissions come from the parent's inheritable ACL; the requested
        // mode has no representation.
        let _ = mode;
        std::fs::DirBuilder::new()
            .recursive(recursive)
            .create(path.as_ref())
    }
}

/// Return whether the given path is on a remote filesystem.
pub fn path_remoteness(path: &wstr) -> DirRemoteness {
    cfg_if! {
        // illumos doesn't have statfs and MNT_LOCAL doesn't work for statvfs
        // we _could_ use statvfs to match against known filesystems but that is fragile
        if #[cfg(target_os = "illumos")] {
            DirRemoteness::Unknown
        } else {
            let narrow = wcs2zstring(path);
            cfg_if! {
                if #[cfg(windows)] {
                    use std::os::windows::ffi::OsStrExt as _;
                    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumePathNameW};
                    use windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOTE;
                    let wide: Vec<u16> = wcs2osstring(path)
                        .encode_wide()
                        .chain(std::iter::once(0))
                        .collect();
                    let mut volume = [0u16; 261];
                    if unsafe {
                        GetVolumePathNameW(wide.as_ptr(), volume.as_mut_ptr(), volume.len() as u32)
                    } == 0 {
                        return DirRemoteness::Unknown;
                    }
                    if unsafe { GetDriveTypeW(volume.as_ptr()) } == DRIVE_REMOTE {
                        DirRemoteness::Remote
                    } else {
                        DirRemoteness::Local
                    }
                } else if #[cfg(any(target_os = "linux", cygwin))] {
                    use nix::sys::statfs::statfs;
                    let Ok(buf) = statfs(narrow.as_c_str()) else {
                        return DirRemoteness::Unknown;
                    };
                    // Linux has constants for these like NFS_SUPER_MAGIC, SMB_SUPER_MAGIC, CIFS_MAGIC_NUMBER but
                    // these are in varying headers. Simply hard code them.
                    // Note that we treat FUSE filesystems as remote, which means we lock less on such filesystems.
                    // NOTE: The cast is necessary for 32-bit systems because of the 4-byte CIFS_MAGIC_NUMBER
                    match buf.filesystem_type().0 as usize  {
                        0x5346414F | // AFS_SUPER_MAGIC - Andrew File System
                        0x6B414653 | // AFS_FS_MAGIC - Kernel AFS and AuriStorFS
                        0x73757245 | // CODA_SUPER_MAGIC - Coda File System
                        0x47504653 | // GPFS - General Parallel File System
                        0x564c |     // NCP_SUPER_MAGIC - Novell NetWare
                        0x6969 |     // NFS_SUPER_MAGIC
                        0x7461636f | // OCFS2_SUPER_MAGIC - Oracle Cluster File System
                        0x61636673 | // ACFS - Oracle ACFS. Undocumented magic number.
                        0x517B |     // SMB_SUPER_MAGIC
                        0xFE534D42 | // SMB2_MAGIC_NUMBER
                        0xFF534D42 |  // CIFS_MAGIC_NUMBER
                        0x01021997 | // V9FS_MAGIC
                        0x19830326 | // fhgfs / BeeGFS. Undocumented magic number.
                        0x013111A7 | 0x013111A8 | // IBRIX. Undocumented.
                        0x65735546 | // FUSE_SUPER_MAGIC
                        0xA501FCF5 // VXFS_SUPER_MAGIC
                            => DirRemoteness::Remote,
                        _ => {
                            DirRemoteness::Unknown
                        }
                    }
                } else if #[cfg(target_os = "netbsd")] {
                    // NetBSD doesn't have statfs, but MNT_LOCAL works for statvfs.
                    use std::mem::MaybeUninit;
                    let mut buf = MaybeUninit::uninit();
                    if unsafe { libc::statvfs(narrow.as_ptr(), buf.as_mut_ptr()) } < 0 {
                        return DirRemoteness::Unknown;
                    }
                    let buf = unsafe { buf.assume_init() };
                    #[allow(clippy::useless_conversion)]
                    let flags = buf.f_flag as u64;
                    #[allow(clippy::unnecessary_cast)]
                    if flags & (libc::MNT_LOCAL as u64) != 0 {
                        DirRemoteness::Local
                    } else {
                        DirRemoteness::Remote
                    }
                } else {
                    let mut buf = std::mem::MaybeUninit::uninit();
                    if unsafe { libc::statfs(narrow.as_ptr(), buf.as_mut_ptr()) } < 0 {
                        return DirRemoteness::Unknown;
                    }
                    let buf = unsafe { buf.assume_init() };
                    // statfs::f_flags types differ.
                    #[allow(clippy::useless_conversion)]
                    let flags = buf.f_flags as u64;
                    #[allow(clippy::unnecessary_cast)]
                    if flags & (libc::MNT_LOCAL as u64) != 0 {
                        DirRemoteness::Local
                    } else {
                        DirRemoteness::Remote
                    }
                }
            }
        }
    }
}

static DATA_DIRECTORY: LazyLock<BaseDirectory> =
    LazyLock::new(|| make_base_directory(L!("XDG_DATA_HOME"), L!("/.local/share/fish")));

static CACHE_DIRECTORY: LazyLock<BaseDirectory> =
    LazyLock::new(|| make_base_directory(L!("XDG_CACHE_HOME"), L!("/.cache/fish")));

static CONFIG_DIRECTORY: LazyLock<BaseDirectory> = LazyLock::new(|| {
    let config_dir = make_base_directory(L!("XDG_CONFIG_HOME"), L!("/.config/fish"));
    if config_dir.created {
        let mut path = PathBuf::from(wcs2osstring(&config_dir.path));
        for basename in ["completions", "conf.d", "functions"] {
            path.push(basename);
            let _ = create_dir(&path, 0o700, false);
            path.pop();
        }
        path.push("config.fish");
        let _ = std::fs::write(
            path,
            br#"
if status is-interactive
    # Commands to run in interactive sessions can go here
end
"#
            .trim_ascii_start(),
        );
    }
    config_dir
});

/// Appends a path component, with a / if necessary.
pub fn append_path_component(path: &mut WString, component: &wstr) {
    if path.is_empty() || component.is_empty() {
        path.push_utfstr(component);
    } else {
        let path_len = path.len();
        let path_slash = path.char_at(path_len - 1) == '/';
        let comp_slash = component.as_char_slice()[0] == '/';
        if !path_slash && !comp_slash {
            // Need a slash
            path.push('/');
        } else if path_slash && comp_slash {
            // Too many slashes.
            path.pop();
        }
        path.push_utfstr(component);
    }
}

#[cfg(test)]
mod tests {
    use super::{path_apply_working_directory, path_make_canonical, paths_are_equivalent};
    use crate::prelude::*;

    #[test]
    fn test_path_make_canonical() {
        let mut path = L!("//foo//////bar/").to_owned();
        path_make_canonical(&mut path);
        assert_eq!(path, "/foo/bar");

        path = L!("/").to_owned();
        path_make_canonical(&mut path);
        assert_eq!(path, "/");
    }

    #[test]
    fn test_path() {
        let mut path = L!("//foo//////bar/").to_owned();
        path_make_canonical(&mut path);
        assert_eq!(&path, L!("/foo/bar"));

        path = L!("/").to_owned();
        path_make_canonical(&mut path);
        assert_eq!(&path, L!("/"));

        path = L!("/home/fishuser/").to_owned();
        path_make_canonical(&mut path);
        assert_eq!(&path, L!("/home/fishuser"));

        assert!(!paths_are_equivalent(L!("/foo/bar/baz"), L!("foo/bar/baz")));
        assert!(paths_are_equivalent(
            L!("///foo///bar/baz"),
            L!("/foo/bar////baz//")
        ));
        assert!(paths_are_equivalent(L!("/foo/bar/baz"), L!("/foo/bar/baz")));
        assert!(paths_are_equivalent(L!("/"), L!("/")));

        assert_eq!(
            path_apply_working_directory(L!("abc"), L!("/def/")),
            L!("/def/abc")
        );
        assert_eq!(
            path_apply_working_directory(L!("abc/"), L!("/def/")),
            L!("/def/abc/")
        );
        assert_eq!(
            path_apply_working_directory(L!("/abc/"), L!("/def/")),
            L!("/abc/")
        );
        assert_eq!(
            path_apply_working_directory(L!("/abc"), L!("/def/")),
            L!("/abc")
        );
        assert!(path_apply_working_directory(L!(""), L!("/def/")).is_empty());
        assert_eq!(path_apply_working_directory(L!("abc"), L!("")), L!("abc"));
    }
}
