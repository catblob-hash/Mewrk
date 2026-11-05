//! The accessibility tree: what `role` and `aria-*` were to the DOM, handed to the platform through AccessKit.
//!
//! Nothing here runs until an assistive client asks. The adapter says so, the driver calls `set_a11y_active`, and from then on
//! each frame ends with a `TreeUpdate` for what it painted. Until then a frame pays a branch, and the `aria_*` builders drop what
//! they are given before converting it.
//!
//! The tree is much smaller than the element tree. Most elements are layout and have no place in it, so their children rise to the
//! nearest element that does; what a control says is read from its content, which then stays out of the tree, as a button's text
//! does in a browser. So most views need nothing: text is a label, `on_click` is a button, a field is a text input, a scroller
//! scrolls. Actions come back through the same handlers a pointer and a keyboard reach.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

use accesskit::{Action, ActionData, ActionRequest, AriaCurrent, HasPopup, Invalid, Live, Node as AxNode, NodeId, Orientation, Rect as AxRect, Role, ScrollUnit, TextDirection, TextPosition, TextSelection, Toggled, TreeId, TreeInfo, TreeUpdate};

use super::*;

type ValueHandler<A> = Rc<dyn Fn(&mut A, f64)>;
/// What the frame runs once it is finished.
type Fired<A> = Vec<Box<dyn FnOnce(&mut A)>>;

/// What a view said about an element, beyond what its events and its leaf already say.
pub(super) struct Aria<A: 'static> {
    pub(super) role: Option<Role>,
    pub(super) label: Option<Arc<str>>,
    pub(super) description: Option<Arc<str>>,
    pub(super) value_text: Option<Arc<str>>,
    /// Now, least and most.
    pub(super) value: Option<(f64, f64, f64)>,
    pub(super) hidden: bool,
    pub(super) disabled: bool,
    pub(super) busy: bool,
    pub(super) expanded: Option<bool>,
    pub(super) selected: Option<bool>,
    pub(super) toggled: Option<Toggled>,
    pub(super) invalid: Option<Invalid>,
    pub(super) current: Option<AriaCurrent>,
    pub(super) has_popup: Option<HasPopup>,
    pub(super) live: Option<Live>,
    pub(super) orientation: Option<Orientation>,
    pub(super) level: Option<usize>,
    pub(super) shortcuts: Option<Arc<str>>,
    pub(super) controls: Vec<u64>,
    pub(super) labelled_by: Vec<u64>,
    pub(super) described_by: Vec<u64>,
    /// A field that was not painted this frame, being scrolled away, is still a field.
    pub(super) input: Option<TextInput>,
    pub(super) on_value: Option<ValueHandler<A>>,
    pub(super) children: Option<Rc<[AriaChild<A>]>>,
}

/// Something a `custom` leaf paints that a reader can stop on, as a node of the DOM could be stopped on: a message's heading, its link.
/// They stand in the tree where the leaf does, which says nothing of its own, as a `div` around them would not.
pub struct AriaChild<A: 'static> {
    /// Unique in the leaf and the same from frame to frame, so a reader keeps its place.
    pub key: u64,
    pub role: Role,
    pub label: Option<Arc<str>>,
    /// What a label reads out, or a code block holds.
    pub value: Option<Arc<str>>,
    pub description: Option<Arc<str>>,
    pub url: Option<Arc<str>>,
    pub level: Option<usize>,
    pub expanded: Option<bool>,
    pub toggled: Option<Toggled>,
    pub disabled: bool,
    /// In the leaf's coordinates. Without one it is said to be all of the leaf.
    pub rect: Option<Rect>,
    /// What pressing it does.
    pub on_press: Option<Handler<A>>,
    pub children: Vec<AriaChild<A>>,
}

impl<A: 'static> AriaChild<A> {
    pub fn new(key: u64, role: Role) -> Self {
        Self { key, role, label: None, value: None, description: None, url: None, level: None, expanded: None, toggled: None, disabled: false, rect: None, on_press: None, children: Vec::new() }
    }
}

impl<A: 'static> Default for Aria<A> {
    fn default() -> Self {
        Self { role: None, label: None, description: None, value_text: None, value: None, hidden: false, disabled: false, busy: false, expanded: None, selected: None, toggled: None, invalid: None, current: None, has_popup: None, live: None, orientation: None, level: None, shortcuts: None, controls: Vec::new(), labelled_by: Vec::new(), described_by: Vec::new(), input: None, on_value: None, children: None }
    }
}

/// A `TextEdit` as the tree sees it. Offsets count characters, since the bytes of an obscured value are not what is exposed.
#[derive(Clone, Default)]
pub(crate) struct TextInput {
    /// The id it holds focus under.
    pub edit: u64,
    /// The element that painted it, filled in as it is noted.
    pub painter: u64,
    /// An obscured field's is bullets: what it holds never leaves for another process.
    pub value: String,
    pub caret: usize,
    pub anchor: usize,
    pub multiline: bool,
    pub obscured: bool,
    pub read_only: bool,
    pub placeholder: Arc<str>,
    pub rect: Option<Rect>,
    /// Where each line of `value` is, when that was worth working out.
    pub lines: Vec<Rect>,
}

/// What a screen reader asked of a field, taken by the field as it paints so its owner hears of it as it hears of typing.
pub(crate) enum A11yEdit {
    SetValue(String),
    ReplaceSelected(String),
    /// In characters.
    Select { anchor: usize, focus: usize },
}

