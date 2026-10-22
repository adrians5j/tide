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

use std::collections::HashMap;
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

/// A picture an agent wants on screen: usually a browser screenshot it just
/// took, so what it saw lands next to what it said.
pub struct Shot {
    pub path: String,
    /// One line about what it shows.
    pub note: String,
}

/// A closing summary for a job TIDE handed over: what was done, in the agent's
/// own words, shown as a card rather than lost at the end of a long turn.
pub struct Summary {
    /// What the card is about, in a few words. Empty falls back to "Summary".
    pub title: String,
    pub text: String,
}

/// A task an agent filed on the todo list.
pub struct NewTodo {
    /// Which existing task this is about. Empty files a new one — that is the
    /// only difference between filing and editing.
    pub id: String,
    /// Take it off the list. Only means anything alongside an id.
    pub delete: bool,
    /// Every field below is `None` when the request didn't mention it, which on
    /// an edit means "leave this alone" — a different thing from clearing it.
    pub text: Option<String>,
    /// The long half: context an agent has and the one-liner can't hold.
    pub details: Option<String>,
    /// A type key (`feat`, `fix`, …), or empty.
    pub category: Option<String>,
    pub status: Option<String>,
    pub link: Option<String>,
    pub scope: Option<String>,
    /// Who it's for, when it's client work.
    pub client: Option<String>,
    /// What this belongs with. TIDE fills in the workspace's title when a *new*
    /// task doesn't say, so a batch out of one session stays together.
    pub group: Option<String>,
}

/// One stop on a code tour: where to stand, and what to understand there.
pub struct TourStep {
    /// The file as the agent wrote it — repo-relative, absolute, or with the
    /// line glued on (`src/a.ts:67`). TIDE resolves it the same way it resolves
    /// a file reference in an answer.
    pub file: String,
    /// 1-based. 0 when the reference named no line.
    pub line: usize,
    /// Last line of the stop, `line` when it is a single line.
    pub end: usize,
    /// A few words naming the stop.
    pub title: String,
    /// The part of the tour this stop belongs to — "The AWS side", "Standalone"
    /// — so a long walk reads as a handful of sections rather than thirty
    /// stops. Empty when the tour didn't group itself.
    pub group: String,
    /// What happens here and why it matters, in the agent's words.
    pub text: String,
}

/// A walkthrough an agent prepared: ordered stops through code that isn't
/// running, stepped like a debugger.
pub struct Tour {
    pub title: String,
    pub steps: Vec<TourStep>,
}

/// A name proposed for the workspace.
pub struct Title {
    /// The workspace's name. Empty when the call is only moving the subtitle.
    pub text: String,
    /// Where the work stands inside that name — "UI auto syncing" under "AI
    /// Chat". `None` when the call didn't mention it, `Some("")` to clear it.
    pub subtitle: Option<String>,
    /// Rename a workspace that already has a name. Off by default, so an agent
    /// naming a fresh workspace can never quietly rewrite one the human named;
    /// the agent sets it when the human asked for the rename.
    pub force: bool,
}

/// A batch of PR files an agent wants marked (or unmarked) as viewed.
pub struct Viewed {
    /// Paths as the agent wrote them: repo-relative, absolute, `./`-prefixed.
    /// TIDE matches them against the files actually in the PR.
    pub paths: Vec<String>,
    pub viewed: bool,
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
    /// Set by `/refresh`: something changed that TIDE should re-read (a PR was
    /// opened, a branch switched).
    refresh: Arc<AtomicBool>,
    /// Workspace titles posted to `/title`; TIDE decides whether to take one.
    titles: Arc<Mutex<Vec<Title>>>,
    /// Viewed-state batches posted to `/viewed`, applied to the PR pane.
    viewed: Arc<Mutex<Vec<Viewed>>>,
    /// Tasks posted to `/todo`.
    todos: Arc<Mutex<Vec<NewTodo>>>,
    /// Screenshots posted to `/shot`.
    shots: Arc<Mutex<Vec<Shot>>>,
    /// Summaries posted to `/summary`.
    summaries: Arc<Mutex<Vec<Summary>>>,
    /// Code tours posted to `/tour`. An empty-step tour means "put it away".
    tours: Arc<Mutex<Vec<Tour>>>,
    /// What this workspace *is*, as JSON, for `GET /workspace`: the folder, the
    /// title TIDE shows for it, the branch. Kept up to date by the UI, since the
    /// server thread can't see any of it.
    info: Arc<Mutex<String>>,
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
        let refresh = Arc::new(AtomicBool::new(false));
        let titles = Arc::new(Mutex::new(Vec::new()));
        let viewed = Arc::new(Mutex::new(Vec::new()));
        let info = Arc::new(Mutex::new("{}".to_string()));
        let todos = Arc::new(Mutex::new(Vec::new()));
        let shots = Arc::new(Mutex::new(Vec::new()));
        let summaries = Arc::new(Mutex::new(Vec::new()));
        let tours = Arc::new(Mutex::new(Vec::new()));
        {
            let (queue, replies, dirty, token) =
                (queue.clone(), replies.clone(), dirty.clone(), token.clone());
            let (refresh, titles, viewed, info, todos, shots, summaries, tours) = (
                refresh.clone(),
                titles.clone(),
                viewed.clone(),
                info.clone(),
                todos.clone(),
                shots.clone(),
                summaries.clone(),
                tours.clone(),
            );
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    // one short-lived connection per report; a wedged client
                    // can't block the next one for more than the read timeout
                    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                    handle(
                        stream, &token, &queue, &replies, &dirty, &refresh, &titles, &viewed,
                        &info, &todos, &shots, &summaries, &tours,
                    );
                }
            });
        }
        Some(Server {
            port,
            token,
            queue,
            replies,
            dirty,
            refresh,
            titles,
            viewed,
            info,
            todos,
            shots,
            summaries,
            tours,
        })
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

    /// Titles proposed for this workspace since the last call.
    pub fn drain_titles(&self) -> Vec<Title> {
        self.titles.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
    }

    /// Publish what `GET /workspace` answers. Cheap enough to call on every
    /// poll; it only replaces the string when it actually changed.
    pub fn set_info(&self, json: String) {
        if let Ok(mut cur) = self.info.lock() {
            if *cur != json {
                *cur = json;
            }
        }
    }

    /// Screenshots posted since the last call.
    pub fn drain_shots(&self) -> Vec<Shot> {
        self.shots.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
    }

    /// Summaries posted since the last call.
    pub fn drain_summaries(&self) -> Vec<Summary> {
        self.summaries.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
    }

    /// Code tours posted since the last call.
    pub fn drain_tours(&self) -> Vec<Tour> {
        self.tours.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
    }

    /// Tasks filed on the todo list since the last call.
    pub fn drain_todos(&self) -> Vec<NewTodo> {
        self.todos.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
    }

    /// Viewed-state batches posted since the last call.
    pub fn drain_viewed(&self) -> Vec<Viewed> {
        self.viewed.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
    }

    /// True once since the last call if anything asked TIDE to re-read git state
    /// (an agent that just opened a PR or switched branch).
    pub fn take_refresh(&self) -> bool {
        self.refresh.swap(false, Ordering::Relaxed)
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
    refresh: &AtomicBool,
    titles: &Mutex<Vec<Title>>,
    viewed: &Mutex<Vec<Viewed>>,
    info: &Mutex<String>,
    todos: &Mutex<Vec<NewTodo>>,
    shots: &Mutex<Vec<Shot>>,
    summaries: &Mutex<Vec<Summary>>,
    tours: &Mutex<Vec<Tour>>,
) {
    let Some((method, path, headers, body)) = read_request(&mut stream) else {
        respond(&mut stream, 400, "{\"error\":\"bad request\"}");
        return;
    };
    // health check is unauthenticated so a client can discover whether this
    // port is still a live TIDE before bothering with a token
    if path == "/health" {
        respond(&mut stream, 200, "{\"ok\":true}");
        return;
    }
    // a browser opening a link can't set a header, so the phone's routes carry
    // the token in the query string instead
    let (path, query_token) = match path.split_once('?') {
        Some((p, q)) => (
            p.to_string(),
            q.split('&')
                .find_map(|kv| kv.strip_prefix("t="))
                .map(|t| t.to_string())
                .unwrap_or_default(),
        ),
        None => (path, String::new()),
    };
    let given = headers
        .iter()
        .find(|(k, _)| k == "x-tide-token")
        .map(|(_, v)| v.as_str())
        .unwrap_or(&query_token);
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
    // a name for the workspace, proposed once the agent knows what the work is.
    // TIDE ignores it when the human already named this one, unless the post
    // says `"force": true` — which is how a rename the human asked for lands.
    if path == "/title" {
        let force = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| match v.get("force")? {
                serde_json::Value::Bool(b) => Some(*b),
                serde_json::Value::String(s) => Some(!matches!(s.trim(), "false" | "0" | "no")),
                _ => None,
            })
            .unwrap_or(false);
        let title = parse_field(&body, "title").map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
        // the subtitle is its own thing: a call may move it without touching
        // the name, which is the usual case once the work has one
        let subtitle = parse_field(&body, "subtitle").map(|t| t.trim().to_string());
        if title.is_none() && subtitle.is_none() {
            respond(
                &mut stream,
                400,
                "{\"error\":\"want {\\\"title\\\":\\\"<short name>\\\"} or \
                 {\\\"subtitle\\\":\\\"<where the work is>\\\"}\"}",
            );
            return;
        }
        if let Ok(mut q) = titles.lock() {
            q.push(Title { text: title.unwrap_or_default(), subtitle, force });
        }
        dirty.store(true, Ordering::Relaxed);
        respond(&mut stream, 200, "{\"ok\":true}");
        return;
    }
    // the closing word on a job TIDE handed over
    if path == "/summary" {
        // not `parse_field`: that caps at 120 characters and strips newlines,
        // which is right for a path and wrong for a paragraph of markdown
        let text = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| {
                let k = v.get("text").or_else(|| v.get("summary"))?;
                Some(
                    k.as_str()?
                        .chars()
                        .filter(|c| *c == '\n' || !c.is_control())
                        .take(4000)
                        .collect::<String>(),
                )
            })
            .unwrap_or_default()
            .trim()
            .to_string();
        if text.is_empty() {
            respond(
                &mut stream,
                400,
                "{\"error\":\"want {\\\"text\\\":\\\"<what you did>\\\"}\"}",
            );
            return;
        }
        let title = parse_field(&body, "title").unwrap_or_default().trim().to_string();
        if let Ok(mut q) = summaries.lock() {
            if q.len() > 32 {
                q.remove(0);
            }
            q.push(Summary { title, text });
        }
        dirty.store(true, Ordering::Relaxed);
        respond(&mut stream, 200, "{\"ok\":true}");
        return;
    }
    // a walkthrough of code that isn't running: ordered stops the human steps
    // through, each with its lines and what to understand about them
    if path == "/tour" {
        match parse_tour(&body) {
            Some(tour) => {
                if let Ok(mut q) = tours.lock() {
                    if q.len() > 8 {
                        q.remove(0);
                    }
                    q.push(tour);
                }
                dirty.store(true, Ordering::Relaxed);
                respond(&mut stream, 200, "{\"ok\":true}");
            }
            None => respond(
                &mut stream,
                400,
                "{\"error\":\"want {\\\"steps\\\":[{\\\"file\\\":\\\"<path>\\\",\\\"line\\\":<n>,\
                 \\\"title\\\":\\\"<a few words>\\\",\\\"text\\\":\\\"<what happens here>\\\"}]}, \
                 and {\\\"steps\\\":[]} to put the tour away\"}",
            ),
        }
        return;
    }
    // a picture to put in the transcript: a screenshot the agent just took
    if path == "/shot" {
        let file = parse_field(&body, "path").unwrap_or_default();
        let file = file.trim().to_string();
        if file.is_empty() {
            respond(
                &mut stream,
                400,
                "{\"error\":\"want {\\\"path\\\":\\\"/path/to/shot.png\\\"}\"}",
            );
            return;
        }
        if let Ok(mut q) = shots.lock() {
            if q.len() > 32 {
                q.remove(0);
            }
            let note = parse_field(&body, "note").unwrap_or_default();
            q.push(Shot { path: file, note: note.trim().to_string() });
        }
        dirty.store(true, Ordering::Relaxed);
        respond(&mut stream, 200, "{\"ok\":true}");
        return;
    }
    // the list itself, so an agent can see what's already on it (and what it
    // just filed) rather than having to ask
    if path == "/todo" && method == "GET" {
        let list = crate::todo::load();
        respond(&mut stream, 200, &format!("{{\"todos\":{}}}", crate::todo::to_json(&list)));
        return;
    }
    // things to come back to: one task, or a batch of them under one group
    if path == "/todo" {
        let items = parse_todos(&body);
        if items.is_empty() {
            respond(
                &mut stream,
                400,
                "{\"error\":\"want {\\\"text\\\":\\\"<task>\\\"} to file one, or \
                 {\\\"id\\\":\\\"<id>\\\",…} to change one\"}",
            );
            return;
        }
        if let Ok(mut q) = todos.lock() {
            for t in items {
                if q.len() > 256 {
                    q.remove(0);
                }
                q.push(t);
            }
        }
        dirty.store(true, Ordering::Relaxed);
        respond(&mut stream, 200, "{\"ok\":true}");
        return;
    }
    // "which workspace am I in?" — the folder on disk, TIDE's title for it, and
    // the branch. An agent putting that in a PR body has to ask: the title is
    // the human's, and it changes while the session runs.
    if path == "/workspace" {
        let body = info.lock().map(|i| i.clone()).unwrap_or_else(|_| "{}".to_string());
        respond(&mut stream, 200, &body);
        return;
    }
    // files the agent has finished reviewing on the PR: the same checkbox the
    // human ticks in the PR pane, mirrored onto GitHub from there
    if path == "/viewed" {
        match parse_viewed(&body) {
            Some(v) => {
                if let Ok(mut q) = viewed.lock() {
                    if q.len() > 64 {
                        q.remove(0);
                    }
                    q.push(v);
                }
                dirty.store(true, Ordering::Relaxed);
                respond(&mut stream, 200, "{\"ok\":true}");
            }
            None => respond(
                &mut stream,
                400,
                "{\"error\":\"want {\\\"paths\\\":[\\\"<repo-relative path>\\\"],\\\"viewed\\\":true}\"}",
            ),
        }
        return;
    }
    // "I just changed something git-visible" — a PR opened, a branch switched.
    // TIDE re-reads its own state rather than waiting for the next poll or ⌘R.
    if path == "/refresh" {
        refresh.store(true, Ordering::Relaxed);
        dirty.store(true, Ordering::Relaxed);
        respond(&mut stream, 200, "{\"ok\":true}");
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
fn read_request(
    stream: &mut TcpStream,
) -> Option<(String, String, Vec<(String, String)>, String)> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_uppercase();
    let path = parts.next()?.to_string();
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
    // The body is read before the token is checked, so the cap is the only
    // thing standing between an unauthenticated caller and a 12 MB allocation.
    // Only the phone's `/say` carries a picture; everything else is JSON, and a
    // /viewed batch of hundreds of paths still fits in 64 KB.
    let big = path.starts_with("/say");
    let len = len.min(if big { 12 * 1024 * 1024 } else { 65536 });
    let mut body = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut body).ok()?;
    }
    Some((method, path, headers, String::from_utf8_lossy(&body).to_string()))
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

