use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Debug, PartialEq)]
struct Word {
	text: String,
	quoted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Op {
	Pipe,
	And,
	Or,
	Semi,
	Background,
	Out(u8, bool),
	In,
	Dup(u8, u8),
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
	Word(Word),
	Op(Op),
}

fn lex(line: &str) -> Result<Vec<Token>, String> {
	let chars: Vec<char> = line.chars().collect();
	let mut tokens = Vec::new();
	let mut i = 0;
	while i < chars.len() {
		let c = chars[i];
		if c.is_whitespace() {
			i += 1;
			continue;
		}
		if c == '#' {
			break;
		}
		if let Some((op, used)) = operator_at(&chars, i) {
			tokens.push(Token::Op(op));
			i += used;
			continue;
		}
		let mut text = String::new();
		let mut quoted = false;
		while i < chars.len() {
			let c = chars[i];
			if c.is_whitespace() || matches!(c, '|' | '&' | ';' | '<' | '>') {
				break;
			}
			if c == '\'' {
				quoted = true;
				i += 1;
				while i < chars.len() && chars[i] != '\'' {
					text.push(chars[i]);
					i += 1;
				}
				if i < chars.len() {
					i += 1;
				}
			} else if c == '"' {
				quoted = true;
				i += 1;
				while i < chars.len() && chars[i] != '"' {
					if chars[i] == '\\' && i + 1 < chars.len() && matches!(chars[i + 1], '"' | '\\') {
						i += 1;
					}
					text.push(chars[i]);
					i += 1;
				}
				if i < chars.len() {
					i += 1;
				}
			} else if c == '\\' && i + 1 < chars.len() {
				quoted = true;
				i += 1;
				text.push(chars[i]);
				i += 1;
			} else {
				text.push(c);
				i += 1;
			}
		}
		if !quoted {
			text = expand_tilde(&text);
		}
		if !text.is_empty() || quoted {
			tokens.push(Token::Word(Word { text, quoted }));
		}
	}
	Ok(tokens)
}

fn operator_at(chars: &[char], i: usize) -> Option<(Op, usize)> {
	let c = chars[i];
	let next = chars.get(i + 1).copied();
	match c {
		'|' if next == Some('|') => Some((Op::Or, 2)),
		'|' => Some((Op::Pipe, 1)),
		'&' if next == Some('&') => Some((Op::And, 2)),
		'&' => Some((Op::Background, 1)),
		';' => Some((Op::Semi, 1)),
		'<' => Some((Op::In, 1)),
		'>' if next == Some('>') => Some((Op::Out(1, true), 2)),
		'>' => Some((Op::Out(1, false), 1)),
		'2' if next == Some('>') => {
			if chars.get(i + 2) == Some(&'>') {
				Some((Op::Out(2, true), 3))
			} else if chars.get(i + 2) == Some(&'&') {
				match chars.get(i + 3) {
					Some('1') => Some((Op::Dup(2, 1), 4)),
					Some('2') => Some((Op::Dup(2, 2), 4)),
					_ => Some((Op::Out(2, false), 2)),
				}
			} else {
				Some((Op::Out(2, false), 2))
			}
		}
		'1' if next == Some('>') && chars.get(i + 2) == Some(&'&') => match chars.get(i + 3) {
			Some('2') => Some((Op::Dup(1, 2), 4)),
			Some('1') => Some((Op::Dup(1, 1), 4)),
			_ => Some((Op::Out(1, false), 2)),
		},
		_ => None,
	}
}

fn expand_tilde(text: &str) -> String {
	if text == "~" || text.starts_with("~/") || text.starts_with("~\\") {
		if let Ok(home) = env::var("USERPROFILE") {
			return format!("{home}{}", &text[1..]);
		}
	}
	text.to_string()
}

#[derive(Clone, Debug, Default)]
struct Simple {
	args: Vec<Word>,
	redirects: Vec<Redirect>,
}

#[derive(Clone, Debug)]
enum Redirect {
	Out { fd: u8, path: String, append: bool },
	In { path: String },
	Dup { fd: u8, target: u8 },
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Connector {
	Start,
	And,
	Or,
	Semi,
	Background,
}

#[derive(Clone, Debug)]
struct Pipeline {
	connector: Connector,
	commands: Vec<Simple>,
}

fn parse(tokens: Vec<Token>) -> Result<Vec<Pipeline>, String> {
	let mut pipelines: Vec<Pipeline> = Vec::new();
	let mut commands: Vec<Simple> = Vec::new();
	let mut current = Simple::default();
	let mut connector = Connector::Start;
	let mut i = 0;
	let mut pending_pipe = false;
	while i < tokens.len() {
		match &tokens[i] {
			Token::Word(word) => {
				current.args.push(word.clone());
				pending_pipe = false;
			}
			Token::Op(op) => match op {
				Op::Pipe => {
					if current.args.is_empty() {
						return Err(String::from("syntax: empty command around |"));
					}
					commands.push(std::mem::take(&mut current));
					pending_pipe = true;
				}
				Op::And | Op::Or | Op::Semi | Op::Background => {
					if !pending_pipe && current.args.is_empty() {
						return Err(String::from("syntax: empty command"));
					}
					if !current.args.is_empty() {
						commands.push(std::mem::take(&mut current));
					}
					pipelines.push(Pipeline {
						connector,
						commands: std::mem::take(&mut commands),
					});
					connector = match op {
						Op::And => Connector::And,
						Op::Or => Connector::Or,
						Op::Background => Connector::Background,
						_ => Connector::Semi,
					};
					pending_pipe = false;
				}
				Op::Dup(fd, target) => current.redirects.push(Redirect::Dup {
					fd: *fd,
					target: *target,
				}),
				Op::Out(_, _) | Op::In => {
					let word = match tokens.get(i + 1) {
						Some(Token::Word(word)) => word.clone(),
						_ => return Err(String::from("syntax: redirect needs a target")),
					};
					i += 1;
					match op {
						Op::Out(fd, append) => current.redirects.push(Redirect::Out {
							fd: *fd,
							path: word.text,
							append: *append,
						}),
						Op::In => current.redirects.push(Redirect::In { path: word.text }),
						_ => {}
					}
				}
			},
		}
		i += 1;
	}
	if pending_pipe || (current.args.is_empty() && commands.is_empty() && pipelines.is_empty()) {
		if pending_pipe {
			return Err(String::from("syntax: trailing |"));
		}
		return Ok(Vec::new());
	}
	if !current.args.is_empty() || !current.redirects.is_empty() {
		commands.push(current);
	}
	if !commands.is_empty() {
		pipelines.push(Pipeline { connector, commands });
	}
	Ok(pipelines)
}

fn expand_vars(text: &str) -> String {
	let chars: Vec<char> = text.chars().collect();
	let mut out = String::new();
	let mut i = 0;
	while i < chars.len() {
		if chars[i] == '$' && i + 1 < chars.len() && (chars[i + 1].is_alphabetic() || chars[i + 1] == '_') {
			let mut j = i + 1;
			while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
				j += 1;
			}
			let name: String = chars[i + 1..j].iter().collect();
			out.push_str(&env::var(&name).unwrap_or_default());
			i = j;
		} else {
			out.push(chars[i]);
			i += 1;
		}
	}
	out
}

fn expand_words(words: &[Word]) -> Vec<String> {
	let mut out = Vec::new();
	for word in words {
		let text = expand_vars(&word.text);
		if !word.quoted && has_meta(&text) {
			let found = glob(&text);
			if found.is_empty() {
				out.push(text);
			} else {
				out.extend(found);
			}
		} else {
			out.push(text);
		}
	}
	out
}

fn has_meta(text: &str) -> bool {
	text.contains('*') || text.contains('?') || text.contains('[')
}

fn glob(pattern: &str) -> Vec<String> {
	let mut bases = vec![PathBuf::new()];
	for component in Path::new(pattern).components() {
		use std::path::Component;
		match component {
			Component::Prefix(prefix) => {
				for base in &mut bases {
					base.push(prefix.as_os_str());
				}
			}
			Component::RootDir => {
				for base in &mut bases {
					base.push(Path::new("\\"));
				}
			}
			Component::CurDir => {}
			Component::ParentDir => {
				for base in &mut bases {
					base.push("..");
				}
			}
			Component::Normal(segment) => {
				let name = segment.to_string_lossy().to_string();
				let pattern: Vec<char> = name.chars().collect();
				let mut next = Vec::new();
				for base in &bases {
					if has_meta(&name) {
						let dir = if base.as_os_str().is_empty() {
							PathBuf::from(".")
						} else {
							base.clone()
						};
						if let Ok(entries) = fs::read_dir(&dir) {
							let mut names: Vec<String> = entries
								.flatten()
								.map(|entry| entry.file_name().to_string_lossy().to_string())
								.collect();
							names.sort();
							for entry in names {
								let candidate: Vec<char> = entry.chars().collect();
								if wildcard(&pattern, &candidate) {
									next.push(base.join(entry));
								}
							}
						}
					} else {
						let path = base.join(segment);
						if path.exists() {
							next.push(path);
						}
					}
				}
				bases = next;
			}
		}
	}
	bases
		.into_iter()
		.map(|path| {
			let text = path.to_string_lossy().replace('\\', "/");
			text.strip_prefix("./").unwrap_or(&text).to_string()
		})
		.collect()
}

fn wildcard(pattern: &[char], name: &[char]) -> bool {
	if pattern.is_empty() {
		return name.is_empty();
	}
	match pattern[0] {
		'*' => {
			if wildcard(&pattern[1..], name) {
				return true;
			}
			for i in 0..name.len() {
				if wildcard(&pattern[1..], &name[i + 1..]) {
					return true;
				}
			}
			false
		}
		'?' => !name.is_empty() && wildcard(&pattern[1..], &name[1..]),
		'[' => {
			let mut i = 1;
			let mut negate = false;
			if i < pattern.len() && (pattern[i] == '!' || pattern[i] == '^') {
				negate = true;
				i += 1;
			}
			let mut matched = false;
			let mut closed = false;
			while i < pattern.len() {
				if pattern[i] == ']' && i > 0 {
					closed = true;
					break;
				}
				if i + 2 < pattern.len() && pattern[i + 1] == '-' && pattern[i + 2] != ']' {
					if !name.is_empty() && name[0] >= pattern[i] && name[0] <= pattern[i + 2] {
						matched = true;
					}
					i += 3;
				} else {
					if !name.is_empty() && name[0] == pattern[i] {
						matched = true;
					}
					i += 1;
				}
			}
			if !closed {
				return !name.is_empty() && name[0] == '[' && wildcard(&pattern[1..], &name[1..]);
			}
			!name.is_empty() && matched != negate && wildcard(&pattern[i + 1..], &name[1..])
		}
		c => !name.is_empty() && name[0] == c && wildcard(&pattern[1..], &name[1..]),
	}
}

fn is_builtin(name: &str) -> bool {
	matches!(
		name.to_ascii_lowercase().as_str(),
		"cd" | "pwd"
			| "ls"
			| "cat"
			| "echo"
			| "mkdir"
			| "rm"
			| "cp"
			| "mv"
			| "touch"
			| "which"
			| "type"
			| "clear"
			| "exit"
			| "help"
			| "true"
			| "false"
	)
}

fn current_exe() -> String {
	env::current_exe()
		.map(|path| path.to_string_lossy().to_string())
		.unwrap_or_else(|_| String::from("delta-odd.exe"))
}

fn run_line(line: &str) -> i32 {
	let tokens = match lex(line) {
		Ok(tokens) => tokens,
		Err(err) => {
			eprintln!("odd: {err}");
			return 2;
		}
	};
	let pipelines = match parse(tokens) {
		Ok(pipelines) => pipelines,
		Err(err) => {
			eprintln!("odd: {err}");
			return 2;
		}
	};
	let mut status = 0;
	for pipeline in pipelines {
		let run = match pipeline.connector {
			Connector::And => status == 0,
			Connector::Or => status != 0,
			_ => true,
		};
		if !run {
			continue;
		}
		status = run_pipeline(&pipeline);
	}
	status
}

fn run_pipeline(pipeline: &Pipeline) -> i32 {
	if pipeline.commands.is_empty() {
		return 0;
	}
	if pipeline.commands.len() == 1
		&& pipeline.connector != Connector::Background
		&& pipeline.commands[0].redirects.is_empty()
	{
		let args = expand_words(&pipeline.commands[0].args);
		if args.is_empty() {
			return 0;
		}
		if is_builtin(&args[0]) {
			return run_builtin(&args[0], &args[1..]).unwrap_or(127);
		}
	}
	let mut children = Vec::new();
	let mut previous: Option<std::process::ChildStdout> = None;
	let total = pipeline.commands.len();
	for (index, simple) in pipeline.commands.iter().enumerate() {
		let args = expand_words(&simple.args);
		if args.is_empty() {
			continue;
		}
		let builtin = is_builtin(&args[0]);
		let mut command = if builtin {
			let mut command = Command::new(current_exe());
			command.arg("--builtin").arg(&args[0]).args(&args[1..]);
			command
		} else {
			let mut command = Command::new(&args[0]);
			command.args(&args[1..]);
			command
		};
		let mut stdout_redirected = false;
		let mut stderr_redirected = false;
		let mut stdout_file: Option<(String, bool)> = None;
		let mut merge_stderr = false;
		for redirect in &simple.redirects {
			match redirect {
				Redirect::In { path } => match fs::File::open(path) {
					Ok(file) => {
						command.stdin(Stdio::from(file));
					}
					Err(err) => {
						eprintln!("odd: {path}: {err}");
						return 1;
					}
				},
				Redirect::Out { fd, path, append } => {
					let file = OpenOptions::new()
						.create(true)
						.write(true)
						.truncate(!append)
						.append(*append)
						.open(path);
					match file {
						Ok(file) => {
							if *fd == 2 {
								command.stderr(Stdio::from(file));
								stderr_redirected = true;
							} else {
								command.stdout(Stdio::from(file));
								stdout_redirected = true;
								stdout_file = Some((path.clone(), *append));
							}
						}
						Err(err) => {
							eprintln!("odd: {path}: {err}");
							return 1;
						}
					}
				}
				Redirect::Dup { fd, target } => {
					if *fd == 2 && *target == 1 {
						merge_stderr = true;
					}
				}
			}
		}
		if merge_stderr && !stderr_redirected {
			if let Some((path, append)) = &stdout_file {
				if let Ok(file) = OpenOptions::new()
					.create(true)
					.write(true)
					.truncate(!*append)
					.append(*append)
					.open(path)
				{
					command.stderr(Stdio::from(file));
					stderr_redirected = true;
				}
			}
		}
		if let Some(out) = previous.take() {
			command.stdin(Stdio::from(out));
		}
		if index + 1 < total {
			if !stdout_redirected {
				command.stdout(Stdio::piped());
			}
			if merge_stderr && !stderr_redirected {
				command.stderr(Stdio::piped());
			}
		}
		match command.spawn() {
			Ok(mut child) => {
				if index + 1 < total && !stdout_redirected {
					if let Some(out) = child.stdout.take() {
						previous = Some(out);
					}
				}
				if merge_stderr {
					if let Some(mut err) = child.stderr.take() {
						std::thread::spawn(move || {
							let mut buf = [0u8; 4096];
							loop {
								match err.read(&mut buf) {
									Ok(0) | Err(_) => break,
									Ok(n) => {
										let _ = io::stdout().write_all(&buf[..n]);
									}
								}
							}
						});
					}
				}
				children.push(child);
			}
			Err(err) => {
				let name = args[0].clone();
				if err.kind() == io::ErrorKind::NotFound {
					eprintln!("odd: {name}: command not found");
				} else {
					eprintln!("odd: {name}: {err}");
				}
				return 127;
			}
		}
	}
	if pipeline.connector == Connector::Background {
		if let Some(child) = children.last_mut() {
			println!("[background] {}", child.id());
		}
		return 0;
	}
	let mut status = 0;
	for mut child in children {
		match child.wait() {
			Ok(exit) => status = exit.code().unwrap_or(1),
			Err(_) => status = 1,
		}
	}
	status
}

fn run_builtin(name: &str, args: &[String]) -> Option<i32> {
	let lower = name.to_ascii_lowercase();
	match lower.as_str() {
		"cd" => {
			let target = args
				.first()
				.cloned()
				.or_else(|| env::var("USERPROFILE").ok())
				.unwrap_or_else(|| String::from("."));
			let path = expand_tilde(&target);
			match env::set_current_dir(&path) {
				Ok(()) => Some(0),
				Err(err) => {
					eprintln!("odd: cd: {path}: {err}");
					Some(1)
				}
			}
		}
		"pwd" => {
			match env::current_dir() {
				Ok(path) => println!("{}", path.to_string_lossy()),
				Err(err) => eprintln!("odd: pwd: {err}"),
			}
			Some(0)
		}
		"ls" => {
			let target = args.first().cloned().unwrap_or_else(|| String::from("."));
			match fs::metadata(&target) {
				Ok(meta) if !meta.is_dir() => {
					println!("{target}");
					Some(0)
				}
				Ok(_) => match fs::read_dir(&target) {
					Ok(entries) => {
						let mut names: Vec<(String, bool)> = entries
							.flatten()
							.map(|entry| {
								let name = entry.file_name().to_string_lossy().to_string();
								let dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
								(name, dir)
							})
							.collect();
						names.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
						for (name, dir) in names {
							if dir {
								println!("\x1b[34m{name}/\x1b[0m");
							} else {
								println!("{name}");
							}
						}
						Some(0)
					}
					Err(err) => {
						eprintln!("odd: ls: {target}: {err}");
						Some(1)
					}
				},
				Err(err) => {
					eprintln!("odd: ls: {target}: {err}");
					Some(1)
				}
			}
		}
		"cat" => {
			if args.is_empty() {
				let mut input = Vec::new();
				let _ = io::stdin().read_to_end(&mut input);
				let _ = io::stdout().write_all(&input);
				return Some(0);
			}
			let mut status = 0;
			for path in args {
				if path == "-" {
					let mut input = Vec::new();
					let _ = io::stdin().read_to_end(&mut input);
					let _ = io::stdout().write_all(&input);
					continue;
				}
				match fs::read(path) {
					Ok(bytes) => {
						let _ = io::stdout().write_all(&bytes);
					}
					Err(err) => {
						eprintln!("odd: cat: {path}: {err}");
						status = 1;
					}
				}
			}
			Some(status)
		}
		"echo" => {
			let mut newline = true;
			let mut start = 0;
			if args.first().map(String::as_str) == Some("-n") {
				newline = false;
				start = 1;
			}
			let text = args[start..].join(" ");
			if newline {
				println!("{text}");
			} else {
				print!("{text}");
				let _ = io::stdout().flush();
			}
			Some(0)
		}
		"mkdir" => {
			let mut parents = false;
			let mut status = 0;
			for arg in args {
				if arg == "-p" {
					parents = true;
					continue;
				}
				let result = if parents {
					fs::create_dir_all(arg)
				} else {
					fs::create_dir(arg)
				};
				if let Err(err) = result {
					eprintln!("odd: mkdir: {arg}: {err}");
					status = 1;
				}
			}
			Some(status)
		}
		"rm" => {
			let mut recursive = false;
			let mut status = 0;
			for arg in args {
				if arg == "-r" || arg == "-rf" || arg == "-fr" || arg == "-R" {
					recursive = true;
					continue;
				}
				if arg.starts_with('-') {
					continue;
				}
				let path = Path::new(arg);
				let result = match fs::metadata(path) {
					Ok(meta) if meta.is_dir() => {
						if recursive {
							fs::remove_dir_all(path)
						} else {
							fs::remove_dir(path)
						}
					}
					Ok(_) => fs::remove_file(path),
					Err(err) => Err(err),
				};
				if let Err(err) = result {
					eprintln!("odd: rm: {arg}: {err}");
					status = 1;
				}
			}
			Some(status)
		}
		"cp" => {
			if args.len() < 2 {
				eprintln!("odd: cp: missing operand");
				return Some(1);
			}
			let target = Path::new(&args[args.len() - 1]);
			let mut status = 0;
			for source in &args[..args.len() - 1] {
				if source.starts_with('-') {
					continue;
				}
				let destination = if target.is_dir() {
					match Path::new(source).file_name() {
						Some(name) => target.join(name),
						None => target.to_path_buf(),
					}
				} else {
					target.to_path_buf()
				};
				if let Err(err) = fs::copy(source, &destination) {
					eprintln!("odd: cp: {source}: {err}");
					status = 1;
				}
			}
			Some(status)
		}
		"mv" => {
			if args.len() < 2 {
				eprintln!("odd: mv: missing operand");
				return Some(1);
			}
			let target = Path::new(&args[args.len() - 1]);
			let mut status = 0;
			for source in &args[..args.len() - 1] {
				if source.starts_with('-') {
					continue;
				}
				let destination = if target.is_dir() {
					match Path::new(source).file_name() {
						Some(name) => target.join(name),
						None => target.to_path_buf(),
					}
				} else {
					target.to_path_buf()
				};
				if fs::rename(source, &destination).is_err() {
					if let Err(err) = fs::copy(source, &destination)
						.and_then(|_| fs::remove_file(source).map(|_| 0))
					{
						eprintln!("odd: mv: {source}: {err}");
						status = 1;
					}
				}
			}
			Some(status)
		}
		"touch" => {
			let mut status = 0;
			for path in args {
				if let Err(err) = OpenOptions::new().create(true).append(true).open(path) {
					eprintln!("odd: touch: {path}: {err}");
					status = 1;
				}
			}
			Some(status)
		}
		"which" => {
			let mut status = 0;
			for name in args {
				match find_program(name) {
					Some(path) => println!("{}", path.to_string_lossy()),
					None => status = 1,
				}
			}
			Some(status)
		}
		"type" => {
			let mut status = 0;
			for name in args {
				if is_builtin(name) {
					println!("{name} is a builtin");
					continue;
				}
				match find_program(name) {
					Some(path) => println!("{}", path.to_string_lossy()),
					None => {
						eprintln!("odd: type: {name}: not found");
						status = 1;
					}
				}
			}
			Some(status)
		}
		"clear" => {
			print!("\x1b[2J\x1b[H");
			let _ = io::stdout().flush();
			Some(0)
		}
		"help" => {
			println!("odd: a bash-shaped shell with pipes, globs, quoting, redirects and && || ;");
			println!("builtins: cd pwd ls cat echo mkdir rm cp mv touch which type clear exit help true false");
			println!("not here: $ substitution of the prompt, and most commands; external programs still run");
			Some(0)
		}
		"true" => Some(0),
		"false" => Some(1),
		"exit" => {
			let code = args
				.first()
				.and_then(|arg| arg.parse::<i32>().ok())
				.unwrap_or(0);
			std::process::exit(code);
		}
		_ => None,
	}
}

fn find_program(name: &str) -> Option<PathBuf> {
	let direct = Path::new(name);
	if direct.components().count() > 1 || direct.is_absolute() {
		return if direct.is_file() {
			Some(direct.to_path_buf())
		} else {
			None
		};
	}
	let path = env::var_os("PATH")?;
	let extensions: Vec<String> = env::var("PATHEXT")
		.unwrap_or_else(|_| String::from(".COM;.EXE;.BAT;.CMD"))
		.split(';')
		.filter(|ext| !ext.is_empty())
		.map(|ext| ext.to_lowercase())
		.collect();
	for dir in env::split_paths(&path) {
		let candidate = dir.join(name);
		if candidate.is_file() {
			return Some(candidate);
		}
		for ext in &extensions {
			let candidate = dir.join(format!("{name}{ext}"));
			if candidate.is_file() {
				return Some(candidate);
			}
		}
	}
	None
}

fn prompt() -> String {
	String::from("odd> ")
}

fn main() {
	let args: Vec<String> = env::args().skip(1).collect();
	if args.first().map(String::as_str) == Some("--builtin") {
		let name = args.get(1).cloned().unwrap_or_default();
		let rest = if args.len() > 2 { &args[2..] } else { &[] };
		let status = run_builtin(&name, rest).unwrap_or(127);
		std::process::exit(status);
	}
	let mut line_mode = None;
	if let Some(position) = args.iter().position(|arg| arg == "-c") {
		line_mode = args.get(position + 1).cloned();
	} else if args.first().map(String::as_str) == Some("-h")
		|| args.first().map(String::as_str) == Some("--help")
	{
		let _ = run_builtin("help", &[]);
		return;
	}
	if let Some(line) = line_mode {
		let status = run_line(&line);
		std::process::exit(status);
	}
	let stdin = io::stdin();
	let mut input = stdin.lock();
	println!("odd: type help for builtins; exit leaves");
	loop {
		print!("{}", prompt());
		let _ = io::stdout().flush();
		let mut buffer = String::new();
		match input.read_line(&mut buffer) {
			Ok(0) => break,
			Ok(_) => {}
			Err(_) => break,
		}
		let line = buffer.trim_end_matches(['\r', '\n']);
		if line.trim().is_empty() {
			continue;
		}
		run_line(line);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn lexes_quotes_and_operators() {
		let tokens = lex("echo \"a b\" 'c d' | cat").unwrap();
		assert_eq!(
			tokens,
			vec![
				Token::Word(Word { text: String::from("echo"), quoted: false }),
				Token::Word(Word { text: String::from("a b"), quoted: true }),
				Token::Word(Word { text: String::from("c d"), quoted: true }),
				Token::Op(Op::Pipe),
				Token::Word(Word { text: String::from("cat"), quoted: false }),
			]
		);
	}

	#[test]
	fn parses_pipelines_and_redirects() {
		let pipelines = parse(lex("ls | grep x > out.txt 2>&1 && echo ok").unwrap()).unwrap();
		assert_eq!(pipelines.len(), 2);
		assert_eq!(pipelines[0].commands.len(), 2);
		assert_eq!(pipelines[0].commands[1].redirects.len(), 2);
		assert_eq!(pipelines[1].connector, Connector::And);
	}

	#[test]
	fn wildcard_matches() {
		let pattern: Vec<char> = "*.rs".chars().collect();
		assert!(wildcard(&pattern, &"main.rs".chars().collect::<Vec<_>>()));
		assert!(!wildcard(&pattern, &"main.c".chars().collect::<Vec<_>>()));
		let class: Vec<char> = "f[aeiou]sh".chars().collect();
		assert!(wildcard(&class, &"fish".chars().collect::<Vec<_>>()));
		assert!(!wildcard(&class, &"fsh".chars().collect::<Vec<_>>()));
	}

	#[test]
	fn rejects_dangling_pipe() {
		assert!(parse(lex("ls |").unwrap()).is_err());
	}
}
