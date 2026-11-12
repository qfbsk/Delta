//! Delta byte layer: DEC ANSI parser and character screen.

pub mod parser;
pub mod screen;

pub use parser::{Action, Csi, Parser};
pub use screen::{
	ATTR_BLINK, ATTR_BOLD, ATTR_CURLY_UNDERLINE, ATTR_DASHED_UNDERLINE, ATTR_DIM,
	ATTR_DOTTED_UNDERLINE, ATTR_DOUBLE_UNDERLINE, ATTR_INVISIBLE, ATTR_ITALIC, ATTR_OVERLINE,
	ATTR_REVERSE, ATTR_STRIKE, ATTR_UNDERLINE, Cell, Color, MouseEncoding, MouseMode, Row, Screen,
};
