//! The little controls every card shares: `IconButton`, `CopyButton` and friends of `Common.tsx`, sized as the timeline's
//! CSS sizes them.

use std::cell::RefCell;
use std::hash::Hash;
use std::time::{Duration, Instant};

use mewrk_client::prelude::*;

thread_local! {
    static PENDING_COPY: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// A handler has no clipboard to write to, but the next paint does. `clipboard_leaf` sits in the timeline's tree.
pub fn copy_text(text: impl Into<String>) {
    PENDING_COPY.with(|p| *p.borrow_mut() = Some(text.into()));
    repaint();
}

/// Writes what `copy_text` left, from inside a paint.
pub fn clipboard_leaf() -> E {
    custom::<App>(|_, ui, _| {
        if let Some(text) = PENDING_COPY.with(|p| p.borrow_mut().take()) {
            ui.clipboard.set(&text);
        }
    })
    .size(1.0, 1.0)
    .absolute()
    .pass_through()
}

/// A callback that belongs to the UI thread and only rides through a worker on its way back there: `invoke` and `spawn` want `Send`
/// for what they carry, and what an editor hands them holds its `Rc`s. A worker that dies first leaks it rather than dropping it there.
pub struct UiBound<T>(std::mem::ManuallyDrop<T>);

// SAFETY: the value is only taken out by `into_inner`, which every caller reaches through `App::post`, `App::invoke`'s `done` or
// `Task::post`, all of which run on the UI thread; dropped unopened on a worker it leaks instead of touching a refcount there.
unsafe impl<T> Send for UiBound<T> {}

impl<T> UiBound<T> {
    pub fn new(value: T) -> Self {
        Self(std::mem::ManuallyDrop::new(value))
    }

    pub fn into_inner(self) -> T {
        std::mem::ManuallyDrop::into_inner(self.0)
    }
}

/// `.context-actions .icon-button`: 24 px with a 13 px glyph. The label is its tooltip and its name.
pub fn action_button(key: impl Hash, name: impl Into<IconRef>, label: impl Into<Arc<str>>, on_click: impl Fn(&mut App) + 'static) -> E {
    let t = theme();
    row()
        .key(key)
        .center()
        .size(24.0, 24.0)
        .radius(6.0)
        .shrink0()
        .hover_bg(t.subtle_hover)
        .press_bg(t.subtle_pressed)
        .tooltip(label)
        .child(icon(name, 13.0).color(t.text_secondary))
        .on_click(on_click)
}

/// The same when it cannot be pressed. A reason is `disabled_reason`, chained on.
pub fn action_button_disabled(key: impl Hash, name: impl Into<IconRef>, label: impl Into<Arc<str>>) -> E {
    let t = theme();
    row().key(key).center().size(24.0, 24.0).radius(6.0).shrink0().tooltip(label).opacity(0.45).role(Role::Button).aria_disabled(true).child(icon(name, 13.0).color(t.text_tertiary))
}

/// How long a copy button says it worked.
const COPIED_FOR: Duration = Duration::from_millis(1400);

/// `CopyButton`: copies `text`, and shows a tick for a moment.
pub fn copy_button(key: impl Hash + Clone, text: &str, label: impl Into<Arc<str>>, disabled: bool) -> E {
    let t = theme();
    let copied_at = use_state(("copied", hash_key(key.clone())), || None::<Instant>);
    let copied = copied_at.get().is_some_and(|at| at.elapsed() < COPIED_FOR);
    let label: Arc<str> = label.into();
    if disabled {
        return action_button_disabled(("copy", hash_key(key)), "Copy", label);
    }
    let text = text.to_owned();
    let glyph = if copied { icon("Check", 13.0).color(t.success) } else { icon("Copy", 13.0).color(t.text_secondary) };
    let view = row().key(("copy", hash_key(key))).center().size(24.0, 24.0).radius(6.0).shrink0().hover_bg(t.subtle_hover).press_bg(t.subtle_pressed).tooltip(label).child(glyph);
    let view = view.on_click(move |_| {
        copy_text(text.clone());
        copied_at.set(Some(Instant::now()));
    });
    if copied {
        // The tick has to go away with nothing else asking for a frame.
        repaint();
    }
    view
}

/// `DocsLink inline`: the "Learn more" that trails a short line, in that line's own size.
pub fn docs_link(page: &'static str, section: &'static str) -> E {
    let t_ = theme();
    let locale = if mewrk_client::i18n::is_english() { "en" } else { "zh-CN" };
    let href = format!("https://mewrk.dev/{locale}/{page}.html#{section}");
    row()
        .key(("tl-docs", page, section))
        .items_center()
        .gap(2.0)
        .shrink0()
        .cursor(CursorKind::Hand)
        .role(Role::Link)
        .on_click(move |app: &mut App| app.invoke_logged("open_external_url", json!({ "url": href })))
        .child(text(t("详情", "Learn more")).ty(ty::NOTE).color(t_.accent_text).nowrap())
        .child(icon("ExternalLink", 11.0).color(t_.accent_text))
}

thread_local! {
    static RETAINED_FLAGS: std::cell::RefCell<std::collections::HashMap<u64, bool>> = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// A disclosure's state, which has to outlive its row: a timeline builds only what is near the view, and upstream's scrolled-away
/// rows stay mounted. Only what someone toggled is kept.
pub fn retained_flag(key: impl Hash, default: bool) -> bool {
    let key = hash_key(key);
    RETAINED_FLAGS.with(|flags| flags.borrow().get(&key).copied()).unwrap_or(default)
}

pub fn set_retained_flag(key: impl Hash, value: bool) {
    let key = hash_key(key);
    RETAINED_FLAGS.with(|flags| flags.borrow_mut().insert(key, value));
    repaint();
}

/// `.timeline-row__badge`.
pub fn badge(text_: impl Into<Arc<str>>, title: impl Into<Arc<str>>) -> E {
    let t = theme();
    row()
        .items_center()
        .shrink0()
        .mr(4.0)
        .px(5.0)
        .py(1.0)
        .radius(999.0)
        .border(t.card_stroke)
        .tooltip(title)
        .child(text(text_).ty(TypeStyle { size: 9.0, line: 12.0, weight: Weight::Semibold, mono: false, italic: false, serif: false }).color(t.text_secondary).nowrap())
}

/// The arrow of a disclosure, turned when open.
pub fn chevron(open: bool, size: f32, color: Color) -> E {
    icon("ChevronRight", size).color(color).rotate(if open { 90.0 } else { 0.0 })
}

/// `EmptyState`: a mark, a title and maybe a line under it, centred.
pub fn empty_state(mark: Option<E>, title: impl Into<Arc<str>>, description: Option<&str>) -> E {
    let t = theme();
    col()
        .grow()
        .center()
        .gap(6.0)
        .py(48.0)
        .child(mark)
        .child(text(title).ty(BODY_SMALL_STRONG).color(t.text_primary).text_center())
        .child(description.map(|d| text(d.to_owned()).ty(CAPTION).color(t.text_tertiary).text_center()))
}

/// A row of plain text in the code face that keeps its line breaks and wraps: `<pre>` with `white-space: pre-wrap`.
pub fn pre_wrap(s: impl Into<Arc<str>>, ty: TypeStyle, color: Color) -> E {
    text(s).ty(ty).color(color).selectable()
}

/// The text styles the timeline's CSS spells out, all in one place.
pub mod ty {
    use fluent::theme::{TypeStyle, Weight};

    const fn style(size: f32, line: f32, weight: Weight, mono: bool) -> TypeStyle {
        TypeStyle { size, line, weight, mono, italic: false, serif: false }
    }

    /// `.context-card { font-size: 13px; line-height: 1.58 }`.
    pub const CARD: TypeStyle = style(13.0, 20.5, Weight::Regular, false);
    /// `.timeline-row__prose { font-size: 12.5px; line-height: 1.62 }`.
    pub const PROSE: TypeStyle = style(12.5, 20.25, Weight::Regular, false);
    /// A row's name and line.
    pub const ROW: TypeStyle = style(12.5, 18.0, Weight::Regular, false);
    pub const ROW_CODE: TypeStyle = style(11.5, 18.0, Weight::Regular, true);
    pub const FIGURE: TypeStyle = style(11.0, 18.0, Weight::Regular, false);
    pub const OUTPUT: TypeStyle = style(11.5, 17.0, Weight::Regular, true);
    pub const NOTE: TypeStyle = style(11.5, 17.25, Weight::Regular, false);
    pub const MICRO: TypeStyle = style(10.0, 14.0, Weight::Regular, false);
}
