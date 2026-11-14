//! Delta's MCP endpoint. One localhost HTTP listener, JSON-RPC 2.0 on POST.
//! The UI thread owns all state; requests are handed over a channel and the
//! handler blocks for the answer. Read tools need no takeover, write tools do.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(12 * 60 * 60);
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub struct McpRequest {
	pub tool: String,
	pub args: Value,
	pub agent: String,
	pub session: Option<String>,
	pub expires: Instant,
	pub cancelled: Arc<AtomicBool>,
	pub reply: Sender<Result<Value, String>>,
}

impl McpRequest {
	/// Check immediately before executing, including after approval. Cancellation
	/// only prevents commands not yet started; it cannot roll back execution.
	pub fn is_active(&self) -> bool {
		Instant::now() < self.expires && !self.cancelled.load(Ordering::Acquire)
	}
}

pub struct McpServer {
	pub url: String,
	pub rx: Receiver<McpRequest>,
}

pub fn start() -> std::io::Result<McpServer> {
	let listener = TcpListener::bind("127.0.0.1:0")?;
	let port = listener.local_addr()?.port();
	let (tx, rx) = mpsc::channel();
	thread::spawn(move || {
		for stream in listener.incoming().flatten() {
			let tx = tx.clone();
			thread::spawn(move || handle_connection(stream, tx));
		}
	});
	Ok(McpServer { url: format!("http://127.0.0.1:{port}/mcp"), rx })
}

fn handle_connection(mut stream: TcpStream, tx: Sender<McpRequest>) {
	let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
	let mut head = Vec::new();
	let mut byte = [0u8; 1];
	loop {
		match stream.read(&mut byte) {
			Ok(1) => {
				head.push(byte[0]);
				if head.ends_with(b"\r\n\r\n") {
					break;
				}
				if head.len() > 1 << 20 {
					return;
				}
			}
			_ => return,
		}
	}
	let head_text = String::from_utf8_lossy(&head).to_string();
	let mut lines = head_text.split("\r\n");
	let request_line = lines.next().unwrap_or("");
	let mut parts = request_line.split_whitespace();
	let method = parts.next().unwrap_or("");
	let path = parts.next().unwrap_or("");
	if method == "OPTIONS" {
		let _ = respond(&mut stream, 204, "text/plain", "");
		return;
	}
	let mut content_length = 0usize;
	let mut agent = String::from("ai");
	for line in lines {
		let Some((name, value)) = line.split_once(':') else { continue };
		match name.to_ascii_lowercase().as_str() {
			"content-length" => content_length = value.trim().parse().unwrap_or(0),
			"x-delta-agent" => agent = value.trim().to_string(),
			_ => {}
		}
	}
	if path.ends_with("/mcp") && method == "GET" {
		// We never push server-initiated messages; closing is the correct answer.
		let _ = respond(&mut stream, 405, "text/plain", "no server push");
		return;
	}
	if method != "POST" {
		let _ = respond(&mut stream, 405, "text/plain", "POST only");
		return;
	}
	let mut body = vec![0u8; content_length.min(16 << 20)];
	if content_length > 0 && stream.read_exact(&mut body).is_err() {
		return;
	}
	let request: Value = match serde_json::from_slice(&body) {
		Ok(value) => value,
		Err(err) => {
			let reply = json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": err.to_string()}});
			let _ = respond(&mut stream, 200, "application/json",
				&serde_json::to_string(&reply).unwrap_or_default());
			return;
		}
	};
	let id = request.get("id").cloned();
	let rpc_method = request["method"].as_str().unwrap_or("");
	if id.is_none() {
		// This stateless endpoint has no server-issued MCP client session. In
		// particular, do not match notifications/cancelled across connections by
		// request ID, agent header or OpenCode session metadata: none identifies
		// the originating MCP client. Cancellation notifications remain ignored.
		let _ = respond(&mut stream, 202, "text/plain", "");
		return;
	}
	let id = id.unwrap();
	let response = match rpc_method {
		"initialize" => json!({
			"jsonrpc": "2.0",
			"id": id,
			"result": {
				"protocolVersion": "2025-03-26",
				"capabilities": {"tools": {}},
				"serverInfo": {"name": "delta", "version": "0.1.0"},
			}
		}),
		"ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
		"tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tools()}}),
		"tools/call" => {
			let name = request["params"]["name"].as_str().unwrap_or("").to_string();
			let args = request["params"]["arguments"].clone();
			let (reply_tx, reply_rx) = mpsc::channel();
			let session = request["params"]["_meta"]["ai.opencode/sessionID"].as_str().map(String::from);
			let expires = Instant::now() + REQUEST_TIMEOUT;
			let cancelled = Arc::new(AtomicBool::new(false));
			if tx.send(McpRequest {
				tool: name.clone(), args, agent: agent.clone(), session, expires,
				cancelled: Arc::clone(&cancelled), reply: reply_tx,
			}).is_err() {
				cancelled.store(true, Ordering::Release);
				json!({"jsonrpc": "2.0", "id": id, "result": {
					"content": [{"type": "text", "text": "Delta is not running (channel closed)."}],
					"isError": true
				}})
			} else {
				match wait_for_reply(&stream, &reply_rx, expires, &cancelled) {
					Ok(Ok(value)) => {
						let text = value["text"].as_str().map(String::from).unwrap_or_else(|| value.to_string());
						let is_error = value["error"].as_bool().unwrap_or(false);
						json!({"jsonrpc": "2.0", "id": id, "result": {
							"content": [{"type": "text", "text": text}],
							"isError": is_error,
						}})
					}
					Ok(Err(err)) => json!({"jsonrpc": "2.0", "id": id, "result": {
						"content": [{"type": "text", "text": err}],
						"isError": true,
					}}),
					Err(err) => json!({"jsonrpc": "2.0", "id": id, "result": {
						"content": [{"type": "text", "text": err}],
						"isError": true,
					}}),
				}
			}
		}
		other => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("unknown method {other}")}}),
	};
	let text = serde_json::to_string(&response).unwrap_or_default();
	let _ = respond(&mut stream, 200, "application/json", &text);
}

