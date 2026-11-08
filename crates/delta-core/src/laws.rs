//! 每条测试以一个法则编号命名：违反法则会以测试失败出现，而不是以一次讨论出现。

use std::path::{Path, PathBuf};

use crate::{
	CloseDenial, Delta, Esc, EventId, GroupClosed, KeymapOwner, OwnershipMode, Subject, Takeover,
};

fn path(name: &str) -> PathBuf {
	Path::new("/repo").join(name)
}

fn scenario() -> (Delta, crate::ActorId, crate::ActorId) {
	let mut d = Delta::null();
	let human = d.add_human();
	let ai = d.add_ai();
	(d, human, ai)
}

#[test]
fn law1_running_event_pins_its_window() {
	let (mut d, _human, ai) = scenario();
	let console = d.console(ai).unwrap();
	let event = d
		.start_event(
			ai,
			Subject::Task {
				label: "cargo build".into(),
			},
		)
		.unwrap();
	assert_eq!(
		d.request_close(console),
		Err(CloseDenial::Pinned {
			window: console,
			events: vec![event]
		})
	);
}

#[test]
fn law10_one_window_can_bind_several_events_and_counts_each() {
	let (mut d, _human, ai) = scenario();
	let console = d.console(ai).unwrap();
	let first = d
		.start_event(ai, Subject::Task { label: "a".into() })
		.unwrap();
	let second = d
		.start_event(ai, Subject::Task { label: "b".into() })
		.unwrap();
	d.finish_event(first);
	assert_eq!(
		d.request_close(console),
		Err(CloseDenial::Pinned {
			window: console,
			events: vec![second]
		}),
		"先结束的那个事件解锁不了另一个还在往里写的窗口"
	);
	d.finish_event(second);
	assert_eq!(d.close(console), Ok(()));
}

#[test]
fn law2_edit_pins_only_while_that_edit_is_in_flight() {
	let (mut d, _human, ai) = scenario();
	let token = d.begin_edit(ai, path("src/main.rs")).unwrap();
	let windows = d.windows_for_path(&path("src/main.rs"));
	assert_eq!(windows.len(), 1);
	let window = windows[0];
	assert_eq!(d.window(window).unwrap().pin_count(), 1);
	assert!(matches!(
		d.request_close(window),
		Err(CloseDenial::Pinned { .. })
	));
	d.end_edit(token);
	assert_eq!(d.window(window).unwrap().pin_count(), 0);
	assert_eq!(d.request_close(window), Ok(()), "编辑完的可以关");
}

#[test]
fn law11_and_law3_edit_without_a_window_opens_one_and_never_duplicates_it() {
	let (mut d, _human, ai) = scenario();
	let first = d.begin_edit(ai, path("src/lib.rs")).unwrap();
	let second = d.begin_edit(ai, path("src/lib.rs")).unwrap();
	assert_eq!(d.windows_for_path(&path("src/lib.rs")).len(), 1);
	assert_ne!(first, second, "两次编辑是两个在飞的编辑，不是两个窗口");
	d.end_edit(first);
	d.end_edit(second);
}

#[test]
fn law11_the_window_the_machine_opens_does_not_steal_focus() {
	let (mut d, human, ai) = scenario();
	let pane = d.open_terminal(path("/repo"));
	d.focus(human, pane);
	let _ = d.begin_edit(ai, path("src/new.rs"));
	// 看见 ≠ 焦点：核心层承诺的是键盘不动，怎么显示（分屏 / overlay / 图层）归 chrome。
	assert_eq!(d.keyboard.focused, Some(pane), "看见不等于焦点");
	assert_eq!(d.window(pane).unwrap().ownership, OwnershipMode::Normal);
}

#[test]
fn law46_ai_delete_opens_no_window_and_shows_nothing() {
	let (mut d, _human, ai) = scenario();
	d.begin_edit(ai, path("gone.rs")).ok();
	let before = d.windows_for_path(&path("gone.rs")).len();
	d.ai_delete(ai, path("other.rs"));
	assert_eq!(d.windows_for_path(&path("other.rs")).len(), 0);
	assert_eq!(d.windows_for_path(&path("gone.rs")).len(), before);
}

#[test]
fn law4_takeover_of_a_window_the_human_never_showed_needs_no_consent() {
	let (mut d, _human, ai) = scenario();
	let pane = d.open_terminal(path("/repo"));
	assert_eq!(d.request_takeover(ai, pane), Takeover::Granted);
	assert_eq!(d.window(pane).unwrap().ownership, OwnershipMode::Agent(ai));
}

#[test]
fn law4_a_human_foreground_visit_asks_once_and_consent_clears_the_record() {
	let (mut d, human, ai) = scenario();
	let pane = d.open_terminal(path("/repo"));
	d.focus(human, pane);
	assert_eq!(
		d.request_takeover(ai, pane),
		Takeover::NeedsConsent { window: pane, ai }
	);
	assert_eq!(
		d.window(pane).unwrap().ownership,
		OwnershipMode::Normal,
		"没同意之前不许进 agent"
	);
	d.grant_takeover(ai, pane);
	assert_eq!(d.window(pane).unwrap().ownership, OwnershipMode::Agent(ai));
	d.release_agent(ai, pane);
	assert_eq!(d.request_takeover(ai, pane), Takeover::Granted, "一次就行");
}

