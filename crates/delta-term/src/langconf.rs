//! VS Code 1.145.2 language-configuration 引擎的 Rust 转写。
//!
//! 原文在 `vendor/vscode-1.145.2/engine/`：`onEnter.ts`、`characterPair.ts`、
//! `indentRules.ts`、`enterAction.ts`、`autoIndent.ts`、`cursorTypeEditOperations.ts`
//! （AutoChecking 输入操作）、`cursorCommon.ts`（isQuote / shouldAutoCloseBefore）、
//! `shiftCommand.ts` 与 `misc-indentation.ts`（缩进换算）。
//! 语言配置原文在 `vendor/vscode-1.145.2/language-configuration/`，测试原文在
//! `vendor/vscode-1.145.2/tests/`。
//!
//! 唯一的替代：VS Code 用 Monarch 词法判断“某个字符在字符串/注释/正则里”，
//! solid 只有 tree-sitter，所以这里用解析树的作用域代替 token 类型。
//! 单语言文件里，VS Code 的 `ScopedLineTokens.languageId` 恒为文件语言，
//! `IndentationContextProcessor` 里“上一行文本”“语言是否从行首开始”的嵌入语言分支
//! 因此退化为：上一行文本恒可用（第一行为空），语言不跨行切换。
//! 未转写：`getTokenTypeIfInsertingCharacter` 的中性字符试探、电字符（electric char）、
//! 多光标与撤销栈的 undo stop——由作用域判断和 solid 自己的撤销粒度替代。

use std::sync::OnceLock;

use onig::Regex as OnigRegex;
use serde_json::Value;

use crate::solid::Lang;

/// characterPair.ts: CharacterPairSupport 里的两组默认“闭合前允许的字符”。
const DEFAULT_AUTOCLOSE_BEFORE_QUOTES: &str = ";:.,=}])> \n\t";
const DEFAULT_AUTOCLOSE_BEFORE_BRACKETS: &str = "'\"`;:.,=}])> \n\t";

/// cursorCommon.ts 默认 wordSeparators（editorOptions.ts 的 USUAL_WORD_SEPARATORS）。
const USUAL_WORD_SEPARATORS: &str = "`~!@#$%^&*()-=+[{]}\\|;:'\",.<>/?";

/// 1-based 行列。列按字符计，和 solid 的光标一致。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeKind {
	Other,
	String,
	Comment,
	Regex,
}

pub trait Model {
	fn line(&self, line: usize) -> String;
	fn processed_line(&self, line: usize) -> String;
	fn processed_before(&self, line: usize, column: usize) -> String;
	fn processed_after(&self, line: usize, column: usize) -> String;
	/// 插入点前一个字符所在的 token 类型（VS Code 的 findTokenIndexAtOffset(column - 2)）。
	fn scope_before(&self, line: usize, column: usize) -> ScopeKind;
	/// bracketPairs.hasUnmatchedClosingBracketAfter。
	fn unmatched_closing_after(&self, line: usize, column: usize, open: &str) -> bool;
}

/// EnterAction 的取值，顺序同 languageConfiguration.ts。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndentAction {
	None,
	Indent,
	IndentOutdent,
	Outdent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnterAction {
	pub indent_action: IndentAction,
	pub append_text: String,
	pub remove_text: usize,
	pub indentation: String,
}

/// onEnter 规则的 action（还没有按光标补上 indentation）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnterRuleAction {
	pub indent_action: IndentAction,
	pub append_text: String,
	pub remove_text: usize,
}

impl EnterRuleAction {
	fn new(indent_action: IndentAction) -> Self {
		Self {
			indent_action,
			append_text: String::new(),
			remove_text: 0,
		}
	}
}

#[derive(Clone)]
pub struct Re(std::sync::Arc<OnigRegex>);

impl Re {
	fn new(pattern: &str) -> Option<Self> {
		OnigRegex::new(pattern).ok().map(|re| Re(std::sync::Arc::new(re)))
	}

	pub fn is_match(&self, text: &str) -> bool {
		// onig::Regex::is_match 是整串匹配；VS Code 的 RegExp.test 是搜索。
		self.0.find(text).is_some()
	}
}

impl std::fmt::Debug for Re {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("Re")
	}
}

#[derive(Debug)]
pub struct AutoClosingPair {
	pub open: String,
	pub close: String,
	pub before_text: Option<Re>,
	in_string: bool,
	in_comment: bool,
	in_regex: bool,
}

impl AutoClosingPair {
	pub fn plain(open: &str, close: &str) -> Self {
		Self {
			open: open.to_string(),
			close: close.to_string(),
			before_text: None,
			in_string: true,
			in_comment: true,
			in_regex: true,
		}
	}

	/// StandardAutoClosingPairConditional.isOK
	pub fn is_ok(&self, kind: ScopeKind) -> bool {
		match kind {
			ScopeKind::Other => true,
			ScopeKind::Comment => self.in_comment,
			ScopeKind::String => self.in_string,
			ScopeKind::Regex => self.in_regex,
		}
	}

	/// StandardAutoClosingPairConditional.shouldAutoCloseBefore
	pub fn should_auto_close_before(&self, text: &str) -> bool {
		match &self.before_text {
			None => true,
			Some(re) => re.is_match(text),
		}
	}
}

#[derive(Debug)]
pub struct IndentRules {
	pub increase: Option<Re>,
	pub decrease: Option<Re>,
	pub indent_next_line: Option<Re>,
	pub un_indented: Option<Re>,
}

#[derive(Clone, Debug)]
pub struct OnEnterRule {
	pub before: Option<Re>,
	pub after: Option<Re>,
	pub previous: Option<Re>,
	pub action: EnterRuleAction,
}

#[derive(Clone, Debug)]
struct BracketRule {
	open: Re,
	close: Re,
}

/// onEnter.ts: OnEnterSupport。
#[derive(Clone, Default)]
pub struct OnEnterSupport {
	brackets: Vec<BracketRule>,
	rules: Vec<OnEnterRule>,
}

#[derive(Default)]
pub struct LanguageConfiguration {
	pub brackets: Vec<(String, String)>,
	pub auto_closing: Vec<AutoClosingPair>,
	pub surrounding: Vec<(String, String)>,
	pub auto_close_before: Option<String>,
	pub indentation: Option<IndentRules>,
	pub on_enter: Vec<OnEnterRule>,
	/// parse_language 末尾按 brackets 与 on_enter 预编译。
	pub on_enter_support: OnEnterSupport,
	block_comment_start: Option<String>,
}

impl LanguageConfiguration {
	/// LanguageConfiguration.getAutoClosingPairs() / getSurroundingPairs() 的按图查表。
	pub fn auto_close_before_chars(&self, for_quotes: bool) -> &str {
		match &self.auto_close_before {
			Some(set) => set,
			None => {
				if for_quotes {
					DEFAULT_AUTOCLOSE_BEFORE_QUOTES
				} else {
					DEFAULT_AUTOCLOSE_BEFORE_BRACKETS
				}
			}
		}
	}
}

impl OnEnterSupport {
	pub fn from_parts(brackets: &[(String, String)], rules: Vec<OnEnterRule>) -> Self {
		let mut processed = Vec::new();
		for (open, close) in brackets {
			let open_re = create_open_bracket_regexp(open);
			let close_re = create_close_bracket_regexp(close);
			if let (Some(open), Some(close)) = (open_re, close_re) {
				processed.push(BracketRule { open, close });
			}
		}
		Self {
			brackets: processed,
			rules,
		}
	}

	/// OnEnterSupport.onEnter（autoIndent 固定为 Full，规则在 Advanced 起生效，括号在 Brackets 起生效）。
	pub fn on_enter(
		&self,
		previous_line_text: &str,
		before_enter_text: &str,
		after_enter_text: &str,
	) -> Option<EnterRuleAction> {
		for rule in &self.rules {
			let all = [
				(rule.before.as_ref(), before_enter_text),
				(rule.after.as_ref(), after_enter_text),
				(rule.previous.as_ref(), previous_line_text),
			]
			.iter()
			.all(|(re, text)| re.is_none_or(|re| re.is_match(text)));
			if all {
				return Some(rule.action.clone());
			}
		}

		if !before_enter_text.is_empty() && !after_enter_text.is_empty() {
			for bracket in &self.brackets {
				if bracket.open.is_match(before_enter_text)
					&& bracket.close.is_match(after_enter_text)
				{
					return Some(EnterRuleAction::new(IndentAction::IndentOutdent));
				}
			}
		}

		if !before_enter_text.is_empty() {
			for bracket in &self.brackets {
				if bracket.open.is_match(before_enter_text) {
					return Some(EnterRuleAction::new(IndentAction::Indent));
				}
			}
		}

		None
	}
}

fn create_open_bracket_regexp(bracket: &str) -> Option<Re> {
	let mut str = escape_reg_exp_characters(bracket);
	if !first_char_is_non_word(&str) {
		str.insert_str(0, "\\b");
	}
	str.push_str("\\s*$");
	Re::new(&str)
}

fn create_close_bracket_regexp(bracket: &str) -> Option<Re> {
	let mut str = escape_reg_exp_characters(bracket);
	if !last_char_is_non_word(&str) {
		str.push_str("\\b");
	}
	str.insert_str(0, "^\\s*");
	Re::new(&str)
}

/// strings.ts: escapeRegExpCharacters。括号都是 ASCII，空格也转义。
fn escape_reg_exp_characters(value: &str) -> String {
	let mut out = String::with_capacity(value.len());
	for c in value.chars() {
		if "\\{}*+?|^$.,[]()#".contains(c) || c.is_whitespace() {
			out.push('\\');
		}
		out.push(c);
	}
	out
}

fn first_char_is_non_word(value: &str) -> bool {
	match value.chars().next() {
		Some(c) => !is_word_char(c),
		None => true,
	}
}

fn last_char_is_non_word(value: &str) -> bool {
	match value.chars().last() {
		Some(c) => !is_word_char(c),
		None => true,
	}
}

fn is_word_char(c: char) -> bool {
	c.is_ascii_alphanumeric() || c == '_'
}

/// IndentRulesSupport 的四个判断。
pub fn leading_whitespace(text: &str) -> &str {
	let end = text
		.char_indices()
		.find(|(_, c)| !c.is_whitespace())
		.map(|(i, _)| i)
		.unwrap_or(text.len());
	&text[..end]
}

fn first_non_whitespace_index(text: &str) -> Option<usize> {
	text.char_indices()
		.find(|(_, c)| !c.is_whitespace())
		.map(|(i, _)| i)
}

fn is_whitespace_only(text: &str) -> bool {
	text.chars().all(|c| c.is_whitespace())
}

fn should_increase(rules: &IndentRules, text: &str) -> bool {
	rules.increase.as_ref().is_some_and(|re| re.is_match(text))
}

fn should_decrease(rules: &IndentRules, text: &str) -> bool {
	rules.decrease.as_ref().is_some_and(|re| re.is_match(text))
}

fn should_indent_next_line(rules: &IndentRules, text: &str) -> bool {
	rules
		.indent_next_line
		.as_ref()
		.is_some_and(|re| re.is_match(text))
}

fn should_ignore(rules: &IndentRules, text: &str) -> bool {
	rules.un_indented.as_ref().is_some_and(|re| re.is_match(text))
}

const INCREASE_MASK: u8 = 0b0000_0001;
const DECREASE_MASK: u8 = 0b0000_0010;
const INDENT_NEXTLINE_MASK: u8 = 0b0000_0100;
const UNINDENT_MASK: u8 = 0b0000_1000;

