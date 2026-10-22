//! A live view of the Chromium that `agent-browser` drives.
//!
//! **Parked.** It works, and it isn't worth it: see `BROWSER_PANEL` in
//! `main.rs` to switch it back on. gpui packs every picture into a 1024x1024
//! sprite atlas shared with the glyphs, and destroys an atlas texture the
//! moment its last tile is freed — so a full-viewport frame, which is the only
//! thing that fits in an atlas of its own, means creating and destroying a
//! Metal texture and pushing 3.7MB through it for every frame. Scaling frames
//! under 1000px puts them back in the shared atlas and fixes the cost, at the
//! price of a soft picture on a Retina display. Slicing a frame into strips
//! would buy the sharpness back, but two frames' worth of strips overflow one
//! atlas, so it would churn between atlas textures instead — the same problem
//! wearing a hat. A frame stream is not what a glyph atlas is for.
//!
//! What made this interesting was sharing the agent's browser session. The
//! real Chromium window does that too, sharper, at 60fps, with devtools, and
//! `/shot` already brings pictures from it into the transcript.
//!
//! Not an embedded engine. `agent-browser` runs a websocket per session that
//! pushes viewport frames and accepts input events, so TIDE connects to that
//! and paints the frames like any other picture. The point isn't the pixels:
//! it's the same browser the agent is driving, so the page you're looking at is
//! the page it just clicked through, already signed in, and a click here lands
//! in the session it will screenshot next.
//!
//! Frames arrive as base64 JPEG and are decoded on this module's own thread
//! into the BGRA buffer gpui uploads, so the UI thread only ever paints. The
//! connection asks for ack pacing: one frame in flight at a time, acknowledged
//! when it has been decoded, so a slow frame is replaced rather than queued and
//! we never draw ten seconds of history after a stall.

use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::RenderImage;

/// One decoded frame, with what it was a picture *of*.
#[derive(Clone)]
pub struct Frame {
    pub image: Arc<RenderImage>,
    /// The page's own viewport, in CSS pixels. The frame's pixels may be capped
    /// or scaled away from this, so a click maps through the painted rect to
    /// here — not to the image's size.
    pub page_w: f32,
    pub page_h: f32,
}

#[derive(Default)]
struct State {
    /// Where the mirrored tab is, as the stream reports it.
    url: String,
    connected: bool,
    /// Set when the reader thread gives up, so the UI can say so.
    error: Option<String>,
}

/// A connection to one session's stream.
pub struct Mirror {
    frame: Arc<Mutex<Option<Frame>>>,
    state: Arc<Mutex<State>>,
    /// Outgoing JSON, drained by the reader thread between reads.
    tx: Sender<String>,
    /// A new frame has landed since the UI last looked.
    dirty: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Drop for Mirror {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Mirror {
    /// Find the session's stream and start reading it. `None` when
    /// `agent-browser` isn't installed or won't say which port it's on.
    pub fn start() -> Option<Mirror> {
        let port = stream_port()?;
        let frame: Arc<Mutex<Option<Frame>>> = Arc::new(Mutex::new(None));
        let state = Arc::new(Mutex::new(State::default()));
        let dirty = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel::<String>();
        {
            let (frame, state, dirty, stop) =
                (frame.clone(), state.clone(), dirty.clone(), stop.clone());
            let tx_ack = tx.clone();
            std::thread::spawn(move || {
                let err = run(port, &frame, &state, &dirty, &stop, rx, tx_ack);
                if let (Some(e), Ok(mut st)) = (err, state.lock()) {
                    st.connected = false;
                    st.error = Some(e);
                }
            });
        }
        Some(Mirror { frame, state, tx, dirty, stop })
    }

    /// The newest frame, or `None` before the first one arrives.
    pub fn frame(&self) -> Option<Frame> {
        self.frame.lock().ok().and_then(|f| f.clone())
    }

    /// Has a frame landed since the last call? Clears the flag.
    pub fn dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::Relaxed)
    }

    pub fn url(&self) -> String {
        self.state.lock().map(|s| s.url.clone()).unwrap_or_default()
    }

    pub fn connected(&self) -> bool {
        self.state.lock().map(|s| s.connected).unwrap_or(false)
    }

    pub fn error(&self) -> Option<String> {
        self.state.lock().ok().and_then(|s| s.error.clone())
    }

    /// `mousePressed`, `mouseReleased` or `mouseMoved`, in page coordinates.
    pub fn mouse(&self, event: &str, x: f32, y: f32, clicks: u32) {
        let _ = self.tx.send(format!(
            "{{\"type\":\"input_mouse\",\"eventType\":\"{event}\",\"x\":{:.0},\"y\":{:.0},\
             \"button\":\"left\",\"clickCount\":{clicks}}}",
            x.max(0.0),
            y.max(0.0)
        ));
    }

