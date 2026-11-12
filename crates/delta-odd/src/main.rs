//! odd: Delta's shell. Bash's shape, not its dialect. No `$` or `@` syntax —
//! those belong to Delta. Basic builtins, quotes, globs, pipes, external
//! programs. The line editor follows fish's byte-level input (not its keys).

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicIsize, Ordering};

use windows_sys::Win32::Foundation::HANDLE;

fn main() {
	let args: Vec<String> = std::env::args().skip(1).collect();
	if args.first().map(|s| s == "--builtin").unwrap_or(false) {
		let code = run_builtin(&args[1], &args[2..]);
		std::process::exit(code);
	}
	unsafe {
		windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(keep_shell_alive), 1);
	}
	install_job();
	watch_interrupts();
	let _console = save_console_mode();
	raw_console();
	let mut shell = Shell { history: Vec::new() };
	shell.run();
}

/// The console input mode odd inherited. The shell puts it back when it exits:
/// fish copies whatever mode its child left behind, so a raw mode left on the
/// console would also disable the out-of-band Ctrl+C path that ends foreground
/// jobs.
struct ConsoleGuard(Option<u32>);

impl Drop for ConsoleGuard {
	fn drop(&mut self) {
		use windows_sys::Win32::System::Console::{GetStdHandle, SetConsoleMode, STD_INPUT_HANDLE};
		let Some(mode) = self.0 else { return };
		unsafe {
			SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), mode);
		}
	}
}

fn save_console_mode() -> ConsoleGuard {
	use windows_sys::Win32::System::Console::{GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE};
	let mut mode = 0u32;
	let ok = unsafe { GetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), &mut mode) } != 0;
	ConsoleGuard(ok.then_some(mode))
}

unsafe extern "system" fn keep_shell_alive(kind: u32) -> i32 {
	// Ctrl+C belongs to the running child; the shell only needs to survive
	// long enough to print the next prompt.
	(kind == 0) as i32
}

static JOB: AtomicIsize = AtomicIsize::new(0);

fn install_job() {
	use windows_sys::Win32::System::JobObjects::{
		JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
		JobObjectExtendedLimitInformation, CreateJobObjectW, SetInformationJobObject,
	};
	unsafe {
		let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
		if job.is_null() {
			return;
		}
		let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
		info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
		SetInformationJobObject(
			job,
			JobObjectExtendedLimitInformation,
			&info as *const _ as *const std::ffi::c_void,
			std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
		);
		JOB.store(job as isize, Ordering::SeqCst);
	}
}

fn track(child: &Child) {
	use std::os::windows::io::AsRawHandle;
	use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
	let job = JOB.load(Ordering::SeqCst) as HANDLE;
	if !job.is_null() {
		unsafe {
			AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE);
		}
	}
}

fn end_foreground_job() {
	use windows_sys::Win32::System::JobObjects::TerminateJobObject;
	let job = JOB.load(Ordering::SeqCst) as HANDLE;
	if !job.is_null() {
		unsafe {
			TerminateJobObject(job, 1);
		}
	}
}

fn processed_input() -> bool {
	use windows_sys::Win32::System::Console::{
		ENABLE_PROCESSED_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE,
	};
	unsafe {
		let mut mode = 0u32;
		GetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), &mut mode) != 0
			&& mode & ENABLE_PROCESSED_INPUT != 0
	}
}

// conhost's ConPTY swallows a Ctrl+C key event without ever raising
// CTRL_C_EVENT, so the terminal hands us the key out of band and the shell
// ends the foreground job itself. Raw-input programs clear
// ENABLE_PROCESSED_INPUT and get the key as input instead.
fn watch_interrupts() {
	let Ok(name) = std::env::var("DELTA_CTRL") else {
		return;
	};
	std::thread::spawn(move || {
		let Ok(mut pipe) = std::fs::OpenOptions::new().read(true).open(&name) else {
			return;
		};
		let mut byte = [0u8; 1];
		loop {
			match pipe.read(&mut byte) {
				Ok(1) => {}
				_ => break,
			}
			if processed_input() {
				end_foreground_job();
			}
		}
	});
}