fn get_indent_metadata(rules: &IndentRules, text: &str) -> u8 {
	let mut ret = 0;
	if should_increase(rules, text) {
		ret |= INCREASE_MASK;
	}
	if should_decrease(rules, text) {
		ret |= DECREASE_MASK;
	}
	if should_indent_next_line(rules, text) {
		ret |= INDENT_NEXTLINE_MASK;
	}
	if should_ignore(rules, text) {
		ret |= UNINDENT_MASK;
	}
	ret
}

/// ProcessedIndentRulesSupport：先按处理过的行文本判断。
fn processed_should_increase(model: &dyn Model, rules: &IndentRules, line: usize) -> bool {
	should_increase(rules, &model.processed_line(line))
}

fn processed_should_decrease(model: &dyn Model, rules: &IndentRules, line: usize) -> bool {
	should_decrease(rules, &model.processed_line(line))
}

fn processed_should_indent_next_line(
	model: &dyn Model,
	rules: &IndentRules,
	line: usize,
) -> bool {
	should_indent_next_line(rules, &model.processed_line(line))
}

fn processed_should_ignore(model: &dyn Model, rules: &IndentRules, line: usize) -> bool {
	should_ignore(rules, &model.processed_line(line))
}

fn get_indentation_at_position(raw_line: &str, column: usize) -> String {
	let indentation = leading_whitespace(raw_line);
	indentation.chars().take(column - 1).collect()
}

/// ShiftCommand.shiftIndent / unshiftIndent。solid 用制表符缩进（insertSpaces = false），
/// 但两条分支照抄。
fn shift_indent(indentation: &str, tab_size: usize, indent_size: usize, insert_spaces: bool) -> String {
	let content_start_visible_column = visible_column_from_column(indentation);
	if insert_spaces {
		let desired_tab_stop = next_render_tab_stop(content_start_visible_column, indent_size);
		let indent_count = desired_tab_stop / indent_size;
		" ".repeat(indent_count * indent_size)
	} else {
		let desired_tab_stop = next_render_tab_stop(content_start_visible_column, tab_size);
		let indent_count = desired_tab_stop / tab_size;
		"\t".repeat(indent_count)
	}
}

fn unshift_indent(indentation: &str, tab_size: usize, indent_size: usize, insert_spaces: bool) -> String {
	let content_start_visible_column = visible_column_from_column(indentation);
	if insert_spaces {
		let desired_tab_stop = prev_render_tab_stop(content_start_visible_column, indent_size);
		let indent_count = desired_tab_stop / indent_size;
		" ".repeat(indent_count * indent_size)
	} else {
		let desired_tab_stop = prev_render_tab_stop(content_start_visible_column, tab_size);
		let indent_count = desired_tab_stop / tab_size;
		"\t".repeat(indent_count)
	}
}

/// CursorColumns.visibleColumnFromColumn(line, line.length + 1, tabSize) 对全空白行。
fn visible_column_from_column(line: &str) -> usize {
	let mut result = 0;
	for c in line.chars() {
		if c == '\t' {
			result = next_render_tab_stop(result, 4);
		} else {
			result += 1;
		}
	}
	result
}

fn next_render_tab_stop(visible_column: usize, tab_size: usize) -> usize {
	visible_column + tab_size - visible_column % tab_size
}

fn prev_render_tab_stop(column: usize, tab_size: usize) -> usize {
	column.saturating_sub(1 + (column.saturating_sub(1)) % tab_size)
}

/// misc/indentation.ts: normalizeIndentation。
pub fn normalize_indentation(text: &str, indent_size: usize, insert_spaces: bool) -> String {
	let first = first_non_whitespace_index(text).unwrap_or(text.len());
	let whitespace = &text[..first];
	let rest = &text[first..];
	let mut spaces_cnt = 0;
	for c in whitespace.chars() {
		if c == '\t' {
			spaces_cnt = next_render_tab_stop(spaces_cnt, indent_size);
		} else {
			spaces_cnt += 1;
		}
	}
	let mut result = String::new();
	if !insert_spaces {
		let tabs_cnt = spaces_cnt / indent_size;
		spaces_cnt %= indent_size;
		result.push_str(&"\t".repeat(tabs_cnt));
	}
	result.push_str(&" ".repeat(spaces_cnt));
	result.push_str(rest);
	result
}

struct InheritIndent {
	indentation: String,
	action: Option<IndentAction>,
	line: Option<usize>,
}

/// autoIndent.ts: getPrecedingValidLine。单语言文件，语言边界分支只剩“行号到 1 结束”。
fn get_preceding_valid_line(model: &dyn Model, rules: &IndentRules, line: usize) -> isize {
	if line > 1 {
		let mut result_line = -1isize;
		for last in (1..line).rev() {
			let text = model.line(last);
			if processed_should_ignore(model, rules, last) || is_whitespace_only(&text) || text.is_empty() {
				result_line = last as isize;
				continue;
			}
			return last as isize;
		}
		result_line
	} else {
		-1
	}
}

/// autoIndent.ts: getInheritIndentForLine。
fn get_inherit_indent_for_line(
	model: &dyn Model,
	rules: &IndentRules,
	line: usize,
	honor_intentional_indent: bool,
) -> Option<InheritIndent> {
	if line <= 1 {
		return Some(InheritIndent {
			indentation: String::new(),
			action: None,
			line: None,
		});
	}

	for prior in (1..line).rev() {
		if !model.line(prior).is_empty() {
			break;
		}
		if prior == 1 {
			return Some(InheritIndent {
				indentation: String::new(),
				action: None,
				line: None,
			});
		}
	}

	let preceding = get_preceding_valid_line(model, rules, line);
	if preceding < 0 {
		return None;
	} else if preceding < 1 {
		return Some(InheritIndent {
			indentation: String::new(),
			action: None,
			line: None,
		});
	}
	let preceding = preceding as usize;

	if processed_should_increase(model, rules, preceding)
		|| processed_should_indent_next_line(model, rules, preceding)
	{
		let content = model.line(preceding);
		return Some(InheritIndent {
			indentation: leading_whitespace(&content).to_string(),
			action: Some(IndentAction::Indent),
			line: Some(preceding),
		});
	} else if processed_should_decrease(model, rules, preceding) {
		let content = model.line(preceding);
		return Some(InheritIndent {
			indentation: leading_whitespace(&content).to_string(),
			action: None,
			line: Some(preceding),
		});
	}

	if preceding == 1 {
		return Some(InheritIndent {
			indentation: leading_whitespace(&model.line(preceding)).to_string(),
			action: None,
			line: Some(preceding),
		});
	}

	let previous_line = preceding - 1;
	let previous_metadata = get_indent_metadata(rules, &model.line(previous_line));
	if previous_metadata & (INCREASE_MASK | DECREASE_MASK) == 0
		&& previous_metadata & INDENT_NEXTLINE_MASK != 0
	{
		let mut stop_line = 0;
		for i in (1..previous_line).rev() {
			if processed_should_indent_next_line(model, rules, i) {
				continue;
			}
			stop_line = i;
			break;
		}
		return Some(InheritIndent {
			indentation: leading_whitespace(&model.line(stop_line + 1)).to_string(),
			action: None,
			line: Some(stop_line + 1),
		});
	}

	if honor_intentional_indent {
		return Some(InheritIndent {
			indentation: leading_whitespace(&model.line(preceding)).to_string(),
			action: None,
			line: Some(preceding),
		});
	}

	for i in (1..=preceding).rev() {
		if processed_should_increase(model, rules, i) {
			return Some(InheritIndent {
				indentation: leading_whitespace(&model.line(i)).to_string(),
				action: Some(IndentAction::Indent),
				line: Some(i),
			});
		} else if processed_should_indent_next_line(model, rules, i) {
			let mut stop_line = 0;
			// 原文这里内层判断的也是 i（autoIndent.ts 第 189 行），照抄。
			for j in (1..i).rev() {
				if processed_should_indent_next_line(model, rules, i) {
					continue;
				}
				stop_line = j;
				break;
			}
			return Some(InheritIndent {
				indentation: leading_whitespace(&model.line(stop_line + 1)).to_string(),
				action: None,
				line: Some(stop_line + 1),
			});
		} else if processed_should_decrease(model, rules, i) {
			return Some(InheritIndent {
				indentation: leading_whitespace(&model.line(i)).to_string(),
				action: None,
				line: Some(i),
			});
		}
	}

	Some(InheritIndent {
		indentation: leading_whitespace(&model.line(1)).to_string(),
		action: None,
		line: Some(1),
	})
}

struct VirtualModel<'a> {
	inner: &'a dyn Model,
	row: usize,
	text: String,
}

impl Model for VirtualModel<'_> {
	fn line(&self, line: usize) -> String {
		if line == self.row {
			self.text.clone()
		} else {
			self.inner.line(line)
		}
	}

	fn processed_line(&self, line: usize) -> String {
		if line == self.row {
			self.text.clone()
		} else {
			self.inner.processed_line(line)
		}
	}

	fn processed_before(&self, line: usize, column: usize) -> String {
		self.inner.processed_before(line, column)
	}

	fn processed_after(&self, line: usize, column: usize) -> String {
		self.inner.processed_after(line, column)
	}

	fn scope_before(&self, line: usize, column: usize) -> ScopeKind {
		self.inner.scope_before(line, column)
	}

	fn unmatched_closing_after(&self, line: usize, column: usize, open: &str) -> bool {
		self.inner.unmatched_closing_after(line, column, open)
	}
}

const TAB_SIZE: usize = 4;
const INDENT_SIZE: usize = 4;
const INSERT_SPACES: bool = false;

pub fn normalize_indent(text: &str) -> String {
	normalize_indentation(text, INDENT_SIZE, INSERT_SPACES)
}

fn shift(indentation: &str) -> String {
	shift_indent(indentation, TAB_SIZE, INDENT_SIZE, INSERT_SPACES)
}

fn unshift(indentation: &str) -> String {
	unshift_indent(indentation, TAB_SIZE, INDENT_SIZE, INSERT_SPACES)
}

/// enterAction.ts: getEnterAction。`start_*` / `end_*` 是即将被换行替换的 Range（1-based），
/// after 文本取 Range 末尾之后（IndentationContextProcessor._getProcessedTokensAfterRange）。
pub fn get_enter_action(
	model: &dyn Model,
	config: &LanguageConfiguration,
	start_line: usize,
	start_column: usize,
	end_line: usize,
	end_column: usize,
) -> Option<EnterAction> {
	let support = &config.on_enter_support;
	let previous = if start_line > 1 {
		model.processed_line(start_line - 1)
	} else {
		String::new()
	};
	let before = model.processed_before(start_line, start_column);
	let after = if start_line == end_line && start_column == end_column {
		model.processed_after(start_line, start_column)
	} else {
		model.processed_after(end_line, end_column)
	};
	let rule = support.on_enter(&previous, &before, &after)?;

	let mut append_text = rule.append_text;
	if append_text.is_empty() {
		if matches!(
			rule.indent_action,
			IndentAction::Indent | IndentAction::IndentOutdent
		) {
			append_text = "\t".to_string();
		}
	} else if rule.indent_action == IndentAction::Indent {
		append_text.insert(0, '\t');
	}

	let mut indentation = get_indentation_at_position(&model.line(start_line), start_column);
	let remove_text = rule.remove_text;
	if remove_text > 0 {
		let len = indentation.chars().count();
		indentation = indentation.chars().take(len.saturating_sub(remove_text)).collect();
	}

	Some(EnterAction {
		indent_action: rule.indent_action,
		append_text,
		remove_text,
		indentation,
	})
}

