//! The preview pane: the system's WKWebView in a native child view, kept over the pane's body rectangle.
//! No browser engine is linked in or bundled, and nothing here is drawn by the app except the address bar.

use std::ffi::c_void;

use fluent::theme::*;
use fluent::ui::{rect, CursorKind, Icon, Rect, Ui};
use fluent::TextEdit;
use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::NSView;
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, NSURLRequest, NSURL};
use objc2_web_kit::{WKWebView, WKWebViewConfiguration};

const BAR_H: f32 = 34.0;

pub struct Preview {
    web: Option<Retained<WKWebView>>,
    address: TextEdit,
    last_frame: [f64; 4],
    hidden: bool,
    /// The app's frame number when this was last drawn. A preview nobody drew has to leave the screen by itself.
    pub shown_frame: u64,
    loaded: bool,
}

/// What the user typed, as a URL. A bare host is a local dev server when it looks like one.
pub fn normalize_url(input: &str) -> Option<String> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    if s.contains("://") {
        return Some(s.to_owned());
    }
    let local = s.starts_with("localhost") || s.starts_with("127.") || s.starts_with("0.0.0.0") || s.starts_with('[') || s.parse::<u16>().is_ok();
    if s.parse::<u16>().is_ok() {
        return Some(format!("http://localhost:{s}"));
    }
    Some(format!("{}://{s}", if local { "http" } else { "https" }))
}

impl Preview {
    pub fn new() -> Self {
        Self { web: None, address: TextEdit::new(), last_frame: [0.0; 4], hidden: true, shown_frame: 0, loaded: false }
    }

    fn ensure_web(&mut self, parent: *mut c_void) -> Option<&Retained<WKWebView>> {
        if self.web.is_none() {
            let mtm = MainThreadMarker::new()?;
            let parent = parent as *mut NSView;
            if parent.is_null() {
                return None;
            }
            // SAFETY: `parent` is the live content view of the winit window, and we are on the main thread that owns it.
            // The configuration and the web view are created and used on this thread only.
            unsafe {
                let config = WKWebViewConfiguration::new(mtm);
                let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(10.0, 10.0));
                let web = WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), frame, &config);
                web.setHidden(true);
                (*parent).addSubview(&web);
                self.web = Some(web);
            }
        }
        self.web.as_ref()
    }

    fn navigate(&mut self, url: &str) {
        let Some(web) = &self.web else { return };
        // SAFETY: the web view lives on this thread, and these objects are created for this one call.
        unsafe {
            let Some(u) = NSURL::URLWithString(&NSString::from_str(url)) else { return };
            let request = NSURLRequest::requestWithURL(&u);
            let _ = web.loadRequest(&request);
        }
        self.loaded = true;
    }

    pub fn set_hidden(&mut self, hidden: bool) {
        if hidden != self.hidden {
            self.hidden = hidden;
            if let Some(web) = &self.web {
                web.setHidden(hidden);
            }
        }
    }

    /// Draws the address bar and keeps the native view over the rest of `body`. `covered` means a flyout or a dialog
    /// is open: a native view always sits above everything the GPU draws, so it has to get out of the way.
    pub fn show(&mut self, ui: &mut Ui<'_>, id: u64, body: Rect, parent: *mut c_void, covered: bool, frame_no: u64) {
        let t = ui.theme;
        self.shown_frame = frame_no;
        if !parent.is_null() {
            self.ensure_web(parent);
        }
        let bar = rect(body.x, body.y, body.w, BAR_H);
        ui.fill(rect(bar.x, bar.bottom() - 1.0, bar.w, 1.0), 0.0, t.divider);
        let (can_back, can_forward, loading, url) = match &self.web {
            // SAFETY: plain property reads on an object owned by this thread.
            Some(web) => unsafe { (web.canGoBack(), web.canGoForward(), web.isLoading(), web.URL().and_then(|u| u.absoluteString()).map(|s| s.to_string())) },
            None => (false, false, false, None),
        };
        // While a page loads, its address changes with no event reaching the app.
        if loading {
            ui.wake_after(0.25);
        }
        let mut x = bar.x + 6.0;
        let cy = bar.y + 5.0;
        if nav_button(ui, id + 1, x, cy, Icon::ArrowLeft, can_back)
            && let Some(web) = &self.web
        {
            // SAFETY: as above.
            let _ = unsafe { web.goBack() };
        }
        x += 26.0;
        if nav_button(ui, id + 2, x, cy, Icon::ArrowRight, can_forward)
            && let Some(web) = &self.web
        {
            // SAFETY: as above.
            let _ = unsafe { web.goForward() };
        }
        x += 26.0;
        if nav_button(ui, id + 3, x, cy, Icon::Refresh, self.loaded)
            && let Some(web) = &self.web
        {
            // SAFETY: as above.
            let _ = unsafe { web.reload() };
        }
        x += 30.0;
        let field = rect(x, bar.y + 4.0, (bar.right() - x - 8.0).max(40.0), BAR_H - 8.0);
        let focused = ui.mem.focus == id + 4;
        if !focused
            && let Some(u) = &url
            && *u != self.address.text
        {
            self.address.set_text(u.clone());
        }
        ui.card(field, CONTROL_RADIUS, t.control_fill, t.control_stroke);
        let text_area = rect(field.x + 9.0, field.y + 4.0, field.w - 18.0, field.h - 6.0);
        let resp = self.address.show_bare(ui, id + 4, text_area, "输入地址，如 localhost:5173", &CAPTION, true);
        if resp.submitted {
            if let Some(u) = normalize_url(&self.address.text) {
                self.navigate(&u);
            }
            ui.mem.focus = 0;
        }

        let page = rect(body.x, bar.bottom(), body.w, (body.h - BAR_H).max(0.0));
        if !self.loaded || self.web.is_none() {
            let message = if self.web.is_none() { "预览只在窗口里可用" } else { "输入地址打开页面" };
            ui.label_centered(page, message, &CAPTION, t.text_tertiary);
        }
        if let Some(web) = self.web.clone() {
            let parent_view = parent as *mut NSView;
            // SAFETY: the parent is the live content view, on its own thread.
            let (parent_h, flipped) = unsafe { ((*parent_view).frame().size.height, (*parent_view).isFlipped()) };
            let (fx, fw, fh) = (page.x as f64, page.w as f64, page.h as f64);
            let fy = if flipped { page.y as f64 } else { parent_h - page.y as f64 - fh };
            let frame = [fx, fy, fw, fh];
            if frame != self.last_frame {
                self.last_frame = frame;
                web.setFrame(NSRect::new(NSPoint::new(fx, fy), NSSize::new(fw, fh)));
            }
            self.set_hidden(!self.loaded || covered);
        }
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        if let Some(web) = &self.web {
            web.removeFromSuperview();
        }
    }
}

