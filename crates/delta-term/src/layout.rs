//! tmux 3.8 `layout.c` ported to Rust: the tiled window layout is a tree of
//! cells, each either a left-right container, a top-bottom container, or one
//! window pane. Sizes are terminal cells and a one-cell separator sits between
//! siblings.
//!
//! Ported from `vendor/tmux-3.8/layout.c`: layout_geometry_init,
//! layout_create_cell, layout_free_cell, layout_set_size, layout_make_leaf,
//! layout_make_node, layout_cell_is_tiled, layout_cell_has_tiled_child,
//! layout_fix_offsets, layout_fix_panes, layout_resize_check,
//! layout_resize_adjust, layout_resize_set_size, layout_cell_get_neighbour,
//! layout_destroy_cell, layout_init, layout_resize, layout_resize_layout,
//! layout_resize_pane, layout_resize_pane_grow, layout_resize_pane_shrink,
//! layout_split_check_space, layout_split_sizes, layout_replace_with_node,
//! layout_split_pane, layout_close_pane and layout_count_cells. The grid used
//! whenever the drawn set changes is `layout_set_tiled` from
//! `vendor/tmux-3.8/layout-set.c`.
//!
//! Floating panes, pane status lines, scrollbars and SPAWN_FULLSIZE belong to
//! extensions this Delta tree does not use, so they are not carried over. The
//! cell tree itself, its geometry rules, and the resize/split/close algorithms
//! are the tmux ones, so window resizing, `%`/`"` splits and pane closing
//! redistribute space exactly as tmux does.
//!
//! Directional pane selection is from `window.c` window_pane_find_left /
//! window_pane_find_right / window_pane_find_up / window_pane_find_down.

pub type PaneId = u32;

/// tmux.h PANE_MINIMUM, not including border lines.
pub const PANE_MINIMUM: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
	/// New pane to the right (tmux `%`).
	Right,
	/// New pane below (tmux `"`).
	Down,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutType {
	LeftRight,
	TopBottom,
	WindowPane,
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
		Self {
			left,
			top,
			right,
			bottom,
		}
	}

	pub fn width(&self) -> i32 {
		(self.right - self.left).max(0)
	}

	pub fn height(&self) -> i32 {
		(self.bottom - self.top).max(0)
	}

	pub fn contains(&self, x: i32, y: i32) -> bool {
		x >= self.left && x < self.right && y >= self.top && y < self.bottom
	}
}

/// struct layout_geometry.
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
	pub sx: u32,
	pub sy: u32,
	pub xoff: i32,
	pub yoff: i32,
}

impl Geometry {
	/// layout_geometry_init.
	fn sentinel() -> Self {
		Self {
			sx: u32::MAX,
			sy: u32::MAX,
			xoff: i32::MAX,
			yoff: i32::MAX,
		}
	}
}

/// struct layout_cell, stored in an arena so parent/child links stay simple.
struct Cell {
	lc_type: LayoutType,
	parent: Option<usize>,
	g: Geometry,
	wp: Option<PaneId>,
	cells: Vec<usize>,
}

pub struct Layout {
	cells: Vec<Cell>,
	root: Option<usize>,
}

impl Layout {
	/// layout_init: one window pane filling the window.
	pub fn new(pane: PaneId, sx: u32, sy: u32) -> Self {
		let mut layout = Layout {
			cells: Vec::new(),
			root: None,
		};
		let root = layout.create(None);
		layout.set_size(root, sx.max(PANE_MINIMUM), sy.max(PANE_MINIMUM), 0, 0);
		layout.make_leaf(root, pane);
		layout.root = Some(root);
		layout
	}

	/// How many rows and columns layout_set_tiled wants. tmux's
	/// `tiled-layout-max-columns` is unset, so columns are unlimited.
	fn tiled_grid(n: usize) -> (u32, u32) {
		let n = n.max(1) as u32;
		let (mut rows, mut columns) = (1u32, 1u32);
		while rows * columns < n {
			rows += 1;
			if rows * columns < n {
				columns += 1;
			}
		}
		(rows, columns)
	}

	/// Whether a tiled grid of `n` panes still leaves every cell at least
	/// PANE_MINIMUM in both directions.
	pub fn tiled_fits(n: usize, sx: u32, sy: u32) -> bool {
		let (rows, columns) = Self::tiled_grid(n);
		columns * (PANE_MINIMUM + 1) - 1 <= sx && rows * (PANE_MINIMUM + 1) - 1 <= sy
	}

