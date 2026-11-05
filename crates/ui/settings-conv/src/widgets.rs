//! The small pieces every page of the pane draws with: `Common.tsx`'s `Field`, `IconButton` and `ConfirmDeleteButton`, the cache tone,
//! `DocsLink.tsx`, `SettingsLayout.tsx`, `SettingsPageHeading.tsx` and `PopoverMenu.tsx`'s select shape.

use std::cell::{Cell, RefCell};
use std::hash::Hash;

use mewrk_client::prelude::*;

use crate::lock::{lock_hint, LockTone};
use crate::logic::docs_href;

/// There is no DOM to ask what a frame said, so scripted tests read what the views handed to the text layer.
pub mod probe {
    use super::*;

    thread_local! {
        static ON: Cell<bool> = const { Cell::new(false) };
        static LOG: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    pub fn enable() {
        ON.with(|on| on.set(true));
    }

    pub fn clear() {
        LOG.with(|l| l.borrow_mut().clear());
    }

    /// Everything said since the last `clear`.
    pub fn said() -> Vec<String> {
        LOG.with(|l| l.borrow().clone())
    }

    pub fn say(s: &str) {
        if ON.with(Cell::get) {
            LOG.with(|l| l.borrow_mut().push(s.to_owned()));
        }
    }
}

pub use probe::say;

/// Plain text, as the page's type ramp draws most of it.
pub fn label(s: impl Into<Arc<str>>) -> E {
    let s: Arc<str> = s.into();
    say(&s);
    text(s).ty(BODY_SMALL)
}

pub fn small(s: impl Into<Arc<str>>) -> E {
    let s: Arc<str> = s.into();
    say(&s);
    text(s).ty(FINE).color(theme().text_tertiary)
}

pub fn strong(s: impl Into<Arc<str>>) -> E {
    let s: Arc<str> = s.into();
    say(&s);
    text(s).ty(BODY_SMALL_STRONG)
}

/// A hint coloured as a complaint.
pub fn error_text(s: impl Into<Arc<str>>) -> E {
    let s: Arc<str> = s.into();
    say(&s);
    text(s).ty(FINE).color(theme().critical)
}

/// What a reader says for an element, which tests can read as well. Nothing is drawn for it.
pub trait Tip {
    fn tip(self, s: impl Into<Arc<str>>) -> Self;
}

impl Tip for E {
    fn tip(self, s: impl Into<Arc<str>>) -> Self {
        let s: Arc<str> = s.into();
        say(&s);
        self.tooltip(s)
    }
}

/// Upstream's `IconButton`. The label is its name to a reader, and the row's actions are told apart by it.
pub fn icon_btn(key: impl Hash, name: &'static str, tip: impl Into<Arc<str>>) -> E {
    let tip: Arc<str> = tip.into();
    say(&tip);
    icon_button::<App>(name, tip).size(24.0, 24.0).radius(5.0).key(key)
}

/// `Field`: a label, its control, and the line under it.
pub fn field_block(title: impl Into<Arc<str>>, control: E, hint: Option<Hint>) -> E {
    let title: Arc<str> = title.into();
    say(&title);
    col().gap(6.0).mb(0.0).min_w(0.0).child(text(title).ty(CAPTION_STRONG)).child(control).child(hint.map(Hint::view))
}

/// What is said under a field: standing advice or a complaint.
#[derive(Clone, Default)]
pub struct Hint {
    pub text: String,
    pub error: bool,
    pub docs: Option<(&'static str, Option<&'static str>)>,
}

impl Hint {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into(), ..Self::default() }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self { text: text.into(), error: true, ..Self::default() }
    }

    pub fn docs(mut self, page: &'static str, section: &'static str) -> Self {
        self.docs = Some((page, Some(section)));
        self
    }

    pub fn view(self) -> E {
        col()
            .gap(2.0)
            .min_w(0.0)
            .when(!self.text.is_empty() || self.docs.is_some(), |e| {
                let line = if self.error { error_text(self.text.clone()) } else { small(self.text.clone()) };
                e.child(row().wrap().items_center().gap(4.0).min_w(0.0).child(line).child(self.docs.map(|(p, s)| docs_inline(p, s))))
            })
    }
}