fn set_processed_input(on: bool) {
	use windows_sys::Win32::System::Console::{
		GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_PROCESSED_INPUT, STD_INPUT_HANDLE,
	};
	unsafe {
		let handle = GetStdHandle(STD_INPUT_HANDLE);
		let mut mode = 0u32;
		if GetConsoleMode(handle, &mut mode) != 0 {
			if on {
				mode |= ENABLE_PROCESSED_INPUT;
			} else {
				mode &= !ENABLE_PROCESSED_INPUT;
			}
			SetConsoleMode(handle, mode);
		}
	}
}

fn raw_console() {
	use windows_sys::Win32::System::Console::{
		GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
		ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT, STD_INPUT_HANDLE,
	};
	unsafe {
		let handle = GetStdHandle(STD_INPUT_HANDLE);
		let mut mode = 0u32;
		if GetConsoleMode(handle, &mut mode) != 0 {
			mode &= !(ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT);
			mode |= ENABLE_VIRTUAL_TERMINAL_INPUT;
			SetConsoleMode(handle, mode);
		}
	}
}

struct Shell {
	history: Vec<String>,
}

impl Shell {
	fn run(&mut self) {
		print_banner();
		print!("\x1b[?2004h");
		let _ = std::io::stdout().flush();
		loop {
			let prompt = prompt_text();
			let line = match read_line(&prompt, &self.history) {
				Ok(Some(line)) => line,
				Ok(None) => {
					println!();
					return;
				}
				Err(()) => continue,
			};
			let line = line.trim().to_string();
			if line.is_empty() {
				continue;
			}
			if self.history.last() != Some(&line) {
				self.history.push(line.clone());
			}
			match execute(&line) {
				Flow::Continue => {}
				Flow::Exit => return,
			}
		}
	}
}

enum Flow {
	Continue,
	Exit,
}

fn print_banner() {
	println!("\x1b[1modd\x1b[0m — Delta shell. help 看内建命令，exit 退出。");
}

fn prompt_text() -> String {
	let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
	format!("\x1b[36m{}\x1b[0m> ", cwd.display())
}

fn execute(line: &str) -> Flow {
	let stages = match parse_pipeline(line) {
		Ok(stages) => stages,
		Err(err) => {
			eprintln!("odd: {err}");
			return Flow::Continue;
		}
	};
	if stages.len() == 1 {
		let argv = expand(&stages[0]);
		if argv.is_empty() {
			return Flow::Continue;
		}
		return match argv[0].as_str() {
			"exit" => Flow::Exit,
			"cd" => {
				builtin_cd(&argv[1..]);
				Flow::Continue
			}
			"cp" | "md" | "ls" | "pwd" | "rm" | "echo" | "cat" | "clear" | "help" => {
				run_builtin(&argv[0], &argv[1..]);
				// Builtins print without a trailing newline sometimes; a shell must
				// show the bytes before the next prompt, not on exit.
				let _ = std::io::stdout().flush();
				Flow::Continue
			}
			_ => {
				run_external(&argv, Stdio::inherit(), Stdio::inherit());
				Flow::Continue
			}
		};
	}
	run_pipeline(&stages);
	Flow::Continue
}

fn run_pipeline(stages: &[Vec<String>]) -> bool {
	set_processed_input(true);
	let mut prev: Option<std::process::ChildStdout> = None;
	let mut children = Vec::new();
	for (index, stage) in stages.iter().enumerate() {
		let argv = expand(stage);
		if argv.is_empty() {
			continue;
		}
		let last = index + 1 == stages.len();
		let stdin: Stdio = match prev.take() {
			Some(out) => Stdio::from(out),
			None => Stdio::inherit(),
		};
		let stdout = if last { Stdio::inherit() } else { Stdio::piped() };
		match spawn_stage(&argv, stdin, stdout) {
			Ok(mut child) => {
				if !last {
					prev = child.stdout.take();
				}
				children.push(child);
			}
			Err(err) => {
				eprintln!("odd: {err}");
				set_processed_input(false);
				return false;
			}
		}
	}
	for mut child in children {
		let _ = child.wait();
	}
	set_processed_input(false);
	true
}

fn spawn_stage(argv: &[String], stdin: Stdio, stdout: Stdio) -> std::io::Result<std::process::Child> {
	let name = argv[0].as_str();
	let child = if is_builtin(name) {
		let exe = std::env::current_exe()?;
		Command::new(exe)
			.arg("--builtin")
			.arg(name)
			.args(&argv[1..])
			.stdin(stdin)
			.stdout(stdout)
			.stderr(Stdio::inherit())
			.spawn()?
	} else {
		Command::new(name)
			.args(&argv[1..])
			.stdin(stdin)
			.stdout(stdout)
			.stderr(Stdio::inherit())
			.spawn()?
	};
	track(&child);
	Ok(child)
}