#[derive(Default)]
struct State {
    actions: Vec<ActionRequest>,
    announcements: Vec<(Arc<str>, Live)>,
    announced: u64,
    update: Option<TreeUpdate>,
    /// What the platform holds, so an update carries only what differs.
    sent: HashMap<u64, AxNode>,
    sent_focus: u64,
    painted: Vec<TextInput>,
    painting: u64,
    edits: Vec<(u64, A11yEdit)>,
    /// From the tree last sent, which is the one an action was aimed at. A node to the id its field holds focus under.
    inputs: HashMap<u64, u64>,
    /// A text run to its field, and how many characters come before it.
    runs: HashMap<u64, (u64, usize)>,
    /// A node that had to be given an id of its own, to the id its element shares and which of those it is.
    aliases: HashMap<u64, (u64, usize)>,
}

thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// Whether an assistive client is listening. Worth asking before working out anything expensive to say.
#[inline]
pub fn a11y_active() -> bool {
    ACTIVE.get()
}

/// The adapter's word that a client has arrived or gone. An arrival gets the whole tree on the next frame.
pub fn set_a11y_active(on: bool) {
    ACTIVE.set(on);
    STATE.with(|s| *s.borrow_mut() = State::default());
    repaint();
}

/// An action from the adapter. It reaches its handler during the next frame.
pub fn a11y_action(request: ActionRequest) {
    if a11y_active() {
        STATE.with(|s| s.borrow_mut().actions.push(request));
        repaint();
    }
}

/// What the last frame changed in the tree, for the adapter. `None` when nothing did, and when nobody is listening.
pub fn take_a11y_update() -> Option<TreeUpdate> {
    if !a11y_active() {
        return None;
    }
    STATE.with(|s| s.borrow_mut().update.take())
}

/// Says something without moving anything: `aria-live="polite"` on a span that exists only to be read. The same words twice are
/// read twice.
pub fn announce(message: impl Into<Arc<str>>) {
    announce_as(message, Live::Polite);
}

/// The same, interrupting what is being read: `role="alert"`.
pub fn announce_assertive(message: impl Into<Arc<str>>) {
    announce_as(message, Live::Assertive);
}

fn announce_as(message: impl Into<Arc<str>>, live: Live) {
    if a11y_active() {
        let message = message.into();
        STATE.with(|s| s.borrow_mut().announcements.push((message, live)));
        repaint();
    }
}

pub(crate) fn a11y_note_text_input(mut input: TextInput) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        input.painter = s.painting;
        s.painted.push(input);
    });
}

pub(crate) fn a11y_take_edits(edit: u64) -> Vec<A11yEdit> {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if s.edits.is_empty() {
            return Vec::new();
        }
        let (mine, others) = std::mem::take(&mut s.edits).into_iter().partition(|(id, _)| *id == edit);
        s.edits = others;
        mine.into_iter().map(|(_, e): (u64, A11yEdit)| e).collect::<Vec<_>>()
    })
}

pub(super) fn set_painting(id: u64) {
    STATE.with(|s| s.borrow_mut().painting = id);
}

fn window_id() -> u64 {
    hash_key("a11y-window")
}

/// Roles a browser names from their content, keeping that content out of the tree.
fn named_by_content(role: Role) -> bool {
    matches!(role, Role::Button | Role::DefaultButton | Role::Link | Role::MenuItem | Role::MenuItemCheckBox | Role::MenuItemRadio | Role::MenuListOption | Role::ListBoxOption | Role::Tab | Role::CheckBox | Role::RadioButton | Role::Switch | Role::TreeItem | Role::Heading | Role::Tooltip | Role::DisclosureTriangle | Role::ComboBox | Role::Alert | Role::Status | Role::Label)
}

fn implied_live(role: Role) -> Option<Live> {
    match role {
        Role::Alert => Some(Live::Assertive),
        Role::Status | Role::Log => Some(Live::Polite),
        _ => None,
    }
}

fn clickable<A>(n: &Node<A>) -> bool {
    n.on.click.is_some() || n.on.double_click.is_some()
}

fn aria_disabled<A>(n: &Node<A>) -> bool {
    n.disabled || n.aria.as_ref().is_some_and(|a| a.disabled)
}

fn is_modal_root<A: 'static>(frame: &Frame<A>, idx: usize) -> bool {
    frame.nodes[idx].parent == NONE && frame.portals.iter().any(|p| p.root == idx && p.portal.modal)
}

struct Builder<'a, A: 'static> {
    frame: &'a Frame<A>,
    painted: &'a [TextInput],
    scale: f64,
    out: Vec<(u64, AxNode)>,
    seen: HashSet<u64>,
    repeats: HashMap<u64, usize>,
    aliases: HashMap<u64, (u64, usize)>,
    /// Relations to resolve once every node is known, since one to a node that is not there is an error to the platform.
    relations: Vec<(usize, Relation, Vec<u64>)>,
    inputs: HashMap<u64, u64>,
    runs: HashMap<u64, (u64, usize)>,
}

#[derive(Clone, Copy)]
enum Relation {
    Controls,
    LabelledBy,
    DescribedBy,
}