/// Opens an address in the system browser.
pub fn open_url(app: &mut App, url: String) {
    app.invoke_logged("open_external_url", json!({ "url": url }));
}

/// `DocsLink inline`: the "Learn more" that trails a short description.
pub fn docs_inline(page: &'static str, section: Option<&'static str>) -> E {
    let t_ = theme();
    let href = docs_href(page, section);
    say(&format!("docs:{href}"));
    let label_text = t("详情", "Learn more");
    say(label_text);
    // Several rows of a page can lead to one section, and two elements under one key are one Tab stop to the keyboard, so these go by where they are.
    row()
        .items_center()
        .gap(2.0)
        .cursor(CursorKind::Hand)
        .role(Role::Link)
        .on_click(move |app: &mut App| open_url(app, href.clone()))
        .child(text(label_text).ty(FINE).color(t_.accent_text).nowrap())
        .child(icon("ExternalLink", 9.0).color(t_.accent_text))
}

/// `DocsLink`: the one-line "Configuration docs" a page's heading row ends with.
pub fn docs_link(page: &'static str, section: Option<&'static str>) -> E {
    let t_ = theme();
    let href = docs_href(page, section);
    say(&format!("docs:{href}"));
    let label_text = t("配置说明文档", "Configuration docs");
    say(label_text);
    row()
        .key(("docs-link", page, section.unwrap_or("")))
        .items_center()
        .gap(4.0)
        .shrink0()
        .cursor(CursorKind::Hand)
        .role(Role::Link)
        .on_click(move |app: &mut App| open_url(app, href.clone()))
        .child(icon("ExternalLink", 11.0).color(t_.accent_text))
        .child(text(label_text).ty(FINE).color(t_.accent_text).nowrap())
}

/// The gap between the items of a settings page.
pub const SETTING_ROW_GAP: f32 = 4.0;
/// How narrow a row's text may get before its control drops to a line of its own.
const SETTING_LEAD_MIN: f32 = 160.0;

/// One item of a settings page: what it is on the left, its control on the right. Every item has this shape, so a page's
/// items sit alike whatever they control.
pub fn setting_row(key: impl Hash, lead: E, control: E) -> E {
    row().key(key).wrap().items_center().justify_end().gap(9.0).min_h(48.0).py(6.0).px(4.0).radius(6.0).child(lead.flex1().min_w(SETTING_LEAD_MIN)).child(control.min_w(0.0))
}

/// A title with its line under it, for [`setting_row`].
pub fn setting_lead(title: impl Into<Arc<str>>, hint: Hint) -> E {
    col().gap(1.0).child(strong(title)).child(hint.view())
}

/// One choice of a [`segmented`] group.
pub struct Segment<V> {
    pub id: &'static str,
    pub value: V,
    pub label: &'static str,
    /// What choosing it means, to a reader; or why it can't be chosen, which is drawn on hover too.
    pub tip: Arc<str>,
    /// On show but not on offer.
    pub disabled: bool,
}

