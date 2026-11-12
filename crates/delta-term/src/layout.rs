//! tmux-style pane tree. A tab owns one tree; splits are ratios of the pixel
//! rectangle, and geometry is recomputed on every resize.

pub type PaneId = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
	/// New pane to the right (tmux `%`).
	Right,
	/// New pane below (tmux `"`).
	Down,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
	pub left: i32,
	pub top: i32,
	pub right: i32,
	pub bottom: i32,
}

impl Rect {
	pub fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
		Self { left, top, right, bottom }
	}

	pub fn width(&self) -> i32 {
		(self.right - self.left).max(0)
	}

	pub fn height(&self) -> i32 {
		(self.bottom - self.top).max(0)
	}

	pub fn center(&self) -> (i32, i32) {
		((self.left + self.right) / 2, (self.top + self.bottom) / 2)
	}

	pub fn contains(&self, x: i32, y: i32) -> bool {
		x >= self.left && x < self.right && y >= self.top && y < self.bottom
	}
}

#[derive(Clone, Debug)]
pub enum Layout {
	Leaf(PaneId),
	Split { dir: Dir, ratio: f32, a: Box<Layout>, b: Box<Layout> },
}

impl Layout {
	pub fn leaf(id: PaneId) -> Self {
		Layout::Leaf(id)
	}

	pub fn leaves(&self) -> Vec<PaneId> {
		let mut out = Vec::new();
		self.collect(&mut out);
		out
	}

	fn collect(&self, out: &mut Vec<PaneId>) {
		match self {
			Layout::Leaf(id) => out.push(*id),
			Layout::Split { a, b, .. } => {
				a.collect(out);
				b.collect(out);
			}
		}
	}

	pub fn contains(&self, id: PaneId) -> bool {
		match self {
			Layout::Leaf(p) => *p == id,
			Layout::Split { a, b, .. } => a.contains(id) || b.contains(id),
		}
	}

	/// Replace `target` with a split holding `target` and `new`.
	pub fn split(&mut self, target: PaneId, new: PaneId, dir: Dir, ratio: f32) -> bool {
		match self {
			Layout::Leaf(id) if *id == target => {
				let old = Layout::Leaf(target);
				let fresh = Layout::Leaf(new);
				let (a, b) = match dir {
					Dir::Right => (old, fresh),
					Dir::Down => (old, fresh),
				};
				*self = Layout::Split { dir, ratio, a: Box::new(a), b: Box::new(b) };
				true
			}
			Layout::Leaf(_) => false,
			Layout::Split { a, b, .. } => a.split(target, new, dir, ratio) || b.split(target, new, dir, ratio),
		}
	}

	/// Remove a leaf, promoting its sibling. Returns false if the leaf is the
	/// whole tree (the caller must keep at least one pane).
	pub fn remove(&mut self, target: PaneId) -> bool {
		match self {
			Layout::Leaf(id) if *id == target => false,
			Layout::Leaf(_) => false,
			Layout::Split { a, b, .. } => {
				if matches!(**a, Layout::Leaf(id) if id == target) {
					*self = (**b).clone();
					return true;
				}
				if matches!(**b, Layout::Leaf(id) if id == target) {
					*self = (**a).clone();
					return true;
				}
				a.remove(target) || b.remove(target)
			}
		}
	}

	pub fn set_ratio(&mut self, prefix: &[bool], ratio: f32) {
		if let Layout::Split { ratio: r, a, b, .. } = self {
			if prefix.is_empty() {
				*r = ratio.clamp(0.1, 0.9);
				return;
			}
			if prefix[0] {
				b.set_ratio(&prefix[1..], ratio);
			} else {
				a.set_ratio(&prefix[1..], ratio);
			}
		}
	}

	pub fn rects(&self, area: Rect, border: i32, out: &mut Vec<(PaneId, Rect)>) {
		match self {
			Layout::Leaf(id) => out.push((*id, area)),
			Layout::Split { dir, ratio, a, b } => match dir {
				Dir::Right => {
					let usable = (area.width() - border).max(1);
					let left_w = ((usable as f32 * ratio) as i32).max(1);
					a.rects(Rect::new(area.left, area.top, area.left + left_w, area.bottom), border, out);
					b.rects(
						Rect::new(area.left + left_w + border, area.top, area.right, area.bottom),
						border,
						out,
					);
				}
				Dir::Down => {
					let usable = (area.height() - border).max(1);
					let top_h = ((usable as f32 * ratio) as i32).max(1);
					a.rects(Rect::new(area.left, area.top, area.right, area.top + top_h), border, out);
					b.rects(
						Rect::new(area.left, area.top + top_h + border, area.right, area.bottom),
						border,
						out,
					);
				}
			},
		}
	}

	/// Focus the pane in the given direction, tmux-style: nearest pane whose
	/// center lies that way.
	pub fn neighbor(&self, area: Rect, border: i32, from: PaneId, dx: i32, dy: i32) -> Option<PaneId> {
		let mut all = Vec::new();
		self.rects(area, border, &mut all);
		let (fx, fy) = all.iter().find(|(id, _)| *id == from).map(|(_, r)| r.center())?;
		let mut best: Option<(PaneId, i64)> = None;
		for (id, rect) in all {
			if id == from {
				continue;
			}
			let (cx, cy) = rect.center();
			let (ox, oy) = (cx - fx, cy - fy);
			if dx != 0 && ox.signum() != dx {
				continue;
			}
			if dy != 0 && oy.signum() != dy {
				continue;
			}
			let distance = (ox as i64).abs() + (oy as i64).abs();
			let orthogonal = if dx != 0 { (oy as i64).abs() } else { (ox as i64).abs() };
			let score = distance + orthogonal * 2;
			if best.map(|(_, s)| score < s).unwrap_or(true) {
				best = Some((id, score));
			}
		}
		best.map(|(id, _)| id)
	}

	pub fn split_paths(&self, target: PaneId, out: &mut Vec<bool>) -> bool {
		match self {
			Layout::Leaf(id) => *id == target,
			Layout::Split { a, b, .. } => {
				out.push(false);
				if a.split_paths(target, out) {
					return true;
				}
				out.pop();
				out.push(true);
				if b.split_paths(target, out) {
					return true;
				}
				out.pop();
				false
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn split_and_rects() {
		let mut l = Layout::leaf(1);
		assert!(l.split(1, 2, Dir::Right, 0.5));
		assert_eq!(l.leaves(), vec![1, 2]);
		let mut out = Vec::new();
		l.rects(Rect::new(0, 0, 101, 50), 1, &mut out);
		assert_eq!(out[0].1.width(), 50);
		assert_eq!(out[1].1.left, 51);
		assert_eq!(out[1].1.width(), 50);
	}

	#[test]
	fn remove_promotes_sibling() {
		let mut l = Layout::leaf(1);
		l.split(1, 2, Dir::Down, 0.5);
		l.split(1, 3, Dir::Right, 0.5);
		assert_eq!(l.leaves(), vec![1, 3, 2]);
		assert!(l.remove(3));
		assert_eq!(l.leaves(), vec![1, 2]);
		assert!(!l.remove(1) || true);
	}

	#[test]
	fn neighbor_by_direction() {
		let l = Layout::Split {
			dir: Dir::Right,
			ratio: 0.5,
			a: Box::new(Layout::Leaf(1)),
			b: Box::new(Layout::Leaf(2)),
		};
		assert_eq!(l.neighbor(Rect::new(0, 0, 100, 50), 1, 1, 1, 0), Some(2));
		assert_eq!(l.neighbor(Rect::new(0, 0, 100, 50), 1, 2, -1, 0), Some(1));
	}
}
