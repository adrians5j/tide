//! Loopback control channel: a tiny HTTP server the coding agent running in a
//! terminal pane reports its state to ("working" / "needs input" / "done"), so
//! TIDE can show which pane wants attention.
//!
//! Every pane's shell is spawned with `TIDE_RPC`, `TIDE_TOKEN` and `TIDE_PANE`
//! in its environment, so anything started inside that pane — Claude Code, its
//! hooks, a script — knows where to report and which pane it *is*. No PID
//! guessing, and reports can't be attributed to the wrong window.
//!
//! The channel only writes UI state: no paths are read, nothing is executed.
//! It binds 127.0.0.1 on an ephemeral port and requires the per-instance token.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// What an agent says it is doing. `Working` is the only state that clears
/// itself in the UI — the others sit there until the next report.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AgentState {
    /// Nothing in flight and nothing waiting — the workspace is free to grab.
    /// Set by a workspace reset, or reported explicitly.
    Idle,
    Working,
    /// Waiting on the human: a question, a permission prompt, a decision.
    Blocked,
    Done,
    Failed,
}

impl AgentState {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "idle" | "free" => Some(AgentState::Idle),
            "working" | "running" | "start" => Some(AgentState::Working),
            "blocked" | "needs_input" | "waiting" | "question" => Some(AgentState::Blocked),
            "done" | "finished" | "stop" => Some(AgentState::Done),
            "failed" | "error" => Some(AgentState::Failed),
            _ => None,
        }
    }

    /// Ranked for roll-up onto a tab: the state that most wants attention wins.
    pub fn priority(self) -> u8 {
        match self {
            AgentState::Blocked => 5,
            AgentState::Failed => 4,
            AgentState::Done => 3,
            AgentState::Working => 2,
            AgentState::Idle => 1,
        }
    }

    /// Idle is a "nothing to see here" marker — surfaced when scanning across
    /// workspaces, but kept out of the terminal dock so panes stay quiet.
    pub fn is_idle(self) -> bool {
        matches!(self, AgentState::Idle)
    }
}

/// A reply to a review comment, posted by the agent to `/comment`.
pub struct Reply {
    /// The comment's short id, as handed to the agent in the sent message.
    pub id: String,
    pub text: String,
}

/// One report from a pane.
pub struct Report {
    /// `TIDE_PANE` — the reporting pane's `EntityId` as a u64. `None` when the
    /// caller didn't say (curl by hand); the UI then applies it to the tab's
    /// active pane.
    pub pane: Option<u64>,
    pub state: AgentState,
    pub summary: String,
}

/// The running server: where to reach it, and the queue it fills.
pub struct Server {
    pub port: u16,
    pub token: String,
    queue: Arc<Mutex<Vec<Report>>>,
    replies: Arc<Mutex<Vec<Reply>>>,
    dirty: Arc<AtomicBool>,
}

impl Server {
    /// Bind on loopback and serve until the process exits. `None` if the port
    /// can't be bound — TIDE just runs without the channel then.
    pub fn start(token: String) -> Option<Server> {
        let listener = TcpListener::bind("127.0.0.1:0").ok()?;
        let port = listener.local_addr().ok()?.port();
        let queue: Arc<Mutex<Vec<Report>>> = Arc::new(Mutex::new(Vec::new()));
        let replies: Arc<Mutex<Vec<Reply>>> = Arc::new(Mutex::new(Vec::new()));
        let dirty = Arc::new(AtomicBool::new(false));
        {
            let (queue, replies, dirty, token) =
                (queue.clone(), replies.clone(), dirty.clone(), token.clone());
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    // one short-lived connection per report; a wedged client
                    // can't block the next one for more than the read timeout
                    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                    handle(stream, &token, &queue, &replies, &dirty);
                }
            });
        }
        Some(Server { port, token, queue, replies, dirty })
    }

    /// Take everything reported since the last call.
    pub fn drain(&self) -> Vec<Report> {
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return Vec::new();
        }
        self.queue.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
    }

    /// Take the replies posted to review comments since the last call.
    pub fn drain_replies(&self) -> Vec<Reply> {
        self.replies.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

fn handle(
    mut stream: TcpStream,
    token: &str,
    queue: &Mutex<Vec<Report>>,
    replies: &Mutex<Vec<Reply>>,
    dirty: &AtomicBool,
) {
    let Some((path, headers, body)) = read_request(&mut stream) else {
        respond(&mut stream, 400, "{\"error\":\"bad request\"}");
        return;
    };
    // health check is unauthenticated so a client can discover whether this
    // port is still a live TIDE before bothering with a token
    if path == "/health" {
        respond(&mut stream, 200, "{\"ok\":true}");
        return;
    }
    let given = headers
        .iter()
        .find(|(k, _)| k == "x-tide-token")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    if !constant_eq(given, token) {
        respond(&mut stream, 403, "{\"error\":\"bad token\"}");
        return;
    }
    // answers to review comments the agent was handed
    if path == "/comment" {
        match parse_reply(&body) {
            Some(reply) => {
                if let Ok(mut q) = replies.lock() {
                    if q.len() > 256 {
                        q.remove(0);
                    }
                    q.push(reply);
                }
                dirty.store(true, Ordering::Relaxed);
                respond(&mut stream, 200, "{\"ok\":true}");
            }
            None => respond(
                &mut stream,
                400,
                "{\"error\":\"want {\\\"id\\\":\\\"<comment id>\\\",\\\"text\\\":\\\"<reply>\\\"}\"}",
            ),
        }
        return;
    }
    if path != "/agent" {
        respond(&mut stream, 404, "{\"error\":\"unknown route\"}");
        return;
    }
    let Some(report) = parse_report(&body) else {
        respond(&mut stream, 400, "{\"error\":\"want {\\\"state\\\":\\\"working|blocked|done|failed\\\"}\"}");
        return;
    };
    if let Ok(mut q) = queue.lock() {
        // bound the queue: if the UI isn't draining, drop the oldest
        if q.len() > 256 {
            q.remove(0);
        }
        q.push(report);
    }
    dirty.store(true, Ordering::Relaxed);
    respond(&mut stream, 200, "{\"ok\":true}");
}

/// Request line + headers + body. Only what these three routes need: no
/// chunked encoding, no keep-alive, 8 KiB body cap.
fn read_request(stream: &mut TcpStream) -> Option<(String, Vec<(String, String)>, String)> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let path = line.split_whitespace().nth(1)?.to_string();
    let mut headers = Vec::new();
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).ok()? == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            let (k, v) = (k.trim().to_lowercase(), v.trim().to_string());
            if k == "content-length" {
                len = v.parse().unwrap_or(0);
            }
            headers.push((k, v));
        }
    }
    let len = len.min(8192);
    let mut body = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut body).ok()?;
    }
    Some((path, headers, String::from_utf8_lossy(&body).to_string()))
}