fn is_builtin(name: &str) -> bool {
	matches!(name, "cp" | "md" | "ls" | "pwd" | "rm" | "echo" | "cat" | "clear" | "help")
}

fn run_external(argv: &[String], stdin: Stdio, stdout: Stdio) {
	set_processed_input(true);
	match Command::new(&argv[0]).args(&argv[1..]).stdin(stdin).stdout(stdout).stderr(Stdio::inherit()).spawn() {
		Ok(mut child) => {
			track(&child);
			let _ = child.wait();
		}
		Err(err) => {
			if err.kind() == std::io::ErrorKind::NotFound {
				eprintln!("odd: not found: {}", argv[0]);
			} else {
				eprintln!("odd: {}: {err}", argv[0]);
			}
		}
	}
	set_processed_input(false);
}

fn run_builtin(name: &str, args: &[String]) -> i32 {
	match name {
		"pwd" => {
			match std::env::current_dir() {
				Ok(path) => println!("{}", path.display()),
				Err(err) => eprintln!("odd: pwd: {err}"),
			}
			0
		}
		"cd" => {
			builtin_cd(args);
			0
		}
		"echo" => {
			println!("{}", args.join(" "));
			0
		}
		"clear" => {
			print!("\x1b[2J\x1b[H");
			let _ = std::io::stdout().flush();
			0
		}
		"help" => {
			println!("内建命令：cd cp md ls pwd rm echo cat clear help exit");
			println!("%NAME% 展开环境变量；%SOLID% 是 Delta 自己，例如 %SOLID% README.md 打开编辑器。");
			println!("其他命令按程序名执行。支持引号、通配符和管道。@ 不归 shell。");
			0
		}
		"ls" => builtin_ls(args),
		"cat" => builtin_cat(args),
		"md" => builtin_md(args),
		"cp" => builtin_cp(args),
		"rm" => builtin_rm(args),
		_ => {
			eprintln!("odd: not a builtin: {name}");
			1
		}
	}
}

fn builtin_cd(args: &[String]) {
	let target = args.first().filter(|s| !s.is_empty()).cloned().unwrap_or_else(|| {
		std::env::var("USERPROFILE").unwrap_or_else(|_| String::from("."))
	});
	match std::env::set_current_dir(&target) {
		Ok(()) => {}
		Err(err) => eprintln!("odd: cd: {target}: {err}"),
	}
}

fn builtin_ls(args: &[String]) -> i32 {
	let mut all = false;
	let mut paths = Vec::new();
	for arg in args {
		if arg == "-a" {
			all = true;
		} else {
			paths.push(arg.clone());
		}
	}
	if paths.is_empty() {
		paths.push(String::from("."));
	}
	let mut code = 0;
	for path in paths {
		let p = Path::new(&path);
		let entries = if p.is_dir() {
			match fs::read_dir(p) {
				Ok(read) => read.filter_map(|e| e.ok()).map(|e| e.path()).collect::<Vec<_>>(),
				Err(err) => {
					eprintln!("odd: ls: {path}: {err}");
					code = 1;
					continue;
				}
			}
		} else {
			vec![p.to_path_buf()]
		};
		let mut names: Vec<(String, bool)> = entries
			.iter()
			.filter_map(|entry| {
				let name = entry.file_name()?.to_string_lossy().into_owned();
				if !all && name.starts_with('.') {
					return None;
				}
				Some((name, entry.is_dir()))
			})
			.collect();
		names.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
		if entries.len() > 1 && p.is_dir() {
			println!("{}:", p.display());
		}
		for (name, dir) in &names {
			if *dir {
				println!("\x1b[34m{name}/\x1b[0m");
			} else {
				println!("{name}");
			}
		}
	}
	code
}

fn builtin_cat(args: &[String]) -> i32 {
	if args.is_empty() {
		let mut buf = Vec::new();
		let _ = std::io::stdin().read_to_end(&mut buf);
		let _ = std::io::stdout().write_all(&buf);
		return 0;
	}
	let mut code = 0;
	for path in args {
		match fs::read(path) {
			Ok(bytes) => {
				let _ = std::io::stdout().write_all(&bytes);
			}
			Err(err) => {
				eprintln!("odd: cat: {path}: {err}");
				code = 1;
			}
		}
	}
	code
}