/// This machine's name on the tailnet, when Tailscale is up. Reachable from
/// anywhere the phone can reach the tailnet, which is the point: no port
/// forwarding, no public URL, and nothing on the open internet holding a token
/// that can type at your agents.
///
/// None when Tailscale isn't installed, isn't logged in, or is stopped, and
/// that's the honest answer: the link falls back to the local names.
fn tailnet_host() -> Option<String> {
    let bin = ["/usr/local/bin/tailscale", "/opt/homebrew/bin/tailscale",
               "/Applications/Tailscale.app/Contents/MacOS/Tailscale"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())?;
    let out = std::process::Command::new(bin)
        .args(["status", "--json"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    // Stopped or logged out: the name may still be there, but nothing answers
    if v.get("BackendState").and_then(|b| b.as_str()) != Some("Running") {
        return None;
    }
    let name = v.get("Self")?.get("DNSName")?.as_str()?.trim_end_matches('.').to_string();
    (!name.is_empty()).then_some(name)
}

/// This machine's Bonjour name, `adrians-macbook-pro-2.local`. macOS publishes
/// it over mDNS on whichever interface is up, so a phone on the same wifi
/// resolves it without anything being configured — and unlike the IP address it
/// is the same tomorrow. iOS and Android 12+ both do mDNS; older Android does
/// not, which is why the IP link stays available beside it.
fn mdns_host() -> Option<String> {
    let name = std::process::Command::new("scutil")
        .args(["--get", "LocalHostName"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())?
        .trim()
        .to_lowercase();
    (!name.is_empty()).then(|| format!("{name}.local"))
}

/// This machine's address on the local network. `route get` names the interface
/// the default route uses, which is the one a phone on the same wifi can reach;
/// a hostname would depend on the phone resolving `.local`.
fn lan_ip() -> Option<String> {
    let iface = std::process::Command::new("route")
        .args(["-n", "get", "default"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())?
        .lines()
        .find_map(|l| l.trim().strip_prefix("interface:").map(|s| s.trim().to_string()))?;
    let out = std::process::Command::new("ipconfig")
        .args(["getifaddr", &iface])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())?;
    let ip = out.trim().to_string();
    (!ip.is_empty()).then_some(ip)
}

/// One task, or a batch: `{"text": …}`, or `{"todos": [{…}, …], "group": …}`
/// where a top-level `group` applies to every new item that doesn't name its
/// own. An item carrying an `id` edits that task instead of filing one, and
/// only the fields it mentions are touched.
fn parse_todos(body: &str) -> Vec<NewTodo> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else { return Vec::new() };
    // `None` is "the request never said", which an edit has to tell apart from
    // "set this to nothing"
    let opt = |v: Option<&serde_json::Value>, cap: usize| -> Option<String> {
        v.and_then(|x| x.as_str()).map(|s| {
            let s: String = s.chars().filter(|c| !c.is_control()).take(cap).collect();
            s.trim().to_string()
        })
    };
    let shared_group = opt(v.get("group"), 80);
    let one = |item: &serde_json::Value, fallback: &Option<String>| -> Option<NewTodo> {
        // a bare string in the array is just the text
        let item = match item {
            serde_json::Value::String(s) => serde_json::json!({ "text": s }),
            other => other.clone(),
        };
        let id = opt(item.get("id"), 40).unwrap_or_default();
        let text = opt(item.get("text").or_else(|| item.get("title")), 400).filter(|t| !t.is_empty());
        // a new task needs something to say; an edit only needs to say which one
        if id.is_empty() && text.is_none() {
            return None;
        }
        let group = opt(item.get("group"), 80);
        Some(NewTodo {
            // an edit keeps the group it has unless it names a new one: the
            // batch's group is for the batch's own tasks
            group: if id.is_empty() { group.or_else(|| fallback.clone()) } else { group },
            id,
            delete: item.get("delete").and_then(|d| d.as_bool()).unwrap_or(false),
            text,
            details: opt(item.get("details").or_else(|| item.get("body")), 4000),
            category: opt(item.get("category"), 20).map(|s| s.to_lowercase()),
            status: opt(item.get("status"), 20).map(|s| s.to_lowercase()),
            link: opt(item.get("link"), 500),
            scope: opt(item.get("scope"), 60),
            client: opt(item.get("client"), 60),
        })
    };
    match v.get("todos") {
        Some(serde_json::Value::Array(a)) => {
            a.iter().filter_map(|i| one(i, &shared_group)).take(100).collect()
        }
        _ => one(&v, &shared_group).into_iter().collect(),
    }
}

/// `{"title":…,"steps":[{"file":…,"line":…,"end":…,"title":…,"text":…}]}`.
///
/// Generous about shape, because this is the one route an agent writes by hand
/// from a long answer: `path` works for `file`, `desc`/`note`/`body` for `text`,
/// a bare string in `steps` is a file reference on its own, and the line may be
/// glued to the path (`src/a.ts:67`) or sent as a string. A tour with no steps
/// is valid and means "put the tour away" — the only way an agent can.
fn parse_tour(body: &str) -> Option<Tour> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let clean = |s: &str, cap: usize| -> String {
        s.chars().filter(|c| *c == '\n' || !c.is_control()).take(cap).collect::<String>().trim().to_string()
    };
    let num = |x: Option<&serde_json::Value>| -> Option<usize> {
        match x? {
            serde_json::Value::Number(n) => n.as_u64().map(|n| n as usize),
            serde_json::Value::String(s) => s.trim().parse().ok(),
            _ => None,
        }
    };
    let steps_val = v.get("steps").or_else(|| v.get("stops"));
    // `{"steps": …}` is the shape; anything else is only a tour if it is itself
    // an array of stops
    let arr = match steps_val.or(Some(&v))? {
        serde_json::Value::Array(a) => a.clone(),
        _ => return None,
    };
    let mut steps = Vec::new();
    for item in arr.iter().take(60) {
        let item = match item {
            serde_json::Value::String(s) => serde_json::json!({ "file": s }),
            other => other.clone(),
        };
        let file = clean(
            item.get("file").or_else(|| item.get("path")).and_then(|f| f.as_str()).unwrap_or(""),
            500,
        );
        if file.is_empty() {
            continue;
        }
        // `src/a.ts:67` and `src/a.ts:67-72` carry their own lines
        let (file, glued, glued_end) = split_line_suffix(&file);
        let line = num(item.get("line")).or(glued).unwrap_or(0);
        let end = num(item.get("end")).or(glued_end).unwrap_or(line).max(line);
        steps.push(TourStep {
            file,
            line,
            end,
            title: clean(item.get("title").and_then(|t| t.as_str()).unwrap_or(""), 120),
            group: clean(
                item.get("group")
                    .or_else(|| item.get("section"))
                    .and_then(|g| g.as_str())
                    .unwrap_or(""),
                60,
            ),
            text: clean(
                item.get("text")
                    .or_else(|| item.get("desc"))
                    .or_else(|| item.get("note"))
                    .or_else(|| item.get("body"))
                    .and_then(|t| t.as_str())
                    .unwrap_or(""),
                2000,
            ),
        });
    }
    // an array that named no files at all is a malformed tour, not an empty one
    if steps.is_empty() && !arr.is_empty() {
        return None;
    }
    let title = clean(v.get("title").and_then(|t| t.as_str()).unwrap_or(""), 120);
    Some(Tour { title, steps })
}

/// `src/a.ts:67-72` → `("src/a.ts", Some(67), Some(72))`. Anything that isn't a
/// line suffix is left on the path: a Windows drive letter or a `:` in a name
/// shouldn't be eaten.
fn split_line_suffix(file: &str) -> (String, Option<usize>, Option<usize>) {
    let Some((head, tail)) = file.rsplit_once(':') else {
        return (file.to_string(), None, None);
    };
    let (a, b) = match tail.split_once('-') {
        Some((a, b)) => (a, Some(b)),
        None => (tail, None),
    };
    let Ok(line) = a.trim().parse::<usize>() else {
        return (file.to_string(), None, None);
    };
    let end = b.and_then(|b| b.trim().parse::<usize>().ok());
    (head.to_string(), Some(line), end)
}

/// `{"paths": [...], "viewed": true}`. A single string is accepted for `paths`
/// too, since marking one file is the obvious first thing anyone tries.
/// `viewed` defaults to true: unmarking is the rare case and has to be asked for.
fn parse_viewed(body: &str) -> Option<Viewed> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let one = |s: &str| -> Option<String> {
        let s: String = s.chars().filter(|c| !c.is_control()).take(500).collect();
        let s = s.trim().to_string();
        (!s.is_empty()).then_some(s)
    };
    let paths: Vec<String> = match v.get("paths").or_else(|| v.get("path"))? {
        serde_json::Value::String(s) => one(s).into_iter().collect(),
        serde_json::Value::Array(a) => {
            a.iter().filter_map(|p| p.as_str()).filter_map(one).take(2000).collect()
        }
        _ => return None,
    };
    if paths.is_empty() {
        return None;
    }
    let viewed = v.get("viewed").and_then(|b| match b {
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::String(s) => Some(!matches!(s.trim(), "false" | "0" | "no")),
        _ => None,
    });
    Some(Viewed { paths, viewed: viewed.unwrap_or(true) })
}