/// Buttons side by side, exactly one of them chosen. Each is keyed `(key.0, key.1, id)`.
pub fn segmented<V: Copy + PartialEq + 'static>(key: (&'static str, &str), options: Vec<Segment<V>>, selected: V, tone: Option<LockTone>, on_select: Rc<dyn Fn(&mut App, V)>) -> E {
    let t_ = theme();
    let tint = cache_tint(tone);
    let mut group = row().p(2.0).gap(2.0).radius(CONTROL_RADIUS + 2.0).bg(t_.subtle_hover).role(Role::RadioGroup);
    for option in options {
        let active = option.value == selected;
        let (value, f) = (option.value, on_select.clone());
        say(option.label);
        group = group.child(
            row()
                .key((key.0, key.1, option.id))
                .h(24.0)
                .px(9.0)
                .grow()
                .center()
                .radius(CONTROL_RADIUS)
                .when(active, |e| match tint {
                    Some(tint) => e.bg(tint),
                    None => e.bg(t_.control_fill).border(t_.control_stroke),
                })
                .when(!active, |e| e.hover_bg(t_.subtle_pressed))
                .tip(option.tip.clone())
                .when(option.disabled, |e| e.hover_text(option.tip))
                .disabled(option.disabled)
                .role(Role::RadioButton)
                .aria_checked(active)
                .on_click(move |app: &mut App| {
                    if !active {
                        f(app, value);
                    }
                })
                .child(text(option.label).ty(if active { CAPTION_STRONG } else { CAPTION }).color(match (active, tint) {
                    (true, Some(_)) => t_.text_on_accent,
                    (true, None) => t_.text_primary,
                    _ => t_.text_secondary,
                }).nowrap()),
        );
    }
    group
}

/// The colour a toned control takes in place of its main one. Nothing else about a control says the cache is warm; what it means is in the warning it moves through.
pub fn cache_tint(tone: Option<LockTone>) -> Option<Color> {
    tone.map(|_| {
        say("cache-orange");
        theme().cache_warm
    })
}

/// [`cache_tint`] for a control that is a filled row.
pub fn cache_fill(tone: Option<LockTone>) -> Option<Color> {
    tone.map(|_| {
        say("cache-orange");
        theme().cache_warm_bg
    })
}

/// `LockableSwitchRow`: a title and a line saying what the switch does. An orange switch moves through the caller's cache-break warning. A row held for a reason of its own says that reason under the line.
pub struct SwitchRow {
    pub key: String,
    pub title: String,
    pub description: String,
    pub docs: Option<(&'static str, Option<&'static str>)>,
    pub checked: bool,
    /// The switch's name, which says its state.
    pub label: String,
    pub tone: Option<LockTone>,
    pub disabled: bool,
    pub disabled_note: Option<String>,
    /// Drawn just before the switch, for what the switch turns on.
    pub control: Option<E>,
    pub on_change: Rc<dyn Fn(&mut App, bool)>,
}

impl SwitchRow {
    pub fn view(self) -> E {
        let t_ = theme();
        let shown_tone = if self.disabled { None } else { self.tone };
        let note = if self.disabled { self.disabled_note } else { None };
        let checked = self.checked;
        let on_change = self.on_change;
        say(&self.label);
        let name: Arc<str> = Arc::from(self.label.as_str());
        let toggle = move |app: &mut App| on_change(app, !checked);
        let sw = match cache_tint(shown_tone) {
            Some(tint) => tinted_switch(checked, tint, toggle),
            None => switch(checked, toggle),
        };
        let tip = match shown_tone {
            Some(tone) => format!("{}\n{}", self.label, lock_hint(tone)),
            None => self.label,
        };
        let sw = sw.key(("switch", self.key.as_str())).tip(tip).aria_label(name).disabled(self.disabled).shrink0();
        let lead = col()
            .gap(1.0)
            .child(strong(self.title))
            .child(row().wrap().items_center().gap(4.0).min_w(0.0).child(small(self.description)).child(self.docs.map(|(p, s)| docs_inline(p, s))))
            .child(note.map(|n| small(n).color(t_.text_secondary)));
        let controls = row().items_center().gap(9.0).child(self.control).child(sw);
        setting_row(("switch-row", self.key.as_str()), lead, controls).when(self.disabled, |e| e.opacity(0.6))
    }
}

/// A trigger with a menu under it, for every select on these pages. It is not the shared `select` because a trigger may say what no row
/// says ("Repair search provider", a dead model binding), carry a lock, read as an error, and a row may open a second step.
pub struct SelectBox {
    pub key: String,
    pub trigger: String,
    /// Said small beside the value: the native tool version.
    pub note: Option<String>,
    pub tone: Option<LockTone>,
    pub error: bool,
    pub disabled: bool,
    pub rows: Vec<SelectRow>,
    pub empty_label: Option<String>,
}