impl<'a, A: 'static> Builder<'a, A> {
    fn bounds(&self, r: Rect) -> AxRect {
        // AccessKit's coordinates are physical pixels.
        AxRect { x0: r.x as f64 * self.scale, y0: r.y as f64 * self.scale, x1: r.right() as f64 * self.scale, y1: r.bottom() as f64 * self.scale }
    }

    fn claim(&mut self, id: u64) -> u64 {
        if self.seen.insert(id) {
            return id;
        }
        // Two elements under one key work on screen, and to the platform a node with two parents is fatal.
        let nth = self.repeats.entry(id).or_insert(0);
        *nth += 1;
        let unique = hash_key((id, *nth as u64));
        self.seen.insert(unique);
        self.aliases.insert(unique, (id, *nth));
        unique
    }

    fn input_of(&self, n: &'a Node<A>) -> Option<&'a TextInput> {
        self.painted.iter().find(|input| hash_key(input.edit) == n.id).or_else(|| n.aria.as_ref().and_then(|a| a.input.as_ref()))
    }

    /// Whether this stands for itself in the tree even inside a control, rather than being what the control says.
    fn is_control(&self, idx: usize) -> bool {
        let n = &self.frame.nodes[idx];
        let aria = n.aria.as_deref();
        match aria.and_then(|a| a.role) {
            Some(Role::GenericContainer) => return false,
            Some(_) => return true,
            None => {}
        }
        if self.painted.iter().any(|input| input.painter == n.id) || self.input_of(n).is_some() || aria.is_some_and(|a| a.children.is_some()) {
            return true;
        }
        if clickable(n) || n.disabled {
            return self.has_content(n);
        }
        if matches!(n.leaf, Leaf::Text { .. } | Leaf::Icon { .. } | Leaf::Image { .. }) {
            return false;
        }
        n.on.focusable || n.scroll.is_some() || aria.is_some_and(|a| a.value.is_some() || a.live.is_some()) || (!n.children.is_empty() && aria.is_some_and(|a| a.label.is_some()))
    }

    /// A press target with nothing in it is a click catcher behind something else, as a `div` with an `onClick` was: nothing a
    /// reader should stop at.
    fn has_content(&self, n: &Node<A>) -> bool {
        !n.children.is_empty() || n.tooltip.is_some() || n.aria.as_ref().is_some_and(|a| a.label.is_some()) || matches!(n.leaf, Leaf::Text { .. } | Leaf::Icon { .. } | Leaf::Image { .. })
    }

    fn holds_controls(&self, idx: usize) -> bool {
        self.frame.nodes[idx].children.iter().any(|&c| {
            let hidden = self.frame.nodes[c].aria.as_ref().is_some_and(|a| a.hidden);
            !hidden && (self.is_control(c) || self.holds_controls(c))
        })
    }

    fn has_text(&self, idx: usize) -> bool {
        let n = &self.frame.nodes[idx];
        matches!(n.leaf, Leaf::Text { .. }) || n.children.iter().any(|&c| self.has_text(c))
    }

    /// The role an element has in the tree, or `None` when it is only layout.
    fn role_of(&self, idx: usize) -> Option<Role> {
        let n = &self.frame.nodes[idx];
        let aria = n.aria.as_deref();
        if let Some(role) = aria.and_then(|a| a.role) {
            return (role != Role::GenericContainer).then_some(role);
        }
        if is_modal_root(self.frame, idx) {
            return Some(Role::Dialog);
        }
        if let Some(input) = self.input_of(n) {
            return Some(input_role(input));
        }
        if (clickable(n) || n.disabled) && self.has_content(n) {
            return Some(if self.holds_controls(idx) { Role::Group } else { Role::Button });
        }
        let labelled = aria.is_some_and(|a| a.label.is_some());
        match &n.leaf {
            Leaf::Text { spans, .. } => return (labelled || spans.iter().any(|s| !s.text.trim().is_empty())).then_some(Role::Label),
            Leaf::Icon { .. } | Leaf::Image { .. } => return (labelled || n.tooltip.is_some()).then_some(Role::Image),
            _ => {}
        }
        if let Some(a) = aria {
            if a.value.is_some() {
                return Some(if n.on.pointer.is_some() || n.on.key.is_some() || a.on_value.is_some() { Role::Slider } else { Role::ProgressIndicator });
            }
            if a.live.is_some() {
                return Some(Role::Status);
            }
        }
        if n.on.focusable {
            return Some(Role::Group);
        }
        if n.scroll.is_some() {
            return Some(Role::ScrollView);
        }
        if labelled {
            return Some(if n.children.is_empty() { Role::Label } else { Role::Group });
        }
        (n.tooltip.is_some() && !self.has_text(idx)).then_some(Role::Label)
    }

    /// What an element that is not a control adds to the words of the control it is in.
    fn leaf_text(&self, idx: usize, into: &mut String) {
        let n = &self.frame.nodes[idx];
        let explicit = n.aria.as_deref().and_then(|a| a.label.as_deref());
        let words: Option<std::borrow::Cow<'_, str>> = match (&n.leaf, explicit) {
            (_, Some(label)) => Some(label.into()),
            (Leaf::Text { spans, .. }, None) => Some(spans_text(spans).into()),
            (Leaf::Icon { .. } | Leaf::Image { .. }, None) => n.tooltip.as_deref().map(Into::into),
            _ => None,
        };
        if let Some(words) = words.filter(|w| !w.trim().is_empty()) {
            if !into.is_empty() {
                into.push(' ');
            }
            into.push_str(words.trim());
        }
    }

    fn absorb(&mut self, idx: usize, content: &mut String, kids: &mut Vec<NodeId>, scrolled: bool) {
        let n = &self.frame.nodes[idx];
        if n.aria.as_ref().is_some_and(|a| a.hidden) {
            return;
        }
        if self.is_control(idx) {
            self.emit(idx, kids, scrolled);
            return;
        }
        self.leaf_text(idx, content);
        for &c in &n.children {
            self.absorb(c, content, kids, scrolled);
        }
    }

    /// Adds this element's part of the tree to `into`.
    fn emit(&mut self, idx: usize, into: &mut Vec<NodeId>, scrolled: bool) {
        let frame = self.frame;
        let n = &frame.nodes[idx];
        let aria = n.aria.as_deref();
        if aria.is_some_and(|a| a.hidden) {
            return;
        }
        let Some(role) = self.role_of(idx) else {
            for &c in &n.children {
                self.emit(c, into, scrolled);
            }
            self.emit_foreign_inputs(n, into, scrolled);
            if let Some(children) = aria.and_then(|a| a.children.as_deref()) {
                self.emit_children(n, children, into, scrolled);
            }
            return;
        };
        let id = self.claim(n.id);
        let mut node = AxNode::new(role);
        node.set_bounds(self.bounds(n.rect));

        // ---- what is inside
        let inner = scrolled || n.scroll.is_some();
        let input = self.input_of(n);
        let by_content = input.is_none() && (named_by_content(role) || (role == Role::Group && clickable(n)));
        let mut kids = Vec::new();
        let mut content = String::new();
        if by_content {
            if let Leaf::Text { spans, .. } = &n.leaf {
                content.push_str(spans_text(spans).trim());
            }
            for &c in &n.children {
                self.absorb(c, &mut content, &mut kids, inner);
            }
        } else {
            for &c in &n.children {
                self.emit(c, &mut kids, inner);
            }
        }
        if let Some(input) = input {
            self.fill_text_input(&mut node, id, input, n.rect, &mut kids);
        }
        self.emit_foreign_inputs(n, &mut kids, inner);
        if let Some(children) = aria.and_then(|a| a.children.as_deref()) {
            self.emit_children(n, children, &mut kids, inner);
        }

        // ---- what it says
        let label = aria.and_then(|a| a.label.as_deref()).filter(|words| !words.is_empty());
        let tooltip = n.tooltip.as_deref();
        let name: Option<&str> = label.or((!content.is_empty()).then_some(content.as_str())).or(tooltip);
        let mut said_name = None;
        match role {
            // Its words are its value: a label inside a button counts towards the button's name only as one.
            Role::Label => {
                if let Some(words) = name {
                    node.set_value(words);
                    said_name = Some(words);
                }
            }
            // What a dropdown shows is what it is set to, and its name is what it sets.
            Role::ComboBox => {
                if let Some(words) = aria.and_then(|a| a.value_text.as_deref()).or((!content.is_empty()).then_some(content.as_str())) {
                    node.set_value(words);
                }
                if let Some(words) = label.or(tooltip) {
                    node.set_label(words);
                    said_name = Some(words);
                }
            }
            _ => {
                let words = if input.is_some() { label.or(tooltip) } else { name };
                if let Some(words) = words {
                    node.set_label(words);
                    said_name = Some(words);
                }
            }
        }
        if let Some(words) = aria.and_then(|a| a.description.as_deref()).or(tooltip.filter(|t| Some(*t) != said_name)) {
            node.set_description(words);
        }

        // Only layout that turned out to hold nothing leaves. Something asked for by name stays, empty or not.
        if aria.and_then(|a| a.role).is_none() && kids.is_empty() && said_name.is_none() && matches!(role, Role::Group | Role::ScrollView) && !n.on.focusable {
            self.seen.remove(&id);
            return;
        }

        // ---- its state
        let disabled = aria_disabled(n);
        if disabled {
            node.set_disabled();
        }
        if let Some(a) = aria {
            if let Some(on) = a.expanded {
                node.set_expanded(on);
            }
            if let Some(on) = a.selected {
                node.set_selected(on);
            }
            if let Some(state) = a.toggled {
                node.set_toggled(state);
            }
            if let Some(state) = a.invalid {
                node.set_invalid(state);
            }
            if let Some(state) = a.current {
                node.set_aria_current(state);
            }
            if let Some(kind) = a.has_popup {
                node.set_has_popup(kind);
            }
            if let Some(direction) = a.orientation {
                node.set_orientation(direction);
            }
            if let Some(level) = a.level {
                node.set_level(level);
            }
            if let Some(keys) = &a.shortcuts {
                node.set_keyboard_shortcut(&**keys);
            }
            if a.busy {
                node.set_busy();
            }
            if let Some((now, min, max)) = a.value {
                node.set_numeric_value(now);
                node.set_min_numeric_value(min);
                node.set_max_numeric_value(max);
            }
            if role != Role::ComboBox
                && input.is_none()
                && let Some(words) = &a.value_text
            {
                node.set_value(&**words);
            }
        }
        if let Some(live) = aria.and_then(|a| a.live).or(implied_live(role)) {
            node.set_live(live);
        }
        if is_modal_root(frame, idx) {
            node.set_modal();
        }
        self.popup_state(&mut node, n, aria);

        // ---- what can be asked of it
        let modal_root = is_modal_root(frame, idx);
        let valued = aria.is_some_and(|a| a.value.is_some());
        if !disabled {
            if !modal_root && (clickable(n) || (!valued && n.on.pointer.is_some() && aria.is_some_and(|a| a.role.is_some()))) {
                node.add_action(Action::Click);
                if aria.is_some_and(|a| a.expanded.is_some()) {
                    node.add_action(Action::Expand);
                    node.add_action(Action::Collapse);
                }
            }
            if n.on.focusable {
                node.add_action(Action::Focus);
            }
            if n.on.context_menu.is_some() {
                node.add_action(Action::ShowContextMenu);
            }
            let on_value = aria.is_some_and(|a| a.on_value.is_some());
            if valued && (n.on.key.is_some() || on_value) {
                node.add_action(Action::Increment);
                node.add_action(Action::Decrement);
            }
            if on_value {
                node.add_action(Action::SetValue);
            }
        }
        if scrolled {
            node.add_action(Action::ScrollIntoView);
        }
        if n.scroll.is_some() {
            let (offset, max) = with_rt(|rt| rt.scrolls.get(&n.id).map_or((0.0, 0.0), |s| (s.offset, s.max_offset())));
            node.set_scroll_y(offset as f64 * self.scale);
            node.set_scroll_y_min(0.0);
            node.set_scroll_y_max(max as f64 * self.scale);
            // What has scrolled out of view steps out of the tree, leaving one on each side to step onto.
            node.set_clips_children();
            node.add_action(Action::ScrollUp);
            node.add_action(Action::ScrollDown);
            node.add_action(Action::SetScrollOffset);
        }

        node.set_children(kids);
        let at = self.out.len();
        if let Some(a) = aria {
            for (relation, ids) in [(Relation::Controls, &a.controls), (Relation::LabelledBy, &a.labelled_by), (Relation::DescribedBy, &a.described_by)] {
                if !ids.is_empty() {
                    self.relations.push((at, relation, ids.clone()));
                }
            }
        }
        let popups: Vec<u64> = frame.portals.iter().filter(|p| p.portal.anchor == Some(n.id)).map(|p| frame.nodes[p.root].id).collect();
        if !popups.is_empty() {
            self.relations.push((at, Relation::Controls, popups));
        }
        self.out.push((id, node));
        into.push(NodeId(id));
    }

    /// A control whose popover is up says so, without each trigger having to.
    fn popup_state(&self, node: &mut AxNode, n: &Node<A>, aria: Option<&Aria<A>>) {
        let Some(portal) = self.frame.portals.iter().find(|p| p.portal.anchor == Some(n.id)) else { return };
        if aria.and_then(|a| a.expanded).is_none() {
            node.set_expanded(true);
        }
        if aria.and_then(|a| a.has_popup).is_none() {
            match self.frame.nodes[portal.root].aria.as_deref().and_then(|a| a.role) {
                Some(Role::Menu) => node.set_has_popup(HasPopup::Menu),
                Some(Role::ListBox) => node.set_has_popup(HasPopup::Listbox),
                Some(Role::Dialog) => node.set_has_popup(HasPopup::Dialog),
                _ => {}
            }
        }
    }

    /// A `TextEdit` a custom leaf paints under a focus id that is not the leaf's own: a node beside what the leaf says of itself.
    fn emit_foreign_inputs(&mut self, n: &'a Node<A>, into: &mut Vec<NodeId>, scrolled: bool) {
        let painted = self.painted;
        for input in painted.iter().filter(|input| input.painter == n.id && hash_key(input.edit) != n.id) {
            let id = self.claim(hash_key(input.edit));
            let mut node = AxNode::new(input_role(input));
            let r = input.rect.unwrap_or(n.rect);
            node.set_bounds(self.bounds(r));
            let mut kids = Vec::new();
            self.fill_text_input(&mut node, id, input, r, &mut kids);
            if scrolled {
                node.add_action(Action::ScrollIntoView);
            }
            node.set_children(kids);
            self.out.push((id, node));
            into.push(NodeId(id));
        }
    }

    /// What a leaf said it paints.
    fn emit_children(&mut self, n: &Node<A>, children: &[AriaChild<A>], into: &mut Vec<NodeId>, scrolled: bool) {
        for child in children {
            let id = self.claim(child_id(n.id, child));
            let mut node = AxNode::new(child.role);
            node.set_bounds(self.bounds(child_rect(n, child)));
            if let Some(words) = &child.label {
                node.set_label(&**words);
            }
            if let Some(words) = &child.value {
                node.set_value(&**words);
            }
            if let Some(words) = &child.description {
                node.set_description(&**words);
            }
            if let Some(url) = &child.url {
                node.set_url(&**url);
            }
            if let Some(level) = child.level {
                node.set_level(level);
            }
            if let Some(on) = child.expanded {
                node.set_expanded(on);
            }
            if let Some(state) = child.toggled {
                node.set_toggled(state);
            }
            if child.disabled {
                node.set_disabled();
            }
            if child.on_press.is_some() {
                node.add_action(Action::Click);
                if child.expanded.is_some() {
                    node.add_action(Action::Expand);
                    node.add_action(Action::Collapse);
                }
            }
            if scrolled {
                node.add_action(Action::ScrollIntoView);
            }
            let mut kids = Vec::new();
            self.emit_children(n, &child.children, &mut kids, scrolled);
            node.set_children(kids);
            self.out.push((id, node));
            into.push(NodeId(id));
        }
    }

    /// The field's value, caret and selection. AccessKit reads text through runs: one per line, each knowing how long its
    /// characters are, and a position is a run and a character in it.
    fn fill_text_input(&mut self, node: &mut AxNode, id: u64, input: &TextInput, r: Rect, kids: &mut Vec<NodeId>) {
        node.set_value(input.value.as_str());
        if !input.placeholder.is_empty() {
            node.set_placeholder(&*input.placeholder);
        }
        if input.read_only {
            node.set_read_only();
        } else {
            node.add_action(Action::SetValue);
            node.add_action(Action::ReplaceSelectedText);
            node.add_action(Action::Focus);
        }
        node.add_action(Action::SetTextSelection);
        self.inputs.insert(id, input.edit);

        let mut rest = input.value.as_str();
        let mut before = 0;
        let mut line = 0;
        let (mut anchor, mut focus) = (None, None);
        loop {
            let (head, tail) = match rest.find('\n') {
                Some(at) if input.multiline => (&rest[..=at], Some(&rest[at + 1..])),
                _ => (rest, None),
            };
            let run_id = mix_id(id, line);
            let mut run = AxNode::new(Role::TextRun);
            let lengths: Vec<u8> = head.chars().map(|c| c.len_utf8() as u8).collect();
            let count = lengths.len();
            run.set_value(head);
            run.set_character_lengths(lengths);
            run.set_text_direction(TextDirection::LeftToRight);
            run.set_bounds(self.bounds(input.lines.get(line).copied().unwrap_or(r)));
            let last = tail.is_none();
            // A position at a line end is the start of the next line, except at the very end.
            let place = |at: usize| (at >= before && (at < before + count || last)).then_some(TextPosition { node: NodeId(run_id), character_index: at - before });
            anchor = anchor.or(place(input.anchor));
            focus = focus.or(place(input.caret));
            self.runs.insert(run_id, (input.edit, before));
            self.seen.insert(run_id);
            self.out.push((run_id, run));
            kids.push(NodeId(run_id));
            before += count;
            line += 1;
            match tail {
                Some(tail) => rest = tail,
                None => break,
            }
        }
        if let (Some(anchor), Some(focus)) = (anchor, focus) {
            node.set_text_selection(TextSelection { anchor, focus });
        }
    }

    fn link_relations(&mut self) {
        for (at, relation, ids) in std::mem::take(&mut self.relations) {
            let ids: Vec<NodeId> = ids.into_iter().filter(|id| self.seen.contains(id)).map(NodeId).collect();
            let node = &mut self.out[at].1;
            match relation {
                Relation::Controls => ids.into_iter().for_each(|id| node.push_controlled(id)),
                Relation::LabelledBy => node.set_labelled_by(ids),
                Relation::DescribedBy => node.set_described_by(ids),
            }
        }
    }

    /// The node holding the app's focus. With nothing holding it, the topmost dialog or menu that is up is where a reader should be.
    fn focus_target(&self, focus: u64, window: u64) -> u64 {
        if focus != 0 {
            for id in [focus, hash_key(focus)] {
                if self.seen.contains(&id) {
                    return id;
                }
            }
        }
        for portal in self.frame.portals.iter().rev() {
            let id = self.frame.nodes[portal.root].id;
            let takes_it = portal.portal.modal || self.frame.nodes[portal.root].aria.as_deref().and_then(|a| a.role).is_some_and(|r| matches!(r, Role::Menu | Role::Dialog | Role::AlertDialog | Role::ListBox));
            if takes_it && self.seen.contains(&id) {
                return id;
            }
        }
        window
    }
}

