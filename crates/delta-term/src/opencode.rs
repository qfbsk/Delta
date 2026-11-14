//! opencode shared server. `opencode serve` is the only route that lets the
//! TUI, even and Delta see the same session (ACP's per-process server is
//! private; see 第一代/02-有据.md §5).

use std::collections::HashSet;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::os::windows::process::CommandExt;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

use base64::Engine as _;
use serde_json::{Value, json};

pub const MODEL_PROVIDER: &str = "deepseek";
pub const DEFAULT_MODEL: &str = "deepseek-flash";
pub const MODEL_CHOICES: [(&str, &str); 2] = [
	("deepseek-flash", "DeepSeek V4.1 Flash"),
	("deepseek-v4-pro", "DeepSeek V4 Pro"),
];

pub fn program() -> String {
	std::env::var("DELTA_OPENCODE").unwrap_or_else(|_| String::from(r"D:\OPENCODE\opencode.exe"))
}

#[derive(Clone)]
pub struct Client {
	pub url: String,
	pub password: String,
	pub mcp_url: Option<String>,
	agent: ureq::Agent,
	mcp_directories: Arc<Mutex<HashSet<String>>>,
}

impl Client {
	fn new(url: String, password: String) -> Self {
		Self {
			url,
			password,
			mcp_url: None,
			agent: ureq::AgentBuilder::new()
				.timeout_connect(Duration::from_secs(3))
				.timeout_read(Duration::from_secs(30))
				.build(),
			mcp_directories: Arc::new(Mutex::new(HashSet::new())),
		}
	}

	pub fn env(&self) -> Vec<(String, String)> {
		vec![
			(String::from("DELTA_OPENCODE_SERVER"), self.url.clone()),
			(
				String::from("DELTA_OPENCODE_PASSWORD"),
				self.password.clone(),
			),
			(String::from("OPENCODE_PASSWORD"), self.password.clone()),
		]
	}

