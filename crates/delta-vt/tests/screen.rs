use delta_vt::{Color, Screen};

fn screen(cols: usize, rows: usize) -> Screen {
	Screen::new(cols, rows)
}

fn plain(s: &Screen) -> String {
	s.all_text().replace('\n', "")
}

#[test]
fn basic_print_and_crlf() {
	let mut s = screen(10, 3);
	s.feed(b"hello\r\nworld");
	assert_eq!(s.line_text(0).unwrap(), "hello");
	assert_eq!(s.line_text(1).unwrap(), "world");
	assert_eq!(s.cursor(), (5, 1));
}

#[test]
fn cursor_and_erase() {
	let mut s = screen(10, 3);
	s.feed(b"abcdef\x1b[1;3H\x1b[K");
	assert_eq!(s.line_text(0).unwrap(), "ab");
	s.feed(b"\x1b[2J");
	assert_eq!(plain(&s), "");
}

#[test]
fn sgr_truecolor_semicolon_and_colon() {
	let mut s = screen(10, 2);
	s.feed(b"\x1b[38;2;1;2;3mX");
	assert_eq!(s.row_abs(0).unwrap().cells[0].fg, Color::Rgb(1, 2, 3));
	s.feed(b"\x1b[0m\x1b[38:2::4:5:6mY");
	assert_eq!(s.row_abs(0).unwrap().cells[1].fg, Color::Rgb(4, 5, 6));
	s.feed(b"\x1b[0m\x1b[48;5;196mZ");
	assert_eq!(s.row_abs(0).unwrap().cells[2].bg, Color::Indexed(196));
}

#[test]
fn autowrap_defers_one_cell() {
	let mut s = screen(5, 2);
	s.feed(b"abcde");
	assert_eq!(s.cursor(), (4, 0));
	assert!(!s.row_abs(0).unwrap().wrapped);
	s.feed(b"f");
	assert!(s.row_abs(0).unwrap().wrapped);
	assert_eq!(s.line_text(0).unwrap(), "abcde");
	assert_eq!(s.line_text(1).unwrap(), "f");
	assert_eq!(s.cursor(), (1, 1));
}

#[test]
fn wide_char_occupies_two_cells() {
	let mut s = screen(4, 2);
	s.feed("中".as_bytes());
	let row = s.row_abs(0).unwrap();
	assert_eq!(row.cells[0].wide, 1);
	assert_eq!(row.cells[1].wide, 2);
	assert_eq!(s.cursor(), (2, 0));
	assert_eq!(s.line_text(0).unwrap(), "中");
}

#[test]
fn wide_char_wraps_at_margin() {
	let mut s = screen(4, 2);
	s.feed("abc".as_bytes());
	s.feed("中".as_bytes());
	assert!(s.row_abs(0).unwrap().wrapped);
	assert_eq!(s.line_text(0).unwrap(), "abc");
	assert_eq!(s.line_text(1).unwrap(), "中");
}

#[test]
fn alt_screen_restores_cursor_and_content() {
	let mut s = screen(10, 5);
	s.feed(b"A\x1b[3;4H");
	s.feed(b"\x1b[?1049h");
	assert!(s.is_alt());
	s.feed(b"B");
	assert_eq!(s.alt_text().trim_end(), "B");
	s.feed(b"\x1b[?1049l");
	assert!(!s.is_alt());
	assert_eq!(s.line_text(0).unwrap(), "A");
	assert_eq!(s.cursor(), (3, 2));
}

#[test]
fn scroll_into_scrollback() {
	let mut s = screen(10, 3);
	s.feed(b"1\r\n2\r\n3\r\n4\r\n5");
	assert_eq!(s.scrollback_len(), 2);
	assert_eq!(s.line_text(0).unwrap(), "1");
	assert_eq!(s.line_text(2).unwrap(), "3");
	assert_eq!(s.line_text(4).unwrap(), "5");
}