#[test]
fn law4_consent_is_recorded_per_window_and_ai() {
	let (mut d, human, first) = scenario();
	let second = d.add_ai();
	let pane = d.open_terminal(path("/repo"));
	d.focus(human, pane);
	d.grant_takeover(first, pane);
	d.release_agent(first, pane);
	assert_eq!(
		d.request_takeover(second, pane),
		Takeover::NeedsConsent {
			window: pane,
			ai: second
		}
	);
}

#[test]
fn law4_reading_another_editors_buffer_is_not_a_takeover() {
	let (mut d, human, ai) = scenario();
	let window = d.open_editor(human, path("src/read_only.rs"));
	assert!(d.peek(ai, window).is_some(), "读取不申请");
	assert_eq!(d.window(window).unwrap().ownership, OwnershipMode::Normal);
}

#[test]
fn law16_being_in_normal_does_not_let_you_close_an_agent_window() {
	let (mut d, human, ai) = scenario();
	let a = d.open_terminal(path("/repo/a"));
	let b = d.open_terminal(path("/repo/b"));
	d.enter_agent(ai, b);
	d.focus(human, a);
	d.set_input(crate::InputMode::Insert);
	assert_eq!(d.esc(), Some(Esc::ToNormal));
	assert_eq!(
		d.request_close(b),
		Err(CloseDenial::Owned {
			window: b,
			ai,
			mode: OwnershipMode::Agent(ai)
		}),
		"就算你是 normal，你也关闭不了 B 窗口"
	);
}

#[test]
fn law14_the_ai_alone_leaves_agent_mode() {
	let (mut d, human, ai) = scenario();
	let pane = d.open_terminal(path("/repo"));
	d.request_takeover(ai, pane);
	assert!(
		!d.release_agent(human, pane),
		"退出由 AI 决定，人类的出口是调度器的停止"
	);
	assert!(d.release_agent(ai, pane));
}

#[test]
fn law15_share_cannot_be_closed_or_left() {
	let (mut d, human, ai) = scenario();
	let pane = d.open_terminal(path("/repo"));
	d.request_takeover(ai, pane);
	d.ask_share(ai, pane);
	assert_eq!(d.window(pane).unwrap().ownership, OwnershipMode::Share(ai));
	assert!(matches!(
		d.request_close(pane),
		Err(CloseDenial::Owned { .. })
	));
	assert!(!d.release_agent(human, pane), "share 也不能由人类退出");
	d.share_resolved_in_group(pane);
	assert_eq!(d.window(pane).unwrap().ownership, OwnershipMode::Agent(ai));
}

#[test]
fn law8_stop_is_a_separate_surface_and_releases_the_pin() {
	let (mut d, _human, ai) = scenario();
	let console = d.console(ai).unwrap();
	let event = d
		.start_event(
			ai,
			Subject::Task {
				label: "hangs".into(),
			},
		)
		.unwrap();
	assert!(matches!(
		d.request_close(console),
		Err(CloseDenial::Pinned { .. })
	));
	assert!(d.stop_event(event));
	assert_eq!(d.request_close(console), Ok(()), "窗口还在，但是可以关闭了");
}

#[test]
fn law8_stop_clears_the_ownership_mode_too() {
	let (mut d, _human, ai) = scenario();
	let pane = d.open_terminal(path("/repo"));
	d.request_takeover(ai, pane);
	let event = d
		.start_event_in(
			ai,
			pane,
			Subject::Task {
				label: "gcc -o".into(),
			},
		)
		.unwrap();
	d.stop_event(event);
	assert_eq!(d.window(pane).unwrap().ownership, OwnershipMode::Normal);
	assert_eq!(d.request_close(pane), Ok(()));
}

#[test]
fn law21_the_scheduler_and_the_live_stream_are_one_window() {
	let (mut d, _human, ai) = scenario();
	let console = d.console(ai).unwrap();
	let one = d
		.start_event(
			ai,
			Subject::Task {
				label: "one".into(),
			},
		)
		.unwrap();
	let two = d
		.start_event(
			ai,
			Subject::Task {
				label: "two".into(),
			},
		)
		.unwrap();
	let view: Vec<EventId> = d.scheduler_view(ai).iter().map(|e| e.id).collect();
	assert_eq!(view, vec![one, two]);
	assert!(d.scheduler_view(ai).iter().all(|e| e.output == console));
	let stopped = d.stop_ai(ai);
	assert_eq!(stopped.len(), 2, "点那个 AI 的窗口，在那里关闭最好用");
	assert_eq!(d.request_close(console), Ok(()));
}

#[test]
fn law22_group_close_is_a_stop_action_not_a_bare_close() {
	let (mut d, human, ai) = scenario();
	let group = d.open_group(human, PathBuf::from("/repo"));
	d.group_add_ai(group, ai);
	let window = d.group_window(group);
	assert_eq!(
		d.request_close(window),
		Err(CloseDenial::GroupCloseIsStopAction { window, group }),
		"关闭永远不含停止；群聊的关闭是另一个动作"
	);
}

