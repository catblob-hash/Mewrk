//! The accessibility tree, driven with scripted input and read the way the platform adapters read it: every update goes through
//! AccessKit's own consumer, which panics on one the adapters could not take, and the tree is walked through its filter.

use std::cell::Cell;
use std::rc::Rc;

use accesskit::{Action, ActionData, ActionRequest, NodeId, Point, TextPosition, TextSelection, TreeId};
use accesskit_consumer::{common_filter, FilterResult, NodeRef, Tree, TreeChangeHandler};
use fluent::el::*;
use fluent::ui::{rect, Icon, Input, Key, KeyPress, NoClipboard, Rect, UiMemory};
use fluent::{Scene, TextCtx, Theme, Ui};

#[derive(Default)]
struct App {
    clicks: Vec<&'static str>,
    open: bool,
    dialog: bool,
    value: String,
    number: f64,
    on: bool,
    keys: Vec<Key>,
    pointer: Vec<Phase>,
    words: Vec<&'static str>,
    rows: Vec<&'static str>,
}

/// What the platform would be told about, as the macOS adapter's events do: a live region's words when they arrive or change.
#[derive(Default)]
struct Heard {
    spoken: Vec<(String, Live)>,
}

impl TreeChangeHandler for Heard {
    fn node_added(&mut self, node: &NodeRef) {
        if common_filter(node) == FilterResult::Include && node.live() != Live::Off && let Some(words) = said(node) {
            self.spoken.push((words, node.live()));
        }
    }
    fn node_updated(&mut self, old: &NodeRef, new: &NodeRef) {
        if new.live() != Live::Off && said(old) != said(new) && let Some(words) = said(new) {
            self.spoken.push((words, new.live()));
        }
    }
    fn focus_moved(&mut self, _: Option<&NodeRef>, _: Option<&NodeRef>) {}
    fn node_removed(&mut self, _: &NodeRef) {}
}

fn said(node: &NodeRef) -> Option<String> {
    if node.label_comes_from_value() { node.value() } else { node.label() }
}

struct Rig {
    scene: Scene,
    text: TextCtx,
    mem: UiMemory,
    input: Input,
    clipboard: NoClipboard,
    scale: f32,
    tree: Option<Tree>,
    heard: Heard,
    updates: usize,
    last_nodes: Vec<NodeId>,
}

impl Rig {
    fn new() -> Self {
        set_a11y_active(true);
        Self { scene: Scene::default(), text: TextCtx::new(1.0), mem: UiMemory::default(), input: Input::default(), clipboard: NoClipboard, scale: 1.0, tree: None, heard: Heard::default(), updates: 0, last_nodes: Vec::new() }
    }

    fn frame(&mut self, app: &mut App, view: &dyn Fn(&App) -> El<App>) {
        self.scene.clear();
        let mut ui = Ui::new(&mut self.scene, &mut self.text, Theme::dark(), &self.input, &mut self.mem, &mut self.clipboard, self.scale, (800.0 / self.scale, 600.0 / self.scale), 0.016, 0.0);
        render(view(app), app, &mut ui);
        self.input.end_frame();
        self.last_nodes.clear();
        if let Some(update) = take_a11y_update() {
            self.updates += 1;
            self.last_nodes = update.nodes.iter().map(|(id, _)| *id).collect();
            match &mut self.tree {
                Some(tree) => tree.update_and_process_changes(update, &mut self.heard),
                None => self.tree = Some(Tree::new(update, true)),
            }
        }
    }

    /// Frames enough for what handlers changed to be on screen.
    fn settle(&mut self, app: &mut App, view: &dyn Fn(&App) -> El<App>) {
        self.frame(app, view);
        self.frame(app, view);
    }

    fn act(&mut self, app: &mut App, view: &dyn Fn(&App) -> El<App>, node: NodeId, action: Action, data: Option<ActionData>) {
        a11y_action(ActionRequest { action, target_tree: TreeId::ROOT, target_node: node, data });
        self.settle(app, view);
    }

    fn root(&self) -> NodeRef<'_> {
        self.tree.as_ref().expect("a frame ran").state().root()
    }

    /// The node a reader reaches with this name or these words, walking what the filter lets through.
    fn named(&self, name: &str) -> NodeRef<'_> {
        fn walk<'a>(node: NodeRef<'a>, name: &str) -> Option<NodeRef<'a>> {
            if said(&node).as_deref() == Some(name) {
                return Some(node);
            }
            node.filtered_children(common_filter).find_map(|c| walk(c, name))
        }
        walk(self.root(), name).unwrap_or_else(|| panic!("nothing says {name:?} in {}", self.dump()))
    }

    fn with_key(&self, key: impl std::hash::Hash) -> NodeRef<'_> {
        let id = NodeId(hash_key(key));
        fn walk<'a>(node: NodeRef<'a>, id: NodeId) -> Option<NodeRef<'a>> {
            if node.locate().0 == id {
                return Some(node);
            }
            node.filtered_children(common_filter).find_map(|c| walk(c, id))
        }
        walk(self.root(), id).unwrap_or_else(|| panic!("that key is not in {}", self.dump()))
    }

    fn says_anything(&self, name: &str) -> bool {
        fn walk(node: NodeRef<'_>, name: &str) -> bool {
            said(&node).as_deref() == Some(name) || node.filtered_children(common_filter).any(|c| walk(c, name))
        }
        walk(self.root(), name)
    }

    /// The tree as roles and words, children in brackets.
    fn dump(&self) -> String {
        fn walk(node: NodeRef<'_>, out: &mut String) {
            out.push_str(&format!("{:?}", node.role()));
            if let Some(words) = said(&node) {
                out.push_str(&format!(" {words:?}"));
            }
            let kids: Vec<_> = node.filtered_children(common_filter).collect();
            if !kids.is_empty() {
                out.push_str(" [");
                for (i, kid) in kids.into_iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    walk(kid, out);
                }
                out.push(']');
            }
        }
        let mut out = String::new();
        walk(self.root(), &mut out);
        out
    }

    fn focus_key(&self) -> NodeId {
        self.tree.as_ref().expect("a frame ran").state().focus().expect("focus").locate().0
    }
}

