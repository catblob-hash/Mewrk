//! What a screen reader is told about the shell, by upstream's `aria-*` and `role`s, and a sweep for Tab stops that say nothing.

use accesskit::{Action, Node, NodeId, Orientation, Toggled};
use mewrk_client::prelude::*;
use mewrk_client::side_panes::SidePanesAction;
use mewrk_shell::state::{shell, with_ssh_prompt, ConfirmationRequest, SshPrompt, SshPromptKind};

use crate::rig::*;

type Tree = Vec<(NodeId, Node)>;

/// All of it, as a client that has just arrived would get it.
fn read(rig: &mut Rig, app: &mut App) -> Tree {
    set_a11y_active(true);
    rig.frames(app, 4);
    take_a11y_update().expect("a client gets the tree").nodes
}

fn says<'a>(tree: &'a Tree, name: &str) -> Option<&'a Node> {
    tree.iter().map(|(_, node)| node).find(|node| node.label() == Some(name) || (node.role() == Role::Label && node.value() == Some(name)))
}

fn named<'a>(tree: &'a Tree, name: &str) -> &'a Node {
    says(tree, name).unwrap_or_else(|| panic!("nothing says {name:?} in {:?}", tree.iter().map(|(_, node)| (node.role(), node.label(), node.value())).collect::<Vec<_>>()))
}

fn with_role(tree: &Tree, role: Role) -> Vec<&Node> {
    tree.iter().map(|(_, node)| node).filter(|node| node.role() == role).collect()
}

fn role_named<'a>(tree: &'a Tree, role: Role, name: &str) -> &'a Node {
    with_role(tree, role).into_iter().find(|node| node.label() == Some(name)).unwrap_or_else(|| panic!("no {role:?} says {name:?}"))
}

fn says_something(tree: &Tree, node: &Node) -> bool {
    node.label().is_some_and(|words| !words.trim().is_empty()) || !node.labelled_by().is_empty() || (node.role() == Role::Label && node.value().is_some()) || node.children().iter().any(|child| tree.iter().find(|(id, _)| id == child).is_some_and(|(_, kid)| says_something(tree, kid)))
}

/// The sweep: a Tab stop or a press target that says nothing is a bug. One around what has words of its own, a message or a scroller,
/// has as little name as it has in a browser, and is read by what it holds.
fn unnamed(tree: &Tree) -> Vec<String> {
    tree.iter().map(|(_, node)| node).filter(|node| node.supports_action(Action::Click) || node.supports_action(Action::Focus)).filter(|node| !says_something(tree, node)).map(|node| format!("{node:?}")).collect()
}

fn all_named(tree: &Tree) {
    let said_nothing = unnamed(tree);
    assert!(said_nothing.is_empty(), "these say nothing: {said_nothing:?}");
}

/// What the frames since the last read said aloud. An announcement lasts one frame, so each is read as it comes.
fn heard(rig: &mut Rig, app: &mut App) -> Vec<String> {
    let mut heard = Vec::new();
    for at in 0..4 {
        if at > 0 {
            rig.frame(app);
        }
        if let Some(update) = take_a11y_update() {
            heard.extend(update.nodes.iter().filter(|(_, node)| node.role() == Role::Label && node.live().is_some()).filter_map(|(_, node)| node.value().map(str::to_owned)));
        }
    }
    heard
}

#[test]
fn the_sidebar_is_a_named_landmark_and_its_rows_say_what_they_are() {
    let (mut rig, mut app) = Rig::new("a11y-sidebar");
    let id = rig.add_conversation(&mut app, "ws1", "fix the build");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    let tree = read(&mut rig, &mut app);
    assert_eq!(named(&tree, "Projects and conversations").role(), Role::Complementary);
    assert_eq!(named(&tree, "Conversation list").role(), Role::Navigation);
    let drawer = named(&tree, "Collapse sidebar");
    assert_eq!((drawer.role(), drawer.is_expanded()), (Role::Button, Some(true)));
    assert_eq!(named(&tree, "Back to the previous conversation").role(), Role::Button);
    assert_eq!(named(&tree, "Search conversations").role(), Role::Button);

    let handle = named(&tree, "Resize sidebar");
    assert_eq!((handle.role(), handle.orientation(), handle.numeric_value()), (Role::Splitter, Some(Orientation::Vertical), Some(264.0)));
    assert_eq!(handle.value(), Some("264 pixels"));
    assert!(handle.supports_action(Action::Increment) && handle.supports_action(Action::Focus));

    let toggle = named(&tree, "alpha");
    assert_eq!((toggle.role(), toggle.is_expanded()), (Role::Button, Some(true)));
    assert_eq!(toggle.keyboard_shortcut(), Some("Alt+ArrowUp Alt+ArrowDown"));
    assert_eq!(toggle.controls().len(), 1, "the list it opens and closes");
    let row = named(&tree, "fix the build");
    assert_eq!((row.role(), row.keyboard_shortcut()), (Role::Button, Some("Alt+ArrowUp Alt+ArrowDown")));
    assert_eq!(named(&tree, "Rename fix the build").role(), Role::Button);
    assert_eq!(named(&tree, "Delete fix the build").role(), Role::Button);
    assert_eq!(named(&tree, "Create a task in alpha").role(), Role::Button);
    assert_eq!(named(&tree, "More options for alpha").has_popup(), Some(HasPopup::Menu));

    // Folded, what it held is out of the tree and the relation with it.
    rig.click(&mut app, ("workspace-toggle", "ws1"));
    let tree = read(&mut rig, &mut app);
    let toggle = named(&tree, "alpha");
    assert_eq!(toggle.is_expanded(), Some(false));
    assert!(toggle.controls().is_empty());
    assert!(says(&tree, "fix the build").is_none_or(|node| node.role() != Role::Button));
    set_a11y_active(false);
}