    /// A scroll at the pointer, in page coordinates.
    pub fn wheel(&self, x: f32, y: f32, dx: f32, dy: f32) {
        let _ = self.tx.send(format!(
            "{{\"type\":\"input_mouse\",\"eventType\":\"mouseWheel\",\"x\":{:.0},\"y\":{:.0},\
             \"button\":\"none\",\"deltaX\":{dx:.0},\"deltaY\":{dy:.0}}}",
            x.max(0.0),
            y.max(0.0)
        ));
    }

    /// Cap the frame rate for this client. Every frame costs a full window
    /// redraw, so this is the difference between a live page and a page that
    /// makes the editor stutter. Takes effect immediately, loosening included.
    pub fn set_fps(&self, fps: u32) {
        let _ = self.tx.send(format!("{{\"type\":\"config\",\"maxFps\":{fps}}}"));
    }

    /// One keystroke, down then up. `text` is what the key produces, empty for
    /// a key that types nothing (an arrow, escape).
    pub fn key(&self, key: &str, text: &str) {
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let (k, t) = (esc(key), esc(text));
        let text_field = if t.is_empty() { String::new() } else { format!(",\"text\":\"{t}\"") };
        for phase in ["keyDown", "keyUp"] {
            let _ = self.tx.send(format!(
                "{{\"type\":\"input_keyboard\",\"eventType\":\"{phase}\",\"key\":\"{k}\"{}}}",
                if phase == "keyDown" { text_field.clone() } else { String::new() }
            ));
        }
    }
}