	/// layout_set_tiled: rows of `columns` panes, the last row taking the
	/// remaining width or a whole row when only one pane is left. The last
	/// pane of a full row and the last row absorb the remainder.
	pub fn tiled(panes: &[PaneId], sx: u32, sy: u32) -> Self {
		let n = panes.len();
		if n <= 1 {
			return match panes.first() {
				Some(pane) => Layout::new(*pane, sx, sy),
				None => Layout {
					cells: Vec::new(),
					root: None,
				},
			};
		}
		let (rows, columns) = Self::tiled_grid(n);
		let width = ((sx - (columns - 1)) / columns).max(PANE_MINIMUM);
		let height = ((sy - (rows - 1)) / rows).max(PANE_MINIMUM);

		let mut layout = Layout {
			cells: Vec::new(),
			root: None,
		};
		let root = layout.create(None);
		layout.set_size(root, sx, sy, 0, 0);
		layout.make_node(root, LayoutType::TopBottom);
		layout.root = Some(root);

		let mut next = 0usize;
		for _row in 0..rows as usize {
			if next >= n {
				break;
			}
			if n - next == 1 || columns == 1 {
				let cell = layout.create(Some(root));
				layout.set_size(cell, sx, height, 0, 0);
				layout.make_leaf(cell, panes[next]);
				layout.cells[root].cells.push(cell);
				next += 1;
				continue;
			}
			let row = layout.create(Some(root));
			layout.make_node(row, LayoutType::LeftRight);
			layout.set_size(row, sx, height, 0, 0);
			layout.cells[root].cells.push(row);
			let mut column = 0u32;
			while column < columns {
				let cell = layout.create(Some(row));
				layout.set_size(cell, width, height, 0, 0);
				layout.make_leaf(cell, panes[next]);
				layout.cells[row].cells.push(cell);
				next += 1;
				if next >= n {
					break;
				}
				column += 1;
			}
			let last = if column == columns {
				column - 1
			} else {
				column
			};
			let used = (last + 1) * (width + 1) - 1;
			if sx > used {
				if let Some(&cell) = layout.cells[row].cells.last() {
					layout.resize_adjust(cell, LayoutType::LeftRight, (sx - used) as i32);
				}
			}
		}
		let used = rows * height + rows - 1;
		if sy > used {
			if let Some(&cell) = layout.cells[root].cells.last() {
				layout.resize_adjust(cell, LayoutType::TopBottom, (sy - used) as i32);
			}
		}
		layout.fix_offsets();
		layout
	}

	/// layout_create_cell.
	fn create(&mut self, parent: Option<usize>) -> usize {
		self.cells.push(Cell {
			lc_type: LayoutType::WindowPane,
			parent,
			g: Geometry::sentinel(),
			wp: None,
			cells: Vec::new(),
		});
		self.cells.len() - 1
	}

	/// layout_free_cell: detach a subtree; the arena slot stays but unreachable.
	fn free_cell(&mut self, idx: usize) {
		let children = std::mem::take(&mut self.cells[idx].cells);
		for child in children {
			self.free_cell(child);
		}
		self.cells[idx].wp = None;
		self.cells[idx].lc_type = LayoutType::WindowPane;
	}

	/// layout_set_size.
	fn set_size(&mut self, idx: usize, sx: u32, sy: u32, xoff: i32, yoff: i32) {
		self.cells[idx].g.sx = sx;
		self.cells[idx].g.sy = sy;
		self.cells[idx].g.xoff = xoff;
		self.cells[idx].g.yoff = yoff;
	}

	/// layout_make_leaf.
	fn make_leaf(&mut self, idx: usize, pane: PaneId) {
		self.cells[idx].lc_type = LayoutType::WindowPane;
		self.cells[idx].cells.clear();
		self.cells[idx].wp = Some(pane);
	}

	/// layout_make_node.
	fn make_node(&mut self, idx: usize, lc_type: LayoutType) {
		self.cells[idx].lc_type = lc_type;
		self.cells[idx].cells.clear();
		self.cells[idx].wp = None;
	}

	/// layout_cell_is_tiled; floating cells do not exist here.
	fn is_tiled(&self, idx: usize) -> bool {
		self.cells[idx].lc_type == LayoutType::WindowPane
	}

	/// layout_cell_has_tiled_child.
	fn has_tiled_child(&self, idx: usize) -> bool {
		if self.cells[idx].lc_type == LayoutType::WindowPane {
			return false;
		}
		self.cells[idx]
			.cells
			.iter()
			.any(|child| self.is_tiled(*child) || self.has_tiled_child(*child))
	}

	/// layout_cell_is_last_tiled.
	fn is_last_tiled(&self, idx: usize) -> bool {
		let Some(parent) = self.cells[idx].parent else {
			return self.is_tiled(idx);
		};
		let last = self.cells[parent]
			.cells
			.iter()
			.rev()
			.find(|child| self.is_tiled(**child) || self.has_tiled_child(**child));
		last == Some(&idx)
	}

	/// layout_fix_offsets.
	pub fn fix_offsets(&mut self) {
		let Some(root) = self.root else { return };
		self.cells[root].g.xoff = 0;
		self.cells[root].g.yoff = 0;
		self.fix_offsets1(root);
	}