fn builtin_md(args: &[String]) -> i32 {
	if args.is_empty() {
		eprintln!("odd: md: 缺少路径");
		return 1;
	}
	let mut code = 0;
	for path in args {
		if let Err(err) = fs::create_dir_all(path) {
			eprintln!("odd: md: {path}: {err}");
			code = 1;
		}
	}
	code
}

fn builtin_cp(args: &[String]) -> i32 {
	let mut recursive = false;
	let mut paths = Vec::new();
	for arg in args {
		if arg == "-r" || arg == "-R" {
			recursive = true;
		} else {
			paths.push(arg.clone());
		}
	}
	if paths.len() < 2 {
		eprintln!("odd: cp: 需要源和目标");
		return 1;
	}
	let target = PathBuf::from(paths.pop().unwrap());
	let dest_is_dir = target.is_dir();
	if paths.len() > 1 && !dest_is_dir {
		eprintln!("odd: cp: 多个来源时目标必须是目录");
		return 1;
	}
	let mut code = 0;
	for src in paths {
		let src_path = PathBuf::from(&src);
		let dest = if dest_is_dir {
			match src_path.file_name() {
				Some(name) => target.join(name),
				None => {
					eprintln!("odd: cp: {src}: 无法得到文件名");
					code = 1;
					continue;
				}
			}
		} else {
			target.clone()
		};
		if let Err(err) = copy_path(&src_path, &dest, recursive) {
			eprintln!("odd: cp: {src}: {err}");
			code = 1;
		}
	}
	code
}

fn copy_path(src: &Path, dest: &Path, recursive: bool) -> std::io::Result<()> {
	if src.is_dir() {
		if !recursive {
			return Err(std::io::Error::new(std::io::ErrorKind::Other, "是目录，需要 -r"));
		}
		fs::create_dir_all(dest)?;
		for entry in fs::read_dir(src)? {
			let entry = entry?;
			copy_path(&entry.path(), &dest.join(entry.file_name()), true)?;
		}
		Ok(())
	} else {
		fs::copy(src, dest)?;
		Ok(())
	}
}

fn builtin_rm(args: &[String]) -> i32 {
	let mut recursive = false;
	let mut paths = Vec::new();
	for arg in args {
		if arg == "-r" || arg == "-R" {
			recursive = true;
		} else {
			paths.push(arg.clone());
		}
	}
	if paths.is_empty() {
		eprintln!("odd: rm: 缺少路径");
		return 1;
	}
	let mut code = 0;
	for path in paths {
		let p = Path::new(&path);
		let result = if p.is_dir() {
			if recursive {
				fs::remove_dir_all(p)
			} else {
				Err(std::io::Error::new(std::io::ErrorKind::Other, "是目录，需要 -r"))
			}
		} else {
			fs::remove_file(p)
		};
		if let Err(err) = result {
			eprintln!("odd: rm: {path}: {err}");
			code = 1;
		}
	}
	code
}

/// Split a command line into stages on unquoted `|`, honoring single and
/// double quotes. Backslash is literal; Windows paths need no escaping.
fn parse_pipeline(line: &str) -> Result<Vec<Vec<String>>, String> {
	let mut stages = Vec::new();
	let mut current = Vec::new();
	let mut token = String::new();
	let mut has_token = false;
	let mut quote: Option<char> = None;
	let mut chars = line.chars().peekable();
	while let Some(c) = chars.next() {
		match quote {
			Some(q) => {
				if c == q {
					quote = None;
				} else if c == '\\' && q == '"' {
					// A Windows path is literal inside quotes; only \" keeps the
					// quote from closing. UNC \\server shares survive as typed.
					if chars.peek() == Some(&'"') {
						token.push(chars.next().unwrap());
					} else {
						token.push('\\');
					}
				} else {
					token.push(c);
				}
			}
			None => match c {
				'\'' | '"' => {
					quote = Some(c);
					has_token = true;
				}
				'\\' => {
					// Backslash is the native path separator on Windows, so it is
					// literal outside quotes; quote a path to keep spaces together.
					token.push('\\');
					has_token = true;
				}
				'|' => {
					if has_token {
						current.push(token.clone());
						token.clear();
						has_token = false;
					}
					if current.is_empty() {
						return Err(String::from("管道一侧是空的"));
					}
					stages.push(std::mem::take(&mut current));
				}
				c if c.is_whitespace() => {
					if has_token {
						current.push(token.clone());
						token.clear();
						has_token = false;
					}
				}
				_ => {
					token.push(c);
					has_token = true;
				}
			},
		}
	}
	if quote.is_some() {
		return Err(String::from("引号没有闭合"));
	}
	if has_token {
		current.push(token);
	}
	if current.is_empty() {
		return Err(String::from("空命令"));
	}
	stages.push(current);
	Ok(stages)
}