/// The pictures attached to a phone message: `[{"mime":…,"b64":…}]`. Anything
/// that isn't an image, or is too big to put in a prompt, is dropped here
/// rather than carried all the way to the adapter to be refused there.
fn parse_imgs(v: &serde_json::Value) -> Vec<(String, String)> {
    const MAX_B64: usize = 8 * 1024 * 1024;
    v.get("imgs")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|i| {
                    let mime = i.get("mime")?.as_str()?;
                    let b64 = i.get("b64")?.as_str()?;
                    (mime.starts_with("image/") && !b64.is_empty() && b64.len() <= MAX_B64)
                        .then(|| (mime.to_string(), b64.to_string()))
                })
                .take(6)
                .collect()
        })
        .unwrap_or_default()
}

/// One string field of a JSON body, stripped of control characters and capped.
fn parse_field(body: &str, key: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    Some(v.get(key)?.as_str()?.chars().filter(|c| !c.is_control()).take(120).collect())
}

/// One listener for every workspace, so a phone sees the whole window rather
/// than whichever project happened to turn it on.
///
/// It owns no state of its own: the UI publishes a snapshot (the workspace list,
/// plus the transcript of whichever one the phone is reading) and drains what
/// the phone typed. Serialising a transcript is the only real cost, so the hub
/// records which workspace is wanted and the UI only builds that one.
pub struct RemoteHub {
    token: String,
    port: u16,
    /// `{"wss":[…],"msgs":[…]}` — the list and the open transcript.
    snapshot: Arc<Mutex<String>>,
    /// Which workspace the phone last asked for.
    wanted: Arc<Mutex<usize>>,
    /// What the phone typed, with any pictures that came with it.
    said: Arc<Mutex<Vec<Said>>>,
    /// Pictures the transcript refers to, by gpui's content hash. The snapshot
    /// names them and the phone fetches each one once; inlining base64 in a
    /// snapshot that reprints twice a second would be absurd.
    imgs: Arc<Mutex<HashMap<u64, (String, Vec<u8>)>>>,
    /// When a phone last polled, in seconds since start. The UI uses it to stop
    /// serialising transcripts nobody is reading.
    seen: Arc<Mutex<Option<std::time::Instant>>>,
    /// The last pair of host names we worked out, and when. Finding them costs
    /// three subprocesses, and `url()` is read from the render path, so the
    /// answer is kept for a minute — long enough to cost nothing per frame,
    /// short enough that joining a different network fixes the link on its own.
    addr: Mutex<Option<(Hosts, std::time::Instant)>>,
}

/// Something typed on the phone, on its way to a workspace.
pub struct Said {
    pub ws: usize,
    pub text: String,
    /// `(mime, base64)` for each picture that went with it. The phone can paste
    /// a screenshot or pick one from the camera roll, and it travels inline
    /// because there is nowhere else to put it.
    pub imgs: Vec<(String, String)>,
}

/// The ways to reach this machine from a phone.
#[derive(Clone, Default)]
struct Hosts {
    /// `adrians-macbook-pro-2.tailxxxx.ts.net`, when Tailscale is running. The
    /// only one of the three that works from outside the house, and it works
    /// inside it too, so one bookmark covers both.
    tailnet: Option<String>,
    /// `adrians-macbook-pro-2.local`, resolved over mDNS. It survives a new DHCP
    /// lease, which is the whole reason to prefer it.
    mdns: Option<String>,
    /// `192.168.1.114`. Always works, never memorable, and wrong after the
    /// router hands out a different address.
    ip: Option<String>,
}

/// The port the phone listener asks for. Fixed, so the link is the same after
/// a restart and a bookmark on the phone keeps working. If something else has
/// it, the OS picks one and that session's link is a different number.
pub const REMOTE_PORT: u16 = 7420;

impl RemoteHub {
    pub fn start(token: String) -> Option<RemoteHub> {
        let listener = TcpListener::bind(("0.0.0.0", REMOTE_PORT))
            .or_else(|_| TcpListener::bind("0.0.0.0:0"))
            .ok()?;
        let port = listener.local_addr().ok()?.port();
        let snapshot = Arc::new(Mutex::new("{\"wss\":[],\"msgs\":[]}".to_string()));
        let wanted = Arc::new(Mutex::new(0usize));
        let said: Arc<Mutex<Vec<Said>>> = Arc::new(Mutex::new(Vec::new()));
        let imgs: Arc<Mutex<HashMap<u64, (String, Vec<u8>)>>> = Arc::new(Mutex::new(HashMap::new()));
        let seen = Arc::new(Mutex::new(None));
        {
            let (snapshot, wanted, said, seen, imgs, token) = (
                snapshot.clone(),
                wanted.clone(),
                said.clone(),
                seen.clone(),
                imgs.clone(),
                token.clone(),
            );
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    // generous: a phone on a weak signal posting a photo needs
                    // longer than a JSON line does
                    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));
                    serve_phone(stream, &token, &snapshot, &wanted, &said, &seen, &imgs);
                }
            });
        }
        Some(RemoteHub { token, port, snapshot, wanted, said, seen, imgs, addr: Mutex::new(None) })
    }

    /// The address to open on a phone. The tailnet name wins when there is one,
    /// because it is the only one that still works when you leave; the Bonjour
    /// name next, since it survives a new DHCP lease; the address last.
    pub fn url(&self) -> Option<String> {
        let h = self.hosts();
        self.link(h.tailnet.or(h.mdns).or(h.ip)?)
    }

    /// True when the link works from outside the house.
    pub fn is_far(&self) -> bool {
        self.hosts().tailnet.is_some()
    }

    /// The same page by IP address, for a phone that won't resolve `.local`.
    pub fn url_ip(&self) -> Option<String> {
        self.link(self.hosts().ip?)
    }

    fn link(&self, host: String) -> Option<String> {
        Some(format!("http://{host}:{}/m?t={}", self.port, self.token))
    }

    /// Both names, re-read at most once a minute.
    fn hosts(&self) -> Hosts {
        const FOR: std::time::Duration = std::time::Duration::from_secs(60);
        let Ok(mut slot) = self.addr.lock() else { return Hosts::default() };
        match &*slot {
            Some((h, at)) if at.elapsed() < FOR => h.clone(),
            _ => {
                let h = Hosts { tailnet: tailnet_host(), mdns: mdns_host(), ip: lan_ip() };
                *slot = Some((h.clone(), std::time::Instant::now()));
                h
            }
        }
    }

    /// Which workspace the phone is reading.
    pub fn wanted(&self) -> usize {
        self.wanted.lock().map(|w| *w).unwrap_or(0)
    }

    /// Has a phone polled recently? Nothing is serialised when nothing is there.
    pub fn watched(&self) -> bool {
        self.seen
            .lock()
            .ok()
            .and_then(|s| *s)
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(30))
    }

    pub fn publish(&self, json: String) {
        if let Ok(mut s) = self.snapshot.lock() {
            *s = json;
        }
    }

    /// Is this picture already on the server? Asked before the bytes are
    /// cloned, so an unchanged transcript costs nothing to republish.
    pub fn has_img(&self, id: u64) -> bool {
        self.imgs.lock().map(|m| m.contains_key(&id)).unwrap_or(true)
    }

    /// Keep a picture for the phone to fetch. Old ones go when the store is
    /// full: the phone only ever asks for what the current snapshot names, and
    /// that is the recent end of one transcript.
    pub fn put_img(&self, id: u64, mime: String, bytes: Vec<u8>) {
        const MAX: usize = 48;
        let Ok(mut m) = self.imgs.lock() else { return };
        if m.len() >= MAX {
            m.clear();
        }
        m.insert(id, (mime, bytes));
    }

    pub fn drain_said(&self) -> Vec<Said> {
        self.said.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
    }
}