fn child_id<A: 'static>(owner: u64, child: &AriaChild<A>) -> u64 {
    hash_key((owner, child.key))
}

fn child_rect<A: 'static>(n: &Node<A>, child: &AriaChild<A>) -> Rect {
    child.rect.map_or(n.rect, |r| rect(n.rect.x + r.x, n.rect.y + r.y, r.w, r.h))
}

fn find_child<'a, A: 'static>(owner: u64, children: &'a [AriaChild<A>], id: u64) -> Option<&'a AriaChild<A>> {
    children.iter().find_map(|child| if child_id(owner, child) == id { Some(child) } else { find_child(owner, &child.children, id) })
}

fn input_role(input: &TextInput) -> Role {
    if input.obscured {
        Role::PasswordInput
    } else if input.multiline {
        Role::MultilineTextInput
    } else {
        Role::TextInput
    }
}

fn spans_text(spans: &[Span]) -> String {
    let mut out = String::new();
    for s in spans {
        out.push_str(&s.text);
    }
    out
}

/// The frame's tree, once everything has been painted and every field has said what it holds.
pub(super) fn finish_frame<A: 'static>(frame: &Frame<A>, root: usize, focus: u64, window: Rect, scale: f32) {
    STATE.with(|cell| {
        let mut guard = cell.borrow_mut();
        let st = &mut *guard;
        let painted = std::mem::take(&mut st.painted);
        let mut b = Builder { frame, painted: &painted, scale: scale as f64, out: Vec::with_capacity(frame.nodes.len() / 4), seen: HashSet::new(), repeats: HashMap::new(), aliases: HashMap::new(), relations: Vec::new(), inputs: HashMap::new(), runs: HashMap::new() };

        // A portal is a child of the window wherever it was written, in paint order, as it was of `<body>` upstream. A modal hides
        // whatever lies behind it, and what opened over it stays.
        let modal = frame.portals.iter().rposition(|p| p.portal.modal);
        let window_id = window_id();
        b.seen.insert(window_id);
        let mut top = Vec::new();
        if modal.is_none() {
            b.emit(root, &mut top, false);
        }
        for portal in &frame.portals[modal.unwrap_or(0)..] {
            b.emit(portal.root, &mut top, false);
        }
        // A new node each time, because a live region speaks when a node arrives or changes: the same words again would be neither. The
        // next frame takes it away, and brings the next one: a platform given two at once reads them in no particular order.
        let said = !st.announcements.is_empty();
        if said {
            let (message, live) = st.announcements.remove(0);
            st.announced += 1;
            let id = hash_key(("a11y-announcement", st.announced));
            let mut node = AxNode::new(Role::Label);
            node.set_value(&*message);
            node.set_live(live);
            b.seen.insert(id);
            b.out.push((id, node));
            top.push(NodeId(id));
        }
        let mut window_node = AxNode::new(Role::Window);
        window_node.set_bounds(b.bounds(window));
        window_node.set_children(top);
        b.out.push((window_id, window_node));
        b.link_relations();

        let focus_id = b.focus_target(focus, window_id);
        let Builder { out, inputs, runs, aliases, .. } = b;
        st.inputs = inputs;
        st.runs = runs;
        st.aliases = aliases;

        // An update nobody took is folded into this one by sending everything, which is always allowed.
        let whole = st.sent.is_empty() || st.update.is_some();
        let mut next = HashMap::with_capacity(out.len());
        let mut changed = Vec::new();
        for (id, node) in out {
            if whole || st.sent.get(&id) != Some(&node) {
                changed.push((NodeId(id), node.clone()));
            }
            next.insert(id, node);
        }
        st.sent = next;
        if whole || !changed.is_empty() || focus_id != st.sent_focus {
            st.sent_focus = focus_id;
            st.update = Some(TreeUpdate { nodes: changed, tree: whole.then(|| TreeInfo::new(NodeId(window_id))), tree_id: TreeId::ROOT, focus: NodeId(focus_id) });
        }
        st.edits.clear();
        if said {
            repaint();
        }
    });
}