fn expand(argv: &[String]) -> Vec<String> {
	let mut out = Vec::new();
	for arg in argv {
		let expanded = expand_vars(arg);
		if expanded.is_empty() && !arg.is_empty() {
			continue;
		}
		out.extend(expand_glob(&expanded));
	}
	out
}

fn expand_vars(text: &str) -> String {
	let mut out = String::new();
	let mut chars = text.chars().peekable();
	while let Some(ch) = chars.next() {
		if ch != '%' {
			out.push(ch);
			continue;
		}
		let mut name = String::new();
		let mut closed = false;
		for ch in chars.by_ref() {
			if ch == '%' {
				closed = true;
				break;
			}
			name.push(ch);
		}
		if closed && !name.is_empty() {
			out.push_str(&std::env::var(&name).unwrap_or_default());
		} else {
			out.push('%');
			out.push_str(&name);
			if closed {
				out.push('%');
			}
		}
	}
	out
}

fn expand_glob(pattern: &str) -> Vec<String> {
	if !pattern.contains('*') && !pattern.contains('?') {
		return vec![pattern.to_string()];
	}
	let path = Path::new(pattern);
	let (dir, name) = match (path.parent(), path.file_name()) {
		(Some(dir), Some(name)) => {
			let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
			(dir.to_path_buf(), name.to_string_lossy().into_owned())
		}
		_ => return vec![pattern.to_string()],
	};
	let Ok(read) = fs::read_dir(&dir) else { return vec![pattern.to_string()] };
	let mut out = Vec::new();
	for entry in read.flatten() {
		let candidate = entry.file_name().to_string_lossy().into_owned();
		if wildcard(&name, &candidate) {
			let shown = if dir == Path::new(".") {
				candidate
			} else {
				dir.join(candidate).to_string_lossy().into_owned()
			};
			out.push(shown);
		}
	}
	if out.is_empty() {
		vec![pattern.to_string()]
	} else {
		out.sort();
		out
	}
}

fn wildcard(pattern: &str, text: &str) -> bool {
	let p: Vec<char> = pattern.chars().collect();
	let t: Vec<char> = text.chars().collect();
	let mut dp = vec![vec![false; t.len() + 1]; p.len() + 1];
	dp[0][0] = true;
	for i in 1..=p.len() {
		if p[i - 1] == '*' {
			dp[i][0] = dp[i - 1][0];
		}
	}
	for i in 1..=p.len() {
		for j in 1..=t.len() {
			dp[i][j] = match p[i - 1] {
				'*' => dp[i - 1][j] || dp[i][j - 1],
				'?' => dp[i - 1][j - 1],
				c => dp[i - 1][j - 1] && c == t[j - 1],
			};
		}
	}
	dp[p.len()][t.len()]
}

fn read_line(prompt: &str, history: &[String]) -> Result<Option<String>, ()> {
	print!("{prompt}");
	let _ = std::io::stdout().flush();
	let mut line: Vec<char> = Vec::new();
	let mut cursor = 0usize;
	let mut hist_index = history.len();
	let mut draft = String::new();
	let mut utf8: Vec<u8> = Vec::new();
	let mut pushback: Option<u8> = None;
	loop {
		let Some(b) = next_byte(&mut pushback) else {
			return Ok(None);
		};
		if !utf8.is_empty() || b >= 0x80 {
			utf8.push(b);
			if let Ok(s) = std::str::from_utf8(&utf8) {
				for c in s.chars() {
					line.insert(cursor, c);
					cursor += 1;
				}
				utf8.clear();
				redraw(prompt, &line, cursor);
			}
			continue;
		}
		match b {
			b'\r' | b'\n' => {
				println!();
				return Ok(Some(line.iter().collect()));
			}
			0x03 => {
				println!("^C");
				return Ok(Some(String::new()));
			}
			0x04 => {
				if line.is_empty() {
					return Ok(None);
				}
				if cursor < line.len() {
					line.remove(cursor);
					redraw(prompt, &line, cursor);
				}
			}
			0x7f | 0x08 => {
				if cursor > 0 {
					cursor -= 1;
					line.remove(cursor);
					redraw(prompt, &line, cursor);
				}
			}
			0x1b => {
				if !input_pending(40) {
					continue;
				}
				read_escape(
					&mut line,
					&mut cursor,
					prompt,
					&history,
					&mut hist_index,
					&mut draft,
					&mut pushback,
				)?;
			}
			0x15 => {
				// Ctrl+U
				line.drain(..cursor);
				cursor = 0;
				redraw(prompt, &line, cursor);
			}
			c if c >= 0x20 => {
				line.insert(cursor, c as char);
				cursor += 1;
				redraw(prompt, &line, cursor);
			}
			_ => {}
		}
	}
}

