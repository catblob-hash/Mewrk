//! Which computer's network a page uses, and getting it there before anything loads: the sequential
//! parts of upstream's `BrowserPanel.tsx`. They run on a task thread, and take the host as a trait so
//! the orderings, which are the whole point, can be tested against a script.

use serde_json::Value;

use crate::address::normalize_browser_address;
use crate::browser::BrowserStatus;
use crate::preview::*;

/// The host, as far as one page's flows reach it.
pub trait PageOps {
    fn status(&self) -> Result<BrowserStatus, String>;
    fn navigate(&self, url: &str) -> Result<BrowserStatus, String>;
    fn set_network(&self, target: Option<&PreviewTarget>) -> Result<(), String>;
    /// `onOpenPage`: opens this tab's page the way selecting the tab does.
    fn open_page(&self, url: &str) -> Result<(), String>;
    fn action(&self, name: &str, value: Value) -> Result<BrowserStatus, String>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkSide {
    Viewer,
    Web,
}

/// One page's flows.
pub struct PageFlow<'a, O: PageOps> {
    pub ops: &'a O,
    pub session_id: &'a str,
    pub target: Option<&'a PreviewTarget>,
    pub can_open_page: bool,
    /// What the pane last heard, for a page with no network change to ask about.
    pub known_has_page: bool,
    pub known_suspended: bool,
    pub is_viewer: &'a dyn Fn(&str) -> bool,
}

impl<O: PageOps> PageFlow<'_, O> {
    /// Puts the page on the network the document it is about to show needs, and says whether it asked
    /// the host to change it. A viewer is on THIS computer's loopback: a page bound to an SSH machine
    /// would look for it there and hand its token to whatever listens on that port, so a viewer isn't
    /// opened until the page is off that network, and when that can't be done the error is shown instead.
    pub fn bind_page_network(&self, side: NetworkSide, leaving_viewer: bool) -> Result<bool, String> {
        if side == NetworkSide::Viewer {
            self.ops.set_network(None)?;
            note_viewer_network_cleared(self.session_id);
            return Ok(true);
        }
        let Some(target) = self.target else {
            return Ok(false);
        };
        let marked = take_viewer_network_cleared(self.session_id);
        if !marked && !leaving_viewer {
            return Ok(false);
        }
        // A binding that fails leaves the page on this computer, as it does for a new tab.
        let _ = self.ops.set_network(Some(target));
        Ok(true)
    }

    /// Opens the page at `url` when it has none, or when changing its network just took it down. A page
    /// is opened *at the address it is going to*: nothing loads on the wrong network in between.
    fn reopen_page_at(&self, url: &str, network_changed: bool) -> Result<Option<BrowserStatus>, String> {
        if !self.can_open_page {
            return Ok(None);
        }
        let (has_page, suspended) = if network_changed {
            let known = self.ops.status()?;
            (known.has_page, known.suspended)
        } else {
            (self.known_has_page, self.known_suspended)
        };
        let absent = !has_page && !suspended;
        let taken_down = network_changed && suspended;
        if !absent && !taken_down {
            return Ok(None);
        }
        self.ops.open_page(url)?;
        // An open that failed says why only in the status it leaves behind.
        let opened = self.ops.status()?;
        if !opened.has_page || opened.suspended {
            return Err(opened.error.clone().unwrap_or_else(|| mewrk_client::t("无法打开这个标签页的页面", "Could not open this tab's page").to_owned()));
        }
        // A page made anew has no history, whatever the pane remembered of the one it replaced.
        reset_page_history(self.session_id);
        Ok(Some(opened))
    }

    /// `reopen_page_at`, then makes sure the reopened page is on `url`. `None`: the page was there.
    fn show_page_at(&self, url: &str, network_changed: bool) -> Result<Option<BrowserStatus>, String> {
        let Some(opened) = self.reopen_page_at(url, network_changed)? else {
            return Ok(None);
        };
        if preview_addresses_match(&opened.url, url) {
            return Ok(Some(opened));
        }
        self.ops.navigate(url).map(Some)
    }

    /// The address bar and a server row. A viewer's address is a viewer even when it was typed.
    pub fn navigate(&self, raw: &str, viewer: bool, current_url: &str) -> Result<BrowserStatus, String> {
        let url = normalize_browser_address(raw)?;
        let viewer = viewer || (self.is_viewer)(&url);
        let network_changed = self.bind_page_network(if viewer { NetworkSide::Viewer } else { NetworkSide::Web }, page_shows_viewer(self.session_id, current_url, self.is_viewer))?;
        let next = match self.show_page_at(&url, network_changed)? {
            Some(status) => status,
            None => self.ops.navigate(&url)?,
        };
        note_page_navigation(self.session_id, &history_status(&next), viewer);
        Ok(next)
    }

    /// Back or Forward, with the network put right first when the entry it lands on is the other kind of
    /// document. Where the host can't switch a live page's network the page is taken down with its
    /// history, so the entry is opened by its address instead.
    pub fn step_history(&self, back: bool, status: &BrowserStatus) -> Result<BrowserStatus, String> {
        let destination = peek_page_history(self.session_id, back, &history_status(status), self.is_viewer);
        let here = page_shows_viewer(self.session_id, &status.url, self.is_viewer);
        if let Some((url, viewer)) = destination {
            if viewer != here {
                let network_changed = self.bind_page_network(if viewer { NetworkSide::Viewer } else { NetworkSide::Web }, here)?;
                if network_changed {
                    if let Some(shown) = self.show_page_at(&url, true)? {
                        note_page_navigation(self.session_id, &history_status(&shown), viewer);
                        return Ok(shown);
                    }
                }
            }
        }
        self.ops.action(if back { "back" } else { "forward" }, Value::Null)
    }

    /// Reload, on the right network. Only a page something moved without this pane has anything to fix.
    pub fn reload(&self, status: &BrowserStatus) -> Result<BrowserStatus, String> {
        let here = page_shows_viewer(self.session_id, &status.url, self.is_viewer);
        if !here || status.network_machine.is_some() {
            let network_changed = self.bind_page_network(if here { NetworkSide::Viewer } else { NetworkSide::Web }, here)?;
            if network_changed {
                if let Some(shown) = self.show_page_at(&status.url, true)? {
                    return Ok(shown);
                }
            }
        }
        self.ops.action("reload", Value::Null)
    }
}

