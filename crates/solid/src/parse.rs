//! solid 的词法：bash 的**形状**，不是 bash 的方言（法则 25）。
//!
//! `$`、`@`、反引号这类符号一律拒绝，并说明它们保留给 delta 自己的语法——
//! `@` 已经是群聊的通信原语，shell 不能把它吃掉。

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
	/// 保留给 delta 的语法符号。
	Reserved(char),
	/// 引号没有闭合。
	UnterminatedQuote(char),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
	pub words: Vec<String>,
}

impl Statement {
	pub fn is_empty(&self) -> bool {
		self.words.is_empty()
	}

	pub fn name(&self) -> Option<&str> {
		self.words.first().map(String::as_str)
	}
}

/// `;` 与换行分隔语句。没有管道、没有重定向、没有变量展开——那些不是"基本内建"。
pub fn parse(line: &str) -> Result<Vec<Statement>, ParseError> {
	let mut statements = Vec::new();
	let mut words = Vec::new();
	let mut current = String::new();
	let mut quote: Option<char> = None;
	let mut in_word = false;

	for ch in line.chars() {
		match quote {
			Some(q) => {
				if ch == q {
					quote = None;
				} else {
					current.push(ch);
				}
			}
			None => match ch {
				'\'' | '"' => {
					quote = Some(ch);
					in_word = true;
				}
				'$' | '@' | '`' => return Err(ParseError::Reserved(ch)),
				';' | '\n' => {
					finish_word(&mut words, &mut current, &mut in_word);
					statements.push(Statement {
						words: std::mem::take(&mut words),
					});
				}
				c if c.is_whitespace() => finish_word(&mut words, &mut current, &mut in_word),
				c => {
					current.push(c);
					in_word = true;
				}
			},
		}
	}

	if let Some(q) = quote {
		return Err(ParseError::UnterminatedQuote(q));
	}
	finish_word(&mut words, &mut current, &mut in_word);
	if !words.is_empty() {
		statements.push(Statement { words });
	}
	Ok(statements)
}

fn finish_word(words: &mut Vec<String>, current: &mut String, in_word: &mut bool) {
	if *in_word {
		words.push(std::mem::take(current));
		*in_word = false;
	}
}