fn read_escape(
	line: &mut Vec<char>,
	cursor: &mut usize,
	prompt: &str,
	history: &[String],
	hist_index: &mut usize,
	draft: &mut String,
	pushback: &mut Option<u8>,
) -> Result<(), ()> {
	let Some(first) = next_byte(pushback) else { return Ok(()) };
	let introducer = match first {
		b'[' | b'O' => first,
		// Alt+key arrives as ESC + byte; no binding, but the byte must not be lost.
		_ => {
			*pushback = Some(first);
			return Ok(());
		}
	};
	let mut params: Vec<u8> = Vec::new();
	let final_byte = loop {
		let Some(b) = next_byte(pushback) else { return Ok(()) };
		if (0x20..=0x3f).contains(&b) {
			params.push(b);
			if params.len() > 64 {
				return Ok(());
			}
		} else if (0x40..=0x7e).contains(&b) {
			break b;
		} else {
			return Ok(());
		}
	};
	let params = String::from_utf8_lossy(&params).into_owned();
	let modifiers: Option<u32> = params
		.split_once(';')
		.and_then(|(_, rest)| rest.split(';').next())
		.and_then(|value| value.parse::<u32>().ok())
		.filter(|value| *value > 1);
	let first_num = params.split(';').next().unwrap_or("").parse::<u32>().unwrap_or(1);
	match final_byte {
		b'A' => {
			if *hist_index > 0 {
				if *hist_index == history.len() {
					*draft = line.iter().collect();
				}
				*hist_index -= 1;
				*line = history[*hist_index].chars().collect();
				*cursor = line.len();
				redraw(prompt, line, *cursor);
			}
		}
		b'B' => {
			if *hist_index < history.len() {
				*hist_index += 1;
				*line = if *hist_index == history.len() {
					draft.chars().collect()
				} else {
					history[*hist_index].chars().collect()
				};
				*cursor = line.len();
				redraw(prompt, line, *cursor);
			}
		}
		b'C' => {
			if matches!(modifiers, Some(3) | Some(5) | Some(7)) {
				*cursor = word_right(line, *cursor);
				redraw(prompt, line, *cursor);
			} else if *cursor < line.len() {
				*cursor += 1;
				move_cursor_right(1);
			}
		}
		b'D' => {
			if matches!(modifiers, Some(3) | Some(5) | Some(7)) {
				*cursor = word_left(line, *cursor);
				redraw(prompt, line, *cursor);
			} else if *cursor > 0 {
				*cursor -= 1;
				print!("\x1b[D");
				let _ = std::io::stdout().flush();
			}
		}
		b'H' => {
			*cursor = 0;
			redraw(prompt, line, *cursor);
		}
		b'F' => {
			*cursor = line.len();
			redraw(prompt, line, *cursor);
		}
		b'~' => match first_num {
			3 => {
				if *cursor < line.len() {
					line.remove(*cursor);
					redraw(prompt, line, *cursor);
				}
			}
			1 | 7 => {
				*cursor = 0;
				redraw(prompt, line, *cursor);
			}
			4 | 8 => {
				*cursor = line.len();
				redraw(prompt, line, *cursor);
			}
			200 if introducer == b'[' => paste(line, cursor, prompt)?,
			_ => {}
		},
		_ => {}
	}
	Ok(())
}

fn next_byte(pushback: &mut Option<u8>) -> Option<u8> {
	pushback.take().or_else(console_byte)
}

