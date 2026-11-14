use crate::event::{enqueue_signal, is_signal_observed};
use crate::flog::flog;
use crate::prelude::*;
use crate::reader::{signal_safe_reader_handle_sigint, signal_safe_reader_set_exit_signal};
use crate::termsize::signal_safe_termsize_invalidate_tty;
use crate::topic_monitor::{self, Generation, GenerationsList, Topic};
use crate::tty_handoff::signal_safe_mark_tty_invalid;
use crate::wutil::fish_wcstoi;
use fish_common::os::{
    SIGABRT, SIGALRM, SIGBUS, SIGCHLD, SIGCONT, SIGFPE, SIGHUP, SIGILL, SIGINT, SIGIOT, SIGIO,
    SIGKILL, SIGPIPE, SIGPROF, SIGQUIT, SIGSEGV, SIGSTOP, SIGSYS, SIGTERM, SIGTRAP, SIGTSTP,
    SIGTTIN, SIGTTOU, SIGURG, SIGUSR1, SIGUSR2, SIGVTALRM, SIGWINCH, SIGXCPU, SIGXFSZ,
};
use nix::sys::signal::SigSet;
use nix::unistd::getpid;
use std::{
    num::NonZeroI32,
    sync::{
        LazyLock,
        atomic::{AtomicI32, AtomicUsize, Ordering},
    },
};

/// Store the "main" pid.
static MAIN_PID: AtomicI32 = AtomicI32::new(0);

const SIG_DFL: usize = 0;

/// The cancellation signal we have received.
/// Of course this is modified from a signal handler.
static CANCELLATION_SIGNAL: AtomicI32 = AtomicI32::new(0);

/// Ctrl+C bytes conhost queues even though the same key press already reached us out of band.
/// The byte lags behind the interrupt and would run the ctrl-c binding on whatever the user typed
/// in between, so the reader drops it.
static REDUNDANT_INTR_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Consume one redundant Ctrl+C byte, returning whether there was one.
pub fn take_redundant_intr_byte() -> bool {
    let mut current = REDUNDANT_INTR_BYTES.load(Ordering::Relaxed);
    loop {
        if current == 0 {
            return false;
        }
        match REDUNDANT_INTR_BYTES.compare_exchange_weak(
            current,
            current - 1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(actual) => current = actual,
        }
    }
}

/// Set the cancellation signal to zero.
/// In generally this should only be done in interactive sessions.
pub fn signal_clear_cancel() {
    CANCELLATION_SIGNAL.store(0, Ordering::Relaxed);
}

/// Return the most recent cancellation signal received by the fish process.
/// Currently only SIGINT is considered a cancellation signal.
/// This is thread safe.
pub fn signal_check_cancel() -> i32 {
    CANCELLATION_SIGNAL.load(Ordering::Relaxed)
}

/// The single signal handler. Windows has no asynchronous POSIX signals; the console control
/// handler maps Ctrl+C, Ctrl+Break and console shutdown events onto these codes, and the child
/// process registry reports exits.
fn fish_signal_handler(sig: i32) {
    // Check if fish script cares about this.
    let observed = is_signal_observed(sig);
    if observed {
        enqueue_signal(sig);
    }

    // Do some signal-specific stuff.
    match sig {
        SIGWINCH => {
            // Respond to a winch signal by telling the termsize container.
            signal_safe_termsize_invalidate_tty();
        }
        SIGHUP | SIGTERM => {
            // Exit unless the signal was trapped.
            if !observed {
                signal_safe_reader_set_exit_signal(sig);
                if sig == SIGHUP {
                    signal_safe_mark_tty_invalid();
                }
            }
            topic_monitor::principal().post(Topic::SigHupIntTerm);
        }
        SIGINT => {
            // Cancel unless the signal was trapped.
            if !observed {
                CANCELLATION_SIGNAL.store(SIGINT, Ordering::Relaxed);
            }
            signal_safe_reader_handle_sigint();
            topic_monitor::principal().post(Topic::SigHupIntTerm);
        }
        SIGCHLD => {
            // A child process stopped or exited.
            topic_monitor::principal().post(Topic::SigChld);
        }
        SIGQUIT => {
            // Interactive shells ignore ^\.
        }
        SIGALRM => {
            // We have a sigalarm handler that does nothing. This is used in the signal torture
            // test, to verify that we behave correctly when receiving lots of irrelevant signals.
        }
        _ => {}
    }
}