/// autoIndent.ts: getIndentForEnter。before 取 start 行的 start 列之前，after 取 end 行 end 列之后。
fn get_indent_for_enter(
	model: &dyn Model,
	rules: &IndentRules,
	start_line: usize,
	start_column: usize,
	end_line: usize,
	end_column: usize,
) -> Option<(String, String)> {
	let before = model.processed_before(start_line, start_column);
	let after = if start_line == end_line && start_column == end_column {
		model.processed_after(start_line, start_column)
	} else {
		model.processed_after(end_line, end_column)
	};
	let before_enter_indent = leading_whitespace(&before).to_string();

	let virtual_model = VirtualModel {
		inner: model,
		row: start_line,
		text: before,
	};
	let after_enter_action = get_inherit_indent_for_line(&virtual_model, rules, start_line + 1, true);

	let after_enter_action = match after_enter_action {
		None => {
			return Some((before_enter_indent.clone(), before_enter_indent));
		}
		Some(action) => action,
	};

	let mut after_enter_indent = after_enter_action.indentation.clone();
	if after_enter_action.action == Some(IndentAction::Indent) {
		after_enter_indent = shift(&after_enter_indent);
	}

	if should_decrease(rules, &after) {
		after_enter_indent = unshift(&after_enter_indent);
	}

	Some((before_enter_indent, after_enter_indent))
}

/// cursorTypeEditOperations.ts: EnterOperation._enter 的结果。
/// 把 `from`（1-based 行、列）到 `to`（1-based 行、列，不含终点）替换成 `text`；
/// 光标放在文本末尾再按 `line_delta`、`column_delta` 偏移。
#[derive(Debug, PartialEq, Eq)]
pub struct EnterEdit {
	pub from_line: usize,
	pub from_column: usize,
	pub to_line: usize,
	pub to_column: usize,
	pub text: String,
	pub line_delta: isize,
	pub column_delta: isize,
}

pub fn get_enter_edit(
	model: &dyn Model,
	config: &LanguageConfiguration,
	start_line: usize,
	start_column: usize,
	end_line: usize,
	end_column: usize,
) -> EnterEdit {
	let simple = |text: String| EnterEdit {
		from_line: start_line,
		from_column: start_column,
		to_line: end_line,
		to_column: end_column,
		text,
		line_delta: 0,
		column_delta: 0,
	};

	if let Some(r) = get_enter_action(model, config, start_line, start_column, end_line, end_column) {
		match r.indent_action {
			IndentAction::None | IndentAction::Indent => {
				let text = format!("\n{}", normalize_indent(&(r.indentation + &r.append_text)));
				return simple(text);
			}
			IndentAction::IndentOutdent => {
				let normal_indent = normalize_indent(&r.indentation);
				let increased_indent = normalize_indent(&(r.indentation + &r.append_text));
				let text = format!("\n{increased_indent}\n{normal_indent}");
				return EnterEdit {
					line_delta: -1,
					column_delta: increased_indent.chars().count() as isize
						- normal_indent.chars().count() as isize,
					..simple(text)
				};
			}
			IndentAction::Outdent => {
				let actual = unshift(&r.indentation);
				let text = format!("\n{}", normalize_indent(&(actual + &r.append_text)));
				return simple(text);
			}
		}
	}

	if let Some(rules) = &config.indentation {
		if let Some((_before_enter, after_enter)) =
			get_indent_for_enter(model, rules, start_line, start_column, end_line, end_column)
		{
			let raw_end_line = model.line(end_line);
			let mut old_end_view_column = visible_column_of_position(&raw_end_line, end_column);
			let first_non_whitespace = first_non_whitespace_index(&raw_end_line);
			let to_column = match first_non_whitespace {
				Some(index) => end_column.max(index + 1),
				None => raw_end_line.chars().count() + 1,
			};
			let normalized_after = normalize_indent(&after_enter);
			let offset = if first_non_whitespace.is_some_and(|index| end_column <= index + 1) {
				if !INSERT_SPACES {
					old_end_view_column = old_end_view_column.div_ceil(INDENT_SIZE);
				}
				(old_end_view_column as isize
					+ 1
					- normalized_after.chars().count() as isize
					- 1)
					.min(0)
			} else {
				0
			};
			return EnterEdit {
				from_line: start_line,
				from_column: start_column,
				to_line: end_line,
				to_column,
				text: format!("\n{normalized_after}"),
				line_delta: 0,
				column_delta: offset,
			};
		}
	}

	let indentation = get_indentation_at_position(&model.line(start_line), start_column);
	simple(format!("\n{}", normalize_indent(&indentation)))
}

fn visible_column_of_position(line: &str, column: usize) -> usize {
	let text: String = line.chars().take(column - 1).collect();
	visible_column_from_column(&text)
}

/// autoIndent.ts: getIndentActionForType。返回新的整行前导缩进。
pub fn get_indent_action_for_type(
	model: &dyn Model,
	config: &LanguageConfiguration,
	line: usize,
	column: usize,
	ch: &str,
) -> Option<String> {
	let rules = config.indentation.as_ref()?;
	let before = model.processed_before(line, column);
	let after = model.processed_after(line, column);
	let text_around = format!("{before}{after}");
	let text_around_with_ch = format!("{before}{ch}{after}");

	if !should_decrease(rules, &text_around) && should_decrease(rules, &text_around_with_ch) {
		let r = get_inherit_indent_for_line(model, rules, line, false)?;
		let mut indentation = r.indentation;
		if r.action != Some(IndentAction::Indent) {
			indentation = unshift(&indentation);
		}
		return Some(indentation);
	}

	let previous_line_number = line as isize - 1;
	if previous_line_number > 0 {
		let previous_line = model.line(previous_line_number as usize);
		if should_indent_next_line(rules, &previous_line) && should_increase(rules, &text_around_with_ch)
		{
			if let Some(inherited) = get_inherit_indent_for_line(model, rules, line, false) {
				let current_line = model.line(line);
				let actual_current_indentation = leading_whitespace(&current_line).to_string();
				let inferred_current_indentation = shift(&inherited.indentation);
				let inferred_equals_actual = inferred_current_indentation == actual_current_indentation;
				let text_is_only_whitespace = text_around.chars().all(|c| c.is_whitespace());
				let ch_is_auto_closing_open = config.auto_closing.iter().any(|p| p.open.ends_with(ch));
				if inferred_equals_actual && ch_is_auto_closing_open && text_is_only_whitespace {
					return Some(inherited.indentation);
				}
			}
		}
	}

	None
}

/// cursorTypeEditOperations.ts: AutoClosingOpenCharTypeOperation.getAutoClosingPair。
/// 单光标版本；ch 是正在输入的字符，尚未进入缓冲区。
pub fn get_auto_closing_pair(
	model: &dyn Model,
	config: &LanguageConfiguration,
	line: usize,
	column: usize,
	ch: &str,
) -> Option<(String, String)> {
	let pair = find_auto_closing_pair_open(model, config, line, column, ch)?;

	let ch_is_quote = is_quote(ch);
	let (auto_close_before, should_check_bracket_balance) = if ch_is_quote {
		(config.auto_close_before_chars(true), false)
	} else {
		let pair_is_for_comments = config
			.block_comment_start
			.as_ref()
			.is_some_and(|start| pair.open.contains(start.as_str()));
		if pair_is_for_comments {
			// cursorCommon.ts: shouldAutoCloseBefore.comment 用的是 forQuotes = false 的那一组。
			(config.auto_close_before_chars(false), false)
		} else {
			(config.auto_close_before_chars(false), true)
		}
	};

	// 默认 autoClosingBrackets / autoClosingQuotes / autoClosingComments 都是 languageDefined，
	// 只有 'never' / 'always' 会改变行为；这里按 languageDefined 转写。

	let contained_pair_close = find_contained_auto_closing_pair(config, &pair)
		.map(|p| p.close.clone())
		.unwrap_or_default();
	let mut is_contained_pair_present = true;

	let line_text: String = model.line(line).chars().collect();
	let line_before: String = line_text.chars().take(column - 1).collect();
	let line_after: String = line_text.chars().skip(column - 1).collect();

	if !line_after.starts_with(&contained_pair_close) {
		is_contained_pair_present = false;
	}
	if let Some(character_after) = line_after.chars().next() {
		let is_before_close_brace = is_before_closing_brace(config, &line_after);
		if !is_before_close_brace && !auto_close_before.contains(character_after) {
			return None;
		}
	}
	if should_check_bracket_balance && model.unmatched_closing_after(line, column, &pair.open) {
		return None;
	}
	if pair.open.chars().count() == 1 && (ch == "'" || ch == "\"") {
		if let Some(character_before) = line_before.chars().last() {
			if is_regular_word_character(character_before) {
				return None;
			}
		}
	}
	if !pair.is_ok(model.scope_before(line, column)) {
		return None;
	}
	// 未转写：中性字符试探（getTokenTypeIfInsertingCharacter）。作用域在插入点前都已判断过
	// 字符串/注释，剩下的“引号会不会结束当前字符串”由 tree-sitter 的字符串作用域承担。

	if is_contained_pair_present {
		let close_len = pair.close.chars().count() - contained_pair_close.chars().count();
		let close: String = pair.close.chars().take(close_len).collect();
		Some((pair.open.clone(), close))
	} else {
		Some((pair.open.clone(), pair.close.clone()))
	}
}

fn is_regular_word_character(c: char) -> bool {
	!USUAL_WORD_SEPARATORS.contains(c) && !c.is_whitespace()
}

fn find_auto_closing_pair_open<'a>(
	model: &dyn Model,
	config: &'a LanguageConfiguration,
	line: usize,
	column: usize,
	ch: &str,
) -> Option<&'a AutoClosingPair> {
	let mut result: Option<&AutoClosingPair> = None;
	for candidate in &config.auto_closing {
		if !candidate.open.ends_with(ch) {
			continue;
		}
		if result.is_some_and(|r| candidate.open.chars().count() <= r.open.chars().count()) {
			continue;
		}
		let open_len = candidate.open.chars().count();
		let line_text = model.line(line);
		// Range(line, column - open_len + 1, line, column)：取 open 去掉末字符的部分。
		let start = column.saturating_sub(open_len);
		let relevant: String = line_text
			.chars()
			.skip(start)
			.take(open_len - 1)
			.collect();
		if relevant + ch != candidate.open {
			continue;
		}
		if candidate.before_text.is_some() {
			let prefix: String = line_text.chars().take(column - 1).collect();
			if !candidate.should_auto_close_before(&(prefix + ch)) {
				continue;
			}
		}
		result = Some(candidate);
	}
	result
}

fn find_contained_auto_closing_pair<'a>(
	config: &'a LanguageConfiguration,
	pair: &AutoClosingPair,
) -> Option<&'a AutoClosingPair> {
	if pair.open.chars().count() <= 1 {
		return None;
	}
	let last_char = pair.close.chars().last()?;
	let mut result: Option<&AutoClosingPair> = None;
	for candidate in &config.auto_closing {
		if !candidate.close.ends_with(last_char) {
			continue;
		}
		if candidate.open != pair.open
			&& pair.open.contains(candidate.open.as_str())
			&& pair.close.ends_with(candidate.close.as_str())
		{
			if result.is_none_or(|r| {
				candidate.open.chars().count() > r.open.chars().count()
			}) {
				result = Some(candidate);
			}
		}
	}
	result
}

fn is_before_closing_brace(config: &LanguageConfiguration, line_after: &str) -> bool {
	let Some(next_char) = line_after.chars().next() else {
		return false;
	};
	let is_before_starting_brace = config
		.auto_closing
		.iter()
		.any(|p| p.open.starts_with(next_char) && line_after.starts_with(p.open.as_str()));
	let is_before_closing_brace = config
		.auto_closing
		.iter()
		.any(|p| p.close.starts_with(next_char) && line_after.starts_with(p.close.as_str()));
	!is_before_starting_brace && is_before_closing_brace
}