/// The phone's three routes. Everything else on this port is a 404.
fn serve_phone(
    mut stream: TcpStream,
    token: &str,
    snapshot: &Mutex<String>,
    wanted: &Mutex<usize>,
    said: &Mutex<Vec<Said>>,
    seen: &Mutex<Option<std::time::Instant>>,
    imgs: &Mutex<HashMap<u64, (String, Vec<u8>)>>,
) {
    let Some((method, path, _headers, body)) = read_request(&mut stream) else {
        respond(&mut stream, 400, "{\"error\":\"bad request\"}");
        return;
    };
    // the browser asks before it posts, and asks without the token
    if method == "OPTIONS" {
        respond(&mut stream, 200, "{\"ok\":true}");
        return;
    }
    let (path, query) = path.split_once('?').map(|(p, q)| (p.to_string(), q.to_string())).unwrap_or((path, String::new()));
    let param = |k: &str| {
        query
            .split('&')
            .find_map(|kv| kv.strip_prefix(&format!("{k}=")))
            .map(|v| v.to_string())
    };
    if !constant_eq(&param("t").unwrap_or_default(), token) {
        respond(&mut stream, 403, "{\"error\":\"bad token\"}");
        return;
    }
    if let Ok(mut s) = seen.lock() {
        *s = Some(std::time::Instant::now());
    }
    match path.as_str() {
        "/m" => respond_html(&mut stream, MOBILE_PAGE),
        "/img" => {
            let found = param("id")
                .and_then(|v| v.parse::<u64>().ok())
                .and_then(|id| imgs.lock().ok().and_then(|m| m.get(&id).cloned()));
            match found {
                // immutable: the id is the hash of the bytes, so a cached copy
                // can never be the wrong picture
                Some((mime, bytes)) => respond_bytes(&mut stream, &mime, &bytes),
                None => respond(&mut stream, 404, "{\"error\":\"no such picture\"}"),
            }
        }
        "/thread" => {
            if let Some(n) = param("ws").and_then(|v| v.parse::<usize>().ok()) {
                if let Ok(mut w) = wanted.lock() {
                    *w = n;
                }
            }
            let body = snapshot.lock().map(|s| s.clone()).unwrap_or_else(|_| "{}".into());
            respond(&mut stream, 200, &body);
        }
        "/say" if method == "POST" => {
            let v = serde_json::from_str::<serde_json::Value>(&body).unwrap_or_default();
            let text = v
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .chars()
                .take(8000)
                .collect::<String>()
                .trim()
                .to_string();
            let imgs = parse_imgs(&v);
            // a picture on its own is a message; empty text with nothing
            // attached is not
            if text.is_empty() && imgs.is_empty() {
                respond(&mut stream, 400, "{\"error\":\"nothing to send\"}");
                return;
            }
            let ws = param("ws").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
            if let Ok(mut q) = said.lock() {
                q.push(Said { ws, text, imgs });
            }
            respond(&mut stream, 200, "{\"ok\":true}");
        }
        _ => respond(&mut stream, 404, "{\"error\":\"no such thing\"}"),
    }
}

/// The page a phone opens. Deliberately one file with no dependencies: it has
/// to load over a phone's connection to a laptop that may be on battery, and
/// anything it needed to fetch would be one more thing to go wrong.
const MOBILE_PAGE: &str = r##"<!doctype html>
<html><head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1,maximum-scale=1,user-scalable=no,viewport-fit=cover">
<meta name="apple-mobile-web-app-capable" content="yes">
<meta name="apple-mobile-web-app-status-bar-style" content="black-translucent">
<meta name="apple-mobile-web-app-title" content="TIDE">
<title>TIDE</title>
<style>
:root{color-scheme:dark;
  /* the notch and home-indicator insets, behind variables so the layout can be
     measured at a real phone's values in a desktop browser — env() itself
     cannot be emulated, which is how a 44px gap survived two fixes */
  --safe-t:env(safe-area-inset-top,0px); --safe-b:env(safe-area-inset-bottom,0px);
  --sidebar:#19171d; --bg:#1a1d21; --raised:#222529; --line:#35373b;
  --text:#d1d2d3; --dim:#9a9b9e; --blue:#1164a3; --green:#2bac76; --yellow:#e8912d}
*{box-sizing:border-box}
html{touch-action:manipulation;-webkit-text-size-adjust:100%}
html,body{height:100%}
body{margin:0;background:var(--bg);color:var(--text);
     font:15px/1.46 -apple-system,system-ui,"Helvetica Neue",sans-serif;
     overflow:hidden}

/* the drawer, Slack's workspace rail — off to the left until the ☰ pulls it in */
#side{position:fixed;inset:0 auto 0 0;width:78%;max-width:300px;z-index:3;
      background:var(--sidebar);display:flex;flex-direction:column;
      transform:translateX(-102%);transition:transform .18s ease-out}
#side.open{transform:none}
#side{padding-top:var(--safe-t)}
#side h1{margin:0;padding:14px 16px 6px;font-size:16px;color:#fff;letter-spacing:.2px}
#side p{margin:0 16px 10px;font-size:11px;color:var(--dim);
        text-transform:uppercase;letter-spacing:.07em;display:flex;align-items:center;gap:8px}
#side p span{flex:1}
/* All / Active: parked and idle workspaces are most of the list and none of
   the work, so the phone opens on the ones that are live */
#onlyon{border:1px solid var(--line);background:none;color:var(--dim);
        font:inherit;font-size:10px;text-transform:uppercase;letter-spacing:.07em;
        padding:3px 8px;border-radius:999px}
#onlyon.on{background:var(--blue);border-color:var(--blue);color:#fff}
#wss{flex:1;overflow-y:auto;padding:0 8px 18px}
#wss button{display:flex;align-items:center;gap:8px;width:100%;text-align:left;
            padding:7px 8px;margin:1px 0;border:0;border-radius:6px;background:none;
            color:#c9c9cb;font:inherit;font-size:14.5px}
#wss button.on{background:var(--blue);color:#fff;font-weight:600}
#wss button em{flex:0 0 auto;width:7px;height:7px;border-radius:50%;background:transparent}
#wss button.busy em{background:var(--green)}
#wss button span{flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
#scrim{position:fixed;inset:0;z-index:2;background:#0007;opacity:0;pointer-events:none;
       transition:opacity .18s}
#scrim.on{opacity:1;pointer-events:auto}

/* the column: header, transcript, composer.
   Pinned rather than sized at 100dvh. With a translucent status bar iOS paints
   the page from y=0 but reports dvh as the screen minus the top inset, so a
   100dvh column stops 44px short of the bottom and leaves a dead strip under
   the composer. `inset:0` is the visible viewport, whatever dvh thinks. */
#main{position:fixed;inset:0;display:flex;flex-direction:column}
header{display:flex;align-items:center;gap:10px;padding:10px 12px;background:var(--bg);
       border-bottom:1px solid var(--line);padding-top:calc(10px + var(--safe-t))}
#burger{width:32px;height:32px;border:0;border-radius:6px;background:var(--raised);
        color:var(--text);font-size:15px;line-height:32px;padding:0}
#who{flex:1;min-width:0;font-weight:700;font-size:15px;color:#fff;
     overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
#dot{flex:0 0 auto;font-size:11.5px;color:var(--dim)}
#dot.off{color:#e01e5a}

main{flex:1;overflow-y:auto;padding:10px 0 14px;-webkit-overflow-scrolling:touch}

