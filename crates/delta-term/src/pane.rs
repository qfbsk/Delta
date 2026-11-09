use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::even::GroupPane;
use crate::opencode::Client;
use crate::pty::ConPty;
use crate::solid::Editor;
use crate::term::Terminal;

pub enum PaneKind {
	Terminal {
		pty: ConPty,
		term: Arc<Mutex<Terminal>>,
		reader: Option<JoinHandle<()>>,
	},
	Editor(Editor),
	Group(GroupPane),
}

pub struct Pane {
	pub id: u64,
	pub kind: PaneKind,
}

impl Pane {
	pub fn new(id: u64, command: &str, cols: usize, rows: usize) -> io::Result<Self> {
		Self::new_with_env(id, command, cols, rows, &[])
	}

	pub fn new_with_env(
		id: u64,
		command: &str,
		cols: usize,
		rows: usize,
		envs: &[(String, String)],
	) -> io::Result<Self> {
		Ok(Self {
			id,
			kind: PaneKind::Terminal {
				pty: ConPty::spawn_with_env(cols, rows, command, envs)?,
				term: Arc::new(Mutex::new(Terminal::new(cols, rows))),
				reader: None,
			},
		})
	}

	pub fn editor(id: u64, path: PathBuf, hwnd: usize) -> io::Result<Self> {
		Ok(Self {
			id,
			kind: PaneKind::Editor(Editor::open(path, hwnd)?),
		})
	}

	pub fn group(id: u64, client: Option<Client>, cwd: String) -> Self {
		Self {
			id,
			kind: PaneKind::Group(GroupPane::new(client, cwd)),
		}
	}

	pub fn is_terminal(&self) -> bool {
		matches!(self.kind, PaneKind::Terminal { .. })
	}

	pub fn terminal_handles(&self) -> Option<(usize, usize, usize, Arc<Mutex<Terminal>>)> {
		match &self.kind {
			PaneKind::Terminal { pty, term, .. } => Some((
				pty.output_read() as usize,
				pty.input_write() as usize,
				pty.process_handle() as usize,
				term.clone(),
			)),
			_ => None,
		}
	}

	pub fn pty(&self) -> Option<&ConPty> {
		match &self.kind {
			PaneKind::Terminal { pty, .. } => Some(pty),
			_ => None,
		}
	}

	pub fn term(&self) -> Option<&Arc<Mutex<Terminal>>> {
		match &self.kind {
			PaneKind::Terminal { term, .. } => Some(term),
			_ => None,
		}
	}

	pub fn set_reader(&mut self, handle: JoinHandle<()>) {
		if let PaneKind::Terminal { reader, .. } = &mut self.kind {
			*reader = Some(handle);
		}
	}

	pub fn editor_ref(&self) -> Option<&Editor> {
		match &self.kind {
			PaneKind::Editor(editor) => Some(editor),
			_ => None,
		}
	}

	pub fn editor_mut(&mut self) -> Option<&mut Editor> {
		match &mut self.kind {
			PaneKind::Editor(editor) => Some(editor),
			_ => None,
		}
	}

	pub fn group_ref(&self) -> Option<&GroupPane> {
		match &self.kind {
			PaneKind::Group(group) => Some(group),
			_ => None,
		}
	}
}