	/// layout_fix_offsets1.
	fn fix_offsets1(&mut self, idx: usize) {
		match self.cells[idx].lc_type {
			LayoutType::LeftRight => {
				let mut xoff = self.cells[idx].g.xoff;
				let yoff = self.cells[idx].g.yoff;
				for child in self.cells[idx].cells.clone() {
					if !self.is_tiled(child) && !self.has_tiled_child(child) {
						continue;
					}
					self.cells[child].g.xoff = xoff;
					self.cells[child].g.yoff = yoff;
					if self.cells[child].lc_type != LayoutType::WindowPane {
						self.fix_offsets1(child);
					}
					xoff += self.cells[child].g.sx as i32 + 1;
				}
			}
			_ => {
				let xoff = self.cells[idx].g.xoff;
				let mut yoff = self.cells[idx].g.yoff;
				for child in self.cells[idx].cells.clone() {
					if !self.is_tiled(child) && !self.has_tiled_child(child) {
						continue;
					}
					self.cells[child].g.xoff = xoff;
					self.cells[child].g.yoff = yoff;
					if self.cells[child].lc_type != LayoutType::WindowPane {
						self.fix_offsets1(child);
					}
					yoff += self.cells[child].g.sy as i32 + 1;
				}
			}
		}
	}

	/// layout_fix_panes: the geometry each pane should be resized to.
	pub fn panes(&self) -> Vec<(PaneId, Geometry)> {
		let Some(root) = self.root else {
			return Vec::new();
		};
		let mut out = Vec::new();
		self.collect_panes(root, &mut out);
		out
	}

	fn collect_panes(&self, idx: usize, out: &mut Vec<(PaneId, Geometry)>) {
		match self.cells[idx].lc_type {
			LayoutType::WindowPane => {
				if let Some(pane) = self.cells[idx].wp {
					out.push((pane, self.cells[idx].g));
				}
			}
			_ => {
				for child in &self.cells[idx].cells {
					self.collect_panes(*child, out);
				}
			}
		}
	}

	pub fn leaves(&self) -> Vec<PaneId> {
		self.panes().into_iter().map(|(id, _)| id).collect()
	}

	/// layout_count_cells.
	pub fn count_cells(&self) -> u32 {
		let Some(root) = self.root else { return 0 };
		self.count_cells1(root)
	}

	fn count_cells1(&self, idx: usize) -> u32 {
		if self.cells[idx].lc_type == LayoutType::WindowPane {
			return 1;
		}
		self.cells[idx]
			.cells
			.iter()
			.map(|child| self.count_cells1(*child))
			.sum()
	}

	/// layout_resize_check.
	fn resize_check(&self, idx: usize, lc_type: LayoutType) -> u32 {
		if !self.is_tiled(idx) && !self.has_tiled_child(idx) {
			return 0;
		}
		if self.cells[idx].lc_type == LayoutType::WindowPane {
			let available = if lc_type == LayoutType::LeftRight {
				self.cells[idx].g.sx
			} else {
				self.cells[idx].g.sy
			};
			if available > PANE_MINIMUM {
				available - PANE_MINIMUM
			} else {
				0
			}
		} else if self.cells[idx].lc_type == lc_type {
			self.cells[idx]
				.cells
				.iter()
				.map(|child| self.resize_check(*child, lc_type))
				.sum()
		} else {
			let mut minimum = u32::MAX;
			for child in &self.cells[idx].cells {
				if !self.is_tiled(*child) && !self.has_tiled_child(*child) {
					continue;
				}
				let available = self.resize_check(*child, lc_type);
				if available < minimum {
					minimum = available;
				}
			}
			minimum
		}
	}

	/// layout_resize_adjust.
	fn resize_adjust(&mut self, idx: usize, lc_type: LayoutType, change: i32) {
		if lc_type == LayoutType::LeftRight {
			self.cells[idx].g.sx = (self.cells[idx].g.sx as i32 + change).max(0) as u32;
		} else {
			self.cells[idx].g.sy = (self.cells[idx].g.sy as i32 + change).max(0) as u32;
		}

		if lc_type == LayoutType::WindowPane {
			return;
		}

		if self.cells[idx].lc_type != lc_type {
			for child in self.cells[idx].cells.clone() {
				if !self.is_tiled(child) && !self.has_tiled_child(child) {
					continue;
				}
				self.resize_adjust(child, lc_type, change);
			}
			return;
		}

		if !self.has_tiled_child(idx) {
			return;
		}

		let mut change = change;
		while change != 0 {
			let mut changed = false;
			for child in self.cells[idx].cells.clone() {
				if change == 0 {
					break;
				}
				if !self.is_tiled(child) && !self.has_tiled_child(child) {
					continue;
				}
				if change > 0 {
					self.resize_adjust(child, lc_type, 1);
					change -= 1;
					changed = true;
					continue;
				}
				if self.resize_check(child, lc_type) > 0 {
					self.resize_adjust(child, lc_type, -1);
					change += 1;
					changed = true;
				}
			}
			if !changed {
				break;
			}
		}
	}

	/// layout_resize_set_size.
	pub fn resize_set_size(&mut self, idx: usize, lc_type: LayoutType, size: u32) {
		let current = if lc_type == LayoutType::LeftRight {
			self.cells[idx].g.sx
		} else {
			self.cells[idx].g.sy
		};
		self.resize_adjust(idx, lc_type, size as i32 - current as i32);
	}

