//! Native ACP (Agent Client Protocol) client.
//!
//! Spawns Zed's `@zed-industries/claude-code-acp` adapter as a subprocess and
//! talks to it over stdio using **newline-delimited JSON-RPC 2.0** (one complete
//! JSON object per line — NOT LSP `Content-Length` framing).
//!
//! Auth: the adapter drives the Claude Agent SDK, which reuses the subscription
//! OAuth minted by `claude /login` (`~/.claude/.credentials.json`). No
//! `ANTHROPIC_API_KEY` is required.
//!
//! Handshake: `initialize` → `session/new` (returns a `sessionId`) → then each
//! user turn is a `session/prompt`. The adapter streams `session/update`
//! notifications (assistant text, reasoning, tool calls) and, when it wants to
//! touch the filesystem, sends us `fs/read_text_file` / `fs/write_text_file`
//! *requests* which we service against disk. Permission prompts are
//! auto-approved for now.
//!
//! The reader thread turns everything into `AcpEvent`s pushed onto a shared
//! queue; the UI drains them on its render poll (mirroring the run-console).

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

/// One thing that happened in the agent conversation, drained by the UI poll.
pub enum AcpEvent {
    /// Session established; the client is now accepting prompts.
    Ready,
    /// A streamed chunk of assistant message text (append to the current bubble).
    Text(String),
    /// A streamed chunk of assistant reasoning ("thinking").
    Thought(String),
    /// A tool call was announced (`tool_call`): id + human title + status.
    Tool { id: String, title: String, status: String },
    /// A tool call changed state (`tool_call_update`).
    ToolStatus { id: String, status: String },
    /// The current prompt turn finished.
    TurnEnd,
    /// Something went wrong (handshake failed, prompt errored, adapter died).
    Error(String),
}

/// A live ACP session against one project root.
pub struct Acp {
    stdin: Arc<Mutex<ChildStdin>>,
    next_id: AtomicI64,
    session: Arc<Mutex<Option<String>>>,
    queued: Arc<Mutex<Vec<String>>>, // prompts sent before the session existed
    events: Arc<Mutex<Vec<AcpEvent>>>,
    dirty: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
    _child: Child,
}

impl Acp {
    /// Spawn the adapter for `root` and kick off the handshake on a background
    /// thread. Returns None if the adapter can't be launched (no node/npx).
    pub fn new(root: &Path) -> Option<Arc<Acp>> {
        let mut cmd = Command::new("npx");
        cmd.args(["-y", "@zed-industries/claude-code-acp"])
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // The adapter refuses to launch if it thinks it's nested inside another
        // Claude Code session; clear the markers so it starts cleanly.
        cmd.env_remove("CLAUDECODE");
        cmd.env_remove("CLAUDE_CODE_ENTRYPOINT");
        cmd.env_remove("CLAUDE_CODE_SSE_PORT");

        let mut child = cmd.spawn().ok()?;
        let stdin = Arc::new(Mutex::new(child.stdin.take()?));
        let stdout = child.stdout.take()?;

        let acp = Arc::new(Acp {
            stdin: stdin.clone(),
            next_id: AtomicI64::new(1),
            session: Arc::new(Mutex::new(None)),
            queued: Arc::new(Mutex::new(Vec::new())),
            events: Arc::new(Mutex::new(Vec::new())),
            dirty: Arc::new(AtomicBool::new(false)),
            ready: Arc::new(AtomicBool::new(false)),
            _child: child,
        });

        // Reader thread: parse each JSON line and route it.
        {
            let acp = acp.clone();
            let root = root.to_path_buf();
            std::thread::spawn(move || acp.read_loop(stdout, root));
        }

        // Handshake thread: initialize, then create a session. Both are simple
        // request/response; we watch the reader-populated `session` for the id.
        {
            let acp = acp.clone();
            let root = root.to_path_buf();
            std::thread::spawn(move || acp.handshake(root));
        }

        Some(acp)
    }