pub struct SelectRow {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub checked: bool,
    pub hint: Option<String>,
    pub enabled: bool,
    pub on_select: Option<Rc<dyn Fn(&mut App)>>,
    /// The second step this row opens into.
    pub children: Vec<SelectRow>,
}

impl SelectRow {
    pub fn new(id: impl Into<String>, label: impl Into<String>, on_select: impl Fn(&mut App) + 'static) -> Self {
        Self { id: id.into(), label: label.into(), description: None, checked: false, hint: None, enabled: true, on_select: Some(Rc::new(on_select)), children: Vec::new() }
    }

    pub fn checked(mut self, on: bool) -> Self {
        self.checked = on;
        self
    }
}

fn select_rows(key: &str, depth: usize, rows: Vec<SelectRow>, close_all: Rc<dyn Fn(&mut App)>) -> E {
    let t_ = theme();
    let mut panel = flyout_panel::<App>().min_w(180.0).max_w(420.0).p(4.0).max_h(320.0).scroll_y().key(("select-panel", key, depth)).role(Role::Menu);
    let open_sub = use_state(("select-sub", key, depth), || None::<String>);
    for r in rows {
        say(&r.label);
        let fg = if r.enabled { t_.text_primary } else { t_.text_disabled };
        let row_key = ("select-row", key.to_owned(), r.id.clone());
        let mut el = row().key(row_key.clone()).items_center().gap(8.0).min_h(30.0).px(8.0).radius(CONTROL_RADIUS).shrink0().aria_disabled(!r.enabled);
        el = if r.children.is_empty() { el.role(Role::MenuItemRadio).aria_checked(r.checked) } else { el.role(Role::MenuItem).aria_haspopup(HasPopup::Menu).aria_expanded(open_sub.with(|v| v.as_deref() == Some(r.id.as_str()))) };
        if r.enabled {
            el = el.hover_bg(t_.subtle_hover).press_bg(t_.subtle_pressed);
            if r.children.is_empty() {
                if let Some(f) = r.on_select.clone() {
                    let close = close_all.clone();
                    el = el.on_click(move |app: &mut App| {
                        close(app);
                        f(app);
                    });
                }
            } else {
                let id = r.id.clone();
                let sub = open_sub.clone();
                el = el.on_click(move |_| sub.update(|v| *v = if v.as_deref() == Some(id.as_str()) { None } else { Some(id.clone()) }));
            }
        }
        el = el.child(
            col()
                .flex1()
                .min_w(0.0)
                .py(4.0)
                .child(text(r.label).ty(BODY_SMALL).color(fg).ellipsis())
                .child(r.description.map(|d| {
                    say(&d);
                    text(d).ty(FINE).color(t_.text_tertiary).ellipsis()
                })),
        );
        if let Some(h) = r.hint {
            say(&h);
            el = el.child(text(h).ty(FINE).color(t_.text_tertiary).nowrap().ml(16.0));
        }
        if r.checked {
            el = el.child(icon("Check", 12.0).color(t_.accent));
        }
        if !r.children.is_empty() {
            el = el.child(icon("ChevronRight", 12.0).color(t_.text_tertiary));
            if open_sub.with(|v| v.as_deref() == Some(r.id.as_str())) {
                let sub = open_sub.clone();
                let inner = select_rows(&format!("{key}/{}", r.id), depth + 1, r.children, close_all.clone());
                el = el.child(inner.popover(row_key, Side::Right, Align::Start, 4.0, move |_| sub.set(None)));
            }
        }
        panel = panel.child(el);
    }
    panel
}

impl SelectBox {
    pub fn new(key: impl Into<String>, trigger: impl Into<String>, rows: Vec<SelectRow>) -> Self {
        Self { key: key.into(), trigger: trigger.into(), note: None, tone: None, error: false, disabled: false, rows, empty_label: None }
    }