	/// layout_cell_get_neighbour_dir.
	fn neighbour_dir(&self, idx: usize, forward: bool) -> Option<usize> {
		let parent = self.cells[idx].parent?;
		let siblings = &self.cells[parent].cells;
		let pos = siblings.iter().position(|cell| *cell == idx)? as isize;
		let mut next = pos;
		loop {
			next += if forward { 1 } else { -1 };
			if next < 0 || next >= siblings.len() as isize {
				return None;
			}
			let child = siblings[next as usize];
			if self.is_tiled(child) || self.has_tiled_child(child) {
				return Some(child);
			}
		}
	}

	/// layout_cell_get_neighbour. Prefers cells after this one.
	fn get_neighbour(&self, idx: usize) -> Option<usize> {
		let parent = self.cells[idx].parent?;
		let forward = self.cells[parent].cells.last() != Some(&idx);
		self.neighbour_dir(idx, forward)
			.or_else(|| self.neighbour_dir(idx, !forward))
	}

	/// layout_destroy_cell, returning the cell that replaced a collapsed parent.
	fn destroy_cell(&mut self, idx: usize) {
		let Some(parent) = self.cells[idx].parent else {
			if self.root == Some(idx) {
				self.root = None;
				self.free_cell(idx);
			}
			return;
		};

		if !self.is_tiled(idx) {
			self.cells[parent].cells.retain(|cell| *cell != idx);
			self.free_cell(idx);
		} else {
			if let Some(other) = self.get_neighbour(idx) {
				let change = if self.cells[parent].lc_type == LayoutType::LeftRight {
					self.cells[idx].g.sx as i32 + 1
				} else {
					self.cells[idx].g.sy as i32 + 1
				};
				self.resize_adjust(other, self.cells[parent].lc_type, change);
			}
			self.cells[parent].cells.retain(|cell| *cell != idx);
			self.free_cell(idx);
		}

		/*
		 * If the parent now has one cell, remove the parent from the tree and
		 * replace it by that cell.
		 */
		if self.cells[parent].cells.len() == 1 {
			let only = self.cells[parent].cells.remove(0);
			let grand = self.cells[parent].parent;
			self.cells[only].parent = grand;
			match grand {
				None => {
					if self.is_tiled(only) {
						self.cells[only].g.xoff = 0;
						self.cells[only].g.yoff = 0;
					}
					self.root = Some(only);
				}
				Some(g) => {
					if let Some(pos) = self.cells[g].cells.iter().position(|cell| *cell == parent) {
						self.cells[g].cells[pos] = only;
					}
				}
			}
			self.free_cell(parent);
		}
	}

	/// layout_destroy_cell for the cell holding a pane.
	pub fn destroy(&mut self, pane: PaneId) {
		let Some(idx) = self.index_of(pane) else {
			return;
		};
		self.destroy_cell(idx);
		self.fix_offsets();
	}

	/// layout_resize: fit the layout to a new window size.
	pub fn resize(&mut self, sx: u32, sy: u32) {
		let Some(root) = self.root else { return };
		let sx = sx.max(PANE_MINIMUM);
		let sy = sy.max(PANE_MINIMUM);

		let mut xchange = sx as i32 - self.cells[root].g.sx as i32;
		let xlimit = self.resize_check(root, LayoutType::LeftRight) as i32;
		if xchange < 0 && xchange < -xlimit {
			xchange = -xlimit;
		}
		if xlimit == 0 {
			xchange = if sx <= self.cells[root].g.sx {
				0
			} else {
				sx as i32 - self.cells[root].g.sx as i32
			};
		}
		if xchange != 0 {
			self.resize_adjust(root, LayoutType::LeftRight, xchange);
		}

		let mut ychange = sy as i32 - self.cells[root].g.sy as i32;
		let ylimit = self.resize_check(root, LayoutType::TopBottom) as i32;
		if ychange < 0 && ychange < -ylimit {
			ychange = -ylimit;
		}
		if ylimit == 0 {
			ychange = if sy <= self.cells[root].g.sy {
				0
			} else {
				sy as i32 - self.cells[root].g.sy as i32
			};
		}
		if ychange != 0 {
			self.resize_adjust(root, LayoutType::TopBottom, ychange);
		}

		self.fix_offsets();
	}

	/// layout_resize_layout.
	fn resize_layout(&mut self, idx: usize, lc_type: LayoutType, change: i32, opposite: bool) {
		let mut needed = change;
		while needed != 0 {
			let size = if change > 0 {
				self.resize_pane_grow(idx, lc_type, needed, opposite)
			} else {
				self.resize_pane_shrink(idx, lc_type, needed)
			};
			if change > 0 {
				needed -= size;
			} else {
				needed += size;
			}
			if size == 0 {
				break;
			}
		}
		self.fix_offsets();
	}

