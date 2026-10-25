//! Native ACP (Agent Client Protocol) client.
//!
//! Spawns the `@agentclientprotocol/claude-agent-acp` adapter as a subprocess and
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
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Standard base64 (with padding) — image attachments have to reach the adapter
/// as text, and pulling in a crate for 20 lines isn't worth it.
pub fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { A[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { A[n as usize & 63] as char } else { '=' });
    }
    out
}

/// The other direction: a picture the phone posted arrives as base64 and has to
/// become bytes before anything can draw it. Whitespace and a `data:` prefix are
/// tolerated because a browser's `FileReader` produces both. None when the text
/// isn't base64 at all.
pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.rsplit_once("base64,").map(|(_, rest)| rest).unwrap_or(s);
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a') as u32 + 26,
            b'0'..=b'9' => (c - b'0') as u32 + 52,
            b'+' | b'-' => 62, // `-`/`_` so a URL-safe encoding works too
            b'/' | b'_' => 63,
            _ => return None,
        })
    };
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        acc = (acc << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// One thing that happened in the agent conversation, drained by the UI poll.
pub enum AcpEvent {
    /// Session established; the client is now accepting prompts.
    Ready,
    /// A streamed chunk of assistant message text (append to the current bubble).
    Text(String),
    /// A streamed chunk of assistant reasoning ("thinking").
    Thought(String),
    /// A tool call was announced (`tool_call`): id + human title + status, plus
    /// whatever the adapter could say about it yet (a Bash command's purpose, a
    /// task's prompt).
    Tool { id: String, title: String, status: String, detail: String },
    /// A tool call changed. Every field is optional: the adapter announces a
    /// call the moment it sees one, while the tool's input is still streaming,
    /// then refines it — which is where a Bash call's actual command arrives,
    /// and later its output.
    ToolUpdate {
        id: String,
        title: Option<String>,
        status: Option<String>,
        detail: Option<String>,
    },
    /// The current prompt turn finished.
    TurnEnd,
    /// A user turn replayed by `session/load` (resuming an earlier session).
    User(String),
    /// Sessions available for this project, newest first: `(id, title)`.
    Sessions(Vec<(String, String)>),
    /// Context consumed by this session so far: `(used, window)` in tokens.
    Usage { used: u64, size: u64 },
    /// The adapter's title for the session (its summary of the first prompt).
    Title(String),
    /// Something went wrong (handshake failed, prompt errored, adapter died).
    Error(String),
}

/// A live ACP session against one project root.
pub struct Acp {
    stdin: Arc<Mutex<ChildStdin>>,
    next_id: AtomicI64,
    session: Arc<Mutex<Option<String>>>,
    queued: Arc<Mutex<Vec<String>>>, // prompts sent before the session existed
    list_id: Arc<Mutex<Option<i64>>>, // id of the in-flight session/list request
    /// What each in-flight request asked for, so an error can name it. An
    /// adapter that answers "Invalid request" and dies says nothing about
    /// which request it refused.
    asked: Arc<Mutex<HashMap<i64, &'static str>>>,
    /// The adapter's last words on stderr. Thrown away until now, which left
    /// a dead agent with no explanation anywhere.
    stderr_tail: Arc<Mutex<Vec<String>>>,
    events: Arc<Mutex<Vec<AcpEvent>>>,
    dirty: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
    child: Mutex<Child>,
}

impl Acp {
    /// Spawn the adapter for `root` and kick off the handshake on a background
    /// thread. Returns None if the adapter can't be launched (no node/npx).
    /// `rpc` is TIDE's control channel `(url, token)`; the adapter inherits it,
    /// so the agent's own Bash calls can post review-thread answers back to us
    /// exactly like a terminal agent does.
    pub fn new(root: &Path, rpc: Option<(String, String)>) -> Option<Arc<Acp>> {
        let mut cmd = Command::new("npx");
        // renamed from @zed-industries/claude-code-acp, which is deprecated and
        // frozen at 0.16.
        //
        // Pinned. Unpinned, `npx -y` takes whatever is newest, so a release
        // published at 8pm changes the agent mid-session with nothing in this
        // repo moving — and the first spawn after one races npx's own install
        // of it, which is how 0.95.0 arrived: `initialize` answered, then
        // `session/list` reached for part of the package that wasn't on disk
        // yet and the adapter exited. To take a newer one, change the version
        // here, so the upgrade is a commit that can be reverted.
        cmd.args(["-y", "@agentclientprotocol/claude-agent-acp@0.95.0"])
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // The adapter refuses to launch if it thinks it's nested inside another
        // Claude Code session; clear the markers so it starts cleanly.
        cmd.env_remove("CLAUDECODE");
        cmd.env_remove("CLAUDE_CODE_ENTRYPOINT");
        cmd.env_remove("CLAUDE_CODE_SSE_PORT");
        if let Some((url, token)) = rpc {
            cmd.env("TIDE_RPC", url);
            cmd.env("TIDE_TOKEN", token);
            cmd.env("TIDE_ROOT", root.to_string_lossy().to_string());
        }

        let mut child = cmd.spawn().ok()?;
        let stdin = Arc::new(Mutex::new(child.stdin.take()?));
        let stdout = child.stdout.take()?;
        let stderr = child.stderr.take();

        let acp = Arc::new(Acp {
            stdin: stdin.clone(),
            next_id: AtomicI64::new(1),
            session: Arc::new(Mutex::new(None)),
            queued: Arc::new(Mutex::new(Vec::new())),
            list_id: Arc::new(Mutex::new(None)),
            asked: Arc::new(Mutex::new(HashMap::new())),
            stderr_tail: Arc::new(Mutex::new(Vec::new())),
            events: Arc::new(Mutex::new(Vec::new())),
            dirty: Arc::new(AtomicBool::new(false)),
            ready: Arc::new(AtomicBool::new(false)),
            child: Mutex::new(child),
        });

        // Drain stderr into a short ring. npx and the adapter both write their
        // real complaints here — a version refusing to install, a bad
        // argument — and none of it reached the pane before.
        if let Some(stderr) = stderr {
            let tail = acp.stderr_tail.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines() {
                    let Ok(line) = line else { break };
                    if line.trim().is_empty() {
                        continue;
                    }
                    if let Ok(mut t) = tail.lock() {
                        t.push(line);
                        let n = t.len();
                        if n > 20 {
                            t.drain(..n - 20);
                        }
                    }
                }
            });
        }

        // Reader thread: parse each JSON line and route it.
        {
            let acp = acp.clone();
            let root = root.to_path_buf();
            std::thread::spawn(move || acp.read_loop(stdout, root));
        }

        // The handshake goes out before this returns. It is two writes to a
        // pipe, so there is nothing to wait for — and on a thread it raced its
        // own caller: `ensure_acp` followed by `list_sessions` could put
        // session/list on the wire first, which 0.95.0 answers with
        // "Invalid request: first ACP request must be initialize" and then
        // exits. Sending it here is what makes that ordering impossible.
        acp.handshake(root.to_path_buf());

        Some(acp)
    }

    fn handshake(&self, root: PathBuf) {
        // 1. initialize
        self.send(json!({
            "jsonrpc": "2.0",
            "id": self.take_id_for("initialize"),
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
            "id": self.take_id_for("session/new"),
            "method": "session/new",
            "params": { "cwd": root.to_string_lossy(), "mcpServers": [] }
        }));
    }

    fn take_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// An id that remembers which request it belongs to, so an error carrying
    /// it can say what was refused.
    fn take_id_for(&self, method: &'static str) -> i64 {
        let id = self.take_id();
        if let Ok(mut a) = self.asked.lock() {
            a.insert(id, method);
            // the handshake and a long session make a handful of entries, not
            // a leak, but don't let a very long run grow it without bound
            if a.len() > 64 {
                a.clear();
                a.insert(id, method);
            }
        }
        id
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
        self.prompt_with(text, &[]);
    }

    /// Send a user turn with image attachments: `(mime type, base64 bytes)`.
    /// Images are dropped if the session isn't up yet — only the text is queued,
    /// since the queue is a plain string list.
    pub fn prompt_with(&self, text: &str, images: &[(String, String)]) {
        let sid = self.session.lock().ok().and_then(|s| s.clone());
        match sid {
            Some(sid) => self.send_prompt_with(&sid, text, images),
            None => {
                if let Ok(mut q) = self.queued.lock() {
                    q.push(text.to_string());
                }
            }
        }
    }

    /// Interrupt the turn in flight (ACP `session/cancel`, a notification — the
    /// adapter answers by ending the turn, which arrives as `TurnEnd`). No-op
    /// when no session exists yet.
    /// Stop the adapter. A session that exists only to answer one question (a
    /// reviewer) has no reason to keep a Node process alive afterwards.
    pub fn shutdown(&self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    pub fn cancel(&self) {
        let Some(sid) = self.session.lock().ok().and_then(|s| s.clone()) else { return };
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": sid }
        }));
    }

    /// Start a fresh session, discarding the current one.
    pub fn new_session(&self, root: &Path) {
        self.ready.store(false, Ordering::Relaxed);
        if let Ok(mut sid) = self.session.lock() {
            *sid = None;
        }
        self.send(json!({
            "jsonrpc": "2.0",
            "id": self.take_id_for("session/new"),
            "method": "session/new",
            "params": { "cwd": root.to_string_lossy(), "mcpServers": [] }
        }));
    }

    /// Ask for this project's sessions. The answer arrives as `Sessions`.
    pub fn list_sessions(&self, root: &Path) {
        let id = self.take_id_for("session/list");
        if let Ok(mut l) = self.list_id.lock() {
            *l = Some(id);
        }
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/list",
            "params": { "cwd": root.to_string_lossy() }
        }));
    }

    /// Resume `session_id`: the adapter replays its history as `User`/`Text`
    /// events, then the session accepts new prompts as usual.
    pub fn load_session(&self, root: &Path, session_id: &str) {
        self.ready.store(false, Ordering::Relaxed);
        self.send(json!({
            "jsonrpc": "2.0",
            "id": self.take_id_for("session/load"),
            "method": "session/load",
            "params": {
                "sessionId": session_id,
                "cwd": root.to_string_lossy(),
                "mcpServers": []
            }
        }));
    }

    fn send_prompt(&self, sid: &str, text: &str) {
        self.send_prompt_with(sid, text, &[]);
    }

    fn send_prompt_with(&self, sid: &str, text: &str, images: &[(String, String)]) {
        // images first: the question usually refers to what's in them
        let mut blocks: Vec<Value> = images
            .iter()
            .map(|(mime, data)| json!({ "type": "image", "mimeType": mime, "data": data }))
            .collect();
        if !text.is_empty() || blocks.is_empty() {
            blocks.push(json!({ "type": "text", "text": text }));
        }
        self.send(json!({
            "jsonrpc": "2.0",
            "id": self.take_id_for("session/prompt"),
            "method": "session/prompt",
            "params": { "sessionId": sid, "prompt": blocks }
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
        let why = self
            .stderr_tail
            .lock()
            .ok()
            .map(|t| t.iter().rev().take(3).rev().cloned().collect::<Vec<_>>().join(" / "))
            .filter(|s| !s.is_empty());
        self.push(AcpEvent::Error(match why {
            Some(w) => format!("agent process exited: {w}"),
            None => "agent process exited".into(),
        }));
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
                let detail = tool_text(update).unwrap_or_default();
                self.push(AcpEvent::Tool { id, title, status, detail });
            }
            "user_message_chunk" => {
                if let Some(t) = update
                    .get("content")
                    .and_then(|c| c.get("text"))
                    .and_then(|t| t.as_str())
                {
                    self.push(AcpEvent::User(t.to_string()));
                }
            }
            "usage_update" => {
                let get = |k: &str| update.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                self.push(AcpEvent::Usage { used: get("used"), size: get("size") });
            }
            "session_info_update" => {
                if let Some(t) = update.get("title").and_then(|t| t.as_str()) {
                    self.push(AcpEvent::Title(t.to_string()));
                }
            }
            "tool_call_update" => {
                let id = update
                    .get("toolCallId")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                // a refining update carries the real title (Bash's `title` is
                // its command, but the first notification goes out before the
                // input has finished streaming), and the closing one carries the
                // result the model saw
                self.push(AcpEvent::ToolUpdate {
                    id,
                    title: update
                        .get("title")
                        .and_then(|s| s.as_str())
                        .filter(|t| !t.is_empty())
                        .map(|t| t.to_string()),
                    status: update.get("status").and_then(|s| s.as_str()).map(|s| s.to_string()),
                    detail: tool_text(update),
                });
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
            let what = msg
                .get("id")
                .and_then(|i| i.as_i64())
                .and_then(|id| self.asked.lock().ok().and_then(|a| a.get(&id).copied()));
            // JSON-RPC puts *what* was invalid in `data`; the message alone is
            // the category, which on its own says nothing worth acting on
            let why = err.get("data").map(|d| match d.as_str() {
                Some(t) => t.to_string(),
                None => d.to_string(),
            });
            let mut text = match what {
                Some(w) => format!("{m} ({w})"),
                None => m.to_string(),
            };
            if let Some(w) = why.filter(|w| !w.is_empty() && w != "null") {
                text.push_str(": ");
                text.push_str(&w);
            }
            self.push(AcpEvent::Error(text));
            return;
        }
        let Some(result) = msg.get("result") else { return };
        // session/list answer (matched by the id we recorded when asking)
        let is_list = msg
            .get("id")
            .and_then(|i| i.as_i64())
            .zip(self.list_id.lock().ok().and_then(|l| *l))
            .map(|(a, b)| a == b)
            .unwrap_or(false);
        if is_list {
            let sessions = result
                .get("sessions")
                .and_then(|s| s.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|s| {
                            let id = s.get("sessionId")?.as_str()?.to_string();
                            let title = s
                                .get("title")
                                .and_then(|t| t.as_str())
                                .unwrap_or("(untitled)")
                                .to_string();
                            Some((id, title))
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            self.push(AcpEvent::Sessions(sessions));
            return;
        }
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
/// The text of a tool call's `content` blocks: before it runs that's what the
/// adapter can say about it (a Bash command's description, a task's prompt);
/// after it ran it's the output the model saw. `diff` and `terminal` blocks
/// carry no text and drop out.
fn tool_text(update: &Value) -> Option<String> {
    let mut out = String::new();
    for b in update.get("content")?.as_array()? {
        if let Some(t) = b.get("content").and_then(text_of) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&t);
        }
    }
    (!out.trim().is_empty()).then_some(out)
}

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

#[cfg(test)]
mod tests {
    use super::{base64, base64_decode};

    /// Decoding undoes encoding for every length, including the two that pad.
    #[test]
    fn base64_round_trips() {
        for n in 0..40usize {
            let bytes: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            let text = base64(&bytes);
            assert_eq!(base64_decode(&text).as_deref(), Some(&bytes[..]), "n={n}");
        }
        // what a browser's FileReader hands over, prefix and newlines included
        assert_eq!(base64_decode("data:image/png;base64,aGk=").as_deref(), Some(&b"hi"[..]));
        assert_eq!(base64_decode("aG\nkh\n").as_deref(), Some(&b"hi!"[..]));
        // URL-safe alphabet, and plain nonsense
        assert_eq!(base64_decode("-_8=").as_deref(), Some(&[0xfb, 0xff][..]));
        assert!(base64_decode("not base64!").is_none());
    }

    #[test]
    fn base64_matches_the_reference_encoding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        // bytes above 0x7f must not sign-extend
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
    }
}
