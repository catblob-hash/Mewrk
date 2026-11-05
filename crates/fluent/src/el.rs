//! Elements: a tree rebuilt every frame, laid out with flexbox and grid, then painted and hit-tested as one.
//!
//! This is the layer views are written in. A view is a function from application state to an `El`, so a React
//! component translates into one directly: the same nesting, the same flex and grid, closures for handlers, and
//! `use_state` for local state. Handlers run after the frame's painting with `&mut A`, so a view only ever reads.
//!
//! Anything that is not boxes and text (a terminal, an editor, a timeline with its own virtualisation) is a
//! `custom` leaf that paints with the immediate-mode `Ui` inside its rectangle.

use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::Arc;

use taffy::prelude::{auto, length, percent, AvailableSpace, Size, TaffyTree};
pub use taffy::style::Style as LayoutStyle;
use taffy::style::{AlignItems, Dimension, Display, FlexDirection, FlexWrap, JustifyContent, LengthPercentage, LengthPercentageAuto, Overflow, Position};

use crate::edit::TextEdit;
use crate::gpu::{ImageId, LAYER_BASE, LAYER_MODAL, LAYER_POPUP, LAYER_TOOLTIP};
use crate::text::{BlockKind, Doc, FxHasher, TextCtx};
use crate::theme::*;
use crate::ui::{rect, CursorKind, Icon, Key, KeyPress, Rect, Scroll, Ui};

mod a11y;
pub use a11y::{a11y_action, a11y_active, announce, announce_assertive, set_a11y_active, take_a11y_update, AriaChild};
pub(crate) use a11y::{a11y_note_text_input, a11y_take_edits, A11yEdit, TextInput};
pub use accesskit::{AriaCurrent, HasPopup, Invalid, Live, Orientation, Role, Toggled};

pub type Handler<A> = Rc<dyn Fn(&mut A)>;
pub type PointerHandler<A> = Rc<dyn Fn(&mut A, PointerEvent)>;
pub type KeyHandler<A> = Rc<dyn Fn(&mut A, &KeyPress) -> bool>;
type CustomPaint<A> = Box<dyn FnOnce(&mut A, &mut Ui<'_>, Rect)>;
type CustomMeasure<A> = Box<dyn FnMut(&mut A, &mut Ui<'_>, Option<f32>, f32) -> (f32, f32)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Down,
    Move,
    Up,
}

/// A pointer event in window coordinates. From the press on, a captured element keeps getting them wherever the
/// pointer goes, until the release.
#[derive(Clone, Copy, Debug)]
pub struct PointerEvent {
    pub phase: Phase,
    pub x: f32,
    pub y: f32,
    /// Distance from where the press happened.
    pub dx: f32,
    pub dy: f32,
    /// The element's rectangle.
    pub rect: Rect,
}

/// A press as a capture handler hears it, before anything inside has reacted.
#[derive(Clone, Copy, Debug)]
pub struct PressEvent {
    pub x: f32,
    pub y: f32,
    /// Command or Control is held.
    pub modifier: bool,
    /// It landed on something that takes text, where a press means a caret.
    pub on_editable: bool,
    /// The element's rectangle.
    pub rect: Rect,
}

/// Hashes anything into the id space elements live in.
pub fn hash_key(key: impl Hash) -> u64 {
    let mut h = FxHasher::default();
    key.hash(&mut h);
    h.finish() | 1
}

fn mix_id(parent: u64, index: usize) -> u64 {
    (parent ^ (index as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15)).wrapping_mul(0x100_0000_01B3) | 1
}

// ---------------------------------------------------------------- runtime

/// What views and handlers can reach without being passed anything. The UI runs on one thread.
#[derive(Default)]
struct Runtime {
    frame: u64,
    states: HashMap<(TypeId, u64), (Rc<dyn Any>, u64)>,
    rects: HashMap<u64, Rect>,
    scrolls: HashMap<u64, Scroll>,
    scroll_requests: Vec<(u64, ScrollRequest)>,
    focus_request: Option<u64>,
    repaint: bool,
    docs: HashMap<u64, CachedDoc>,
    selections: HashMap<u64, (usize, usize)>,
    selecting: Option<u64>,
    press: Option<(f32, f32)>,
    overlay_open: bool,
    /// Elements with an `on_hover_change` under the pointer last frame.
    hover_watched: Vec<u64>,
    overlay_rects: Vec<Rect>,
    /// The ids the last press landed on, deepest first: what "focus is somewhere in here" asks about.
    press_path: Vec<u64>,
    focus: u64,
    /// A text input held focus when last painted.
    text_input: bool,
    /// Shift is down as of this frame's input.
    shift: bool,
    /// Tab moved the focus and no press has happened since: `:focus-visible`.
    focus_visible: bool,
}

struct CachedDoc {
    doc: Doc,
    frame: u64,
}

#[derive(Clone, Copy, Debug)]
pub enum ScrollRequest {
    To(f32),
    ToEnd,
    By(f32),
    /// Brings a child with this rectangle (in the scroller's content coordinates) into view.
    Reveal { y: f32, h: f32 },
    /// Moves by this much at once, with no easing: content that changed height above the viewport stays put on screen.
    Shift(f32),
}

thread_local! {
    static RT: RefCell<Runtime> = RefCell::new(Runtime::default());
}

fn with_rt<R>(f: impl FnOnce(&mut Runtime) -> R) -> R {
    RT.with(|rt| f(&mut rt.borrow_mut()))
}

/// Local state, kept for as long as a view keeps asking for it, like React's `useState`. A key is global, so it
/// should name what it belongs to: `("sidebar-collapsed", project_id)`.
pub fn use_state<T: 'static>(key: impl Hash, init: impl FnOnce() -> T) -> Local<T> {
    let k = (TypeId::of::<T>(), hash_key(key));
    with_rt(|rt| {
        let frame = rt.frame;
        let entry = rt.states.entry(k).or_insert_with(|| (Rc::new(RefCell::new(init())) as Rc<dyn Any>, frame));
        entry.1 = frame;
        let cell = entry.0.clone().downcast::<RefCell<T>>().expect("the key includes the type");
        Local(cell)
    })
}

pub struct Local<T>(Rc<RefCell<T>>);

impl<T> Clone for Local<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> Local<T> {
    pub fn with<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        f(&self.0.borrow())
    }

    pub fn update<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let r = f(&mut self.0.borrow_mut());
        repaint();
        r
    }

    pub fn set(&self, value: T) {
        *self.0.borrow_mut() = value;
        repaint();
    }

    pub fn borrow(&self) -> std::cell::Ref<'_, T> {
        self.0.borrow()
    }

    pub fn borrow_mut(&self) -> std::cell::RefMut<'_, T> {
        self.0.borrow_mut()
    }
}

impl<T: Clone> Local<T> {
    pub fn get(&self) -> T {
        self.0.borrow().clone()
    }
}

/// Asks for another frame.
pub fn repaint() {
    with_rt(|rt| rt.repaint = true);
}

/// Whether a popover, menu or dialog was up last frame. A native child view sits above everything the GPU draws, so it
/// has to get out of the way of these.
pub fn overlay_open() -> bool {
    with_rt(|rt| rt.overlay_open)
}

/// Where those were: so a native view only moves for a surface that actually reaches it.
pub fn overlay_rects() -> Vec<Rect> {
    with_rt(|rt| rt.overlay_rects.clone())
}

/// The same by an id from `hash_key`, for something that kept the id and not the key.
pub fn rect_by_id(id: u64) -> Option<Rect> {
    with_rt(|rt| rt.rects.get(&id).copied())
}

/// Where an element with this key was last frame.
pub fn rect_of(key: impl Hash) -> Option<Rect> {
    let id = hash_key(key);
    with_rt(|rt| rt.rects.get(&id).copied())
}

pub fn scroll_request(key: impl Hash, request: ScrollRequest) {
    let id = hash_key(key);
    with_rt(|rt| {
        rt.scroll_requests.push((id, request));
        rt.repaint = true;
    });
}

/// A scroller's state as of last frame: offset, content height, viewport height.
pub fn scroll_state(key: impl Hash) -> Option<(f32, f32, f32)> {
    let id = hash_key(key);
    with_rt(|rt| rt.scrolls.get(&id).map(|s| (s.offset, s.content, s.viewport)))
}

/// Whether the last press landed inside the element with this key, for keys that answer "while focus is in here".
pub fn last_press_inside(key: impl Hash) -> bool {
    let id = hash_key(key);
    with_rt(|rt| rt.press_path.contains(&id))
}

/// What holds keyboard focus as of last frame, 0 for nothing: a text field's own key, or a focusable element's id.
pub fn focused_id() -> u64 {
    with_rt(|rt| rt.focus)
}

/// Whether the keyboard moved the focus to where it is, so it should show: `:focus-visible`.
pub fn focus_visible() -> bool {
    with_rt(|rt| rt.focus_visible)
}

/// Whether a text input holds focus: keys typed there are its own, however a surrounding surface would read them.
pub fn text_input_focused() -> bool {
    with_rt(|rt| rt.text_input)
}

/// Whether Shift is down: a handler hears only where the pointer is, and a drag may be held to a line by this.
pub fn shift_held() -> bool {
    with_rt(|rt| rt.shift)
}

/// Whether any selectable text has a non-empty selection, so Cmd+C belongs to those words.
pub fn text_selection_active() -> bool {
    with_rt(|rt| rt.selections.values().any(|(a, b)| a != b))
}

pub fn clear_text_selection() {
    with_rt(|rt| {
        rt.selections.clear();
        rt.selecting = None;
    });
}

pub fn focus_request(key: impl Hash) {
    focus_request_id(hash_key(key));
}

/// The same with an id from `hash_key` or `focused_id`, for handing focus back to what had it; 0 leaves it nowhere.
pub fn focus_request_id(id: u64) {
    with_rt(|rt| {
        rt.focus_request = Some(id);
        rt.repaint = true;
    });
}

// ---------------------------------------------------------------- the element

#[derive(Clone, Copy, Default)]
struct Visual {
    bg: Option<Color>,
    bg2: Option<Color>,
    hover_bg: Option<Color>,
    press_bg: Option<Color>,
    radii: [f32; 4],
    border: Option<Color>,
    /// Top, right, bottom, left.
    sides: [Option<Color>; 4],
    opacity: Option<f32>,
}

#[derive(Clone)]
pub struct Span {
    pub text: Arc<str>,
    pub ty: TypeStyle,
    pub color: Option<Color>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextFlow {
    /// One line per `\n`, as wide as it needs.
    NoWrap,
    /// One line, cut with an ellipsis when it does not fit.
    Ellipsis,
    /// Wraps at the box's width.
    Wrap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextAlign {
    Start,
    Center,
    End,
}

enum Leaf<A: 'static> {
    None,
    /// `hanging` is `padding-left` with `text-indent` taking it back, on every line of a wrapping text.
    Text { spans: Vec<Span>, flow: TextFlow, align: TextAlign, selectable: bool, hanging: f32 },
    Icon { icon: IconRef, size: f32, color: Option<Color>, rotate: f32 },
    Image { id: ImageId, natural: (u32, u32), cover: bool, radius: f32 },
    Custom { paint: Option<CustomPaint<A>>, measure: Option<CustomMeasure<A>> },
}

#[derive(Clone, Copy, Debug)]
pub enum IconRef {
    Fluent(Icon),
    Named(&'static str),
}

impl From<Icon> for IconRef {
    fn from(i: Icon) -> Self {
        IconRef::Fluent(i)
    }
}

impl From<&'static str> for IconRef {
    fn from(n: &'static str) -> Self {
        IconRef::Named(n)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScrollAxis {
    Y,
    /// Starts at the end and stays there as content is added, until the user scrolls away.
    YStickToEnd,
    /// Never follows on its own: the view decides, through `ScrollRequest`s.
    YManual,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Below,
    Above,
    Right,
    Left,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Start,
    Center,
    End,
}

pub struct Portal<A: 'static> {
    modal: bool,
    anchor: Option<u64>,
    at: Option<(f32, f32)>,
    side: Side,
    align: Align,
    gap: f32,
    on_dismiss: Option<Handler<A>>,
}

struct Events<A: 'static> {
    click: Option<Handler<A>>,
    double_click: Option<Handler<A>>,
    middle_click: Option<Handler<A>>,
    context_menu: Option<PointerHandler<A>>,
    pointer: Option<PointerHandler<A>>,
    key: Option<KeyHandler<A>>,
    key_global: Option<KeyHandler<A>>,
    hover_change: Option<Rc<dyn Fn(&mut A, bool)>>,
    press_capture: Option<Rc<dyn Fn(&mut A, PressEvent) -> bool>>,
    focusable: bool,
}

impl<A> Default for Events<A> {
    fn default() -> Self {
        Self { click: None, double_click: None, middle_click: None, context_menu: None, pointer: None, key: None, key_global: None, hover_change: None, press_capture: None, focusable: false }
    }
}

pub struct El<A: 'static> {
    /// An id is never zero, so this costs no more than the id: an element is moved by value at every call on it.
    key: Option<std::num::NonZeroU64>,
    style: LayoutStyle,
    vis: Visual,
    children: Vec<El<A>>,
    leaf: Leaf<A>,
    on: Events<A>,
    tooltip: Option<Arc<str>>,
    hover_text: Option<Arc<str>>,
    full_text: Option<Arc<str>>,
    cursor: Option<CursorKind>,
    scroll: Option<ScrollAxis>,
    clip: bool,
    pass_through: bool,
    inert: bool,
    invisible: bool,
    overlay: bool,
    reveal: bool,
    reveal_force: bool,
    conceal_levels: u8,
    disabled: bool,
    keep_focus: bool,
    editable: bool,
    focus_under: Option<u64>,
    keeps_tab: bool,
    skip_tab: bool,
    presentation: bool,
    portal: Option<Portal<A>>,
    aria: Option<Box<a11y::Aria<A>>>,
}

/// A box that stacks its children vertically, which is what a `div` does.
pub fn div<A>() -> El<A> {
    El {
        key: None,
        style: LayoutStyle { display: Display::Flex, flex_direction: FlexDirection::Column, ..LayoutStyle::default() },
        vis: Visual::default(),
        children: Vec::new(),
        leaf: Leaf::None,
        on: Events::default(),
        tooltip: None,
        hover_text: None,
        full_text: None,
        cursor: None,
        scroll: None,
        clip: false,
        pass_through: false,
        inert: false,
        invisible: false,
        overlay: false,
        reveal: false,
        reveal_force: false,
        conceal_levels: 0,
        disabled: false,
        keep_focus: false,
        editable: false,
        focus_under: None,
        keeps_tab: false,
        skip_tab: false,
        presentation: false,
        portal: None,
        aria: None,
    }
}

/// `display: flex`.
pub fn row<A>() -> El<A> {
    div().row()
}

/// `display: flex; flex-direction: column`.
pub fn col<A>() -> El<A> {
    div()
}

/// Text with one style. Unless told otherwise it wraps.
pub fn text<A>(s: impl Into<Arc<str>>) -> El<A> {
    spans(vec![Span { text: s.into(), ty: BODY_SMALL, color: None }])
}

pub fn spans<A>(spans: Vec<Span>) -> El<A> {
    let mut e = div();
    e.leaf = Leaf::Text { spans, flow: TextFlow::Wrap, align: TextAlign::Start, selectable: false, hanging: 0.0 };
    e
}

pub fn span(s: impl Into<Arc<str>>, ty: TypeStyle) -> Span {
    Span { text: s.into(), ty, color: None }
}

pub fn icon<A>(name: impl Into<IconRef>, size: f32) -> El<A> {
    let mut e = div();
    e.style.size = Size { width: length(size), height: length(size) };
    e.style.flex_shrink = 0.0;
    e.leaf = Leaf::Icon { icon: name.into(), size, color: None, rotate: 0.0 };
    e
}