    pub fn view(self) -> E {
        let t_ = theme();
        let open = use_state(("select-box", self.key.as_str()), || false);
        let is_open = open.get();
        let anchor = ("select-box-anchor", self.key.clone());
        let toggle = open.clone();
        say(&self.trigger);
        if let Some(n) = &self.note {
            say(n);
        }
        // A closed menu still says its rows to the tests' reader, as an option list in the DOM would.
        for r in &self.rows {
            say(&format!("option:{}", r.label));
            for c in &r.children {
                say(&format!("option:{}", c.label));
            }
        }
        let stroke = if self.error {
            t_.critical
        } else if let Some(tint) = cache_tint(self.tone) {
            tint
        } else {
            t_.control_stroke
        };
        let mut trigger = row()
            .key(anchor.clone())
            .items_center()
            .justify_between()
            .gap(8.0)
            .min_h(30.0)
            .min_w(0.0)
            .px(10.0)
            .radius(CONTROL_RADIUS)
            .bg(t_.control_fill)
            .hover_bg(t_.control_fill_hover)
            .border(stroke)
            .disabled(self.disabled)
            .when(self.disabled, |e| e.opacity(0.6))
            .role(Role::ComboBox)
            .aria_haspopup(HasPopup::Menu)
            .aria_expanded(is_open && !self.disabled)
            .aria_invalid(self.error)
            .on_click(move |_| toggle.update(|v| *v = !*v))
            .child(text(self.trigger).ty(BODY_SMALL).ellipsis().flex1())
            .child(self.note.map(|n| text(n).ty(FINE).color(t_.text_tertiary).nowrap()))
            .child(icon("ChevronDown", 12.0).color(t_.text_secondary));
        if is_open && !self.disabled {
            let closer = open.clone();
            let close: Rc<dyn Fn(&mut App)> = Rc::new(move |_| closer.set(false));
            let dismiss = open.clone();
            let panel = if self.rows.is_empty() {
                let empty = self.empty_label.unwrap_or_default();
                flyout_panel::<App>().p(8.0).child(small(empty))
            } else {
                select_rows(&self.key, 0, self.rows, close)
            };
            trigger = trigger.child(panel.popover(anchor, Side::Below, Align::Start, 4.0, move |_| dismiss.set(false)));
        }
        trigger
    }
}

/// Two clicks to delete: the first arms it and says so, a press elsewhere or Escape disarms it. Rows that are themselves clickable sit under
/// this button, so arming one is not a request to open what is being deleted.
pub fn confirm_delete_button(key: impl Hash, disabled: bool, label_text: String, confirm_label: String, on_delete: Rc<dyn Fn(&mut App)>) -> E {
    let t_ = theme();
    let key = hash_key(key);
    let armed = use_state(("confirm-delete", key), || false);
    let is_armed = armed.get();
    // What the armed button says it will do, rather than the "Confirm" it shows.
    let current: Arc<str> = Arc::from(if is_armed { confirm_label } else { label_text });
    say(&current);
    let arm = armed.clone();
    let disarm = armed.clone();
    let base = if is_armed {
        let word = t("确认", "Confirm");
        say(word);
        row().items_center().justify_center().h(24.0).px(6.0).radius(5.0).bg(t_.critical_bg).child(text(word).ty(FINE_STRONG).color(t_.critical))
    } else {
        row().center().size(24.0, 24.0).radius(5.0).hover_bg(t_.critical_bg).child(icon("Trash2", 13.0).color(t_.text_secondary))
    };
    let disarm_on_key = armed.clone();
    base.key(key)
        .shrink0()
        .tooltip(current.clone())
        .role(Role::Button)
        .aria_label(current)
        .disabled(disabled)
        .when(disabled, |e| e.opacity(0.5))
        .focusable()
        .on_key(move |_, press| {
            if press.key == Key::Escape && disarm_on_key.get() {
                disarm_on_key.set(false);
                return true;
            }
            false
        })
        .on_hover_change(move |_, hovering| {
            // A pointer that leaves has moved on; the blur upstream relies on has no separate moment here.
            if !hovering {
                disarm.set(false);
            }
        })
        .on_click(move |app: &mut App| {
            if !arm.get() {
                arm.set(true);
                return;
            }
            arm.set(false);
            on_delete(app);
        })
}

