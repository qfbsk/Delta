mod app;
mod canvas;
mod even;
mod input;
mod layout;
mod mcp;
mod opencode;
mod pty;
mod solid;
mod term;

use std::path::PathBuf;

fn main() {
	let mut shell = None;
	let mut file = None;
	let mut args = std::env::args().skip(1);
	while let Some(arg) = args.next() {
		match arg.as_str() {
			"--shell" => shell = args.next(),
			"--file" => file = args.next().map(PathBuf::from),
			other => {
				if file.is_none() {
					file = Some(PathBuf::from(other));
				}
			}
		}
	}
	std::process::exit(app::run(shell, file));
}