pub fn image<A>(id: ImageId, natural: (u32, u32)) -> El<A> {
    let mut e = div();
    e.leaf = Leaf::Image { id, natural, cover: false, radius: 0.0 };
    e
}

/// Takes up the free space in a row or a column.
pub fn spacer<A>() -> El<A> {
    div().grow()
}

/// A leaf that paints itself. Give it a size, or `.measure` it.
pub fn custom<A>(paint: impl FnOnce(&mut A, &mut Ui<'_>, Rect) + 'static) -> El<A> {
    let mut e = div();
    e.leaf = Leaf::Custom { paint: Some(Box::new(paint)), measure: None };
    e
}

fn lpa(v: f32) -> LengthPercentageAuto {
    length(v)
}

fn lp(v: f32) -> LengthPercentage {
    length(v)
}

impl<A: 'static> El<A> {
    /// A global identity. Lists need one per row; anything `rect_of`, `.anchor` or a request refers to needs one.
    pub fn key(mut self, key: impl Hash) -> Self {
        self.key = std::num::NonZeroU64::new(hash_key(key));
        self
    }

    /// Any other layout property, for what has no method of its own (grid tracks, for one).
    pub fn with_style(mut self, f: impl FnOnce(&mut LayoutStyle)) -> Self {
        f(&mut self.style);
        self
    }

    pub fn when(self, cond: bool, f: impl FnOnce(Self) -> Self) -> Self {
        if cond { f(self) } else { self }
    }

    pub fn map(self, f: impl FnOnce(Self) -> Self) -> Self {
        f(self)
    }

    pub fn child(mut self, c: impl Into<Option<El<A>>>) -> Self {
        if let Some(c) = c.into() {
            self.children.push(c);
        }
        self
    }

    pub fn children(mut self, cs: impl IntoIterator<Item = El<A>>) -> Self {
        self.children.extend(cs);
        self
    }

    // ------------------------------------------------------------ flex

    pub fn row(mut self) -> Self {
        self.style.flex_direction = FlexDirection::Row;
        self
    }

    pub fn column(mut self) -> Self {
        self.style.flex_direction = FlexDirection::Column;
        self
    }

    pub fn wrap(mut self) -> Self {
        self.style.flex_wrap = FlexWrap::Wrap;
        self
    }

    pub fn items_center(mut self) -> Self {
        self.style.align_items = Some(AlignItems::Center);
        self
    }

    pub fn items_start(mut self) -> Self {
        self.style.align_items = Some(AlignItems::FlexStart);
        self
    }

    pub fn items_end(mut self) -> Self {
        self.style.align_items = Some(AlignItems::FlexEnd);
        self
    }

    pub fn items_stretch(mut self) -> Self {
        self.style.align_items = Some(AlignItems::Stretch);
        self
    }

    pub fn items_baseline(mut self) -> Self {
        self.style.align_items = Some(AlignItems::Baseline);
        self
    }

    pub fn self_center(mut self) -> Self {
        self.style.align_self = Some(AlignItems::Center);
        self
    }

    pub fn self_start(mut self) -> Self {
        self.style.align_self = Some(AlignItems::FlexStart);
        self
    }

    pub fn self_end(mut self) -> Self {
        self.style.align_self = Some(AlignItems::FlexEnd);
        self
    }

    pub fn self_stretch(mut self) -> Self {
        self.style.align_self = Some(AlignItems::Stretch);
        self
    }

    pub fn justify_center(mut self) -> Self {
        self.style.justify_content = Some(JustifyContent::Center);
        self
    }

    pub fn justify_between(mut self) -> Self {
        self.style.justify_content = Some(JustifyContent::SpaceBetween);
        self
    }

    pub fn justify_end(mut self) -> Self {
        self.style.justify_content = Some(JustifyContent::FlexEnd);
        self
    }

    pub fn justify_start(mut self) -> Self {
        self.style.justify_content = Some(JustifyContent::FlexStart);
        self
    }

    pub fn center(self) -> Self {
        self.items_center().justify_center()
    }

    pub fn gap(mut self, v: f32) -> Self {
        self.style.gap = Size { width: lp(v), height: lp(v) };
        self
    }

    pub fn gap_x(mut self, v: f32) -> Self {
        self.style.gap.width = lp(v);
        self
    }

    pub fn gap_y(mut self, v: f32) -> Self {
        self.style.gap.height = lp(v);
        self
    }

    /// `flex-grow: 1`.
    pub fn grow(mut self) -> Self {
        self.style.flex_grow = 1.0;
        self
    }

    pub fn flex_grow(mut self, v: f32) -> Self {
        self.style.flex_grow = v;
        self
    }

    /// `flex: 1; min-width: 0; min-height: 0`, the usual way to make something fill and still shrink.
    pub fn flex1(mut self) -> Self {
        self.style.flex_grow = 1.0;
        self.style.flex_shrink = 1.0;
        self.style.flex_basis = Dimension::length(0.0);
        self.style.min_size = Size { width: length(0.0), height: length(0.0) };
        self
    }

    pub fn shrink0(mut self) -> Self {
        self.style.flex_shrink = 0.0;
        self
    }

    pub fn basis(mut self, v: f32) -> Self {
        self.style.flex_basis = Dimension::length(v);
        self
    }

    // ------------------------------------------------------------ size

    pub fn w(mut self, v: f32) -> Self {
        self.style.size.width = length(v);
        self
    }

    pub fn h(mut self, v: f32) -> Self {
        self.style.size.height = length(v);
        self
    }

    pub fn size(self, w: f32, h: f32) -> Self {
        self.w(w).h(h)
    }

    pub fn w_full(mut self) -> Self {
        self.style.size.width = percent(1.0);
        self
    }

    pub fn h_full(mut self) -> Self {
        self.style.size.height = percent(1.0);
        self
    }

    pub fn w_pct(mut self, v: f32) -> Self {
        self.style.size.width = percent(v / 100.0);
        self
    }

    pub fn h_pct(mut self, v: f32) -> Self {
        self.style.size.height = percent(v / 100.0);
        self
    }

    pub fn min_w(mut self, v: f32) -> Self {
        self.style.min_size.width = length(v);
        self
    }

    pub fn min_h(mut self, v: f32) -> Self {
        self.style.min_size.height = length(v);
        self
    }

    pub fn max_w(mut self, v: f32) -> Self {
        self.style.max_size.width = length(v);
        self
    }

    pub fn max_h(mut self, v: f32) -> Self {
        self.style.max_size.height = length(v);
        self
    }

    pub fn aspect(mut self, ratio: f32) -> Self {
        self.style.aspect_ratio = Some(ratio);
        self
    }

    // ------------------------------------------------------------ spacing

    pub fn p(mut self, v: f32) -> Self {
        self.style.padding = taffy::geometry::Rect { left: lp(v), right: lp(v), top: lp(v), bottom: lp(v) };
        self
    }

    pub fn px(mut self, v: f32) -> Self {
        self.style.padding.left = lp(v);
        self.style.padding.right = lp(v);
        self
    }

    pub fn py(mut self, v: f32) -> Self {
        self.style.padding.top = lp(v);
        self.style.padding.bottom = lp(v);
        self
    }

    pub fn pt(mut self, v: f32) -> Self {
        self.style.padding.top = lp(v);
        self
    }

    pub fn pb(mut self, v: f32) -> Self {
        self.style.padding.bottom = lp(v);
        self
    }

    pub fn pl(mut self, v: f32) -> Self {
        self.style.padding.left = lp(v);
        self
    }

    pub fn pr(mut self, v: f32) -> Self {
        self.style.padding.right = lp(v);
        self
    }

    pub fn m(mut self, v: f32) -> Self {
        self.style.margin = taffy::geometry::Rect { left: lpa(v), right: lpa(v), top: lpa(v), bottom: lpa(v) };
        self
    }

    pub fn mx(mut self, v: f32) -> Self {
        self.style.margin.left = lpa(v);
        self.style.margin.right = lpa(v);
        self
    }

    pub fn my(mut self, v: f32) -> Self {
        self.style.margin.top = lpa(v);
        self.style.margin.bottom = lpa(v);
        self
    }

    pub fn mt(mut self, v: f32) -> Self {
        self.style.margin.top = lpa(v);
        self
    }

    pub fn mb(mut self, v: f32) -> Self {
        self.style.margin.bottom = lpa(v);
        self
    }

    pub fn ml(mut self, v: f32) -> Self {
        self.style.margin.left = lpa(v);
        self
    }

    pub fn mr(mut self, v: f32) -> Self {
        self.style.margin.right = lpa(v);
        self
    }

    /// `margin-left: auto`, which pushes the element to the row's far end.
    pub fn ml_auto(mut self) -> Self {
        self.style.margin.left = auto();
        self
    }

    pub fn mx_auto(mut self) -> Self {
        self.style.margin.left = auto();
        self.style.margin.right = auto();
        self
    }

    // ------------------------------------------------------------ position

    /// Out of flow, positioned against the parent. It also paints above its in-flow siblings.
    pub fn absolute(mut self) -> Self {
        self.style.position = Position::Absolute;
        self.overlay = true;
        self
    }

    pub fn top(mut self, v: f32) -> Self {
        self.style.inset.top = lpa(v);
        self
    }

    pub fn left(mut self, v: f32) -> Self {
        self.style.inset.left = lpa(v);
        self
    }

    pub fn right(mut self, v: f32) -> Self {
        self.style.inset.right = lpa(v);
        self
    }

    /// `left` as a fraction of the parent's width: a marker on a track, say.
    pub fn left_pct(mut self, fraction: f32) -> Self {
        self.style.inset.left = percent(fraction);
        self
    }

    pub fn bottom(mut self, v: f32) -> Self {
        self.style.inset.bottom = lpa(v);
        self
    }

    /// `bottom: <fraction of the parent's height>`, for something standing on its parent's top edge.
    pub fn bottom_pct(mut self, fraction: f32) -> Self {
        self.style.inset.bottom = percent(fraction);
        self
    }

    pub fn inset(self, v: f32) -> Self {
        self.top(v).left(v).right(v).bottom(v)
    }

    /// Paints above in-flow siblings even though it takes part in layout: a sticky header, say.
    pub fn raised(mut self) -> Self {
        self.overlay = true;
        self
    }

    // ------------------------------------------------------------ overflow

    pub fn clip(mut self) -> Self {
        self.clip = true;
        self.style.overflow = taffy::geometry::Point { x: Overflow::Hidden, y: Overflow::Hidden };
        self
    }

    /// A vertical scroller. It needs a key to keep its position across frames, and something that bounds its
    /// height (`flex1`, `h`, `max_h`).
    pub fn scroll_y(mut self) -> Self {
        self.scroll = Some(ScrollAxis::Y);
        self.clip = true;
        self.style.overflow = taffy::geometry::Point { x: Overflow::Hidden, y: Overflow::Scroll };
        self.style.scrollbar_width = 0.0;
        self
    }

    /// A scroller whose owner does all the following itself, as a timeline that glides after a stream does.
    pub fn scroll_manual(mut self) -> Self {
        self = self.scroll_y();
        self.scroll = Some(ScrollAxis::YManual);
        self
    }

    pub fn scroll_stick(mut self) -> Self {
        self = self.scroll_y();
        self.scroll = Some(ScrollAxis::YStickToEnd);
        self
    }

    // ------------------------------------------------------------ look

    pub fn bg(mut self, c: Color) -> Self {
        self.vis.bg = Some(c);
        self
    }

    pub fn gradient(mut self, top: Color, bottom: Color) -> Self {
        self.vis.bg = Some(top);
        self.vis.bg2 = Some(bottom);
        self
    }

    pub fn hover_bg(mut self, c: Color) -> Self {
        self.vis.hover_bg = Some(c);
        self
    }

    pub fn press_bg(mut self, c: Color) -> Self {
        self.vis.press_bg = Some(c);
        self
    }

    pub fn radius(mut self, r: f32) -> Self {
        self.vis.radii = [r; 4];
        self
    }

    /// Top-left, top-right, bottom-right, bottom-left.
    pub fn radii(mut self, r: [f32; 4]) -> Self {
        self.vis.radii = r;
        self
    }

    pub fn border(mut self, c: Color) -> Self {
        self.vis.border = Some(c);
        self
    }

    pub fn border_top(mut self, c: Color) -> Self {
        self.vis.sides[0] = Some(c);
        self
    }

    pub fn border_right(mut self, c: Color) -> Self {
        self.vis.sides[1] = Some(c);
        self
    }

    pub fn border_bottom(mut self, c: Color) -> Self {
        self.vis.sides[2] = Some(c);
        self
    }

    pub fn border_left(mut self, c: Color) -> Self {
        self.vis.sides[3] = Some(c);
        self
    }

    pub fn opacity(mut self, v: f32) -> Self {
        self.vis.opacity = Some(v);
        self
    }

    pub fn cursor(mut self, c: CursorKind) -> Self {
        self.cursor = Some(c);
        self
    }

    /// What a reader says for this element: its name when it has no words of its own, otherwise a description. Nothing is drawn for it: what is
    /// drawn on hover is `disabled_reason`, `full_text` and a line an ellipsis cut.
    pub fn tooltip(mut self, t: impl Into<Arc<str>>) -> Self {
        let t: Arc<str> = t.into();
        if !t.is_empty() {
            self.tooltip = Some(t);
        }
        self
    }

    /// Why this can't be used right now: the tooltip, and drawn while the pointer rests here. Say it on whatever looks disabled, whether or not it has a click handler.
    pub fn disabled_reason(self, why: impl Into<Arc<str>>) -> Self {
        let why: Arc<str> = why.into();
        self.tooltip(why.clone()).hover_text(why)
    }

    /// `disabled_reason`, where there may not be one.
    pub fn maybe_disabled_reason(self, why: Option<impl Into<Arc<str>>>) -> Self {
        match why {
            Some(why) => self.disabled_reason(why),
            None => self,
        }
    }

    /// Words drawn while the pointer rests here, and nothing a reader gets.
    pub fn hover_text(mut self, words: impl Into<Arc<str>>) -> Self {
        let words: Arc<str> = words.into();
        if !words.is_empty() {
            self.hover_text = Some(words);
        }
        self
    }

    /// The tooltip, and what a line inside this element is cut from: a path where the row shows its name, say. Drawn for that line instead of its own words,
    /// and only while it is cut, since a line that fits has nothing more to show.
    pub fn full_text(self, words: impl Into<Arc<str>>) -> Self {
        let words: Arc<str> = words.into();
        self.tooltip(words.clone()).cut_text(words)
    }

    /// `full_text` and nothing a reader gets. Beside `hover_text` on the same element, it is what comes up in that text's place while a line is cut.
    pub fn cut_text(mut self, words: impl Into<Arc<str>>) -> Self {
        let words: Arc<str> = words.into();
        if !words.is_empty() {
            self.full_text = Some(words);
        }
        self
    }

    /// `pointer-events: none`.
    pub fn pass_through(mut self) -> Self {
        self.pass_through = true;
        self
    }

    /// `pointer-events: none` as CSS has it, for all that is inside too.
    pub fn inert(mut self) -> Self {
        self.inert = true;
        self
    }

    /// `visibility: hidden` with `inert`: it keeps its box and all that is inside goes on running, so a list keeps its scroll and a
    /// terminal its size, but none of it reaches the screen, takes a press or holds the focus. Nothing a reader can find either.
    pub fn invisible(mut self) -> Self {
        self.invisible = true;
        self.inert().aria_hidden(true)
    }

    pub fn disabled(mut self, on: bool) -> Self {
        self.disabled = on;
        if on {
            self.vis.opacity = Some(self.vis.opacity.unwrap_or(1.0) * 0.45);
        }
        self
    }

    /// A press here leaves the keyboard focus where it was, as `preventDefault` on mousedown does: a button beside a
    /// text field that acts on the field without ending its edit.
    pub fn keep_focus(mut self) -> Self {
        self.keep_focus = true;
        self
    }

    /// This takes text, so a press on it means a caret and nothing a surrounding surface wants to capture.
    /// The keyboard focus sits under this id rather than the element's own, as a text field's does. Makes it a Tab stop.
    pub fn focus_under(mut self, id: u64) -> Self {
        self.focus_under = Some(id);
        self
    }

    /// `tabindex="-1"`: it can hold the focus, but Tab passes it by, as with a roving row that isn't the current one.
    pub fn skip_tab(mut self, on: bool) -> Self {
        self.skip_tab = on;
        self
    }

    /// Tab is a key this element has a use for, as a terminal does, so it doesn't move the focus on from here.
    pub fn keeps_tab(mut self) -> Self {
        self.keeps_tab = true;
        self
    }

    pub fn editable(mut self) -> Self {
        self.editable = true;
        self
    }

    /// Only painted while its parent is hovered, like a row's action buttons.
    pub fn reveal_on_parent_hover(mut self) -> Self {
        self.reveal = true;
        self
    }

    /// The opposite: not painted while the pointer is over this many levels of what contains it, so figures step aside for the
    /// controls that appear over them.
    pub fn conceal_on_hover_up(mut self, levels: u8) -> Self {
        self.conceal_levels = levels;
        self
    }

    /// The same, staying visible when `on` (its menu is open, say).
    pub fn reveal_unless(mut self, on: bool) -> Self {
        self.reveal = true;
        self.reveal_force = on;
        self
    }

    // ------------------------------------------------------------ text

    pub fn ty(mut self, ty: TypeStyle) -> Self {
        if let Leaf::Text { spans, .. } = &mut self.leaf {
            for s in spans {
                s.ty = ty;
            }
        }
        self
    }

    pub fn color(mut self, c: Color) -> Self {
        match &mut self.leaf {
            Leaf::Text { spans, .. } => {
                for s in spans {
                    s.color = Some(c);
                }
            }
            Leaf::Icon { color, .. } => *color = Some(c),
            _ => {}
        }
        self
    }

    /// What wraps stands this far in from where its line began: each line of the text an item of a list, `padding-left: 2ch; text-indent: -2ch`.
    pub fn hanging(mut self, by: f32) -> Self {
        if let Leaf::Text { hanging, .. } = &mut self.leaf {
            *hanging = by;
        }
        self
    }

    pub fn nowrap(mut self) -> Self {
        if let Leaf::Text { flow, .. } = &mut self.leaf {
            *flow = TextFlow::NoWrap;
        }
        self
    }

    pub fn ellipsis(mut self) -> Self {
        if let Leaf::Text { flow, .. } = &mut self.leaf {
            *flow = TextFlow::Ellipsis;
        }
        self.style.min_size.width = length(0.0);
        self.style.flex_shrink = 1.0;
        self
    }

    pub fn text_center(mut self) -> Self {
        if let Leaf::Text { align, .. } = &mut self.leaf {
            *align = TextAlign::Center;
        }
        self
    }

    pub fn text_end(mut self) -> Self {
        if let Leaf::Text { align, .. } = &mut self.leaf {
            *align = TextAlign::End;
        }
        self
    }

    /// The user can select this text with the pointer and copy it.
    pub fn selectable(mut self) -> Self {
        if let Leaf::Text { selectable, .. } = &mut self.leaf {
            *selectable = true;
        }
        self
    }

    /// Rotates an icon, clockwise, in degrees. Boxes do not rotate.
    pub fn rotate(mut self, deg: f32) -> Self {
        if let Leaf::Icon { rotate, .. } = &mut self.leaf {
            *rotate = deg;
        }
        self
    }

    /// How a `custom` leaf sizes itself: known width if any, available width, and it answers width and height.
    pub fn measure(mut self, f: impl FnMut(&mut A, &mut Ui<'_>, Option<f32>, f32) -> (f32, f32) + 'static) -> Self {
        if let Leaf::Custom { measure, .. } = &mut self.leaf {
            *measure = Some(Box::new(f));
        }
        self
    }

    /// An image covers its box, cropping if it must, instead of being fitted inside it.
    pub fn cover(mut self) -> Self {
        if let Leaf::Image { cover, .. } = &mut self.leaf {
            *cover = true;
        }
        self
    }

    pub fn image_radius(mut self, r: f32) -> Self {
        if let Leaf::Image { radius, .. } = &mut self.leaf {
            *radius = r;
        }
        self
    }

    // ------------------------------------------------------------ events

    pub fn on_click(mut self, f: impl Fn(&mut A) + 'static) -> Self {
        self.on.click = Some(Rc::new(f));
        self
    }

    pub fn on_double_click(mut self, f: impl Fn(&mut A) + 'static) -> Self {
        self.on.double_click = Some(Rc::new(f));
        self
    }

    /// The middle button pressed on this element, or the deepest thing inside it that listens.
    pub fn on_middle_click(mut self, f: impl Fn(&mut A) + 'static) -> Self {
        self.on.middle_click = Some(Rc::new(f));
        self
    }

    pub fn on_context_menu(mut self, f: impl Fn(&mut A, PointerEvent) + 'static) -> Self {
        self.on.context_menu = Some(Rc::new(f));
        self
    }

    /// Press, moves and release. From the press on the pointer is captured, so a drag keeps coming here.
    pub fn on_pointer(mut self, f: impl Fn(&mut A, PointerEvent) + 'static) -> Self {
        self.on.pointer = Some(Rc::new(f));
        self
    }

    /// Hears a press anywhere inside this element before what is inside does, like `onPointerDownCapture`. Answer true and
    /// the press is this element's alone: its `on_pointer` gets the moves and the release, and nothing inside reacts.
    pub fn on_press_capture(mut self, f: impl Fn(&mut A, PressEvent) -> bool + 'static) -> Self {
        self.on.press_capture = Some(Rc::new(f));
        self
    }

    /// Keys while focus is on this element or inside it. Answer true to stop them going further.
    pub fn on_key(mut self, f: impl Fn(&mut A, &KeyPress) -> bool + 'static) -> Self {
        self.on.key = Some(Rc::new(f));
        self.on.focusable = true;
        self
    }

    /// Keys whatever holds focus, topmost element first: shortcuts, and menus that close on Escape.
    pub fn on_key_global(mut self, f: impl Fn(&mut A, &KeyPress) -> bool + 'static) -> Self {
        self.on.key_global = Some(Rc::new(f));
        self
    }

    pub fn on_hover_change(mut self, f: impl Fn(&mut A, bool) + 'static) -> Self {
        self.on.hover_change = Some(Rc::new(f));
        self
    }

    pub fn focusable(mut self) -> Self {
        self.on.focusable = true;
        self
    }

    // ------------------------------------------------------------ portals

    /// Lifts this element out of the tree to the window's overlay layer, placed against the element whose key is
    /// `anchor`. `on_dismiss` fires on a press outside it and on Escape.
    pub fn popover(mut self, anchor: impl Hash, side: Side, align: Align, gap: f32, on_dismiss: impl Fn(&mut A) + 'static) -> Self {
        self.portal = Some(Portal { modal: false, anchor: Some(hash_key(anchor)), at: None, side, align, gap, on_dismiss: Some(Rc::new(on_dismiss)) });
        self
    }

    /// The same, at a point: a context menu, say.
    /// The same with the anchor's id from `hash_key`, for a caller that kept the id and not the key.
    pub fn popover_id(mut self, anchor: u64, side: Side, align: Align, gap: f32, on_dismiss: impl Fn(&mut A) + 'static) -> Self {
        self.portal = Some(Portal { modal: false, anchor: Some(anchor), at: None, side, align, gap, on_dismiss: Some(Rc::new(on_dismiss)) });
        self
    }

    pub fn popover_at(mut self, x: f32, y: f32, on_dismiss: impl Fn(&mut A) + 'static) -> Self {
        self.portal = Some(Portal { modal: false, anchor: None, at: Some((x, y)), side: Side::Below, align: Align::Start, gap: 0.0, on_dismiss: Some(Rc::new(on_dismiss)) });
        self
    }

    /// Lifted to the overlay layer at a point, with nothing to dismiss it: a tray that stays until it is answered. A press or
    /// Escape goes straight past it to whatever is underneath.
    pub fn overlay_at(mut self, x: f32, y: f32) -> Self {
        self.portal = Some(Portal { modal: false, anchor: None, at: Some((x, y)), side: Side::Below, align: Align::Start, gap: 0.0, on_dismiss: None });
        self
    }

    /// At a point the caller worked out, with `anchor` as what a press does not dismiss it from: a menu that places itself.
    pub fn popover_at_anchored(mut self, anchor: u64, x: f32, y: f32, on_dismiss: impl Fn(&mut A) + 'static) -> Self {
        self.portal = Some(Portal { modal: false, anchor: Some(anchor), at: Some((x, y)), side: Side::Below, align: Align::Start, gap: 0.0, on_dismiss: Some(Rc::new(on_dismiss)) });
        self
    }

    /// Fills the window on the dialog layer, blocking everything below. Use `dialog` for the common case.
    pub fn modal_layer(mut self, on_dismiss: Option<Handler<A>>) -> Self {
        self.style.size = Size { width: percent(1.0), height: percent(1.0) };
        self.portal = Some(Portal { modal: true, anchor: None, at: None, side: Side::Below, align: Align::Start, gap: 0.0, on_dismiss });
        self
    }
}

/// Accessibility. Text is a label, `on_click` a button, a field a text input and a scroller scrolls without being told; these are for
/// what cannot be read off the element. While nobody is listening they keep nothing, so a view can call them every frame.
impl<A: 'static> El<A> {
    // A view calls these every frame on an element that is moved by value, so with nobody listening they must come to one branch.
    #[inline(always)]
    fn with_aria(mut self, f: impl FnOnce(&mut a11y::Aria<A>)) -> Self {
        if a11y_active() {
            self.aria_cold(f);
        }
        self
    }

    #[cold]
    #[inline(never)]
    fn aria_cold(&mut self, f: impl FnOnce(&mut a11y::Aria<A>)) {
        f(self.aria.get_or_insert_with(Default::default));
    }

    /// `role`. `Role::GenericContainer` is `role="presentation"`: this element is only layout, whatever handlers it has, and what is
    /// inside it rises to what is around it.
    #[inline]
    pub fn role(mut self, role: Role) -> Self {
        // Whoever is listening: Tab goes by this as well.
        self.presentation = role == Role::GenericContainer;
        self.with_aria(|a| a.role = Some(role))
    }

    /// `aria-label`.
    #[inline]
    pub fn aria_label(self, label: impl Into<Arc<str>>) -> Self {
        self.with_aria(|a| a.label = Some(label.into()))
    }

    /// What a `custom` leaf paints that a reader can stop on. It takes the leaf's place in the tree.
    #[inline]
    pub fn aria_children(self, children: Rc<[AriaChild<A>]>) -> Self {
        self.with_aria(|a| a.children = Some(children))
    }

    /// What is read after the name, `aria-description`. A tooltip that differs from the name is one already.
    #[inline]
    pub fn aria_description(self, text: impl Into<Arc<str>>) -> Self {
        self.with_aria(|a| a.description = Some(text.into()))
    }

    /// `aria-labelledby`: the element with this key says what this is.
    #[inline]
    pub fn aria_labelledby(self, key: impl Hash) -> Self {
        self.with_aria(|a| a.labelled_by.push(hash_key(key)))
    }

    /// `aria-describedby`.
    #[inline]
    pub fn aria_describedby(self, key: impl Hash) -> Self {
        self.with_aria(|a| a.described_by.push(hash_key(key)))
    }

    /// `aria-controls`. A control a popover is anchored to has this and `aria-expanded` on its own while the popover is up.
    #[inline]
    pub fn aria_controls(self, key: impl Hash) -> Self {
        self.with_aria(|a| a.controls.push(hash_key(key)))
    }

    /// `aria-hidden`: this and everything inside it are not there for a screen reader. Portals opened from inside still are.
    #[inline]
    pub fn aria_hidden(self, on: bool) -> Self {
        self.with_aria(|a| a.hidden = on)
    }

    /// `aria-expanded`.
    #[inline]
    pub fn aria_expanded(self, on: bool) -> Self {
        self.with_aria(|a| a.expanded = Some(on))
    }

    /// `aria-selected`.
    #[inline]
    pub fn aria_selected(self, on: bool) -> Self {
        self.with_aria(|a| a.selected = Some(on))
    }

    /// `aria-checked`.
    #[inline]
    pub fn aria_checked(self, on: bool) -> Self {
        self.with_aria(|a| a.toggled = Some(on.into()))
    }

    /// `aria-checked="mixed"`.
    #[inline]
    pub fn aria_checked_mixed(self) -> Self {
        self.with_aria(|a| a.toggled = Some(Toggled::Mixed))
    }

    /// `aria-pressed`.
    #[inline]
    pub fn aria_pressed(self, on: bool) -> Self {
        self.with_aria(|a| a.toggled = Some(on.into()))
    }

    /// `aria-disabled`: says it is unavailable and refuses what a screen reader asks of it. `disabled` does this too, and dims it.
    #[inline]
    pub fn aria_disabled(self, on: bool) -> Self {
        self.with_aria(|a| a.disabled = on)
    }

    /// `aria-busy`.
    #[inline]
    pub fn aria_busy(self, on: bool) -> Self {
        self.with_aria(|a| a.busy = on)
    }

    /// `aria-invalid`.
    #[inline]
    pub fn aria_invalid(self, on: bool) -> Self {
        self.with_aria(|a| a.invalid = on.then_some(Invalid::True))
    }

    /// `aria-current`.
    #[inline]
    pub fn aria_current(self, current: AriaCurrent) -> Self {
        self.with_aria(|a| a.current = Some(current))
    }

    /// `aria-haspopup`.
    #[inline]
    pub fn aria_haspopup(self, kind: HasPopup) -> Self {
        self.with_aria(|a| a.has_popup = Some(kind))
    }

    /// `aria-valuenow`, `aria-valuemin` and `aria-valuemax`. With `on_pointer`, `on_key` or `on_aria_value` this is a slider, and
    /// otherwise a progress bar, unless `role` says which.
    #[inline]
    pub fn aria_value(self, now: f64, min: f64, max: f64) -> Self {
        self.with_aria(|a| a.value = Some((now, min, max)))
    }

    /// `aria-valuetext`.
    #[inline]
    pub fn aria_value_text(self, text: impl Into<Arc<str>>) -> Self {
        self.with_aria(|a| a.value_text = Some(text.into()))
    }

    /// `aria-live`: changes to what this says are read out. `Role::Alert` and `Role::Status` are live already.
    #[inline]
    pub fn aria_live(self, live: Live) -> Self {
        self.with_aria(|a| a.live = Some(live))
    }

    /// `aria-orientation`.
    #[inline]
    pub fn aria_orientation(self, orientation: Orientation) -> Self {
        self.with_aria(|a| a.orientation = Some(orientation))
    }

    /// `aria-level`.
    #[inline]
    pub fn aria_level(self, level: usize) -> Self {
        self.with_aria(|a| a.level = Some(level))
    }

    /// `aria-keyshortcuts`.
    #[inline]
    pub fn aria_keyshortcuts(self, keys: impl Into<Arc<str>>) -> Self {
        self.with_aria(|a| a.shortcuts = Some(keys.into()))
    }

    /// A screen reader set the value. Increment and decrement go to `on_key` as arrow keys when there is one, and come here otherwise.
    #[inline]
    pub fn on_aria_value(self, f: impl Fn(&mut A, f64) + 'static) -> Self {
        self.with_aria(|a| a.on_value = Some(Rc::new(f)))
    }
}