/// What a screen reader asked for since the last frame, through the handlers a pointer and a keyboard reach. Runs before painting, so
/// a field sees what it was told this frame.
pub(super) fn dispatch_actions<A: 'static>(frame: &Frame<A>, ui: &mut Ui<'_>, fired: &mut Fired<A>) {
    let requests = STATE.with(|s| std::mem::take(&mut s.borrow_mut().actions));
    for request in requests {
        if request.target_tree != TreeId::ROOT {
            continue;
        }
        let id = request.target_node.0;
        if id == window_id() {
            if request.action == Action::Focus {
                ui.mem.focus = 0;
            }
            continue;
        }
        if text_action(&request, id, ui) {
            continue;
        }
        let (el, nth) = STATE.with(|s| s.borrow().aliases.get(&id).copied()).unwrap_or((id, 0));
        let Some((idx, n)) = frame.nodes.iter().enumerate().filter(|(_, n)| n.id == el).nth(nth) else {
            child_action(frame, el, &request, fired);
            continue;
        };
        if aria_disabled(n) {
            continue;
        }
        let aria = n.aria.as_deref();
        match request.action {
            Action::Click => click(frame, idx, fired),
            // Only what asks to be told: a toggle already in the state asked for stays there.
            Action::Expand | Action::Collapse => {
                if aria.and_then(|a| a.expanded) != Some(request.action == Action::Expand) {
                    click(frame, idx, fired);
                }
            }
            Action::Focus => ui.mem.focus = n.id,
            Action::Blur => {
                if ui.mem.focus == n.id {
                    ui.mem.focus = 0;
                }
            }
            Action::ShowContextMenu => {
                if let Some(f) = n.on.context_menu.clone() {
                    let e = at_center(Phase::Down, n.rect);
                    fired.push(Box::new(move |a| f(a, e)));
                }
            }
            Action::Increment => step(n, 1.0, fired),
            Action::Decrement => step(n, -1.0, fired),
            Action::SetValue => {
                let value = match &request.data {
                    Some(ActionData::NumericValue(v)) => Some(*v),
                    Some(ActionData::Value(text)) => text.trim().parse().ok(),
                    _ => None,
                };
                if let (Some(value), Some(f)) = (value, aria.and_then(|a| a.on_value.clone())) {
                    fired.push(Box::new(move |a| f(a, value)));
                }
            }
            Action::ScrollIntoView => reveal(frame, idx),
            Action::ScrollUp | Action::ScrollDown => {
                let (viewport, line) = with_rt(|rt| rt.scrolls.get(&n.id).map_or((0.0, 40.0), |s| (s.viewport, 40.0)));
                let by = if matches!(request.data, Some(ActionData::ScrollUnit(ScrollUnit::Item))) { line } else { viewport * 0.9 };
                with_rt(|rt| rt.scroll_requests.push((n.id, ScrollRequest::By(if request.action == Action::ScrollUp { -by } else { by }))));
                repaint();
            }
            Action::SetScrollOffset => {
                if let Some(ActionData::SetScrollOffset(point)) = &request.data {
                    let y = (point.y / ui.scale as f64) as f32;
                    with_rt(|rt| rt.scroll_requests.push((n.id, ScrollRequest::To(y))));
                    repaint();
                }
            }
            _ => {}
        }
    }
}

