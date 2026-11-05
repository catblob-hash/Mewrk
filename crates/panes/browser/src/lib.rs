//! The preview pane, upstream's `BrowserPanel` and `PreviewPane`, and the page lifecycle `App.tsx` kept.
//!
//! Public views and what the shell calls:
//!
//! ```text
//! browser_pane(app: &mut App, props: BrowserPaneProps) -> BrowserPaneView { header: E, subheader: Option<E>, body: E }
//!     The pane, every frame it is on screen. `header` goes in SidePane's header slot (the toolbar, or `props.tabs`
//!     when given, which sends the toolbar to `subheader`), `body` in its body. Stopping the calls unmounts it: the
//!     page is parked and an armed picker disarmed. Pages place themselves from the body's rectangle.
//! idle_page() -> E                                        The one-line standby, for a pane with no session at all.
//! attach_window(app: &mut App)                            Once the window exists: where pages go, and the heartbeat
//!                                                         without which the host hides every page after 12 s.
//! present_page(app, session_id, url: Option<String>, done: FnOnce(&mut App, Result<bool, String>))
//!                                                         Shows a page: issues the Open intent, calls browser_open.
//! open_built_in_browser(app, session_id, epoch, url, done) The same with an epoch the caller claimed itself.
//! hide_built_in_browser(app)                              Hides whichever page is presented.
//! request_browser_session_close(app, session_id, done: FnOnce(&mut App, Result<(), String>))
//!                                                         Closes a page for good, with its retries and fences.
//! note_preview_pane_closed()                              A pane closed: pages it showed take the theme afresh.
//! element_crop_file(&SelectedElement) -> Option<(String, Vec<u8>)>       A pick's crop as a composer attachment.
//! selected_element_block / with_selected_elements / selected_elements_from_text / strip_selected_element_blocks /
//! selected_element_label / selected_element_excerpt / images_without_element_crops       The composer's chip logic.
//! pages::servers_to_stop_for_closed_page(closed, owner_id, servers, remaining_pages, machine_of) -> Vec<&Server>
//! pages::pages_served_by(server, server_machine, pages) -> Vec<session id>      What a closed tab or a stopped server
//!                                                         takes with it; the shell has the roster they read.
//! page_is_served_by_server, preview_viewer_server_of_page, preview_page_is_served_by     The pairing rule itself.
//! ```
//!
//! Props, in place of upstream's: `BrowserPaneProps { session_id, background_sessions, active, target, file_target, tabs, link_notice,
//! trusted_overlay_rect, initial_status, on_attach_image(file name, PNG), on_element_picked, on_open_page,
//! bottom_corner_radius }`. `native`, `browserDev` and `paneId`-family props have nothing to carry here, and
//! `onReservedBottomChange` is gone because the page area is measured with the log drawer already out of it.

pub mod address;
pub mod browser;
pub mod controller;
pub mod flows;
pub mod pages;
pub mod panel;
pub mod preview;
pub mod projection;
pub mod screen_view;
pub mod selected_element;
pub mod servers;

pub use pages::{attach_window, hide_built_in_browser, open_built_in_browser, present_page, request_browser_session_close};
pub use panel::{browser_pane, idle_page, set_window_visible, BrowserPaneProps, BrowserPaneView, OpenPage};
pub use preview::{note_preview_pane_closed, page_is_served_by_server, preview_page_is_served_by, preview_viewer_server_of_page, PreviewTarget};
pub use selected_element::{selected_element_image_file as element_crop_file, images_without_element_crops, selected_element_block, selected_element_excerpt, selected_element_label, selected_elements_from_text, strip_selected_element_blocks, with_selected_elements};
