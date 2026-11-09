use std::io;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde_json::{json, Value};

pub const MODEL_PROVIDER: &str = "deepseek";

pub const MODEL_CHOICES: [(&str, &str); 2] = [
	("deepseek-flash", "DeepSeek V4.1 Flash"),
	("deepseek-v4-pro", "DeepSeek V4 Pro"),
];

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
				.timeout_read(Duration::from_secs(20))
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

	pub fn create_session(
		&self,
		directory: &str,
		title: &str,
		model: &str,
	) -> Result<String, String> {
		let body = json!({
			"location": {"directory": directory},
			"title": title,
			"model": {"providerID": MODEL_PROVIDER, "id": model},
		})
		.to_string();
		let text = self.request("POST", "/api/session", Some(&body))?;
		let value: Value = serde_json::from_str(&text).map_err(|err| err.to_string())?;
		value["data"]["id"]
			.as_str()
			.map(String::from)
			.ok_or_else(|| format!("no session id in {text}"))
	}

	pub fn prompt(&self, session: &str, text: &str) -> Result<(), String> {
		let body = json!({"text": text, "delivery": "steer"}).to_string();
		self.request(
			"POST",
			&format!("/api/session/{session}/prompt"),
			Some(&body),
		)?;
		Ok(())
	}

	pub fn interrupt(&self, session: &str) -> Result<(), String> {
		self.request(
			"POST",
			&format!("/api/session/{session}/interrupt"),
			Some("{}"),
		)?;
		Ok(())
	}

	pub fn messages(&self, session: &str) -> Result<Vec<Value>, String> {
		let text = self.request(
			"GET",
			&format!("/api/session/{session}/message?limit=50&order=asc"),
			None,
		)?;
		let value: Value = serde_json::from_str(&text).map_err(|err| err.to_string())?;
		Ok(value["data"].as_array().cloned().unwrap_or_default())
	}
}

pub struct Server {
	child: Option<Child>,
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
				client: Client::new(url, password),
			});
		}
		let opencode = std::env::var("DELTA_OPENCODE")
			.unwrap_or_else(|_| String::from(r"D:\OPENCODE\opencode.exe"));
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
			.spawn()?;
		for _ in 0..120 {
			if TcpStream::connect(("127.0.0.1", port)).is_ok() {
				return Ok(Self {
					child: Some(child),
					client: Client::new(format!("http://127.0.0.1:{port}"), password),
				});
			}
			thread::sleep(Duration::from_millis(100));
		}
		let _ = child.kill();
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
			let _ = child.kill();
			let _ = child.wait();
		}
	}
}