    fn handshake(&self, root: PathBuf) {
        // 1. initialize
        self.send(json!({
            "jsonrpc": "2.0",
            "id": self.take_id(),
            "method": "initialize",
            "params": {
                "protocolVersion": 1,
                "clientCapabilities": { "fs": { "readTextFile": true, "writeTextFile": true } },
                "clientInfo": { "name": "tide", "version": "0.1.0" }
            }
        }));
        // 2. session/new. The reader stores the returned sessionId; the adapter
        // answers `initialize` first, so a short spin-wait is plenty.
        self.send(json!({
            "jsonrpc": "2.0",
            "id": self.take_id(),
            "method": "session/new",
            "params": { "cwd": root.to_string_lossy(), "mcpServers": [] }
        }));
    }

    fn take_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn send(&self, v: Value) {
        if let Ok(mut w) = self.stdin.lock() {
            let mut line = serde_json::to_vec(&v).unwrap_or_default();
            line.push(b'\n');
            let _ = w.write_all(&line);
            let _ = w.flush();
        }
    }

    fn push(&self, ev: AcpEvent) {
        if let Ok(mut q) = self.events.lock() {
            q.push(ev);
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// True while there are queued events to drain (cheap poll check).
    pub fn dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }

    /// Session established and ready to accept prompts.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    /// Take all queued events (clears the dirty flag).
    pub fn drain(&self) -> Vec<AcpEvent> {
        self.dirty.store(false, Ordering::Relaxed);
        self.events.lock().map(|mut v| std::mem::take(&mut *v)).unwrap_or_default()
    }

    /// Send a user turn. If the session isn't established yet, the prompt is
    /// queued and flushed once `session/new` returns.
    pub fn prompt(&self, text: &str) {
        let sid = self.session.lock().ok().and_then(|s| s.clone());
        match sid {
            Some(sid) => self.send_prompt(&sid, text),
            None => {
                if let Ok(mut q) = self.queued.lock() {
                    q.push(text.to_string());
                }
            }
        }
    }

    fn send_prompt(&self, sid: &str, text: &str) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": self.take_id(),
            "method": "session/prompt",
            "params": {
                "sessionId": sid,
                "prompt": [{ "type": "text", "text": text }]
            }
        }));
    }

    fn read_loop(&self, stdout: std::process::ChildStdout, root: PathBuf) {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
            let has_method = msg.get("method").and_then(|m| m.as_str()).is_some();
            let has_id = msg.get("id").is_some();

            if has_method {
                // Request-from-agent (has id) or notification (no id).
                self.on_incoming(&msg, &root);
            } else if has_id {
                // Response to one of our requests.
                self.on_response(&msg);
            }
        }
        // stdout closed → the adapter exited.
        self.ready.store(false, Ordering::Relaxed);
        self.push(AcpEvent::Error("agent process exited".into()));
    }

    /// Handle an agent→client request or a `session/update` notification.
    fn on_incoming(&self, msg: &Value, root: &Path) {
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        match method {
            "session/update" => {
                if let Some(update) = msg.get("params").and_then(|p| p.get("update")) {
                    self.on_update(update);
                }
            }
            "fs/read_text_file" => {
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                let p = msg.get("params").and_then(|p| p.get("path")).and_then(|s| s.as_str());
                match p.map(|p| read_text_slice(p, msg.get("params"))) {
                    Some(Ok(content)) => self.reply(id, json!({ "content": content })),
                    _ => self.reply_err(id, "read failed"),
                }
            }
            "fs/write_text_file" => {
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                let params = msg.get("params");
                let path = params.and_then(|p| p.get("path")).and_then(|s| s.as_str());
                let content =
                    params.and_then(|p| p.get("content")).and_then(|s| s.as_str()).unwrap_or("");
                match path.map(|p| write_text(p, content)) {
                    Some(Ok(())) => self.reply(id, Value::Null),
                    _ => self.reply_err(id, "write failed"),
                }
            }
            "session/request_permission" => {
                // Auto-approve: pick an "allow" option (fall back to the first).
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                let opts = msg
                    .get("params")
                    .and_then(|p| p.get("options"))
                    .and_then(|o| o.as_array())
                    .cloned()
                    .unwrap_or_default();
                let pick = opts
                    .iter()
                    .find(|o| {
                        o.get("kind").and_then(|k| k.as_str()).map(|k| k.starts_with("allow"))
                            == Some(true)
                    })
                    .or_else(|| opts.first())
                    .and_then(|o| o.get("optionId").cloned());
                match pick {
                    Some(option_id) => self.reply(
                        id,
                        json!({ "outcome": { "outcome": "selected", "optionId": option_id } }),
                    ),
                    None => self.reply(id, json!({ "outcome": { "outcome": "cancelled" } })),
                }
            }
            _ => {
                // Unknown agent→client request: answer with an empty result so it
                // doesn't block. (Notifications have no id and need no reply.)
                if let Some(id) = msg.get("id") {
                    if !id.is_null() {
                        let _ = root; // kept for future capability handlers
                        self.reply(id.clone(), json!({}));
                    }
                }
            }
        }
    }

    /// Map a `session/update` payload onto `AcpEvent`s.
    fn on_update(&self, update: &Value) {
        let kind = update.get("sessionUpdate").and_then(|s| s.as_str()).unwrap_or("");
        match kind {
            "agent_message_chunk" => {
                if let Some(t) = update.get("content").and_then(text_of) {
                    if !t.is_empty() {
                        self.push(AcpEvent::Text(t));
                    }
                }
            }
            "agent_thought_chunk" => {
                if let Some(t) = update.get("content").and_then(text_of) {
                    if !t.is_empty() {
                        self.push(AcpEvent::Thought(t));
                    }
                }
            }
            "tool_call" => {
                let id = update
                    .get("toolCallId")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let title = update
                    .get("title")
                    .and_then(|s| s.as_str())
                    .unwrap_or("tool")
                    .to_string();
                let status = update
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("pending")
                    .to_string();
                self.push(AcpEvent::Tool { id, title, status });
            }
            "tool_call_update" => {
                let id = update
                    .get("toolCallId")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(status) = update.get("status").and_then(|s| s.as_str()) {
                    self.push(AcpEvent::ToolStatus { id, status: status.to_string() });
                }
            }
            // available_commands_update / plan / others: ignored for now.
            _ => {}
        }
    }

    /// Handle a response to a request we sent. Handshake responses carry a
    /// `sessionId`; the end of a prompt turn carries a `stopReason`.
    fn on_response(&self, msg: &Value) {
        if let Some(err) = msg.get("error") {
            let m = err.get("message").and_then(|s| s.as_str()).unwrap_or("agent error");
            self.push(AcpEvent::Error(m.to_string()));
            return;
        }
        let Some(result) = msg.get("result") else { return };
        if let Some(sid) = result.get("sessionId").and_then(|s| s.as_str()) {
            *self.session.lock().unwrap() = Some(sid.to_string());
            self.ready.store(true, Ordering::Relaxed);
            // flush any prompts the user sent before the session was ready
            let pending = self.queued.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
            for text in pending {
                self.send_prompt(sid, &text);
            }
            self.push(AcpEvent::Ready);
        } else if result.get("stopReason").is_some() {
            self.push(AcpEvent::TurnEnd);
        }
    }

    fn reply(&self, id: Value, result: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    fn reply_err(&self, id: Value, message: &str) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32603, "message": message }
        }));
    }
}

