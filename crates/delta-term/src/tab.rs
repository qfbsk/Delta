use crate::layout::{Dir, Node};
use crate::ownership::WindowState;
use crate::pane::Pane;

pub struct Tab {
	pub command: String,
	pub envs: Vec<(String, String)>,
	pub label: String,
	pub panes: Vec<Pane>,
	pub layout: Option<Node>,
	pub active: Option<u64>,
	pub window: WindowState,
}

impl Tab {
	pub fn new(pane: Pane, command: String, label: String) -> Self {
		Self::new_with_env(pane, command, Vec::new(), label)
	}

	pub fn new_with_env(
		pane: Pane,
		command: String,
		envs: Vec<(String, String)>,
		label: String,
	) -> Self {
		let pane_id = pane.id;
		Self {
			command,
			envs,
			label,
			panes: vec![pane],
			layout: Some(Node::leaf(pane_id)),
			active: Some(pane_id),
			window: WindowState::default().human_opened(),
		}
	}

	pub fn ai_opened(mut self) -> Self {
		self.window = WindowState::default().ai_opened();
		self
	}

	pub fn active_pane(&self) -> Option<&Pane> {
		let id = self.active?;
		self.panes.iter().find(|pane| pane.id == id)
	}

	pub fn active_pane_mut(&mut self) -> Option<&mut Pane> {
		let id = self.active?;
		self.panes.iter_mut().find(|pane| pane.id == id)
	}

	pub fn contains_pane(&self, id: u64) -> bool {
		self.panes.iter().any(|pane| pane.id == id)
	}

	pub fn remove_pane(&mut self, id: u64) -> Option<Pane> {
		let pos = self.panes.iter().position(|pane| pane.id == id)?;
		let pane = self.panes.remove(pos);
		self.layout = self.layout.take().and_then(|root| root.remove(id));
		if self.active == Some(id) {
			self.active = self
				.layout
				.as_ref()
				.and_then(|root| root.leaves().first().copied());
		}
		Some(pane)
	}

	pub fn split(&mut self, target: u64, pane: Pane, dir: Dir) {
		let id = pane.id;
		self.panes.push(pane);
		if let Some(root) = self.layout.as_mut() {
			root.split(target, id, dir);
		} else {
			self.layout = Some(Node::leaf(id));
		}
		self.active = Some(id);
	}

	pub fn is_empty(&self) -> bool {
		self.panes.is_empty()
	}
}
