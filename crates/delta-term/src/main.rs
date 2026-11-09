mod even;
mod keys;
mod layout;
mod opencode;
mod ownership;
mod pane;
mod pty;
mod record;
mod solid;
mod tab;
mod term;
mod theme;
mod vt;
mod win;

fn main() {
	let args: Vec<String> = std::env::args().skip(1).collect();
	if args.iter().any(|a| a == "--smoke") {
		if let Err(err) = pty::smoke() {
			eprintln!("smoke: {err}");
			std::process::exit(1);
		}
		return;
	}
	let value = |name: &str| {
		args.windows(2)
			.find(|pair| pair[0] == name)
			.map(|pair| pair[1].clone())
	};
	let shell = value("--shell");
	let image = value("--image");
	let file = value("--file");
	if let Err(err) = win::run(shell, image, file) {
		eprintln!("delta-term: {err}");
		std::process::exit(1);
	}
}