/// The port `agent-browser` streams this session on. Asks, rather than
/// assuming: the default is OS-assigned and changes per daemon.
fn stream_port() -> Option<u16> {
    let ask = || {
        std::process::Command::new("agent-browser")
            .args(["stream", "status", "--json"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
            .and_then(|v| port_of(&v))
    };
    if let Some(p) = ask() {
        return Some(p);
    }
    // no server yet: starting one is what `enable` is for
    let out = std::process::Command::new("agent-browser")
        .args(["stream", "enable", "--json"])
        .output()
        .ok()?;
    serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .ok()
        .and_then(|v| port_of(&v))
        .or_else(ask)
}

/// The port out of `stream status --json`. It answers
/// `{"success":true,"data":{"port":55959,…}}`, but the envelope is the CLI's,
/// not the stream's, so a bare object is read too.
fn port_of(v: &serde_json::Value) -> Option<u16> {
    let here = |o: &serde_json::Value| {
        o.get("port").or_else(|| o.get("streamPort")).and_then(|p| p.as_u64())
    };
    let n = here(v)
        .or_else(|| v.get("data").and_then(here))
        .or_else(|| v.get("stream").and_then(here))?;
    (n > 0 && n <= u16::MAX as u64).then(|| n as u16)
}

/// Own the socket: read frames, write whatever the UI queued. Returns the
/// reason it stopped, or `None` when it was asked to.
fn run(
    port: u16,
    frame: &Arc<Mutex<Option<Frame>>>,
    state: &Arc<Mutex<State>>,
    dirty: &Arc<AtomicBool>,
    stop: &Arc<AtomicBool>,
    rx: Receiver<String>,
    tx: Sender<String>,
) -> Option<String> {
    // ack pacing and the frame cap have to be on the URL: a config message
    // sent after connecting can't cover the opening frame
    // 10 is plenty for watching a page: each frame is a full window redraw at
    // the other end. Ack pacing on top means a frame is only sent once the last
    // one was drawn, so a busy page can't outrun us.
    let url = format!("ws://127.0.0.1:{port}/?pacing=ack&maxFps=10");
    let sock = TcpStream::connect(("127.0.0.1", port))
        .map_err(|e| format!("can't reach agent-browser's stream: {e}"))
        .ok()?;
    let (mut ws, _) = tungstenite::client(&url, sock)
        .map_err(|e| format!("stream refused the connection: {e}"))
        .ok()?;
    // after the handshake, so a slow first frame can't time the handshake out.
    // The timeout is what lets one thread both read and write: a read that
    // finds nothing returns, the queued input goes out, and round it goes.
    let _ = ws.get_mut().set_read_timeout(Some(Duration::from_millis(20)));
    if let Ok(mut st) = state.lock() {
        st.connected = true;
        st.error = None;
    }
    loop {
        if stop.load(Ordering::Relaxed) {
            let _ = ws.close(None);
            return None;
        }
        while let Ok(msg) = rx.try_recv() {
            if ws.send(tungstenite::Message::Text(msg)).is_err() {
                return Some("stream closed".into());
            }
        }
        match ws.read() {
            Ok(tungstenite::Message::Text(t)) => {
                if let Some(ack) = handle(&t, frame, state, dirty) {
                    let _ = tx.send(ack); // goes out on the next turn of the loop
                }
            }
            Ok(tungstenite::Message::Close(_)) => return Some("browser closed the stream".into()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Some(format!("stream ended: {e}")),
        }
    }
}

/// Fold one server message in. Returns an ack to send when it was a frame.
fn handle(
    text: &str,
    frame: &Arc<Mutex<Option<Frame>>>,
    state: &Arc<Mutex<State>>,
    dirty: &Arc<AtomicBool>,
) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    match v.get("type").and_then(|t| t.as_str())? {
        "frame" => {
            let seq = v.get("seq").and_then(|s| s.as_u64()).unwrap_or(0);
            let data = v.get("data").and_then(|d| d.as_str())?;
            let md = v.get("metadata");
            let num = |k: &str| md.and_then(|m| m.get(k)).and_then(|n| n.as_f64()).unwrap_or(0.0);
            if let Some(image) = decode(&b64_decode(data)) {
                let (page_w, page_h) = (num("deviceWidth") as f32, num("deviceHeight") as f32);
                if let Ok(mut f) = frame.lock() {
                    *f = Some(Frame { image, page_w, page_h });
                }
                dirty.store(true, Ordering::Relaxed);
            }
            // acked either way: a frame we couldn't decode shouldn't stall the
            // stream behind it
            Some(format!("{{\"type\":\"ack\",\"seq\":{seq}}}"))
        }
        // sent on connect and on change: whether the browser is actually there
        "status" => {
            if let Ok(mut st) = state.lock() {
                st.connected = v.get("connected").and_then(|c| c.as_bool()).unwrap_or(false);
            }
            None
        }
        "url" => {
            if let (Some(u), Ok(mut st)) = (v.get("url").and_then(|u| u.as_str()), state.lock()) {
                st.url = u.to_string();
            }
            None
        }
        _ => None,
    }
}

/// The biggest frame worth handing to gpui. Its sprite atlas textures are
/// 1024x1024, and a picture that doesn't fit one makes it create a whole new GPU
/// texture — which, for a stream, is a new texture every frame. Staying under
/// that is the difference between a smooth mirror and a stuttering editor. The
/// margin leaves room for a viewport that isn't a round number.
const MAX_FRAME: u32 = 1000;

/// The size to draw a `w`x`h` frame at, or `None` when it already fits.
fn fit(w: u32, h: u32) -> Option<(u32, u32)> {
    if w == 0 || h == 0 || (w <= MAX_FRAME && h <= MAX_FRAME) {
        return None;
    }
    let s = (MAX_FRAME as f32 / w as f32).min(MAX_FRAME as f32 / h as f32);
    Some((((w as f32 * s) as u32).max(1), ((h as f32 * s) as u32).max(1)))
}

/// JPEG bytes to the BGRA buffer gpui uploads, at a size its atlas can hold.
fn decode(bytes: &[u8]) -> Option<Arc<RenderImage>> {
    let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg).ok()?.into_rgba8();
    // resize first, so the channel swap walks the smaller buffer
    let mut data = match fit(img.width(), img.height()) {
        Some((w, h)) => image::imageops::resize(&img, w, h, image::imageops::FilterType::Triangle),
        None => img,
    };
    for px in data.chunks_exact_mut(4) {
        px.swap(0, 2); // RGBA → BGRA
    }
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(data)])))
}