fn nav_button(ui: &mut Ui<'_>, id: u64, x: f32, y: f32, icon: Icon, enabled: bool) -> bool {
    let t = ui.theme;
    let r = rect(x, y, 24.0, 24.0);
    let resp = ui.interact(id, r);
    if enabled && resp.hover_t > 0.0 {
        ui.fill(r, CONTROL_RADIUS, t.subtle_hover.fade(resp.hover_t));
    }
    ui.icon(icon, x + 12.0, y + 12.0, 12.0, if enabled { t.text_secondary } else { t.text_disabled });
    if enabled && resp.hovered {
        ui.cursor = CursorKind::Hand;
    }
    enabled && resp.clicked
}

/// The Edit menu. A native web view takes Cmd+C and friends through the responder chain, which starts at the menu bar.
pub fn install_edit_menu() {
    use objc2::sel;
    use objc2_app_kit::{NSApplication, NSMenu, NSMenuItem};
    let Some(mtm) = MainThreadMarker::new() else { return };
    let app = NSApplication::sharedApplication(mtm);
    let Some(main) = app.mainMenu() else { return };
    // SAFETY: AppKit objects on the main thread, and the selectors are standard responder actions.
    unsafe {
        let edit = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("编辑"));
        for (title, action, key) in [
            ("撤销", sel!(undo:), "z"),
            ("重做", sel!(redo:), "Z"),
            ("剪切", sel!(cut:), "x"),
            ("拷贝", sel!(copy:), "c"),
            ("粘贴", sel!(paste:), "v"),
            ("全选", sel!(selectAll:), "a"),
        ] {
            let item = NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &NSString::from_str(title), Some(action), &NSString::from_str(key));
            edit.addItem(&item);
        }
        let holder = NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &NSString::from_str("编辑"), None, &NSString::from_str(""));
        holder.setSubmenu(Some(&edit));
        main.addItem(&holder);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_become_urls() {
        assert_eq!(normalize_url("localhost:5173").unwrap(), "http://localhost:5173");
        assert_eq!(normalize_url("5173").unwrap(), "http://localhost:5173");
        assert_eq!(normalize_url("127.0.0.1:80/x").unwrap(), "http://127.0.0.1:80/x");
        assert_eq!(normalize_url("example.com").unwrap(), "https://example.com");
        assert_eq!(normalize_url("https://a.b").unwrap(), "https://a.b");
        assert_eq!(normalize_url("  "), None);
    }
}
