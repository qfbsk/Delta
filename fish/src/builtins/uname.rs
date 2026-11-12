//! Implementation of the uname builtin.

use super::prelude::*;
use crate::env::Environment as _;
use crate::err_fmt;

#[cfg(windows)]
use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;

#[cfg(windows)]
#[link(name = "ntdll")]
unsafe extern "system" {
    fn RtlGetVersion(version_info: *mut OSVERSIONINFOW) -> i32;
}

const SHORT_OPTIONS: &wstr = L!("asnrvmoh");
const LONG_OPTIONS: &[WOption] = &[
    wopt(L!("all"), NoArgument, 'a'),
    wopt(L!("help"), NoArgument, 'h'),
    wopt(L!("machine"), NoArgument, 'm'),
    wopt(L!("nodename"), NoArgument, 'n'),
    wopt(L!("operating-system"), NoArgument, 'o'),
    wopt(L!("release"), NoArgument, 'r'),
    wopt(L!("sysname"), NoArgument, 's'),
    wopt(L!("version"), NoArgument, 'v'),
];

const SYSNAME: &wstr = L!("Windows_NT");

const MACHINE: &wstr = if cfg!(target_arch = "x86_64") {
    L!("x86_64")
} else if cfg!(target_arch = "aarch64") {
    L!("aarch64")
} else if cfg!(target_arch = "x86") {
    L!("i686")
} else {
    L!("unknown")
};

#[derive(Default, Clone, Copy)]
struct Flags {
    sysname: bool,
    nodename: bool,
    release: bool,
    version: bool,
    machine: bool,
    os: bool,
}

fn append_field(streams: &mut IoStreams, first: &mut bool, field: &wstr) {
    if !*first {
        streams.out.append(L!(" "));
    }
    streams.out.append(field);
    *first = false;
}

#[cfg(windows)]
fn version_numbers() -> (u32, u32, u32) {
    unsafe {
        let mut info: OSVERSIONINFOW = std::mem::zeroed();
        info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
        if RtlGetVersion(&mut info) == 0 {
            return (info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber);
        }
    }
    (10, 0, 0)
}

#[cfg(not(windows))]
fn version_numbers() -> (u32, u32, u32) {
    (0, 0, 0)
}

pub fn uname(parser: &mut Parser, streams: &mut IoStreams, argv: &mut [&wstr]) -> BuiltinResult {
    let cmd = argv[0];
    let argc = argv.len();
    let mut all = false;
    let mut flags = Flags::default();

    let mut w = WGetopter::new(SHORT_OPTIONS, LONG_OPTIONS, argv);
    while let Some(opt) = w.next_opt() {
        match opt {
            'a' => all = true,
            's' => flags.sysname = true,
            'n' => flags.nodename = true,
            'r' => flags.release = true,
            'v' => flags.version = true,
            'm' => flags.machine = true,
            'o' => flags.os = true,
            'h' => {
                builtin_print_help(parser, streams, cmd);
                return Ok(SUCCESS);
            }
            ':' => {
                builtin_missing_argument(parser, streams, cmd, None, argv[w.wopt_index - 1], false);
                return Err(STATUS_INVALID_ARGS);
            }
            ';' => {
                builtin_unexpected_argument(parser, streams, cmd, argv[w.wopt_index - 1], false);
                return Err(STATUS_INVALID_ARGS);
            }
            '?' => {
                builtin_unknown_option(parser, streams, cmd, argv[w.wopt_index - 1], false);
                return Err(STATUS_INVALID_ARGS);
            }
            _ => panic!("unexpected retval from WGetopter"),
        }
    }

    if w.wopt_index != argc {
        err_fmt!(Error::UNEXP_ARG_COUNT, 0, argc - 1)
            .cmd(cmd)
            .finish(streams);
        return Err(STATUS_INVALID_ARGS);
    }

    if all {
        flags = Flags {
            sysname: true,
            nodename: true,
            release: true,
            version: true,
            machine: true,
            os: true,
        };
    }
    if !(flags.sysname
        || flags.nodename
        || flags.release
        || flags.version
        || flags.machine
        || flags.os)
    {
        flags.sysname = true;
    }

    let (major, minor, build) = version_numbers();
    let release = sprintf!("%d.%d.%d", major, minor, build);
    let version = sprintf!("build %d", build);
    let nodename = parser
        .vars()
        .get(L!("COMPUTERNAME"))
        .map(|value| value.as_string())
        .unwrap_or_else(|| L!("localhost").to_owned());

    let mut first = true;
    if flags.sysname {
        append_field(streams, &mut first, SYSNAME);
    }
    if flags.nodename {
        append_field(streams, &mut first, &nodename);
    }
    if flags.release {
        append_field(streams, &mut first, &release);
    }
    if flags.version {
        append_field(streams, &mut first, &version);
    }
    if flags.machine {
        append_field(streams, &mut first, MACHINE);
    }
    if flags.os {
        append_field(streams, &mut first, SYSNAME);
    }
    streams.out.append(L!("\n"));

    Ok(SUCCESS)
}