impl<A> From<El<A>> for Option<Box<El<A>>> {
    fn from(e: El<A>) -> Self {
        Some(Box::new(e))
    }
}

// ---------------------------------------------------------------- the frame

struct Node<A: 'static> {
    id: u64,
    keyed: bool,
    parent: usize,
    tf: taffy::tree::NodeId,
    vis: Visual,
    leaf: Leaf<A>,
    on: Events<A>,
    tooltip: Option<Arc<str>>,
    hover_text: Option<Arc<str>>,
    full_text: Option<Arc<str>>,
    /// What a custom leaf said it cut while it was painted.
    cut_line: Option<Arc<str>>,
    cursor: Option<CursorKind>,
    scroll: Option<ScrollAxis>,
    clip: bool,
    pass_through: bool,
    inert: bool,
    invisible: bool,
    overlay: bool,
    reveal: bool,
    reveal_force: bool,
    conceal_levels: u8,
    disabled: bool,
    keep_focus: bool,
    editable: bool,
    focus_under: Option<u64>,
    keeps_tab: bool,
    skip_tab: bool,
    presentation: bool,
    aria: Option<Box<a11y::Aria<A>>>,
    children: Vec<usize>,
    portal_root: usize,
    rect: Rect,
    clip_rect: Rect,
    hovered: bool,
}