/// Console control handler. Windows invokes this on a thread of its own for every console event.
unsafe extern "system" fn console_ctrl_handler(ctrl_type: u32) -> i32 {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    };
    match ctrl_type {
        CTRL_C_EVENT => fish_signal_handler(SIGINT),
        CTRL_BREAK_EVENT => fish_signal_handler(SIGQUIT),
        // The console is going away; behave like a hangup so history is saved.
        CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => fish_signal_handler(SIGHUP),
        _ => {}
    }
    1 // handled
}

/// Called by the child process layer when a spawned process exits.
pub fn signal_notify_child_exit() {
    topic_monitor::principal().post(Topic::SigChld);
}

/// On Windows there is no per-process signal disposition table to reset.
pub fn signal_reset_handlers() {}

fn sighup_disposition() -> usize {
    // Windows has no way for a parent to observe an ignored SIGHUP; treat it as default.
    SIG_DFL
}

fn set_interactive_handlers() {
    // Job control signals (SIGTSTP, SIGTTIN, SIGTTOU) do not exist on Windows. Ctrl+C is handled
    // by the console control handler installed below.
}

/// Whether the console input is still in "cooked" mode, i.e. the foreground program expects the
/// console to turn Ctrl+C into a signal instead of a byte. Raw-input programs clear this bit
/// and must receive the key as input.
#[cfg(windows)]
fn console_input_processed() -> bool {
    use windows_sys::Win32::System::Console::{
        ENABLE_PROCESSED_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE,
    };
    unsafe {
        let mut mode = 0u32;
        GetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), &mut mode) != 0
            && mode & ENABLE_PROCESSED_INPUT != 0
    }
}

/// conhost in a ConPTY swallows the Ctrl+C key event without ever raising CTRL_C_EVENT, so the
/// terminal hands the key out of band on the `DELTA_CTRL` named pipe. Cancel the command and end
/// the foreground job ourselves, as the console host would have. Raw-input programs clear
/// ENABLE_PROCESSED_INPUT and get the key as an ordinary byte on stdin instead.
#[cfg(windows)]
fn watch_delta_interrupts() {
    use crate::env::{EnvMode, EnvSetMode, EnvStack, unsetenv_lock};
    use std::io::Read as _;
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Ok(name) = std::env::var("DELTA_CTRL") else {
            return;
        };
        // This pipe belongs to this shell alone. Drop the name before any child is started,
        // otherwise a nested shell or script inherits it and fails to open the same pipe.
        let _ = EnvStack::globals().remove(
            L!("DELTA_CTRL"),
            EnvSetMode::new_at_early_startup(EnvMode::GLOBAL_EXPORTED),
        );
        unsetenv_lock(L!("DELTA_CTRL"));
        std::thread::spawn(move || {
            let Ok(mut pipe) = std::fs::OpenOptions::new().read(true).open(&name) else {
                flog!(error, "could not open DELTA_CTRL pipe:", name);
                return;
            };
            let mut byte = [0u8; 1];
            while let Ok(1) = pipe.read(&mut byte) {
                if !console_input_processed() {
                    // A raw-input program owns the key; it arrives as a plain byte on stdin.
                    continue;
                }
                REDUNDANT_INTR_BYTES.fetch_add(1, Ordering::Relaxed);
                fish_signal_handler(SIGINT);
                crate::fork_exec::create_process::terminate_foreground_job();
            }
        });
    });
}

/// Set signal handlers to fish default handlers.
pub fn signal_set_handlers(interactive: bool) {
    // Mark our main pid.
    MAIN_PID.store(getpid().as_raw(), Ordering::Relaxed);

    unsafe {
        windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(console_ctrl_handler), 1);
    }
    #[cfg(windows)]
    watch_delta_interrupts();

    if interactive {
        set_interactive_handlers();
    }
}