/// One entry of a rail: `SettingsNavigationItem`.
pub struct NavItem<V> {
    pub id: V,
    pub icon: &'static str,
    pub label: String,
    /// Trailed after the label. A zero says "nothing selected" as clearly as a number says how many, so only `None` draws nothing.
    pub count: Option<usize>,
}

/// Which of the rails a list of pages is drawn as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavStyle {
    /// The side pane's own: 150 px, and it stays a column however narrow the window is.
    Pane,
    /// The side pane dragged under 430 px: 108 px, so the body keeps a readable column.
    NarrowPane,
    /// `SettingsNavigation`, under the window's name: 190 px, 154 at `max-width: 860px`, and a strip across the top at 720.
    Window,
    /// The domain lists' aside: 178 px whatever the window's width.
    Domains,
}

/// `.settings-layout` at `max-width: 720px`: a settings window's rail becomes a strip across the top.
pub fn window_strip() -> bool {
    window_size().0 <= 720.0
}

/// The rail down a window's left edge and the side pane's compact one.
pub fn settings_nav<V: Copy + PartialEq + std::hash::Hash + 'static>(scope: &str, label: &str, items: Vec<NavItem<V>>, view: V, style: NavStyle, title: Option<String>, on_select: Rc<dyn Fn(&mut App, V)>, footer: Option<E>) -> E {
    let t_ = theme();
    let window = matches!(style, NavStyle::Window | NavStyle::Domains);
    let strip = style == NavStyle::Window && window_strip();
    let (item_h, icon_size, gap, item_px) = match style {
        NavStyle::Pane => (30.0, 14.0, 6.0, 7.0),
        NavStyle::NarrowPane => (30.0, 14.0, 5.0, 5.0),
        NavStyle::Window => (36.0, 16.0, 8.0, 9.0),
        NavStyle::Domains => (36.0, 14.0, 8.0, 9.0),
    };
    let mut rows = Vec::new();
    for item in items {
        let active = item.id == view;
        let f = on_select.clone();
        let id = item.id;
        say(&item.label);
        if let Some(count) = item.count {
            say(&format!("{} {count}", item.label));
        }
        let name = text(item.label).ty(match (window, active) {
            (true, true) => BODY_SMALL_STRONG,
            (true, false) => BODY_SMALL,
            (false, _) => CAPTION,
        }).color(if active { t_.text_primary } else { t_.text_secondary });
        rows.push(
            row()
                .key(("nav", scope, id))
                .items_center()
                .gap(gap)
                .h(item_h)
                .px(item_px)
                .radius(if window { 7.0 } else { 6.0 })
                .shrink0()
                .when(active, |e| e.bg(t_.subtle_pressed).aria_current(AriaCurrent::True))
                .hover_bg(t_.subtle_hover)
                .on_click(move |app: &mut App| f(app, id))
                .child(icon(item.icon, icon_size).color(if active { t_.text_primary } else { t_.text_secondary }))
                .child(if strip { name.nowrap() } else { name.ellipsis().flex1() })
                .child(item.count.map(|c| text(c.to_string()).ty(FINE).color(t_.text_tertiary).nowrap())),
        );
    }
    let name = title.map(|ti| {
        say(&ti);
        text(ti).ty(BODY_SMALL_STRONG).role(Role::Heading).aria_level(2)
    });
    if strip {
        // It scrolls sideways and ends short of the close button, with its foot pinned at the end so a long strip does not scroll the save out of reach.
        let strip = row().items_center().gap(2.0).min_h(36.0).role(Role::Navigation).aria_label(label).child(name.map(|n| row().px(9.0).items_center().child(n.nowrap()))).children(rows);
        return row().items_center().min_h(48.0).mr(48.0).py(6.0).px(9.0).child(sideways(("settings-strip", scope), strip)).child(footer.map(|f| row().ml_auto().shrink0().pl(9.0).gap(6.0).items_center().child(f)));
    }
    let width = match style {
        NavStyle::Pane => 150.0,
        NavStyle::NarrowPane => 108.0,
        NavStyle::Window if window_size().0 <= 860.0 => 154.0,
        NavStyle::Window => 190.0,
        NavStyle::Domains => 178.0,
    };
    let list = col().flex1().min_h(0.0).gap(2.0).scroll_y().key(("settings-nav-list", scope)).children(rows);
    let frame = if window { col().pb(11.0).px(9.0).gap(2.0).bg(t_.card_secondary) } else { col().py(9.0).px(if style == NavStyle::NarrowPane { 5.0 } else { 7.0 }).gap(8.0) };
    frame
        .w(width)
        .shrink0()
        .min_h(0.0)
        .border_right(t_.divider)
        .role(Role::Navigation)
        .aria_label(label)
        .child(name.map(|n| row().h(48.0).px(9.0).items_center().shrink0().child(n.ellipsis().flex1())))
        .child(list)
        .child(footer.map(|f| col().pt(if window { 9.0 } else { 8.0 }).gap(6.0).border_top(t_.divider).shrink0().child(f)))
}

