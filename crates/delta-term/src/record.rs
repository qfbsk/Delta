/// The record layer is a stub this phase: call points exist, persistence does not.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub enum Event {
	WindowOpened {
		window: u64,
		kind: &'static str,
		label: String,
	},
	WindowClosed {
		window: u64,
	},
	CommandStarted {
		window: u64,
		command: String,
	},
	FileSaved {
		window: u64,
		path: String,
	},
	AiTask {
		member: String,
		task: u64,
		session: String,
		text: String,
	},
	AiStopped {
		member: String,
		session: String,
	},
	MemberRemoved {
		member: String,
		session: String,
	},
}

pub trait Recorder: Send + Sync {
	fn record(&self, event: Event);
}

pub struct NullRecorder;

impl Recorder for NullRecorder {
	fn record(&self, _event: Event) {}
}

#[cfg(test)]
pub struct CollectingRecorder {
	pub events: std::sync::Mutex<Vec<Event>>,
}

#[cfg(test)]
impl CollectingRecorder {
	pub fn new() -> Self {
		Self {
			events: std::sync::Mutex::new(Vec::new()),
		}
	}
}

#[cfg(test)]
impl Recorder for CollectingRecorder {
	fn record(&self, event: Event) {
		self.events.lock().unwrap().push(event);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn recorder_keeps_call_sites() {
		let recorder = CollectingRecorder::new();
		recorder.record(Event::FileSaved {
			window: 2,
			path: String::from("notes.md"),
		});
		assert_eq!(recorder.events.lock().unwrap().len(), 1);
	}
}