pub fn signal_set_handlers_once(interactive: bool) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| signal_set_handlers(interactive));
}

/// Mark that a signal is being handled.
pub fn signal_handle(_sig: RawSignal) {
    // Every console event that fish cares about is already routed to fish_signal_handler.
}

pub static SIGNALS_TO_DEFAULT: LazyLock<SigSet> = LazyLock::new(|| {
    let mut set = SigSet::empty();
    for data in SIGNAL_TABLE.iter() {
        set.add_raw(data.signal.code());
    }
    set
});

/// Ensure we did not inherit any blocked signals. See issue #3964.
/// Windows has no signal masks; the console handler is always active.
pub fn signal_unblock_all() {}

/// Tell fish that the console window size may have changed. Called by the input layer when it sees
/// a window-buffer-size event.
///
/// Unlike the other console events this one is not delivered by the control handler, so the
/// pending-signal bit is set here as the Unix handler would; fire_delayed() turns it into a
/// termsize refresh and a redraw.
pub fn signal_winch() {
    enqueue_signal(SIGWINCH);
    fish_signal_handler(SIGWINCH);
}

/// A Sigchecker can be used to check if a SIGINT (or SIGHUP) has been delivered.
pub struct SigChecker {
    topic: Topic,
    r#gen: Generation,
}

impl SigChecker {
    /// Create a new checker for the given topic.
    pub fn new(topic: Topic) -> Self {
        let mut res = SigChecker { topic, r#gen: 0 };
        // Call check() to update our generation.
        res.check();
        res
    }

    /// Create a new checker for SIGHUP and SIGINT.
    pub fn new_sighupintterm() -> Self {
        Self::new(Topic::SigHupIntTerm)
    }

    /// Check if a sigint has been delivered since the last call to check(), or since the detector
    /// was created.
    pub fn check(&mut self) -> bool {
        let tm = topic_monitor::principal();
        let r#gen = tm.generation_for_topic(self.topic);
        let changed = self.r#gen != r#gen;
        self.r#gen = r#gen;
        changed
    }

    /// Wait until a sigint is delivered.
    pub fn wait(&self) {
        let tm = topic_monitor::principal();
        let mut gens = GenerationsList::invalid();
        gens.set(self.topic, self.r#gen);
        tm.check(&mut gens, true /* wait */);
    }
}

/// Struct describing an entry for the lookup table used to convert between signal names and signal
/// ids, etc.
struct LookupEntry {
    signal: RawSignal,
    name: &'static wstr,
    desc: LocalizableString, // Note: this needs to be localized via gettext before presenting it to the user.
}

impl LookupEntry {
    const fn new(signal: i32, name: &'static wstr, desc: LocalizableString) -> Self {
        Self {
            signal: RawSignal::new(signal),
            name,
            desc,
        }
    }
}

macro_rules! signal_entry {
    ($name:ident, $desc:literal) => {
        LookupEntry::new(
            $name,
            L!(stringify!($name)),
            localizable_string!($desc),
        )
    };
    ($name:ident, $desc:expr) => {
        LookupEntry::new($name, L!(stringify!($name)), $desc)
    };
}

// Lookup table used to convert between signal names and signal ids, etc.
#[rustfmt::skip]
const SIGNAL_TABLE : &[LookupEntry] = &[
    signal_entry!(SIGHUP, "Terminal hung up"),
    signal_entry!(SIGINT, "Quit request from job control (^C)"),
    signal_entry!(SIGQUIT, "Quit request from job control with core dump (^\\)"),
    signal_entry!(SIGILL, "Illegal instruction"),
    signal_entry!(SIGTRAP, "Trace or breakpoint trap"),
    signal_entry!(SIGABRT, "Abort"),
    signal_entry!(SIGBUS, "Misaligned address error"),
    signal_entry!(SIGFPE, "Floating point exception"),
    signal_entry!(SIGKILL, "Forced quit"),
    signal_entry!(SIGUSR1, "User defined signal 1"),
    signal_entry!(SIGUSR2, "User defined signal 2"),
    signal_entry!(SIGSEGV, "Address boundary error"),
    signal_entry!(SIGPIPE, "Broken pipe"),
    signal_entry!(SIGALRM, "Timer expired"),
    signal_entry!(SIGTERM, "Polite quit request"),
    signal_entry!(SIGCHLD, "Child process status changed"),
    signal_entry!(SIGCONT, "Continue previously stopped process"),
    signal_entry!(SIGSTOP, "Forced stop"),
    signal_entry!(SIGTSTP, "Stop request from job control (^Z)"),
    signal_entry!(SIGTTIN, "Stop from terminal input"),
    signal_entry!(SIGTTOU, "Stop from terminal output"),
    signal_entry!(SIGURG, "Urgent socket condition"),
    signal_entry!(SIGXCPU, "CPU time limit exceeded"),
    signal_entry!(SIGXFSZ, "File size limit exceeded"),
    signal_entry!(SIGVTALRM, "Virtual timer expired"),
    signal_entry!(SIGPROF, "Profiling timer expired"),
    signal_entry!(SIGWINCH, "Window size change"),
    signal_entry!(SIGIO, "I/O on asynchronous file descriptor is possible"),
    signal_entry!(SIGSYS, "Bad system call"),
    signal_entry!(SIGIOT, "Abort (Alias for SIGABRT)"),

    #[cfg(any(apple, bsd))]
    signal_entry!(SIGEMT, SIGEMT_DESC),

    #[cfg(any(apple, bsd))]
    signal_entry!(SIGINFO, SIGINFO_DESC),

    #[cfg(all(
        target_os = "linux",
        not(any(
            target_arch = "mips",
            target_arch = "mips32r6",
            target_arch = "mips64",
            target_arch = "mips64r6",
            target_arch = "sparc",
            target_arch = "sparc64",
        ))
    ))]
    signal_entry!(SIGSTKFLT, SIGSTKFLT_DESC),

    #[cfg(target_os = "linux")]
    signal_entry!(SIGPWR, SIGPWR_DESC),

    // TODO: determine whether SIGWIND is defined on any platform.
    //signal_entry!(SIGWIND, "Window size change"),
];

