//! The host's browser commands, typed: upstream's `lib/browser.ts`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::preview::PreviewTarget;

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BrowserViewport {
    pub width: f64,
    pub height: f64,
}

/// Logical bounds of the page relative to the window's content area. `occludedTop` reserves chrome
/// inside that rectangle, so the page's viewport is `height - occludedTop`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserPanelBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub visible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occluded_top: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bottom_corner_radius: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserControlStatus {
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub handoff_requested: bool,
    #[serde(default)]
    pub requested_tool: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserElementPicker {
    #[serde(default)]
    pub armed: bool,
    #[serde(default)]
    pub pending_pick: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserPageDialog {
    pub id: u64,
    pub kind: String,
    pub message: String,
    #[serde(default)]
    pub default_value: Option<String>,
}

/// State returned by every browser command.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BrowserStatus {
    pub has_page: bool,
    pub suspended: bool,
    pub open: bool,
    pub loading: bool,
    pub url: String,
    pub title: Option<String>,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub zoom: f64,
    pub viewport: BrowserViewport,
    pub error: Option<String>,
    pub control: Option<BrowserControlStatus>,
    pub element_picker: Option<BrowserElementPicker>,
    pub occluded: bool,
    pub projected: bool,
    pub network_machine: Option<String>,
    pub dialog: Option<BrowserPageDialog>,
}