/// cursorCommon.ts: isQuote。
pub fn is_quote(ch: &str) -> bool {
	ch == "'" || ch == "\"" || ch == "`"
}

/// cursorTypeEditOperations.ts: isAutoClosingOvertype（autoClosingOvertype 默认 'auto'）。
/// `auto_closed` 是自动插入的闭合符起点（0-based 行列）。
pub fn is_auto_closing_overtype(
	config: &LanguageConfiguration,
	line_text: &str,
	column: usize,
	ch: &str,
	auto_closed: &[(usize, usize)],
	line: usize,
) -> bool {
	if !config
		.auto_closing
		.iter()
		.any(|p| p.open.chars().count() == 1 && p.close.chars().count() == 1 && p.close == ch)
	{
		return false;
	}
	let after_character = line_text.chars().nth(column - 1);
	if after_character != ch.chars().next() {
		return false;
	}
	if is_quote(ch) && column > 2 && line_text.chars().nth(column - 2) == Some('\\') {
		return false;
	}
	auto_closed.contains(&(line - 1, column - 1))
}

/// 退格删掉整对括号（DeleteOperations.isAutoClosingPairDelete，源码不在 vendor 内，
/// 按安装版 workbench.desktop.main.js 转写）。autoClosingDelete 默认 'auto'：
/// 只有闭合符是我们自动插入的、且光标正停在它前面时才删一对。
pub fn is_auto_closing_pair_delete(
	config: &LanguageConfiguration,
	line_text: &str,
	column: usize,
	auto_closed: &[(usize, usize)],
	line: usize,
) -> bool {
	let chars: Vec<char> = line_text.chars().collect();
	if column < 2 || column >= chars.len() + 1 {
		return false;
	}
	let before = chars[column - 2].to_string();
	let after = chars[column - 1].to_string();
	if !config
		.auto_closing
		.iter()
		.any(|pair| pair.open == before && pair.close == after)
	{
		return false;
	}
	auto_closed.contains(&(line - 1, column - 1))
}

/// cursorTypeEditOperations.ts: SurroundSelectionOperation._isSurroundSelectionType。
/// `lines` 是被选中的行文本（1-based 行对应的原始内容），start/end 是选区（1-based）。
pub fn is_surround_selection_type(
	config: &LanguageConfiguration,
	lines: &[String],
	start_line: usize,
	start_column: usize,
	end_line: usize,
	end_column: usize,
	ch: &str,
) -> bool {
	// autoSurround 默认 languageDefined，shouldSurroundChar 恒真。
	if !config.surrounding.iter().any(|(open, _)| open == ch) {
		return false;
	}
	let is_typing_a_quote = is_quote(ch);
	let mut selection_contains_only_whitespace = true;
	for line_number in start_line..=end_line {
		let line_text = &lines[line_number - 1];
		let start_index = if line_number == start_line {
			start_column - 1
		} else {
			0
		};
		let end_index = if line_number == end_line {
			end_column - 1
		} else {
			line_text.chars().count()
		};
		let selected: String = line_text
			.chars()
			.skip(start_index)
			.take(end_index.saturating_sub(start_index))
			.collect();
		if selected.chars().any(|c| c != ' ' && c != '\t') {
			selection_contains_only_whitespace = false;
			break;
		}
	}
	if selection_contains_only_whitespace {
		return false;
	}
	if is_typing_a_quote && start_line == end_line && start_column + 1 == end_column {
		let selection_text: String = lines[start_line - 1]
			.chars()
			.skip(start_column - 1)
			.take(end_column - start_column)
			.collect();
		if is_quote(&selection_text) {
			return false;
		}
	}
	true
}

/// 处理过的行：字符串/注释/正则里的括号字符去掉（indentationLineProcessor.ts）。
pub fn process_line(text: &[char], kinds: &[ScopeKind], config: &LanguageConfiguration) -> String {
	let mut out = String::with_capacity(text.len());
	let mut i = 0;
	while i < text.len() {
		let kind = kinds.get(i).copied().unwrap_or(ScopeKind::Other);
		if matches!(kind, ScopeKind::String | ScopeKind::Comment | ScopeKind::Regex) {
			let rest: String = text[i..].iter().collect();
			let mut skipped = 0;
			for bracket in config
				.brackets
				.iter()
				.flat_map(|(open, close)| [open.as_str(), close.as_str()])
			{
				if !bracket.is_empty() && rest.starts_with(bracket) {
					skipped = bracket.chars().count();
					break;
				}
			}
			if skipped > 0 {
				i += skipped;
				continue;
			}
		}
		out.push(text[i]);
		i += 1;
	}
	out
}

// ---- 配置载入 ----

fn strip_jsonc(src: &str) -> String {
	let chars: Vec<char> = src.chars().collect();
	let mut out = String::with_capacity(src.len());
	let mut i = 0;
	let mut in_string = false;
	let mut escaped = false;
	while i < chars.len() {
		let c = chars[i];
		if in_string {
			out.push(c);
			if escaped {
				escaped = false;
			} else if c == '\\' {
				escaped = true;
			} else if c == '"' {
				in_string = false;
			}
			i += 1;
			continue;
		}
		match c {
			'"' => {
				in_string = true;
				out.push(c);
				i += 1;
			}
			'/' if chars.get(i + 1) == Some(&'/') => {
				while i < chars.len() && chars[i] != '\n' {
					i += 1;
				}
			}
			'/' if chars.get(i + 1) == Some(&'*') => {
				i += 2;
				while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
					i += 1;
				}
				i = (i + 2).min(chars.len());
			}
			_ => {
				out.push(c);
				i += 1;
			}
		}
	}
	strip_trailing_commas(&out)
}

fn strip_trailing_commas(src: &str) -> String {
	let chars: Vec<char> = src.chars().collect();
	let mut out = String::with_capacity(src.len());
	let mut in_string = false;
	let mut escaped = false;
	let mut i = 0;
	while i < chars.len() {
		let c = chars[i];
		if in_string {
			out.push(c);
			if escaped {
				escaped = false;
			} else if c == '\\' {
				escaped = true;
			} else if c == '"' {
				in_string = false;
			}
			i += 1;
			continue;
		}
		if c == '"' {
			in_string = true;
			out.push(c);
			i += 1;
			continue;
		}
		if c == ',' {
			let mut j = i + 1;
			while j < chars.len() && chars[j].is_whitespace() {
				j += 1;
			}
			if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
				i += 1;
				continue;
			}
		}
		out.push(c);
		i += 1;
	}
	out
}

fn pattern_of(value: Option<&Value>) -> Option<Re> {
	let text = match value? {
		Value::String(text) => text.clone(),
		Value::Object(object) => object.get("pattern")?.as_str()?.to_string(),
		_ => return None,
	};
	Re::new(&text)
}

fn pair_of(value: &Value) -> Option<(String, String)> {
	match value {
		Value::Array(items) if items.len() >= 2 => Some((
			items[0].as_str()?.to_string(),
			items[1].as_str()?.to_string(),
		)),
		Value::Object(object) => Some((
			object.get("open")?.as_str()?.to_string(),
			object.get("close")?.as_str()?.to_string(),
		)),
		_ => None,
	}
}

fn auto_closing_pair_of(value: &Value) -> Option<AutoClosingPair> {
	let (open, close) = pair_of(value)?;
	let object = value.as_object();
	let before_text = object
		.and_then(|o| o.get("beforeText"))
		.and_then(|v| pattern_of(Some(v)));
	let mut pair = AutoClosingPair::plain(&open, &close);
	pair.before_text = before_text;
	if let Some(not_in) = object.and_then(|o| o.get("notIn")).and_then(Value::as_array) {
		for item in not_in {
			match item.as_str() {
				Some("string") => pair.in_string = false,
				Some("comment") => pair.in_comment = false,
				Some("regex") => pair.in_regex = false,
				_ => {}
			}
		}
	}
	Some(pair)
}

fn parse_language(value: &Value) -> Option<LanguageConfiguration> {
	let mut config = LanguageConfiguration::default();

	if let Some(brackets) = value.get("brackets").and_then(Value::as_array) {
		config.brackets = brackets.iter().filter_map(pair_of).collect();
	}

	match value.get("autoClosingPairs").and_then(Value::as_array) {
		Some(pairs) => {
			config.auto_closing = pairs.iter().filter_map(auto_closing_pair_of).collect();
		}
		None if !config.brackets.is_empty() => {
			config.auto_closing = config
				.brackets
				.iter()
				.map(|(open, close)| AutoClosingPair::plain(open, close))
				.collect();
		}
		None => {}
	}

	config.surrounding = match value.get("surroundingPairs").and_then(Value::as_array) {
		Some(pairs) => pairs.iter().filter_map(pair_of).collect(),
		None => config
			.auto_closing
			.iter()
			.map(|pair| (pair.open.clone(), pair.close.clone()))
			.collect(),
	};

	config.auto_close_before = value
		.get("autoCloseBefore")
		.and_then(Value::as_str)
		.map(str::to_string);

	if let Some(rules) = value.get("indentationRules") {
		config.indentation = Some(IndentRules {
			increase: pattern_of(rules.get("increaseIndentPattern")),
			decrease: pattern_of(rules.get("decreaseIndentPattern")),
			indent_next_line: pattern_of(rules.get("indentNextLinePattern")),
			un_indented: pattern_of(rules.get("unIndentedLinePattern")),
		});
	}

	if let Some(rules) = value.get("onEnterRules").and_then(Value::as_array) {
		for rule in rules {
			let action = rule.get("action");
			let indent_action = match action
				.and_then(|a| a.get("indent"))
				.and_then(Value::as_str)
				.unwrap_or("none")
			{
				"indent" => IndentAction::Indent,
				"indentOutdent" => IndentAction::IndentOutdent,
				"outdent" => IndentAction::Outdent,
				_ => IndentAction::None,
			};
			config.on_enter.push(OnEnterRule {
				before: pattern_of(rule.get("beforeText")),
				after: pattern_of(rule.get("afterText")),
				previous: pattern_of(rule.get("previousLineText")),
				action: EnterRuleAction {
					indent_action,
					append_text: action
						.and_then(|a| a.get("appendText"))
						.and_then(Value::as_str)
						.unwrap_or("")
						.to_string(),
					remove_text: action
						.and_then(|a| a.get("removeText"))
						.and_then(Value::as_u64)
						.unwrap_or(0) as usize,
				},
			});
		}
	}

	config.block_comment_start = value
		.get("comments")
		.and_then(|c| c.get("blockComment"))
		.and_then(Value::as_array)
		.and_then(|items| items.first())
		.and_then(Value::as_str)
		.map(str::to_string);

	config.on_enter_support = OnEnterSupport::from_parts(
		&on_enter_brackets(value, &config.brackets),
		config.on_enter.clone(),
	);

	Some(config)
}

/// onEnter.ts 的 OnEnterSupport 在配置没写 brackets 时自带 `(){}[]`。
fn on_enter_brackets(value: &Value, parsed: &[(String, String)]) -> Vec<(String, String)> {
	if value.get("brackets").is_some() {
		parsed.to_vec()
	} else {
		vec![
			("(".to_string(), ")".to_string()),
			("{".to_string(), "}".to_string()),
			("[".to_string(), "]".to_string()),
		]
	}
}

