use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ownership {
	Normal,
	Agent(String),
	Share(String),
}

#[derive(Clone, Debug)]
pub struct WindowState {
	pub ownership: Ownership,
	pub human_foreground: bool,
	approved: HashSet<String>,
}

impl Default for WindowState {
	fn default() -> Self {
		Self {
			ownership: Ownership::Normal,
			human_foreground: false,
			approved: HashSet::new(),
		}
	}
}

impl WindowState {
	pub fn human_opened(&self) -> Self {
		Self {
			ownership: Ownership::Normal,
			human_foreground: true,
			approved: HashSet::new(),
		}
	}

	pub fn ai_opened(&self) -> Self {
		Self {
			ownership: Ownership::Normal,
			human_foreground: false,
			approved: HashSet::new(),
		}
	}

	pub fn requires_application(&self, ai: &str) -> bool {
		self.human_foreground && !self.approved.contains(ai)
	}

	pub fn approve(&mut self, ai: &str) {
		self.approved.insert(String::from(ai));
		self.ownership = Ownership::Agent(String::from(ai));
	}

	pub fn share(&mut self, ai: &str) {
		self.ownership = Ownership::Share(String::from(ai));
	}

	pub fn release(&mut self) {
		self.ownership = Ownership::Normal;
	}

	pub fn can_close(&self) -> bool {
		matches!(self.ownership, Ownership::Normal)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn human_window_asks_once_per_ai() {
		let mut window = WindowState::default().human_opened();
		assert!(window.requires_application("sam"));
		window.approve("sam");
		assert!(!window.requires_application("sam"));
		assert!(window.requires_application("apple"));
		assert_eq!(window.ownership, Ownership::Agent(String::from("sam")));
	}

	#[test]
	fn ai_window_needs_no_application() {
		let window = WindowState::default().ai_opened();
		assert!(!window.requires_application("sam"));
	}
}