#[test]
fn law22_closing_the_group_closes_the_ai() {
	let (mut d, human, ai) = scenario();
	let group = d.open_group(human, PathBuf::from("/repo"));
	d.group_add_ai(group, ai);
	let event = d
		.start_event(
			ai,
			Subject::Task {
				label: "still working".into(),
			},
		)
		.unwrap();
	let GroupClosed {
		stopped, ai_closed, ..
	} = d.close_group(human, group);
	assert_eq!(stopped, vec![event]);
	assert_eq!(ai_closed, vec![ai]);
	assert!(d.console(ai).is_some());
	assert_eq!(
		d.scheduler_view(ai).len(),
		0,
		"AI 关了，它的事件也不再是活的"
	);
}

#[test]
fn law23_the_workspace_is_only_the_root_a_group_chose() {
	let mut d = Delta::null();
	let human = d.add_human();
	let a = d.open_group(human, PathBuf::from("/repo"));
	let b = d.open_group(human, PathBuf::from("/repo"));
	assert_eq!(d.group_root(a), d.group_root(b));
	assert_ne!(a, b, "没有工作区这一层，所以两个群可以同一个根");
}

#[test]
fn law13_esc_exits_only_where_delta_owns_the_keymap() {
	let (mut d, human, _ai) = scenario();
	let miku = d.open_editor(human, path("src/miku.rs"));
	assert_eq!(d.keyboard.focused, Some(miku));
	d.set_input(crate::InputMode::Insert);
	assert_eq!(d.esc(), Some(Esc::ToNormal));

	let foreign = d.open_terminal(path("/repo"));
	d.focus(human, foreign);
	d.set_keymap(foreign, KeymapOwner::ForeignApp);
	d.set_input(crate::InputMode::Insert);
	assert_eq!(
		d.esc(),
		Some(Esc::GivenToApp),
		"键已经交出去了，ESC 是那个程序的字节"
	);
	assert_eq!(
		d.keyboard.input,
		crate::InputMode::Insert,
		"chrome 不该在这里改键盘状态"
	);
}

#[test]
fn law19_the_window_survives_the_program_s_own_exit() {
	let (mut d, human, _ai) = scenario();
	let pane = d.open_terminal(path("/repo"));
	d.set_keymap(pane, KeymapOwner::ForeignApp);
	d.set_keymap(pane, KeymapOwner::Delta);
	assert!(
		d.window(pane).is_some(),
		"nvim有自己的退出，你别推了，窗口还在"
	);
	d.focus(human, pane);
	assert_eq!(d.close(pane), Ok(()));
}

#[test]
fn law9_only_the_front_is_drawn_and_sleep_is_not_close() {
	let (mut d, _human, _ai) = scenario();
	let bottom = d.open_terminal(path("/repo/bottom"));
	let top = d.open_terminal(path("/repo/top"));
	assert_eq!(d.drawn(), Some(top));
	assert_eq!(d.counted_but_hidden(), d.windows.len() - 1);
	d.sleep(top);
	assert_eq!(
		d.drawn(),
		Some(bottom),
		"下面的就先不画：睡下去的那个不再是前景"
	);
	assert!(d.window(top).is_some(), "睡眠不是关闭");
	d.raise(top);
	assert_eq!(d.drawn(), Some(top));
}

#[test]
fn derived_released_machine_opened_window_sinks_instead_of_closing() {
	let (mut d, human, ai) = scenario();
	let pane = d.open_terminal(path("/repo"));
	d.focus(human, pane);
	let token = d.begin_edit(ai, path("src/sink.rs")).unwrap();
	let window = d.windows_for_path(&path("src/sink.rs"))[0];
	d.end_edit(token);
	assert!(d.window(window).is_some(), "关掉会把法则 2 要留的证据丢掉");
	assert_ne!(
		d.drawn(),
		Some(window),
		"已解除绑定的窗口沉到图层底部，不画"
	);
}

#[derive(Default)]
struct Counting(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl crate::Recorder for Counting {
	fn append(&mut self, _row: crate::LedgerRow) {
		self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
	}
}

#[test]
fn law5_every_op_has_a_record_call_even_though_the_recorder_is_blank_this_phase() {
	let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
	let mut d = Delta::new(Box::new(Counting(counter.clone())));
	let human = d.add_human();
	let ai = d.add_ai();
	let file = path("src/counted.rs");
	let token = d.begin_edit(ai, file.clone()).unwrap();
	d.end_edit(token);
	d.ai_delete(ai, file);
	let event = d
		.start_event(ai, Subject::Task { label: "x".into() })
		.unwrap();
	d.stop_event(event);
	let pane = d.open_terminal(path("/repo"));
	d.grant_takeover(ai, pane);
	// Edit, Delete, EventStart, EventStop, Consent, Takeover —— 法则 5 的两个自动里，
	// 这一期只有调用点在，实现是 NullRecorder。
	assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 6);
	let _ = human;
}