macro_rules! config_slot {
	($lang:expr) => {{
		static SLOT: OnceLock<Option<LanguageConfiguration>> = OnceLock::new();
		SLOT.get_or_init(|| {
			let source = match $lang {
				Lang::Rust => include_str!("../../../vendor/vscode-1.145.2/language-configuration/rust/language-configuration.json"),
				Lang::C => include_str!("../../../vendor/vscode-1.145.2/language-configuration/cpp/language-configuration.json"),
				Lang::Python => include_str!("../../../vendor/vscode-1.145.2/language-configuration/python/language-configuration.json"),
				Lang::JavaScript => include_str!("../../../vendor/vscode-1.145.2/language-configuration/javascript/javascript-language-configuration.json"),
				Lang::Json => include_str!("../../../vendor/vscode-1.145.2/language-configuration/json/language-configuration.json"),
				Lang::Plain => return None,
			};
			let json = strip_jsonc(source);
			let value: Value = serde_json::from_str(&json).ok()?;
			parse_language(&value)
		})
		.as_ref()
	}};
}

pub fn configuration(lang: Lang) -> Option<&'static LanguageConfiguration> {
	match lang {
		Lang::Rust => config_slot!(Lang::Rust),
		Lang::C => config_slot!(Lang::C),
		Lang::Python => config_slot!(Lang::Python),
		Lang::JavaScript => config_slot!(Lang::JavaScript),
		Lang::Json => config_slot!(Lang::Json),
		Lang::Plain => None,
	}
}

#[cfg(all(test, not(miri)))]
mod tests {
	use super::*;

	struct TestModel {
		lines: Vec<String>,
		config: &'static LanguageConfiguration,
		scopes: Vec<Vec<ScopeKind>>,
	}

	impl TestModel {
		fn new(lines: &[&str], config: &'static LanguageConfiguration) -> Self {
			Self {
				lines: lines.iter().map(|s| s.to_string()).collect(),
				config,
				scopes: lines
					.iter()
					.map(|line| vec![ScopeKind::Other; line.chars().count()])
					.collect(),
			}
		}

		fn with_scopes(lines: &[&str], config: &'static LanguageConfiguration, scopes: Vec<Vec<ScopeKind>>) -> Self {
			Self {
				lines: lines.iter().map(|s| s.to_string()).collect(),
				config,
				scopes,
			}
		}
	}

	fn processed(model: &TestModel, line: usize) -> String {
		let row = line - 1;
		let chars: Vec<char> = model.lines[row].chars().collect();
		process_line(&chars, &model.scopes[row], model.config)
	}

	impl Model for TestModel {
		fn line(&self, line: usize) -> String {
			self.lines[line - 1].clone()
		}

		fn processed_line(&self, line: usize) -> String {
			processed(self, line)
		}

		fn processed_before(&self, line: usize, column: usize) -> String {
			let chars: Vec<char> = self.lines[line - 1].chars().take(column - 1).collect();
			process_line(&chars, &self.scopes[line - 1][..chars.len().min(self.scopes[line - 1].len())], self.config)
		}

		fn processed_after(&self, line: usize, column: usize) -> String {
			let chars: Vec<char> = self.lines[line - 1].chars().skip(column - 1).collect();
			let start = (column - 1).min(self.scopes[line - 1].len());
			let kinds = &self.scopes[line - 1][start..];
			process_line(&chars, kinds, self.config)
		}

		fn scope_before(&self, line: usize, column: usize) -> ScopeKind {
			if column < 2 {
				return ScopeKind::Other;
			}
			self.scopes[line - 1]
				.get(column - 2)
				.copied()
				.unwrap_or(ScopeKind::Other)
		}

		fn unmatched_closing_after(&self, _line: usize, _column: usize, _open: &str) -> bool {
			false
		}
	}