#[test]
fn moving_a_row_says_so() {
    let (mut rig, mut app) = Rig::new("a11y-move");
    set_a11y_active(true);
    rig.frames(&mut app, 2);
    rig.click(&mut app, ("workspace-toggle", "ws1"));
    let _ = take_a11y_update();
    rig.key(&mut app, KeyPress { key: Key::Down, shift: false, cmd: false, alt: true, ctrl: false });
    let heard = heard(&mut rig, &mut app);
    assert!(heard.iter().any(|words| words == "Project moved down"), "{heard:?}");
    set_a11y_active(false);
}

#[test]
fn the_top_bar_names_the_title_and_says_which_panes_are_open() {
    let (mut rig, mut app) = Rig::new("a11y-topbar");
    let id = rig.add_conversation(&mut app, "ws1", "fix the build");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    let tree = read(&mut rig, &mut app);
    assert_eq!(with_role(&tree, Role::Main).len(), 1);
    let heading = role_named(&tree, Role::Heading, "fix the build");
    assert_eq!(heading.level(), Some(1));
    assert_eq!(named(&tree, "Conversation title: fix the build. Click to rename").role(), Role::Button);
    let preview = named(&tree, "Preview");
    assert_eq!(preview.toggled(), Some(Toggled::False));
    assert_eq!(named(&tree, "More options").has_popup(), Some(HasPopup::Menu));
    assert_eq!(named(&tree, "Terminal").has_popup(), Some(HasPopup::Menu));

    // Renaming, the field takes the name.
    rig.click(&mut app, "conversation-title-name");
    let tree = read(&mut rig, &mut app);
    assert_eq!(named(&tree, "Conversation title").role(), Role::TextInput);
    set_a11y_active(false);
}

#[test]
fn a_pane_is_a_named_region_and_the_dividers_are_splitters_that_say_their_share() {
    let (mut rig, mut app) = Rig::new("a11y-panes");
    let id = rig.add_conversation(&mut app, "ws1", "fix the build");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    rig.frames(&mut app, 2);
    for pane in ["settings", "files"] {
        app.dispatch_side_panes(SidePanesAction::Toggle { conversation_id: id.clone(), pane: pane.to_owned() });
    }
    let tree = read(&mut rig, &mut app);
    assert!(with_role(&tree, Role::Region).iter().any(|region| region.label().is_some()));
    let handles: Vec<&Node> = with_role(&tree, Role::Splitter).into_iter().filter(|node| node.label() == Some("Resize panes")).collect();
    assert!(!handles.is_empty());
    for handle in handles {
        assert!(handle.numeric_value().is_some_and(|share| (0.0..=100.0).contains(&share)));
        assert_eq!((handle.min_numeric_value(), handle.max_numeric_value()), (Some(0.0), Some(100.0)));
        assert!(handle.supports_action(Action::Increment));
    }
    assert_eq!(named(&tree, "Close pane").role(), Role::Button);
    set_a11y_active(false);
}

