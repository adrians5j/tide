use futures::channel::oneshot;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

/// One completion candidate.
#[derive(Clone)]
pub struct CompItem {
    pub label: String,
    pub insert: String,
    pub detail: String,
    pub kind: u8,
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>;

/// A minimal LSP client driving `typescript-language-server` over stdio.
pub struct Lsp {
    stdin: Arc<Mutex<ChildStdin>>,
    next_id: AtomicI64,
    pending: Pending,
    ready: Arc<AtomicBool>,
    queued: Arc<Mutex<Vec<Vec<u8>>>>, // notifications buffered until `initialized`
    _child: Child,
}

impl Lsp {
    /// Spawn the server for `root`. Returns None if no server binary is found.
    pub fn new(root: &Path) -> Option<Arc<Lsp>> {
        let bin = server_bin()?;
        let mut child = Command::new(&bin.0)
            .args(&bin.1)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        let stdin = Arc::new(Mutex::new(child.stdin.take()?));
        let stdout = child.stdout.take()?;
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let ready = Arc::new(AtomicBool::new(false));
        let queued: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));

        let lsp = Arc::new(Lsp {
            stdin: stdin.clone(),
            next_id: AtomicI64::new(1),
            pending: pending.clone(),
            ready: ready.clone(),
            queued: queued.clone(),
            _child: child,
        });

        // reader thread: parse framed messages, route responses
        {
            let pending = pending.clone();
            std::thread::spawn(move || read_loop(stdout, pending));
        }

        // initialize handshake
        let init_id = lsp.next_id.fetch_add(1, Ordering::Relaxed);
        let rx = {
            let (tx, rx) = oneshot::channel();
            pending.lock().unwrap().insert(init_id, tx);
            rx
        };
        let root_uri = format!("file://{}", root.display());
        lsp.write_raw(&framed(&json!({
            "jsonrpc": "2.0",
            "id": init_id,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": root_uri,
                "capabilities": {
                    "textDocument": {
                        "synchronization": { "didSave": true, "dynamicRegistration": false },
                        "completion": {
                            "completionItem": { "snippetSupport": false, "documentationFormat": ["plaintext"] }
                        }
                    }
                }
            }
        })));

        // finish the handshake on a background thread, then flush queued notifies
        {
            let lsp2 = lsp.clone();
            std::thread::spawn(move || {
                let _ = futures::executor::block_on(rx);
                // send `initialized`
                lsp2.write_raw(&framed(&json!({
                    "jsonrpc": "2.0",
                    "method": "initialized",
                    "params": {}
                })));
                lsp2.ready.store(true, Ordering::Relaxed);
                let mut q = lsp2.queued.lock().unwrap();
                for msg in q.drain(..) {
                    let _ = lsp2.stdin.lock().unwrap().write_all(&msg);
                }
                let _ = lsp2.stdin.lock().unwrap().flush();
            });
        }

        Some(lsp)
    }

    fn write_raw(&self, bytes: &[u8]) {
        if let Ok(mut w) = self.stdin.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    /// Send a notification (buffered until the server is initialized).
    fn notify(&self, method: &str, params: Value) {
        let msg = framed(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
        if self.ready.load(Ordering::Relaxed) {
            self.write_raw(&msg);
        } else {
            self.queued.lock().unwrap().push(msg);
        }
    }

    /// Send a request; resolves with the `result` value (or null on error).
    fn request(&self, method: &str, params: Value) -> oneshot::Receiver<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let msg = framed(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        // requests also wait for ready
        if self.ready.load(Ordering::Relaxed) {
            self.write_raw(&msg);
        } else {
            self.queued.lock().unwrap().push(msg);
        }
        rx
    }

    pub fn did_open(&self, uri: &str, language_id: &str, version: i64, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({ "textDocument": { "uri": uri, "languageId": language_id, "version": version, "text": text } }),
        );
    }

    pub fn did_change(&self, uri: &str, version: i64, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": text }]
            }),
        );
    }

    pub fn did_close(&self, uri: &str) {
        self.notify("textDocument/didClose", json!({ "textDocument": { "uri": uri } }));
    }

    /// Request completions at a 0-based (line, character).
    pub fn completion(&self, uri: &str, line: usize, character: usize) -> oneshot::Receiver<Value> {
        self.request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }),
        )
    }

    /// Request hover info at a 0-based (line, character).
    pub fn hover(&self, uri: &str, line: usize, character: usize) -> oneshot::Receiver<Value> {
        self.request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }),
        )
    }

    /// Request the definition location of the symbol at a 0-based (line, char).
    pub fn definition(&self, uri: &str, line: usize, character: usize) -> oneshot::Receiver<Value> {
        self.request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }),
        )
    }
}

