//! Implementation of the ls builtin.
//!
//! There are no coreutils on a plain Windows install, and odd does not sit on top of
//! MSYS or Cygwin, so the listing itself lives here. The shape follows the ls people
//! know: names in columns for a terminal, one per line when piped, -l for details.

use std::fs::Metadata;
use std::path::{Path, PathBuf};

use super::prelude::*;
use crate::err_fmt;
use crate::termsize::termsize_last;
use fish_wcstringutil::fish_wcwidth_visible;
use fish_widestring::bytes2wcstring;

fn display_width(text: &str) -> usize {
    text.chars()
        .map(|c| usize::try_from(fish_wcwidth_visible(c)).unwrap_or(0))
        .sum()
}

const SHORT_OPTIONS: &wstr = L!("al1hFG");
const COLOR_CHAR: char = '\u{1}';

const LONG_OPTIONS: &[WOption] = &[
    wopt(L!("all"), NoArgument, 'a'),
    wopt(L!("color"), OptionalArgument, COLOR_CHAR),
    wopt(L!("help"), NoArgument, '\u{2}'),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum ColorWhen {
    Auto,
    Always,
    Never,
}

struct Options {
    all: bool,
    long: bool,
    one_per_line: bool,
    human: bool,
    classify: bool,
    color: ColorWhen,
}

struct Entry {
    name: String,
    path: PathBuf,
    meta: Metadata,
    symlink: bool,
}

fn is_hidden(name: &str, meta: &Metadata) -> bool {
    if name.starts_with('.') {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        return meta.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0;
    }
    #[cfg(not(windows))]
    {
        let _ = meta;
        false
    }
}

fn is_executable(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    let ext = ext.to_ascii_lowercase();
    matches!(ext.as_str(), "exe" | "com" | "bat" | "cmd" | "ps1")
}

fn is_symlink(meta: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

#[cfg(windows)]
fn modified_text(meta: &Metadata) -> String {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
    use windows_sys::Win32::System::Time::FileTimeToSystemTime;

    let raw = meta.last_write_time();
    let utc = FILETIME {
        dwLowDateTime: raw as u32,
        dwHighDateTime: (raw >> 32) as u32,
    };
    let mut local: FILETIME = unsafe { std::mem::zeroed() };
    let mut system: SYSTEMTIME = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        FileTimeToLocalFileTime(&utc, &mut local) != 0
            && FileTimeToSystemTime(&local, &mut system) != 0
    };
    if !ok {
        return String::from("?");
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        system.wYear, system.wMonth, system.wDay, system.wHour, system.wMinute
    )
}

#[cfg(not(windows))]
fn modified_text(meta: &Metadata) -> String {
    match meta.modified() {
        Ok(time) => {
            let secs = time
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("{secs}")
        }
        Err(_) => String::from("?"),
    }
}

fn size_text(len: u64, human: bool) -> String {
    if !human || len < 1024 {
        return format!("{len}");
    }
    let units = ["K", "M", "G", "T"];
    let mut value = len as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < units.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1}{}", units[unit])
}

fn mode_text(entry: &Entry) -> String {
    let kind = if entry.symlink {
        'l'
    } else if entry.meta.is_dir() {
        'd'
    } else {
        '-'
    };
    let perms = if entry.meta.permissions().readonly() {
        "r--r--r--"
    } else {
        "rw-rw-rw-"
    };
    format!("{kind}{perms}")
}

fn classify_suffix(entry: &Entry) -> &'static str {
    if entry.meta.is_dir() {
        "/"
    } else if is_executable(&entry.path) {
        "*"
    } else {
        ""
    }
}

fn paint(entry: &Entry, text: &str, color: bool, classify: bool) -> String {
    let mut name = text.to_owned();
    if classify {
        name.push_str(classify_suffix(entry));
    }
    if !color {
        return name;
    }
    let code = if entry.symlink {
        "1;36"
    } else if entry.meta.is_dir() {
        "1;34"
    } else if is_executable(&entry.path) {
        "1;32"
    } else {
        return name;
    };
    format!("\x1b[{code}m{name}\x1b[0m")
}

fn read_entries(dir: &Path, all: bool) -> std::io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    if all {
        for (name, path) in [(".", dir.to_path_buf()), ("..", dir.join(".."))] {
            if let Ok(meta) = std::fs::metadata(&path) {
                let symlink = false;
                entries.push(Entry {
                    name: String::from(name),
                    path,
                    meta,
                    symlink,
                });
            }
        }
    }
    for item in std::fs::read_dir(dir)? {
        let item = item?;
        let path = item.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let name = item.file_name().to_string_lossy().into_owned();
        if !all && is_hidden(&name, &meta) {
            continue;
        }
        let symlink = is_symlink(&meta);
        entries.push(Entry {
            name,
            path,
            meta,
            symlink,
        });
    }
    entries.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(entries)
}

