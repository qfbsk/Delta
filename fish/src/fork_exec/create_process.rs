//! Spawning child processes on Windows.
//!
//! fish's Unix process layer is built around fork() followed by execve() in the child. Windows has
//! neither, so this module replaces both steps with a single CreateProcessW call. The redirection
//! list that a forked child would install with dup2() becomes a set of inheritable handles handed
//! to the child as its standard streams.
//!
//! Only fds 0, 1 and 2 can be passed to a child this way: the C runtime of the new process maps
//! those three handles onto its own descriptors, and no CreateProcess parameter can install an
//! arbitrary descriptor number.

use crate::null_terminated_array::OwningNullTerminatedArray;
use crate::redirection::Dup2List;
use fish_common::os::{RawFd, STDERR_FILENO, STDIN_FILENO, STDOUT_FILENO, handle_from_fd};
use fish_widestring::{WString, bytes2wcstring, wcs2osstring, wstr};
use nix::errno::Errno;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::sync::Mutex;
use windows_sys::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, GENERIC_READ, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, SYNCHRONIZE,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, INFINITE,
    InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
};

/// Job object holding the processes of foreground commands. conhost in a ConPTY swallows the
/// Ctrl+C key without ever raising CTRL_C_EVENT, so the terminal reports the key out of band and
/// fish ends the foreground command by terminating this job. Background commands stay out of it so
/// that interrupting one command leaves them alone.
///
/// A terminated job object cannot accept processes again, so it is closed after every interrupt and
/// created fresh when the next process joins.
static FOREGROUND_JOB: Mutex<isize> = Mutex::new(0);

fn new_foreground_job() -> HANDLE {
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
    if job.is_null() {
        return job;
    }
    unsafe {
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
    }
    job
}

fn with_foreground_job<R>(f: impl FnOnce(HANDLE) -> R) -> R {
    let mut guard = FOREGROUND_JOB.lock().unwrap_or_else(|e| e.into_inner());
    if *guard == 0 {
        *guard = new_foreground_job() as isize;
    }
    f(*guard as HANDLE)
}

fn join_foreground_job(process: HANDLE) {
    use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
    if process.is_null() {
        return;
    }
    with_foreground_job(|job| {
        if !job.is_null() {
            unsafe { AssignProcessToJobObject(job, process) };
        }
    });
}

/// End the foreground command, killing any processes it left running in the job.
/// The exit status matches what Ctrl+C produces on Unix, where the command dies from SIGINT.
pub fn terminate_foreground_job() {
    use windows_sys::Win32::System::JobObjects::TerminateJobObject;
    let job = {
        let mut guard = FOREGROUND_JOB.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::replace(&mut *guard, 0)
    } as HANDLE;
    if job.is_null() {
        return;
    }
    unsafe {
        TerminateJobObject(job, 130);
        CloseHandle(job);
    }
}

/// Put a process that is already running into the foreground job.
///
/// A job that was started in the background waits in its own process group; when `fg` brings it
/// back, Ctrl+C must end it exactly like a command that started in the foreground.
pub fn adopt_foreground_process(handle: isize) {
    join_foreground_job(handle as HANDLE);
}

fn to_wide(s: &wstr) -> Vec<u16> {
    let os = wcs2osstring(s);
    let mut wide: Vec<u16> = os.encode_wide().collect();
    wide.push(0);
    wide
}

/// Duplicate `handle` as an inheritable handle owned by the caller.
fn duplicate_inheritable(handle: HANDLE) -> Result<HANDLE, Errno> {
    let mut dup: HANDLE = ptr::null_mut();
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle,
            GetCurrentProcess(),
            &mut dup,
            0,
            1, /* inheritable */
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == 0 {
        Err(Errno::last())
    } else {
        Ok(dup)
    }
}