const NONE: usize = usize::MAX;

struct PortalRoot<A: 'static> {
    root: usize,
    portal: Portal<A>,
}

struct Frame<A: 'static> {
    nodes: Vec<Node<A>>,
    taffy: TaffyTree<usize>,
    portals: Vec<PortalRoot<A>>,
    /// Nodes whose width only `max_w` limits, with that limit.
    capped: Vec<(taffy::tree::NodeId, f32)>,
}

impl<A: 'static> Frame<A> {
    fn add(&mut self, el: El<A>, parent: usize, id: u64, portal_root: usize) -> usize {
        let El { key, mut style, vis, children, leaf, on, tooltip, hover_text, full_text, cursor, scroll, clip, pass_through, inert, invisible, overlay, reveal, reveal_force, conceal_levels, disabled, keep_focus, editable, focus_under, keeps_tab, skip_tab, presentation, portal, aria } = el;
        let id = key.map_or(id, std::num::NonZeroU64::get);
        let inert = inert || self.nodes.get(parent).is_some_and(|parent| parent.inert);
        let pass_through = pass_through || inert;
        if matches!(leaf, Leaf::Text { flow: TextFlow::Wrap, .. }) && style.min_size.width == auto() {
            style.min_size.width = length(0.0);
        }
        let idx = self.nodes.len();
        let cap = style.max_size.width.into_raw();
        let cap = (style.size.width == auto() && cap.tag() == taffy::style::CompactLength::LENGTH_TAG).then(|| cap.value());
        let tf = match leaf {
            Leaf::None => self.taffy.new_leaf(style).expect("taffy"),
            _ => self.taffy.new_leaf_with_context(style, idx).expect("taffy"),
        };
        if let Some(cap) = cap {
            self.capped.push((tf, cap));
        }
        let is_portal = portal.is_some();
        self.nodes.push(Node {
            id,
            keyed: key.is_some(),
            parent: if is_portal { NONE } else { parent },
            tf,
            vis,
            leaf,
            on,
            tooltip,
            hover_text,
            full_text,
            cut_line: None,
            cursor,
            scroll,
            clip,
            pass_through,
            inert,
            invisible,
            overlay,
            reveal,
            reveal_force,
            conceal_levels,
            disabled,
            keep_focus,
            editable,
            focus_under,
            keeps_tab,
            skip_tab,
            presentation,
            aria,
            children: Vec::new(),
            portal_root: if is_portal { idx } else { portal_root },
            rect: rect(0.0, 0.0, 0.0, 0.0),
            clip_rect: rect(0.0, 0.0, 0.0, 0.0),
            hovered: false,
        });
        if let Some(p) = portal {
            self.portals.push(PortalRoot { root: idx, portal: p });
        }
        let root_for_children = if is_portal { idx } else { portal_root };
        let mut tf_children = Vec::with_capacity(children.len());
        for (i, c) in children.into_iter().enumerate() {
            let hoisted = c.portal.is_some();
            let child = self.add(c, idx, mix_id(id, i), root_for_children);
            if !hoisted {
                self.nodes[idx].children.push(child);
                tf_children.push(self.nodes[child].tf);
            }
        }
        if !tf_children.is_empty() {
            self.taffy.set_children(tf, &tf_children).expect("taffy");
        }
        idx
    }

    /// Absolute rectangles and paint order, from the layout and the scrollers' current offsets.
    fn place(&mut self, idx: usize, ox: f32, oy: f32, clip: Rect, order: &mut Vec<usize>) {
        let layout = *self.taffy.layout(self.nodes[idx].tf).expect("taffy");
        let (x, y) = (ox + layout.location.x, oy + layout.location.y);
        let r = rect(x, y, layout.size.width, layout.size.height);
        let node = &mut self.nodes[idx];
        node.rect = r;
        node.clip_rect = clip;
        order.push(idx);
        let inner_clip = if node.clip { clip.intersect(&r) } else { clip };
        let mut scroll_y = 0.0;
        if node.scroll.is_some() {
            scroll_y = with_rt(|rt| rt.scrolls.get(&node.id).map_or(0.0, |s| s.offset));
        }
        let children = std::mem::take(&mut self.nodes[idx].children);
        // In-flow children first; overlays above them.
        for &c in children.iter().filter(|&&c| !self.nodes[c].overlay).collect::<Vec<_>>() {
            self.place(c, x, y - scroll_y, inner_clip, order);
        }
        for &c in children.iter().filter(|&&c| self.nodes[c].overlay).collect::<Vec<_>>() {
            self.place(c, x, y - scroll_y, inner_clip, order);
        }
        self.nodes[idx].children = children;
    }
}

fn tf_avail(a: AvailableSpace) -> f32 {
    match a {
        AvailableSpace::Definite(v) => v,
        AvailableSpace::MinContent => 0.0,
        AvailableSpace::MaxContent => f32::INFINITY,
    }
}

fn longest_unbreakable(ctx: &mut TextCtx, s: &str, ty: &TypeStyle, scale: f32) -> f32 {
    // Between any two characters of a CJK run a line may break, so only Latin words have any width that must stay whole.
    let mut best = 0.0f32;
    for word in s.split(|c: char| c.is_whitespace() || crate::text::is_cjk(c)) {
        if !word.is_empty() {
            best = best.max(ctx.line(word, ty, Color::WHITE).width / scale);
        }
    }
    best.max(ty.size)
}

fn doc_key(id: u64, spans: &[Span]) -> u64 {
    let mut h = FxHasher::default();
    id.hash(&mut h);
    for s in spans {
        s.text.hash(&mut h);
        s.ty.size.to_bits().hash(&mut h);
        s.ty.line.to_bits().hash(&mut h);
        s.ty.weight.hash(&mut h);
        s.ty.mono.hash(&mut h);
        s.color.map(|c| (c.r, c.g, c.b, c.a)).hash(&mut h);
    }
    h.finish()
}

/// A leaf's document, with what it holds set out as the leaf says.
fn begin_text_doc(ctx: &mut TextCtx, doc: &mut Doc, spans: &[Span], hanging: f32) {
    let first = spans.first().map_or(BODY_SMALL, |s| s.ty);
    doc.hang_each_line = hanging > 0.0;
    doc.begin_hanging_block(ctx, BlockKind::Text, hanging, hanging, 0.0, first);
}

fn wrapped_height(ctx: &mut TextCtx, id: u64, spans: &[Span], hanging: f32, width: f32, scale: f32, theme: &Theme) -> f32 {
    with_rt(|rt| {
        let frame = rt.frame;
        let key = doc_key(id, spans);
        let cached = rt.docs.entry(key).or_insert_with(|| {
            let mut doc = Doc::new((width * scale).max(1.0));
            doc.track_carets = true;
            begin_text_doc(ctx, &mut doc, spans, hanging);
            for s in spans {
                doc.push(ctx, &s.text, s.ty, s.color.unwrap_or(theme.text_primary));
            }
            CachedDoc { doc, frame }
        });
        cached.frame = frame;
        if (cached.doc.width() - (width * scale).max(1.0)).abs() > 0.5 {
            cached.doc.set_width(ctx, (width * scale).max(1.0));
        }
        cached.doc.height() / scale
    })
}

#[allow(clippy::too_many_arguments)]
fn measure_text(ctx: &mut TextCtx, id: u64, spans: &[Span], flow: TextFlow, hanging: f32, known: Size<Option<f32>>, avail: Size<AvailableSpace>, scale: f32, theme: &Theme) -> Size<f32> {
    let line_h = spans.iter().map(|s| s.ty.line).fold(0.0, f32::max);
    // The widest line.
    let mut lines = vec![0.0f32];
    for (i, s) in spans.iter().enumerate() {
        // Between spans a trailing space takes room; at the very end it doesn't.
        let last = i + 1 == spans.len();
        let mut parts = s.text.split('\n');
        if let Some(first) = parts.next() {
            let run = ctx.line(first, &s.ty, Color::WHITE);
            *lines.last_mut().expect("never empty") += if last { run.width } else { run.advance } / scale;
        }
        for part in parts {
            let run = ctx.line(part, &s.ty, Color::WHITE);
            lines.push(if last { run.width } else { run.advance } / scale);
        }
    }
    let widest = lines.iter().copied().fold(0.0, f32::max);
    match flow {
        TextFlow::NoWrap | TextFlow::Ellipsis => {
            let w = known.width.unwrap_or_else(|| match (flow, avail.width) {
                (TextFlow::Ellipsis, AvailableSpace::MinContent) => 0.0,
                _ => widest,
            });
            let h = if flow == TextFlow::Ellipsis { line_h } else { line_h * lines.len() as f32 };
            Size { width: w.ceil(), height: known.height.unwrap_or(h) }
        }
        TextFlow::Wrap => {
            let w = match (known.width, avail.width) {
                (Some(w), _) => w,
                (None, AvailableSpace::MinContent) => spans.iter().map(|s| longest_unbreakable(ctx, &s.text, &s.ty, scale)).fold(0.0, f32::max) + hanging,
                (None, AvailableSpace::MaxContent) => widest,
                (None, AvailableSpace::Definite(v)) => widest.min(v),
            };
            let h = if widest <= w && lines.len() == 1 { line_h } else { wrapped_height(ctx, id, spans, hanging, w, scale, theme) };
            Size { width: w.ceil(), height: known.height.unwrap_or(h) }
        }
    }
}

/// How long the pointer rests before hover text comes up, so that passing over a list draws nothing.
const HOVER_TEXT_DELAY: f32 = 0.4;
const HOVER_TEXT_MAX_W: f32 = 420.0;
/// Past this height an element is too big to place words by: they would land far from the pointer, so they go by the pointer instead.
const HOVER_ANCHOR_MAX_H: f32 = 64.0;

/// What the place the pointer rests on has to say: a reason on it or around it, otherwise all of a line too long for its box.
fn hover_note<A>(frame: &Frame<A>, order: &[usize], target: usize, ui: &mut Ui<'_>) -> Option<(u64, Rect, Arc<str>)> {
    let (px, py) = ui.input.pointer?;
    if target == NONE {
        return None;
    }
    // A line can be let through to what holds it, so it may lie above the target.
    let line = order.iter().rev().copied().take_while(|&i| i != target).chain(std::iter::once(target)).find(|&i| {
        let n = &frame.nodes[i];
        (n.cut_line.is_some() || matches!(n.leaf, Leaf::Text { flow: TextFlow::Ellipsis, .. })) && n.rect.contains(px, py) && n.clip_rect.contains(px, py)
    });
    let own: Option<Arc<str>> = line.and_then(|line| {
        let n = &frame.nodes[line];
        match (&n.cut_line, &n.leaf) {
            (Some(whole), _) => Some(whole.clone()),
            (None, Leaf::Text { spans, .. }) => {
                let wide: f32 = spans.iter().map(|s| s.text.split('\n').map(|part| ui.measure(part, &s.ty)).sum::<f32>()).sum();
                (wide > n.rect.w + 0.5).then(|| spans.iter().map(|s| s.text.as_ref()).collect::<String>().into())
            }
            _ => None,
        }
    });
    let mut at = target;
    while at != NONE {
        let n = &frame.nodes[at];
        if let Some(words) = &n.hover_text {
            let words = n.full_text.as_ref().filter(|_| own.is_some()).unwrap_or(words);
            let anchor = if n.rect.h > HOVER_ANCHOR_MAX_H { rect(px, py, 0.0, 18.0) } else { n.rect };
            return Some((n.id, anchor, words.clone()));
        }
        at = n.parent;
    }
    let (line, own) = (line?, own?);
    let n = &frame.nodes[line];
    let mut at = line;
    while at != NONE {
        if let Some(words) = &frame.nodes[at].full_text {
            return Some((n.id, n.rect, words.clone()));
        }
        at = frame.nodes[at].parent;
    }
    Some((n.id, n.rect, own))
}

fn paint_hover_text(ui: &mut Ui<'_>, id: u64, anchor: Rect, words: Arc<str>) {
    let (scale, theme) = (ui.scale, ui.theme);
    let spans = [Span { text: words, ty: CAPTION, color: Some(theme.text_primary) }];
    let room = HOVER_TEXT_MAX_W.min(ui.size.0 - 24.0).max(1.0);
    let size = measure_text(ui.text, id, &spans, TextFlow::Wrap, 0.0, Size { width: None, height: None }, Size { width: AvailableSpace::Definite(room), height: AvailableSpace::MaxContent }, scale, &theme);
    let (w, h) = (size.width + 16.0, size.height + 10.0);
    let x = (anchor.x + anchor.w * 0.5 - w * 0.5).clamp(4.0, (ui.size.0 - w - 4.0).max(4.0));
    let below = anchor.bottom() + 6.0;
    let y = if below + h > ui.size.1 { (anchor.y - h - 6.0).max(4.0) } else { below };
    let (layer, clip) = (ui.layer, ui.clip);
    ui.layer = LAYER_TOOLTIP;
    ui.clip = rect(0.0, 0.0, ui.size.0, ui.size.1);
    let r = rect(x, y, w, h);
    ui.acrylic(r, CONTROL_RADIUS, theme.flyout, theme.flyout_stroke);
    paint_text(ui, id, rect(x + 8.0, y + 5.0, size.width, size.height), &spans, TextFlow::Wrap, TextAlign::Start, false, 0.0, false);
    ui.layer = layer;
    ui.clip = clip;
}

