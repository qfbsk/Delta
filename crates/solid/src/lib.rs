//! **solid** —— Delta 自己的 shell（法则 24 给的名字，法则 25 给的语言）。
//!
//! bash 的形状，不是 bash 的方言；只有基本内建；`$`、`@` 这类符号留给 delta 自己的语法。

pub mod parse;
pub mod shell;

pub use parse::{ParseError, Statement, parse};
pub use shell::{Reply, Shell};

#[cfg(test)]
mod tests {
	use super::*;
	use std::path::PathBuf;

	fn scratch(tag: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!("solid-test-{}-{tag}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		dir
	}

	fn words(line: &str) -> Vec<Vec<String>> {
		parse(line)
			.unwrap()
			.iter()
			.map(|s| s.words.clone())
			.collect()
	}

	#[test]
	fn parses_bash_shaped_statements() {
		assert_eq!(words("cd src ; pwd"), vec![vec!["cd", "src"], vec!["pwd"]]);
		assert_eq!(words("ls \"a b\""), vec![vec!["ls", "a b"]]);
		assert_eq!(words("ls 'a b'"), vec![vec!["ls", "a b"]]);
	}

	#[test]
	fn reserved_symbols_are_refused_not_interpreted() {
		for line in ["echo $HOME", "ls`x`", "opencode@main"] {
			assert!(
				matches!(parse(line), Err(ParseError::Reserved(_))),
				"{line}"
			);
		}
	}

	#[test]
	fn unterminated_quote_is_an_error_not_a_hang() {
		assert_eq!(parse("ls \"a"), Err(ParseError::UnterminatedQuote('"')));
	}

	#[test]
	fn cd_pwd_and_dotdot_resolve_against_the_shell_cwd() {
		let root = scratch("cd");
		std::fs::create_dir(root.join("src")).unwrap();
		let mut sh = Shell::new(&root);
		assert_eq!(sh.run("cd src"), vec![Reply::Ok]);
		assert_eq!(
			sh.run("pwd"),
			vec![Reply::Out(vec![root.join("src").display().to_string()])]
		);
		assert_eq!(sh.run("cd .."), vec![Reply::Ok]);
		assert_eq!(sh.cwd(), root.as_path());
		assert_eq!(
			sh.run("cd nowhere")[0].error(),
			Some("cd: 没有那个目录 nowhere")
		);
	}

	#[test]
	fn md_creates_and_ls_lists_with_a_slash_on_directories() {
		let root = scratch("md");
		std::fs::write(root.join("a.txt"), "x").unwrap();
		let mut sh = Shell::new(&root);
		assert_eq!(sh.run("md out"), vec![Reply::Ok]);
		assert_eq!(
			sh.run("ls"),
			vec![Reply::Out(vec!["a.txt".into(), "out/".into()])]
		);
	}

	#[test]
	fn cp_copies_files_and_refuses_directories() {
		let root = scratch("cp");
		std::fs::write(root.join("from.txt"), "hello").unwrap();
		let mut sh = Shell::new(&root);
		assert_eq!(sh.run("cp from.txt to.txt"), vec![Reply::Ok]);
		assert_eq!(std::fs::read(root.join("to.txt")).unwrap(), b"hello");
		assert!(
			sh.run("cp . out")[0]
				.error()
				.unwrap()
				.starts_with("cp: 目录要递归")
		);
	}

	#[test]
	fn unknown_commands_are_named_as_outside_this_phase() {
		let root = scratch("unknown");
		let mut sh = Shell::new(&root);
		let reply = &sh.run("gcc -o app main.c")[0];
		assert!(matches!(reply, Reply::Err(_)), "{reply:?}");
	}

	#[test]
	fn relative_paths_do_not_need_a_dot_slash() {
		// 继承语义：裸名字先从 shell 的 cwd 找（carried-semantics/shell-cmd-semantics.md）。
		let root = scratch("relative");
		std::fs::write(root.join("seed.txt"), "1").unwrap();
		std::fs::create_dir(root.join("sub")).unwrap();
		let mut sh = Shell::new(&root);
		assert_eq!(sh.run("cd sub"), vec![Reply::Ok]);
		assert_eq!(sh.run("cp ../seed.txt copy.txt"), vec![Reply::Ok]);
		assert_eq!(
			std::fs::read(root.join("sub").join("copy.txt")).unwrap(),
			b"1"
		);
	}
}