fn parse_report(body: &str) -> Option<Report> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let state = AgentState::parse(v.get("state")?.as_str()?.trim())?;
    // pane comes back from the shell as a string ("$TIDE_PANE"), but accept a
    // number too so hand-written JSON works
    let pane = v.get("pane").and_then(|p| match p {
        serde_json::Value::String(s) => s.trim().parse::<u64>().ok(),
        serde_json::Value::Number(n) => n.as_u64(),
        _ => None,
    });
    let summary = v
        .get("summary")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect();
    Some(Report { pane, state, summary })
}

fn parse_reply(body: &str) -> Option<Reply> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let id = v.get("id")?.as_str()?.trim().to_string();
    let text: String = v
        .get("text")?
        .as_str()?
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .take(4000)
        .collect();
    if id.is_empty() || text.trim().is_empty() {
        return None;
    }
    Some(Reply { id, text })
}

fn respond(stream: &mut TcpStream, code: u16, body: &str) {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        _ => "Not Found",
    };
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

/// Length-independent-ish comparison — the token never leaves the machine, but
/// there's no reason to leak its prefix through early exit.
fn constant_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fire one request at the server and return the status line + body.
    fn post(port: u16, path: &str, token: Option<&str>, body: &str) -> String {
        let mut c = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let auth = token.map(|t| format!("X-Tide-Token: {t}\r\n")).unwrap_or_default();
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\n{auth}Content-Length: {}\r\n\r\n{body}",
            body.len()
        );
        c.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).unwrap();
        out
    }

    /// Reports land in the queue only after the server thread handles them.
    fn wait_for_reports(s: &Server) -> Vec<Report> {
        for _ in 0..100 {
            let r = s.drain();
            if !r.is_empty() {
                return r;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Vec::new()
    }

    #[test]
    fn accepts_a_report_and_queues_it() {
        let s = Server::start("tok123".into()).unwrap();
        let resp = post(
            s.port,
            "/agent",
            Some("tok123"),
            "{\"pane\":\"42\",\"state\":\"needs_input\",\"summary\":\"which db?\"}",
        );
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        let reports = wait_for_reports(&s);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].pane, Some(42));
        assert_eq!(reports[0].state, AgentState::Blocked);
        assert_eq!(reports[0].summary, "which db?");
        assert!(s.drain().is_empty(), "drain should have emptied the queue");
    }

    #[test]
    fn rejects_a_bad_token_and_queues_nothing() {
        let s = Server::start("right".into()).unwrap();
        let resp = post(s.port, "/agent", Some("wrong"), "{\"state\":\"done\"}");
        assert!(resp.starts_with("HTTP/1.1 403"), "{resp}");
        let resp = post(s.port, "/agent", None, "{\"state\":\"done\"}");
        assert!(resp.starts_with("HTTP/1.1 403"), "{resp}");
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(s.drain().is_empty());
    }

    #[test]
    fn health_needs_no_token_and_unknown_state_is_rejected() {
        let s = Server::start("tok".into()).unwrap();
        assert!(post(s.port, "/health", None, "").starts_with("HTTP/1.1 200"));
        let resp = post(s.port, "/agent", Some("tok"), "{\"state\":\"dancing\"}");
        assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
        // no pane → the UI decides where it belongs
        let resp = post(s.port, "/agent", Some("tok"), "{\"state\":\"done\"}");
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        let reports = wait_for_reports(&s);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].pane, None);
        assert_eq!(reports[0].state, AgentState::Done);
    }
}