/// One frame of a tree: layout, hit testing, painting, then handlers. Returns whether another frame is wanted.
pub fn render<A: 'static>(root: El<A>, app: &mut A, ui: &mut Ui<'_>) -> bool {
    with_rt(|rt| rt.frame += 1);
    let window = rect(0.0, 0.0, ui.size.0, ui.size.1);
    let scale = ui.scale;
    let theme = ui.theme;

    // ---- build
    let mut frame = Frame { nodes: Vec::with_capacity(256), taffy: TaffyTree::new(), portals: Vec::new(), capped: Vec::new() };
    let root_style_size = Size { width: length(window.w), height: length(window.h) };
    let root = root.with_style(|s| s.size = root_style_size);
    let root_idx = frame.add(root, NONE, 0x1505, NONE);

    // ---- layout
    let mut nodes_ptr = std::mem::take(&mut frame.nodes);
    {
        let roots: Vec<(taffy::tree::NodeId, bool)> = std::iter::once((nodes_ptr[root_idx].tf, true)).chain(frame.portals.iter().map(|p| (nodes_ptr[p.root].tf, p.portal.modal))).collect();
        // Taffy takes a column's height from its children before `max_w` limits its width, so text that wraps at the limit
        // is measured as one line and the column comes out too short. A node that hit its limit is laid out again at that width.
        for pass in 0..2 {
            if pass == 1 {
                let mut again = false;
                for &(tf, cap) in &frame.capped {
                    if frame.taffy.layout(tf).expect("taffy").size.width >= cap - 0.5 {
                        let mut style = frame.taffy.style(tf).expect("taffy").clone();
                        style.size.width = length(cap);
                        frame.taffy.set_style(tf, style).expect("taffy");
                        again = true;
                    }
                }
                if !again {
                    break;
                }
            }
            for &(tf, fill) in &roots {
                let avail = if fill { Size { width: AvailableSpace::Definite(window.w), height: AvailableSpace::Definite(window.h) } } else { Size { width: AvailableSpace::MaxContent, height: AvailableSpace::MaxContent } };
                frame
                    .taffy
                    .compute_layout_with_measure(tf, avail, |known, avail, _id, ctx, _style| {
                        let Some(&mut idx) = ctx else { return Size::ZERO };
                        if let (Some(w), Some(h)) = (known.width, known.height) {
                            return Size { width: w, height: h };
                        }
                        let node = &mut nodes_ptr[idx];
                        let id = node.id;
                        match &mut node.leaf {
                            Leaf::Text { spans, flow, hanging, .. } => measure_text(ui.text, id, spans, *flow, *hanging, known, avail, scale, &theme),
                            Leaf::Icon { size, .. } => Size { width: *size, height: *size },
                            Leaf::Image { natural, .. } => {
                                let (nw, nh) = (natural.0 as f32, natural.1 as f32);
                                match (known.width, known.height) {
                                    (Some(w), None) => Size { width: w, height: w * nh / nw.max(1.0) },
                                    (None, Some(h)) => Size { width: h * nw / nh.max(1.0), height: h },
                                    _ => {
                                        let max = tf_avail(avail.width);
                                        let w = nw.min(max);
                                        Size { width: w, height: w * nh / nw.max(1.0) }
                                    }
                                }
                            }
                            Leaf::Custom { measure, .. } => match measure {
                                Some(m) => {
                                    let (w, h) = m(app, ui, known.width, tf_avail(avail.width));
                                    Size { width: known.width.unwrap_or(w), height: known.height.unwrap_or(h) }
                                }
                                None => Size::ZERO,
                            },
                            Leaf::None => Size::ZERO,
                        }
                    })
                    .expect("taffy");
            }
        }
    }
    frame.nodes = nodes_ptr;

    // ---- place
    let mut order = Vec::with_capacity(frame.nodes.len());
    let place_all = |frame: &mut Frame<A>, order: &mut Vec<usize>| {
        order.clear();
        frame.place(root_idx, 0.0, 0.0, window, order);
        for i in 0..frame.portals.len() {
            let (root, modal) = (frame.portals[i].root, frame.portals[i].portal.modal);
            let layout = *frame.taffy.layout(frame.nodes[root].tf).expect("taffy");
            let (w, h) = (layout.size.width, layout.size.height);
            let (x, y) = if modal {
                (0.0, 0.0)
            } else {
                let p = &frame.portals[i].portal;
                place_popover(p, w, h, window)
            };
            // `place` adds the node's own location, which for a portal root is meaningless.
            frame.place(root, x - layout.location.x, y - layout.location.y, window, order);
        }
    };
    place_all(&mut frame, &mut order);

    // ---- hit test
    let pointer = ui.input.pointer;
    let mut target = NONE;
    if let Some((px, py)) = pointer {
        for &i in order.iter().rev() {
            let n = &frame.nodes[i];
            if n.pass_through || n.disabled && n.on.click.is_none() && n.hover_text.is_none() {
                continue;
            }
            if n.rect.contains(px, py) && n.clip_rect.contains(px, py) {
                target = i;
                break;
            }
        }
    }
    let mut hover_path = Vec::new();
    let mut at = target;
    while at != NONE {
        frame.nodes[at].hovered = true;
        hover_path.push(at);
        at = frame.nodes[at].parent;
    }

    // ---- wheel, scroll requests and scroller motion
    // The innermost scroller under the pointer takes the wheel, and what it has no room for goes on to the one around it.
    let mut wheel_left = if ui.mem.block_pointer { 0.0 } else { ui.input.scroll };
    let mut wheel_shares: Vec<(usize, f32)> = Vec::new();
    for &i in &hover_path {
        if wheel_left == 0.0 {
            break;
        }
        if frame.nodes[i].scroll.is_none() {
            continue;
        }
        let room = with_rt(|rt| rt.scrolls.get(&frame.nodes[i].id).map_or(0.0, |s| s.wheel_room(wheel_left)));
        let share = wheel_left.abs().min(room.max(0.0)).copysign(wheel_left);
        if share != 0.0 {
            wheel_shares.push((i, share));
            wheel_left -= share;
        }
    }
    let requests = with_rt(|rt| std::mem::take(&mut rt.scroll_requests));
    let mut moved = false;
    for i in order.clone() {
        let (id, axis) = (frame.nodes[i].id, frame.nodes[i].scroll);
        let Some(axis) = axis else { continue };
        let layout = *frame.taffy.layout(frame.nodes[i].tf).expect("taffy");
        let content = layout.content_size.height.max(layout.size.height);
        let r = frame.nodes[i].rect;
        let mut s = with_rt(|rt| rt.scrolls.remove(&id)).unwrap_or_else(|| if axis == ScrollAxis::YStickToEnd { Scroll::default() } else { Scroll::from_top() });
        let before = s.offset;
        s.content = content;
        s.viewport = r.h;
        for (rid, req) in &requests {
            if *rid == id {
                match *req {
                    ScrollRequest::To(y) => s.scroll_to(y),
                    ScrollRequest::ToEnd => s.scroll_to_end(),
                    ScrollRequest::By(d) => s.scroll_to(s.offset + d),
                    ScrollRequest::Shift(d) => s.shift_clamped(d),
                    ScrollRequest::Reveal { y, h } => {
                        if y < s.offset {
                            s.scroll_to(y);
                        } else if y + h > s.offset + s.viewport {
                            s.scroll_to(y + h - s.viewport);
                        }
                    }
                }
            }
        }
        if axis == ScrollAxis::YManual {
            s.stick = false;
        }
        let share = wheel_shares.iter().find(|(at, _)| *at == i).map_or(0.0, |(_, share)| *share);
        s.advance_by(ui, r, content, share);
        if axis == ScrollAxis::YManual {
            s.stick = false;
        }
        if (s.offset - before).abs() > 0.01 {
            moved = true;
        }
        with_rt(|rt| rt.scrolls.insert(id, s));
    }
    if moved {
        for n in &mut frame.nodes {
            n.hovered = false;
        }
        place_all(&mut frame, &mut order);
        target = NONE;
        if let Some((px, py)) = pointer {
            for &i in order.iter().rev() {
                let n = &frame.nodes[i];
                if !n.pass_through && n.rect.contains(px, py) && n.clip_rect.contains(px, py) {
                    target = i;
                    break;
                }
            }
        }
        hover_path.clear();
        let mut at = target;
        while at != NONE {
            frame.nodes[at].hovered = true;
            hover_path.push(at);
            at = frame.nodes[at].parent;
        }
    }

    with_rt(|rt| rt.shift = ui.input.shift);

    // ---- what the press and release did
    let mut fired: Vec<Box<dyn FnOnce(&mut A)>> = Vec::new();
    let (px, py) = pointer.unwrap_or((0.0, 0.0));
    let event = |phase: Phase, r: Rect, start: Option<(f32, f32)>| {
        let (sx, sy) = start.unwrap_or((px, py));
        PointerEvent { phase, x: px, y: py, dx: px - sx, dy: py - sy, rect: r }
    };
    let by_id = |frame: &Frame<A>, id: u64| frame.nodes.iter().position(|n| n.id == id);

    if let Some(req) = with_rt(|rt| rt.focus_request.take()) {
        ui.mem.focus = req;
    }
    // What turned invisible gives the focus up, or keys would go on landing in something nobody can see.
    if ui.mem.focus != 0 && (0..frame.nodes.len()).any(|i| frame.nodes[i].invisible && focus_within(&frame, i, ui.mem.focus)) {
        ui.mem.focus = 0;
    }
    if a11y_active() {
        a11y::dispatch_actions(&frame, ui, &mut fired);
    }

    let mut captured: Option<u64> = None;
    if ui.input.pressed {
        // A press outside a popover closes it; the press itself still lands.
        let target_portal = if target == NONE { NONE } else { frame.nodes[target].portal_root };
        for i in (0..frame.portals.len()).rev() {
            let root = frame.portals[i].root;
            if target_portal == root {
                break;
            }
            let on_anchor = frame.portals[i].portal.anchor.is_some_and(|a| hover_path.iter().any(|&h| frame.nodes[h].id == a));
            if frame.portals[i].portal.modal {
                // A modal only closes on a press on its own backdrop, which is handled by the backdrop.
                break;
            }
            if let Some(f) = frame.portals[i].portal.on_dismiss.clone()
                && !on_anchor {
                    fired.push(Box::new(move |a| f(a)));
                }
        }
        with_rt(|rt| {
            rt.press = Some((px, py));
            rt.press_path.clear();
            rt.focus_visible = false;
        });
        let press_path: Vec<u64> = hover_path.iter().map(|&h| frame.nodes[h].id).collect();
        with_rt(|rt| rt.press_path = press_path);
        // Outermost first, as capture goes in the DOM.
        for &i in hover_path.iter().rev() {
            let Some(f) = frame.nodes[i].on.press_capture.clone() else { continue };
            let on_editable = hover_path.iter().any(|&h| frame.nodes[h].editable);
            let event = PressEvent { x: px, y: py, modifier: ui.input.zoom_modifier, on_editable, rect: frame.nodes[i].rect };
            if f(app, event) {
                captured = Some(frame.nodes[i].id);
                break;
            }
        }
        ui.mem.press_keeps_focus = hover_path.iter().any(|&h| frame.nodes[h].keep_focus);
        let on_scrollbar = hover_path.iter().any(|&i| {
            let n = &frame.nodes[i];
            n.scroll.is_some() && px > n.rect.right() - 12.0 && with_rt(|rt| rt.scrolls.get(&n.id).is_some_and(|s| s.max_offset() > 0.0))
        });
        if let Some(id) = captured {
            ui.mem.active = id;
        } else if !on_scrollbar {
            let mut claimed = false;
            let mut focus_taken = false;
            for &i in &hover_path {
                let n = &frame.nodes[i];
                if matches!(n.leaf, Leaf::Custom { .. }) {
                    // The widget deals with the press on its own.
                    claimed = true;
                    focus_taken = true;
                    break;
                }
                let selectable = matches!(n.leaf, Leaf::Text { selectable: true, .. });
                if !focus_taken && n.on.focusable {
                    ui.mem.focus = n.id;
                    focus_taken = true;
                }
                if n.disabled {
                    claimed = true;
                    break;
                }
                if n.on.click.is_some() || n.on.pointer.is_some() || n.on.double_click.is_some() || selectable {
                    ui.mem.active = n.id;
                    claimed = true;
                    if let Some(f) = n.on.pointer.clone() {
                        let e = event(Phase::Down, n.rect, None);
                        fired.push(Box::new(move |a| f(a, e)));
                    }
                    break;
                }
            }
            if !focus_taken && !ui.mem.press_keeps_focus {
                ui.mem.focus = 0;
            }
            let _ = claimed;
        }
    }

    let active = ui.mem.active;
    if active != 0
        && let Some(i) = by_id(&frame, active) {
            let n = &frame.nodes[i];
            let start = with_rt(|rt| rt.press);
            if let Some(f) = n.on.pointer.clone() {
                if ui.input.released {
                    let e = event(Phase::Up, n.rect, start);
                    fired.push(Box::new(move |a| f(a, e)));
                } else if !ui.input.pressed {
                    let e = event(Phase::Move, n.rect, start);
                    fired.push(Box::new(move |a| f(a, e)));
                }
            }
            if ui.input.released {
                if hover_path.contains(&i)
                    && let Some(f) = n.on.click.clone()
                        && !n.disabled {
                            fired.push(Box::new(move |a| f(a)));
                        }
            }
        }
    if ui.input.released {
        if active != 0 {
            ui.mem.active = 0;
        }
        with_rt(|rt| rt.selecting = None);
    }
    if ui.input.double_click {
        for &i in &hover_path {
            if let Some(f) = frame.nodes[i].on.double_click.clone() {
                fired.push(Box::new(move |a| f(a)));
                break;
            }
        }
    }
    if ui.input.middle_pressed {
        for &i in &hover_path {
            if let Some(f) = frame.nodes[i].on.middle_click.clone() {
                fired.push(Box::new(move |a| f(a)));
                break;
            }
        }
    }
    if ui.input.right_pressed {
        for &i in &hover_path {
            let n = &frame.nodes[i];
            if let Some(f) = n.on.context_menu.clone() {
                let e = event(Phase::Down, n.rect, None);
                fired.push(Box::new(move |a| f(a, e)));
                break;
            }
            if matches!(n.leaf, Leaf::Custom { .. }) {
                break;
            }
        }
    }

    // ---- hover changes
    {
        let now: Vec<u64> = hover_path.iter().filter(|&&i| frame.nodes[i].on.hover_change.is_some()).map(|&i| frame.nodes[i].id).collect();
        let before = with_rt(|rt| std::mem::replace(&mut rt.hover_watched, now.clone()));
        for id in before.iter().filter(|id| !now.contains(id)) {
            if let Some(f) = by_id(&frame, *id).and_then(|i| frame.nodes[i].on.hover_change.clone()) {
                fired.push(Box::new(move |a| f(a, false)));
            }
        }
        for id in now.iter().filter(|id| !before.contains(id)) {
            if let Some(f) = by_id(&frame, *id).and_then(|i| frame.nodes[i].on.hover_change.clone()) {
                fired.push(Box::new(move |a| f(a, true)));
            }
        }
    }

    // ---- keys
    if !ui.input.keys.is_empty() {
        let keys: Vec<KeyPress> = ui.input.keys.clone();
        let focus_path: Vec<usize> = {
            let mut path = Vec::new();
            let mut at = if ui.mem.focus != 0 { by_id(&frame, ui.mem.focus).unwrap_or(NONE) } else { NONE };
            while at != NONE {
                path.push(at);
                at = frame.nodes[at].parent;
            }
            path
        };
        for k in keys {
            let mut focused: Vec<KeyHandler<A>> = focus_path.iter().filter_map(|&i| frame.nodes[i].on.key.clone()).collect();
            // A text field holds focus under its raw key, one hash short of its node's id, so the walk above misses it.
            // Its own handler still hears the keys; what it sits inside doesn't, or Backspace would reach a list's delete.
            if focus_path.is_empty()
                && ui.mem.focus != 0
                && let Some(handler) = by_id(&frame, hash_key(ui.mem.focus)).and_then(|i| frame.nodes[i].on.key.clone()) {
                    focused.push(handler);
                }
            let global: Vec<KeyHandler<A>> = order.iter().rev().filter_map(|&i| frame.nodes[i].on.key_global.clone()).collect();
            // The topmost surface that has something to dismiss: a tray nothing dismisses does not stand in the way of a menu
            // under it, though a dialog does.
            let dismiss = if k.key == Key::Escape { frame.portals.iter().rev().find(|p| p.portal.modal || p.portal.on_dismiss.is_some()).and_then(|p| p.portal.on_dismiss.clone()) } else { None };
            let default = key_default(&frame, root_idx, ui.mem.focus, &k);
            fired.push(Box::new(move |a| {
                for f in &focused {
                    if f(a, &k) {
                        return;
                    }
                }
                if let Some(d) = &dismiss {
                    d(a);
                    return;
                }
                for f in &global {
                    if f(a, &k) {
                        return;
                    }
                }
                match default {
                    Some(KeyDefault::Focus { id, reveal }) => with_rt(|rt| {
                        rt.focus_request = Some(id);
                        rt.focus_visible = true;
                        rt.scroll_requests.extend(reveal);
                        rt.repaint = true;
                    }),
                    Some(KeyDefault::Activate(f)) => f(a),
                    None => {}
                }
            }));
        }
    }

    // ---- paint
    let mut base_layer = LAYER_BASE;
    let mut modal_layer = LAYER_MODAL;
    let mut popup_layer = LAYER_POPUP;
    let saved_layer = ui.layer;
    let saved_clip = ui.clip;
    let tree_roots: Vec<(usize, Option<bool>)> = std::iter::once((root_idx, None)).chain(frame.portals.iter().map(|p| (p.root, Some(p.portal.modal)))).collect();
    let (mut modal_seen, mut popup_seen) = (false, false);
    // Painting is where selectable text and custom widgets hear a press, and a captured one is not theirs.
    let target = if captured.is_some() { NONE } else { target };
    for (root, portal) in tree_roots {
        let mut layer_of = match portal {
            None => &mut base_layer,
            Some(true) => &mut modal_layer,
            Some(false) => &mut popup_layer,
        };
        // A layer draws its boxes before its text, so one surface over another needs a layer of its own, or the text at
        // the top of the one below shows through the box of the one above: a drawer over a dialog with a close button.
        match portal {
            Some(true) if std::mem::replace(&mut modal_seen, true) => *layer_of += 1,
            Some(false) if std::mem::replace(&mut popup_seen, true) => *layer_of += 1,
            _ => {}
        }
        ui.layer = *layer_of;
        paint(&mut frame, root, app, ui, target, &mut layer_of, &mut fired);
    }
    ui.layer = saved_layer;
    ui.clip = saved_clip;

    match hover_note(&frame, &order, target, ui) {
        Some((id, anchor, words)) => {
            // Something held has no use for it, nor has what it passes over, and it stays away after the release until the pointer moves on.
            let since = if ui.mem.active != 0 {
                f32::INFINITY
            } else {
                match ui.mem.hover_note {
                    Some((on, since)) if on == id => since,
                    _ => ui.time,
                }
            };
            ui.mem.hover_note = Some((id, since));
            if since.is_finite() {
                let left = HOVER_TEXT_DELAY - (ui.time - since);
                if left <= 0.0 {
                    paint_hover_text(ui, id, anchor, words);
                } else {
                    ui.wake_after(left);
                }
            }
        }
        None => ui.mem.hover_note = None,
    }

    // The topmost hovered element with a cursor decides.
    for &i in &hover_path {
        let n = &frame.nodes[i];
        if let Some(c) = n.cursor {
            ui.cursor = c;
            break;
        }
        if n.on.click.is_some() && !n.disabled {
            ui.cursor = CursorKind::Hand;
            break;
        }
        if let Leaf::Text { selectable: true, .. } = n.leaf {
            ui.cursor = CursorKind::Text;
            break;
        }
        if matches!(n.leaf, Leaf::Custom { .. }) {
            break;
        }
    }
    // ---- remember
    let any_portal = !frame.portals.is_empty();
    let portal_rects: Vec<Rect> = frame.portals.iter().map(|portal| frame.nodes[portal.root].rect).collect();
    let focus_now = ui.mem.focus;
    let text_input = std::mem::take(&mut ui.mem.text_input_focused);
    with_rt(|rt| {
        rt.focus = focus_now;
        rt.text_input = text_input;
        rt.overlay_open = any_portal;
        rt.overlay_rects = portal_rects;
        rt.rects.clear();
        for n in &frame.nodes {
            if n.keyed {
                rt.rects.insert(n.id, n.rect);
            }
        }
        let frame_no = rt.frame;
        rt.states.retain(|_, (_, last)| frame_no.saturating_sub(*last) < 2);
        rt.docs.retain(|_, d| frame_no.saturating_sub(d.frame) < 30);
    });
    if a11y_active() {
        a11y::finish_frame(&frame, root_idx, focus_now, window, scale);
    }

    // ---- handlers, with the whole frame finished
    let any = !fired.is_empty();
    for f in fired {
        f(app);
    }
    with_rt(|rt| std::mem::take(&mut rt.repaint)) || any
}