/// An inheritable handle for the NUL device, readable or writable.
fn open_nul(write: bool) -> Result<HANDLE, Errno> {
    let mut device: Vec<u16> = "NUL".encode_utf16().collect();
    device.push(0);
    let access = if write { GENERIC_WRITE } else { GENERIC_READ };
    let handle = unsafe {
        CreateFileW(
            device.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(Errno::last());
    }
    let result = duplicate_inheritable(handle);
    unsafe { CloseHandle(handle) };
    result
}

/// Quote one argument the way CommandLineToArgvW (and the MSVC runtime) parses it back.
fn append_quoted_arg(out: &mut Vec<u16>, arg: &[u16]) {
    let needs_quotes = arg.is_empty()
        || arg
            .iter()
            .any(|&c| matches!(c, 0x20 | 0x09 | 0x0A | 0x0B | 0x22));
    if !needs_quotes {
        out.extend_from_slice(arg);
        return;
    }
    out.push(u16::from(b'"'));
    let mut backslashes = 0;
    for &c in arg {
        if c == u16::from(b'\\') {
            backslashes += 1;
        } else if c == u16::from(b'"') {
            // Backslashes before a quote are doubled, and the quote itself is escaped.
            for _ in 0..backslashes * 2 + 1 {
                out.push(u16::from(b'\\'));
            }
            out.push(u16::from(b'"'));
            backslashes = 0;
        } else {
            for _ in 0..backslashes {
                out.push(u16::from(b'\\'));
            }
            backslashes = 0;
            out.push(c);
        }
    }
    // Backslashes before the closing quote are doubled so they survive parsing.
    for _ in 0..backslashes * 2 {
        out.push(u16::from(b'\\'));
    }
    out.push(u16::from(b'"'));
}

/// Build a UTF-16 command line from argv, each element quoted for CommandLineToArgvW.
fn build_command_line(argv: &[WString]) -> Vec<u16> {
    let mut line: Vec<u16> = Vec::new();
    for (i, arg) in argv.iter().enumerate() {
        if i > 0 {
            line.push(u16::from(b' '));
        }
        let os = wcs2osstring(arg);
        let mut wide: Vec<u16> = os.encode_wide().collect();
        if i == 0 {
            // cmd.exe (and some other programs) parse their own argv0 and misread a
            // drive path written with forward slashes, e.g. C:/Windows/System32/cmd.exe.
            // argv0 is only a name for the program, so hand it native separators.
            if wide.contains(&u16::from(b'/')) {
                for unit in wide.iter_mut() {
                    if *unit == u16::from(b'/') {
                        *unit = u16::from(b'\\');
                    }
                }
            }
        }
        append_quoted_arg(&mut line, &wide);
    }
    line.push(0);
    line
}

/// Build the null-separated, double-null-terminated environment block CreateProcessW wants.
/// The strings fish stores are UTF-8 with raw bytes parked in the private use area, so they go
/// through the normal wide conversion rather than being interpreted as ANSI.
fn build_environment_block(envv: &OwningNullTerminatedArray) -> Vec<u16> {
    fn fold(c: &u16) -> u16 {
        if *c < 0x80 {
            u16::from((*c as u8).to_ascii_uppercase())
        } else {
            *c
        }
    }
    let mut entries: Vec<Vec<u16>> = envv
        .iter()
        .map(|entry| {
            let value = bytes2wcstring(entry.to_bytes());
            wcs2osstring(&value).encode_wide().collect()
        })
        .collect();
    // The block should be sorted case-insensitively; this also keeps duplicate names grouped.
    entries.sort_by(|a, b| a.iter().map(fold).cmp(b.iter().map(fold)));
    let mut block: Vec<u16> = Vec::new();
    for entry in entries {
        block.extend_from_slice(&entry);
        block.push(0);
    }
    block.push(0);
    block
}

/// The three standard handles prepared for the child, plus every duplicated handle that must be
/// closed in the parent once CreateProcessW has copied it.
struct StdioHandles {
    std_in: HANDLE,
    std_out: HANDLE,
    std_err: HANDLE,
    owned: Vec<HANDLE>,
}

impl StdioHandles {
    fn new() -> Self {
        Self {
            std_in: ptr::null_mut(),
            std_out: ptr::null_mut(),
            std_err: ptr::null_mut(),
            owned: Vec::new(),
        }
    }

    /// Resolve a target descriptor through the redirection chain and duplicate it for the child.
    /// A closed descriptor becomes the NUL device, which behaves like /dev/null.
    fn handle_for_fd(&mut self, dup2s: &Dup2List, target: RawFd) -> Result<HANDLE, Errno> {
        let src = dup2s.fd_for_target_fd(target);
        let raw = if src < 0 {
            ptr::null_mut()
        } else {
            handle_from_fd(src)
        };
        let handle = if raw.is_null() {
            open_nul(target != STDIN_FILENO)?
        } else {
            duplicate_inheritable(raw)?
        };
        self.owned.push(handle);
        Ok(handle)
    }
}

impl Drop for StdioHandles {
    fn drop(&mut self) {
        for handle in self.owned.drain(..) {
            unsafe { CloseHandle(handle) };
        }
    }
}

/// A process thread attribute list that restricts handle inheritance to an explicit set.
struct HandleListAttribute {
    // Backing storage for the opaque attribute list; the list pointer points into it.
    storage: Vec<usize>,
    list: *mut core::ffi::c_void,
    // CreateProcessW reads this array through the pointer stored in the attribute list.
    handles: Vec<HANDLE>,
}

impl HandleListAttribute {
    fn new(handles: Vec<HANDLE>) -> Result<Self, Errno> {
        let mut size: usize = 0;
        unsafe {
            InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut size);
        }
        if size == 0 {
            return Err(Errno::last());
        }
        let mut storage = vec![0usize; size.div_ceil(size_of::<usize>())];
        let list = storage.as_mut_ptr().cast::<core::ffi::c_void>();
        let ok = unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) };
        if ok == 0 {
            return Err(Errno::last());
        }
        let mut attribute = Self {
            storage,
            list,
            handles,
        };
        let ok = unsafe {
            UpdateProcThreadAttribute(
                attribute.list,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                attribute.handles.as_ptr().cast(),
                attribute.handles.len() * size_of::<HANDLE>(),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if ok == 0 {
            let err = Errno::last();
            unsafe { DeleteProcThreadAttributeList(attribute.list) };
            attribute.list = ptr::null_mut();
            return Err(err);
        }
        Ok(attribute)
    }
}

