//! opencode shared server. `opencode serve` is the only route that lets the
//! TUI, even and Delta see the same session (ACP's per-process server is
//! private; see 第一代/02-有据.md §5).

use std::io;
use std::net::{TcpListener, TcpStream};

use windows_sys::Win32::Foundation::CloseHandle;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde_json::{json, Value};

pub const MODEL_PROVIDER: &str = "deepseek";
pub const DEFAULT_MODEL: &str = "deepseek-flash";
pub const MODEL_CHOICES: [(&str, &str); 2] =
	[("deepseek-flash", "DeepSeek V4.1 Flash"), ("deepseek-v4-pro", "DeepSeek V4 Pro")];

pub fn program() -> String {
	std::env::var("DELTA_OPENCODE").unwrap_or_else(|_| String::from(r"D:\OPENCODE\opencode.exe"))
}

#[derive(Clone)]
pub struct Client {
	pub url: String,
	pub password: String,
	agent: ureq::Agent,
}

impl Client {
	fn new(url: String, password: String) -> Self {
		Self {
			url,
			password,
			agent: ureq::AgentBuilder::new()
				.timeout_connect(Duration::from_secs(3))
				.timeout_read(Duration::from_secs(30))
				.build(),
		}
	}

	pub fn env(&self) -> Vec<(String, String)> {
		vec![
			(String::from("DELTA_OPENCODE_SERVER"), self.url.clone()),
			(String::from("DELTA_OPENCODE_PASSWORD"), self.password.clone()),
			(String::from("OPENCODE_PASSWORD"), self.password.clone()),
		]
	}

	fn request(&self, method: &str, path: &str, body: Option<&str>) -> Result<String, String> {
		let url = format!("{}{}", self.url, path);
		let auth = format!(
			"Basic {}",
			base64::engine::general_purpose::STANDARD
				.encode(format!("opencode:{}", self.password))
		);
		let request = match method {
			"GET" => self.agent.get(&url),
			"POST" => self.agent.post(&url),
			_ => return Err(String::from("unsupported method")),
		}
		.set("Authorization", &auth);
		let result = match body {
			Some(body) => request.set("Content-Type", "application/json").send_string(body),
			None => request.call(),
		};
		match result {
			Ok(response) => response.into_string().map_err(|err| err.to_string()),
			Err(ureq::Error::Status(code, response)) => {
				let text = response.into_string().unwrap_or_default();
				Err(format!("HTTP {code}: {text}"))
			}
			Err(err) => Err(err.to_string()),
		}
	}

	pub fn create_session(&self, directory: &str, title: &str, model: &str) -> Result<String, String> {
		let body = json!({
			"location": {"directory": directory},
			"title": title,
			"model": {"providerID": MODEL_PROVIDER, "id": model},
		})
		.to_string();
		let text = self.request("POST", "/api/session", Some(&body))?;
		let value: Value = serde_json::from_str(&text).map_err(|err| err.to_string())?;
		value["data"]["id"].as_str().map(String::from).ok_or_else(|| format!("no session id in {text}"))
	}

	pub fn prompt(&self, session: &str, text: &str) -> Result<(), String> {
		let body = json!({"text": text, "delivery": "steer"}).to_string();
		self.request("POST", &format!("/api/session/{session}/prompt"), Some(&body))?;
		Ok(())
	}

	pub fn interrupt(&self, session: &str) -> Result<(), String> {
		self.request("POST", &format!("/api/session/{session}/interrupt"), Some("{}"))?;
		Ok(())
	}

	pub fn messages(&self, session: &str) -> Result<Vec<Value>, String> {
		// Newest page first, then back to chronological order: an asc page
		// would freeze at the oldest 80 messages of a long session.
		let text =
			self.request("GET", &format!("/api/session/{session}/message?limit=80&order=desc"), None)?;
		let value: Value = serde_json::from_str(&text).map_err(|err| err.to_string())?;
		let mut messages = value["data"].as_array().cloned().unwrap_or_default();
		messages.reverse();
		Ok(messages)
	}
}

pub struct Server {
	child: Option<Child>,
	job: isize,
	pub client: Client,
}

impl Server {
	pub fn start() -> io::Result<Self> {
		if let (Ok(url), Ok(password)) =
			(std::env::var("DELTA_OPENCODE_SERVER"), std::env::var("DELTA_OPENCODE_PASSWORD"))
		{
			return Ok(Self { child: None, job: 0, client: Client::new(url, password) });
		}
		let opencode = program();
		if !Path::new(&opencode).is_file() {
			return Err(io::Error::new(
				io::ErrorKind::NotFound,
				format!("{opencode} not found"),
			));
		}
		let listener = TcpListener::bind("127.0.0.1:0")?;
		let port = listener.local_addr()?.port();
		drop(listener);
		let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().subsec_nanos();
		let password = format!("delta-{}-{nanos}", std::process::id());
		let mut child = Command::new(&opencode)
			.args(["serve", "--hostname", "127.0.0.1", "--port", &port.to_string()])
			.env("OPENCODE_PASSWORD", &password)
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.spawn()?;
		let job = job_with(&child);
		for _ in 0..150 {
			if TcpStream::connect(("127.0.0.1", port)).is_ok() {
				return Ok(Self {
					child: Some(child),
					job,
					client: Client::new(format!("http://127.0.0.1:{port}"), password),
				});
			}
			thread::sleep(Duration::from_millis(100));
		}
		if job != 0 {
			unsafe { CloseHandle(job as windows_sys::Win32::Foundation::HANDLE) };
		}
		let _ = child.kill();
		kill_tree(child.id());
		Err(io::Error::new(io::ErrorKind::TimedOut, "opencode serve did not open its port"))
	}

	pub fn env(&self) -> Vec<(String, String)> {
		self.client.env()
	}
}

impl Drop for Server {
	fn drop(&mut self) {
		if let Some(child) = self.child.as_mut() {
			let pid = child.id();
			let _ = child.kill();
			kill_tree(pid);
			let _ = child.wait();
		}
		if self.job != 0 {
			// Closing the last handle kills anything still in the job; the OS
			// does this too when Delta itself is force-terminated.
			unsafe { CloseHandle(self.job as windows_sys::Win32::Foundation::HANDLE) };
			self.job = 0;
		}
	}
}

fn job_with(child: &Child) -> isize {
	use std::os::windows::io::AsRawHandle;
	use windows_sys::Win32::System::JobObjects::{
		AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
		SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
		JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
	};
	unsafe {
		let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
		if job.is_null() {
			return 0;
		}
		let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
		info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
		SetInformationJobObject(
			job,
			JobObjectExtendedLimitInformation,
			&info as *const _ as *const std::ffi::c_void,
			std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
		);
		AssignProcessToJobObject(job, child.as_raw_handle() as _);
		job as isize
	}
}

fn kill_tree(pid: u32) {
	let _ = Command::new("taskkill")
		.args(["/PID", &pid.to_string(), "/T", "/F"])
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.status();
}
