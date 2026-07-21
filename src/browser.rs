//! In-pane web browser: an embedded WKWebView (via `wry`) parented onto the
//! gpui window as a child view and positioned over the browser dock region.
//!
//! gpui paints normally underneath; the WebView floats on top of the rectangle
//! we give it. Because it's a native subview (not a gpui element), we can't lay
//! it out with flexbox — the workspace computes the dock rect each frame and
//! calls `set_bounds`, and hides the view (`set_visible(false)`) when the
//! browser dock is closed.

use gpui::Window;
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{Rect, WebViewBuilder};

pub struct Browser {
    webview: wry::WebView,
}

impl Browser {
    /// Create the WebView as a child of `window`, showing `url`, positioned at
    /// the given logical rect (top-left origin, y down — matching gpui coords).
    pub fn new(window: &Window, url: &str, x: f32, y: f32, w: f32, h: f32) -> Option<Browser> {
        let webview = WebViewBuilder::new()
            .with_url(url)
            .with_devtools(true) // enable Web Inspector (⌥⌘I / right-click → Inspect)
            .with_bounds(rect(x, y, w, h))
            .build_as_child(window)
            .ok()?;
        Some(Browser { webview })
    }

    /// Reposition/resize the view to match the dock region.
    pub fn set_bounds(&self, x: f32, y: f32, w: f32, h: f32) {
        let _ = self.webview.set_bounds(rect(x, y, w, h));
    }

    pub fn set_visible(&self, visible: bool) {
        let _ = self.webview.set_visible(visible);
    }

    pub fn navigate(&self, url: &str) {
        let _ = self.webview.load_url(url);
    }

    pub fn back(&self) {
        let _ = self.webview.evaluate_script("history.back()");
    }

    pub fn forward(&self) {
        let _ = self.webview.evaluate_script("history.forward()");
    }

    pub fn reload(&self) {
        let _ = self.webview.evaluate_script("location.reload()");
    }
}

fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
    Rect {
        position: LogicalPosition::new(x as f64, y as f64).into(),
        size: LogicalSize::new(w.max(0.) as f64, h.max(0.) as f64).into(),
    }
}