/// `settings-nav__action` and the pane's own text button: what belongs to a whole window, at the foot of its rail.
pub fn nav_action(button: E, window: bool) -> E {
    let t_ = theme();
    let sized = if window { button.min_h(32.0).radius(7.0) } else { button.h(24.0).px(4.0).radius(6.0) };
    sized.when(!(window && window_strip()), |e| e.w_full()).border(t_.card_stroke)
}

/// The page of a settings window, on the right of its rail. `section.settings-page`: the rail names the page, so the heading says only what it is for; the whole page scrolls unless it is a
/// timeline, which brings its own scroller and gets the body flush.
pub fn window_page(key: impl Hash, description: E, action: Option<E>, note: Option<E>, body: E, flush: bool) -> E {
    let strip = window_strip();
    let heading = row().items_start().justify_between().gap(20.0).pb(16.0).pr(if strip { 0.0 } else { 30.0 }).min_w(0.0).shrink0().child(col().flex1().min_w(0.0).child(description)).child(action.map(|a| a.shrink0()));
    let page = col().flex1().min_h(0.0).min_w(0.0).pt(if strip { 21.0 } else { 14.0 }).px(22.0).child(heading).child(note.map(|n| n.mb(12.0).shrink0()));
    if flush {
        page.child(col().flex1().min_h(0.0).min_w(0.0).child(body))
    } else {
        page.key(key).scroll_y().pb(30.0).child(body)
    }
}

/// The line a window's page opens with.
pub fn window_page_blurb(s: impl Into<Arc<str>>) -> E {
    small(s).ty(CAPTION).color(theme().text_secondary)
}

/// A settings window's frame: `Dialog sidebar`, a rail on the left, the page on the right, the close button at its corner. `stacked` lays a strip of a rail over the page instead.
pub fn settings_window(key: impl Hash, title: &str, width: f32, dismissible: bool, stacked: bool, on_close: Rc<dyn Fn(&mut App)>, nav: E, page: E) -> E {
    let t_ = theme();
    let (w, h) = window_size();
    let key = hash_key(key);
    // `.modal-backdrop`'s padding, 8 px at `max-width: 720px`.
    let width = width.min(w - if window_strip() { 16.0 } else { 48.0 }).max(320.0);
    let height = (h - 48.0).clamp(240.0, 760.0);
    let backdrop: Option<Handler<App>> = if dismissible {
        let f = on_close.clone();
        Some(Rc::new(move |app: &mut App| f(app)))
    } else {
        None
    };
    let close = on_close;
    let frame = if stacked { col() } else { row() };
    let body = frame
        .key(("settings-window", key))
        .h(height)
        .min_w(0.0)
        .child(nav)
        .child(col().flex1().min_w(0.0).min_h(0.0).when(stacked, |e| e.border_top(t_.divider)).child(page))
        .child(
            icon_button::<App>("X", t("关闭", "Close"))
                .key(("settings-window-close", key))
                .absolute()
                .top(8.0)
                .right(12.0)
                .on_click(move |app: &mut App| close(app)),
        )
        .bg(t_.flyout);
    dialog(width, body, backdrop).aria_label(title)
}