/// A request aimed at something a leaf paints.
fn child_action<A: 'static>(frame: &Frame<A>, id: u64, request: &ActionRequest, fired: &mut Fired<A>) {
    for (idx, n) in frame.nodes.iter().enumerate() {
        let Some(child) = n.aria.as_deref().and_then(|a| a.children.as_deref()).and_then(|children| find_child(n.id, children, id)) else { continue };
        let pressed = match request.action {
            Action::Click => true,
            // Only what asks to be told: a toggle already in the state asked for stays there.
            Action::Expand | Action::Collapse => child.expanded != Some(request.action == Action::Expand),
            Action::ScrollIntoView => {
                reveal_rect(frame, idx, child_rect(n, child));
                false
            }
            _ => false,
        };
        if pressed && let Some(f) = child.on_press.clone() {
            dismiss_outside(frame, idx, fired);
            fired.push(Box::new(move |a| f(a)));
        }
        return;
    }
}

/// A request aimed at a field: focus goes to the id it holds focus under, and an edit waits for it to paint.
fn text_action(request: &ActionRequest, id: u64, ui: &mut Ui<'_>) -> bool {
    let edit = STATE.with(|s| s.borrow().inputs.get(&id).copied());
    let Some(edit) = edit else {
        return false;
    };
    let wanted = match (&request.action, &request.data) {
        (Action::Focus, _) => {
            ui.mem.focus = edit;
            return true;
        }
        (Action::Blur, _) => {
            if ui.mem.focus == edit {
                ui.mem.focus = 0;
            }
            return true;
        }
        (Action::SetValue, Some(ActionData::Value(text))) => Some(A11yEdit::SetValue(text.to_string())),
        (Action::ReplaceSelectedText, Some(ActionData::Value(text))) => Some(A11yEdit::ReplaceSelected(text.to_string())),
        (Action::SetTextSelection, Some(ActionData::SetTextSelection(selection))) => STATE.with(|s| {
            let s = s.borrow();
            let chars = |p: &TextPosition| s.runs.get(&p.node.0).map(|(_, before)| before + p.character_index);
            Some(A11yEdit::Select { anchor: chars(&selection.anchor)?, focus: chars(&selection.focus)? })
        }),
        // Not one of a field's: let the element's own have it, a scroll into view say.
        _ => return false,
    };
    if let Some(wanted) = wanted {
        STATE.with(|s| s.borrow_mut().edits.push((edit, wanted)));
    }
    true
}

