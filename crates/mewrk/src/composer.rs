//! The composer's wiring: what `App.tsx` passes it, and what a pane hands it.

use mewrk_client::prelude::*;
use mewrk_client::runs::attachments::ComposerFile;
use mewrk_pane_browser::browser::SelectedElement;

/// `onAttachImage`.
pub fn attach_image(app: &mut App, conversation: &str, name: String, png: Vec<u8>) {
    app.add_composer_images(conversation, vec![ComposerFile { name, media_type: "image/png".to_owned(), bytes: png }], |_, _| {});
}

/// `onElementPicked`.
pub fn element_picked(app: &mut App, conversation: &str, element: SelectedElement) {
    let block = mewrk_pane_browser::selected_element_block(&element);
    let crop = mewrk_pane_browser::element_crop_file(&element);
    mewrk_ui_composer::stage_element_pick(app, conversation, block, crop);
}

/// The composer, with what only the shell knows: `composer=` of `App.tsx`.
pub fn composer_view(app: &mut App, conversation: &str) -> E {
    use crate::state::{shell, RemoteWorkspacePicker, WorkspaceSettingsTarget};
    let owns_tool_prompt = crate::effects::approval_dock_owner(app, conversation) == crate::effects::DockOwner::Composer;
    let pages = crate::effects::review_pages(app);
    let review_open = mewrk_client::side_panes::pane_is_open(mewrk_client::side_panes::side_pane_layout_for(app.side_panes(), Some(conversation)), "review");
    let git_review_open = review_open && crate::effects::active_review_member(app, &pages) == app.active_workspace_member();
    mewrk_ui_composer::composer(
        app,
        mewrk_ui_composer::ComposerProps {
            owns_tool_prompt,
            git_review_open,
            on_open_git_review: Some(Rc::new(|app: &mut App, member| crate::actions::open_git_review(app, Some(member)))),
            on_configure_machine: Some(Rc::new(|app: &mut App, machine| {
                shell(app).machine_settings = Some(machine);
                repaint();
            })),
            on_configure_workspace: Some(Rc::new(|app: &mut App, machine, path: String| {
                shell(app).workspace_settings = Some(WorkspaceSettingsTarget { machine, name: path.clone(), path });
                repaint();
            })),
            on_browse_machine: Some(Rc::new(|app: &mut App, machine, name| {
                shell(app).remote_picker = Some(RemoteWorkspacePicker { machine, name });
                repaint();
            })),
            has_native_workspace_picker: true,
            workspace_terminal_busy: Some(Rc::new(|app: &mut App, workspace: &str| {
                let owners: Vec<(String, bool)> = mewrk_pane_terminal::store(app).controller.current().values().map(|session| (session.conversation_id.clone(), session.busy)).collect();
                owners.iter().any(|(owner, busy)| *busy && app.owner_works_in(owner, workspace))
            })),
        },
    )
}