fn word_left(line: &[char], mut at: usize) -> usize {
	while at > 0 && line[at - 1].is_whitespace() {
		at -= 1;
	}
	while at > 0 && !line[at - 1].is_whitespace() {
		at -= 1;
	}
	at
}

fn word_right(line: &[char], mut at: usize) -> usize {
	while at < line.len() && !line[at].is_whitespace() {
		at += 1;
	}
	while at < line.len() && line[at].is_whitespace() {
		at += 1;
	}
	at
}

fn console_byte() -> Option<u8> {
	use windows_sys::Win32::Storage::FileSystem::ReadFile;
	use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};
	let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
	if handle.is_null() {
		return None;
	}
	let mut byte = [0u8; 1];
	let mut read = 0u32;
	let ok = unsafe { ReadFile(handle, byte.as_mut_ptr(), 1, &mut read, std::ptr::null_mut()) };
	if ok == 0 || read == 0 {
		None
	} else {
		Some(byte[0])
	}
}

/// True when another console input record is already queued; a lone ESC has
/// none, so the editor can tell it apart from the start of a CSI sequence.
fn input_pending(ms: u64) -> bool {
	use windows_sys::Win32::System::Console::{GetNumberOfConsoleInputEvents, GetStdHandle, STD_INPUT_HANDLE};
	let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
	let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
	loop {
		let mut count = 0u32;
		if unsafe { GetNumberOfConsoleInputEvents(handle, &mut count) } == 0 {
			// Redirected stdin: let the blocking read decide.
			return true;
		}
		if count > 0 {
			return true;
		}
		if std::time::Instant::now() >= deadline {
			return false;
		}
		std::thread::sleep(std::time::Duration::from_millis(2));
	}
}

fn paste(line: &mut Vec<char>, cursor: &mut usize, prompt: &str) -> Result<(), ()> {
	let mut data: Vec<u8> = Vec::new();
	loop {
		let Some(byte) = console_byte() else { break };
		data.push(byte);
		if data.ends_with(b"\x1b[201~") {
			data.truncate(data.len() - 6);
			break;
		}
		if data.len() > 4 << 20 {
			break;
		}
	}
	let text = String::from_utf8_lossy(&data);
	for ch in text.chars() {
		let ch = if ch == '\r' || ch == '\n' { ' ' } else { ch };
		line.insert(*cursor, ch);
		*cursor += 1;
	}
	redraw(prompt, line, *cursor);
	Ok(())
}

fn redraw(prompt: &str, line: &[char], cursor: usize) {
	let text: String = line.iter().collect();
	print!("\r\x1b[K{prompt}{text}");
	let tail = line.len().saturating_sub(cursor);
	if tail > 0 {
		print!("\x1b[{}D", tail);
	}
	let _ = std::io::stdout().flush();
}

fn move_cursor_right(n: usize) {
	print!("\x1b[{}C", n);
	let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn percent_expands_environment() {
		let expected = std::env::var("USERPROFILE").unwrap();
		assert_eq!(expand_vars("%USERPROFILE%"), expected);
		assert_eq!(expand_vars("a%USERPROFILE%b"), format!("a{expected}b"));
	}

	#[test]
	fn dollar_belongs_to_delta() {
		assert_eq!(expand_vars("$USERPROFILE"), "$USERPROFILE");
		assert_eq!(expand_vars("$SOLID README.md"), "$SOLID README.md");
	}

	#[test]
	fn unknown_and_unclosed_percent_stay() {
		let expected = std::env::var("USERPROFILE").unwrap();
		assert_eq!(expand_vars("%NOPE_X%"), "");
		assert_eq!(expand_vars("100%"), "100%");
		assert_eq!(expand_vars("%NOPE_X"), "%NOPE_X");
		assert_eq!(expand_vars("<%USERPROFILE%>"), format!("<{expected}>"));
	}

	#[test]
	fn program_path_expands_to_an_executable() {
		let argv = expand(&[String::from("%SystemRoot%\\System32\\where.exe"), String::from("cmd")]);
		assert_eq!(argv.len(), 2);
		assert!(Path::new(&argv[0]).is_file(), "{}", argv[0]);
		let dropped = expand(&[String::from("%NOPE_X%"), String::from("cmd")]);
		assert_eq!(dropped, vec![String::from("cmd")]);
	}
}