fn wait_for_reply(
	stream: &TcpStream,
	reply: &Receiver<Result<Value, String>>,
	expires: Instant,
	cancelled: &AtomicBool,
) -> Result<Result<Value, String>, &'static str> {
	let result = (|| {
		stream.set_nonblocking(true).map_err(|_| "Delta cannot monitor the connection.")?;
		let mut answer = None;
		loop {
			if cancelled.load(Ordering::Acquire) {
				return Err("Delta request cancelled; commands already started are not rolled back.");
			}
			let remaining = expires.saturating_duration_since(Instant::now());
			if remaining.is_zero() {
				return Err("Delta request timed out; commands not yet started are cancelled. Commands already started are not rolled back.");
			}
			// Nonblocking peek detects FIN (including a write half-close) or reset
			// without spending the HTTP read timeout on an otherwise idle client.
			match stream.peek(&mut [0u8; 1]) {
				Ok(0) => return Err("Delta client disconnected."),
				// One HTTP request per connection. Do not leave extra bytes queued:
				// they would hide a subsequent FIN from every later peek.
				Ok(_) => return Err("Delta accepts only one request per connection."),
				Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
				Err(_) => return Err("Delta connection failed."),
			}
			// Recheck cancellation, deadline and socket after receiving a reply,
			// so an answer racing with a detected disconnect cannot win the wait.
			if let Some(answer) = answer {
				return Ok(answer);
			}
			match reply.recv_timeout(remaining.min(CANCEL_POLL_INTERVAL)) {
				Ok(value) => answer = Some(value),
				Err(RecvTimeoutError::Timeout) => {}
				Err(RecvTimeoutError::Disconnected) => return Err("Delta reply channel closed."),
			}
		}
	})();
	if result.is_err() {
		// Publish before any response I/O: the UI may still hold the request.
		cancelled.store(true, Ordering::Release);
	}
	// respond uses write_all and expects a blocking socket.
	if stream.set_nonblocking(false).is_err() {
		cancelled.store(true, Ordering::Release);
		return Err("Delta cannot restore the connection for its reply.");
	}
	result
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &str) -> std::io::Result<()> {
	let reason = match status {
		200 => "OK",
		202 => "Accepted",
		204 => "No Content",
		405 => "Method Not Allowed",
		_ => "OK",
	};
	// Without the charset, clients that follow the HTTP default decode the
	// UTF-8 JSON as Latin-1 and Chinese text comes back as mojibake.
	let head = format!(
		"HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}; charset=utf-8\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: *\r\n\r\n",
		body.len()
	);
	stream.write_all(head.as_bytes())?;
	stream.write_all(body.as_bytes())?;
	stream.flush()
}

