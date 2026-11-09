use windows_sys::Win32::Foundation::RECT;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
	/// First on the left, second on the right.
	Columns,
	/// First on top, second on the bottom.
	Rows,
}

#[derive(Clone, Debug)]
pub enum Node {
	Leaf(u64),
	Split {
		dir: Dir,
		ratio: f32,
		first: Box<Node>,
		second: Box<Node>,
	},
}

impl Node {
	pub fn leaf(id: u64) -> Self {
		Self::Leaf(id)
	}

	pub fn contains(&self, id: u64) -> bool {
		match self {
			Self::Leaf(x) => *x == id,
			Self::Split { first, second, .. } => first.contains(id) || second.contains(id),
		}
	}

	pub fn split(&mut self, target: u64, new_id: u64, dir: Dir) -> bool {
		match self {
			Self::Leaf(id) if *id == target => {
				*self = Self::Split {
					dir,
					ratio: 0.5,
					first: Box::new(Self::Leaf(target)),
					second: Box::new(Self::Leaf(new_id)),
				};
				true
			}
			Self::Leaf(_) => false,
			Self::Split { first, second, .. } => {
				if first.contains(target) {
					first.split(target, new_id, dir)
				} else if second.contains(target) {
					second.split(target, new_id, dir)
				} else {
					false
				}
			}
		}
	}

	pub fn remove(self, id: u64) -> Option<Self> {
		match self {
			Self::Leaf(x) => {
				if x == id {
					None
				} else {
					Some(Self::Leaf(x))
				}
			}
			Self::Split {
				dir,
				ratio,
				first,
				second,
			} => {
				if first.contains(id) {
					match first.remove(id) {
						Some(f) => Some(Self::Split {
							dir,
							ratio,
							first: Box::new(f),
							second,
						}),
						None => Some(*second),
					}
				} else if second.contains(id) {
					match second.remove(id) {
						Some(s) => Some(Self::Split {
							dir,
							ratio,
							first,
							second: Box::new(s),
						}),
						None => Some(*first),
					}
				} else {
					Some(Self::Split {
						dir,
						ratio,
						first,
						second,
					})
				}
			}
		}
	}

	pub fn leaves(&self) -> Vec<u64> {
		let mut out = Vec::new();
		self.collect_leaves(&mut out);
		out
	}

	fn collect_leaves(&self, out: &mut Vec<u64>) {
		match self {
			Self::Leaf(id) => out.push(*id),
			Self::Split { first, second, .. } => {
				first.collect_leaves(out);
				second.collect_leaves(out);
			}
		}
	}

	pub fn rects(&self, rect: RECT, out: &mut Vec<(u64, RECT)>) {
		match self {
			Self::Leaf(id) => out.push((*id, rect)),
			Self::Split {
				dir,
				ratio,
				first,
				second,
			} => {
				let width = (rect.right - rect.left).max(1);
				let height = (rect.bottom - rect.top).max(1);
				if *dir == Dir::Columns {
					let available = (width - 1).max(1);
					let first_width =
						((available as f32 * ratio).round() as i32).clamp(1, available.max(1));
					let mid = rect.left + first_width;
					first.rects(
						RECT {
							left: rect.left,
							top: rect.top,
							right: mid,
							bottom: rect.bottom,
						},
						out,
					);
					second.rects(
						RECT {
							left: mid + 1,
							top: rect.top,
							right: rect.right,
							bottom: rect.bottom,
						},
						out,
					);
				} else {
					let available = (height - 1).max(1);
					let first_height =
						((available as f32 * ratio).round() as i32).clamp(1, available.max(1));
					let mid = rect.top + first_height;
					first.rects(
						RECT {
							left: rect.left,
							top: rect.top,
							right: rect.right,
							bottom: mid,
						},
						out,
					);
					second.rects(
						RECT {
							left: rect.left,
							top: mid + 1,
							right: rect.right,
							bottom: rect.bottom,
						},
						out,
					);
				}
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn client() -> RECT {
		RECT {
			left: 0,
			top: 0,
			right: 800,
			bottom: 600,
		}
	}

	#[test]
	fn split_and_remove() {
		let mut root = Node::leaf(1);
		assert!(root.split(1, 2, Dir::Columns));
		assert_eq!(root.leaves(), vec![1, 2]);
		let mut rects = Vec::new();
		root.rects(client(), &mut rects);
		assert_eq!(rects.len(), 2);
		assert_eq!(rects[0].1.right + 1, rects[1].1.left);
		let root = root.remove(1).unwrap();
		assert_eq!(root.leaves(), vec![2]);
	}

	#[test]
	fn nested_splits() {
		let mut root = Node::leaf(1);
		root.split(1, 2, Dir::Columns);
		root.split(2, 3, Dir::Rows);
		assert_eq!(root.leaves(), vec![1, 2, 3]);
		let root = root.remove(2).unwrap();
		assert_eq!(root.leaves(), vec![1, 3]);
	}
}
