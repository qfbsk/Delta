//! Implementation of the mkdir builtin.

use super::prelude::*;
use crate::err_fmt;

const SHORT_OPTIONS: &wstr = L!("ph");
const LONG_OPTIONS: &[WOption] = &[
    wopt(L!("parents"), NoArgument, 'p'),
    wopt(L!("help"), NoArgument, 'h'),
];

fn parse_options(
    args: &mut [&wstr],
    parser: &Parser,
    streams: &mut IoStreams,
) -> Result<(bool, bool, usize), ErrorCode> {
    let cmd = args[0];
    let mut parents = false;
    let mut print_help = false;

    let mut w = WGetopter::new(SHORT_OPTIONS, LONG_OPTIONS, args);
    while let Some(opt) = w.next_opt() {
        match opt {
            'p' => parents = true,
            'h' => print_help = true,
            ':' => {
                builtin_missing_argument(parser, streams, cmd, None, args[w.wopt_index - 1], false);
                return Err(STATUS_INVALID_ARGS);
            }
            ';' => {
                builtin_unexpected_argument(parser, streams, cmd, args[w.wopt_index - 1], false);
                return Err(STATUS_INVALID_ARGS);
            }
            '?' => {
                builtin_unknown_option(parser, streams, cmd, args[w.wopt_index - 1], false);
                return Err(STATUS_INVALID_ARGS);
            }
            _ => panic!("unexpected retval from WGetopter"),
        }
    }

    Ok((parents, print_help, w.wopt_index))
}

pub fn mkdir(parser: &mut Parser, streams: &mut IoStreams, argv: &mut [&wstr]) -> BuiltinResult {
    let cmd = argv[0];
    let (parents, print_help, optind) = parse_options(argv, parser, streams)?;

    if print_help {
        builtin_print_help(parser, streams, cmd);
        return Ok(SUCCESS);
    }

    if optind == argv.len() {
        err_fmt!("missing operand").cmd(cmd).finish(streams);
        return Err(STATUS_INVALID_ARGS);
    }

    let mut had_error = false;
    for &arg in &argv[optind..] {
        let path_string = arg.to_string();
        let path = std::path::Path::new(&path_string);
        let result = if parents {
            std::fs::create_dir_all(path)
        } else {
            std::fs::create_dir(path)
        };
        if let Err(error) = result {
            err_fmt!("%s: %s", arg, error.to_string())
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