fn tools() -> Vec<Value> {
	vec![
		json!({
			"name": "list_windows",
			"description": "List tabs and panes (id, tab, title, kind, owner, events). Reading needs no takeover.",
			"inputSchema": {"type": "object", "properties": {}}
		}),
		json!({
			"name": "read_window",
			"description": "Read a pane's visible screen and scrollback.",
			"inputSchema": {"type": "object", "properties": {"id": {"type": "integer"}}, "required": ["id"]}
		}),
		json!({
			"name": "write_window",
			"description": "Replace the contents of a solid editor pane and save. If the pane is not yours, this call itself asks the human for one-time takeover and waits for the answer; when approved it writes immediately. Do not pre-check with takeover_status.",
			"inputSchema": {"type": "object", "properties": {"id": {"type": "integer"}, "text": {"type": "string"}}, "required": ["id", "text"]}
		}),
		json!({
			"name": "run_in_terminal",
			"description": "Type a command into a terminal pane and press Enter. If the pane is not yours, this call itself asks the human for one-time takeover and waits for the answer; when approved it runs immediately. Do not pre-check with takeover_status.",
			"inputSchema": {"type": "object", "properties": {"id": {"type": "integer"}, "command": {"type": "string"}}, "required": ["id", "command"]}
		}),
		json!({
			"name": "create_window",
			"description": "Create a pane. kind: terminal|solid|even. solid needs path. AI-created panes do not steal focus.",
			"inputSchema": {"type": "object", "properties": {
				"kind": {"type": "string"}, "path": {"type": "string"},
				"command": {"type": "string"}, "title": {"type": "string"}
			}}
		}),
		json!({
			"name": "close_window",
			"description": "Close a pane. Refused while an event is bound to it.",
			"inputSchema": {"type": "object", "properties": {"id": {"type": "integer"}}, "required": ["id"]}
		}),
		json!({
			"name": "list_events",
			"description": "List running events and the panes they pin.",
			"inputSchema": {"type": "object", "properties": {}}
		}),
		json!({
			"name": "stop_event",
			"description": "Ask one running event to stop and report that the request was sent; confirm with list_events and read_window.",
			"inputSchema": {"type": "object", "properties": {"id": {"type": "integer"}}, "required": ["id"]}
		}),
		json!({
			"name": "takeover_status",
			"description": "Inspect a pane's owner and whether it is authorized for you. Not a required pre-step: write_window and run_in_terminal raise the takeover request by themselves.",
			"inputSchema": {"type": "object", "properties": {"id": {"type": "integer"}}, "required": ["id"]}
		}),
	]
}

#[cfg(test)]
mod tests {
	use super::*;

	const TEST_TIMEOUT: Duration = Duration::from_secs(5);

	fn tcp_pair() -> (TcpStream, TcpStream) {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
		client.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
		client.set_write_timeout(Some(TEST_TIMEOUT)).unwrap();
		let (server, _) = listener.accept().unwrap();
		(client, server)
	}

	fn pending_call() -> (TcpStream, McpRequest, Receiver<()>) {
		let (mut client, server) = tcp_pair();
		let (tx, rx) = mpsc::channel();
		let (done_tx, done_rx) = mpsc::channel();
		thread::spawn(move || {
			handle_connection(server, tx);
			let _ = done_tx.send(());
		});
		let body = json!({
			"jsonrpc": "2.0", "id": 7, "method": "tools/call",
			"params": {
				"name": "run_in_terminal", "arguments": {"id": 1, "command": "pwd"},
				"_meta": {"ai.opencode/sessionID": "session-1"}
			}
		}).to_string();
		write!(client,
			"POST /mcp HTTP/1.1\r\nHost: localhost\r\nX-Delta-Agent: sam\r\nContent-Length: {}\r\n\r\n{}",
			body.len(), body,
		).unwrap();
		let request = rx.recv_timeout(TEST_TIMEOUT).unwrap();
		(client, request, done_rx)
	}

	fn read_response(client: &mut TcpStream, done: Receiver<()>) -> Value {
		let mut response = String::new();
		client.read_to_string(&mut response).unwrap();
		done.recv_timeout(TEST_TIMEOUT).unwrap();
		assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
		serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap()
	}