	fn request(
		&self,
		method: &str,
		path: &[&str],
		query: &[(&str, &str)],
		body: Option<&str>,
	) -> Result<String, String> {
		if !matches!(method, "GET" | "POST" | "PUT" | "PATCH") {
			return Err(String::from("unsupported method"));
		}
		if path
			.iter()
			.any(|segment| matches!(*segment, "" | "." | ".."))
		{
			return Err(String::from("invalid API path segment"));
		}
		let mut url = self
			.agent
			.get(&self.url)
			.request_url()
			.map_err(|err| err.to_string())?
			.as_url()
			.clone();
		url.set_query(None);
		url.set_fragment(None);
		url.path_segments_mut()
			.map_err(|_| String::from("invalid server URL"))?
			.clear()
			.extend(path.iter().copied());
		let auth = format!(
			"Basic {}",
			base64::engine::general_purpose::STANDARD.encode(format!("opencode:{}", self.password))
		);
		let mut request = self
			.agent
			.request_url(method, &url)
			.set("Authorization", &auth);
		for (key, value) in query {
			request = request.query(key, value);
		}
		let result = match body {
			Some(body) => request
				.set("Content-Type", "application/json")
				.send_string(body),
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

	fn ensure_mcp(&self, directory: &str, namespace: &str) -> Result<(), String> {
		let mcp_url = self
			.mcp_url
			.as_deref()
			.filter(|url| !url.trim().is_empty())
			.ok_or_else(|| {
				String::from("Delta MCP URL is not configured; cannot create even session")
			})?;
		let mut directories = self
			.mcp_directories
			.lock()
			.map_err(|_| String::from("Delta MCP registration lock poisoned"))?;
		if directories.contains(directory) {
			return Ok(());
		}
		let body = json!({
			"config": {
				"type": "remote",
				"url": mcp_url,
				"oauth": false,
				"codemode": false,
				"timeout": {"execution": 12 * 60 * 60 * 1000},
			}
		})
		.to_string();
		let query = [("location[directory]", directory)];
		self.request(
			"PUT",
			&["api", "experimental", "mcp", namespace],
			&query,
			Some(&body),
		)?;
		// McpHandler.mcp.add returns 204 even when MCP.connection.start records a failed status,
		// and the connection itself is asynchronous: the first status is usually pending.
		let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
		loop {
			let text = self.request("GET", &["api", "mcp"], &query, None)?;
			let value: Value = serde_json::from_str(&text).map_err(|err| err.to_string())?;
			let server = value["data"]
				.as_array()
				.and_then(|servers| servers.iter().find(|server| server["name"] == namespace))
				.ok_or_else(|| format!("Delta MCP {namespace} is missing after registration"))?;
			let status = &server["status"];
			if status["status"] == "connected" {
				directories.insert(directory.to_owned());
				return Ok(());
			}
			if status["status"] != "pending" || std::time::Instant::now() >= deadline {
				return Err(format!("Delta MCP {namespace} is not connected: {status}"));
			}
			std::thread::sleep(std::time::Duration::from_millis(100));
		}
	}

	pub fn create_session(
		&self,
		directory: &str,
		title: &str,
		model: &str,
		identity: &str,
		role: Option<&str>,
	) -> Result<String, String> {
		let namespace = format!("delta_{}", std::process::id());
		self.ensure_mcp(directory, &namespace)?;
		let mut permissions = vec![json!({"action": "*", "resource": "*", "effect": "deny"})];
		permissions
			.push(json!({"action": format!("{namespace}_*"), "resource": "*", "effect": "allow"}));
		for action in ["read", "glob", "grep", "webfetch", "websearch"] {
			permissions.push(json!({"action": action, "resource": "*", "effect": "allow"}));
		}
		let body = json!({
			"location": {"directory": directory},
			"title": title,
			"model": {"providerID": MODEL_PROVIDER, "id": model},
			"permissions": permissions,
		})
		.to_string();
		let text = self.request("POST", &["api", "session"], &[], Some(&body))?;
		let value: Value = serde_json::from_str(&text).map_err(|err| err.to_string())?;
		let session = value["data"]["id"]
			.as_str()
			.filter(|id| !id.is_empty())
			.ok_or_else(|| format!("no session id in {text}"))?;
		let instructions = format!(
			concat!(
				"你是 Delta even 的成员，成员代号是 {identity}；本条 role 字段是完整角色文本，null 表示未设角色。\n",
				"用户消息从 even 群聊路由给你；@ 是 Delta 的分发语法，不是 OpenCode 的文件引用，不要把 @成员 当成文件。\n",
				"群聊里的 :add、:members、:open、:stop、:remove、:clear、:help 和 @代号 都是给人用的；成员不要发送这些命令，也不要自己添加或移除成员。\n",
				"你只能通过本实例的 {namespace}_* 工具在 Delta 的窗口里做事，不要另起 shell、edit、subagent、worktree 或 schedule 绕过。\n",
				"动手前先调用 {namespace}_list_windows，看每个 pane 的 id、kind（terminal / solid / even）、title、owner，再决定操作哪个 pane；优先在原窗口原地操作。\n",
				"read_window 能读任何 pane；write_window 只接受 solid 窗口，run_in_terminal 只接受 terminal 窗口。对 even 窗口调用它们会分别报「这个窗口不是 solid」「这个窗口不是 terminal」，那是 id 选错了：重新 list_windows 换一个，不要对同一个 id 反复重试。\n",
				"要新建窗口用 {namespace}_create_window：kind=terminal 或 solid（solid 需要 path），返回值里是新窗口 id。AI 创建的窗口不抢焦点。\n",
				"delta-term.exe 没有 --help、--version、-h 这类命令行开关；不认识的参数会被当成要打开的文件路径，会多开一个空窗口并在磁盘上留下空文件。不要用它试探命令。\n",
				"需要接管时直接调用 {namespace}_write_window 或 {namespace}_run_in_terminal：Delta 会在人眼前弹出接管请求并让这次调用等待，人同意后当场执行，不需要先查 takeover_status，也不要因为 owner 不是你就自己放弃。\n",
				"接管由人批准一次，批准后在该窗口继续；等待批准、被拒绝、或调用一直等到超时，都停下来把情况告诉人，不要换别的工具绕过，也不要重复发同一条命令。\n",
				"odd 是已经运行的原生 shell，不要安装 WSL，也不要另建 shell 来代替它。\n",
				"odd 是原生 fish：ls/cp/md 是内建命令，cat/grep 这类不保证存在，不要假定有 coreutils。\n",
				"在终端里写 Windows 路径要加引号或用正斜杠（反斜杠是 fish 的转义符）；发给 cmd 自己的命令（如 dir）的路径要用反斜杠，并把整条命令加引号。\n",
				"执行后必须用 {namespace}_read_window 验证真实窗口结果，验证前不要宣称成功。\n",
				"同一会话的成员身份由 OpenCode 自动提供的 MCP 元数据 _meta.ai.opencode/sessionID 交给 Delta 解析；不要自己填写或伪造身份参数或 header。"
			),
			identity = identity,
			namespace = namespace
		);
		let body =
			json!({"value": {"identity": identity, "role": role, "instructions": instructions}})
				.to_string();
		self.request(
			"PUT",
			&[
				"api",
				"experimental",
				"session",
				session,
				"instructions",
				"entries",
				"delta",
			],
			&[],
			Some(&body),
		)?;
		Ok(session.to_owned())
	}

	pub fn prompt(&self, session: &str, text: &str) -> Result<(), String> {
		let body = json!({"text": text, "delivery": "steer"});
		self.request(
			"POST",
			&["api", "session", session, "prompt"],
			&[],
			Some(&body.to_string()),
		)?;
		Ok(())
	}

	pub fn interrupt(&self, session: &str) -> Result<(), String> {
		self.request(
			"POST",
			&["api", "session", session, "interrupt"],
			&[],
			Some("{}"),
		)?;
		Ok(())
	}

	pub fn messages(&self, session: &str) -> Result<Vec<Value>, String> {
		// Newest page first, then back to chronological order: an asc page
		// would freeze at the oldest 80 messages of a long session.
		let text = self.request(
			"GET",
			&["api", "session", session, "message"],
			&[("limit", "80"), ("order", "desc")],
			None,
		)?;
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
		if let (Ok(url), Ok(password)) = (
			std::env::var("DELTA_OPENCODE_SERVER"),
			std::env::var("DELTA_OPENCODE_PASSWORD"),
		) {
			return Ok(Self {
				child: None,
				job: 0,
				client: Client::new(url, password),
			});
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
		let nanos = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or_default()
			.subsec_nanos();
		let password = format!("delta-{}-{nanos}", std::process::id());
		let mut child = Command::new(&opencode)
			.args([
				"serve",
				"--hostname",
				"127.0.0.1",
				"--port",
				&port.to_string(),
			])
			.env("OPENCODE_PASSWORD", &password)
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.creation_flags(CREATE_NO_WINDOW)
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
		Err(io::Error::new(
			io::ErrorKind::TimedOut,
			"opencode serve did not open its port",
		))
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
		AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
		JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
		SetInformationJobObject,
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
		.creation_flags(CREATE_NO_WINDOW)
		.status();
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::io::{BufRead, BufReader, Read, Write};
	use std::sync::atomic::{AtomicBool, Ordering};
	use std::sync::{Barrier, mpsc};

	#[derive(Debug)]
	struct HttpRequest {
		method: String,
		target: String,
		headers: Vec<(String, String)>,
		body: Value,
	}

	struct Fixture {
		client: Client,
		requests: mpsc::Receiver<HttpRequest>,
		stopping: Arc<AtomicBool>,
		worker: Option<thread::JoinHandle<()>>,
	}

	impl Fixture {
		fn new(mut respond: impl FnMut(&HttpRequest) -> (u16, String) + Send + 'static) -> Self {
			let listener = TcpListener::bind("127.0.0.1:0").unwrap();
			let mut client = Client::new(
				format!("http://{}", listener.local_addr().unwrap()),
				String::from("fixture"),
			);
			client.agent = ureq::AgentBuilder::new()
				.try_proxy_from_env(false)
				.timeout(Duration::from_secs(5))
				.build();
			let stopping = Arc::new(AtomicBool::new(false));
			let stop = stopping.clone();
			let (sender, requests) = mpsc::channel();
			let worker = thread::spawn(move || {
				for stream in listener.incoming() {
					let stream = stream.unwrap();
					if stop.load(Ordering::SeqCst) {
						break;
					}
					stream
						.set_read_timeout(Some(Duration::from_secs(5)))
						.unwrap();
					stream
						.set_write_timeout(Some(Duration::from_secs(5)))
						.unwrap();
					let mut reader = BufReader::new(stream);
					let mut line = String::new();
					reader.read_line(&mut line).unwrap();
					let mut parts = line.split_whitespace();
					let method = parts.next().unwrap().to_owned();
					let target = parts.next().unwrap().to_owned();
					let mut headers = Vec::new();
					loop {
						line.clear();
						assert_ne!(reader.read_line(&mut line).unwrap(), 0);
						if line == "\r\n" {
							break;
						}
						let (key, value) = line.trim_end().split_once(':').unwrap();
						headers.push((key.to_ascii_lowercase(), value.trim().to_owned()));
					}
					let size = headers
						.iter()
						.find(|(key, _)| key == "content-length")
						.map(|(_, value)| value.parse::<usize>().unwrap())
						.unwrap_or(0);
					let mut bytes = vec![0; size];
					reader.read_exact(&mut bytes).unwrap();
					let body = if bytes.is_empty() {
						Value::Null
					} else {
						serde_json::from_slice(&bytes).unwrap()
					};
					let request = HttpRequest {
						method,
						target,
						headers,
						body,
					};
					let (status, body) = respond(&request);
					sender.send(request).unwrap();
					write!(reader.get_mut(),
						"HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
						body.len()).unwrap();
				}
			});
			Self {
				client,
				requests,
				stopping,
				worker: Some(worker),
			}
		}

		fn enable_mcp(&mut self) {
			self.client.mcp_url = Some(format!("{}/delta-mcp", self.client.url));
		}

		fn recorded(&self) -> Vec<HttpRequest> {
			self.requests.try_iter().collect()
		}
	}

	impl Drop for Fixture {
		fn drop(&mut self) {
			self.stopping.store(true, Ordering::SeqCst);
			let _ = TcpStream::connect(self.client.url.trim_start_matches("http://"));
			if let Some(worker) = self.worker.take() {
				let result = worker.join();
				if !thread::panicking() {
					result.unwrap();
				}
			}
		}
	}

	fn ready(request: &HttpRequest) -> (u16, String) {
		if request.method == "GET" && request.target.starts_with("/api/mcp?") {
			return (
				200,
				json!({
					"location": {"directory": "E:/Delta"},
					"data": [{"name": format!("delta_{}", std::process::id()), "status": {"status": "connected"}}],
				})
				.to_string(),
			);
		}
		if request.method == "POST" && request.target == "/api/session" {
			return (200, json!({"data": {"id": "ses_fixture"}}).to_string());
		}
		if request.method == "PUT"
			&& (request.target.starts_with("/api/experimental/mcp/")
				|| request.target.ends_with("/instructions/entries/delta"))
		{
			return (204, String::new());
		}
		(500, String::from("unexpected fixture request"))
	}

	fn directory_query(request: &HttpRequest) -> Vec<(String, String)> {
		ureq::get(&format!("http://127.0.0.1{}", request.target))
			.request_url()
			.unwrap()
			.as_url()
			.query_pairs()
			.into_owned()
			.collect()
	}

	#[test]
	fn create_session_sends_location_permissions_and_full_identity() {
		let mut fixture = Fixture::new(ready);
		fixture.enable_mcp();
		let directory = "E:\\团队 空间\\a&b+#%=?";
		let role = "  审查员：完整角色\n保留换行、空白和 \"引号\"；@sam 不代表文件。  ";
		assert_eq!(
			fixture
				.client
				.create_session(
					directory,
					"even1 / sam",
					DEFAULT_MODEL,
					"even1:sam",
					Some(role)
				)
				.unwrap(),
			"ses_fixture"
		);
		let requests = fixture.recorded();
		assert_eq!(requests.len(), 4);
		let namespace = format!("delta_{}", std::process::id());
		assert_eq!(requests[0].method, "PUT");
		assert_eq!(
			requests[0].target.split('?').next().unwrap(),
			format!("/api/experimental/mcp/{namespace}")
		);
		assert!(requests[0].target.contains("location%5Bdirectory%5D="));
		assert!(requests[0].target.contains("%26b%2B%23%25%3D%3F"));
		for request in &requests[..2] {
			assert_eq!(
				directory_query(request),
				vec![(String::from("location[directory]"), directory.to_owned())]
			);
		}
		assert_eq!(
			requests[0].body,
			json!({"config": {
				"type": "remote", "url": fixture.client.mcp_url, "oauth": false, "codemode": false,
				"timeout": {"execution": 43_200_000},
			}})
		);
		assert_eq!(requests[1].method, "GET");
		assert_eq!(requests[2].method, "POST");
		assert_eq!(requests[2].target, "/api/session");
		assert_eq!(
			requests[2].body,
			json!({
				"location": {"directory": directory}, "title": "even1 / sam",
				"model": {"providerID": "deepseek", "id": DEFAULT_MODEL},
				"permissions": [
					{"action": "*", "resource": "*", "effect": "deny"},
					{"action": format!("{namespace}_*"), "resource": "*", "effect": "allow"},
					{"action": "read", "resource": "*", "effect": "allow"},
					{"action": "glob", "resource": "*", "effect": "allow"},
					{"action": "grep", "resource": "*", "effect": "allow"},
					{"action": "webfetch", "resource": "*", "effect": "allow"},
					{"action": "websearch", "resource": "*", "effect": "allow"},
				],
			})
		);
		assert_eq!(requests[3].method, "PUT");
		assert_eq!(
			requests[3].target,
			"/api/experimental/session/ses_fixture/instructions/entries/delta"
		);
		let entry = &requests[3].body["value"];
		assert_eq!(entry["identity"], "even1:sam");
		assert_eq!(entry["role"], role);
		let instructions = entry["instructions"].as_str().unwrap();
		for text in [
			"even1:sam",
			"用户消息从 even 群聊路由给你",
			"@ 是 Delta 的分发语法",
			"不是 OpenCode 的文件引用",
			"成员不要发送这些命令",
			"只能通过本实例",
			"kind（terminal / solid / even）",
			"write_window 只接受 solid 窗口",
			"run_in_terminal 只接受 terminal 窗口",
			"不要对同一个 id 反复重试",
			"create_window",
			"没有 --help",
			"接管由人批准一次",
			"弹出接管请求",
			"不要换别的工具绕过",
			"已经运行的原生 shell",
			"不要安装 WSL",
			"不要假定有 coreutils",
			"反斜杠是 fish 的转义符",
			"验证前不要宣称成功",
			"_meta.ai.opencode/sessionID",
			"不要自己填写或伪造身份",
		] {
			assert!(instructions.contains(text), "missing instruction: {text}");
		}
		for tool in ["list_windows", "read_window"] {
			assert!(instructions.contains(&format!("{namespace}_{tool}")));
		}
		for request in &requests {
			assert!(request.headers.contains(&(
				String::from("authorization"),
				String::from("Basic b3BlbmNvZGU6Zml4dHVyZQ==")
			)));
			assert!(
				!request
					.headers
					.iter()
					.any(|(_, value)| value.contains("even1:sam"))
			);
			assert!(!request.target.ends_with("/prompt"));
		}
	}

	#[test]
	fn missing_mcp_url_fails_without_http() {
		let mut fixture = Fixture::new(ready);
		assert!(fixture.client.mcp_url.is_none());
		for url in [None, Some(String::new()), Some(String::from(" \t"))] {
			fixture.client.mcp_url = url;
			let error = fixture
				.client
				.create_session("E:/Delta", "sam", DEFAULT_MODEL, "even1:sam", None)
				.unwrap_err();
			assert!(error.contains("MCP URL is not configured"));
		}
		assert!(fixture.recorded().is_empty());
		assert!(fixture.client.mcp_directories.lock().unwrap().is_empty());
	}

	#[test]
	fn clones_serialize_and_cache_registration_for_one_location() {
		let mut sessions = 0;
		let mut fixture = Fixture::new(move |request| {
			if request.method == "POST" && request.target == "/api/session" {
				sessions += 1;
				return (
					200,
					json!({"data": {"id": format!("ses_{sessions}")}}).to_string(),
				);
			}
			ready(request)
		});
		fixture.enable_mcp();
		let barrier = Arc::new(Barrier::new(6));
		let workers: Vec<_> = (0..6)
			.map(|i| {
				let client = fixture.client.clone();
				let barrier = barrier.clone();
				thread::spawn(move || {
					barrier.wait();
					client.create_session(
						"E:/Delta",
						"member",
						DEFAULT_MODEL,
						&format!("even1:member{i}"),
						None,
					)
				})
			})
			.collect();
		let sessions: HashSet<_> = workers
			.into_iter()
			.map(|worker| worker.join().unwrap().unwrap())
			.collect();
		assert_eq!(sessions.len(), 6);
		let requests = fixture.recorded();
		assert_eq!(requests.len(), 14);
		assert_eq!(
			requests
				.iter()
				.filter(|request| request.target.starts_with("/api/experimental/mcp/"))
				.count(),
			1
		);
		assert_eq!(
			requests
				.iter()
				.filter(|request| request.target.starts_with("/api/mcp?"))
				.count(),
			1
		);
		let entries: Vec<_> = requests
			.iter()
			.filter(|request| request.target.ends_with("/instructions/entries/delta"))
			.collect();
		assert_eq!(entries.len(), 6);
		assert!(
			entries
				.iter()
				.all(|request| request.body["value"]["role"].is_null())
		);
		let identities: HashSet<_> = entries
			.iter()
			.map(|request| request.body["value"]["identity"].as_str().unwrap())
			.collect();
		assert_eq!(identities.len(), 6);
		assert_eq!(
			*fixture.client.mcp_directories.lock().unwrap(),
			HashSet::from([String::from("E:/Delta")])
		);
	}

	#[test]
	fn different_locations_register_separately() {
		let mut fixture = Fixture::new(ready);
		fixture.enable_mcp();
		for directory in ["E:/Delta", "E:/other & 工作区"] {
			fixture
				.client
				.clone()
				.create_session(directory, "sam", DEFAULT_MODEL, "even1:sam", None)
				.unwrap();
		}
		let requests = fixture.recorded();
		assert_eq!(requests.len(), 8);
		assert_eq!(directory_query(&requests[0])[0].1, "E:/Delta");
		assert_eq!(directory_query(&requests[4])[0].1, "E:/other & 工作区");
		assert_eq!(fixture.client.mcp_directories.lock().unwrap().len(), 2);
	}

	#[test]
	fn registration_http_failure_is_not_cached_and_can_retry() {
		let mut failed = false;
		let mut fixture = Fixture::new(move |request| {
			if !failed {
				failed = true;
				return (503, String::from("MCP registration unavailable"));
			}
			ready(request)
		});
		fixture.enable_mcp();
		let error = fixture
			.client
			.create_session("E:/Delta", "sam", DEFAULT_MODEL, "even1:sam", None)
			.unwrap_err();
		assert_eq!(error, "HTTP 503: MCP registration unavailable");
		assert!(fixture.client.mcp_directories.lock().unwrap().is_empty());
		let requests = fixture.recorded();
		assert_eq!(requests.len(), 1);
		assert_eq!(requests[0].method, "PUT");
		fixture
			.client
			.clone()
			.create_session("E:/Delta", "sam", DEFAULT_MODEL, "even1:sam", None)
			.unwrap();
		assert_eq!(fixture.recorded().len(), 4);
	}

	#[test]
	fn disconnected_mcp_after_204_is_not_cached_or_used() {
		for status in [
			json!({"status": "failed", "error": "connection refused"}),
			json!({"status": "needs_auth", "error": "authentication required"}),
			json!({"status": "disabled"}),
		] {
			let expected = status.to_string();
			let mut fixture = Fixture::new(move |request| {
				if request.method == "GET" {
					return (200, json!({"data": [{"name": format!("delta_{}", std::process::id()), "status": status}]}).to_string());
				}
				ready(request)
			});
			fixture.enable_mcp();
			for _ in 0..2 {
				let error = fixture
					.client
					.create_session("E:/Delta", "sam", DEFAULT_MODEL, "even1:sam", None)
					.unwrap_err();
				assert!(error.contains(&expected));
				assert!(fixture.client.mcp_directories.lock().unwrap().is_empty());
				let requests = fixture.recorded();
				assert_eq!(requests.len(), 2);
				assert_eq!(requests[0].method, "PUT");
				assert_eq!(requests[1].method, "GET");
			}
		}
	}

	#[test]
	fn pending_mcp_status_is_polled_until_connected() {
		let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
		let counter = polls.clone();
		let mut fixture = Fixture::new(move |request| {
			if request.method == "GET" {
				let poll = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
				let status = if poll == 0 {
					json!({"status": "pending"})
				} else {
					json!({"status": "connected"})
				};
				return (
					200,
					json!({"data": [{"name": format!("delta_{}", std::process::id()), "status": status}]})
						.to_string(),
				);
			}
			ready(request)
		});
		fixture.enable_mcp();
		let session = fixture
			.client
			.create_session("E:/Delta", "sam", DEFAULT_MODEL, "even1:sam", None)
			.unwrap();
		assert_eq!(session, "ses_fixture");
		assert_eq!(polls.load(std::sync::atomic::Ordering::SeqCst), 2);
	}

	#[test]
	fn invalid_mcp_status_response_prevents_session_creation() {
		for body in ["not JSON", "{}", "{\"data\":[]}"] {
			let mut fixture = Fixture::new(move |request| {
				if request.method == "GET" {
					return (200, body.to_owned());
				}
				ready(request)
			});
			fixture.enable_mcp();
			assert!(
				fixture
					.client
					.create_session("E:/Delta", "sam", DEFAULT_MODEL, "even1:sam", None)
					.is_err()
			);
			assert!(fixture.client.mcp_directories.lock().unwrap().is_empty());
			assert_eq!(fixture.recorded().len(), 2);
		}
	}

	#[test]
	fn failed_session_creation_never_puts_instructions_or_prompts() {
		for (status, body) in [
			(500, "create failed"),
			(200, "not JSON"),
			(200, "{}"),
			(200, "{\"data\":{\"id\":\"\"}}"),
		] {
			let mut fixture = Fixture::new(move |request| {
				if request.method == "POST" {
					return (status, body.to_owned());
				}
				ready(request)
			});
			fixture.enable_mcp();
			assert!(
				fixture
					.client
					.create_session("E:/Delta", "sam", DEFAULT_MODEL, "even1:sam", None)
					.is_err()
			);
			let requests = fixture.recorded();
			assert_eq!(requests.len(), 3);
			assert_eq!(requests[2].target, "/api/session");
		}
	}

	#[test]
	fn failed_instructions_never_returns_session_or_sends_prompt() {
		let mut fixture = Fixture::new(|request| {
			if request.target.ends_with("/instructions/entries/delta") {
				return (500, String::from("instructions failed"));
			}
			ready(request)
		});
		fixture.enable_mcp();
		let error = fixture
			.client
			.create_session("E:/Delta", "sam", DEFAULT_MODEL, "even1:sam", None)
			.unwrap_err();
		assert_eq!(error, "HTTP 500: instructions failed");
		let requests = fixture.recorded();
		assert_eq!(requests.len(), 4);
		assert!(
			!requests
				.iter()
				.any(|request| request.target.ends_with("/prompt"))
		);
	}

	#[test]
	fn request_encodes_path_segments_and_queries_with_put_and_patch() {
		let fixture = Fixture::new(|request| {
			if request.method == "GET" {
				return (
					200,
					json!({"data": [{"id": "new"}, {"id": "old"}]}).to_string(),
				);
			}
			(204, String::new())
		});
		let session = "ses_a/b?x=1#% 中文";
		for method in ["PUT", "PATCH"] {
			fixture
				.client
				.request(
					method,
					&["api", "session", session],
					&[("a&b", "x+y &%#")],
					Some("{}"),
				)
				.unwrap();
		}
		fixture.client.prompt(session, "@sam 请检查").unwrap();
		fixture.client.interrupt(session).unwrap();
		assert_eq!(
			fixture.client.messages(session).unwrap(),
			vec![json!({"id": "old"}), json!({"id": "new"})]
		);
		let requests = fixture.recorded();
		assert_eq!(requests.len(), 5);
		let path = "/api/session/ses_a%2Fb%3Fx=1%23%25%20%E4%B8%AD%E6%96%87";
		for (index, method) in ["PUT", "PATCH"].iter().enumerate() {
			assert_eq!(requests[index].method, *method);
			assert_eq!(requests[index].target.split('?').next().unwrap(), path);
			assert_eq!(
				directory_query(&requests[index]),
				vec![(String::from("a&b"), String::from("x+y &%#"))]
			);
		}
		assert_eq!(requests[2].target, format!("{path}/prompt"));
		assert_eq!(
			requests[2].body,
			json!({"text": "@sam 请检查", "delivery": "steer"})
		);
		assert_eq!(requests[3].target, format!("{path}/interrupt"));
		assert_eq!(
			requests[4].target,
			format!("{path}/message?limit=80&order=desc")
		);
		for segment in ["", ".", ".."] {
			assert!(
				fixture
					.client
					.request("GET", &["api", "session", segment], &[], None)
					.is_err()
			);
		}
		assert!(fixture.recorded().is_empty());
	}
}