	/// layout_resize_pane: grow or shrink one pane by `change` cells.
	pub fn resize_pane(&mut self, pane: PaneId, lc_type: LayoutType, change: i32, opposite: bool) {
		let Some(mut lc) = self.index_of(pane) else {
			return;
		};
		let mut parent = self.cells[lc].parent;
		while let Some(p) = parent {
			if self.cells[p].lc_type == lc_type {
				break;
			}
			lc = p;
			parent = self.cells[p].parent;
		}
		let Some(_parent) = parent else { return };

		/* If this is the last tiled cell, move back one. */
		if self.is_last_tiled(lc) {
			let Some(previous) = self.neighbour_dir(lc, false) else {
				return;
			};
			lc = previous;
		}

		self.resize_layout(lc, lc_type, change, opposite);
	}

	/// layout_resize_pane_grow.
	fn resize_pane_grow(
		&mut self,
		idx: usize,
		lc_type: LayoutType,
		needed: i32,
		opposite: bool,
	) -> i32 {
		let mut remove = self.neighbour_dir(idx, true);
		let mut size = 0u32;
		while let Some(cell) = remove {
			size = self.resize_check(cell, lc_type);
			if size > 0 {
				break;
			}
			remove = self.neighbour_dir(cell, true);
		}

		if opposite && remove.is_none() {
			remove = self.neighbour_dir(idx, false);
			while let Some(cell) = remove {
				size = self.resize_check(cell, lc_type);
				if size > 0 {
					break;
				}
				remove = self.neighbour_dir(cell, false);
			}
		}

		let Some(remove) = remove else { return 0 };
		if size > needed as u32 {
			size = needed as u32;
		}
		self.resize_adjust(idx, lc_type, size as i32);
		self.resize_adjust(remove, lc_type, -(size as i32));
		size as i32
	}

	/// layout_resize_pane_shrink.
	fn resize_pane_shrink(&mut self, idx: usize, lc_type: LayoutType, needed: i32) -> i32 {
		let mut remove = idx;
		let mut size;
		loop {
			size = self.resize_check(remove, lc_type);
			if size != 0 {
				break;
			}
			let Some(previous) = self.neighbour_dir(remove, false) else {
				return 0;
			};
			remove = previous;
		}

		let Some(add) = self.neighbour_dir(idx, true) else {
			return 0;
		};

		let mut size = size as i32;
		if size > -needed {
			size = -needed;
		}
		self.resize_adjust(add, lc_type, size);
		self.resize_adjust(remove, lc_type, -size);
		size
	}

	/// layout_split_check_space.
	fn split_check_space(&self, idx: usize, lc_type: LayoutType) -> bool {
		match lc_type {
			LayoutType::LeftRight => self.cells[idx].g.sx >= PANE_MINIMUM * 2 + 1,
			LayoutType::TopBottom => self.cells[idx].g.sy >= PANE_MINIMUM * 2 + 1,
			LayoutType::WindowPane => false,
		}
	}

	/// layout_split_sizes.
	fn split_sizes(&self, idx: usize, size: i32, before: bool, lc_type: LayoutType) -> (u32, u32) {
		let ss = if lc_type == LayoutType::LeftRight {
			self.cells[idx].g.sx
		} else {
			self.cells[idx].g.sy
		};
		let mut s2 = if size < 0 {
			(ss + 1) / 2 - 1
		} else if before {
			ss - size as u32 - 1
		} else {
			size as u32
		};
		if s2 < PANE_MINIMUM {
			s2 = PANE_MINIMUM;
		} else if s2 > ss - 2 {
			s2 = ss - 2;
		}
		(ss - 1 - s2, s2)
	}

	/// layout_replace_with_node.
	fn replace_with_node(&mut self, idx: usize, lc_type: LayoutType) -> usize {
		let parent = self.cells[idx].parent;
		let node = self.create(parent);
		self.make_node(node, lc_type);
		let g = self.cells[idx].g;
		self.set_size(node, g.sx, g.sy, g.xoff, g.yoff);
		match parent {
			None => self.root = Some(node),
			Some(p) => {
				if let Some(pos) = self.cells[p].cells.iter().position(|cell| *cell == idx) {
					self.cells[p].cells[pos] = node;
				}
			}
		}
		self.cells[idx].parent = Some(node);
		self.cells[node].cells.insert(0, idx);
		node
	}

	/// layout_split_pane followed by layout_assign_pane, for a half split.
	pub fn split(&mut self, pane: PaneId, new: PaneId, dir: Dir) -> bool {
		self.split_sized(pane, new, dir, -1)
	}

	/// layout_split_check_space, asked before spawning a pane so a refused
	/// split does not leave an orphan process behind.
	pub fn can_split(&self, pane: PaneId, dir: Dir) -> bool {
		let lc_type = match dir {
			Dir::Right => LayoutType::LeftRight,
			Dir::Down => LayoutType::TopBottom,
		};
		let Some(idx) = self.index_of(pane) else {
			return false;
		};
		self.cells[idx].lc_type == LayoutType::WindowPane && self.split_check_space(idx, lc_type)
	}