/// Standard base64 decode. Anything that isn't base64 is skipped rather than
/// poisoning the frame, since the only caller is a picture we can redraw.
pub fn b64_decode(s: &str) -> Vec<u8> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a') as u32 + 26,
            b'0'..=b'9' => (c - b'0') as u32 + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    };
    let (mut acc, mut bits) = (0u32, 0u32);
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for c in s.bytes() {
        let Some(v) = val(c) else { continue };
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_through_the_reference_encoding() {
        // the encoder next door is the one the agent channel uses
        for case in ["", "f", "fo", "foo", "foob", "fooba", "foobar", "\u{0}\u{1}\u{ff}"] {
            let enc = crate::acp::base64(case.as_bytes());
            assert_eq!(b64_decode(&enc), case.as_bytes(), "{case:?}");
        }
    }

    #[test]
    fn padding_and_newlines_are_skipped() {
        assert_eq!(b64_decode("Zm9v"), b"foo");
        assert_eq!(b64_decode("Zm9v\n"), b"foo");
        assert_eq!(b64_decode("Zg=="), b"f");
    }

    #[test]
    fn the_port_is_read_out_of_the_clis_envelope() {
        // what `agent-browser stream status --json` actually prints
        let real: serde_json::Value = serde_json::from_str(
            r#"{"success":true,"data":{"connected":true,"enabled":true,"port":55959,
                "screencasting":false},"error":null}"#,
        )
        .unwrap();
        assert_eq!(port_of(&real), Some(55959));
        // and a bare object, in case the envelope ever goes away
        let bare: serde_json::Value = serde_json::from_str(r#"{"enabled":true,"port":41000}"#).unwrap();
        assert_eq!(port_of(&bare), Some(41000));
        let none: serde_json::Value =
            serde_json::from_str(r#"{"success":true,"data":{"enabled":false,"port":0}}"#).unwrap();
        assert_eq!(port_of(&none), None);
    }

    /// A frame message carries the picture and the viewport it was a picture
    /// of; the ack has to name the same `seq` the server sent.
    #[test]
    fn a_frame_message_is_acked_by_sequence() {
        let frame = Arc::new(Mutex::new(None));
        let state = Arc::new(Mutex::new(State::default()));
        let dirty = Arc::new(AtomicBool::new(false));
        // not a JPEG, so it decodes to nothing — the ack still has to go out,
        // or the stream stalls behind a frame we couldn't draw
        let msg = r#"{"type":"frame","seq":41,"data":"AAAA","metadata":{"deviceWidth":1280}}"#;
        assert_eq!(
            handle(msg, &frame, &state, &dirty).as_deref(),
            Some(r#"{"type":"ack","seq":41}"#)
        );
        assert!(frame.lock().unwrap().is_none());
        assert!(!dirty.load(Ordering::Relaxed), "nothing to draw, nothing to redraw for");
    }

    /// A frame has to fit gpui's 1024x1024 atlas or every one of them costs a
    /// new GPU texture.
    #[test]
    fn an_oversized_frame_is_brought_under_the_atlas() {
        assert_eq!(fit(1280, 720), Some((1000, 562)), "scaled by width, aspect kept");
        assert_eq!(fit(800, 1400), Some((571, 1000)), "and by height when that's the long side");
        assert_eq!(fit(900, 600), None, "a frame that already fits is left alone");
        // a sprite exactly filling the atlas would leave no room for the text
        // and icons that share it, so the limit sits under 1024, not at it
        assert_eq!(fit(1024, 768), Some((1000, 750)));
        assert_eq!(fit(1000, 1000), None, "and so is one exactly at the limit");
        assert_eq!(fit(0, 0), None, "nothing to scale");
        // the aspect ratio survives the trip, which is what keeps a click in
        // the right place
        let (w, h) = fit(1280, 720).unwrap();
        assert!(((w as f32 / h as f32) - (1280. / 720.)).abs() < 0.01);
    }

    #[test]
    fn a_status_message_says_whether_the_browser_is_there() {
        let (frame, dirty) = (Arc::new(Mutex::new(None)), Arc::new(AtomicBool::new(false)));
        let state = Arc::new(Mutex::new(State::default()));
        let up = r#"{"type":"status","connected":true,"engine":"chrome","viewportWidth":1280}"#;
        assert_eq!(handle(up, &frame, &state, &dirty), None);
        assert!(state.lock().unwrap().connected);
        let down = r#"{"type":"status","connected":false}"#;
        handle(down, &frame, &state, &dirty);
        assert!(!state.lock().unwrap().connected, "the browser went away");
    }

    #[test]
    fn a_url_message_moves_the_address_bar() {
        let (frame, dirty) = (Arc::new(Mutex::new(None)), Arc::new(AtomicBool::new(false)));
        let state = Arc::new(Mutex::new(State::default()));
        let msg = r#"{"type":"url","url":"https://wby9.localhost/admin"}"#;
        assert_eq!(handle(msg, &frame, &state, &dirty), None, "nothing to ack");
        assert_eq!(state.lock().unwrap().url, "https://wby9.localhost/admin");
        // anything else on the wire is ignored rather than guessed at
        assert_eq!(handle(r#"{"type":"console"}"#, &frame, &state, &dirty), None);
        assert_eq!(handle("not json", &frame, &state, &dirty), None);
    }
}

#[cfg(test)]
mod bench {
    /// Not an assertion about speed, a guard against the decode path quietly
    /// becoming the expensive part again. Run with `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn time_the_decode() {
        let path = std::env::var("FRAME").unwrap_or_else(|_| "frame.jpg".into());
        let bytes = std::fs::read(&path).expect("set FRAME to a jpeg");
        let t = std::time::Instant::now();
        const N: u32 = 40;
        for _ in 0..N {
            super::decode(&bytes).expect("decodes");
        }
        println!("decode+resize: {:?} per frame", t.elapsed() / N);
    }
}
