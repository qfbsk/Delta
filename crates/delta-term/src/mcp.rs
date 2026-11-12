//! Delta's MCP endpoint. One localhost HTTP listener, JSON-RPC 2.0 on POST.
//! The UI thread owns all state; requests are handed over a channel and the
//! handler blocks for the answer. Read tools need no takeover, write tools do.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

pub struct McpRequest {
	pub tool: String,
	pub args: Value,
	pub agent: String,
	pub reply: Sender<Result<Value, String>>,
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
			if tx.send(McpRequest { tool: name.clone(), args, agent: agent.clone(), reply: reply_tx }).is_err() {
				json!({"jsonrpc": "2.0", "id": id, "result": {
					"content": [{"type": "text", "text": "Delta is not running (channel closed)."}],
					"isError": true
				}})
			} else {
				match reply_rx.recv_timeout(Duration::from_secs(20)) {
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
					Err(_) => json!({"jsonrpc": "2.0", "id": id, "result": {
						"content": [{"type": "text", "text": "Delta did not answer in 20s."}],
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
			"description": "Replace the contents of a solid editor pane. Requires an agent-owned or granted pane.",
			"inputSchema": {"type": "object", "properties": {"id": {"type": "integer"}, "text": {"type": "string"}}, "required": ["id", "text"]}
		}),
		json!({
			"name": "run_in_terminal",
			"description": "Type a command into a terminal pane and press Enter. Requires an agent-owned or granted pane.",
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
			"description": "Stop one running event (stops the AI task or process).",
			"inputSchema": {"type": "object", "properties": {"id": {"type": "integer"}}, "required": ["id"]}
		}),
		json!({
			"name": "takeover_status",
			"description": "Check whether you may take over a pane.",
			"inputSchema": {"type": "object", "properties": {"id": {"type": "integer"}}, "required": ["id"]}
		}),
	]
}