fn place_popover<A>(p: &Portal<A>, w: f32, h: f32, window: Rect) -> (f32, f32) {
    let margin = 4.0;
    let a = match (p.anchor, p.at) {
        (_, Some((x, y))) => rect(x, y, 0.0, 0.0),
        (Some(id), _) => with_rt(|rt| rt.rects.get(&id).copied()).unwrap_or(rect(window.w * 0.5, window.h * 0.5, 0.0, 0.0)),
        _ => rect(window.w * 0.5, window.h * 0.5, 0.0, 0.0),
    };
    let cross_x = match p.align {
        Align::Start => a.x,
        Align::Center => a.x + (a.w - w) * 0.5,
        Align::End => a.right() - w,
    };
    let cross_y = match p.align {
        Align::Start => a.y,
        Align::Center => a.y + (a.h - h) * 0.5,
        Align::End => a.bottom() - h,
    };
    let (x, y) = match p.side {
        Side::Below => {
            let y = a.bottom() + p.gap;
            (cross_x, if y + h > window.h - margin && a.y - p.gap - h >= margin { a.y - p.gap - h } else { y })
        }
        Side::Above => {
            let y = a.y - p.gap - h;
            (cross_x, if y < margin && a.bottom() + p.gap + h <= window.h - margin { a.bottom() + p.gap } else { y })
        }
        Side::Right => {
            let x = a.right() + p.gap;
            (if x + w > window.w - margin && a.x - p.gap - w >= margin { a.x - p.gap - w } else { x }, cross_y)
        }
        Side::Left => {
            let x = a.x - p.gap - w;
            (if x < margin && a.right() + p.gap + w <= window.w - margin { a.right() + p.gap } else { x }, cross_y)
        }
    };
    (x.clamp(margin, (window.w - w - margin).max(margin)), y.clamp(margin, (window.h - h - margin).max(margin)))
}

/// What a key does when nothing that heard it wanted it, as a browser's default action does.
enum KeyDefault<A> {
    Focus { id: u64, reveal: Vec<(u64, ScrollRequest)> },
    Activate(Handler<A>),
}

fn key_default<A: 'static>(frame: &Frame<A>, root: usize, focus: u64, key: &KeyPress) -> Option<KeyDefault<A>> {
    if key.cmd || key.alt || key.ctrl {
        return None;
    }
    match key.key {
        Key::Tab => next_tab_stop(frame, root, focus, key.shift),
        // `<button>`: Enter and Space press what holds the focus.
        Key::Enter | Key::Char(' ') if !key.shift => {
            let n = frame.nodes.iter().find(|n| n.id == focus)?;
            if n.disabled || n.focus_under.is_some() {
                return None;
            }
            n.on.click.clone().map(KeyDefault::Activate)
        }
        _ => None,
    }
}

/// Where Tab goes from what holds `focus`, walking the stops in document order and round at the ends. Nothing, when what holds
/// it reads its own keys. With a dialog up, only that dialog and what lies over it: the focus trap upstream's `DialogShell` keeps.
fn next_tab_stop<A: 'static>(frame: &Frame<A>, root: usize, focus: u64, back: bool) -> Option<KeyDefault<A>> {
    let holder = frame.nodes.iter().position(|n| focus != 0 && (n.id == focus || n.focus_under == Some(focus)));
    if let Some(holder) = holder {
        let n = &frame.nodes[holder];
        if n.keeps_tab || (matches!(n.leaf, Leaf::Custom { .. }) && n.focus_under.is_none()) {
            return None;
        }
    } else if focus != 0 {
        // A widget with an id of its own that no element answers for.
        return None;
    }
    let mut stops = Vec::new();
    match frame.portals.iter().rposition(|p| p.portal.modal) {
        Some(modal) => frame.portals[modal..].iter().for_each(|p| walk_tab_stops(frame, p.root, &mut stops)),
        None => std::iter::once(root).chain(frame.portals.iter().map(|p| p.root)).for_each(|r| walk_tab_stops(frame, r, &mut stops)),
    }
    if stops.is_empty() {
        return None;
    }
    // With nothing focused the walk starts from what was last pressed, the browser's sequential focus starting point.
    let from = holder.or_else(|| with_rt(|rt| rt.press_path.iter().find_map(|&id| stops.iter().copied().find(|&i| frame.nodes[i].id == id))));
    let at = from.and_then(|from| stops.iter().position(|&i| i == from));
    let next = match (at, back) {
        (Some(at), false) => stops[(at + 1) % stops.len()],
        (Some(at), true) => stops[(at + stops.len() - 1) % stops.len()],
        (None, false) => stops[0],
        (None, true) => stops[stops.len() - 1],
    };
    let n = &frame.nodes[next];
    let mut reveal = Vec::new();
    let mut up = n.parent;
    while up != NONE {
        let scroller = &frame.nodes[up];
        if scroller.scroll.is_some() {
            let offset = with_rt(|rt| rt.scrolls.get(&scroller.id).map_or(0.0, |s| s.offset));
            reveal.push((scroller.id, ScrollRequest::Reveal { y: n.rect.y - scroller.rect.y + offset, h: n.rect.h }));
        }
        up = scroller.parent;
    }
    Some(KeyDefault::Focus { id: n.focus_under.unwrap_or(n.id), reveal })
}

/// A portal's root is the surface or the backdrop, where a press dismisses, and `role="presentation"` is a click that only stops
/// the click from going further. Neither is a control.
fn walk_tab_stops<A: 'static>(frame: &Frame<A>, idx: usize, stops: &mut Vec<usize>) {
    let n = &frame.nodes[idx];
    if n.inert || n.disabled {
        return;
    }
    let catcher = n.presentation || frame.portals.iter().any(|p| p.root == idx);
    let stop = n.focus_under.is_some() || n.on.focusable || (n.on.click.is_some() && !catcher);
    if stop && !n.skip_tab && n.rect.w > 0.0 && n.rect.h > 0.0 {
        stops.push(idx);
    }
    for &child in &n.children {
        walk_tab_stops(frame, child, stops);
    }
}

/// Whether the focus lies in this element, so what shows only under a pointer shows for the keyboard too: `:focus-within`.
fn focus_within<A: 'static>(frame: &Frame<A>, idx: usize, focus: u64) -> bool {
    let n = &frame.nodes[idx];
    n.id == focus || n.focus_under == Some(focus) || n.children.iter().any(|&child| focus_within(frame, child, focus))
}

fn paint<A: 'static>(frame: &mut Frame<A>, idx: usize, app: &mut A, ui: &mut Ui<'_>, target: usize, layer: &mut &mut usize, fired: &mut Vec<Box<dyn FnOnce(&mut A)>>) {
    // Everything in it runs as it always does, because a custom leaf keeps state as it paints. Then none of what it drew stays, and it has spent no layers.
    if std::mem::take(&mut frame.nodes[idx].invisible) {
        let (mark, spent, at) = (ui.paint_mark(), **layer, ui.layer);
        paint(frame, idx, app, ui, target, layer, fired);
        ui.discard_since(mark);
        **layer = spent;
        ui.layer = at;
        return;
    }
    let r = frame.nodes[idx].rect;
    let clip = frame.nodes[idx].clip_rect;
    let inner_clip = if frame.nodes[idx].clip { clip.intersect(&r) } else { clip };
    let culled = !clip.intersects(&r) && !frame.nodes[idx].children.iter().any(|_| !frame.nodes[idx].clip);

    if frame.nodes[idx].overlay {
        **layer += 1;
        ui.layer = **layer;
    }

    let parent_hovered = frame.nodes[idx].parent != NONE && frame.nodes[frame.nodes[idx].parent].hovered;
    if frame.nodes[idx].reveal && !frame.nodes[idx].reveal_force && !parent_hovered && !frame.nodes[idx].hovered && !(with_rt(|rt| rt.focus_visible) && focus_within(frame, idx, ui.mem.focus)) {
        return;
    }
    if culled {
        return;
    }
    let mut up = frame.nodes[idx].parent;
    for _ in 0..frame.nodes[idx].conceal_levels {
        if up == NONE {
            break;
        }
        if frame.nodes[up].hovered {
            return;
        }
        up = frame.nodes[up].parent;
    }

    let vis = frame.nodes[idx].vis;
    let mark = vis.opacity.map(|o| (ui.paint_mark(), o));
    ui.clip = clip;

    // Box.
    let id = frame.nodes[idx].id;
    let hovered = frame.nodes[idx].hovered && !frame.nodes[idx].disabled;
    let pressed = ui.mem.active == id;
    let mut fill = vis.bg.unwrap_or(Color::TRANSPARENT);
    if vis.hover_bg.is_some() || vis.press_bg.is_some() {
        let t = ui.animate(id ^ 0xB0B0, if hovered { 1.0 } else { 0.0 }, motion::FASTER);
        // On a solid fill a translucent hover is a wash over it, so a row that is filled in stays filled in. Between two
        // translucent fills it is the other way: a control's hover is a stronger version of its rest, not a second coat.
        let base = fill;
        let on_base = |c: Color, t: f32| if base.a == 255 && c.a < 255 { base.over(c.fade(t)) } else { base.mix(c, t) };
        if let Some(h) = vis.hover_bg {
            fill = on_base(h, t);
        }
        if pressed && hovered
            && let Some(p) = vis.press_bg {
                fill = on_base(p, 1.0);
            }
    }
    let bottom_fill = vis.bg2.unwrap_or(fill);
    if fill.a > 0 || bottom_fill.a > 0 || vis.border.is_some() {
        if bottom_fill != fill {
            ui.fill_gradient(r, vis.radii, fill, bottom_fill);
        } else {
            match vis.border {
                Some(b) => ui.card_radii(r, vis.radii, fill, b),
                None => ui.fill_radii(r, vis.radii, fill),
            }
        }
    }
    let [top, right, bottom, left] = vis.sides;
    if let Some(c) = top {
        ui.fill(rect(r.x, r.y, r.w, 1.0), 0.0, c);
    }
    if let Some(c) = right {
        ui.fill(rect(r.right() - 1.0, r.y, 1.0, r.h), 0.0, c);
    }
    if let Some(c) = bottom {
        ui.fill(rect(r.x, r.bottom() - 1.0, r.w, 1.0), 0.0, c);
    }
    if let Some(c) = left {
        ui.fill(rect(r.x, r.y, 1.0, r.h), 0.0, c);
    }

    // Leaf.
    let is_target = target == idx;
    // What is not a closure stays where it is, for the accessibility tree to read once the frame is painted.
    match &frame.nodes[idx].leaf {
        Leaf::None => {}
        Leaf::Text { spans, flow, align, selectable, hanging } => paint_text(ui, id, r, spans, *flow, *align, *selectable, *hanging, is_target),
        Leaf::Icon { icon, size, color, rotate } => {
            let (size, rotate) = (*size, *rotate);
            let c = color.unwrap_or(ui.theme.text_secondary);
            match *icon {
                IconRef::Fluent(i) => ui.icon(i, r.x + r.w * 0.5, r.y + r.h * 0.5, size, c),
                IconRef::Named(name) => ui.named_icon(name, r.x + r.w * 0.5, r.y + r.h * 0.5, size, c, rotate),
            }
        }
        Leaf::Image { id: image, natural, cover, radius } => {
            if *cover {
                ui.image_cover(*image, r, *natural, *radius, 1.0);
            } else {
                ui.image(*image, r, *radius, 1.0);
            }
        }
        Leaf::Custom { .. } => {
            if let Leaf::Custom { paint: Some(p), .. } = std::mem::replace(&mut frame.nodes[idx].leaf, Leaf::None) {
                if a11y_active() {
                    a11y::set_painting(id);
                }
                let on_top = is_target || hover_contains_target(frame, idx, target);
                let saved = ui.mem.block_pointer;
                ui.mem.block_pointer = saved || !on_top;
                ui.clip = clip.intersect(&r);
                p(app, ui, r);
                ui.mem.block_pointer = saved;
                frame.nodes[idx].cut_line = ui.cut_line.take();
            }
        }
    }

    // Children, in-flow before overlays.
    let children = std::mem::take(&mut frame.nodes[idx].children);
    let start_layer = ui.layer;
    for &c in children.iter().filter(|&&c| !frame.nodes[c].overlay).collect::<Vec<_>>() {
        ui.layer = ui.layer.max(start_layer);
        paint(frame, c, app, ui, target, layer, fired);
    }
    for &c in children.iter().filter(|&&c| frame.nodes[c].overlay).collect::<Vec<_>>() {
        paint(frame, c, app, ui, target, layer, fired);
    }
    frame.nodes[idx].children = children;

    // `:focus-visible`. A field and a terminal show the focus themselves.
    if id == ui.mem.focus && frame.nodes[idx].focus_under.is_none() && with_rt(|rt| rt.focus_visible) {
        ui.clip = inner_clip;
        let accent = ui.theme.accent;
        ui.card_radii(r, vis.radii, Color::TRANSPARENT, accent);
        ui.card_radii(rect(r.x + 1.0, r.y + 1.0, (r.w - 2.0).max(0.0), (r.h - 2.0).max(0.0)), vis.radii.map(|radius| (radius - 1.0).max(0.0)), Color::TRANSPARENT, accent);
    }

    if frame.nodes[idx].scroll.is_some() {
        ui.clip = clip;
        let saved_layer = ui.layer;
        **layer += 1;
        ui.layer = **layer;
        if let Some(mut s) = with_rt(|rt| rt.scrolls.remove(&id)) {
            s.scrollbar(ui, id ^ 0x5C5C, r);
            with_rt(|rt| rt.scrolls.insert(id, s));
        }
        ui.layer = saved_layer;
    }

    ui.clip = inner_clip;
    if let Some((m, o)) = mark {
        ui.fade_since(m, o);
    }
    let _ = fired;
}