fn print_columns(entries: &[Entry], opts: &Options, color: bool, streams: &mut IoStreams) {
    let labels: Vec<String> = entries
        .iter()
        .map(|e| paint(e, &e.name, color, opts.classify))
        .collect();
    let widths: Vec<usize> = entries
        .iter()
        .map(|e| display_width(&e.name) + if opts.classify { classify_suffix(e).len() } else { 0 })
        .collect();
    let widest = widths.iter().copied().max().unwrap_or(0);
    let term_width = termsize_last().width() as usize;
    let columns = if opts.one_per_line || !streams.out_is_terminal() {
        1
    } else {
        (term_width / (widest + 2).max(1)).max(1).min(entries.len().max(1))
    };
    let rows = entries.len().div_ceil(columns);
    for row in 0..rows {
        let mut line = WString::new();
        for column in 0..columns {
            let index = column * rows + row;
            if index >= entries.len() {
                break;
            }
            let wide = bytes2wcstring(labels[index].as_bytes());
            line.push_utfstr(&wide);
            if column + 1 < columns {
                let next = (column + 1) * rows + row;
                if next < entries.len() {
                    for _ in widths[index]..widest + 2 {
                        line.push(' ');
                    }
                }
            }
        }
        streams.out.appendln(&line);
    }
}

fn print_long(entries: &[Entry], opts: &Options, color: bool, streams: &mut IoStreams) {
    for entry in entries {
        let line = format!(
            "{} {:>10} {} {}",
            mode_text(entry),
            size_text(entry.meta.len(), opts.human),
            modified_text(&entry.meta),
            paint(entry, &entry.name, color, opts.classify)
        );
        streams.out.appendln(&bytes2wcstring(line.as_bytes()));
    }
}

pub fn ls(parser: &mut Parser, streams: &mut IoStreams, argv: &mut [&wstr]) -> BuiltinResult {
    let cmd = argv[0];
    let mut opts = Options {
        all: false,
        long: false,
        one_per_line: false,
        human: false,
        classify: false,
        color: ColorWhen::Auto,
    };
    let mut w = WGetopter::new(SHORT_OPTIONS, LONG_OPTIONS, argv);
    while let Some(opt) = w.next_opt() {
        match opt {
            'a' => opts.all = true,
            'l' => opts.long = true,
            '1' => opts.one_per_line = true,
            'h' => opts.human = true,
            'F' => opts.classify = true,
            'G' => {}
            COLOR_CHAR => {
                let value = w.woptarg;
                opts.color = if value.is_none() || value == Some(L!("always")) {
                    ColorWhen::Always
                } else if value == Some(L!("never")) {
                    ColorWhen::Never
                } else if value == Some(L!("auto")) {
                    ColorWhen::Auto
                } else {
                    err_fmt!("invalid argument '%s' for '--color'", value.unwrap_or_default())
                        .cmd(cmd)
                        .finish(streams);
                    return Err(STATUS_INVALID_ARGS);
                };
            }
            '\u{2}' => {
                streams.out.appendln(L!("Usage: ls [-al1hF] [--color=WHEN] [PATH ...]"));
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

    let optind = w.wopt_index;
    drop(w);

    let color = match opts.color {
        ColorWhen::Always => true,
        ColorWhen::Never => false,
        ColorWhen::Auto => streams.out_is_terminal(),
    };

    let targets: Vec<String> = if optind < argv.len() {
        argv[optind..].iter().map(|s| s.to_string()).collect()
    } else {
        vec![String::from(".")]
    };

    let mut had_error = false;
    let multiple = targets.len() > 1;
    let mut printed = false;
    for target in &targets {
        let path = PathBuf::from(target);
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(error) => {
                err_fmt!("%s: %s", target, error.to_string())
                    .cmd(cmd)
                    .finish(streams);
                had_error = true;
                continue;
            }
        };
        if !meta.is_dir() {
            let symlink = is_symlink(&meta);
            let entry = Entry {
                name: target.clone(),
                path: path.clone(),
                meta,
                symlink,
            };
            streams
                .out
                .appendln(&bytes2wcstring(paint(&entry, &entry.name, color, opts.classify).as_bytes()));
            printed = true;
            continue;
        }
        if multiple {
            if printed {
                streams.out.appendln(L!(""));
            }
            let header = format!("{target}:");
            streams.out.appendln(&bytes2wcstring(header.as_bytes()));
            printed = true;
        }
        let entries = match read_entries(&path, opts.all) {
            Ok(entries) => entries,
            Err(error) => {
                err_fmt!("%s: %s", target, error.to_string())
                    .cmd(cmd)
                    .finish(streams);
                had_error = true;
                continue;
            }
        };
        if opts.long {
            print_long(&entries, &opts, color, streams);
        } else {
            print_columns(&entries, &opts, color, streams);
        }
    }

    if had_error {
        Err(STATUS_CMD_ERROR)
    } else {
        Ok(SUCCESS)
    }
}