impl Drop for HandleListAttribute {
    fn drop(&mut self) {
        if !self.list.is_null() {
            unsafe { DeleteProcThreadAttributeList(self.list) };
        }
        let _ = &self.storage;
        let _ = &self.handles;
    }
}

/// Start a thread that waits for the new process and wakes up fish's SIGCHLD machinery.
fn watch_child(process: HANDLE) {
    let mut wait_handle: HANDLE = ptr::null_mut();
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            process,
            GetCurrentProcess(),
            &mut wait_handle,
            SYNCHRONIZE,
            0,
            0,
        )
    };
    if ok == 0 {
        return;
    }
    // Raw handles are not Send; carry the value as an integer into the waiter thread.
    let wait_handle = wait_handle as isize;
    let spawned = std::thread::Builder::new()
        .name("fish-child-watch".into())
        .spawn(move || {
            let wait_handle = wait_handle as HANDLE;
            unsafe {
                WaitForSingleObject(wait_handle, INFINITE);
                CloseHandle(wait_handle);
            }
            crate::signal::signal_notify_child_exit();
        });
    if spawned.is_err() {
        unsafe { CloseHandle(wait_handle as HANDLE) };
    }
}

/// Launch `actual_cmd` with the given argv, environment and redirections.
/// Returns the new process id. The process is registered so waitpid()/kill can find it.
/// `new_group` puts the child in its own console process group, which allows Ctrl+Break to be
/// delivered to it without hitting fish; foreground children share fish's group so that Ctrl+C
/// reaches them directly.
pub fn spawn(
    actual_cmd: &wstr,
    argv: &[WString],
    envv: &OwningNullTerminatedArray,
    dup2s: &Dup2List,
    new_group: bool,
) -> Result<i32, Errno> {
    let mut stdio = StdioHandles::new();
    stdio.std_in = stdio.handle_for_fd(dup2s, STDIN_FILENO)?;
    stdio.std_out = stdio.handle_for_fd(dup2s, STDOUT_FILENO)?;
    stdio.std_err = stdio.handle_for_fd(dup2s, STDERR_FILENO)?;

    let mut command_line = build_command_line(argv);
    let app_name = if actual_cmd.is_empty() {
        None
    } else {
        Some(to_wide(actual_cmd))
    };
    let environment = build_environment_block(envv);
    let attributes = HandleListAttribute::new(vec![stdio.std_in, stdio.std_out, stdio.std_err])?;

    let mut si: STARTUPINFOEXW = STARTUPINFOEXW::default();
    si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = stdio.std_in;
    si.StartupInfo.hStdOutput = stdio.std_out;
    si.StartupInfo.hStdError = stdio.std_err;
    si.lpAttributeList = attributes.list;

    let mut flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT;
    if new_group {
        flags |= CREATE_NEW_PROCESS_GROUP;
    }

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        CreateProcessW(
            app_name.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            1, /* inherit handles */
            flags,
            environment.as_ptr().cast(),
            ptr::null(), /* inherit fish's current directory */
            (&si as *const STARTUPINFOEXW).cast(),
            &mut pi,
        )
    };
    if ok == 0 {
        return Err(Errno::last());
    }

    unsafe { CloseHandle(pi.hThread) };
    let pid = pi.dwProcessId as i32;
    if !new_group {
        join_foreground_job(pi.hProcess);
    }
    nix::register_child(pid, pi.hProcess as isize, new_group);
    watch_child(pi.hProcess);
    Ok(pid)
}