fn hover_contains_target<A>(frame: &Frame<A>, idx: usize, target: usize) -> bool {
    let mut at = target;
    while at != NONE {
        if at == idx {
            return true;
        }
        at = frame.nodes[at].parent;
    }
    false
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
fn paint_text(ui: &mut Ui<'_>, id: u64, r: Rect, spans: &[Span], flow: TextFlow, align: TextAlign, selectable: bool, hanging: f32, is_target: bool) {
    let scale = ui.scale;
    let theme = ui.theme;
    // Aligned text that fits on one line: a document always starts its lines at the left, and this path can place it.
    let aligned_line = flow == TextFlow::Wrap && align != TextAlign::Start && !selectable && spans.len() == 1 && !spans[0].text.contains('\n') && ui.measure(&spans[0].text, &spans[0].ty) <= r.w + 0.5;
    if flow == TextFlow::Ellipsis || (flow == TextFlow::NoWrap && !selectable) || aligned_line {
        // Single-line fast path: no document, just cached runs.
        let mut x = r.x;
        if spans.len() == 1 && !spans[0].text.contains('\n') {
            let s = &spans[0];
            let color = s.color.unwrap_or(theme.text_primary);
            let w = ui.measure(&s.text, &s.ty);
            let dx = match align {
                TextAlign::Start => 0.0,
                TextAlign::Center => ((r.w - w) * 0.5).max(0.0),
                TextAlign::End => (r.w - w).max(0.0),
            };
            let y = r.y + (r.h - s.ty.line) * 0.5;
            if flow == TextFlow::Ellipsis {
                ui.label_ellipsis(r.x + dx, y, r.w, &s.text, &s.ty, color);
            } else {
                ui.label(r.x + dx, y, &s.text, &s.ty, color);
            }
            return;
        }
        let mut y = r.y;
        let line_h = spans.iter().map(|s| s.ty.line).fold(0.0, f32::max);
        for s in spans {
            let color = s.color.unwrap_or(theme.text_primary);
            let mut parts = s.text.split('\n').peekable();
            while let Some(part) = parts.next() {
                let avail = r.right() - x;
                // A run's width leaves out its trailing spaces, but the next span still starts after them.
                let advance = ui.text.line(part, &s.ty, color).advance / scale;
                if flow == TextFlow::Ellipsis {
                    ui.label_ellipsis(x, y, avail, part, &s.ty, color);
                    x += advance;
                } else {
                    ui.label(x, y, part, &s.ty, color);
                    x += advance;
                }
                if parts.peek().is_some() && flow != TextFlow::Ellipsis {
                    y += line_h;
                    x = r.x;
                }
            }
        }
        return;
    }

    // A document, shared with the measure pass.
    let key = doc_key(id, spans);
    let TextCtxRef { ctx } = TextCtxRef { ctx: &mut *ui.text };
    let mut cached = with_rt(|rt| {
        let frame = rt.frame;
        let c = rt.docs.remove(&key);
        c.map(|mut c| {
            c.frame = frame;
            c
        })
    })
    .unwrap_or_else(|| {
        let frame = with_rt(|rt| rt.frame);
        let mut doc = Doc::new((r.w * scale).max(1.0));
        begin_text_doc(ctx, &mut doc, spans, hanging);
        for s in spans {
            doc.push(ctx, &s.text, s.ty, s.color.unwrap_or(theme.text_primary));
        }
        CachedDoc { doc, frame }
    });
    // Layout rounds the box after asking for its height at the unrounded width. Reflowed at the rounded one, a line that only just
    // fitted wraps after all and the text overflows the room it took, so a pixel's difference keeps the wrapping it was measured with.
    if (cached.doc.width() - (r.w * scale).max(1.0)).abs() > scale + 0.5 {
        cached.doc.set_width(ctx, (r.w * scale).max(1.0));
    }

    if selectable {
        let to_doc = |px: f32, py: f32| ((px - r.x) * scale, (py - r.y) * scale);
        if ui.input.pressed && is_target
            && let Some((px, py)) = ui.input.pointer {
                let (x, y) = to_doc(px, py);
                let at = cached.doc.hit(x, y);
                with_rt(|rt| {
                    rt.selections.clear();
                    rt.selections.insert(id, (at, at));
                    rt.selecting = Some(id);
                });
            }
        if with_rt(|rt| rt.selecting == Some(id))
            && let Some((px, py)) = ui.input.pointer {
                let (x, y) = to_doc(px, py);
                let at = cached.doc.hit(x, y);
                with_rt(|rt| {
                    if let Some(sel) = rt.selections.get_mut(&id) {
                        sel.1 = at;
                    }
                });
                ui.animating = true;
            }
        if let Some((a, b)) = with_rt(|rt| rt.selections.get(&id).copied())
            && a != b {
                let (lo, hi) = (a.min(b), a.max(b));
                let mut rects = Vec::new();
                cached.doc.selection_rects(lo..hi, 0.0..f32::MAX, &mut rects);
                for [x, y, w, h] in rects {
                    ui.fill(rect(r.x + x / scale, r.y + y / scale, w / scale, h / scale), 2.0, theme.selection);
                }
                let copy = ui.input.keys.iter().any(|k| k.cmd && matches!(k.key, Key::Char('c') | Key::Char('C')));
                if copy {
                    let text = cached.doc.text().get(lo..hi).unwrap_or_default().to_owned();
                    ui.clipboard.set(&text);
                }
            }
    }

    let saved = ui.clip;
    ui.doc(&mut cached.doc, r.x, r.y);
    ui.clip = saved;
    with_rt(|rt| rt.docs.insert(key, cached));
}

struct TextCtxRef<'a> {
    ctx: &'a mut TextCtx,
}

// ---------------------------------------------------------------- widgets everyone needs

/// A Fluent button with a label. Styling lives here so views describe only what they are.
pub fn button<A>(label: impl Into<Arc<str>>) -> El<A> {
    ButtonLook::Standard.build(label)
}

/// The primary action.
pub fn accent_button<A>(label: impl Into<Arc<str>>) -> El<A> {
    ButtonLook::Accent.build(label)
}

/// A button that is only text until hovered.
pub fn subtle_button<A>(label: impl Into<Arc<str>>) -> El<A> {
    ButtonLook::Subtle.build(label)
}

/// A destructive action.
pub fn danger_button<A>(label: impl Into<Arc<str>>) -> El<A> {
    ButtonLook::Danger.build(label)
}

#[derive(Clone, Copy)]
enum ButtonLook {
    Standard,
    Accent,
    Subtle,
    Danger,
}

thread_local! {
    static THEME: RefCell<Theme> = RefCell::new(Theme::dark());
}

/// The theme views should read their colours from. The frame driver sets it.
pub fn theme() -> Theme {
    THEME.with(|t| *t.borrow())
}

pub fn set_theme(t: Theme) {
    THEME.with(|slot| *slot.borrow_mut() = t);
}

impl ButtonLook {
    fn build<A>(self, label: impl Into<Arc<str>>) -> El<A> {
        let t = theme();
        let base = row::<A>().items_center().justify_center().h(30.0).px(12.0).gap(6.0).radius(CONTROL_RADIUS).shrink0().role(Role::Button);
        let (fg, base) = match self {
            ButtonLook::Standard => (t.text_primary, base.bg(t.control_fill).hover_bg(t.control_fill_hover).press_bg(t.control_fill_pressed).border(t.control_stroke)),
            ButtonLook::Accent => (t.text_on_accent, base.bg(t.accent).hover_bg(t.accent_hover).press_bg(t.accent_pressed)),
            ButtonLook::Subtle => (t.text_primary, base.hover_bg(t.subtle_hover).press_bg(t.subtle_pressed)),
            ButtonLook::Danger => (Color::WHITE, base.bg(t.critical).hover_bg(t.critical.mix(Color::WHITE, 0.08)).press_bg(t.critical.mix(Color::BLACK, 0.08))),
        };
        let label: Arc<str> = label.into();
        if label.is_empty() {
            return base;
        }
        base.child(text(label).ty(BODY_SMALL).color(fg).nowrap())
    }
}

/// A square button with an icon.
pub fn icon_button<A>(name: impl Into<IconRef>, tip: impl Into<Arc<str>>) -> El<A> {
    let t = theme();
    row().center().size(28.0, 28.0).radius(CONTROL_RADIUS).hover_bg(t.subtle_hover).press_bg(t.subtle_pressed).shrink0().tooltip(tip).role(Role::Button).child(icon(name, 15.0).color(t.text_secondary))
}

/// A horizontal hairline.
pub fn divider<A>() -> El<A> {
    let t = theme();
    div().h(1.0).w_full().shrink0().bg(t.divider)
}

/// A switch. `on` is its state; a click calls the handler.
pub fn switch<A>(on: bool, on_toggle: impl Fn(&mut A) + 'static) -> El<A> {
    let t = theme();
    switch_in(on, t.accent, t.control_strong_stroke, t.text_secondary, on_toggle)
}

/// A [`switch`] whose main colour says something: `tint` stands in for the accent, and for the outline and knob while it is off.
pub fn tinted_switch<A>(on: bool, tint: Color, on_toggle: impl Fn(&mut A) + 'static) -> El<A> {
    switch_in(on, tint, tint, tint, on_toggle)
}

fn switch_in<A>(on: bool, fill: Color, stroke: Color, knob_off: Color, on_toggle: impl Fn(&mut A) + 'static) -> El<A> {
    let knob = div().size(12.0, 12.0).radius(6.0).bg(if on { theme().text_on_accent } else { knob_off });
    row()
        .items_center()
        .size(36.0, 18.0)
        .px(3.0)
        .radius(9.0)
        .shrink0()
        .when(on, |e| e.justify_end().bg(fill))
        .when(!on, |e| e.justify_start().border(stroke))
        .child(knob)
        .role(Role::Switch)
        .aria_checked(on)
        .on_click(on_toggle)
}

/// A square check box with no label.
pub fn checkbox<A>(on: bool, on_toggle: impl Fn(&mut A) + 'static) -> El<A> {
    let t = theme();
    row()
        .center()
        .size(16.0, 16.0)
        .radius(3.0)
        .shrink0()
        .when(on, |e| e.bg(t.accent).child(icon(Icon::Check, 11.0).color(t.text_on_accent)))
        .when(!on, |e| e.border(t.control_strong_stroke))
        .role(Role::CheckBox)
        .aria_checked(on)
        .on_click(on_toggle)
}

/// A radio dot with no label.
pub fn radio<A>(on: bool, on_toggle: impl Fn(&mut A) + 'static) -> El<A> {
    let t = theme();
    row()
        .center()
        .size(16.0, 16.0)
        .radius(8.0)
        .shrink0()
        .border(if on { t.accent } else { t.control_strong_stroke })
        .when(on, |e| e.child(div().size(8.0, 8.0).radius(4.0).bg(t.accent)))
        .role(Role::RadioButton)
        .aria_checked(on)
        .on_click(on_toggle)
}

/// One row of a menu.
pub struct MenuRow<A: 'static> {
    pub label: Arc<str>,
    pub icon: Option<IconRef>,
    pub hint: Option<Arc<str>>,
    pub checked: bool,
    pub radio: bool,
    pub enabled: bool,
    pub danger: bool,
    pub separated: bool,
    pub on_select: Option<Handler<A>>,
    pub submenu: Vec<MenuRow<A>>,
    /// The family its label is set in, for a menu of fonts.
    pub font: Option<Arc<str>>,
}

impl<A: 'static> MenuRow<A> {
    pub fn new(label: impl Into<Arc<str>>, on_select: impl Fn(&mut A) + 'static) -> Self {
        Self { label: label.into(), icon: None, hint: None, checked: false, radio: false, enabled: true, danger: false, separated: false, on_select: Some(Rc::new(on_select)), submenu: Vec::new(), font: None }
    }

    pub fn font(mut self, family: impl Into<Arc<str>>) -> Self {
        self.font = Some(family.into());
        self
    }

    pub fn icon(mut self, icon: impl Into<IconRef>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    pub fn hint(mut self, h: impl Into<Arc<str>>) -> Self {
        self.hint = Some(h.into());
        self
    }

    pub fn checked(mut self, on: bool) -> Self {
        self.checked = on;
        self
    }

    /// One choice of several: `menuitemradio`, which says so whether it is the one chosen or not.
    pub fn radio(mut self) -> Self {
        self.radio = true;
        self
    }

    pub fn enabled(mut self, on: bool) -> Self {
        self.enabled = on;
        self
    }

    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }

    pub fn separated(mut self) -> Self {
        self.separated = true;
        self
    }
}

/// The panel of a flyout: rows with icon, label, hint and check. Lift it with `.popover(...)`.
pub fn menu_panel<A: 'static>(rows: Vec<MenuRow<A>>, close: Handler<A>) -> El<A> {
    let t = theme();
    let mut panel = flyout_panel().min_w(160.0).max_w(420.0).p(4.0).role(Role::Menu);
    for row_data in rows {
        if row_data.separated {
            panel = panel.child(div().h(1.0).my(4.0).mx(4.0).bg(t.divider));
        }
        let fg = if !row_data.enabled { t.text_disabled } else if row_data.danger { t.critical } else { t.text_primary };
        let close = close.clone();
        let select = row_data.on_select.clone();
        let mut r = row().items_center().gap(8.0).h(30.0).px(8.0).radius(CONTROL_RADIUS).shrink0().aria_label(row_data.label.clone()).aria_disabled(!row_data.enabled);
        r = if row_data.radio {
            r.role(Role::MenuItemRadio).aria_checked(row_data.checked)
        } else if row_data.checked {
            r.role(Role::MenuItemCheckBox).aria_checked(true)
        } else {
            r.role(Role::MenuItem)
        };
        if row_data.enabled {
            r = r.hover_bg(t.subtle_hover).press_bg(t.subtle_pressed).on_click(move |a| {
                close(a);
                if let Some(f) = &select {
                    f(a);
                }
            });
        }
        if let Some(i) = row_data.icon {
            r = r.child(icon(i, 14.0).color(fg));
        }
        r = match row_data.font {
            Some(family) => r.child(family_text(row_data.label, family, BODY_SMALL, fg).flex1()),
            None => r.child(text(row_data.label).ty(BODY_SMALL).color(fg).ellipsis().flex1()),
        };
        if let Some(h) = row_data.hint {
            r = r.aria_description(h.clone()).child(text(h).ty(FINE).color(t.text_tertiary).nowrap().ml(16.0));
        }
        if row_data.checked {
            r = r.child(icon(Icon::Check, 12.0).color(t.accent));
        }
        panel = panel.child(r);
    }
    panel
}