	#[test]
	fn disconnect_cancels_only_that_clients_pending_request() {
		let (client, request, done) = pending_call();
		// Same RPC id, agent and OpenCode session on another connection.
		let (mut other_client, other, other_done) = pending_call();
		drop(client);
		done.recv_timeout(TEST_TIMEOUT).unwrap();
		assert!(request.cancelled.load(Ordering::Acquire));
		assert!(!request.is_active());
		assert!(request.reply.send(Ok(json!({"text": "too late"}))).is_err());
		assert!(other.is_active());
		other.reply.send(Ok(json!({"text": "still active"}))).unwrap();
		let response = read_response(&mut other_client, other_done);
		assert_eq!(response["result"]["content"][0]["text"], "still active");
		assert!(!other.cancelled.load(Ordering::Acquire));
	}

	#[test]
	fn normal_reply_preserves_request_and_response() {
		let before = Instant::now();
		let (mut client, request, done) = pending_call();
		assert!(request.expires >= before + REQUEST_TIMEOUT);
		assert!(request.expires <= Instant::now() + REQUEST_TIMEOUT);
		assert!(request.is_active());
		assert_eq!(request.tool, "run_in_terminal");
		assert_eq!(request.args, json!({"id": 1, "command": "pwd"}));
		assert_eq!(request.agent, "sam");
		assert_eq!(request.session.as_deref(), Some("session-1"));
		request.reply.send(Ok(json!({"text": "正常回复"}))).unwrap();
		assert_eq!(read_response(&mut client, done), json!({
			"jsonrpc": "2.0", "id": 7,
			"result": {"content": [{"type": "text", "text": "正常回复"}], "isError": false}
		}));
		assert!(!request.cancelled.load(Ordering::Acquire));
	}

	#[test]
	fn ui_cancellation_ends_wait_without_a_reply() {
		let (mut client, request, done) = pending_call();
		request.cancelled.store(true, Ordering::Release);
		let response = read_response(&mut client, done);
		assert!(!request.is_active());
		assert_eq!(response["result"]["isError"], true);
		assert!(response["result"]["content"][0]["text"].as_str().unwrap().contains("cancelled"));
		assert!(request.reply.send(Ok(json!({}))).is_err());
	}

	#[test]
	fn deadline_cancels_while_client_remains_connected() {
		let (_client, server) = tcp_pair();
		let (_tx, rx) = mpsc::channel();
		let cancelled = AtomicBool::new(false);
		let expires = Instant::now() + Duration::from_millis(20);
		let result = wait_for_reply(&server, &rx, expires, &cancelled);
		assert!(result.unwrap_err().contains("timed out"));
		assert!(Instant::now() >= expires);
		assert!(cancelled.load(Ordering::Acquire));
	}

	#[test]
	fn closed_reply_channel_cancels_request() {
		let (_client, server) = tcp_pair();
		let (tx, rx) = mpsc::channel();
		let cancelled = AtomicBool::new(false);
		drop(tx);
		let result = wait_for_reply(&server, &rx, Instant::now() + TEST_TIMEOUT, &cancelled);
		assert_eq!(result, Err("Delta reply channel closed."));
		assert!(cancelled.load(Ordering::Acquire));
	}

	#[test]
	fn ui_error_is_a_reply_not_cancellation() {
		let (mut client, request, done) = pending_call();
		request.reply.send(Err("denied".to_string())).unwrap();
		let response = read_response(&mut client, done);
		assert_eq!(response["result"]["isError"], true);
		assert_eq!(response["result"]["content"][0]["text"], "denied");
		assert!(!request.cancelled.load(Ordering::Acquire));
	}

	#[test]
	fn active_requires_both_deadline_and_no_cancellation() {
		let (reply, _rx) = mpsc::channel();
		let mut request = McpRequest {
			tool: "list_windows".to_string(), args: json!({}), agent: "sam".to_string(),
			session: None, expires: Instant::now() + TEST_TIMEOUT,
			cancelled: Arc::new(AtomicBool::new(false)), reply,
		};
		assert!(request.is_active());
		request.cancelled.store(true, Ordering::Release);
		assert!(!request.is_active());
		request.cancelled.store(false, Ordering::Release);
		request.expires = Instant::now();
		assert!(!request.is_active());
		assert!(!request.cancelled.load(Ordering::Acquire));
	}
}