	fn rust_config() -> &'static LanguageConfiguration {
		configuration(Lang::Rust).expect("rust config")
	}

	#[test]
	fn all_shipped_configs_parse() {
		for lang in [Lang::Rust, Lang::C, Lang::Python, Lang::JavaScript, Lang::Json] {
			let config = configuration(lang).unwrap_or_else(|| panic!("{} config", lang.name()));
			assert!(!config.auto_closing.is_empty(), "{} autoClosingPairs", lang.name());
			assert!(!config.brackets.is_empty(), "{} brackets", lang.name());
			assert!(
				config.indentation.is_some() || !config.on_enter.is_empty(),
				"{} indentationRules",
				lang.name()
			);
		}
	}

	fn on_enter_brackets() -> OnEnterSupport {
		OnEnterSupport::from_parts(
			&[("(".to_string(), ")".to_string()), ("begin".to_string(), "end".to_string())],
			Vec::new(),
		)
	}

	// vendor/vscode-1.145.2/tests/onEnter.test.ts: uses brackets
	#[test]
	fn on_enter_uses_brackets() {
		let support = on_enter_brackets();
		let cases = [
			("a", "", None),
			("", "b", None),
			("(", "b", Some(IndentAction::Indent)),
			("a", ")", None),
			("begin", "ending", Some(IndentAction::Indent)),
			("abegin", "end", None),
			("begin", ")", Some(IndentAction::Indent)),
			("begin", "end", Some(IndentAction::IndentOutdent)),
			("begin ", " end", Some(IndentAction::IndentOutdent)),
			(" begin", "end//as", Some(IndentAction::IndentOutdent)),
			("(", ")", Some(IndentAction::IndentOutdent)),
			("( ", ")", Some(IndentAction::IndentOutdent)),
			("a(", ")b", Some(IndentAction::IndentOutdent)),
			("(", "", Some(IndentAction::Indent)),
			("(", "foo", Some(IndentAction::Indent)),
			("begin", "foo", Some(IndentAction::Indent)),
			("begin", "", Some(IndentAction::Indent)),
		];
		for (before, after, expected) in cases {
			let actual = support.on_enter("", before, after);
			match expected {
				None => assert!(actual.is_none(), "before={before:?} after={after:?}"),
				Some(action) => assert_eq!(
					actual.map(|a| a.indent_action),
					Some(action),
					"before={before:?} after={after:?}"
				),
			}
		}
	}

	// vendor/vscode-1.145.2/tests/onEnter.test.ts: Issue #121125
	#[test]
	fn on_enter_rule_with_global_pattern() {
		let support = OnEnterSupport::from_parts(
			&[],
			vec![OnEnterRule {
				before: Some(Re::new("^\\s*\\/{3}.*$").unwrap()),
				after: None,
				previous: None,
				action: EnterRuleAction {
					indent_action: IndentAction::Outdent,
					append_text: "/// ".to_string(),
					remove_text: 0,
				},
			}],
		);
		for _ in 0..2 {
			let actual = support.on_enter("/// line", "/// line", "").expect("rule");
			assert_eq!(actual.indent_action, IndentAction::Outdent);
			assert_eq!(actual.append_text, "/// ");
		}
	}

	fn js_on_enter_rules() -> OnEnterSupport {
		let rule = |before: Option<&str>, after: Option<&str>, previous: Option<&str>, action: EnterRuleAction| OnEnterRule {
			before: before.map(|p| Re::new(p).unwrap()),
			after: after.map(|p| Re::new(p).unwrap()),
			previous: previous.map(|p| Re::new(p).unwrap()),
			action,
		};
		let none = |append: &str| EnterRuleAction {
			indent_action: IndentAction::None,
			append_text: append.to_string(),
			remove_text: 0,
		};
		// OnEnterSupport 的默认括号：配置只给 onEnterRules 时仍有 (){}[]
		OnEnterSupport::from_parts(
			&[
				("(".to_string(), ")".to_string()),
				("{".to_string(), "}".to_string()),
				("[".to_string(), "]".to_string()),
			],
			vec![
				// vendor/vscode-1.145.2/tests/onEnterRules.ts: javascriptOnEnterRules
				rule(
					Some("^\\s*\\/\\*\\*(?!\\/)([^\\*]|\\*(?!\\/))*$"),
					Some("^\\s*\\*\\/$"),
					None,
					EnterRuleAction {
						indent_action: IndentAction::IndentOutdent,
						append_text: " * ".to_string(),
						remove_text: 0,
					},
				),
				rule(Some("^\\s*\\/\\*\\*(?!\\/)([^\\*]|\\*(?!\\/))*$"), None, None, none(" * ")),
				rule(
					Some("^(\\t|[ ])*[ ]\\*([ ]([^\\*]|\\*(?!\\/))*)?$"),
					None,
					Some("(?=^(\\s*(\\/\\*\\*|\\*)).*)(?=(?!(\\s*\\*\\/)))"),
					none("* "),
				),
				rule(
					Some("^(\\t|[ ])*[ ]\\*\\/\\s*$"),
					None,
					None,
					EnterRuleAction {
						indent_action: IndentAction::None,
						append_text: String::new(),
						remove_text: 1,
					},
				),
				rule(
					Some("^(\\t|[ ])*[ ]\\*[^/]*\\*\\/\\s*$"),
					None,
					None,
					EnterRuleAction {
						indent_action: IndentAction::None,
						append_text: String::new(),
						remove_text: 1,
					},
				),
				rule(
					Some("^\\s*(\\bcase\\s.+:|\\bdefault:)$"),
					Some("^(?!\\s*(\\bcase\\b|\\bdefault\\b))"),
					None,
					EnterRuleAction::new(IndentAction::Indent),
				),
				rule(
					Some("^\\s+([^{i\\s]|i(?!f\\b))"),
					None,
					Some("^\\s*(((else ?)?if|for|while)\\s*\\(.*\\)\\s*|else\\s*)$"),
					EnterRuleAction::new(IndentAction::Outdent),
				),
				rule(Some("^.*\\([^\\)]*$"), Some("^\\s*\\).*$"), None, none("\t")),
				rule(Some("^.*\\{[^\\}]*$"), Some("^\\s*\\}.*$"), None, none("\t")),
				rule(Some("^.*\\[[^\\]]*$"), Some("^\\s*\\].*$"), None, none("\t")),
			],
		)
	}

	// vendor/vscode-1.145.2/tests/onEnter.test.ts: uses regExpRules
	#[test]
	fn on_enter_uses_reg_exp_rules() {
		let support = js_on_enter_rules();
		let cases: &[(&str, &str, &str, Option<IndentAction>, Option<&str>, usize)] = &[
			("", "\t/**", " */", Some(IndentAction::IndentOutdent), Some(" * "), 0),
			("", "\t/**", "", Some(IndentAction::None), Some(" * "), 0),
			("", "\t/** * / * / * /", "", Some(IndentAction::None), Some(" * "), 0),
			("", "\t/** /*", "", Some(IndentAction::None), Some(" * "), 0),
			("", "/**", "", Some(IndentAction::None), Some(" * "), 0),
			("", "\t/**/", "", None, None, 0),
			("", "\t/***/", "", None, None, 0),
			("", "\t/*******/", "", None, None, 0),
			("", "\t/** * * * * */", "", None, None, 0),
			("", "\t/** */", "", None, None, 0),
			("", "\t/** asdfg */", "", None, None, 0),
			("", "\t/* asdfg */", "", None, None, 0),
			("", "*/", "", None, None, 0),
			("", "\t/*", "", None, None, 0),
			("", "\t*", "", None, None, 0),
			("\t/**", "\t *", "", Some(IndentAction::None), Some("* "), 0),
			("\t * something", "\t *", "", Some(IndentAction::None), Some("* "), 0),
			("\t *", "\t *", "", Some(IndentAction::None), Some("* "), 0),
			("", "\t */", "", Some(IndentAction::None), None, 1),
			("", "\t * */", "", Some(IndentAction::None), None, 1),
			("", "\t * * / * / * / */", "", None, None, 0),
			("\t/**", "\t * ", "", Some(IndentAction::None), Some("* "), 0),
			("\t * something", "\t * ", "", Some(IndentAction::None), Some("* "), 0),
			("\t *", "\t * ", "", Some(IndentAction::None), Some("* "), 0),
			("/**", " * ", "", Some(IndentAction::None), Some("* "), 0),
			(" * something", " * ", "", Some(IndentAction::None), Some("* "), 0),
			(" *", " * asdfsfagadfg", "", Some(IndentAction::None), Some("* "), 0),
			("/**", " * asdfsfagadfg * * * ", "", Some(IndentAction::None), Some("* "), 0),
			(" * something", " * asdfsfagadfg * * * ", "", Some(IndentAction::None), Some("* "), 0),
			(" *", " * asdfsfagadfg * * * ", "", Some(IndentAction::None), Some("* "), 0),
			("/**", " * /*", "", Some(IndentAction::None), Some("* "), 0),
			(" * something", " * /*", "", Some(IndentAction::None), Some("* "), 0),
			(" *", " * /*", "", Some(IndentAction::None), Some("* "), 0),
			("/**", " * asdfsfagadfg * / * / * /", "", Some(IndentAction::None), Some("* "), 0),
			(" * something", " * asdfsfagadfg * / * / * /", "", Some(IndentAction::None), Some("* "), 0),
			(" *", " * asdfsfagadfg * / * / * /", "", Some(IndentAction::None), Some("* "), 0),
			("/**", " * asdfsfagadfg * / * / * /*", "", Some(IndentAction::None), Some("* "), 0),
			(" * something", " * asdfsfagadfg * / * / * /*", "", Some(IndentAction::None), Some("* "), 0),
			(" *", " * asdfsfagadfg * / * / * /*", "", Some(IndentAction::None), Some("* "), 0),
			("", " */", "", Some(IndentAction::None), None, 1),
			(" */", " * test() {", "", Some(IndentAction::Indent), None, 0),
			("", "\t */", "", Some(IndentAction::None), None, 1),
			("", "\t\t */", "", Some(IndentAction::None), None, 1),
			("", "   */", "", Some(IndentAction::None), None, 1),
			("", "     */", "", Some(IndentAction::None), None, 1),
			("", "\t     */", "", Some(IndentAction::None), None, 1),
			("", " *--------------------------------------------------------------------------------------------*/", "", Some(IndentAction::None), None, 1),
			("class A {", "    * test() {", "", Some(IndentAction::Indent), None, 0),
			("", "    * test() {", "", Some(IndentAction::Indent), None, 0),
			("    ", "    * test() {", "", Some(IndentAction::Indent), None, 0),
			("class A {", "  * test() {", "", Some(IndentAction::Indent), None, 0),
			("", "  * test() {", "", Some(IndentAction::Indent), None, 0),
			("  ", "  * test() {", "", Some(IndentAction::Indent), None, 0),
		];
		for (previous, before, after, expected, append, remove) in cases {
			let actual = support.on_enter(previous, before, after);
			match expected {
				None => assert!(actual.is_none(), "before={before:?}"),
				Some(action) => {
					let actual = actual.unwrap_or_else(|| panic!("before={before:?}"));
					assert_eq!(actual.indent_action, *action, "before={before:?}");
					if let Some(append) = append {
						assert_eq!(actual.append_text, *append, "before={before:?}");
					}
					if *remove != 0 {
						assert_eq!(actual.remove_text, *remove, "before={before:?}");
					}
				}
			}
		}
	}

	// vendor/vscode-1.145.2/tests/onEnter.test.ts: issue #141816
	#[test]
	fn on_enter_issue_141816() {
		let support = js_on_enter_rules();
		for (before, after) in [
			("const r = /{/;", ""),
			("const r = /{[0-9]/;", ""),
			("const r = /[a-zA-Z]{/;", ""),
		] {
			assert!(support.on_enter("", before, after).is_none(), "before={before:?}");
		}
	}

	// vendor/vscode-1.145.2/tests/characterPair.test.ts 的语义：notIn 决定作用域内是否允许闭合。
	#[test]
	fn character_pair_not_in_scopes() {
		let json = super::configuration(Lang::Json).expect("json");
		let brace = json.auto_closing.iter().find(|p| p.open == "{").expect("{");
		assert!(brace.is_ok(ScopeKind::Other));
		assert!(!brace.is_ok(ScopeKind::String));
		let quote = json.auto_closing.iter().find(|p| p.open == "\"").expect("\"");
		assert!(!quote.is_ok(ScopeKind::String));
		assert!(!quote.is_ok(ScopeKind::Comment));
		let loose = AutoClosingPair::plain("{", "}");
		assert!(loose.is_ok(ScopeKind::String));
	}

	#[test]
	fn character_pair_support_defaults() {
		let only_pairs = r#"{"autoClosingPairs": [{"open": "a", "close": "b"}]}"#;
		let config = parse_language(&serde_json::from_str(only_pairs).unwrap()).unwrap();
		assert_eq!(config.auto_closing.len(), 1);
		assert_eq!(config.surrounding, vec![("a".to_string(), "b".to_string())]);

		let only_brackets = r#"{"brackets": [["a", "b"]]}"#;
		let config = parse_language(&serde_json::from_str(only_brackets).unwrap()).unwrap();
		assert_eq!(config.auto_closing.len(), 1);
		assert_eq!(config.surrounding, vec![("a".to_string(), "b".to_string())]);

		let pairs_win = r#"{"autoClosingPairs": [], "brackets": [["a", "b"]]}"#;
		let config = parse_language(&serde_json::from_str(pairs_win).unwrap()).unwrap();
		assert!(config.auto_closing.is_empty());
		assert!(config.surrounding.is_empty());

		let surrounding_only = r#"{"surroundingPairs": [{"open": "a", "close": "b"}]}"#;
		let config = parse_language(&serde_json::from_str(surrounding_only).unwrap()).unwrap();
		assert!(config.auto_closing.is_empty());
		assert_eq!(config.surrounding, vec![("a".to_string(), "b".to_string())]);
	}

	#[test]
	fn enter_indents_after_open_brace() {
		let config = rust_config();
		let model = TestModel::new(&["fn main() {", ""], config);
		let edit = get_enter_edit(&model, config, 1, 12, 1, 12);
		assert_eq!(edit.text, "\n\t");
		let action = get_enter_action(&model, config, 1, 12, 1, 12).expect("bracket rule");
		assert_eq!(action.indent_action, IndentAction::Indent);
	}

	#[test]
	fn enter_between_brackets_adds_outdented_line() {
		let config = rust_config();
		let model = TestModel::new(&["    foo()", ""], config);
		// 光标在 foo() 的 ( ) 之间：`    foo(|)`。
		let edit = get_enter_edit(&model, config, 1, 9, 1, 9);
		assert_eq!(edit.text, "\n\t\t\n\t");
		assert_eq!(edit.line_delta, -1);
	}

	#[test]
	fn enter_keeps_closing_brace_outdented() {
		let config = rust_config();
		let model = TestModel::new(&["fn main() {", "\tlet x = 1;", "\t}"], config);
		let edit = get_enter_edit(&model, config, 3, 2, 3, 2);
		// 在 `}` 之前回车，新行沿用上一行的缩进再退一级。
		assert_eq!(edit.text, "\n");
	}

	#[test]
	fn auto_close_quote_not_inside_string() {
		let config = rust_config();
		let model = TestModel::with_scopes(
			&[r#"let s = "";"#],
			config,
			vec![vec![
				ScopeKind::Other,
				ScopeKind::Other,
				ScopeKind::Other,
				ScopeKind::Other,
				ScopeKind::Other,
				ScopeKind::Other,
				ScopeKind::Other,
				ScopeKind::Other,
				ScopeKind::String,
				ScopeKind::String,
				ScopeKind::Other,
			]],
		);
		// 在空字符串的引号之间再打一个引号：不自动闭合（notIn string）。
		assert!(get_auto_closing_pair(&model, config, 1, 11, "\"").is_none());
	}

	#[test]
	fn auto_close_bracket_after_equals() {
		let config = rust_config();
		let model = TestModel::new(&["let x = "], config);
		let pair = get_auto_closing_pair(&model, config, 1, 9, "[");
		assert_eq!(pair, Some(("[".to_string(), "]".to_string())));
	}

	#[test]
	fn auto_close_quote_after_word_is_skipped() {
		let config = rust_config();
		let model = TestModel::new(&["foo"], config);
		assert!(get_auto_closing_pair(&model, config, 1, 4, "\"").is_none());
	}

	#[test]
	fn auto_close_quote_after_space_is_allowed() {
		let config = rust_config();
		let model = TestModel::new(&["let x = "], config);
		assert_eq!(
			get_auto_closing_pair(&model, config, 1, 9, "\""),
			Some(("\"".to_string(), "\"".to_string()))
		);
	}

	/// vendor/vscode-1.145.2/tests/cursor.test.ts: setupAutoClosingLanguage。
	/// 那边的 __electricCharacterSupport.docComment 与显式 `/**` 项相同；parse_language
	/// 不读电字符配置，这里只留 autoClosingPairs。
	fn auto_closing_config() -> &'static LanguageConfiguration {
		static SLOT: OnceLock<&'static LanguageConfiguration> = OnceLock::new();
		SLOT.get_or_init(|| {
			let json = r#"{
				"comments": { "blockComment": ["/*", "*/"] },
				"autoClosingPairs": [
					{ "open": "{", "close": "}" },
					{ "open": "[", "close": "]" },
					{ "open": "(", "close": ")" },
					{ "open": "'", "close": "'", "notIn": ["string", "comment"] },
					{ "open": "\"", "close": "\"", "notIn": ["string"] },
					{ "open": "`", "close": "`", "notIn": ["string", "comment"] },
					{ "open": "/**", "close": " */", "notIn": ["string"] },
					{ "open": "begin", "close": "end", "notIn": ["string"] }
				]
			}"#;
			let value: Value = serde_json::from_str(json).expect("json");
			Box::leak(Box::new(parse_language(&value).expect("config")))
		})
	}

	/// cursor.test.ts: setAutoClosingLanguageEnabledSet("abc")。
	fn auto_closing_config_before(chars: &str) -> &'static LanguageConfiguration {
		let json = r#"{
			"comments": { "blockComment": ["/*", "*/"] },
			"autoCloseBefore": "__CHARS__",
			"autoClosingPairs": [
				{ "open": "{", "close": "}" },
				{ "open": "[", "close": "]" },
				{ "open": "(", "close": ")" },
				{ "open": "'", "close": "'", "notIn": ["string", "comment"] },
				{ "open": "\"", "close": "\"", "notIn": ["string"] },
				{ "open": "`", "close": "`", "notIn": ["string", "comment"] },
				{ "open": "/**", "close": " */", "notIn": ["string"] }
			]
		}"#
		.replace("__CHARS__", chars);
		let value: Value = serde_json::from_str(&json).expect("json");
		Box::leak(Box::new(parse_language(&value).expect("config")))
	}

	/// cursor.test.ts: extractAutoClosingSpecialColumns。1 = Special1，2 = Special2。
	fn special_columns(max_column: usize, annotated: &str) -> Vec<u8> {
		let mut result = vec![0u8; max_column + 1];
		let mut column = 1;
		for c in annotated.chars() {
			match c {
				'|' => result[column] = 1,
				'!' => result[column] = 2,
				_ => column += 1,
			}
		}
		result
	}

	/// cursor.test.ts: assertType。在 (line, column) 键入 chr，整行等于 expected_insert
	/// 插在 column 前的结果。Special2（越过）要靠 autoClosed 记录，这几张表里没有 `!`。
	fn assert_type(
		model: &TestModel,
		config: &'static LanguageConfiguration,
		line: usize,
		column: usize,
		chr: &str,
		expected_insert: &str,
		message: &str,
	) {
		let original = model.line(line);
		let inserted = match get_auto_closing_pair(model, config, line, column, chr) {
			Some((_open, close)) => format!("{chr}{close}"),
			None => chr.to_string(),
		};
		let mut actual: String = original.chars().take(column - 1).collect();
		actual.push_str(&inserted);
		actual.extend(original.chars().skip(column - 1));
		let mut expected: String = original.chars().take(column - 1).collect();
		expected.push_str(expected_insert);
		expected.extend(original.chars().skip(column - 1));
		assert_eq!(actual, expected, "{message}");
	}

	fn assert_autoclose_table(
		config: &'static LanguageConfiguration,
		lines: &[&str],
		table: &[&str],
		chr: &str,
		special_insert: &str,
		normal_insert: &str,
	) {
		let model = TestModel::new(lines, config);
		for (i, annotated) in table.iter().enumerate() {
			let line = i + 1;
			let columns = special_columns(model.line(line).chars().count() + 1, annotated);
			for column in 1..columns.len() {
				let message = format!("{chr} @ ({line}, {column})");
				match columns[column] {
					1 => assert_type(&model, config, line, column, chr, special_insert, &message),
					2 => assert_type(&model, config, line, column, chr, "", &message),
					_ => assert_type(&model, config, line, column, chr, normal_insert, &message),
				}
			}
		}
	}

	// cursor.test.ts: autoClosingPairs - open parens: default
	#[test]
	fn auto_close_open_parens_default() {
		let config = auto_closing_config();
		let lines = [
			"var a = [];",
			"var b = `asd`;",
			"var c = 'asd';",
			"var d = \"asd\";",
			"var e = /*3*/\t3;",
			"var f = /** 3 */3;",
			"var g = (3+5);",
			"var h = { a: 'value' };",
		];
		let table = [
			r"var| a| |=| [|]|;|",
			r"var| b| |=| |`asd|`|;|",
			r"var| c| |=| |'asd|'|;|",
			r#"var| d| |=| |"asd|"|;|"#,
			"var| e| |=| /*3*/|\t3|;|",
			r"var| f| |=| /**| 3| */3|;|",
			r"var| g| |=| (3+5|)|;|",
			r"var| h| |=| {| a|:| |'value|'| |}|;|",
		];
		assert_autoclose_table(config, &lines, &table, "(", "()", "(");
	}

	// cursor.test.ts: autoClosingPairs - configurable open parens
	#[test]
	fn auto_close_open_parens_configurable() {
		let config = auto_closing_config_before("abc");
		let lines = [
			"var a = [];",
			"var b = `asd`;",
			"var c = 'asd';",
			"var d = \"asd\";",
			"var e = /*3*/\t3;",
			"var f = /** 3 */3;",
			"var g = (3+5);",
			"var h = { a: 'value' };",
		];
		let table = [
			r"v|ar |a = [|];|",
			r"v|ar |b = `|asd`;|",
			r"v|ar |c = '|asd';|",
			r#"v|ar d = "|asd";|"#,
			"v|ar e = /*3*/\t3;|",
			r"v|ar f = /** 3| */3;|",
			r"v|ar g = (3+5|);|",
			r"v|ar h = { |a: 'v|alue' |};|",
		];
		assert_autoclose_table(config, &lines, &table, "(", "()", "(");
	}

	// cursor.test.ts: autoClosingPairs - quote
	#[test]
	fn auto_close_quotes_default() {
		let config = auto_closing_config();
		let lines = [
			"var a = [];",
			"var b = `asd`;",
			"var c = 'asd';",
			"var d = \"asd\";",
			"var e = /*3*/\t3;",
			"var f = /** 3 */3;",
			"var g = (3+5);",
			"var h = { a: 'value' };",
		];
		let table = [
			r"var a |=| [|]|;|",
			r"var b |=| `asd`|;|",
			r"var c |=| 'asd'|;|",
			r#"var d |=| "asd"|;|"#,
			"var e |=| /*3*/|\t3;|",
			r"var f |=| /**| 3 */3;|",
			r"var g |=| (3+5)|;|",
			r"var h |=| {| a:| 'value'| |}|;|",
		];
		assert_autoclose_table(config, &lines, &table, "'", "''", "'");
	}

	// cursor.test.ts: autoClosingPairs - multi-character autoclose
	#[test]
	fn auto_close_multi_character() {
		let config = auto_closing_config();
		let model = TestModel::new(&["begi"], config);
		let pair = get_auto_closing_pair(&model, config, 1, 5, "n").expect("begin/end");
		assert_eq!(format!("begi{}{}", "n", pair.1), "beginend");

		let model = TestModel::new(&["/*"], config);
		let pair = get_auto_closing_pair(&model, config, 1, 3, "*").expect("doc comment");
		assert_eq!(format!("/*{}{}", "*", pair.1), "/** */");
	}

	// cursorCommon.ts: shouldAutoCloseBefore.comment 用的是 forQuotes = false 那一组，
	// 所以注释块配对在反引号前也会闭合。
	#[test]
	fn auto_close_comment_pair_uses_bracket_before_set() {
		let config = configuration(Lang::JavaScript).expect("javascript");
		let model = TestModel::new(&["/*`"], config);
		assert_eq!(
			get_auto_closing_pair(&model, config, 1, 3, "*"),
			Some(("/**".to_string(), " */".to_string()))
		);
	}

	// cursor.test.ts: autoClosingPairs - beforeText、autoClosingPairs - beforeText falls back to a shorter pair
	#[test]
	fn auto_close_before_text() {
		let json = r#"{
			"autoClosingPairs": [
				{ "open": "begin", "close": "end", "beforeText": "(?:^|\\s)begin$" },
				{ "open": "n", "close": "]" }
			]
		}"#;
		let value: Value = serde_json::from_str(json).expect("json");
		let config: &'static LanguageConfiguration =
			Box::leak(Box::new(parse_language(&value).expect("config")));

		let model = TestModel::new(&["begi"], config);
		assert_eq!(
			get_auto_closing_pair(&model, config, 1, 5, "n"),
			Some(("begin".to_string(), "end".to_string()))
		);

		let model = TestModel::new(&["prefixbegi"], config);
		assert_eq!(
			get_auto_closing_pair(&model, config, 1, 11, "n"),
			Some(("n".to_string(), "]".to_string()))
		);

		let model = TestModel::new(&["prefix begi"], config);
		assert_eq!(
			get_auto_closing_pair(&model, config, 1, 12, "n"),
			Some(("begin".to_string(), "end".to_string()))
		);
	}

	// cursor.test.ts: issue #72177 - multi-character autoclose with conflicting patterns
	#[test]
	fn auto_close_multi_character_with_conflicting_patterns() {
		let json = r#"{
			"autoClosingPairs": [
				{ "open": "(", "close": ")" },
				{ "open": "(*", "close": "*)" },
				{ "open": "<@", "close": "@>" },
				{ "open": "<@@", "close": "@@>" }
			]
		}"#;
		let value: Value = serde_json::from_str(json).expect("json");
		let config: &'static LanguageConfiguration =
			Box::leak(Box::new(parse_language(&value).expect("config")));

		let type_ch = |line: &mut Vec<char>, cursor: &mut usize, ch: char| {
			let text: String = line.iter().collect();
			let model = TestModel::new(&[text.as_str()], config);
			let pair = get_auto_closing_pair(&model, config, 1, *cursor + 1, &ch.to_string());
			line.insert(*cursor, ch);
			*cursor += 1;
			// 光标停在开括号之后、自动插入的闭合符之前，和 TypeWithAutoClosingCommand 一致。
			if let Some((_open, close)) = pair {
				for (i, c) in close.chars().enumerate() {
					line.insert(*cursor + i, c);
				}
			}
		};

		let mut line = Vec::new();
		let mut cursor = 0;
		type_ch(&mut line, &mut cursor, '(');
		assert_eq!(line.iter().collect::<String>(), "()");
		type_ch(&mut line, &mut cursor, '*');
		assert_eq!(line.iter().collect::<String>(), "(**)");

		let mut line: Vec<char> = "(".chars().collect();
		let mut cursor = 1;
		type_ch(&mut line, &mut cursor, '*');
		assert_eq!(line.iter().collect::<String>(), "(**)");

		let mut line = Vec::new();
		let mut cursor = 0;
		type_ch(&mut line, &mut cursor, '<');
		type_ch(&mut line, &mut cursor, '@');
		assert_eq!(line.iter().collect::<String>(), "<@@>");
		type_ch(&mut line, &mut cursor, '@');
		assert_eq!(line.iter().collect::<String>(), "<@@@@>");
		type_ch(&mut line, &mut cursor, '(');
		assert_eq!(line.iter().collect::<String>(), "<@@()@@>");
	}

	#[test]
	fn python_colon_indents_next_line() {
		let config = configuration(Lang::Python).expect("python");
		let model = TestModel::new(&["def foo():", ""], config);
		let edit = get_enter_edit(&model, config, 1, 11, 1, 11);
		assert_eq!(edit.text, "\n\t");
	}

	#[test]
	fn typing_open_bracket_between_if_line_indents() {
		let config = configuration(Lang::JavaScript).expect("javascript");
		let model = TestModel::new(&["if (x)", "\t"], config);
		// 光标在第 2 行行首，键入 `{`：上一行命中 indentNextLinePattern，increase 成立，
		// 当前缩进与推断值一致，返回继承缩进。
		assert_eq!(
			get_indent_action_for_type(&model, config, 2, 2, "{"),
			Some(String::new())
		);
	}

	/// vendor/vscode-1.145.2/tests/cursor.test.ts: setupIndentRulesLanguage。
	/// 只有缩进规则，没有 brackets / autoClosingPairs，onEnter 用默认 `(){}[]`。
	fn indent_rules_config() -> &'static LanguageConfiguration {
		static SLOT: OnceLock<&'static LanguageConfiguration> = OnceLock::new();
		SLOT.get_or_init(|| {
			let json = r#"{
				"indentationRules": {
					"decreaseIndentPattern": "^\\s*((?!\\S.*\\/[*]).*[*]\\/\\s*)?[})\\]]|^\\s*(case\\b.*|default):\\s*(\\/\\/.*|\\/[*].*[*]\\/\\s*)?$",
					"increaseIndentPattern": "^((?!\\/\\/).)*(\\{[^}\"'`]*|\\([^)\"']*|\\[[^\\]\"']*|^\\s*(\\{\\}|\\(\\)|\\[\\]|(case\\b.*|default):))\\s*(\\/\\/.*|\\/[*].*[*]\\/\\s*)?$",
					"indentNextLinePattern": "^\\s*(for|while|if|else)\\b(?!.*[;{}]\\s*(\\/\\/.*|\\/[*].*[*]\\/\\s*)?$)",
					"unIndentedLinePattern": "^(?!.*([;{}]|\\S:)\\s*(\\/\\/.*|\\/[*].*[*]\\/\\s*)?$)(?!.*(\\{[^}\"']*|\\([^)\"']*|\\[[^\\]\"']*|^\\s*(\\{\\}|\\(\\)|\\[\\]|(case\\b.*|default):))\\s*(\\/\\/.*|\\/[*].*[*]\\/\\s*)?$)(?!^\\s*((?!\\S.*\\/[*]).*[*]\\/\\s*)?[})\\]]|^\\s*(case\\b.*|default):\\s*(\\/\\/.*|\\/[*].*[*]\\/\\s*)?$)(?!^\\s*(for|while|if|else)\\b(?!.*[;{}]\\s*(\\/\\/.*|\\/[*].*[*]\\/\\s*)?$))"
				}
			}"#;
			let value: Value = serde_json::from_str(json).expect("json");
			Box::leak(Box::new(parse_language(&value).expect("config")))
		})
	}

	struct Doc {
		lines: Vec<Vec<char>>,
		cursor: (usize, usize),
		anchor: Option<(usize, usize)>,
	}

	impl Doc {
		fn new(lines: &[&str]) -> Self {
			Self {
				lines: lines.iter().map(|line| line.chars().collect()).collect(),
				cursor: (0, 0),
				anchor: None,
			}
		}

		/// cursor.test.ts 的 moveTo：1-based。
		fn move_to(&mut self, line: usize, column: usize) {
			self.cursor = (line - 1, column - 1);
			self.anchor = None;
		}

		/// moveTo(..., true)：从原位置拉选区。
		fn extend_to(&mut self, line: usize, column: usize) {
			self.anchor = Some(self.cursor);
			self.cursor = (line - 1, column - 1);
		}

		fn type_text(&mut self, text: &str) {
			let (line, column) = self.cursor;
			self.lines[line].splice(column..column, text.chars());
			self.cursor.1 += text.chars().count();
		}

		fn selection(&self) -> ((usize, usize), (usize, usize)) {
			match self.anchor {
				Some(anchor) if anchor != self.cursor => {
					if anchor <= self.cursor {
						(anchor, self.cursor)
					} else {
						(self.cursor, anchor)
					}
				}
				_ => (self.cursor, self.cursor),
			}
		}

		/// ReplaceCommandWithOffsetCursorState：按 Range 替换，光标放在文本末尾再加偏移。
		fn enter(&mut self, config: &'static LanguageConfiguration) {
			let (start, end) = self.selection();
			let model = DocModel {
				lines: &self.lines,
				config,
			};
			let edit = get_enter_edit(
				&model,
				config,
				start.0 + 1,
				start.1 + 1,
				end.0 + 1,
				end.1 + 1,
			);
			let from_line = edit.from_line - 1;
			let from_column = edit.from_column - 1;
			let to_line = edit.to_line - 1;
			let to_column = edit.to_column - 1;
			let prefix: Vec<char> = self.lines[from_line][..from_column].to_vec();
			let suffix: Vec<char> = self.lines[to_line][to_column..].to_vec();
			let mut parts: Vec<Vec<char>> = edit
				.text
				.split('\n')
				.map(|part| part.chars().collect())
				.collect();
			parts[0].splice(0..0, prefix);
			let last = parts.len() - 1;
			let last_len = parts[last].len();
			parts[last].extend(suffix);
			self.lines.splice(from_line..=to_line, parts);
			let line = (from_line as isize + last as isize + edit.line_delta).max(0) as usize;
			let column = (last_len as isize + edit.column_delta).max(0) as usize;
			self.cursor = (line, column);
			self.anchor = None;
		}

		fn content(&self, line: usize) -> String {
			self.lines[line - 1].iter().collect()
		}

		/// assertCursor：1-based。
		fn at(&self, line: usize, column: usize) -> bool {
			self.cursor == (line - 1, column - 1)
		}
	}

	struct DocModel<'a> {
		lines: &'a [Vec<char>],
		config: &'static LanguageConfiguration,
	}

	impl Model for DocModel<'_> {
		fn line(&self, line: usize) -> String {
			self.lines[line - 1].iter().collect()
		}

		fn processed_line(&self, line: usize) -> String {
			let chars = &self.lines[line - 1];
			process_line(chars, &vec![ScopeKind::Other; chars.len()], self.config)
		}

		fn processed_before(&self, line: usize, column: usize) -> String {
			let chars = &self.lines[line - 1];
			let len = (column - 1).min(chars.len());
			process_line(&chars[..len], &vec![ScopeKind::Other; len], self.config)
		}

		fn processed_after(&self, line: usize, column: usize) -> String {
			let chars = &self.lines[line - 1];
			let len = (column - 1).min(chars.len());
			process_line(&chars[len..], &vec![ScopeKind::Other; chars.len() - len], self.config)
		}

		fn scope_before(&self, _line: usize, _column: usize) -> ScopeKind {
			ScopeKind::Other
		}

		fn unmatched_closing_after(&self, _line: usize, _column: usize, _open: &str) -> bool {
			false
		}
	}

	// 以下 Enter 测试逐条转写自 cursor.test.ts（insertSpaces: false 的那几档；
	// insertSpaces 为 true 或 tabSize/indentSize 为 2 的用例在补齐缩进时就空着）。

	// cursor.test.ts: Enter honors increaseIndentPattern
	#[test]
	fn enter_honors_increase_indent_pattern() {
		let config = indent_rules_config();
		let mut doc = Doc::new(&["if (true) {", "\tif (true) {"]);
		doc.move_to(1, 12);
		doc.enter(config);
		assert!(doc.at(2, 2));
		assert_eq!(doc.content(2), "\t");

		doc.move_to(3, 13);
		doc.enter(config);
		assert!(doc.at(4, 3));
		assert_eq!(doc.content(4), "\t\t");
	}

	// cursor.test.ts: Enter honors unIndentedLinePattern
	#[test]
	fn enter_honors_un_indented_line_pattern() {
		let config = indent_rules_config();
		let mut doc = Doc::new(&["if (true) {", "\t\t\treturn true"]);
		doc.move_to(2, 15);
		doc.enter(config);
		assert!(doc.at(3, 2));
		assert_eq!(doc.content(3), "\t");
	}

	// cursor.test.ts: Enter honors indentNextLinePattern
	#[test]
	fn enter_honors_indent_next_line_pattern() {
		let config = indent_rules_config();
		let mut doc = Doc::new(&[
			"if (true)",
			"\treturn true;",
			"if (true)",
			"\t\t\t\treturn true",
		]);
		doc.move_to(2, 14);
		doc.enter(config);
		assert!(doc.at(3, 1));
		assert_eq!(doc.content(3), "");

		doc.move_to(5, 16);
		doc.enter(config);
		assert!(doc.at(6, 2));
	}

	// cursor.test.ts: Enter honors indentNextLinePattern 2
	#[test]
	fn enter_honors_indent_next_line_pattern_two() {
		let config = indent_rules_config();
		let mut doc = Doc::new(&["if (true)", "\tif (true)"]);
		doc.move_to(2, 11);
		doc.enter(config);
		assert!(doc.at(3, 3));

		doc.type_text("console.log();");
		doc.enter(config);
		assert!(doc.at(4, 1));
		assert_eq!(doc.content(4), "");
	}

	// cursor.test.ts: Enter honors intential indent
	#[test]
	fn enter_honors_intentional_indent() {
		let config = indent_rules_config();
		let mut doc = Doc::new(&["if (true) {", "\tif (true) {", "return true;", "}}"]);
		doc.move_to(3, 13);
		doc.enter(config);
		assert!(doc.at(4, 1));
		assert_eq!(doc.content(3), "return true;");
	}

	// cursor.test.ts: Enter supports selection 1
	#[test]
	fn enter_supports_selection_one() {
		let config = indent_rules_config();
		let mut doc = Doc::new(&["if (true) {", "\tif (true) {", "\t\treturn true;", "\t}a}"]);
		doc.move_to(4, 3);
		doc.extend_to(4, 4);
		doc.enter(config);
		assert!(doc.at(5, 1));
		assert_eq!(doc.content(4), "\t}");
	}

	// cursor.test.ts: Enter supports selection 2
	#[test]
	fn enter_supports_selection_two() {
		let config = indent_rules_config();
		let mut doc = Doc::new(&["if (true) {", "\tif (true) {"]);
		doc.move_to(2, 12);
		doc.extend_to(2, 13);
		doc.enter(config);
		assert!(doc.at(3, 3));

		doc.enter(config);
		assert!(doc.at(4, 3));
	}

	// cursor.test.ts: Enter honors tabSize and insertSpaces 3（insertSpaces: false 那一档）
	#[test]
	fn enter_honors_tab_size_with_tabs() {
		let config = indent_rules_config();
		let mut doc = Doc::new(&["if (true) {", "    if (true) {"]);
		doc.move_to(1, 12);
		doc.enter(config);
		assert!(doc.at(2, 2));

		doc.move_to(3, 16);
		doc.enter(config);
		assert_eq!(doc.content(3), "    if (true) {");
		assert!(doc.at(4, 3));
	}

	// cursor.test.ts: Enter supports intentional indentation
	#[test]
	fn enter_supports_intentional_indentation() {
		let config = indent_rules_config();
		let mut doc = Doc::new(&[
			"\tif (true) {",
			"\t\tswitch(true) {",
			"\t\t\tcase true:",
			"\t\t\t\tbreak;",
			"\t\t}",
			"\t}",
		]);
		doc.move_to(5, 4);
		doc.enter(config);
		assert_eq!(doc.content(5), "\t\t}");
		assert!(doc.at(6, 3));
	}

	// cursor.test.ts: Enter should not adjust cursor position when press enter
	// in the middle of a line 1 / 2
	#[test]
	fn enter_in_the_middle_of_a_line_keeps_the_cursor_column() {
		let config = indent_rules_config();
		let mut doc = Doc::new(&["if (true) {", "\tif (true) {", "\t\treturn true;", "\t}a}"]);
		doc.move_to(3, 9);
		doc.enter(config);
		assert!(doc.at(4, 3));
		assert_eq!(doc.content(4), "\t\t true;");

		let mut doc = Doc::new(&["if (true) {", "\tif (true) {", "\t\treturn true;", "\t}a}"]);
		doc.move_to(3, 3);
		doc.enter(config);
		assert!(doc.at(4, 3));
		assert_eq!(doc.content(4), "\t\treturn true;");
	}

	// cursor.test.ts: Enter should adjust cursor position when press enter
	// in the middle of leading whitespaces 1 / 2
	#[test]
	fn enter_in_the_middle_of_leading_whitespace() {
		let config = indent_rules_config();
		let mut doc = Doc::new(&["if (true) {", "\tif (true) {", "\t\treturn true;", "\t}a}"]);
		doc.move_to(3, 2);
		doc.enter(config);
		assert!(doc.at(4, 2));
		assert_eq!(doc.content(4), "\t\treturn true;");

		doc.move_to(4, 1);
		doc.enter(config);
		assert!(doc.at(5, 1));
		assert_eq!(doc.content(5), "\t\treturn true;");

		let mut doc = Doc::new(&[
			"\tif (true) {",
			"\t\tif (true) {",
			"\t    \treturn true;",
			"\t\t}a}",
		]);
		doc.move_to(3, 4);
		doc.enter(config);
		assert!(doc.at(4, 3));
		assert_eq!(doc.content(4), "\t\t\treturn true;");

		doc.move_to(4, 1);
		doc.enter(config);
		assert!(doc.at(5, 1));
		assert_eq!(doc.content(5), "\t\t\treturn true;");
	}

	// cursor.test.ts: issue microsoft/monaco-editor#108 part 1/2、part 2/2
	#[test]
	fn enter_with_selection_keeps_the_inherited_indent() {
		let config = indent_rules_config();
		for (from, to) in [((3, 8), (2, 12)), ((2, 12), (3, 8))] {
			let mut doc = Doc::new(&[
				"function baz() {",
				"\tvar x = 1;",
				"\t\t\t\t\t\t\treturn x;",
				"}",
			]);
			doc.move_to(from.0, from.1);
			doc.extend_to(to.0, to.1);
			doc.enter(config);
			assert_eq!(doc.content(3), "\treturn x;");
			assert!(doc.at(3, 2));
		}
	}

	#[test]
	fn overtype_only_for_recorded_closer() {
		let config = rust_config();
		assert!(is_auto_closing_overtype(config, "()", 2, ")", &[(0, 1)], 1));
		assert!(!is_auto_closing_overtype(config, "()", 2, ")", &[], 1));
	}

	#[test]
	fn surround_selection_rejects_whitespace_only() {
		let config = rust_config();
		assert!(!is_surround_selection_type(config, &["   ".to_string()], 1, 1, 1, 4, "\""));
		assert!(is_surround_selection_type(config, &["abc".to_string()], 1, 1, 1, 4, "\""));
	}
}