/// One line set in a named family.
pub fn family_text<A: 'static>(label: Arc<str>, family: Arc<str>, ty: TypeStyle, color: Color) -> El<A> {
    let (measured, measured_family) = (label.clone(), family.clone());
    custom::<A>(move |_, ui, r| {
        ui.with_clip(r, |ui| {
            ui.family_label(r.x, r.y + (r.h - ty.line) * 0.5, &label, &family, &ty, color);
        });
    })
    .min_w(0.0)
    .pass_through()
    .measure(move |_, ui, known, _| {
        let run = ui.text.family_line(&measured, &measured_family, &ty, Color::WHITE);
        (known.unwrap_or(run.width / ui.scale), ty.line)
    })
}

/// The surface a flyout stands on.
pub fn flyout_panel<A>() -> El<A> {
    let t = theme();
    div().bg(t.flyout).border(t.flyout_stroke).radius(OVERLAY_RADIUS).clip()
}

/// A dropdown: `options` as a menu under a button showing the selected one.
pub fn select<A: 'static>(key: impl Hash + Clone, options: Vec<Arc<str>>, selected: usize, on_change: impl Fn(&mut A, usize) + 'static) -> El<A> {
    select_in_fonts(key, options, Vec::new(), selected, on_change)
}

/// The same with each option set in a family of its own, `<option style={{ fontFamily }}>`: `None`, or a list that stops short, is the
/// interface font.
pub fn select_in_fonts<A: 'static>(key: impl Hash + Clone, options: Vec<Arc<str>>, fonts: Vec<Option<Arc<str>>>, selected: usize, on_change: impl Fn(&mut A, usize) + 'static) -> El<A> {
    let t = theme();
    let open = use_state(("select-open", hash_key(key.clone())), || false);
    let is_open = open.get();
    let anchor = hash_key(("select", hash_key(key)));
    let label = options.get(selected).cloned().unwrap_or_else(|| Arc::from(""));
    let on_change = Rc::new(on_change);
    let toggle = open.clone();
    let mut trigger = row()
        .key(anchor)
        .items_center()
        .justify_between()
        .gap(8.0)
        .h(30.0)
        .px(10.0)
        .radius(CONTROL_RADIUS)
        .bg(t.control_fill)
        .hover_bg(t.control_fill_hover)
        .border(t.control_stroke)
        .on_click(move |_| toggle.update(|v| *v = !*v))
        .role(Role::ComboBox)
        .aria_expanded(is_open)
        .aria_haspopup(HasPopup::Menu)
        .child(text(label).ty(BODY_SMALL).ellipsis().flex1())
        .child(icon(Icon::ChevronDown, 10.0).color(t.text_secondary));
    if is_open {
        let closer = open.clone();
        let close: Handler<A> = Rc::new(move |_| closer.set(false));
        let rows = options
            .iter()
            .enumerate()
            .map(|(i, label)| {
                let f = on_change.clone();
                let row = MenuRow::new(label.clone(), move |a| f(a, i)).checked(i == selected);
                match fonts.get(i).cloned().flatten() {
                    Some(family) => row.font(family),
                    None => row,
                }
            })
            .collect();
        let dismiss = close.clone();
        let panel = menu_panel(rows, close).max_h(320.0).scroll_y().key(("select-panel", anchor)).popover(anchor, Side::Below, Align::Start, 4.0, move |a| dismiss(a));
        trigger = trigger.child(panel);
    }
    trigger
}

/// The dialog surface, centred over the window. `on_backdrop` runs on a press outside the box.
pub fn dialog<A: 'static>(width: f32, body: El<A>, on_backdrop: Option<Handler<A>>) -> El<A> {
    let t = theme();
    let backdrop = on_backdrop.clone();
    row()
        .center()
        .when(backdrop.is_some(), move |e| {
            let f = backdrop.expect("checked");
            e.on_click(move |a| f(a))
        })
        .child(
            div()
                .w(width)
                .max_h(ui_max_dialog_h())
                .bg(t.flyout)
                .border(t.flyout_stroke)
                .radius(OVERLAY_RADIUS)
                .clip()
                // Clicks on the box stay on the box.
                .on_click(|_| {})
                .role(Role::GenericContainer)
                .cursor(CursorKind::Default)
                .child(body),
        )
        .modal_layer(on_backdrop)
}

fn ui_max_dialog_h() -> f32 {
    WINDOW_H.with(|h| (*h.borrow() - 48.0).max(120.0))
}

thread_local! {
    static WINDOW_H: RefCell<f32> = const { RefCell::new(800.0) };
    static WINDOW_W: RefCell<f32> = const { RefCell::new(1200.0) };
}

pub fn set_window_size(w: f32, h: f32) {
    WINDOW_W.with(|s| *s.borrow_mut() = w);
    WINDOW_H.with(|s| *s.borrow_mut() = h);
}

/// The window's size in logical pixels, for media-query-like decisions in views.
pub fn window_size() -> (f32, f32) {
    (WINDOW_W.with(|s| *s.borrow()), WINDOW_H.with(|s| *s.borrow()))
}

/// What a text field keeps between frames.
pub struct FieldState {
    pub edit: TextEdit,
    pub synced: Arc<str>,
    was_focused: bool,
}

/// A text field whose value belongs to the caller, like a controlled `<input>`. While it holds focus its own text
/// wins; when `value` changes from outside the field takes it.
pub struct Field<A: 'static> {
    key: u64,
    value: Arc<str>,
    placeholder: Arc<str>,
    ty: TypeStyle,
    multiline: bool,
    submit_on_enter: bool,
    bare: bool,
    obscure: bool,
    controlled: bool,
    select_on_focus: bool,
    select_on_focus_range: Option<(usize, usize)>,
    max_chars: Option<usize>,
    min_h: f32,
    max_h: f32,
    ink: Option<Color>,
    align_end: bool,
    on_change: Option<Rc<dyn Fn(&mut A, String)>>,
    on_submit: Option<Rc<dyn Fn(&mut A, String)>>,
    on_key: Option<KeyHandler<A>>,
    on_blur: Option<Rc<dyn Fn(&mut A, String)>>,
}

pub fn field<A: 'static>(key: impl Hash, value: impl Into<Arc<str>>) -> Field<A> {
    Field { key: hash_key(("field", key)), value: value.into(), placeholder: Arc::from(""), ty: BODY_SMALL, multiline: false, submit_on_enter: true, bare: false, obscure: false, controlled: false, select_on_focus: false, select_on_focus_range: None, max_chars: None, min_h: 30.0, max_h: 30.0, ink: None, align_end: false, on_key: None, on_change: None, on_submit: None, on_blur: None }
}

impl<A: 'static> Field<A> {
    pub fn placeholder(mut self, p: impl Into<Arc<str>>) -> Self {
        self.placeholder = p.into();
        self
    }

    pub fn ty(mut self, ty: TypeStyle) -> Self {
        self.ty = ty;
        self
    }

    /// Grows with its text between these heights.
    pub fn multiline(mut self, min_h: f32, max_h: f32) -> Self {
        self.multiline = true;
        self.submit_on_enter = false;
        self.min_h = min_h;
        self.max_h = max_h;
        self
    }

    pub fn submit_on_enter(mut self, on: bool) -> Self {
        self.submit_on_enter = on;
        self
    }

    /// `text-align: right`, for a number in a small box.
    pub fn align_end(mut self) -> Self {
        self.align_end = true;
        self
    }

    /// Its text in this colour: a label being typed is in the ink it will be drawn in.
    pub fn color(mut self, ink: Color) -> Self {
        self.ink = Some(ink);
        self
    }

    /// No box of its own, for a surface that already draws one.
    pub fn bare(mut self) -> Self {
        self.bare = true;
        self
    }

    /// Draws every character as a bullet and refuses to copy it: `type="password"`.
    pub fn obscure(mut self, on: bool) -> Self {
        self.obscure = on;
        self
    }

    /// `value` wins even while the field holds focus, for a caller that swaps what is being edited under the caret.
    pub fn controlled(mut self) -> Self {
        self.controlled = true;
        self
    }

    /// Taking focus selects everything, so what is there is replaced by what is typed.
    pub fn select_on_focus(mut self, on: bool) -> Self {
        self.select_on_focus = on;
        self
    }

    /// Taking focus selects this byte range: a rename selects the name without its extension.
    pub fn select_on_focus_range(mut self, from: usize, to: usize) -> Self {
        self.select_on_focus_range = Some((from, to));
        self
    }

    /// `maxLength`: typing past this many characters is dropped.
    pub fn max_chars(mut self, n: usize) -> Self {
        self.max_chars = Some(n);
        self
    }

    /// Hears a key while the field holds focus, before the field acts on it: Escape dropping an edit, say. What a field sits
    /// inside does not hear its keys, so its owner listens here.
    pub fn on_key(mut self, f: impl Fn(&mut A, &KeyPress) -> bool + 'static) -> Self {
        self.on_key = Some(Rc::new(f));
        self
    }

    /// Focus left the field, with what it holds.
    pub fn on_blur(mut self, f: impl Fn(&mut A, String) + 'static) -> Self {
        self.on_blur = Some(Rc::new(f));
        self
    }

    pub fn on_change(mut self, f: impl Fn(&mut A, String) + 'static) -> Self {
        self.on_change = Some(Rc::new(f));
        self
    }

    pub fn on_submit(mut self, f: impl Fn(&mut A, String) + 'static) -> Self {
        self.on_submit = Some(Rc::new(f));
        self
    }

    pub fn build(self) -> El<A> {
        let Field { key, value, placeholder, ty, multiline, submit_on_enter, bare, obscure, controlled, select_on_focus, select_on_focus_range, max_chars, min_h, max_h, ink, align_end, on_change, on_submit, on_key, on_blur } = self;
        let state = use_state(key, || {
            let mut edit = TextEdit::new();
            edit.set_text(value.to_string());
            FieldState { edit, synced: value.clone(), was_focused: false }
        });
        let measure_state = state.clone();
        let at_rest = a11y_active().then(|| state.borrow().edit.a11y_input(key, &placeholder, multiline, obscure));
        custom::<A>(move |app, ui, r| {
            let mut guard = state.borrow_mut();
            let st = &mut *guard;
            let focused = ui.mem.focus == key;
            // Keys during a composition are the input method's, so an Enter confirming a word isn't the field's Enter.
            if focused
                && !st.edit.is_composing()
                && ui.input.ime_commit.is_none()
                && let Some(f) = &on_key {
                    for k in &ui.input.keys {
                        f(app, k);
                    }
                }
            if (!focused || controlled) && *st.synced != *value {
                st.edit.set_text(value.to_string());
                st.synced = value.clone();
            }
            st.edit.set_obscured(obscure);
            st.edit.set_single_line(!multiline);
            st.edit.set_ink(ink);
            st.edit.set_align_end(align_end);
            let resp = if bare { st.edit.show_bare(ui, key, r, &placeholder, &ty, submit_on_enter) } else { st.edit.show(ui, key, r, &placeholder, &ty, submit_on_enter) };
            if let Some(limit) = max_chars
                && st.edit.text.chars().count() > limit {
                    let kept: String = st.edit.text.chars().take(limit).collect();
                    st.edit.set_text(kept);
                }
            if resp.changed {
                let new_value = st.edit.text.clone();
                st.synced = Arc::from(new_value.as_str());
                if let Some(f) = &on_change {
                    f(app, new_value);
                }
            }
            if resp.submitted
                && let Some(f) = &on_submit {
                    f(app, st.edit.text.clone());
                }
            let now_focused = ui.mem.focus == key;
            if now_focused && !st.was_focused && select_on_focus {
                st.edit.select_all();
                st.edit.end_drag();
            }
            if now_focused && !st.was_focused
                && let Some((from, to)) = select_on_focus_range {
                    st.edit.select_range(from, to);
                    st.edit.end_drag();
                }
            if st.was_focused && !now_focused
                && let Some(f) = &on_blur {
                    f(app, st.edit.text.clone());
                }
            st.was_focused = now_focused;
        })
        .key(key)
        .focus_under(key)
        .editable()
        .min_w(0.0)
        .measure(move |_, ui, known, avail| {
            let w = known.unwrap_or(if avail.is_finite() { avail } else { 240.0 });
            if !multiline {
                return (w, min_h);
            }
            let chrome = if bare { 0.0 } else { 12.0 };
            let inner = (w - if bare { 0.0 } else { 22.0 }).max(1.0);
            let h = measure_state.borrow_mut().edit.measure(ui, &ty, inner) + chrome;
            (w, h.clamp(min_h, max_h))
        })
        .with_aria(|a| a.input = at_rest)
    }
}

/// `overflow-x: auto` for a row of elements: what does not fit slides under the sideways wheel, or the plain wheel with Shift held. Its
/// content lays out at its natural width and is drawn moved along. The other sideways scrollers here live inside a leaf and read the
/// wheel while painting, and this reads it the same way; it shows the same thin bar while the pointer is over it.
pub fn sideways<A: 'static>(key: impl Hash, content: El<A>) -> El<A> {
    let id = hash_key(("sideways", hash_key(key)));
    let inner = hash_key(("sideways-content", id));
    let offset = use_state(id, || 0.0f32);
    let view = rect_of(id).map_or(0.0, |r| r.w);
    let wide = rect_of(inner).map_or(0.0, |r| r.w);
    let max = (wide - view).max(0.0);
    let at = offset.get().clamp(0.0, max);
    let color = theme().text_tertiary.fade(0.6);
    // Whatever is under the pointer here is what is inside, so the row says when it is over it.
    let over = use_state(("sideways-over", id), || false);
    let (is_over, hovering) = (over.get(), over);
    let wheel = custom::<A>(move |_, ui, r| {
        if !is_over || max <= 0.0 {
            return;
        }
        let lines = if ui.input.scroll_is_pixels { 1.0 } else { 48.0 };
        let turned = if ui.input.scroll_x != 0.0 {
            ui.input.scroll_x
        } else if ui.input.shift {
            ui.input.scroll * lines
        } else {
            0.0
        };
        if turned != 0.0 {
            offset.set((at - turned).clamp(0.0, max));
        }
        let thumb = (r.w * r.w / wide).clamp(24.0, r.w);
        ui.fill(rect(r.x + (r.w - thumb) * at / max, r.bottom() - 4.0, thumb, 3.0), 1.5, color);
    })
    .absolute()
    .inset(0.0)
    .pass_through();
    row().key(id).min_w(0.0).clip().on_hover_change(move |_, on| hovering.set(on)).child(content.key(inner).shrink0().ml(-at)).child(wheel)
}

/// How many rows of a uniform virtual list are on screen, from the scroller's state last frame, with `overscan`
/// more on each side. Pair with spacers of the skipped height.
pub fn visible_rows(scroller: impl Hash, count: usize, row_h: f32, overscan: usize) -> std::ops::Range<usize> {
    let (offset, _, viewport) = scroll_state(scroller).unwrap_or((0.0, 0.0, 1200.0));
    let viewport = if viewport > 0.0 { viewport } else { 1200.0 };
    let first = ((offset / row_h).floor() as usize).saturating_sub(overscan);
    let last = (((offset + viewport) / row_h).ceil() as usize + overscan).min(count);
    first.min(count)..last
}

/// Handle to the set of hovered ids, should a test want it.
pub type Ids = HashSet<u64>;