/// Pull plain text out of a content block (`{type:"text", text:"…"}`).
fn text_of(content: &Value) -> Option<String> {
    content.get("text").and_then(|t| t.as_str()).map(|s| s.to_string())
}

/// Read a file for `fs/read_text_file`, honoring optional 1-based `line` +
/// `limit` (line count) params.
fn read_text_slice(path: &str, params: Option<&Value>) -> std::io::Result<String> {
    let text = std::fs::read_to_string(path)?;
    let start = params.and_then(|p| p.get("line")).and_then(|l| l.as_u64());
    let limit = params.and_then(|p| p.get("limit")).and_then(|l| l.as_u64());
    if start.is_none() && limit.is_none() {
        return Ok(text);
    }
    let start = start.unwrap_or(1).saturating_sub(1) as usize;
    let lines: Vec<&str> = text.lines().collect();
    let end = match limit {
        Some(n) => (start + n as usize).min(lines.len()),
        None => lines.len(),
    };
    let slice = lines.get(start..end).unwrap_or(&[]).join("\n");
    Ok(slice)
}

/// Write a file for `fs/write_text_file`, creating parent dirs as needed.
fn write_text(path: &str, content: &str) -> std::io::Result<()> {
    if let Some(dir) = PathBuf::from(path).parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, content)
}