localizable_consts!(
    #[allow(dead_code)]
    SIGEMT_DESC "Unused signal"

    #[allow(dead_code)]
    SIGINFO_DESC "Information request"

    #[allow(dead_code)]
    SIGSTKFLT_DESC "Stack fault"

    #[allow(dead_code)]
    SIGPWR_DESC "Power failure"
);

// Return true if two strings are equal, ignoring ASCII case.
fn equals_ascii_icase(left: &wstr, right: &wstr) -> bool {
    if left.len() != right.len() {
        return false;
    }
    for (lc, rc) in left.chars().zip(right.chars()) {
        if !lc.eq_ignore_ascii_case(&rc) {
            return false;
        }
    }
    true
}

/// Test if \c name is a string describing the signal named \c canonical.
fn match_signal_name(canonical: &wstr, mut name: &wstr) -> bool {
    // Skip the "SIG" prefix if it exists.
    if name.char_count() >= 3 && equals_ascii_icase(name.slice_to(3), L!("sig")) {
        name = name.slice_from(3);
    }
    equals_ascii_icase(canonical.slice_from(3), name)
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
/// A wrapper around the system signal code.
pub struct RawSignal(NonZeroI32);

impl RawSignal {
    /// Creates a new `RawSignal` to represent the passed system signal code `sig`.
    /// Panics if `sig` is zero.
    pub const fn new(sig: i32) -> Self {
        match NonZeroI32::new(sig) {
            None => panic!("Invalid zero signal value!"),
            Some(result) => RawSignal(result),
        }
    }

    /// Return the LookupEntry for ourself.
    fn get_lookup_entry(&self) -> Option<&'static LookupEntry> {
        SIGNAL_TABLE
            .iter()
            .find(|entry| entry.signal == self.code())
    }

    /// Get string representation of a signal.
    /// Previously sig2wcs().
    pub fn name(&self) -> &'static wstr {
        match self.get_lookup_entry() {
            Some(entry) => entry.name,
            None => wgettext!("Unknown"),
        }
    }

    /// Returns a description of the specified signal.
    /// Previously signal_get_desc().
    pub fn desc(&self) -> &'static wstr {
        match self.get_lookup_entry() {
            Some(entry) => entry.desc.localize(),
            None => wgettext!("Unknown"),
        }
    }

    pub fn code(&self) -> i32 {
        self.0.into()
    }
    /// Parses a string into the equivalent [`RawSignal`] sharing the same name.
    /// Accepts both `SIGABC` and `ABC` to match against `Signal::SIGABC`. If the signal name is not
    /// recognized, `None` is returned.
    /// This also accepts integer codes via fish_wcstoi().
    /// Previously sig2wcs().
    pub fn parse(name: &wstr) -> Option<RawSignal> {
        for entry in SIGNAL_TABLE.iter() {
            if match_signal_name(entry.name, name) {
                return Some(entry.signal);
            }
        }

        if let Ok(num) = fish_wcstoi(name) {
            if num > 0 {
                return Some(RawSignal::new(num));
            }
        }
        None
    }
}