	/// layout_split_pane with an explicit size hint in cells (-1 is half).
	pub fn split_sized(&mut self, pane: PaneId, new: PaneId, dir: Dir, size: i32) -> bool {
		let lc_type = match dir {
			Dir::Right => LayoutType::LeftRight,
			Dir::Down => LayoutType::TopBottom,
		};
		let Some(lc) = self.index_of(pane) else {
			return false;
		};
		if self.cells[lc].lc_type != LayoutType::WindowPane {
			return false;
		}
		if !self.split_check_space(lc, lc_type) {
			return false;
		}

		let sx = self.cells[lc].g.sx;
		let sy = self.cells[lc].g.sy;
		let xoff = self.cells[lc].g.xoff;
		let yoff = self.cells[lc].g.yoff;
		let (size1, size2) = self.split_sizes(lc, size, false, lc_type);

		let same_type_parent = self.cells[lc]
			.parent
			.map(|p| self.cells[p].lc_type == lc_type)
			.unwrap_or(false);
		let new_cell = if same_type_parent {
			let parent = self.cells[lc].parent.unwrap();
			let cell = self.create(Some(parent));
			let pos = self.cells[parent]
				.cells
				.iter()
				.position(|c| *c == lc)
				.unwrap();
			self.cells[parent].cells.insert(pos + 1, cell);
			cell
		} else {
			let parent = self.replace_with_node(lc, lc_type);
			let cell = self.create(Some(parent));
			self.cells[parent].cells.push(cell);
			cell
		};

		match lc_type {
			LayoutType::LeftRight => {
				self.set_size(lc, size1, sy, xoff, yoff);
				self.set_size(new_cell, size2, sy, xoff + size1 as i32 + 1, yoff);
			}
			_ => {
				self.set_size(lc, sx, size1, xoff, yoff);
				self.set_size(new_cell, sx, size2, xoff, yoff + size1 as i32 + 1);
			}
		}
		self.make_leaf(new_cell, new);
		true
	}

	fn index_of(&self, pane: PaneId) -> Option<usize> {
		let root = self.root?;
		self.index_of1(root, pane)
	}

	fn index_of1(&self, idx: usize, pane: PaneId) -> Option<usize> {
		if self.cells[idx].lc_type == LayoutType::WindowPane {
			return (self.cells[idx].wp == Some(pane)).then_some(idx);
		}
		self.cells[idx]
			.cells
			.iter()
			.find_map(|child| self.index_of1(*child, pane))
	}

	/// Pixel rectangles in pane order, mapping cell geometry to the canvas.
	pub fn rects(&self, area: Rect, cell_w: i32, cell_h: i32) -> Vec<(PaneId, Rect)> {
		self.panes()
			.into_iter()
			.map(|(pane, g)| {
				let left = area.left + g.xoff * cell_w;
				let top = area.top + g.yoff * cell_h;
				(
					pane,
					Rect::new(
						left,
						top,
						left + g.sx as i32 * cell_w,
						top + g.sy as i32 * cell_h,
					),
				)
			})
			.collect()
	}

	fn full_size_offset(&self, pane: PaneId) -> Option<(i32, i32, u32, u32)> {
		let idx = self.index_of(pane)?;
		let g = self.cells[idx].g;
		Some((g.xoff, g.yoff, g.sx, g.sy))
	}

	/// window_pane_find_left / find_right / find_up / find_down. Among panes
	/// on the edge, tmux picks the most recently active; without that history
	/// we take the first in pane order.
	pub fn neighbour(&self, from: PaneId, dx: i32, dy: i32) -> Option<PaneId> {
		let (fxoff, fyoff, fsx, fsy) = self.full_size_offset(from)?;
		let (window_sx, window_sy) = self.root_size();
		let mut best: Option<PaneId> = None;
		for (pane, g) in self.panes() {
			if pane == from {
				continue;
			}
			let (xoff, yoff, sx, sy) = (g.xoff, g.yoff, g.sx, g.sy);
			let found = if dx < 0 {
				let mut edge = fxoff;
				if edge == 0 {
					edge = window_sx as i32 + 1;
				}
				let (top, bottom) = (fyoff, fyoff + fsy as i32);
				let end = yoff + sy as i32 - 1;
				xoff + sx as i32 + 1 == edge
					&& ((yoff < top && end > bottom)
						|| (yoff >= top && yoff <= bottom)
						|| (end >= top && end <= bottom))
			} else if dx > 0 {
				let mut edge = fxoff + fsx as i32 + 1;
				if edge >= window_sx as i32 {
					edge = 0;
				}
				let (top, bottom) = (fyoff, fyoff + fsy as i32);
				let end = yoff + sy as i32 - 1;
				xoff == edge
					&& ((yoff < top && end > bottom)
						|| (yoff >= top && yoff <= bottom)
						|| (end >= top && end <= bottom))
			} else if dy < 0 {
				let mut edge = fyoff;
				if edge == 0 {
					edge = window_sy as i32 + 1;
				}
				let (left, right) = (fxoff, fxoff + fsx as i32);
				let end = xoff + sx as i32 - 1;
				yoff + sy as i32 + 1 == edge
					&& ((xoff < left && end > right)
						|| (xoff >= left && xoff <= right)
						|| (end >= left && end <= right))
			} else {
				let mut edge = fyoff + fsy as i32 + 1;
				if edge >= window_sy as i32 {
					edge = 0;
				}
				let (left, right) = (fxoff, fxoff + fsx as i32);
				let end = xoff + sx as i32 - 1;
				yoff == edge
					&& ((xoff < left && end > right)
						|| (xoff >= left && xoff <= right)
						|| (end >= left && end <= right))
			};
			if found {
				best = Some(pane);
				break;
			}
		}
		best
	}

