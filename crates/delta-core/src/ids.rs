//! 对象的地址：不透明、只增不减、绝不复用（herdr 的 `w1:w1:p1` 是同一形状）。

macro_rules! ids {
	($($name:ident),* $(,)?) => {
		$(
			#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
			pub struct $name(pub u64);
		)*
	};
}

ids!(ActorId, WindowId, EventId, GroupId, EditId);

/// 所有计数器在一起：`Delta` 只有一个分配器，所以两个窗口不可能拿到同一个号。
#[derive(Debug, Default)]
pub(crate) struct Counter(u64);

impl Counter {
	pub(crate) fn next(&mut self) -> u64 {
		self.0 += 1;
		self.0
	}
}

#[derive(Debug, Default)]
pub(crate) struct Ids {
	pub(crate) actors: Counter,
	pub(crate) windows: Counter,
	pub(crate) events: Counter,
	pub(crate) groups: Counter,
	pub(crate) edits: Counter,
}

macro_rules! alloc {
	($($method:ident : $field:ident => $name:ident),* $(,)?) => {
		impl Ids {
			$(
				pub(crate) fn $method(&mut self) -> $name {
					$name(self.$field.next())
				}
			)*
		}
	};
}

alloc!(
	actor: actors => ActorId,
	window: windows => WindowId,
	event: events => EventId,
	group: groups => GroupId,
	edit: edits => EditId,
);