#[test]
fn the_search_palette_is_a_dialog_with_a_combobox_over_a_listbox() {
    let (mut rig, mut app) = Rig::new("a11y-search");
    rig.add_conversation(&mut app, "ws1", "fix the build");
    rig.add_conversation(&mut app, "ws2", "write the docs");
    rig.click(&mut app, "shell-nav-search");
    let tree = read(&mut rig, &mut app);
    let dialog = role_named(&tree, Role::Dialog, "Search conversations");
    assert!(dialog.is_modal());
    let field = role_named(&tree, Role::EditableComboBox, "Search conversations");
    assert_eq!(field.is_expanded(), Some(true));
    assert_eq!(field.controls().len(), 1);
    assert_eq!(named(&tree, "Conversations").role(), Role::ListBox);
    let options = with_role(&tree, Role::ListBoxOption);
    assert_eq!(options.len(), 2);
    assert_eq!(options.iter().filter(|option| option.is_selected() == Some(true)).count(), 1);
    // The field keeps the focus, so where the arrows land is said aloud.
    let _ = take_a11y_update();
    rig.input.keys.push(KeyPress { key: Key::Down, shift: false, cmd: false, alt: false, ctrl: false });
    let heard = heard(&mut rig, &mut app);
    assert_eq!(heard.len(), 1, "{heard:?}");
    all_named(&read(&mut rig, &mut app));
    set_a11y_active(false);
}

#[test]
fn the_project_dialog_names_its_fields_and_says_what_is_wrong() {
    let (mut rig, mut app) = Rig::new("a11y-project");
    app.set_workspace_dialog_open(true, false);
    let tree = read(&mut rig, &mut app);
    let dialog = role_named(&tree, Role::Dialog, "New project");
    assert!(dialog.is_modal());
    assert_eq!(dialog.described_by().len(), 1, "its lede");
    assert_eq!(role_named(&tree, Role::Heading, "New project").level(), Some(2));
    assert_eq!(role_named(&tree, Role::TextInput, "Display name").role(), Role::TextInput);
    assert_eq!(named(&tree, "Choose a directory for the workspace").role(), Role::Button);
    assert_eq!(with_role(&tree, Role::ListItem).len(), 1);
    assert_eq!(named(&tree, "Close").role(), Role::Button);
    assert!(says(&tree, "Create project").is_some());
    all_named(&tree);
    set_a11y_active(false);
}

#[test]
fn a_question_gets_its_name_from_what_it_asks() {
    let (mut rig, mut app) = Rig::new("a11y-confirm");
    mewrk_shell::dialogs::confirm::ask(&mut app, ConfirmationRequest { question: "Throw it away?".to_owned(), detail: Some("For good.".to_owned()), confirm_label: "Throw away".to_owned(), destructive: true, on_confirm: Rc::new(|_| {}) });
    let tree = read(&mut rig, &mut app);
    assert!(role_named(&tree, Role::Dialog, "Throw it away?").is_modal());
    assert_eq!(named(&tree, "Throw away").role(), Role::Button);
    all_named(&tree);
    set_a11y_active(false);
}

#[test]
fn an_ssh_password_prompt_is_named_by_what_it_asks_and_keeps_what_is_typed_to_itself() {
    let (mut rig, mut app) = Rig::new("a11y-ssh");
    let prompt = SshPrompt { id: "p1".to_owned(), machine: "build box".to_owned(), kind: SshPromptKind::Secret, prompt: "Password for me:".to_owned(), host_key: None, retry: true };
    with_ssh_prompt(&mut shell(&mut app).ssh_prompts, prompt);
    let tree = read(&mut rig, &mut app);
    assert_eq!(with_role(&tree, Role::PasswordInput)[0].label(), Some("Password for me:"));
    let alert = with_role(&tree, Role::Alert);
    assert_eq!(alert[0].live(), Some(Live::Assertive));
    assert_eq!(alert[0].value().or(alert[0].label()), Some("That was not accepted; try again."));
    all_named(&tree);
    set_a11y_active(false);
}

#[test]
fn the_remote_directory_picker_names_its_path_and_its_list() {
    let (mut rig, mut app) = Rig::new("a11y-remote");
    shell(&mut app).remote_picker = Some(mewrk_shell::state::RemoteWorkspacePicker { machine: mewrk_client::model::RunTarget::Ssh { machine_id: "nope".to_owned() }, name: "build box".to_owned() });
    let tree = read(&mut rig, &mut app);
    assert_eq!(role_named(&tree, Role::Dialog, "Choose a workspace on build box").role(), Role::Dialog);
    assert_eq!(role_named(&tree, Role::TextInput, "Path").role(), Role::TextInput);
    assert_eq!(with_role(&tree, Role::ListBox).len(), 1);
    all_named(&tree);
    set_a11y_active(false);
}

#[test]
fn nothing_the_shell_draws_on_its_own_is_a_tab_stop_that_says_nothing() {
    let (mut rig, mut app) = Rig::new("a11y-sweep");
    let tree = read(&mut rig, &mut app);
    all_named(&tree);
    let id = rig.add_conversation(&mut app, "ws1", "fix the build");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    let tree = read(&mut rig, &mut app);
    all_named(&tree);
    set_a11y_active(false);
}
