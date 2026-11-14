//! Implementation of the cp builtin.
//!
//! Basic file copying with -r for directories. On Windows rename cannot move across
//! drives and there is no coreutils, so this is a plain read/write copy.

use std::path::{Path, PathBuf};

use super::prelude::*;
use crate::err_fmt;

const SHORT_OPTIONS: &wstr = L!("rR");
const LONG_OPTIONS: &[WOption] = &[
    wopt(L!("recursive"), NoArgument, 'r'),
    wopt(L!("help"), NoArgument, '\u{2}'),
];

fn copy_file(source: &Path, target: &Path) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::copy(source, target)?;
    Ok(())
}

fn copy_tree(source: &Path, target: &Path) -> std::io::Result<()> {
    if !target.exists() {
        std::fs::create_dir_all(target)?;
    }
    for item in std::fs::read_dir(source)? {
        let item = item?;
        let from = item.path();
        let to = target.join(item.file_name());
        let meta = std::fs::symlink_metadata(&from)?;
        if meta.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            copy_file(&from, &to)?;
        }
    }
    Ok(())
}

fn copy_one(source: &Path, target: &Path, recursive: bool) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(source)?;
    if meta.is_dir() {
        if !recursive {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "omitting directory",
            ));
        }
        copy_tree(source, target)
    } else {
        copy_file(source, target)
    }
}

pub fn cp(parser: &mut Parser, streams: &mut IoStreams, argv: &mut [&wstr]) -> BuiltinResult {
    let cmd = argv[0];
    let mut recursive = false;
    let mut w = WGetopter::new(SHORT_OPTIONS, LONG_OPTIONS, argv);
    while let Some(opt) = w.next_opt() {
        match opt {
            'r' | 'R' => recursive = true,
            '\u{2}' => {
                streams.out.appendln(L!("Usage: cp [-r] SOURCE... DEST"));
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

    let operands = &argv[optind..];
    if operands.is_empty() {
        err_fmt!("missing file operand").cmd(cmd).finish(streams);
        return Err(STATUS_INVALID_ARGS);
    }
    if operands.len() == 1 {
        err_fmt!("missing destination file operand after '%s'", operands[0])
            .cmd(cmd)
            .finish(streams);
        return Err(STATUS_INVALID_ARGS);
    }

    let destination = PathBuf::from(operands[operands.len() - 1].to_string());
    let sources = &operands[..operands.len() - 1];
    let destination_is_dir = destination.is_dir();

    if sources.len() > 1 && !destination_is_dir {
        err_fmt!("target '%s' is not a directory", operands[operands.len() - 1])
            .cmd(cmd)
            .finish(streams);
        return Err(STATUS_CMD_ERROR);
    }

    let mut had_error = false;
    for source in sources {
        let from = PathBuf::from(source.to_string());
        let to = if destination_is_dir {
            match from.file_name() {
                Some(name) => destination.join(name),
                None => {
                    err_fmt!("invalid source '%s'", source.to_string())
                        .cmd(cmd)
                        .finish(streams);
                    had_error = true;
                    continue;
                }
            }
        } else {
            destination.clone()
        };
        if let Err(error) = copy_one(&from, &to, recursive) {
            err_fmt!(
                "cannot copy '%s' to '%s': %s",
                source.to_string(),
                to.to_string_lossy().into_owned(),
                error.to_string()
            )
            .cmd(cmd)
            .finish(streams);
            had_error = true;
        }
    }

    if had_error {
        Err(STATUS_CMD_ERROR)
    } else {
        Ok(SUCCESS)
    }
}
