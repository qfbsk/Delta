#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Category {
	#[default]
	Text,
	Keyword,
	String,
	Comment,
	Number,
	Type,
	Function,
	Property,
	Macro,
	Attribute,
	Operator,
}

pub const EDITOR_BG: (u8, u8, u8) = (16, 16, 20);
pub const EDITOR_FG: (u8, u8, u8) = (212, 212, 212);
pub const GUTTER_FG: (u8, u8, u8) = (86, 88, 96);
pub const GUTTER_BG: (u8, u8, u8) = (22, 22, 27);
pub const STATUS_FG: (u8, u8, u8) = (160, 162, 170);
pub const STATUS_BG: (u8, u8, u8) = (28, 28, 34);
pub const CURSOR_BG: (u8, u8, u8) = (90, 140, 190);
pub const ERROR: (u8, u8, u8) = (224, 108, 117);
pub const WARN: (u8, u8, u8) = (220, 180, 100);
pub const ACCENT: (u8, u8, u8) = (110, 170, 130);
pub const DIM: (u8, u8, u8) = (120, 122, 130);
pub const PANEL_BG: (u8, u8, u8) = (24, 24, 30);
pub const PANEL_BORDER: (u8, u8, u8) = (60, 62, 72);
pub const SELECT_BG: (u8, u8, u8) = (44, 52, 64);

pub fn fg(category: Category) -> (u8, u8, u8) {
	match category {
		Category::Text => EDITOR_FG,
		Category::Keyword => (197, 134, 192),
		Category::String => (206, 145, 120),
		Category::Comment => (106, 153, 85),
		Category::Number => (181, 206, 168),
		Category::Type => (78, 201, 176),
		Category::Function => (220, 220, 170),
		Category::Property => (156, 220, 254),
		Category::Macro => (215, 186, 125),
		Category::Attribute => (215, 186, 125),
		Category::Operator => (170, 172, 180),
	}
}