impl Default for BrowserStatus {
    /// Upstream's `emptyStatus`.
    fn default() -> Self {
        Self {
            has_page: false,
            suspended: false,
            open: false,
            loading: false,
            url: "about:blank".to_owned(),
            title: None,
            can_go_back: false,
            can_go_forward: false,
            zoom: 1.0,
            viewport: BrowserViewport { width: 560.0, height: 720.0 },
            error: None,
            control: Some(BrowserControlStatus { owner: "available".to_owned(), handoff_requested: false, requested_tool: None }),
            element_picker: None,
            occluded: false,
            projected: false,
            network_machine: None,
            dialog: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SelectedElementBox {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// One element the user picked out of the page, as the host assembled it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectedElement {
    pub sequence: i64,
    pub tag_name: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub classes: Vec<String>,
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
    #[serde(default)]
    pub computed_styles: BTreeMap<String, String>,
    #[serde(default)]
    pub bounding_box: SelectedElementBox,
    #[serde(default)]
    pub screenshot_base64: String,
    /// The composer attachment the crop became. Assigned by the client, never read from the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot_image_id: Option<String>,
    /// The block this element was sent as, for a pick restored from a sent message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_block: Option<String>,
    #[serde(default)]
    pub inner_text: Option<String>,
    #[serde(default)]
    pub parent_path: Option<String>,
    #[serde(default)]
    pub react_component: Option<String>,
    #[serde(default)]
    pub react_props: Option<Value>,
    #[serde(default)]
    pub source_file: Option<String>,
    #[serde(default)]
    pub outer_html: Option<String>,
    #[serde(default)]
    pub sibling_html: Option<String>,
}

/// A page capture by value, for a layer that has to draw on top of it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BrowserPageCapture {
    /// Base64 PNG, no data-URL prefix.
    pub data: String,
    pub width: u32,
    pub height: u32,
}

/// Structured native close outcome. Once `intentAccepted` is true, Closed stays authoritative even
/// when native resource cleanup must be retried.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserCloseDisposition {
    pub status: String,
    pub intent_accepted: bool,
    pub cleanup_complete: bool,
    pub surface_hidden: bool,
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

// The arguments of each command, in one place, exactly as the TypeScript sends them. The host's native
// dispatcher injects the renderer-mount authority itself.

pub fn open_args(session_id: &str, url: Option<&str>, lifecycle_epoch: u64) -> Value {
    json!({ "sessionId": session_id, "url": url.map(str::trim).filter(|url| !url.is_empty()), "lifecycleEpoch": lifecycle_epoch })
}

pub fn close_args(session_id: &str, lifecycle_epoch: u64) -> Value {
    json!({ "sessionId": session_id, "lifecycleEpoch": lifecycle_epoch })
}

pub fn session_args(session_id: &str) -> Value {
    json!({ "sessionId": session_id })
}

pub fn bounds_args(session_id: &str, bounds: &BrowserPanelBounds, lifecycle_epoch: u64) -> Value {
    json!({ "sessionId": session_id, "bounds": bounds, "lifecycleEpoch": lifecycle_epoch })
}

pub fn navigate_args(session_id: &str, url: &str) -> Value {
    json!({ "sessionId": session_id, "url": url })
}

pub fn open_local_file_args(session_id: &str, target: Option<&Value>) -> Value {
    json!({ "sessionId": session_id, "target": target })
}

pub fn page_network_args(session_id: &str, target: Option<&PreviewTarget>) -> Value {
    json!({ "sessionId": session_id, "target": target })
}

pub fn action_args(session_id: &str, action: &str, value: Value) -> Value {
    json!({ "sessionId": session_id, "action": action, "value": value })
}

/// Hide and close carry the epoch of the intent they carry out.
pub fn lifecycle_action_args(session_id: &str, action: &str, lifecycle_epoch: u64) -> Value {
    json!({ "sessionId": session_id, "action": action, "value": Value::Null, "lifecycleEpoch": lifecycle_epoch })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_reads_what_the_host_writes() {
        let status: BrowserStatus = serde_json::from_value(json!({
            "hasPage": true, "open": true, "url": "http://x/", "loading": false, "canGoBack": true, "canGoForward": false, "zoom": 1.5,
            "viewport": {"width": 800, "height": 600}, "elementPicker": {"armed": true, "pendingPick": false},
            "dialog": {"id": 3, "kind": "prompt", "message": "m", "defaultValue": "d"},
            "control": {"owner": "user", "handoffRequested": true, "requestedTool": "preview_click", "updatedAtMs": 1}
        })).unwrap();
        assert!(status.has_page && status.can_go_back && !status.projected);
        assert_eq!(status.element_picker.unwrap().armed, true);
        assert_eq!(status.dialog.unwrap().default_value.as_deref(), Some("d"));
        assert_eq!(status.control.unwrap().requested_tool.as_deref(), Some("preview_click"));
    }

    #[test]
    fn an_empty_status_is_about_blank_with_no_page() {
        let status = BrowserStatus::default();
        assert_eq!(status.url, "about:blank");
        assert!(!status.has_page && !status.open);
        let read: BrowserStatus = serde_json::from_value(json!({})).unwrap();
        assert_eq!(read, status);
    }

    #[test]
    fn a_blank_url_opens_the_page_where_it_was() {
        assert_eq!(open_args("s", Some("  "), 5)["url"], Value::Null);
        assert_eq!(open_args("s", Some(" http://x/ "), 5)["url"], "http://x/");
        assert_eq!(open_args("s", None, 5)["lifecycleEpoch"], 5);
    }

    #[test]
    fn bounds_go_out_in_the_host_s_spelling() {
        let bounds = BrowserPanelBounds { x: 1.0, y: 2.0, width: 3.0, height: 4.0, visible: true, occluded_top: None, bottom_corner_radius: Some(6.0) };
        let args = bounds_args("s", &bounds, 7);
        assert_eq!(args["bounds"]["bottomCornerRadius"], 6.0);
        assert!(args["bounds"].get("occludedTop").is_none());
        assert_eq!(args["lifecycleEpoch"], 7);
    }

    #[test]
    fn a_picked_element_reads_what_the_host_writes() {
        let element: SelectedElement = serde_json::from_value(json!({
            "sequence": 2, "tagName": "button", "classes": ["a"], "attributes": {"aria-label": "x"}, "computedStyles": {"color": "red"},
            "boundingBox": {"x": 1, "y": 2, "width": 3, "height": 4}, "screenshotBase64": "", "innerText": "t"
        })).unwrap();
        assert_eq!(element.tag_name, "button");
        assert_eq!(element.bounding_box.height, 4.0);
    }
}
