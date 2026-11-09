use std::path::Path;

use tree_sitter::{Language as TsLanguage, Node, Parser};

use crate::theme::Category;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Language {
	Rust,
	C,
	Python,
	JavaScript,
	Json,
	Plain,
}

impl Language {
	pub fn from_path(path: &Path) -> Self {
		match path
			.extension()
			.and_then(|ext| ext.to_str())
			.unwrap_or("")
			.to_ascii_lowercase()
			.as_str()
		{
			"rs" => Self::Rust,
			"c" | "h" => Self::C,
			"py" | "pyw" => Self::Python,
			"js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" => Self::JavaScript,
			"json" => Self::Json,
			_ => Self::Plain,
		}
	}

	pub fn name(self) -> &'static str {
		match self {
			Self::Rust => "rust",
			Self::C => "c",
			Self::Python => "python",
			Self::JavaScript => "javascript",
			Self::Json => "json",
			Self::Plain => "text",
		}
	}

	pub fn lsp_id(self) -> &'static str {
		match self {
			Self::Rust => "rust",
			Self::C => "c",
			Self::Python => "python",
			Self::JavaScript => "javascript",
			Self::Json => "json",
			Self::Plain => "plaintext",
		}
	}

	pub fn grammar(self) -> Option<TsLanguage> {
		match self {
			Self::Rust => Some(TsLanguage::new(tree_sitter_rust::LANGUAGE)),
			Self::C => Some(TsLanguage::new(tree_sitter_c::LANGUAGE)),
			Self::Python => Some(TsLanguage::new(tree_sitter_python::LANGUAGE)),
			Self::JavaScript => Some(TsLanguage::new(tree_sitter_javascript::LANGUAGE)),
			Self::Json => Some(TsLanguage::new(tree_sitter_json::LANGUAGE)),
			Self::Plain => None,
		}
	}
}

#[derive(Clone, Debug)]
pub struct Highlight {
	pub lines: Vec<Vec<Category>>,
}

impl Highlight {
	pub fn category(&self, line: usize, col: usize) -> Category {
		self.lines
			.get(line)
			.and_then(|row| row.get(col))
			.copied()
			.unwrap_or(Category::Text)
	}
}

pub struct Highlighter {
	language: Language,
	parser: Option<Parser>,
}

impl Highlighter {
	pub fn new(language: Language) -> Self {
		let mut highlighter = Self {
			language,
			parser: None,
		};
		highlighter.install();
		highlighter
	}

	fn install(&mut self) {
		self.parser = self.language.grammar().and_then(|grammar| {
			let mut parser = Parser::new();
			parser.set_language(&grammar).ok()?;
			Some(parser)
		});
	}

	pub fn compute(&mut self, source: &str) -> Highlight {
		let line_count = source.split('\n').count().max(1);
		let mut marks = vec![vec![Category::Text; 0]; line_count];
		let mut starts = vec![0usize];
		for (index, byte) in source.bytes().enumerate() {
			if byte == b'\n' {
				starts.push(index + 1);
			}
		}
		for (index, start) in starts.iter().enumerate() {
			let end = starts.get(index + 1).map(|next| next - 1).unwrap_or(source.len());
			marks[index] = vec![Category::Text; source[*start..end].chars().count()];
		}
		let parser = match self.parser.as_mut() {
			Some(parser) => parser,
			None => return Highlight { lines: marks },
		};
		let tree = match parser.parse(source, None) {
			Some(tree) => tree,
			None => return Highlight { lines: marks },
		};
		walk(
			tree.root_node(),
			"",
			source,
			&starts,
			&mut marks,
		);
		Highlight { lines: marks }
	}
}

fn walk(
	node: Node<'_>,
	parent: &str,
	source: &str,
	starts: &[usize],
	marks: &mut [Vec<Category>],
) {
	let kind = node.kind();
	if kind.contains("comment") {
		mark(
			marks,
			starts,
			source,
			node.start_byte(),
			node.end_byte(),
			Category::Comment,
		);
		return;
	}
	if node.child_count() == 0 {
		if let Some(category) = classify(kind, parent) {
			mark(
				marks,
				starts,
				source,
				node.start_byte(),
				node.end_byte(),
				category,
			);
		}
		return;
	}
	let mut cursor = node.walk();
	for child in node.children(&mut cursor) {
		walk(child, kind, source, starts, marks);
	}
}

fn mark(
	marks: &mut [Vec<Category>],
	starts: &[usize],
	source: &str,
	start: usize,
	end: usize,
	category: Category,
) {
	if start >= end || starts.is_empty() {
		return;
	}
	let mut line = starts.partition_point(|&begin| begin <= start).saturating_sub(1);
	loop {
		let line_start = starts[line];
		let line_end = starts
			.get(line + 1)
			.map(|next| next - 1)
			.unwrap_or(source.len());
		let from = start.max(line_start).min(line_end);
		let to = end.min(line_end);
		if from < to {
			let first = source[line_start..from].chars().count();
			let last = source[line_start..to].chars().count();
			if let Some(row) = marks.get_mut(line) {
				for cell in row.iter_mut().take(last).skip(first) {
					*cell = category;
				}
			}
		}
		if end <= line_end + 1 || line + 1 >= starts.len() {
			break;
		}
		line += 1;
	}
}