/* a Slack message: square avatar, bold name, body under it */
.msg{display:flex;gap:9px;padding:6px 14px}
.msg:hover{background:#ffffff08}
.av{flex:0 0 auto;width:30px;height:30px;border-radius:7px;font-size:13px;font-weight:700;
    color:#fff;display:flex;align-items:center;justify-content:center;margin-top:1px}
.you .av{background:#6b4fbb}
.agent .av{background:#2c7a5c}
.tide .av{background:#1164a3}
.sum .av{background:var(--yellow);color:#1a1d21}
.shot .av{background:#4a4d52}
.bd{min-width:0;flex:1}
.nm{font-weight:700;font-size:14px;color:#fff;margin-bottom:1px}
.tx{white-space:pre-wrap;overflow-wrap:anywhere}
.sum .tx{border-left:3px solid var(--yellow);padding-left:9px}

/* rendered markdown. The blocks carry their own spacing, so the container
   stops preserving whitespace and each paragraph keeps it instead — a soft
   line break inside a paragraph is meaningful in a chat message. */
.tx.md{white-space:normal}
.tx.md > :first-child{margin-top:0}
.tx.md > :last-child{margin-bottom:0}
.tx.md p{margin:0 0 7px;white-space:pre-wrap}
.tx.md h1,.tx.md h2,.tx.md h3{margin:10px 0 4px;font-size:15px;font-weight:700;color:#fff}
.tx.md ul,.tx.md ol{margin:4px 0 7px;padding-left:21px}
.tx.md li{margin:2px 0}
.tx.md strong{font-weight:700;color:#fff}
.tx.md a{color:#6fa8ff}
.tx.md hr{border:0;border-top:1px solid var(--line);margin:9px 0}
.tx.md blockquote{margin:5px 0;padding-left:9px;border-left:3px solid var(--line);color:var(--dim)}
.tx.md code{font-family:ui-monospace,Menlo,monospace;font-size:12.5px;
            background:#ffffff14;padding:1px 4px;border-radius:4px}
/* a fence scrolls on its own rather than widening the page under it */
.tx.md pre{margin:6px 0;max-width:100%;overflow-x:auto;background:#101214;
           border:1px solid var(--line);border-radius:8px;padding:8px 10px}
.tx.md pre code{background:none;padding:0;font-size:12px;line-height:1.45;white-space:pre}

/* a tool line is scenery, not conversation: no avatar, no weight */
.tool{display:flex;gap:7px;padding:2px 14px 2px 53px;color:var(--dim);font-size:12.5px;
      font-family:ui-monospace,Menlo,monospace;overflow:hidden}
.tool b{font-weight:500;color:#6f7175;flex:0 0 auto}
.tool span{overflow:hidden;text-overflow:ellipsis;white-space:nowrap}

.pics{display:flex;flex-wrap:wrap;gap:6px;margin-top:6px}
.pics img{max-width:100%;max-height:340px;border-radius:8px;border:1px solid var(--line);
          display:block}

/* exactly the home-indicator strip, no decorative extra on top of it: the
   composer should sit as close to the bottom edge as the system allows */
footer{position:relative;border-top:1px solid var(--line);background:var(--bg);padding:8px 10px;
       padding-bottom:max(8px, var(--safe-b))}

/* jump to the latest. It hangs off the top of the composer, so it follows the
   composer up as that grows instead of being covered by it. */
#down{position:absolute;top:-44px;left:50%;transform:translateX(-50%);
      width:34px;height:34px;border-radius:50%;border:1px solid var(--line);
      background:#2b2d31;color:#e8e8e9;font-size:17px;line-height:1;
      display:flex;align-items:center;justify-content:center;
      box-shadow:0 2px 8px #0006;cursor:pointer}
#down[hidden]{display:none}
#att{display:flex;gap:6px;overflow-x:auto;padding-bottom:7px}
#att:empty{display:none}
#att figure{position:relative;margin:0;flex:0 0 auto}
#att img{height:60px;border-radius:6px;display:block}
#att span{position:absolute;top:-6px;right:-6px;width:20px;height:20px;border-radius:50%;
          background:var(--raised);border:1px solid var(--line);color:var(--text);
          font-size:13px;line-height:18px;text-align:center}
/* Slack's composer: the text on top, the controls on their own row beneath,
   inside one rounded box */
form{display:flex;flex-direction:column;gap:4px;border:1px solid var(--line);
     border-radius:12px;padding:8px 8px 6px;background:var(--bg)}
textarea{width:100%;min-height:24px;max-height:34dvh;padding:2px 2px 4px;border:0;
         background:none;color:var(--text);
         font:16px/1.4 -apple-system,system-ui,sans-serif;resize:none;outline:none}
#acts{display:flex;align-items:center;gap:4px}
#acts .sp{flex:1}
form label,form button{flex:0 0 auto;width:38px;height:38px;border:0;border-radius:9px;
     background:none;color:var(--dim);font-size:21px;line-height:38px;
     text-align:center;padding:0}
form label:active,form button:active{background:var(--raised)}
#send{background:#007a5a;color:#fff;font-size:19px}
#send:active{background:#148567}
#mic.rec{background:#e01e5a;color:#fff}
/* ?debug=1 — what this device actually reports. env() insets cannot be
   emulated in a desktop browser, so when the layout is wrong on a phone this
   is the only way to see why without guessing. */
#dbg{position:fixed;left:0;right:0;bottom:0;z-index:9;background:#000c;color:#9ae89a;
     font:11px/1.5 ui-monospace,Menlo,monospace;padding:6px 8px;white-space:pre}
#nl{font-size:18px}
</style></head>
<body>
<aside id="side">
  <h1>TIDE</h1>
  <p><span>Workspaces</span><button type="button" id="onlyon" class="on">Active</button></p>
  <nav id="wss"></nav>
</aside>
<div id="scrim"></div>
<pre id="dbg" hidden></pre>
<div id="main">
  <header>
    <button id="burger" aria-label="Workspaces">&#9776;</button>
    <div id="who">connecting…</div>
    <span id="dot"></span>
  </header>
  <main id="log"></main>
  <footer>
    <button type="button" id="down" aria-label="Jump to the latest" hidden>&#8595;</button>
    <div id="att"></div>
    <form id="f">
      <textarea id="t" placeholder="Message the agent" rows="1"
                autocapitalize="sentences" enterkeyhint="send"></textarea>
      <div id="acts">
        <label id="clip" for="file" title="Attach an image">&#65291;</label>
        <input id="file" type="file" accept="image/*" multiple hidden>
        <button type="button" id="nl" title="New line">&#9166;</button>
        <button type="button" id="mic" title="Dictate" hidden>&#127908;</button>
        <span class="sp"></span>
        <button type="submit" id="send" title="Send">&#10148;</button>
      </div>
    </form>
  </footer>
</div>
<script>
for (const ev of ['gesturestart', 'gesturechange', 'gestureend']) {
  document.addEventListener(ev, (e) => e.preventDefault(), {passive: false});
}
const $ = (id) => document.getElementById(id);
const token = new URLSearchParams(location.search).get('t') || '';
const log = $('log'), dot = $('dot'), who = $('who'), box = $('t'), nav = $('wss');
// Active by default: most of the list is parked work. Remembered per device,
// in a try because a private window throws on storage rather than returning
// null, and that would take the whole script with it
let onlyOn = true;
try { onlyOn = localStorage.getItem('tideOnlyActive') !== '0'; } catch (e) {}
const onlyBtn = $('onlyon');
const paintOnly = () => {
  onlyBtn.classList.toggle('on', onlyOn);
  onlyBtn.textContent = onlyOn ? 'Active' : 'All';
};
onlyBtn.onclick = () => {
  onlyOn = !onlyOn;
  try { localStorage.setItem('tideOnlyActive', onlyOn ? '1' : '0'); } catch (e) {}
  paintOnly();
  lastNav = ''; // the list is drawn by the poll; make the next one redraw it
  pull();
};
paintOnly();
const att = $('att'), form = $('f'), file = $('file'), side = $('side'), scrim = $('scrim');
const down = $('down');

let ws = null;
try { const v = localStorage.getItem('tide.ws'); if (v !== null) ws = +v; } catch (e) {}
let last = '', lastNav = '', pending = [], pinned = true, landed = false;

// the drawer
const drawer = (open) => { side.classList.toggle('open', open); scrim.classList.toggle('on', open); };
$('burger').onclick = () => drawer(!side.classList.contains('open'));
scrim.onclick = () => drawer(false);

function pick(i) {
  ws = i;
  try { localStorage.setItem('tide.ws', String(i)); } catch (e) {}
  last = ''; lastNav = ''; landed = false; pinned = true;
  drawer(false);
  pull();
}

// "am I reading history, or watching the end?" Only the second one gets dragged
// along when something new arrives.
log.addEventListener('scroll', () => {
  pinned = log.scrollHeight - log.scrollTop - log.clientHeight < 90;
  showDown();
});
const toEnd = () => { log.scrollTop = log.scrollHeight; showDown(); };
// the arrow is only ever an answer to "I have scrolled up": at the end of the
// thread there is nothing for it to do, and it would sit on top of the last
// message for no reason
const showDown = () => { down.hidden = pinned; };
down.addEventListener('click', () => {
  pinned = true;
  log.scrollTo({top: log.scrollHeight, behavior: 'smooth'});
  showDown();
});

async function pull() {
  try {
    const r = await fetch('/thread?ws=' + (ws ?? 0) + '&t=' + encodeURIComponent(token), {cache:'no-store'});
    if (!r.ok) throw new Error(r.status);
    const d = await r.json();
    if (ws === null && typeof d.active === 'number') { pick(d.active); return; }
    dot.classList.remove('off');
    who.textContent = d.title || d.folder || 'TIDE';
    dot.textContent = d.busy ? 'working…' : '';

    const navStamp = JSON.stringify(d.wss || []) + '|' + onlyOn + '|' + ws;
    if (navStamp !== lastNav) {
      lastNav = navStamp;
      nav.innerHTML = '';
      (d.wss || []).forEach((w, i) => {
        // parked or idle ones are hidden on Active, unless you are in one:
        // the list must never hide where you actually are
        if (onlyOn && w.off && i !== ws) return;
        const b = document.createElement('button');
        b.className = (i === ws ? 'on ' : '') + (w.busy ? 'busy' : '');
        const em = document.createElement('em');
        const sp = document.createElement('span');
        sp.textContent = w.n || ('#' + i);
        b.append(em, sp);
        b.onclick = () => pick(i);
        nav.append(b);
      });
      if (!nav.children.length) {
        const p = document.createElement('div');
        p.style.cssText = 'padding:8px;color:var(--dim);font-size:12px';
        p.textContent = 'Nothing active. Tap All to see the rest.';
        nav.append(p);
      }
      // nine workspaces do not fit; put the one you are in on screen
      const on = nav.querySelector('button.on');
      if (on) on.scrollIntoView({block: 'nearest'});
    }

    const stamp = JSON.stringify(d.msgs);
    if (stamp !== last) {
      last = stamp;
      const stick = pinned || !landed;
      log.innerHTML = '';
      for (const m of d.msgs || []) log.append(row(m));
      landed = true;
      if (stick) { toEnd(); requestAnimationFrame(toEnd); }
    }
  } catch (e) { dot.classList.add('off'); dot.textContent = 'offline'; }
}

const NAMES = {you: 'You', tide: 'TIDE', agent: 'Claude', sum: 'Summary', shot: 'Screenshot'};

// ── markdown ────────────────────────────────────────────────
// Agents write in markdown, and unrendered it reads as noise on a phone:
// every bullet a dash, every emphasis a pair of stars, a code block a wall.
//
// Everything below builds DOM nodes and fills them with textContent. The
// text comes from an agent, and through it from whatever the agent read —
// a PR body, a web page, a file — so it is never handed to innerHTML.
function el(tag, text) {
  const e = document.createElement(tag);
  if (text != null) e.textContent = text;
  return e;
}

// `code`, **bold**, ~~struck~~, *em*, _em_, [text](url), bare http(s) url
const URL_IN = '(?:[^()\\s]|\\([^()\\s]*\\))+';
const INLINE = new RegExp(
  '`([^`]+)`' +
  '|\\*\\*([^*]+)\\*\\*' +
  '|~~([^~]+)~~' +
  '|\\*([^*\\n]+)\\*' +
  '|_([^_\\n]+)_' +
  '|\\[([^\\]]*)\\]\\((' + URL_IN + ')\\)' +
  '|(https?:\\/\\/' + URL_IN + ')'
);

function link(href, text) {
  // only http(s) becomes tappable: a `javascript:` or `data:` url in a message
  // stays text. So does a relative one — `[the test](packages/a/b.test.ts)` is
  // common in agent writing, and the path is the useful half, so keep both
  if (!/^https?:\/\//i.test(href)) {
    return document.createTextNode(text === href ? text : text + ' (' + href + ')');
  }
  const a = el('a', text);
  a.href = href;
  a.target = '_blank';
  a.rel = 'noopener noreferrer';
  return a;
}

function inline(s, into) {
  for (;;) {
    const m = INLINE.exec(s);
    if (!m) { if (s) into.append(s); return; }
    if (m.index) into.append(s.slice(0, m.index));
    if (m[1] != null) into.append(el('code', m[1]));
    else if (m[2] != null) into.append(el('strong', m[2]));
    else if (m[3] != null) into.append(el('s', m[3]));
    else if (m[4] != null) into.append(el('em', m[4]));
    else if (m[5] != null) into.append(el('em', m[5]));
    else if (m[7] != null) into.append(link(m[7], m[6] || m[7]));
    else into.append(link(m[8], m[8]));
    s = s.slice(m.index + m[0].length);
  }
}

const BULLET = /^\s*[-*•]\s+(.*)$/;
const NUMBER = /^\s*\d+[.)]\s+(.*)$/;

function md(text) {
  const out = document.createDocumentFragment();
  const lines = String(text == null ? '' : text).split('\n');
  let para = [];
  const flush = () => {
    if (!para.length) return;
    const p = el('p');
    inline(para.join('\n'), p);
    out.append(p);
    para = [];
  };
  let i = 0;
  while (i < lines.length) {
    const ln = lines[i];
    // a fence runs to its closing ``` or to the end of the message, which is
    // the usual shape mid-turn: the agent is still writing the block
    if (/^\s*```/.test(ln)) {
      flush();
      const body = [];
      i++;
      while (i < lines.length && !/^\s*```/.test(lines[i])) body.push(lines[i++]);
      i++;
      out.append(el('pre').appendChild(el('code', body.join('\n'))).parentNode);
      continue;
    }
    const h = ln.match(/^(#{1,6})\s+(.*)$/);
    if (h) {
      flush();
      // three sizes is all a phone column has room to distinguish
      const e = el('h' + Math.min(h[1].length, 3));
      inline(h[2], e);
      out.append(e);
      i++;
      continue;
    }
    if (/^ {0,3}(-{3,}|\*{3,}|_{3,})\s*$/.test(ln)) {
      flush();
      out.append(el('hr'));
      i++;
      continue;
    }
    if (/^\s*>\s?/.test(ln)) {
      flush();
      const body = [];
      while (i < lines.length && /^\s*>\s?/.test(lines[i])) {
        body.push(lines[i].replace(/^\s*>\s?/, ''));
        i++;
      }
      const q = el('blockquote');
      inline(body.join('\n'), q);
      out.append(q);
      continue;
    }
    const rx = NUMBER.test(ln) ? NUMBER : BULLET.test(ln) ? BULLET : null;
    if (rx) {
      flush();
      const list = el(rx === NUMBER ? 'ol' : 'ul');
      let m;
      while (i < lines.length && (m = lines[i].match(rx))) {
        const li = el('li');
        inline(m[1], li);
        list.append(li);
        i++;
      }
      out.append(list);
      continue;
    }
    if (!ln.trim()) flush();
    else para.push(ln);
    i++;
  }
  flush();
  return out;
}

function row(m) {
  const kind = m.k || 'agent';
  if (kind === 'tool') {
    const el = document.createElement('div');
    el.className = 'tool';
    const b = document.createElement('b'); b.textContent = m.w || '';
    const sp = document.createElement('span'); sp.textContent = m.t || '';
    el.append(b, sp);
    return el;
  }
  const el = document.createElement('div');
  el.className = 'msg ' + kind;
  const name = m.w || NAMES[kind] || '';
  const av = document.createElement('div');
  av.className = 'av';
  av.textContent = (name.match(/[a-z0-9]/i) || ['?'])[0].toUpperCase();
  const bd = document.createElement('div');
  bd.className = 'bd';
  const nm = document.createElement('div');
  nm.className = 'nm';
  nm.textContent = name;
  bd.append(nm);
  if (m.t) {
    const tx = document.createElement('div');
    tx.className = 'tx md';
    tx.append(md(m.t));
    bd.append(tx);
  }
  if (m.imgs && m.imgs.length) {
    const pics = document.createElement('div');
    pics.className = 'pics';
    for (const id of m.imgs) {
      const im = document.createElement('img');
      im.loading = 'lazy';
      im.src = '/img?id=' + id + '&t=' + encodeURIComponent(token);
      // a picture arriving changes the height under you; keep the end in view
      im.onload = () => { if (pinned) toEnd(); };
      pics.append(im);
    }
    bd.append(pics);
  }
  el.append(av, bd);
  return el;
}

box.addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && !e.shiftKey && !e.altKey) { e.preventDefault(); form.requestSubmit(); }
});

// return sends, so the break has to live somewhere. A phone keyboard has no
// ⇧↩, which is the whole reason this button exists.
$('nl').onclick = () => {
  const a = box.selectionStart ?? box.value.length, b = box.selectionEnd ?? a;
  box.value = box.value.slice(0, a) + '\n' + box.value.slice(b);
  box.selectionStart = box.selectionEnd = a + 1;
  box.focus();
  box.dispatchEvent(new Event('input'));
};
box.addEventListener('input', () => {
  box.style.height = 'auto';
  box.style.height = Math.min(box.scrollHeight, window.innerHeight * 0.34) + 'px';
});

box.addEventListener('paste', (e) => {
  const files = [...(e.clipboardData?.items || [])]
    .filter((i) => i.kind === 'file' && i.type.startsWith('image/'))
    .map((i) => i.getAsFile());
  if (!files.length) return;
  e.preventDefault();
  files.forEach(addFile);
});
file.addEventListener('change', () => { [...file.files].forEach(addFile); file.value = ''; });

async function addFile(f) {
  if (!f || !f.type.startsWith('image/')) return;
  pending.push(await shrink(f));
  drawAtt();
}

// a phone photo is 3000px across and several megabytes, and the whole thing
// travels inline in the prompt. 1600px of JPEG reads the same and costs a
// fifth as much. The canvas also turns iOS HEIC into something the adapter
// will accept.
function shrink(f) {
  return new Promise((done) => {
    const img = new Image(), url = URL.createObjectURL(f);
    img.onload = () => {
      const scale = Math.min(1, 1600 / Math.max(img.width, img.height));
      const c = document.createElement('canvas');
      c.width = Math.round(img.width * scale);
      c.height = Math.round(img.height * scale);
      c.getContext('2d').drawImage(img, 0, 0, c.width, c.height);
      URL.revokeObjectURL(url);
      const u = c.toDataURL('image/jpeg', 0.82);
      done({mime: 'image/jpeg', b64: u.split(',')[1], url: u});
    };
    img.onerror = () => { URL.revokeObjectURL(url); raw(f).then(done); };
    img.src = url;
  });
}

function raw(f) {
  return new Promise((done) => {
    const r = new FileReader();
    r.onload = () => { const u = String(r.result); done({mime: f.type, b64: u.split(',')[1], url: u}); };
    r.readAsDataURL(f);
  });
}

function drawAtt() {
  att.innerHTML = '';
  pending.forEach((p, i) => {
    const fig = document.createElement('figure');
    const im = document.createElement('img'); im.src = p.url;
    const x = document.createElement('span'); x.textContent = '×';
    x.onclick = () => { pending.splice(i, 1); drawAtt(); };
    fig.append(im, x);
    att.append(fig);
  });
}

// dictation, where the browser has it. The button stays hidden rather than
// sitting there dead: Safari grew webkitSpeechRecognition late and not
// everywhere, and the keyboard's own mic key still works regardless.
const SR = window.SpeechRecognition || window.webkitSpeechRecognition;
if (SR) {
  const mic = $('mic');
  mic.hidden = false;
  let rec = null, base = '';
  mic.onclick = () => {
    if (rec) { rec.stop(); return; }
    rec = new SR();
    rec.lang = navigator.language || 'en-US';
    rec.continuous = true;
    rec.interimResults = true;
    base = box.value ? box.value.replace(/\s*$/, ' ') : '';
    rec.onresult = (e) => {
      let out = '';
      for (let i = 0; i < e.results.length; i++) out += e.results[i][0].transcript;
      box.value = base + out;
      box.dispatchEvent(new Event('input'));
    };
    const stop = () => { rec = null; mic.classList.remove('rec'); };
    rec.onend = stop;
    rec.onerror = stop;
    mic.classList.add('rec');
    rec.start();
  };
}

form.addEventListener('submit', async (e) => {
  e.preventDefault();
  const text = box.value.trim();
  if (!text && !pending.length) return;
  const imgs = pending.map((p) => ({mime: p.mime, b64: p.b64}));
  box.value = '';
  box.style.height = 'auto';
  pending = [];
  drawAtt();
  pinned = true;
  await fetch('/say?ws=' + (ws ?? 0) + '&t=' + encodeURIComponent(token), {
    method: 'POST', headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({text, imgs})
  });
  pull();
});
if (new URLSearchParams(location.search).get('debug')) {
  const d = $('dbg');
  d.hidden = false;
  const show = () => {
    const cs = getComputedStyle(document.documentElement);
    const ft = document.querySelector('footer').getBoundingClientRect();
    const fm = document.querySelector('form').getBoundingClientRect();
    const vv = window.visualViewport;
    d.textContent = [
      'innerHeight  ' + innerHeight + '   dvh ' + Math.round(parseFloat(getComputedStyle(document.body).height)),
      'visualVp     ' + (vv ? Math.round(vv.height) + ' off ' + Math.round(vv.offsetTop) : '-'),
      'safe top/bot ' + cs.getPropertyValue('--safe-t').trim() + ' / ' + cs.getPropertyValue('--safe-b').trim(),
      'footer bot   ' + Math.round(ft.bottom) + '   height ' + Math.round(ft.height),
      'form bot     ' + Math.round(fm.bottom),
      'gap below    ' + Math.round(innerHeight - fm.bottom),
      'standalone   ' + (navigator.standalone === true)
    ].join('\n');
  };
  show();
  setInterval(show, 1000);
  window.visualViewport?.addEventListener('resize', show);
}
pull(); setInterval(pull, 2000);
</script>
</body></html>"##;

/// A picture, cached hard: the id is a hash of the bytes it serves.
fn respond_bytes(stream: &mut TcpStream, mime: &str, bytes: &[u8]) {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\n\
         Cache-Control: public, max-age=31536000, immutable\r\n{CORS}Connection: close\r\n\r\n",
        bytes.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(bytes);
    let _ = stream.flush();
}

fn respond_html(stream: &mut TcpStream, body: &str) {
    let out = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(out.as_bytes());
    let _ = stream.flush();
}

fn respond(stream: &mut TcpStream, code: u16, body: &str) {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        _ => "Not Found",
    };
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{CORS}Connection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

/// Sent with every answer the hub gives. A browser extension talking to TIDE
/// is a cross-origin caller, and without these the browser throws the answer
/// away before the extension sees it. It opens nothing up: every route still
/// wants the token, and a page that doesn't have it gets a 403 to read.
const CORS: &str = "Access-Control-Allow-Origin: *\r\n\
     Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
     Access-Control-Allow-Headers: Content-Type, X-Tide-Token\r\n\
     Access-Control-Max-Age: 86400\r\n";

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

    fn get(port: u16, path: &str, token: Option<&str>) -> String {
        let mut c = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let auth = token.map(|t| format!("X-Tide-Token: {t}\r\n")).unwrap_or_default();
        let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n{auth}\r\n");
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

    /// A screenshot the agent wants on screen.
    #[test]
    fn queues_a_screenshot() {
        let s = Server::start("tok".into()).unwrap();
        let body = "{\"path\":\" /tmp/shot.png \",\"note\":\"admin after saving\"}";
        assert!(post(s.port, "/shot", Some("tok"), body).starts_with("HTTP/1.1 200"));
        let shots = s.drain_shots();
        assert_eq!(shots.len(), 1);
        assert_eq!(shots[0].path, "/tmp/shot.png");
        assert_eq!(shots[0].note, "admin after saving");
        // a picture with no path isn't a picture
        assert!(post(s.port, "/shot", Some("tok"), "{\"note\":\"x\"}").starts_with("HTTP/1.1 400"));
        assert!(s.drain_shots().is_empty());
    }

    /// A tour the agent prepared: ordered stops, each with its lines and what
    /// to understand there.
    #[test]
    fn queues_a_code_tour() {
        let s = Server::start("tok".into()).unwrap();
        let body = "{\"title\":\"Task dispatch\",\"steps\":[\
            {\"file\":\"src/a.ts\",\"line\":67,\"end\":72,\"title\":\"trigger()\",\"text\":\"saves it\",\"group\":\"The API side\"},\
            {\"file\":\"src/loop.ts:39-44\",\"title\":\"dispatch\",\"desc\":\"one iteration\"},\
            \"src/c.ts:12\"]}";
        assert!(post(s.port, "/tour", Some("tok"), body).starts_with("HTTP/1.1 200"));
        let tours = s.drain_tours();
        assert_eq!(tours.len(), 1);
        let t = &tours[0];
        assert_eq!(t.title, "Task dispatch");
        assert_eq!(t.steps.len(), 3);
        assert_eq!((t.steps[0].line, t.steps[0].end), (67, 72));
        assert_eq!(t.steps[0].text, "saves it");
        assert_eq!(t.steps[0].group, "The API side", "stops can say which part they belong to");
        assert_eq!(t.steps[1].group, "", "and a tour that doesn't group itself still works");
        // the lines may ride on the path, and `desc` is accepted for `text`
        assert_eq!(t.steps[1].file, "src/loop.ts");
        assert_eq!((t.steps[1].line, t.steps[1].end), (39, 44));
        assert_eq!(t.steps[1].text, "one iteration");
        // a bare string is a stop that is only a place
        assert_eq!(t.steps[2].file, "src/c.ts");
        assert_eq!((t.steps[2].line, t.steps[2].end), (12, 12));
        assert!(s.drain_tours().is_empty());
    }

    /// No steps puts the tour away; steps that name no file at all are a
    /// malformed tour and are refused, rather than quietly clearing one.
    #[test]
    fn an_empty_tour_clears_and_a_broken_one_is_refused() {
        let s = Server::start("tok".into()).unwrap();
        assert!(post(s.port, "/tour", Some("tok"), "{\"steps\":[]}").starts_with("HTTP/1.1 200"));
        assert_eq!(s.drain_tours().len(), 1, "an empty tour still arrives: it means stop");
        assert!(post(s.port, "/tour", Some("tok"), "{\"steps\":[{\"text\":\"x\"}]}")
            .starts_with("HTTP/1.1 400"));
        assert!(post(s.port, "/tour", Some("tok"), "{\"title\":\"x\"}").starts_with("HTTP/1.1 400"));
        assert!(post(s.port, "/tour", None, "{\"steps\":[]}").starts_with("HTTP/1.1 403"));
        assert!(s.drain_tours().is_empty());
    }

    /// A batch an agent backlogs out of one session: one call, one group.
    #[test]
    fn queues_filed_todos() {
        let s = Server::start("tok".into()).unwrap();
        let body = "{\"todos\":[{\"text\":\"drop the shim\",\"category\":\"refactor\",\"scope\":\"api\"},\
                    \"chase the flaky test\"],\"group\":\"license flags\"}";
        assert!(post(s.port, "/todo", Some("tok"), body).starts_with("HTTP/1.1 200"));
        let filed = s.drain_todos();
        assert_eq!(filed.len(), 2);
        assert_eq!(filed[0].text.as_deref(), Some("drop the shim"));
        assert_eq!(filed[0].category.as_deref(), Some("refactor"));
        assert_eq!(filed[0].scope.as_deref(), Some("api"));
        // fields the request never mentioned stay unsaid, so an edit can tell
        // them apart from ones it was asked to clear
        assert_eq!(filed[0].link, None);
        assert!(filed[0].id.is_empty(), "no id means a new task");
        // a bare string in the array is just the text, and both take the batch's
        // group
        assert_eq!(filed[1].text.as_deref(), Some("chase the flaky test"));
        assert!(filed.iter().all(|t| t.group.as_deref() == Some("license flags")));
        // one task on its own works too
        assert!(post(s.port, "/todo", Some("tok"), "{\"text\":\"ask mate\"}")
            .starts_with("HTTP/1.1 200"));
        assert_eq!(s.drain_todos()[0].text.as_deref(), Some("ask mate"));
        // and the list reads back, so an agent can see what's on it
        let resp = get(s.port, "/todo", Some("tok"));
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.contains("\"todos\":["), "{resp}");
        assert!(get(s.port, "/todo", None).starts_with("HTTP/1.1 403"), "reading needs the token");
        // nothing to file is a bad request, not an empty batch
        assert!(post(s.port, "/todo", Some("tok"), "{\"todos\":[]}").starts_with("HTTP/1.1 400"));
        assert!(post(s.port, "/todo", Some("tok"), "{\"text\":\"  \"}").starts_with("HTTP/1.1 400"));
        assert!(s.drain_todos().is_empty());
    }

    /// The same path edits: an `id` says which task, and only the fields the
    /// request carries are touched.
    #[test]
    fn queues_an_edit_and_a_delete() {
        let s = Server::start("tok".into()).unwrap();
        let body = "{\"todos\":[{\"id\":\"58w65\",\"status\":\"done\"},                    {\"id\":\"eg3m7\",\"delete\":true}],\"group\":\"batch group\"}";
        assert!(post(s.port, "/todo", Some("tok"), body).starts_with("HTTP/1.1 200"));
        let q = s.drain_todos();
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].id, "58w65");
        assert_eq!(q[0].status.as_deref(), Some("done"));
        assert_eq!(q[0].text, None, "an edit needn't repeat the text");
        // the batch's group belongs to the batch's own tasks, not to whatever
        // it edits — an edit keeps the group it already has
        assert_eq!(q[0].group, None);
        assert!(q[1].delete);
        // an id alone is enough to be worth queueing, where text alone would be
        // a new task
        assert!(post(s.port, "/todo", Some("tok"), "{\"id\":\"58w65\"}")
            .starts_with("HTTP/1.1 200"));
        assert_eq!(s.drain_todos()[0].id, "58w65");
    }

    /// The subtitle moves on its own: a call may say where the work is without
    /// touching what the work is called.
    #[test]
    fn a_subtitle_rides_the_title_route() {
        let s = Server::start("tok".into()).unwrap();
        assert!(post(s.port, "/title", Some("tok"), "{\"subtitle\":\"UI auto syncing\"}")
            .starts_with("HTTP/1.1 200"));
        let q = s.drain_titles();
        assert_eq!(q.len(), 1);
        assert!(q[0].text.is_empty(), "no name was proposed");
        assert_eq!(q[0].subtitle.as_deref(), Some("UI auto syncing"));
        // both together is fine, and an empty subtitle clears it
        assert!(post(
            s.port,
            "/title",
            Some("tok"),
            "{\"title\":\"feat: ai chat\",\"subtitle\":\"\"}"
        )
        .starts_with("HTTP/1.1 200"));
        let q = s.drain_titles();
        assert_eq!(q[0].text, "feat: ai chat");
        assert_eq!(q[0].subtitle.as_deref(), Some(""), "said, and said empty");
        // a title alone leaves the subtitle alone
        assert!(post(s.port, "/title", Some("tok"), "{\"title\":\"feat: x\"}")
            .starts_with("HTTP/1.1 200"));
        assert_eq!(s.drain_titles()[0].subtitle, None);
        // and a call that says neither is a bad request
        assert!(post(s.port, "/title", Some("tok"), "{}").starts_with("HTTP/1.1 400"));
    }

    /// A summary comes back as markdown, so it keeps its line breaks where the
    /// other one-line fields do not.
    #[test]
    fn a_summary_keeps_its_shape() {
        let s = Server::start("tok".into()).unwrap();
        let body = "{\"text\":\"Done:\\n- merged next\\n- tests pass\"}";
        assert!(post(s.port, "/summary", Some("tok"), body).starts_with("HTTP/1.1 200"));
        let got = s.drain_summaries();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "Done:\n- merged next\n- tests pass");
        assert!(got[0].title.is_empty(), "no title given, so the card names itself");
        // a title rides along when there is one
        let titled = "{\"title\":\"Merged origin/next\",\"text\":\"all green\"}";
        assert!(post(s.port, "/summary", Some("tok"), titled).starts_with("HTTP/1.1 200"));
        assert_eq!(s.drain_summaries()[0].title, "Merged origin/next");
        // nothing to say is a bad request, not an empty card
        assert!(post(s.port, "/summary", Some("tok"), "{\"text\":\"  \"}")
            .starts_with("HTTP/1.1 400"));
        assert!(s.drain_summaries().is_empty());
        assert!(post(s.port, "/summary", None, "{\"text\":\"x\"}").starts_with("HTTP/1.1 403"));
    }

    /// Every element the script reaches for has to be in the markup.
    ///
    /// A browser extension is a cross-origin caller: without the headers the
    /// browser drops the answer, and without an answer to the preflight it
    /// never sends the post at all.
    #[test]
    fn the_hub_answers_a_browser() {
        let hub = RemoteHub::start("tok".into()).unwrap();
        let port = hub.port;
        hub.publish("{\"wss\":[{\"n\":\"one\"}],\"msgs\":[]}".into());
        let pre = {
            let mut c = TcpStream::connect(("127.0.0.1", port)).expect("connect");
            let req = "OPTIONS /say?ws=0 HTTP/1.1\r\nHost: localhost\r\n\
                       Origin: chrome-extension://abc\r\n\
                       Access-Control-Request-Method: POST\r\n\r\n";
            c.write_all(req.as_bytes()).unwrap();
            let mut out = String::new();
            c.read_to_string(&mut out).unwrap();
            out
        };
        assert!(pre.starts_with("HTTP/1.1 200"), "a preflight carries no token: {pre}");
        assert!(pre.contains("Access-Control-Allow-Origin: *"), "{pre}");
        assert!(pre.contains("Access-Control-Allow-Methods: GET, POST, OPTIONS"), "{pre}");
        // and the answers themselves carry it
        let got = get(port, "/thread?ws=0&t=tok", None);
        assert!(got.starts_with("HTTP/1.1 200"), "{got}");
        assert!(got.contains("Access-Control-Allow-Origin: *"), "{got}");
        // the token still decides who gets in
        assert!(get(port, "/thread?ws=0&t=nope", None).starts_with("HTTP/1.1 403"));
    }

    /// The workspace strip shipped without its `<nav id="wss">`: `nav` was null,
    /// the first `nav.innerHTML` threw inside the poll's `try`, and the `catch`
    /// turned a missing element into "not connected". One missing id cost the
    /// whole page, so the ids are checked rather than trusted.
    #[test]
    fn the_page_has_every_element_its_script_looks_up() {
        // both spellings: the raw call and the `$('…')` helper the page uses
        let wanted: Vec<&str> = ["getElementById('", "$('"]
            .into_iter()
            .flat_map(|pat| {
                MOBILE_PAGE.match_indices(pat).map(move |(i, m)| {
                    let rest = &MOBILE_PAGE[i + m.len()..];
                    &rest[..rest.find('\'').expect("closing quote")]
                })
            })
            .collect();
        assert!(wanted.len() >= 5, "expected the script to look up several ids, got {wanted:?}");
        for id in wanted {
            assert!(
                MOBILE_PAGE.contains(&format!("id=\"{id}\"")),
                "the script wants #{id} and the markup has no such element"
            );
        }
    }

    /// The phone's listener is its own thing: one for the window, token in the
    /// query string because a browser opening a link can't set a header.
    #[test]
    fn the_phone_reads_and_writes_by_workspace() {
        let hub = RemoteHub::start("tok".into()).unwrap();
        let port = hub.port;
        hub.publish("{\"wss\":[{\"n\":\"one\"},{\"n\":\"two\"}],\"msgs\":[]}".into());

        // nothing has polled yet, so the UI has nothing to build for
        assert!(!hub.watched());
        let resp = get(port, "/thread?ws=1&t=tok", None);
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.contains("\"n\":\"two\""), "{resp}");
        // the poll says which workspace to serialise next, and that someone is there
        assert_eq!(hub.wanted(), 1);
        assert!(hub.watched());

        // what the phone types carries the workspace it was typed in
        assert!(post(port, "/say?ws=1&t=tok", None, "{\"text\":\"from the sofa\"}")
            .starts_with("HTTP/1.1 200"));
        let said = hub.drain_said();
        assert_eq!(said.len(), 1);
        assert_eq!((said[0].ws, said[0].text.as_str()), (1, "from the sofa"));
        assert!(said[0].imgs.is_empty());

        // a photo rides along with the message, and one on its own is still a
        // message — "look at this" is often the whole of what you want to say
        let body = "{\"text\":\"\",\"imgs\":[{\"mime\":\"image/png\",\"b64\":\"aGk=\"}]}";
        assert!(post(port, "/say?ws=0&t=tok", None, body).starts_with("HTTP/1.1 200"));
        let said = hub.drain_said();
        assert_eq!(said[0].imgs, vec![("image/png".to_string(), "aGk=".to_string())]);

        // nothing at all is still nothing, and a non-image is dropped rather
        // than carried to the adapter to be refused there
        assert!(post(port, "/say?ws=0&t=tok", None, "{\"text\":\"  \"}")
            .starts_with("HTTP/1.1 400"));
        let evil = "{\"text\":\"x\",\"imgs\":[{\"mime\":\"text/html\",\"b64\":\"aGk=\"}]}";
        assert!(post(port, "/say?ws=0&t=tok", None, evil).starts_with("HTTP/1.1 200"));
        assert!(hub.drain_said()[0].imgs.is_empty());

        // and the token is the whole of the door
        assert!(get(port, "/thread?t=nope", None).starts_with("HTTP/1.1 403"));
        assert!(get(port, "/m", None).starts_with("HTTP/1.1 403"));
        assert!(get(port, "/health?t=tok", None).starts_with("HTTP/1.1 404"), "one page, three calls");
    }

    /// The workspace's own description, for a PR body that says where it came

    /// from. It starts empty and the UI keeps it current.
    #[test]
    fn workspace_info_is_served_back() {
        let s = Server::start("tok".into()).unwrap();
        assert!(get(s.port, "/workspace", Some("tok")).ends_with("{}"), "empty until the UI sets it");
        s.set_info("{\"folder\":\"wby-next4\",\"title\":\"Event Van Handler\"}".into());
        let resp = get(s.port, "/workspace", Some("tok"));
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.contains("Event Van Handler"), "{resp}");
        // it says which workspace this is, so it needs the token like the rest
        assert!(get(s.port, "/workspace", None).starts_with("HTTP/1.1 403"));
    }

    /// A batch of PR files to tick off, the way an agent that just reviewed a
    /// hundred identical version bumps would send it.
    #[test]
    fn queues_a_viewed_batch() {
        let s = Server::start("tok".into()).unwrap();
        let body = "{\"paths\":[\"a/package.json\",\" ./b/package.json \"]}";
        assert!(post(s.port, "/viewed", Some("tok"), body).starts_with("HTTP/1.1 200"));
        let batches = s.drain_viewed();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].paths, vec!["a/package.json", "./b/package.json"]);
        assert!(batches[0].viewed, "viewed defaults to true");
        // one path as a bare string, and unticking
        let body = "{\"path\":\"a/package.json\",\"viewed\":false}";
        assert!(post(s.port, "/viewed", Some("tok"), body).starts_with("HTTP/1.1 200"));
        let batches = s.drain_viewed();
        assert_eq!(batches[0].paths, vec!["a/package.json"]);
        assert!(!batches[0].viewed);
        // nothing to mark is a bad request, not an empty batch
        assert!(post(s.port, "/viewed", Some("tok"), "{\"paths\":[]}").starts_with("HTTP/1.1 400"));
        assert!(post(s.port, "/viewed", Some("tok"), "{}").starts_with("HTTP/1.1 400"));
        assert!(s.drain_viewed().is_empty());
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
        // /refresh is a bare signal: it carries no body and latches once
        assert!(!s.take_refresh());
        assert!(post(s.port, "/refresh", Some("tok"), "").starts_with("HTTP/1.1 200"));
        // a title is queued for TIDE to accept or ignore
        assert!(post(s.port, "/title", Some("tok"), "{\"title\":\"  SQL date bug \"}")
            .starts_with("HTTP/1.1 200"));
        let titles = s.drain_titles();
        assert_eq!(titles[0].text, "SQL date bug");
        assert!(!titles[0].force, "a plain post never renames a named workspace");
        // a rename the human asked for says so
        assert!(post(s.port, "/title", Some("tok"), "{\"title\":\"feat: x\",\"force\":true}")
            .starts_with("HTTP/1.1 200"));
        assert!(s.drain_titles()[0].force);
        assert!(post(s.port, "/title", Some("tok"), "{\"title\":\"\"}").starts_with("HTTP/1.1 400"));
        assert!(s.take_refresh());
        assert!(!s.take_refresh(), "the flag is consumed by the first read");
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