#[test]
fn resize_reflows_soft_wraps() {
	let mut s = screen(6, 4);
	s.feed(b"abcdefghijkl");
	assert_eq!(s.cursor(), (5, 1));
	s.resize(4, 4);
	assert_eq!(plain(&s), "abcdefghijkl");
	assert_eq!(s.cursor(), (3, 2));
	s.resize(8, 4);
	assert_eq!(plain(&s), "abcdefghijkl");
	assert_eq!(s.cursor(), (3, 1));
}

#[test]
fn resize_keeps_hard_lines() {
	let mut s = screen(6, 4);
	s.feed(b"ab\r\ncd");
	s.resize(3, 4);
	assert_eq!(s.line_text(0).unwrap(), "ab");
	assert_eq!(s.line_text(1).unwrap(), "cd");
}

#[test]
fn resize_to_one_column_with_wide_char_terminates() {
	let mut s = screen(6, 2);
	s.feed("中文".as_bytes());
	s.resize(1, 4);
	assert_eq!(plain(&s), "中文");
}

#[test]
fn osc_title_and_cwd() {
	let mut s = screen(10, 2);
	s.feed(b"\x1b]2;my title\x07");
	assert_eq!(s.title, "my title");
	s.feed(b"\x1b]7;file:///E:/Delta\x07");
	assert_eq!(s.cwd.as_deref(), Some("/E:/Delta"));
}

#[test]
fn private_modes_tracked() {
	let mut s = screen(10, 2);
	s.feed(b"\x1b[?9001h\x1b[?2004h\x1b[?1006h\x1b[?1002h");
	assert!(s.modes.win32_input);
	assert!(s.modes.bracketed_paste);
	assert_eq!(s.modes.mouse_encoding, delta_vt::MouseEncoding::Sgr);
	assert_eq!(s.modes.mouse_mode, delta_vt::MouseMode::Button);
	s.feed(b"\x1b[?1002l");
	assert_eq!(s.modes.mouse_mode, delta_vt::MouseMode::None);
}

#[test]
fn dsr_reports_cursor() {
	let mut s = screen(10, 5);
	s.feed(b"\x1b[2;3H\x1b[6n");
	let r = s.take_response();
	assert_eq!(r, b"\x1b[2;3R");
}

#[test]
fn repeat_and_line_drawing() {
	let mut s = screen(10, 2);
	s.feed(b"a\x1b[3b");
	assert_eq!(s.line_text(0).unwrap(), "aaaa");
	s.feed(b"\x1b(0q\x1b(B");
	assert_eq!(s.row_abs(0).unwrap().cells[4].ch, '─');
}

#[test]
fn dcs_apc_do_not_print() {
	let mut s = screen(10, 2);
	s.feed(b"A\x1bPq#0;2;0;0;0#0~~@@\x1b\\B");
	s.feed(b"\x1b_Gi=1;AAAA\x1b\\C");
	assert_eq!(s.line_text(0).unwrap(), "ABC");
}

#[test]
fn utf8_split_across_feeds() {
	let mut s = screen(10, 2);
	let b = "中文".as_bytes();
	s.feed(&b[..2]);
	s.feed(&b[2..]);
	assert_eq!(s.line_text(0).unwrap(), "中文");
}

#[test]
fn selection_text_between() {
	let mut s = screen(10, 3);
	s.feed(b"hello\r\nworld");
	assert_eq!(s.text_between(0, 1, 1, 2), "ello     \nwor");
	assert_eq!(s.text_between(1, 2, 0, 1), "ello     \nwor");
}

#[test]
fn insert_mode_shifts() {
	let mut s = screen(10, 2);
	s.feed(b"abc\x1b[1;2H\x1b[4hX");
	assert_eq!(s.line_text(0).unwrap(), "aXbc");
}

#[test]
fn scroll_region_does_not_touch_scrollback() {
	let mut s = screen(5, 4);
	s.feed(b"a\r\nb\r\nc\r\nd");
	s.feed(b"\x1b[2;3r\x1b[3;1H\n");
	assert_eq!(s.scrollback_len(), 0);
}

#[test]
fn conpty_startup_sequence_sets_win32() {
	let mut s = screen(10, 2);
	s.feed(b"\x1b[c\x1b[?1004h\x1b[?9001h");
	assert!(s.modes.win32_input, "startup 9001 not tracked");
}