fn classify(kind: &str, parent: &str) -> Option<Category> {
	if kind.contains("comment") {
		return Some(Category::Comment);
	}
	if kind.contains("string")
		|| kind == "char_literal"
		|| kind == "escape_sequence"
		|| kind == "character"
	{
		return Some(Category::String);
	}
	if kind.contains("number")
		|| kind.contains("integer")
		|| kind.contains("float")
		|| kind == "true"
		|| kind == "false"
		|| kind == "null"
		|| kind == "none"
	{
		return Some(Category::Number);
	}
	if kind.ends_with("type_identifier")
		|| kind == "primitive_type"
		|| kind == "type"
		|| kind == "type_parameter"
		|| kind == "type_argument_list"
	{
		return Some(Category::Type);
	}
	if kind == "field_identifier"
		|| kind == "property_identifier"
		|| kind == "shorthand_property_identifier"
		|| kind == "shorthand_property_identifier_pattern"
	{
		return Some(Category::Property);
	}
	if kind == "identifier" || kind == "type_identifier" || kind == "name" {
		return match parent {
			"function_item"
			| "function_definition"
			| "function_declaration"
			| "function_declarator"
			| "method_definition"
			| "method_declaration"
			| "call_expression"
			| "macro_invocation" => Some(Category::Function),
			"struct_item"
			| "union_item"
			| "enum_item"
			| "trait_item"
			| "type_item"
			| "type_definition"
			| "class_definition"
			| "class_declaration"
			| "interface_declaration"
			| "struct_specifier"
			| "union_specifier"
			| "enum_specifier" => Some(Category::Type),
			"field_declaration" | "field_expression" => Some(Category::Property),
			"decorator" | "attribute_item" | "attribute" => Some(Category::Attribute),
			_ => Some(Category::Text),
		};
	}
	if kind.starts_with("attribute") || kind == "decorator" || kind == "lifetime" {
		return Some(Category::Attribute);
	}
	if kind == "label" || kind == "label_name" {
		return Some(Category::Attribute);
	}
	if is_keyword(kind) {
		return Some(Category::Keyword);
	}
	if is_operator(kind) {
		return Some(Category::Operator);
	}
	None
}

fn is_keyword(kind: &str) -> bool {
	matches!(
		kind,
		"as" | "async"
			| "await"
			| "break"
			| "case"
			| "catch"
			| "char"
			| "class"
			| "const"
			| "continue"
			| "crate"
			| "debugger"
			| "default"
			| "def"
			| "defer"
			| "del"
			| "delete"
			| "do"
			| "double"
			| "dyn"
			| "elif"
			| "else"
			| "enum"
			| "except"
			| "export"
			| "extern"
			| "finally"
			| "float"
			| "fn"
			| "for"
			| "from"
			| "function"
			| "global"
			| "goto"
			| "if"
			| "impl"
			| "import"
			| "in"
			| "instanceof"
			| "int"
			| "interface"
			| "lambda"
			| "let"
			| "long"
			| "loop"
			| "match"
			| "mod"
			| "move"
			| "mut"
			| "new"
			| "nonlocal"
			| "not"
			| "of"
			| "or"
			| "pass"
			| "pub"
			| "raise"
			| "ref"
			| "return"
			| "self"
			| "short"
			| "signed"
			| "sizeof"
			| "static"
			| "struct"
			| "super"
			| "switch"
			| "this"
			| "throw"
			| "trait"
			| "try"
			| "type"
			| "typedef"
			| "typeof"
			| "union"
			| "unsafe"
			| "unsigned"
			| "use"
			| "var"
			| "void"
			| "volatile"
			| "where"
			| "while"
			| "with"
			| "yield"
			| "and"
			| "is"
			| "assert"
			| "print"
			| "exec"
	)
}

fn is_operator(kind: &str) -> bool {
	matches!(
		kind,
		"+" | "-"
			| "*"
			| "/"
			| "%"
			| "="
			| "=="
			| "!="
			| "<"
			| ">"
			| "<="
			| ">="
			| "&&"
			| "||"
			| "!"
			| "&"
			| "|"
			| "^"
			| "~"
			| "=>"
			| "->"
			| "::"
			| "+="
			| "-="
			| "*="
			| "/="
			| "%="
			| "and"
			| "or"
			| "not"
			| "in"
			| "is"
			| "?"
	)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn marks_rust_keywords_functions_and_comments() {
		let source = "fn main() { let x = 1; } // hi";
		let mut highlighter = Highlighter::new(Language::Rust);
		let marks = highlighter.compute(source);
		assert_eq!(marks.category(0, 0), Category::Keyword);
		assert_eq!(marks.category(0, 3), Category::Function);
		assert_eq!(marks.category(0, 12), Category::Keyword);
		assert_eq!(marks.category(0, 20), Category::Number);
		assert_eq!(marks.category(0, 25), Category::Comment);
	}

	#[test]
	fn marks_python_and_c_with_the_same_theme() {
		let mut python = Highlighter::new(Language::Python);
		let marks = python.compute("def f(x):\n    return \"s\"\n");
		assert_eq!(marks.category(0, 0), Category::Keyword);
		assert_eq!(marks.category(0, 4), Category::Function);
		assert_eq!(marks.category(1, 11), Category::String);

		let mut c = Highlighter::new(Language::C);
		let marks = c.compute("int main(void) { return 0; }");
		assert_eq!(marks.category(0, 0), Category::Type);
		assert_eq!(marks.category(0, 4), Category::Function);
		assert_eq!(marks.category(0, 24), Category::Number);
	}
}