fn at_center(phase: Phase, r: Rect) -> PointerEvent {
    PointerEvent { phase, x: r.x + r.w * 0.5, y: r.y + r.h * 0.5, dx: 0.0, dy: 0.0, rect: r }
}

/// A press outside a popover closes it, and anything else the reader pressed is outside it.
fn dismiss_outside<A: 'static>(frame: &Frame<A>, idx: usize, fired: &mut Fired<A>) {
    let n = &frame.nodes[idx];
    let mut path = Vec::new();
    let mut at = idx;
    while at != NONE {
        path.push(frame.nodes[at].id);
        at = frame.nodes[at].parent;
    }
    for portal in frame.portals.iter().rev() {
        if n.portal_root == portal.root || portal.portal.modal {
            break;
        }
        let on_anchor = portal.portal.anchor.is_some_and(|a| path.contains(&a));
        if let Some(f) = portal.portal.on_dismiss.clone()
            && !on_anchor
        {
            fired.push(Box::new(move |a| f(a)));
        }
    }
}

/// What a press and a release in the middle of the element would have done.
fn click<A: 'static>(frame: &Frame<A>, idx: usize, fired: &mut Fired<A>) {
    let n = &frame.nodes[idx];
    dismiss_outside(frame, idx, fired);
    if let Some(f) = n.on.pointer.clone() {
        let (down, up) = (at_center(Phase::Down, n.rect), at_center(Phase::Up, n.rect));
        fired.push(Box::new(move |a| {
            f(a, down);
            f(a, up);
        }));
    }
    if let Some(f) = n.on.click.clone().or_else(|| n.on.double_click.clone()) {
        fired.push(Box::new(move |a| f(a)));
    }
}