fn field_node(key: &str) -> u64 {
    hash_key(hash_key(("field", key)))
}

#[test]
fn text_is_a_label_and_layout_stays_out_of_the_tree() {
    let view = |_: &App| div::<App>().child(row().p(4.0).child(div().child(text("Hello"))).child(div().child(div())));
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    assert_eq!(rig.dump(), r#"Window [Label "Hello"]"#);
}

#[test]
fn a_click_handler_is_a_button_named_by_what_it_says() {
    let view = |_: &App| div::<App>().child(row().key("b").h(30.0).on_click(|a: &mut App| a.clicks.push("b")).child(icon(Icon::Check, 12.0)).child(text("Save")).child(div().child(text("all"))));
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    assert_eq!(rig.dump(), r#"Window [Button "Save all"]"#);
}

#[test]
fn an_icon_button_is_named_by_its_tooltip_and_its_icon_is_decoration() {
    let view = |_: &App| div::<App>().child(icon_button::<App>("X", "Close").on_click(|_| {})).child(icon::<App>(Icon::Check, 12.0));
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    assert_eq!(rig.dump(), r#"Window [Button "Close"]"#);
    assert!(!rig.named("Close").has_description(), "a tooltip that is the name is not said twice");
}

#[test]
fn a_tooltip_beside_words_is_a_description() {
    let view = |_: &App| div::<App>().child(button::<App>("Stop").tooltip("Stops the run").on_click(|_| {}));
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    assert_eq!(rig.named("Stop").description().as_deref(), Some("Stops the run"));
}

#[test]
fn a_clickable_row_with_controls_inside_keeps_them_reachable() {
    let view = |_: &App| {
        div::<App>().child(row().key("row").h(30.0).on_click(|a: &mut App| a.clicks.push("row")).child(text("Title")).child(icon_button("Ellipsis", "More").on_click(|a: &mut App| a.clicks.push("more"))))
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    assert_eq!(rig.dump(), r#"Window [Group "Title" [Button "More"]]"#);
    let (row_id, more) = (rig.named("Title").locate().0, rig.named("More").locate().0);
    rig.act(&mut app, &view, more, Action::Click, None);
    rig.act(&mut app, &view, row_id, Action::Click, None);
    assert_eq!(app.clicks, ["more", "row"]);
}

#[test]
fn a_click_catcher_with_nothing_in_it_is_not_a_control() {
    let view = |_: &App| div::<App>().child(div().absolute().inset(0.0).on_click(|_| {})).child(text("Under"));
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    assert_eq!(rig.dump(), r#"Window [Label "Under"]"#);
}

#[test]
fn aria_label_wins_and_aria_hidden_removes_a_whole_subtree() {
    let view = |_: &App| {
        div::<App>()
            .child(button::<App>("x").aria_label("Close the panel").on_click(|_| {}))
            .child(div().aria_hidden(true).child(text("Secret")).child(button("Nor this").on_click(|_| {})))
            .child(button("Visible").on_click(|_| {}).child(text("part").aria_hidden(true)))
            .child(div().aria_label("Sidebar").child(text("Inside")))
            .child(div().on_click(|_| {}).role(Role::GenericContainer).child(text("Plain")))
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    assert_eq!(rig.dump(), r#"Window [Button "Close the panel", Button "Visible", Group "Sidebar" [Label "Inside"], Label "Plain"]"#);
}

#[test]
fn shared_widgets_carry_their_state() {
    let view = |a: &App| {
        let on = a.on;
        div::<App>()
            .child(switch(on, |a: &mut App| a.on = !a.on).key("switch").aria_label("Dark"))
            .child(checkbox(on, |_| {}).aria_label("Check"))
            .child(radio(on, |_| {}).aria_label("Radio"))
            .child(button("Off").disabled(true))
            .child(select("pick", vec!["One".into(), "Two".into()], 1, |_, _| {}).aria_label("Number"))
            .child(icon_button("X", "Gone").disabled(true))
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    assert_eq!(rig.named("Dark").role(), Role::Switch);
    assert_eq!(rig.named("Dark").toggled(), Some(Toggled::False));
    assert_eq!(rig.named("Check").role(), Role::CheckBox);
    assert_eq!(rig.named("Radio").role(), Role::RadioButton);
    assert!(rig.named("Off").is_disabled());
    assert!(rig.named("Gone").is_disabled());
    assert!(!rig.named("Off").is_clickable(&common_filter), "nothing to press");
    let combo = rig.named("Number");
    assert_eq!((combo.role(), combo.value().as_deref(), combo.data().is_expanded()), (Role::ComboBox, Some("Two"), Some(false)));
    let id = rig.with_key("switch").locate().0;
    rig.act(&mut app, &view, id, Action::Click, None);
    assert_eq!(rig.named("Dark").toggled(), Some(Toggled::True));
}

#[test]
fn a_select_opens_a_menu_that_reaches_its_handler() {
    let view = |_: &App| div::<App>().child(select("pick", vec!["One".into(), "Two".into()], 0, |a: &mut App, i| a.number = i as f64).aria_label("Number"));
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    let trigger = rig.named("Number").locate().0;
    rig.act(&mut app, &view, trigger, Action::Click, None);
    assert_eq!(rig.named("Number").data().is_expanded(), Some(true));
    assert_eq!(rig.named("One").role(), Role::MenuItemCheckBox);
    assert_eq!(rig.named("One").toggled(), Some(Toggled::True));
    assert_eq!(rig.named("Two").role(), Role::MenuItem);
    let two = rig.named("Two").locate().0;
    rig.act(&mut app, &view, two, Action::Click, None);
    assert_eq!(app.number, 1.0);
    assert!(!rig.says_anything("Two"), "the menu closed: {}", rig.dump());
}

#[test]
fn a_menu_says_its_hint_and_its_disabled_rows() {
    let view = |_: &App| {
        div::<App>().child(div().key("anchor").size(10.0, 10.0)).child(
            menu_panel::<App>(vec![MenuRow::new("Copy", |_| {}).hint("Cmd+C"), MenuRow::new("Paste", |_| {}).enabled(false)], Rc::new(|_| {})).key("panel").popover("anchor", Side::Below, Align::Start, 0.0, |_| {}),
        )
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    assert_eq!(rig.with_key("panel").role(), Role::Menu);
    assert_eq!(rig.named("Copy").description().as_deref(), Some("Cmd+C"));
    assert!(rig.named("Paste").is_disabled());
    assert!(!rig.named("Paste").is_clickable(&common_filter));
}

#[test]
fn aria_state_reaches_the_tree() {
    let view = |_: &App| {
        div::<App>()
            .child(div().key("a").role(Role::TreeItem).aria_label("Expanded").aria_expanded(true).aria_selected(true))
            .child(button("Bold").aria_pressed(true).on_click(|_| {}))
            .child(div().key("meter").aria_label("Usage").aria_value(30.0, 0.0, 100.0))
            .child(div().key("slider").aria_label("Size").aria_value(14.0, 10.0, 20.0).aria_value_text("14 points").on_key(|_, _| false))
            .child(div().key("mixed").role(Role::CheckBox).aria_label("All").aria_checked_mixed())
            .child(div().key("busy").role(Role::List).aria_label("Results").aria_busy(true).aria_current(AriaCurrent::Page).aria_invalid(true).aria_haspopup(HasPopup::Menu).aria_orientation(Orientation::Horizontal).aria_keyshortcuts("Alt+Up"))
            .child(text("Title").role(Role::Heading).aria_level(2))
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    let item = rig.named("Expanded");
    assert_eq!((item.data().is_expanded(), item.is_selected()), (Some(true), Some(true)));
    assert_eq!(rig.named("Bold").toggled(), Some(Toggled::True));
    let meter = rig.named("Usage");
    assert_eq!((meter.role(), meter.numeric_value(), meter.min_numeric_value(), meter.max_numeric_value()), (Role::ProgressIndicator, Some(30.0), Some(0.0), Some(100.0)));
    let slider = rig.named("Size");
    assert_eq!((slider.role(), slider.value().as_deref()), (Role::Slider, Some("14 points")));
    assert_eq!(rig.named("All").toggled(), Some(Toggled::Mixed));
    let list = rig.named("Results");
    assert!(list.data().is_busy());
    assert_eq!(list.data().aria_current(), Some(AriaCurrent::Page));
    assert_eq!(list.data().invalid(), Some(Invalid::True));
    assert_eq!(list.data().has_popup(), Some(HasPopup::Menu));
    assert_eq!(list.data().orientation(), Some(Orientation::Horizontal));
    assert_eq!(list.data().keyboard_shortcut(), Some("Alt+Up"));
    assert_eq!(rig.named("Title").data().level(), Some(2));
    assert_eq!(rig.named("Title").role(), Role::Heading);
}

#[test]
fn relations_point_only_at_what_is_there() {
    let view = |_: &App| {
        div::<App>()
            .child(text("The title").key("title"))
            .child(div().key("box").role(Role::Group).aria_labelledby("title").aria_describedby("gone").aria_controls("title").child(text("Body")))
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    let group = rig.with_key("box");
    assert_eq!(group.label().as_deref(), Some("The title"));
    assert!(group.data().described_by().is_empty());
    assert_eq!(group.data().controls(), [NodeId(hash_key("title"))]);
}

#[test]
fn bounds_are_physical_pixels() {
    let view = |_: &App| div::<App>().child(div().h(10.0)).child(button("Go").key("go").w(100.0).on_click(|_| {}));
    let mut rig = Rig::new();
    rig.scale = 2.0;
    let mut app = App::default();
    rig.frame(&mut app, &view);
    let r = rig.with_key("go").raw_bounds().expect("bounds");
    assert_eq!((r.x0, r.y0, r.x1, r.y1), (0.0, 20.0, 200.0, 80.0));
}

// ---------------------------------------------------------------- the text field

#[test]
fn a_field_is_a_text_input_with_its_value_and_caret() {
    let view = |_: &App| div::<App>().child(field("name", "héllo").placeholder("Your name").build().aria_label("Name"));
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    let node = rig.named("Name");
    assert_eq!((node.role(), node.value().as_deref(), node.data().placeholder()), (Role::TextInput, Some("héllo"), Some("Your name")));
    assert_eq!(node.locate().0, NodeId(field_node("name")));
    assert!(node.supports_text_ranges());
    assert!(node.is_focusable(&common_filter));
    let selection = node.text_selection().expect("a caret");
    assert!(selection.is_degenerate());
    assert_eq!(selection.start().to_global_utf16_index(), 5, "the caret sits after the text, counted in characters");
}

#[test]
fn a_multiline_field_reads_line_by_line() {
    let view = |_: &App| div::<App>().child(field("notes", "one\ntwo\n").multiline(60.0, 200.0).build().aria_label("Notes"));
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    let node = rig.named("Notes");
    assert_eq!(node.role(), Role::MultilineTextInput);
    assert_eq!(node.value().as_deref(), Some("one\ntwo\n"));
    assert_eq!(node.document_range().text(), "one\ntwo\n");
    assert_eq!(node.line_range_from_index(1).expect("a second line").text(), "two\n");
    assert_eq!(node.text_selection_focus().expect("a caret").to_line_index(), 2, "after the last line end is the empty third line");
}

#[test]
fn a_password_never_leaves_in_the_clear() {
    let view = |_: &App| div::<App>().child(field("pw", "hunter2").obscure(true).build().aria_label("Password"));
    let mut rig = Rig::new();
    let mut app = App::default();
    for _ in 0..3 {
        rig.frame(&mut app, &view);
        let node = rig.named("Password");
        assert_eq!(node.role(), Role::PasswordInput);
        assert_eq!(node.value().as_deref(), Some("•••••••"));
        assert_eq!(node.document_range().text(), "•••••••");
    }
}

#[test]
fn a_field_out_of_view_is_still_a_field() {
    let view = |_: &App| {
        col::<App>().child(
            div().key("scroller").h(100.0).scroll_y().child(text("Near")).child(div().h(1000.0).shrink0()).child(field("far", "away").build().aria_label("Far")),
        )
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.settle(&mut app, &view);
    let node = rig.with_key(hash_key(("field", "far")));
    assert_eq!((node.role(), node.value().as_deref()), (Role::TextInput, Some("away")));
    let id = node.locate().0;
    rig.act(&mut app, &view, id, Action::Focus, None);
    assert_eq!(focused_id(), hash_key(("field", "far")));
}

#[test]
fn a_set_value_is_heard_as_typing_is() {
    let view = |a: &App| div::<App>().child(field("name", a.value.clone()).on_change(|a: &mut App, v| a.value = v).build().aria_label("Name"));
    let mut rig = Rig::new();
    let mut app = App { value: "old".into(), ..App::default() };
    rig.frame(&mut app, &view);
    let id = NodeId(field_node("name"));
    rig.act(&mut app, &view, id, Action::SetValue, Some(ActionData::Value("new words".into())));
    assert_eq!(app.value, "new words");
    assert_eq!(rig.named("Name").value().as_deref(), Some("new words"));
}

#[test]
fn a_selection_set_from_outside_is_what_typing_replaces() {
    let view = |a: &App| div::<App>().child(field("name", a.value.clone()).on_change(|a: &mut App, v| a.value = v).build().aria_label("Name"));
    let mut rig = Rig::new();
    let mut app = App { value: "hello wörld".into(), ..App::default() };
    rig.frame(&mut app, &view);
    let id = NodeId(field_node("name"));
    rig.act(&mut app, &view, id, Action::Focus, None);
    let run = rig.named("Name").children().next().expect("a run").locate().0;
    let at = |character_index| TextPosition { node: run, character_index };
    rig.act(&mut app, &view, id, Action::SetTextSelection, Some(ActionData::SetTextSelection(TextSelection { anchor: at(6), focus: at(11) })));
    assert_eq!(rig.named("Name").text_selection().expect("a selection").text(), "wörld");
    rig.act(&mut app, &view, id, Action::ReplaceSelectedText, Some(ActionData::Value("there".into())));
    assert_eq!(app.value, "hello there");
    rig.input.text.push('!');
    rig.settle(&mut app, &view);
    assert_eq!(app.value, "hello there!");
}

#[test]
fn a_read_only_edit_takes_no_edits() {
    let view = |a: &App| {
        let edit = use_state("read-only", || {
            let mut edit = fluent::TextEdit::new();
            edit.set_text("fixed");
            edit.set_read_only(true);
            edit
        });
        let _ = a;
        col::<App>().child(custom(move |_: &mut App, ui: &mut Ui<'_>, r| {
            edit.borrow_mut().show(ui, 77, r, "", &fluent::theme::BODY_SMALL, false);
        }).key(77u64).h(40.0).aria_label("Fixed"))
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.settle(&mut app, &view);
    let node = rig.named("Fixed");
    assert_eq!((node.role(), node.is_read_only()), (Role::MultilineTextInput, true));
    let id = node.locate().0;
    assert!(!rig.named("Fixed").is_focusable(&common_filter));
    rig.act(&mut app, &view, id, Action::SetValue, Some(ActionData::Value("changed".into())));
    assert_eq!(rig.named("Fixed").value().as_deref(), Some("fixed"));
}

#[test]
fn an_edit_a_leaf_paints_under_another_id_is_a_node_beside_it() {
    let view = |_: &App| {
        let edit = use_state("beside", fluent::TextEdit::new);
        col::<App>().child(custom(move |_: &mut App, ui: &mut Ui<'_>, r| {
            edit.borrow_mut().show(ui, 5150, r, "Search", &fluent::theme::BODY_SMALL, false);
        }).h(40.0))
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.settle(&mut app, &view);
    let node = rig.with_key(5150u64);
    assert_eq!((node.role(), node.placeholder()), (Role::MultilineTextInput, Some("Search")), "an empty field shows it");
    let id = node.locate().0;
    rig.act(&mut app, &view, id, Action::Focus, None);
    assert_eq!(focused_id(), 5150);
    assert_eq!(rig.focus_key(), id);
}

// ---------------------------------------------------------------- focus

#[test]
fn focus_is_the_apps_own() {
    let view = |_: &App| {
        div::<App>()
            .child(field("name", "x").build().aria_label("Name"))
            .child(div().key("list").h(40.0).focusable().aria_label("List").child(text("Row")))
            .child(button("Plain").on_click(|_| {}))
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    assert_eq!(rig.root().locate().0, rig.focus_key(), "nothing has focus");

    focus_request(("field", "name"));
    rig.settle(&mut app, &view);
    assert_eq!(rig.focus_key(), NodeId(field_node("name")), "a field holds focus one hash short of its node");

    let list = rig.named("List").locate().0;
    rig.act(&mut app, &view, list, Action::Focus, None);
    assert_eq!(focused_id(), hash_key("list"));
    assert_eq!(rig.focus_key(), list);

    assert!(!rig.named("Plain").is_focusable(&common_filter), "a plain button holds no focus in this app");

    rig.act(&mut app, &view, list, Action::Blur, None);
    assert_eq!(focused_id(), 0);

    let field = NodeId(field_node("name"));
    rig.act(&mut app, &view, field, Action::Focus, None);
    assert_eq!(focused_id(), hash_key(("field", "name")));
    rig.act(&mut app, &view, field, Action::Blur, None);
    assert_eq!(focused_id(), 0);

    rig.act(&mut app, &view, field, Action::Focus, None);
    let window = rig.root().locate().0;
    rig.act(&mut app, &view, window, Action::Focus, None);
    assert_eq!(focused_id(), 0, "the window taking focus is how the macOS adapter says a node lost it");
}

// ---------------------------------------------------------------- actions

#[test]
fn a_click_reaches_the_handler_a_press_does() {
    let view = |_: &App| div::<App>().child(button("Go").key("go").w(100.0).on_click(|a: &mut App| a.clicks.push("go")));
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    let go = rig.named("Go").locate().0;
    rig.act(&mut app, &view, go, Action::Click, None);
    assert_eq!(app.clicks, ["go"]);
    assert!(rig.named("Go").is_clickable(&common_filter));
}

#[test]
fn a_disabled_control_refuses_what_it_is_asked() {
    let view = |_: &App| div::<App>().child(button("Go").disabled(true).on_click(|a: &mut App| a.clicks.push("go"))).child(button("Also").aria_disabled(true).on_click(|a: &mut App| a.clicks.push("also")));
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    for name in ["Go", "Also"] {
        let id = rig.named(name).locate().0;
        assert!(rig.named(name).is_disabled());
        rig.act(&mut app, &view, id, Action::Click, None);
    }
    assert!(app.clicks.is_empty());
}

#[test]
fn a_press_with_only_a_pointer_handler_is_a_press_and_a_release() {
    let view = |_: &App| {
        div::<App>().child(div().key("handle").h(10.0).role(Role::Button).aria_label("Handle").on_pointer(|a: &mut App, e| {
            assert!(e.rect.w > 0.0);
            a.pointer.push(e.phase);
        }))
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    let id = rig.named("Handle").locate().0;
    rig.act(&mut app, &view, id, Action::Click, None);
    assert_eq!(app.pointer, [Phase::Down, Phase::Up]);
}

#[test]
fn a_context_menu_reaches_its_handler() {
    let view = |_: &App| div::<App>().child(button("Row").on_click(|_| {}).on_context_menu(|a: &mut App, _| a.clicks.push("menu")));
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    let id = rig.named("Row").locate().0;
    rig.act(&mut app, &view, id, Action::ShowContextMenu, None);
    assert_eq!(app.clicks, ["menu"]);
}

#[test]
fn expand_and_collapse_only_press_what_is_in_the_other_state() {
    let view = |a: &App| div::<App>().child(button("Section").aria_expanded(a.open).on_click(|a: &mut App| a.open = !a.open));
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    let id = rig.named("Section").locate().0;
    rig.act(&mut app, &view, id, Action::Collapse, None);
    assert!(!app.open);
    rig.act(&mut app, &view, id, Action::Expand, None);
    assert!(app.open);
    rig.act(&mut app, &view, id, Action::Expand, None);
    assert!(app.open);
}

#[test]
fn increment_and_decrement_are_the_arrow_keys() {
    let view = |_: &App| {
        div::<App>().child(div().key("slider").h(10.0).aria_label("Size").aria_value(5.0, 0.0, 10.0).on_key(|a: &mut App, press: &KeyPress| {
            a.keys.push(press.key);
            true
        }))
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    let node = rig.named("Size");
    assert_eq!(node.role(), Role::Slider);
    assert!(node.supports_increment(&common_filter) && node.supports_decrement(&common_filter));
    let id = node.locate().0;
    rig.act(&mut app, &view, id, Action::Increment, None);
    rig.act(&mut app, &view, id, Action::Decrement, None);
    assert_eq!(app.keys, [Key::Right, Key::Left]);
}

#[test]
fn a_slider_with_no_keys_steps_through_its_value_handler() {
    let view = |a: &App| div::<App>().child(div().key("slider").h(10.0).aria_label("Size").aria_value(a.number, 0.0, 100.0).on_aria_value(|a: &mut App, v| a.number = v));
    let mut rig = Rig::new();
    let mut app = App { number: 50.0, ..App::default() };
    rig.frame(&mut app, &view);
    let id = rig.named("Size").locate().0;
    rig.act(&mut app, &view, id, Action::Increment, None);
    assert_eq!(app.number, 55.0);
    rig.act(&mut app, &view, id, Action::Decrement, None);
    assert_eq!(app.number, 50.0);
    rig.act(&mut app, &view, id, Action::SetValue, Some(ActionData::NumericValue(99.0)));
    assert_eq!(app.number, 99.0);
    assert_eq!(rig.named("Size").numeric_value(), Some(99.0));
}

fn long_list(_: &App) -> El<App> {
    col::<App>().child(div().key("list").h(100.0).scroll_y().children((0..20).map(|i| button::<App>(format!("Row {i}")).key(("row", i)).on_click(|_| {}))))
}

#[test]
fn a_scroller_scrolls_when_asked() {
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.settle(&mut app, &long_list);
    let list = rig.with_key("list");
    assert_eq!(list.role(), Role::ScrollView);
    assert!(list.clips_children());
    let id = list.locate().0;
    rig.act(&mut app, &long_list, id, Action::SetScrollOffset, Some(ActionData::SetScrollOffset(Point::new(0.0, 120.0))));
    for _ in 0..60 {
        rig.frame(&mut app, &long_list);
    }
    assert!((scroll_state("list").expect("a scroller").0 - 120.0).abs() < 1.0);
    assert!((rig.with_key("list").data().scroll_y().expect("a position") - 120.0).abs() < 1.0);
    rig.act(&mut app, &long_list, id, Action::ScrollDown, None);
    for _ in 0..60 {
        rig.frame(&mut app, &long_list);
    }
    assert!((scroll_state("list").expect("a scroller").0 - 210.0).abs() < 1.0);
    rig.act(&mut app, &long_list, id, Action::ScrollUp, None);
    for _ in 0..60 {
        rig.frame(&mut app, &long_list);
    }
    assert!((scroll_state("list").expect("a scroller").0 - 120.0).abs() < 1.0);
}

#[test]
fn what_scrolled_away_leaves_one_row_to_step_onto_and_stepping_onto_it_brings_it_in() {
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.settle(&mut app, &long_list);
    // Thirty high, a hundred tall: the fourth row is cut, the fifth is the neighbour a reader can step to, the sixth is out.
    assert!(rig.says_anything("Row 4"), "{}", rig.dump());
    assert!(!rig.says_anything("Row 5"), "{}", rig.dump());
    let neighbour = rig.named("Row 4");
    assert!(neighbour.supports_action(Action::ScrollIntoView, &common_filter));
    let id = neighbour.locate().0;
    rig.act(&mut app, &long_list, id, Action::ScrollIntoView, None);
    for _ in 0..60 {
        rig.frame(&mut app, &long_list);
    }
    let offset = scroll_state("list").expect("a scroller").0;
    assert!((offset - 50.0).abs() < 1.0, "{offset}");
    assert!(rig.says_anything("Row 5"), "{}", rig.dump());
}

// ---------------------------------------------------------------- ids

#[test]
fn nothing_changed_means_nothing_sent() {
    let view = |_: &App| div::<App>().child(button("Go").on_click(|_| {})).child(field("f", "text").build()).child(text("Words"));
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    let first = rig.updates;
    let ids: Vec<NodeId> = rig.root().children().map(|c| c.locate().0).collect();
    for _ in 0..5 {
        rig.frame(&mut app, &view);
    }
    assert_eq!(rig.updates, first, "same frame, no update");
    assert_eq!(ids, rig.root().children().map(|c| c.locate().0).collect::<Vec<_>>());
}

#[test]
fn a_change_sends_only_what_changed() {
    let view = |a: &App| div::<App>().child(text("Fixed")).child(button("Stays").on_click(|_| {})).child(text(if a.on { "After" } else { "Before" }).key("changing"));
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    app.on = true;
    rig.frame(&mut app, &view);
    assert_eq!(rig.last_nodes, [NodeId(hash_key("changing"))]);
    assert_eq!(rig.named("After").locate().0, NodeId(hash_key("changing")));
}

#[test]
fn a_keyed_row_keeps_its_id_when_rows_move() {
    let view = |a: &App| div::<App>().key("list").role(Role::List).children(a.rows.iter().map(|name| button::<App>(*name).key(("row", *name)).h(20.0).on_click(|_| {})));
    let mut rig = Rig::new();
    let mut app = App { rows: vec!["a", "b", "c"], ..App::default() };
    rig.frame(&mut app, &view);
    let before = rig.named("b").locate().0;
    assert_eq!(before, NodeId(hash_key(("row", "b"))));
    app.rows = vec!["b", "c", "a"];
    rig.frame(&mut app, &view);
    assert_eq!(rig.named("b").locate().0, before);
    let order: Vec<_> = rig.with_key("list").children().map(|c| said(&c).expect("named")).collect();
    assert_eq!(order, ["b", "c", "a"]);
    rig.frame(&mut app, &view);
    assert_eq!(rig.named("b").locate().0, before);
}

#[test]
fn two_elements_under_one_key_are_two_nodes_and_each_gets_its_own_action() {
    let view = |_: &App| {
        div::<App>().child(button("First").key("same").on_click(|a: &mut App| a.clicks.push("first"))).child(button("Second").key("same").on_click(|a: &mut App| a.clicks.push("second")))
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.settle(&mut app, &view);
    let (first, second) = (rig.named("First").locate().0, rig.named("Second").locate().0);
    assert_ne!(first, second);
    rig.act(&mut app, &view, second, Action::Click, None);
    rig.act(&mut app, &view, first, Action::Click, None);
    assert_eq!(app.clicks, ["second", "first"]);
}

// ---------------------------------------------------------------- portals

fn with_popover(a: &App) -> El<App> {
    div::<App>()
        .child(text("Before"))
        .child(
            button::<App>("Open")
                .key("trigger")
                .on_click(|a: &mut App| a.open = !a.open)
                // Written inside a button, whose content a reader never reaches.
                .when(a.open, |e| e.child(div().key("pop").role(Role::Menu).aria_label("Options").w(100.0).child(button("Pick").on_click(|a: &mut App| a.clicks.push("pick"))).popover("trigger", Side::Below, Align::Start, 0.0, |a: &mut App| a.open = false))),
        )
        .child(button("After").on_click(|a: &mut App| a.clicks.push("after")))
}

#[test]
fn a_popover_is_a_child_of_the_window_wherever_it_was_written() {
    let mut rig = Rig::new();
    let mut app = App { open: true, ..App::default() };
    rig.settle(&mut app, &with_popover);
    assert_eq!(rig.dump(), r#"Window [Label "Before", Button "Open", Button "After", Menu "Options" [Button "Pick"]]"#);
    let trigger = rig.named("Open");
    assert_eq!(trigger.data().is_expanded(), Some(true), "what is anchored to it says it is open");
    assert_eq!(trigger.data().has_popup(), Some(HasPopup::Menu));
    assert_eq!(trigger.data().controls(), [NodeId(hash_key("pop"))]);
    assert_eq!(rig.focus_key(), NodeId(hash_key("pop")), "with nothing focused, a reader is taken to the menu");
}

#[test]
fn closing_a_popover_takes_it_out_and_gives_the_focus_back() {
    let mut rig = Rig::new();
    let mut app = App { open: true, ..App::default() };
    rig.settle(&mut app, &with_popover);
    app.open = false;
    rig.settle(&mut app, &with_popover);
    assert_eq!(rig.dump(), r#"Window [Label "Before", Button "Open", Button "After"]"#);
    assert_eq!(rig.named("Open").data().is_expanded(), None);
    assert_eq!(rig.focus_key(), rig.root().locate().0);
}

#[test]
fn pressing_something_else_closes_a_popover_as_a_press_does() {
    let mut rig = Rig::new();
    let mut app = App { open: true, ..App::default() };
    rig.settle(&mut app, &with_popover);
    // Its anchor is not outside it: pressing that only runs the toggle, once.
    let trigger = rig.named("Open").locate().0;
    rig.act(&mut app, &with_popover, trigger, Action::Click, None);
    assert!(!app.open);
    rig.act(&mut app, &with_popover, trigger, Action::Click, None);
    assert!(app.open);
    let pick = rig.named("Pick").locate().0;
    rig.act(&mut app, &with_popover, pick, Action::Click, None);
    assert_eq!(app.clicks, ["pick"]);
    assert!(app.open, "inside it is inside it");
    let after = rig.named("After").locate().0;
    rig.act(&mut app, &with_popover, after, Action::Click, None);
    assert_eq!(app.clicks, ["pick", "after"]);
    assert!(!app.open);
}

fn with_dialog(a: &App) -> El<App> {
    div::<App>()
        .child(button("Behind").on_click(|a: &mut App| a.clicks.push("behind")))
        .when(a.dialog, |e| {
            e.child(dialog(300.0, col::<App>().child(text("Delete this?")).child(button("Delete").on_click(|a: &mut App| a.clicks.push("delete"))).child(field("confirm", "").build().aria_label("Type yes")), Some(Rc::new(|a: &mut App| a.dialog = false))).key("dialog").aria_label("Confirm"))
        })
        .when(a.open, |e| e.child(div().key("menu").role(Role::Menu).aria_label("Over it").w(50.0).child(button("Item").on_click(|_| {})).popover_at(10.0, 10.0, |a: &mut App| a.open = false)))
}

#[test]
fn a_modal_hides_what_is_behind_it() {
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.settle(&mut app, &with_dialog);
    let behind = rig.named("Behind").locate().0;
    app.dialog = true;
    rig.settle(&mut app, &with_dialog);
    assert_eq!(rig.dump(), r#"Window [Dialog "Confirm" [Label "Delete this?", Button "Delete", TextInput "Type yes"]]"#);
    let box_ = rig.named("Confirm");
    assert!(box_.is_modal() && box_.is_dialog());
    assert!(!box_.is_clickable(&common_filter), "pressing the dialog is not pressing its backdrop");
    assert_eq!(rig.focus_key(), NodeId(hash_key("dialog")), "with nothing focused, a reader is taken into the dialog");
    // An action aimed at what is behind it can only be a stale one, and the tree no longer has it to aim at; the element is still drawn.
    rig.act(&mut app, &with_dialog, behind, Action::Click, None);
    app.clicks.clear();
    app.dialog = false;
    rig.settle(&mut app, &with_dialog);
    assert_eq!(rig.dump(), r#"Window [Button "Behind"]"#);
    assert_eq!(rig.named("Behind").locate().0, behind, "and what comes back is what it was");
}

#[test]
fn what_opens_over_a_modal_stays_with_it() {
    let mut rig = Rig::new();
    let mut app = App { dialog: true, open: true, ..App::default() };
    rig.settle(&mut app, &with_dialog);
    assert_eq!(rig.dump(), r#"Window [Dialog "Confirm" [Label "Delete this?", Button "Delete", TextInput "Type yes"], Menu "Over it" [Button "Item"]]"#);
}

#[test]
fn focus_inside_a_dialog_stays_where_it_is() {
    let mut rig = Rig::new();
    let mut app = App { dialog: true, ..App::default() };
    rig.settle(&mut app, &with_dialog);
    focus_request(("field", "confirm"));
    rig.settle(&mut app, &with_dialog);
    assert_eq!(rig.focus_key(), NodeId(field_node("confirm")));
    assert_eq!(rig.named("Type yes").text_selection().map(|s| s.is_degenerate()), Some(true));
}

// ---------------------------------------------------------------- live regions

#[test]
fn an_announcement_is_read_each_time_and_does_not_stay() {
    let view = |_: &App| div::<App>().child(text("Page"));
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.settle(&mut app, &view);
    announce("项目顺序已更新");
    rig.frame(&mut app, &view);
    assert_eq!(rig.heard.spoken, [("项目顺序已更新".to_owned(), Live::Polite)]);
    rig.frame(&mut app, &view);
    assert_eq!(rig.dump(), r#"Window [Label "Page"]"#, "gone a frame later");
    announce("项目顺序已更新");
    announce_assertive("保存失败");
    rig.settle(&mut app, &view);
    assert_eq!(rig.heard.spoken[1..], [("项目顺序已更新".to_owned(), Live::Polite), ("保存失败".to_owned(), Live::Assertive)], "one a frame, in order");
}

#[test]
fn a_live_region_is_read_when_it_changes_and_an_alert_when_it_appears() {
    let view = |a: &App| {
        div::<App>()
            .child(div().key("status").aria_live(Live::Polite).child(text(a.words.first().copied().unwrap_or("Saved"))))
            .when(a.on, |e| e.child(div().role(Role::Alert).child(text("Save failed"))))
            .child(text("Quiet"))
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.settle(&mut app, &view);
    assert!(rig.heard.spoken.is_empty(), "what was there from the start is not news");
    assert_eq!(rig.with_key("status").role(), Role::Status);
    app.words = vec!["Saving"];
    rig.frame(&mut app, &view);
    assert_eq!(rig.heard.spoken, [("Saving".to_owned(), Live::Polite)]);
    app.on = true;
    rig.frame(&mut app, &view);
    assert_eq!(rig.heard.spoken[1], ("Save failed".to_owned(), Live::Assertive));
    assert_eq!(rig.named("Save failed").role(), Role::Alert);
}

// ---------------------------------------------------------------- nobody listening

#[test]
fn with_nobody_listening_nothing_is_kept_or_built() {
    let hits = Rc::new(Cell::new(0));
    let view = {
        let hits = hits.clone();
        move |_: &App| {
            let hits = hits.clone();
            div::<App>().child(button("Go").key("go").w(100.0).aria_label("Go on").on_click(move |_| hits.set(hits.get() + 1))).child(field("f", "text").build())
        }
    };
    let mut rig = Rig::new();
    set_a11y_active(false);
    let mut app = App::default();
    announce("nobody hears this");
    a11y_action(ActionRequest { action: Action::Click, target_tree: TreeId::ROOT, target_node: NodeId(hash_key("go")), data: None });
    rig.settle(&mut app, &view);
    assert_eq!(hits.get(), 0, "an action with nobody listening goes nowhere");
    assert!(take_a11y_update().is_none());
    assert_eq!(rig.updates, 0);
    // The client arrives: the whole tree, with what was asked while it was away forgotten.
    set_a11y_active(true);
    rig.frame(&mut app, &view);
    assert_eq!(rig.updates, 1);
    assert!(rig.tree.as_ref().is_some());
    assert_eq!(rig.named("Go on").role(), Role::Button);
    assert!(rig.heard.spoken.is_empty());
}

#[test]
fn a_client_that_comes_back_gets_the_whole_tree() {
    let view = |_: &App| div::<App>().child(button("Go").on_click(|_| {}));
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    assert!(rig.last_nodes.is_empty());
    set_a11y_active(true);
    rig.frame(&mut app, &view);
    assert_eq!(rig.last_nodes.len(), 2, "the window and the button");
}

#[test]
fn an_update_nobody_took_is_not_lost() {
    let view = |a: &App| div::<App>().child(text(if a.on { "After" } else { "Before" }).key("t")).when(a.dialog, |e| e.child(text("More")));
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    // Painted by hand, with the update left unclaimed.
    app.on = true;
    app.dialog = true;
    rig.scene.clear();
    {
        let mut ui = Ui::new(&mut rig.scene, &mut rig.text, Theme::dark(), &rig.input, &mut rig.mem, &mut rig.clipboard, 1.0, (800.0, 600.0), 0.016, 0.0);
        render(view(&app), &mut app, &mut ui);
    }
    app.dialog = false;
    rig.frame(&mut app, &view);
    assert_eq!(rig.dump(), r#"Window [Label "After"]"#);
}

#[test]
fn a_view_that_keeps_changing_shape_stays_a_valid_tree() {
    // A consumer panics on an update it can't apply, so getting through this is the assertion.
    let view = |a: &App| {
        let n = a.number as usize;
        div::<App>()
            .when(n % 2 == 0, |e| e.child(div().key("wrapper").aria_label("Wrapper").child(button("Moved").key("moved").on_click(|_| {}))))
            .when(n % 2 == 1, |e| e.child(button("Moved").key("moved").on_click(|_| {})))
            .when(n % 3 == 0, |e| e.child(text("Sometimes")))
            .when(n % 4 == 1, |e| e.child(dialog(100.0, button("In a dialog").key("moved").on_click(|_| {}), None)))
            .when(n % 5 == 2, |e| e.child(field("f", "text").multiline(40.0, 80.0).build()))
            .children((0..(n % 7)).map(|i| text::<App>(format!("Row {i}")).key(("row", i))))
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    for i in 0..200 {
        app.number = ((i * 31 + 7) % 97) as f64;
        rig.frame(&mut app, &view);
    }
    assert!(rig.updates > 100);
}

#[test]
fn what_a_leaf_paints_stands_in_the_tree_and_answers_for_itself() {
    let view = |_: &App| {
        let heading = AriaChild { label: Some("Title".into()), level: Some(2), rect: Some(rect(0.0, 10.0, 100.0, 20.0)), ..AriaChild::new(1, Role::Heading) };
        let link = AriaChild { label: Some("docs".into()), url: Some("https://example.com".into()), on_press: Some(Rc::new(|a: &mut App| a.clicks.push("link"))), ..AriaChild::new(3, Role::Link) };
        let toggle = AriaChild { label: Some("More".into()), expanded: Some(false), on_press: Some(Rc::new(|a: &mut App| a.clicks.push("toggle"))), ..AriaChild::new(4, Role::DisclosureTriangle) };
        let list = AriaChild { children: vec![link, toggle], ..AriaChild::new(2, Role::List) };
        let leaf = custom(|_: &mut App, _: &mut Ui<'_>, _: Rect| {}).key("leaf").w(200.0).h(100.0).ml(50.0).aria_children(Rc::from(vec![heading, list]));
        div::<App>().child(text("Above")).child(leaf)
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    assert_eq!(rig.dump(), r#"Window [Label "Above", Heading "Title", List [Link "docs", DisclosureTriangle "More"]]"#, "the leaf itself says nothing, as a `div` around them would not");
    let heading = rig.named("Title");
    let bounds = heading.bounding_box().expect("in the leaf's coordinates");
    assert_eq!((bounds.x0, bounds.y1 - bounds.y0, bounds.x1 - bounds.x0), (50.0, 20.0, 100.0));

    let link = rig.named("docs").locate().0;
    rig.act(&mut app, &view, link, Action::Click, None);
    assert_eq!(app.clicks, ["link"]);
    let toggle = rig.named("More").locate().0;
    rig.act(&mut app, &view, toggle, Action::Collapse, None);
    assert_eq!(app.clicks, ["link"], "already that way");
    rig.act(&mut app, &view, toggle, Action::Expand, None);
    assert_eq!(app.clicks, ["link", "toggle"]);
    set_a11y_active(false);
}

#[test]
fn inside_a_button_what_a_leaf_paints_stays_reachable() {
    let view = |_: &App| {
        let link = AriaChild { label: Some("docs".into()), on_press: Some(Rc::new(|a: &mut App| a.clicks.push("link"))), ..AriaChild::new(1, Role::Link) };
        div::<App>().child(row().key("card").h(40.0).on_click(|a: &mut App| a.clicks.push("card")).child(custom(|_: &mut App, _: &mut Ui<'_>, _: Rect| {}).w(100.0).h(20.0).aria_children(Rc::from(vec![link]))))
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    let link = rig.named("docs").locate().0;
    rig.act(&mut app, &view, link, Action::Click, None);
    assert_eq!(app.clicks, ["link"], "the link, not the card around it");
    set_a11y_active(false);
}