pub fn history_status(status: &BrowserStatus) -> PageHistoryStatus<'_> {
    PageHistoryStatus { url: &status.url, loading: Some(status.loading), can_go_back: Some(status.can_go_back), can_go_forward: Some(status.can_go_forward) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Script {
        calls: RefCell<Vec<String>>,
        status: RefCell<BrowserStatus>,
        fail_network: bool,
        /// What a network change does to the page: WebView2 takes a live page down; WebKit doesn't.
        network_suspends: bool,
        open_leaves_error: Option<String>,
    }

    impl PageOps for Script {
        fn status(&self) -> Result<BrowserStatus, String> {
            self.calls.borrow_mut().push("status".into());
            Ok(self.status.borrow().clone())
        }
        fn navigate(&self, url: &str) -> Result<BrowserStatus, String> {
            self.calls.borrow_mut().push(format!("navigate {url}"));
            let mut status = self.status.borrow_mut();
            if status.suspended {
                return Err("this browser task is suspended".into());
            }
            status.url = url.to_owned();
            Ok(status.clone())
        }
        fn set_network(&self, target: Option<&PreviewTarget>) -> Result<(), String> {
            self.calls.borrow_mut().push(format!("network {}", target.map_or("none", |target| target.conversation_id.as_str())));
            if self.fail_network {
                return Err("the page could not be taken off the machine".into());
            }
            if self.network_suspends {
                self.status.borrow_mut().suspended = true;
            }
            Ok(())
        }
        fn open_page(&self, url: &str) -> Result<(), String> {
            self.calls.borrow_mut().push(format!("open {url}"));
            let mut status = self.status.borrow_mut();
            if let Some(error) = &self.open_leaves_error {
                status.error = Some(error.clone());
                status.has_page = false;
                return Ok(());
            }
            status.has_page = true;
            status.suspended = false;
            status.url = url.to_owned();
            Ok(())
        }
        fn action(&self, name: &str, _value: Value) -> Result<BrowserStatus, String> {
            self.calls.borrow_mut().push(format!("action {name}"));
            Ok(self.status.borrow().clone())
        }
    }

    const VIEWER: &str = "http://127.0.0.1:9/token/";
    const WEB: &str = "http://localhost:5173/";

    fn is_viewer(url: &str) -> bool {
        url.starts_with(VIEWER)
    }

    fn target() -> PreviewTarget {
        PreviewTarget { conversation_id: "conv".into(), ..Default::default() }
    }

    fn live(url: &str) -> BrowserStatus {
        BrowserStatus { has_page: true, open: true, url: url.to_owned(), ..Default::default() }
    }

    struct Rig {
        script: Script,
        target: PreviewTarget,
        session: String,
    }

    impl Rig {
        fn new(session: &str) -> Self {
            reset_page_history(session);
            let _ = take_viewer_network_cleared(session);
            Self { script: Script::default(), target: target(), session: session.to_owned() }
        }

        fn flow(&self) -> PageFlow<'_, Script> {
            let status = self.script.status.borrow();
            PageFlow { ops: &self.script, session_id: &self.session, target: Some(&self.target), can_open_page: true, known_has_page: status.has_page, known_suspended: status.suspended, is_viewer: &is_viewer }
        }

        fn calls(&self) -> Vec<String> {
            self.script.calls.borrow().clone()
        }
    }

    #[test]
    fn a_viewer_opens_on_this_computers_network_first() {
        let rig = Rig::new("flow-1");
        *rig.script.status.borrow_mut() = live(WEB);
        let status = rig.flow().navigate(VIEWER, true, WEB).unwrap();
        assert_eq!(status.url, VIEWER);
        assert_eq!(rig.calls()[0], "network none");
        assert!(rig.calls().iter().position(|call| call == "network none") < rig.calls().iter().position(|call| call.starts_with("navigate")));
    }

    #[test]
    fn no_viewer_opens_when_the_page_cant_be_taken_off_the_machines_network() {
        let mut rig = Rig::new("flow-2");
        rig.script.fail_network = true;
        *rig.script.status.borrow_mut() = live(WEB);
        let error = rig.flow().navigate(VIEWER, true, WEB).unwrap_err();
        assert!(error.contains("could not be taken off"));
        assert!(!rig.calls().iter().any(|call| call.starts_with("navigate")), "{:?}", rig.calls());
    }

    #[test]
    fn a_tab_that_showed_a_viewer_is_bound_back_before_a_web_server_opens() {
        let rig = Rig::new("flow-3");
        *rig.script.status.borrow_mut() = live(WEB);
        rig.flow().navigate(VIEWER, true, WEB).unwrap();
        rig.script.calls.borrow_mut().clear();
        rig.flow().navigate(WEB, false, VIEWER).unwrap();
        assert_eq!(rig.calls()[0], "network conv");
    }

    #[test]
    fn a_web_servers_network_is_left_alone_when_no_viewer_was_ever_there() {
        let rig = Rig::new("flow-4");
        *rig.script.status.borrow_mut() = live(WEB);
        rig.flow().navigate("http://localhost:3000/", false, WEB).unwrap();
        assert!(!rig.calls().iter().any(|call| call.starts_with("network")), "{:?}", rig.calls());
    }

    #[test]
    fn a_viewers_address_typed_into_the_bar_is_a_viewer() {
        let rig = Rig::new("flow-5");
        *rig.script.status.borrow_mut() = live(WEB);
        rig.flow().navigate(VIEWER, false, WEB).unwrap();
        assert_eq!(rig.calls()[0], "network none");
    }

    #[test]
    fn a_page_the_network_change_took_down_is_reopened_not_navigated() {
        let mut rig = Rig::new("flow-6");
        rig.script.network_suspends = true;
        *rig.script.status.borrow_mut() = live(WEB);
        let status = rig.flow().navigate(VIEWER, true, WEB).unwrap();
        assert_eq!(status.url, VIEWER);
        assert_eq!(rig.calls(), ["network none", "status", &format!("open {VIEWER}"), "status"]);
    }

    #[test]
    fn a_viewers_page_is_rebound_by_reopening_it_at_the_web_servers_address() {
        let mut rig = Rig::new("flow-7");
        *rig.script.status.borrow_mut() = live(WEB);
        rig.flow().navigate(VIEWER, true, WEB).unwrap();
        rig.script.network_suspends = true;
        rig.script.calls.borrow_mut().clear();
        rig.flow().navigate(WEB, false, VIEWER).unwrap();
        assert_eq!(rig.calls(), ["network conv", "status", &format!("open {WEB}"), "status"]);
    }

    #[test]
    fn it_says_why_when_the_page_cant_be_brought_back() {
        let mut rig = Rig::new("flow-8");
        rig.script.network_suspends = true;
        rig.script.open_leaves_error = Some("the proxy is gone".into());
        *rig.script.status.borrow_mut() = live(WEB);
        assert_eq!(rig.flow().navigate(VIEWER, true, WEB).unwrap_err(), "the proxy is gone");
        assert!(!rig.calls().iter().any(|call| call.starts_with("navigate")), "never a suspended page");
    }

    #[test]
    fn a_tab_with_no_page_gets_one_before_navigating() {
        let rig = Rig::new("flow-9");
        let status = rig.flow().navigate("http://localhost:3000/", false, "about:blank").unwrap();
        assert!(status.has_page);
        assert_eq!(rig.calls()[0], "open http://localhost:3000/");
    }

    #[test]
    fn a_tab_that_cant_get_a_page_says_why() {
        let mut rig = Rig::new("flow-10");
        rig.script.open_leaves_error = Some("no window".into());
        assert_eq!(rig.flow().navigate("http://localhost:3000/", false, "about:blank").unwrap_err(), "no window");
    }

    #[test]
    fn back_onto_a_viewer_puts_it_on_this_computer_first() {
        let rig = Rig::new("flow-11");
        let mut web = live(WEB);
        web.can_go_back = true;
        *rig.script.status.borrow_mut() = live(VIEWER);
        note_page_navigation(&rig.session, &history_status(&live(VIEWER)), true);
        note_page_navigation(&rig.session, &history_status(&web), false);
        *rig.script.status.borrow_mut() = web.clone();
        rig.flow().step_history(true, &web).unwrap();
        assert_eq!(rig.calls(), ["network none", "status", "action back"]);
    }

    #[test]
    fn a_history_move_between_documents_of_one_kind_leaves_the_network_alone() {
        let rig = Rig::new("flow-12");
        let mut second = live("http://localhost:5173/b");
        second.can_go_back = true;
        note_page_navigation(&rig.session, &history_status(&live("http://localhost:5173/a")), false);
        note_page_navigation(&rig.session, &history_status(&second), false);
        *rig.script.status.borrow_mut() = second.clone();
        rig.flow().step_history(true, &second).unwrap();
        assert_eq!(rig.calls(), ["action back"]);
    }

    #[test]
    fn where_a_live_pages_network_cant_switch_the_entry_opens_by_its_address() {
        let mut rig = Rig::new("flow-13");
        rig.script.network_suspends = true;
        let mut web = live(WEB);
        web.can_go_back = true;
        note_page_navigation(&rig.session, &history_status(&live(VIEWER)), true);
        note_page_navigation(&rig.session, &history_status(&web), false);
        *rig.script.status.borrow_mut() = web.clone();
        let shown = rig.flow().step_history(true, &web).unwrap();
        assert_eq!(shown.url, VIEWER);
        assert!(!rig.calls().iter().any(|call| call == "action back"), "{:?}", rig.calls());
    }

    #[test]
    fn a_viewer_left_on_a_machines_network_reloads_on_this_computers() {
        let mut rig = Rig::new("flow-14");
        rig.script.network_suspends = true;
        let mut viewer = live(VIEWER);
        viewer.network_machine = Some("ssh:devbox".into());
        *rig.script.status.borrow_mut() = viewer.clone();
        let shown = rig.flow().reload(&viewer).unwrap();
        assert_eq!(rig.calls()[0], "network none");
        assert_eq!(shown.url, VIEWER);
    }

    #[test]
    fn a_web_page_reloads_as_it_always_did() {
        let rig = Rig::new("flow-15");
        let web = live(WEB);
        *rig.script.status.borrow_mut() = web.clone();
        rig.flow().reload(&web).unwrap();
        assert_eq!(rig.calls(), ["action reload"]);
    }
}