	pub fn root_size(&self) -> (u32, u32) {
		match self.root {
			Some(root) => (self.cells[root].g.sx, self.cells[root].g.sy),
			None => (0, 0),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	// Geometry expectations transcribed from vendor/tmux-3.8/regress/pane-ops.sh:
	// an 80x24 window, split-window -h -l 20 puts a 20x24 pane to the right of a
	// 59x24 pane, and -v -l 25% of 24 gives 6 rows below 17 with one separator.

	#[test]
	fn split_horizontal_keeps_requested_cells() {
		let mut layout = Layout::new(0, 80, 24);
		assert!(layout.split_sized(0, 1, Dir::Right, 20));
		let panes = layout.panes();
		assert_eq!(panes[0].0, 0);
		assert_eq!((panes[0].1.xoff, panes[0].1.sx), (0, 59));
		assert_eq!(panes[0].1.sy, 24);
		assert_eq!(panes[1].0, 1);
		assert_eq!((panes[1].1.xoff, panes[1].1.sx), (60, 20));
		assert_eq!(panes[1].1.sy, 24);
	}

	#[test]
	fn split_vertical_uses_percentage_of_pane() {
		let mut layout = Layout::new(0, 80, 24);
		assert!(layout.split_sized(0, 1, Dir::Right, 20));
		assert!(layout.split_sized(0, 2, Dir::Down, 6));
		let panes = layout.panes();
		let by_id = |id| panes.iter().find(|(pane, _)| *pane == id).unwrap().1;
		assert_eq!(
			(by_id(0).xoff, by_id(0).yoff, by_id(0).sx, by_id(0).sy),
			(0, 0, 59, 17)
		);
		assert_eq!(
			(by_id(2).xoff, by_id(2).yoff, by_id(2).sx, by_id(2).sy),
			(0, 18, 59, 6)
		);
	}

	#[test]
	fn half_split_matrices_add_up() {
		let mut layout = Layout::new(0, 80, 24);
		assert!(layout.split(0, 1, Dir::Right));
		let panes = layout.panes();
		let widths: Vec<u32> = panes.iter().map(|(_, g)| g.sx).collect();
		assert_eq!(widths, vec![40, 39]);
		assert_eq!(panes[1].1.xoff, 41);
	}

	#[test]
	fn closing_a_pane_redistributes_its_cells() {
		let mut layout = Layout::new(0, 80, 24);
		assert!(layout.split_sized(0, 1, Dir::Right, 20));
		assert!(layout.split_sized(0, 2, Dir::Right, 19));
		let before: Vec<u32> = layout.panes().iter().map(|(_, g)| g.sx).collect();
		assert_eq!(before, vec![39, 19, 20]);
		layout.destroy(0);
		let after = layout.panes();
		assert_eq!(after.len(), 2);
		assert_eq!(after[0].0, 2);
		assert_eq!((after[0].1.xoff, after[0].1.sx), (0, 59));
		assert_eq!(after[1].0, 1);
		assert_eq!((after[1].1.xoff, after[1].1.sx), (60, 20));
	}

	#[test]
	fn resize_pane_moves_the_border() {
		let mut layout = Layout::new(0, 80, 24);
		assert!(layout.split_sized(0, 1, Dir::Right, 20));
		layout.resize_pane(1, LayoutType::LeftRight, -2, true);
		let panes = layout.panes();
		assert_eq!((panes[0].1.sx, panes[0].1.xoff), (57, 0));
		assert_eq!((panes[1].1.sx, panes[1].1.xoff), (22, 58));
	}

	#[test]
	fn window_shrink_keeps_layout_fitting() {
		let mut layout = Layout::new(0, 80, 24);
		assert!(layout.split(0, 1, Dir::Right));
		assert!(layout.split(0, 2, Dir::Down));
		layout.resize(60, 20);
		let mut max_right = 0;
		let mut max_bottom = 0;
		for (_, g) in layout.panes() {
			max_right = max_right.max(g.xoff + g.sx as i32);
			max_bottom = max_bottom.max(g.yoff + g.sy as i32);
		}
		assert_eq!(max_right, 60);
		assert_eq!(max_bottom, 20);
	}

	#[test]
	fn neighbour_by_direction() {
		let mut layout = Layout::new(0, 80, 24);
		assert!(layout.split(0, 1, Dir::Right));
		assert_eq!(layout.neighbour(0, 1, 0), Some(1));
		assert_eq!(layout.neighbour(1, -1, 0), Some(0));
		// Down from the left column wraps to the top edge, but the right column
		// does not overlap horizontally, so there is no candidate.
		assert_eq!(layout.neighbour(0, 0, 1), None);

		let mut stacked = Layout::new(0, 80, 24);
		assert!(stacked.split(0, 1, Dir::Down));
		assert_eq!(stacked.neighbour(0, 0, 1), Some(1));
		// Below the bottom pane wraps back to the top one.
		assert_eq!(stacked.neighbour(1, 0, 1), Some(0));
	}

	// layout_set_tiled from vendor/tmux-3.8/layout-set.c: rows grow first,
	// columns join when the product is still short. These are the 80x24 cells
	// that algorithm produces for 1 through 6 panes.
	#[test]
	fn tiled_two_panes_stack() {
		let layout = Layout::tiled(&[0, 1], 80, 24);
		let panes = layout.panes();
		assert_eq!(
			(
				panes[0].1.xoff,
				panes[0].1.yoff,
				panes[0].1.sx,
				panes[0].1.sy
			),
			(0, 0, 80, 11)
		);
		assert_eq!(
			(
				panes[1].1.xoff,
				panes[1].1.yoff,
				panes[1].1.sx,
				panes[1].1.sy
			),
			(0, 12, 80, 12)
		);
	}

	#[test]
	fn tiled_three_panes_fill_a_second_row() {
		let layout = Layout::tiled(&[0, 1, 2], 80, 24);
		let panes = layout.panes();
		assert_eq!(
			(
				panes[0].1.xoff,
				panes[0].1.yoff,
				panes[0].1.sx,
				panes[0].1.sy
			),
			(0, 0, 39, 11)
		);
		assert_eq!(
			(
				panes[1].1.xoff,
				panes[1].1.yoff,
				panes[1].1.sx,
				panes[1].1.sy
			),
			(40, 0, 40, 11)
		);
		assert_eq!(
			(
				panes[2].1.xoff,
				panes[2].1.yoff,
				panes[2].1.sx,
				panes[2].1.sy
			),
			(0, 12, 80, 12)
		);
	}

	#[test]
	fn tiled_four_panes_are_a_two_by_two_grid() {
		let layout = Layout::tiled(&[0, 1, 2, 3], 80, 24);
		let panes = layout.panes();
		let cells: Vec<(i32, i32, u32, u32)> = panes
			.iter()
			.map(|(_, g)| (g.xoff, g.yoff, g.sx, g.sy))
			.collect();
		assert_eq!(
			cells,
			vec![
				(0, 0, 39, 11),
				(40, 0, 40, 11),
				(0, 12, 39, 12),
				(40, 12, 40, 12)
			]
		);
	}

	#[test]
	fn tiled_five_and_six_panes_use_three_rows() {
		let five = Layout::tiled(&[0, 1, 2, 3, 4], 80, 24);
		let cells: Vec<(i32, i32, u32, u32)> = five
			.panes()
			.iter()
			.map(|(_, g)| (g.xoff, g.yoff, g.sx, g.sy))
			.collect();
		assert_eq!(
			cells,
			vec![
				(0, 0, 39, 7),
				(40, 0, 40, 7),
				(0, 8, 39, 7),
				(40, 8, 40, 7),
				(0, 16, 80, 8)
			]
		);

		let six = Layout::tiled(&[0, 1, 2, 3, 4, 5], 80, 24);
		let cells: Vec<(i32, i32, u32, u32)> = six
			.panes()
			.iter()
			.map(|(_, g)| (g.xoff, g.yoff, g.sx, g.sy))
			.collect();
		assert_eq!(
			cells,
			vec![
				(0, 0, 39, 7),
				(40, 0, 40, 7),
				(0, 8, 39, 7),
				(40, 8, 40, 7),
				(0, 16, 39, 8),
				(40, 16, 40, 8)
			]
		);
	}

	#[test]
	fn tiled_refuses_a_grid_below_the_minimum_cell() {
		assert!(Layout::tiled_fits(6, 80, 24));
		assert!(Layout::tiled_fits(1, 1, 1));
		assert!(!Layout::tiled_fits(6, 80, 4));
	}

	#[test]
	fn nested_same_direction_stays_one_row() {
		let mut layout = Layout::new(0, 80, 24);
		assert!(layout.split(0, 1, Dir::Right));
		assert!(layout.split(1, 2, Dir::Right));
		let panes = layout.panes();
		let columns: Vec<i32> = panes.iter().map(|(_, g)| g.xoff).collect();
		assert_eq!(columns, vec![0, 41, 61]);
		for (_, g) in &panes {
			assert_eq!(g.yoff, 0);
			assert_eq!(g.sy, 24);
		}
	}
}