/// An arrow key is how a slider is stepped by hand. A slider with no keys moves a twentieth of its range through its value handler.
fn step<A: 'static>(n: &Node<A>, direction: f64, fired: &mut Fired<A>) {
    if let Some(f) = n.on.key.clone() {
        let press = |key| KeyPress { key, shift: false, cmd: false, alt: false, ctrl: false };
        let (first, second) = if direction > 0.0 { (press(Key::Right), press(Key::Up)) } else { (press(Key::Left), press(Key::Down)) };
        fired.push(Box::new(move |a| {
            if !f(a, &first) {
                f(a, &second);
            }
        }));
    } else if let Some((f, (now, min, max))) = n.aria.as_deref().and_then(|a| Some((a.on_value.clone()?, a.value?))) {
        let next = (now + direction * (max - min) / 20.0).clamp(min, max);
        fired.push(Box::new(move |a| f(a, next)));
    }
}

/// Every scroller around the element brings it into view.
fn reveal<A: 'static>(frame: &Frame<A>, idx: usize) {
    reveal_rect(frame, idx, frame.nodes[idx].rect);
}

fn reveal_rect<A: 'static>(frame: &Frame<A>, idx: usize, r: Rect) {
    let mut at = frame.nodes[idx].parent;
    while at != NONE {
        let n = &frame.nodes[at];
        if n.scroll.is_some() {
            with_rt(|rt| {
                let offset = rt.scrolls.get(&n.id).map_or(0.0, |s| s.offset);
                rt.scroll_requests.push((n.id, ScrollRequest::Reveal { y: r.y - n.rect.y + offset, h: r.h }));
                rt.repaint = true;
            });
        }
        at = n.parent;
    }
}