/// `Dialog`: a one-line title with the way out in its corner, a body, a one-line button row, and no rules fencing them off.
pub fn plain_dialog(key: impl Hash, width: f32, title: String, description: Option<String>, body: Option<E>, footer: Vec<E>, on_close: Rc<dyn Fn(&mut App)>) -> E {
    let t_ = theme();
    let key = hash_key(key);
    say(&title);
    let close_label = tf("关闭 {title}", "Close {title}", &[("title", &title)]);
    let (close_x, close_backdrop) = (on_close.clone(), on_close);
    let backdrop = Some(Rc::new(move |app: &mut App| close_backdrop(app)) as Handler<App>);
    let described = description.is_some();
    let name: Arc<str> = Arc::from(title);
    let inner = col()
        .key(("plain-dialog", key))
        .child(row().pt(9.0).pb(8.0).pl(16.0).pr(10.0).items_center().gap(12.0).child(text(name.clone()).ty(BODY_STRONG).ellipsis().flex1().role(Role::Heading).aria_level(2)).child(icon_button::<App>("X", close_label).key(("plain-dialog-close", key)).on_click(move |app: &mut App| close_x(app))))
        .child(col().pt(4.0).px(20.0).pb(16.0).child(description.map(|d| {
            say(&d);
            text(d).ty(CAPTION).color(t_.text_secondary).mb(14.0).key(("plain-dialog-description", key))
        })).child(body))
        .child(row().pt(6.0).pb(12.0).px(12.0).justify_end().gap(8.0).children(footer));
    dialog(width.min(window_size().0 - 48.0), inner, backdrop).aria_label(name).when(described, |e| e.aria_describedby(("plain-dialog-description", key)))
}

/// A button whose label tests can read.
pub fn btn(key: impl Hash, look: Look, label_text: impl Into<Arc<str>>) -> E {
    let s: Arc<str> = label_text.into();
    say(&s);
    let el: E = match look {
        Look::Standard => button(s),
        Look::Accent => accent_button(s),
        Look::Subtle => subtle_button(s),
        Look::Danger => danger_button(s),
    };
    el.key(key)
}

#[derive(Clone, Copy)]
pub enum Look {
    Standard,
    Accent,
    Subtle,
    Danger,
}

/// A labelled checkbox row: `.confirm-check`. The label takes the click as well, as a `<label>` does.
pub fn check_row(key: impl Hash, on: bool, disabled: bool, label_text: &str, tip: Option<&str>, on_toggle: impl Fn(&mut App) + 'static) -> E {
    say(label_text);
    say(if on { "checkbox:on" } else { "checkbox:off" });
    // The row is the control, a `<label>` and its box being one stop: the box only shows its state.
    let row_el = row()
        .key(key)
        .items_center()
        .gap(8.0)
        .disabled(disabled)
        .when(disabled, |e| e.opacity(0.6))
        .role(Role::CheckBox)
        .aria_checked(on)
        .on_click(on_toggle)
        .child(checkbox(on, |_: &mut App| {}).inert().aria_hidden(true))
        .child(text(label_text.to_owned()).ty(BODY_SMALL));
    match tip {
        Some(tip) => row_el.tip(tip.to_owned()).when(disabled, |e| e.hover_text(tip.to_owned())),
        None => row_el,
    }
}