// Allow signals to be compared against i32.
impl PartialEq<i32> for RawSignal {
    // TODO
    fn eq(&self, other: &i32) -> bool {
        self.code() == *other
    }
}

impl From<RawSignal> for i32 {
    fn from(value: RawSignal) -> Self {
        value.code()
    }
}

impl From<RawSignal> for usize {
    fn from(value: RawSignal) -> Self {
        usize::try_from(value.code()).unwrap()
    }
}

impl From<RawSignal> for NonZeroI32 {
    fn from(value: RawSignal) -> Self {
        value.0
    }
}

#[cfg(test)]
mod tests {
    use super::RawSignal;
    use crate::prelude::*;

    #[test]
    fn test_signal_name() {
        let sig = RawSignal::new(SIGINT);
        assert_eq!(sig.name(), "SIGINT");
    }

    #[rustfmt::skip]
    #[test]
    fn test_signal_parse() {
        assert_eq!(RawSignal::parse(L!("SIGHUP")), Some(RawSignal::new(SIGHUP)));
        assert_eq!(RawSignal::parse(L!("sigwinch")), Some(RawSignal::new(SIGWINCH)));
        assert_eq!(RawSignal::parse(L!("TSTP")), Some(RawSignal::new(SIGTSTP)));
        assert_eq!(RawSignal::parse(L!("TstP")), Some(RawSignal::new(SIGTSTP)));
        assert_eq!(RawSignal::parse(L!("sigCONT")), Some(RawSignal::new(SIGCONT)));
        assert_eq!(RawSignal::parse(L!("SIGFOO")), None);
        assert_eq!(RawSignal::parse(L!("")), None);
        assert_eq!(RawSignal::parse(L!("SIG")), None);
        assert_eq!(RawSignal::parse(L!("سلام")), None);

        assert_eq!(RawSignal::parse(&SIGINT.to_wstring()), Some(RawSignal::new(SIGINT)));
        assert_eq!(RawSignal::parse(L!("0")), None);
        assert_eq!(RawSignal::parse(L!("-0")), None);
        assert_eq!(RawSignal::parse(L!("-1")), None);
    }

    #[test]
    #[cfg(any(target_os = "freebsd", target_os = "netbsd", target_os = "openbsd"))]
    /// Verify bsd feature is detected on the known BSDs, which gives us greater confidence it'll work
    /// for the unknown ones too. We don't need to do this for Linux and macOS because we're using
    /// rust's native OS targeting for those.
    fn bsd_signals() {
        assert_eq!(
            RawSignal::parse(L!("SIGEMT")),
            Some(RawSignal::new(SIGEMT))
        );
        assert_eq!(
            RawSignal::parse(L!("SIGINFO")),
            Some(RawSignal::new(SIGINFO))
        );
    }
}
