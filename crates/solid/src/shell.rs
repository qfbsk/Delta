//! solid 的内建：只有基本的几个（法则 25 举的是 `cp md cd ls pwd` 等等）。
//!
//! 这一期没有外部程序的执行——shell 的形状先站住，`$`/`@` 的保留位先守住。
//! 路径一律按 shell 自己的 cwd 解析（继承语义见 `carried-semantics/shell-cmd-semantics.md`
//! 里"裸 exe 名先从 shell 的 cwd 找，不需要 ./"的那条规矩，将来执行外部程序时同样适用）。

use std::path::{Path, PathBuf};

use crate::parse::{ParseError, Statement, parse};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
	Ok,
	Out(Vec<String>),
	Err(String),
}

impl Reply {
	pub fn error(&self) -> Option<&str> {
		match self {
			Reply::Err(msg) => Some(msg),
			_ => None,
		}
	}
}

#[derive(Debug)]
pub struct Shell {
	cwd: PathBuf,
}

impl Shell {
	pub fn new(cwd: impl Into<PathBuf>) -> Self {
		Self { cwd: cwd.into() }
	}

	pub fn cwd(&self) -> &Path {
		&self.cwd
	}

	pub fn run(&mut self, line: &str) -> Vec<Reply> {
		match parse(line) {
			Err(err) => return vec![Reply::Err(describe(&err))],
			Ok(statements) => statements
				.iter()
				.filter(|s| !s.is_empty())
				.map(|statement| self.exec(statement))
				.collect(),
		}
	}

	fn exec(&mut self, statement: &Statement) -> Reply {
		let name = statement.name().unwrap_or_default();
		let args = &statement.words[1..];
		match name {
			"cd" => self.cd(args),
			"pwd" if args.is_empty() => Reply::Out(vec![self.cwd.display().to_string()]),
			"ls" => self.ls(args),
			"cp" => self.cp(args),
			"md" => self.md(args),
			other => Reply::Err(format!(
				"solid: 未知命令 {other}（本期只有基本内建：cd pwd ls cp md）"
			)),
		}
	}

	fn cd(&mut self, args: &[String]) -> Reply {
		let [target] = args else {
			return Reply::Err("cd: 需要一个路径".into());
		};
		let path = self.resolve(target);
		if !path.is_dir() {
			return Reply::Err(format!("cd: 没有那个目录 {target}"));
		}
		self.cwd = path;
		Reply::Ok
	}

	fn ls(&self, args: &[String]) -> Reply {
		let dir = match args {
			[] => self.cwd.clone(),
			[target] => self.resolve(target),
			_ => return Reply::Err("ls: 最多一个路径".into()),
		};
		let read = match std::fs::read_dir(&dir) {
			Ok(read) => read,
			Err(why) => return Reply::Err(format!("ls: {why}")),
		};
		let mut names: Vec<String> = read
			.filter_map(|entry| entry.ok())
			.map(|entry| {
				let name = entry.file_name().to_string_lossy().into_owned();
				if entry.path().is_dir() {
					format!("{name}/")
				} else {
					name
				}
			})
			.collect();
		names.sort();
		Reply::Out(names)
	}

	fn cp(&self, args: &[String]) -> Reply {
		let [src, dst] = args else {
			return Reply::Err("cp: 需要源和目标".into());
		};
		let (src, dst) = (self.resolve(src), self.resolve(dst));
		if src.is_dir() {
			return Reply::Err(format!("cp: 目录要递归，本期没做 {}", src.display()));
		}
		match std::fs::copy(&src, &dst) {
			Ok(_) => Reply::Ok,
			Err(why) => Reply::Err(format!("cp: {why}")),
		}
	}

	fn md(&self, args: &[String]) -> Reply {
		let [target] = args else {
			return Reply::Err("md: 需要一个目录名".into());
		};
		match std::fs::create_dir(self.resolve(target)) {
			Ok(()) => Reply::Ok,
			Err(why) => Reply::Err(format!("md: {why}")),
		}
	}

	/// 相对路径按 shell 的 cwd 解析，`.` 与 `..` 在这里就做掉，不靠文件系统。
	fn resolve(&self, arg: &str) -> PathBuf {
		let given = Path::new(arg);
		let mut out = if given.is_absolute() {
			PathBuf::new()
		} else {
			self.cwd.clone()
		};
		for component in given.components() {
			use std::path::Component as C;
			match component {
				C::Prefix(p) => out.push(p.as_os_str()),
				C::RootDir => out.push(std::path::MAIN_SEPARATOR_STR),
				C::CurDir => {}
				C::ParentDir => {
					if !out.pop() {
						out = PathBuf::from(std::path::MAIN_SEPARATOR_STR);
					}
				}
				C::Normal(name) => out.push(name),
			}
		}
		out
	}
}

fn describe(err: &ParseError) -> String {
	match err {
		ParseError::Reserved(ch) => {
			format!("solid: `{ch}` 是 delta 自己的语法符号，shell 不用它（法则 25）")
		}
		ParseError::UnterminatedQuote(ch) => format!("solid: {ch} 没有闭合"),
	}
}