/// Parse a definition response into (uri, 0-based line, 0-based char).
/// Handles `Location`, `Location[]`, and `LocationLink[]`.
pub fn parse_location(v: &Value) -> Option<(String, usize, usize)> {
    let loc = if let Some(arr) = v.as_array() {
        arr.first()?.clone()
    } else {
        v.clone()
    };
    let (uri, range) = if let Some(u) = loc.get("uri").and_then(|u| u.as_str()) {
        (u.to_string(), loc.get("range")?)
    } else if let Some(u) = loc.get("targetUri").and_then(|u| u.as_str()) {
        (
            u.to_string(),
            loc.get("targetSelectionRange").or_else(|| loc.get("targetRange"))?,
        )
    } else {
        return None;
    };
    let start = range.get("start")?;
    let line = start.get("line")?.as_u64()? as usize;
    let ch = start.get("character")?.as_u64()? as usize;
    Some((uri, line, ch))
}

/// Extract plain text from a hover response (strips markdown code fences).
pub fn parse_hover(v: &Value) -> Option<String> {
    let c = v.get("contents")?;
    let raw = if let Some(s) = c.as_str() {
        s.to_string()
    } else if let Some(val) = c.get("value").and_then(|x| x.as_str()) {
        val.to_string()
    } else if let Some(arr) = c.as_array() {
        arr.iter()
            .filter_map(|e| {
                e.get("value")
                    .and_then(|x| x.as_str())
                    .or_else(|| e.as_str())
                    .map(|s| s.to_string())
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        return None;
    };
    let cleaned: Vec<&str> = raw
        .lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .collect();
    let t = cleaned.join("\n").trim().to_string();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

/// Parse a completion response Value into our items (capped).
pub fn parse_completions(v: &Value) -> Vec<CompItem> {
    let items = if v.is_array() {
        v.as_array().cloned().unwrap_or_default()
    } else {
        v.get("items").and_then(|i| i.as_array()).cloned().unwrap_or_default()
    };
    let mut out = Vec::new();
    for it in items.iter().take(60) {
        let label = it.get("label").and_then(|l| l.as_str()).unwrap_or("").to_string();
        if label.is_empty() {
            continue;
        }
        let insert = it
            .get("insertText")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| label.clone());
        let detail = it.get("detail").and_then(|d| d.as_str()).unwrap_or("").to_string();
        let kind = it.get("kind").and_then(|k| k.as_u64()).unwrap_or(0) as u8;
        out.push(CompItem { label, insert, detail, kind });
    }
    out
}

fn framed(v: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(v).unwrap_or_default();
    let mut msg = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    msg.extend_from_slice(&body);
    msg
}

fn read_loop(stdout: std::process::ChildStdout, pending: Pending) {
    let mut reader = BufReader::new(stdout);
    loop {
        // read headers
        let mut content_len = 0usize;
        let mut header = Vec::new();
        let mut byte = [0u8; 1];
        // read until \r\n\r\n
        loop {
            if reader.read_exact(&mut byte).is_err() {
                return;
            }
            header.push(byte[0]);
            if header.ends_with(b"\r\n\r\n") {
                break;
            }
            if header.len() > 8192 {
                return; // malformed
            }
        }
        for line in String::from_utf8_lossy(&header).lines() {
            if let Some(rest) = line.strip_prefix("Content-Length:") {
                content_len = rest.trim().parse().unwrap_or(0);
            }
        }
        if content_len == 0 {
            continue;
        }
        let mut buf = vec![0u8; content_len];
        if reader.read_exact(&mut buf).is_err() {
            return;
        }
        let Ok(msg) = serde_json::from_slice::<Value>(&buf) else { continue };
        if let Some(id) = msg.get("id").and_then(|i| i.as_i64()) {
            // response to a request
            if let Some(tx) = pending.lock().unwrap().remove(&id) {
                let result = msg.get("result").cloned().unwrap_or(Value::Null);
                let _ = tx.send(result);
            }
        }
        // notifications (publishDiagnostics, etc.) ignored for now
    }
}

/// Find a typescript-language-server binary: PATH, then the codestorm sibling.
fn server_bin() -> Option<(String, Vec<String>)> {
    let candidates = [
        "typescript-language-server",
        "/Users/adrian/dev/codestorm/node_modules/.bin/typescript-language-server",
    ];
    for c in candidates {
        if Command::new(c).arg("--version").output().map(|o| o.status.success()).unwrap_or(false) {
            return Some((c.to_string(), vec!["--stdio".to_string()]));
        }
    }
    None
}
