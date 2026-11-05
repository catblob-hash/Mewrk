//! What these pages share: upstream's `Common.tsx`, `DocsLink`, `SettingsLayout`, `SettingsPageHeading`, the cards and
//! rows of Appearance, `SettingsRail`, and the provider pages' `Drawer` and `DialogShell`.

use std::cell::RefCell;
use std::hash::Hash;

use mewrk_client::prelude::*;

// ---------------------------------------------------------------- links

const DOCUMENTATION_ORIGIN: &str = "https://mewrk.dev";

/// The address of one page of the site, or of one section of it.
pub fn docs_href(page: &str, section: Option<&str>) -> String {
    let locale = if mewrk_client::i18n::is_english() { "en" } else { "zh-CN" };
    let anchor = section.map_or(String::new(), |s| format!("#{s}"));
    format!("{DOCUMENTATION_ORIGIN}/{locale}/{page}.html{anchor}")
}

/// Hands a link to the default browser. Links leave the app, so a failure has nowhere better to go than the log.
pub fn open_url(app: &mut App, url: &str) {
    app.invoke_logged("open_external_url", json!({ "url": url }));
}

/// A clickable link with an arrow out, for anything that leaves the app.
pub fn link(key: impl Hash, label: impl Into<Arc<str>>, url: String) -> E {
    unkeyed_link(label, url).key(key)
}

fn unkeyed_link(label: impl Into<Arc<str>>, url: String) -> E {
    let t_ = theme();
    row()
        .items_center()
        .gap(3.0)
        .shrink0()
        .cursor(CursorKind::Hand)
        .tooltip(url.clone())
        .role(Role::Link)
        .on_click(move |app: &mut App| open_url(app, &url))
        .child(icon("ExternalLink", 10.0).color(t_.accent_text))
        .child(text(label).ty(FINE).color(t_.accent_text).nowrap())
}

/// The "Learn more" that trails a short description. Several rows of a page can lead to one section, and two elements under one key
/// are one Tab stop to the keyboard, so these go by where they are.
pub fn docs_link(page: &str, section: &str) -> E {
    unkeyed_link(t("详情", "Learn more"), docs_href(page, Some(section)))
}

/// One short line, grey, wrapping.
pub fn help(s: impl Into<Arc<str>>) -> E {
    text(s).ty(FINE).color(theme().text_tertiary)
}

/// A short line that leaves something out, ending with a link to the section that has it.
pub fn help_with_docs(s: impl Into<Arc<str>>, page: &str, section: &str) -> E {
    row().wrap().items_center().gap(4.0).child(help(s).min_w(0.0)).child(docs_link(page, section))
}

pub fn error_line(s: impl Into<Arc<str>>) -> E {
    text(s).ty(FINE).color(theme().critical).role(Role::Alert)
}

// ---------------------------------------------------------------- buttons and small things

/// Spins while it is on screen. It needs frames, so it asks for them.
pub fn spinner(size: f32) -> E {
    repaint();
    let turn = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f32() % 1.0);
    icon("RefreshCw", size).color(theme().text_secondary).rotate(turn * 360.0)
}

/// A small button with an icon before its label.
pub fn icon_label_button(base: E, name: &'static str, size: f32, color: Color) -> E {
    base.child(icon(name, size).color(color))
}

pub fn small_button(label: impl Into<Arc<str>>) -> E {
    button::<App>(label).h(26.0).px(9.0)
}

pub fn small_accent_button(label: impl Into<Arc<str>>) -> E {
    accent_button::<App>(label).h(26.0).px(9.0)
}

pub fn small_subtle_button(label: impl Into<Arc<str>>) -> E {
    subtle_button::<App>(label).h(26.0).px(9.0)
}

pub fn small_danger_button(label: impl Into<Arc<str>>) -> E {
    danger_button::<App>(label).h(26.0).px(9.0)
}

/// A button with a leading icon: the icon goes first, then the label.
pub fn with_icon(icon_el: E, label: impl Into<Arc<str>>, accent: bool) -> E {
    let t_ = theme();
    let base = if accent { accent_button::<App>("") } else { button::<App>("") };
    let fg = if accent { t_.text_on_accent } else { t_.text_primary };
    base.h(26.0).px(9.0).child(icon_el).child(text(label).ty(CAPTION).color(fg).nowrap())
}

/// A progress bar. `None` is one whose end isn't known.
pub fn progress(percent: Option<f32>) -> E {
    let t_ = theme();
    let fill = percent.unwrap_or(30.0).clamp(0.0, 100.0);
    if percent.is_none() {
        repaint();
    }
    let track = row().h(4.0).flex1().radius(2.0).bg(t_.subtle_pressed).clip().role(Role::ProgressIndicator);
    match percent {
        Some(now) => track.aria_value(f64::from(now), 0.0, 100.0).child(div().h_full().w_pct(fill).radius(2.0).bg(t_.accent)),
        None => {
            let phase = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| (d.as_secs_f32() * 0.7) % 1.0);
            track.child(div().h_full().w_pct(phase * 70.0)).child(div().h_full().w_pct(fill).radius(2.0).bg(t_.accent))
        }
    }
}

/// `ConfirmDeleteButton`: the first press arms it, the second deletes. Leaving it disarms.
pub fn confirm_delete(key: impl Hash + Clone, label: String, confirm_label: String, disabled: bool, on_delete: impl Fn(&mut App) + 'static) -> E {
    let t_ = theme();
    let armed = use_state(("confirm-delete", hash_key(key.clone())), || false);
    let is_armed = armed.get();
    let press = armed.clone();
    let leave = armed.clone();
    // What the armed button says it will do, rather than the "Confirm" it shows.
    let tip: Arc<str> = Arc::from(if is_armed { confirm_label } else { label });
    let base = row().key(("confirm-delete-btn", hash_key(key))).center().h(24.0).radius(CONTROL_RADIUS).shrink0().tooltip(tip.clone()).aria_label(tip).hover_bg(t_.critical_bg);
    let base = if is_armed { base.px(6.0).child(text(t("确认", "Confirm")).ty(FINE_STRONG).color(t_.critical).nowrap()) } else { base.w(24.0).child(icon("Trash2", 13.0).color(t_.text_tertiary)) };
    base.disabled(disabled)
        .on_hover_change(move |_: &mut App, hovered| {
            if !hovered {
                leave.set(false);
            }
        })
        // Rows that are themselves clickable sit under this button, and arming it is not a request to open them.
        .on_click(move |app: &mut App| {
            if !press.get() {
                press.set(true);
                return;
            }
            press.set(false);
            on_delete(app);
        })
}

/// A switch that can be disabled, with a key a test or a tooltip can reach.
pub fn labeled_switch(key: impl Hash, on: bool, disabled: bool, on_toggle: impl Fn(&mut App) + 'static) -> E {
    switch::<App>(on, on_toggle).key(key).disabled(disabled)
}

/// `Field` of Common.tsx: a label, its control, a hint.
pub fn field_row(label: impl Into<Arc<str>>, hint: Option<E>, control: E) -> E {
    let t_ = theme();
    col().gap(4.0).mb(12.0).child(text(label).ty(CAPTION).color(t_.text_secondary).nowrap()).child(control).child(hint)
}

/// A one-line text input with a change handler.
pub fn input(key: impl Hash, value: impl Into<Arc<str>>, placeholder: impl Into<Arc<str>>, on_change: impl Fn(&mut App, String) + 'static) -> E {
    field::<App>(key, value).placeholder(placeholder).on_change(on_change).build()
}

pub fn code_input(key: impl Hash, value: impl Into<Arc<str>>, placeholder: impl Into<Arc<str>>, on_change: impl Fn(&mut App, String) + 'static) -> E {
    field::<App>(key, value).placeholder(placeholder).ty(MONO_BODY).on_change(on_change).build()
}

pub fn keycap(s: impl Into<Arc<str>>) -> E {
    let t_ = theme();
    row().center().min_w(22.0).h(21.0).px(5.0).radius(5.0).bg(t_.control_fill).border(t_.control_stroke).child(text(s).ty(FINE).color(t_.text_secondary).nowrap())
}

/// Something the page wants on the clipboard. Handlers can't reach the clipboard, which belongs to the frame, so
/// [`clipboard_pump`] hands it over from inside the next paint.
pub fn copy_to_clipboard(s: String) {
    PENDING_COPY.with(|p| *p.borrow_mut() = Some(s));
    repaint();
}

thread_local! {
    static PENDING_COPY: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Paints nothing and does the copy. It sits at the root of the settings window.
pub fn clipboard_pump() -> E {
    custom::<App>(|_, ui, _| {
        if let Some(s) = PENDING_COPY.with(|p| p.borrow_mut().take()) {
            ui.clipboard.set(&s);
        }
    })
    .absolute()
    .size(0.0, 0.0)
    .pass_through()
}

// ---------------------------------------------------------------- pages

/// `SettingsPageHeading`: without a title, the page's description alone.
pub fn page_heading(title: Option<E>, description: E, action: Option<E>) -> E {
    let untitled = title.is_none();
    // The right padding keeps a page's actions clear of the window's close button, which sits in that corner.
    row().when(!untitled, |e| e.min_h(54.0)).when(untitled, |e| e.pb(16.0)).pr(if settings_strip() { 0.0 } else { 30.0 }).items_start().justify_between().gap(20.0).child(col().flex1().gap(3.0).child(title).child(description)).child(action)
}

pub fn page_title(s: impl Into<Arc<str>>) -> E {
    text(s).ty(BODY_STRONG).nowrap().role(Role::Heading).aria_level(3)
}

pub fn page_description(s: impl Into<Arc<str>>) -> E {
    text(s).ty(CAPTION).color(theme().text_secondary)
}

/// `settings-page`: padding 21px 22px 30px, with the top aligned to the dialog's band. On a strip the band is the strip, and the
/// page starts under it with its own top padding.
pub fn page() -> E {
    col().pt(if settings_strip() { 21.0 } else { 14.0 }).px(22.0).pb(30.0)
}

/// `.settings-layout` at `max-width: 720px`: the rail of pages becomes a strip across the top.
pub fn settings_strip() -> bool {
    window_size().0 <= 720.0
}

/// `.modal-backdrop`'s padding, the room a window leaves around itself.
pub fn dialog_margin() -> f32 {
    if settings_strip() { 8.0 } else { 24.0 }
}

/// A sidebar window's size: `<Dialog sidebar width="1040px">`, and never taller than 760 or what `dialog` itself allows.
pub fn settings_window_size() -> (f32, f32) {
    let (w, h) = window_size();
    (1040.0f32.min(w - dialog_margin() * 2.0), (h - 48.0).clamp(240.0, 760.0))
}

/// The height of a settings window's body, for a page that fills it: `min-height: 100%`.
pub fn settings_body_height() -> f32 {
    let height = settings_window_size().1;
    if settings_strip() { height - 48.0 } else { height }
}

/// `SettingsLayout`: the rail and the selected page, beside each other or the rail over the page.
pub fn settings_layout(label: &str, navigation: E, content: E) -> E {
    let layout = if settings_strip() { col().w_full().h_full().child(navigation).child(content.border_top(theme().divider)) } else { row().w_full().h_full().child(navigation).child(content) };
    layout.role(Role::Region).aria_label(label)
}

/// `settings-card`.
pub fn card(title: Option<&str>) -> E {
    let t_ = theme();
    let e = col().mt(12.0).p(14.0).radius(OVERLAY_RADIUS + 2.0).border(t_.card_stroke).bg(t_.card);
    match title {
        Some(s) => e.child(row().min_h(32.0).items_start().child(text(s.to_owned()).ty(BODY_SMALL_STRONG).nowrap())),
        None => e,
    }
}

/// `SettingRow` of Appearance: a title, an optional description, and the control beside or under them.
pub fn setting_row(title: impl Into<Arc<str>>, description: Option<E>, control: E, vertical: bool) -> E {
    let t_ = theme();
    let copy = col().gap(2.0).min_w(0.0).child(text(title).ty(BODY_SMALL)).child(description);
    // `max-width: 760px` stacks every row, with the control under its copy at the left.
    let stacked = !vertical && window_size().0 <= 760.0;
    if vertical {
        col().min_h(48.0).py(9.0).gap(9.0).border_top(t_.divider).child(copy).child(col().w_full().child(control))
    } else if stacked {
        col().min_h(48.0).py(9.0).gap(8.0).border_top(t_.divider).child(copy).child(row().items_center().child(control))
    } else {
        row().min_h(48.0).py(9.0).gap(22.0).items_center().justify_between().border_top(t_.divider).child(copy.flex1()).child(row().shrink0().items_center().child(control))
    }
}

/// A row that opens its card: `:first-child { border-top: 0 }`.
pub fn first_row(row: E) -> E {
    row.border_top(Color::TRANSPARENT)
}

/// `provider-input-group`: a bare input and whatever trails it, in one 34 px box.
pub fn input_group(input_el: E, error: bool) -> E {
    let t_ = theme();
    row().flex1().min_w(0.0).min_h(34.0).pl(10.0).pr(4.0).gap(4.0).items_center().radius(CONTROL_RADIUS + 3.0).bg(t_.control_fill).border(if error { t_.critical } else { t_.control_stroke }).child(input_el)
}

/// One code input in an input group, the way a provider's address and endpoints are drawn.
pub fn grouped_code_input(key: impl Hash, label: &str, value: impl Into<Arc<str>>, placeholder: impl Into<Arc<str>>, on_change: impl Fn(&mut App, String) + 'static) -> E {
    input_group(field::<App>(key, value).bare().placeholder(placeholder).ty(MONO_BODY).on_change(on_change).build().flex1().h(18.0).aria_label(label), false)
}

/// The eye beside a secret, which spins while it reads one.
pub fn reveal_button(key: impl Hash, label: impl Into<Arc<str>>, revealing: bool, visible: bool, disabled: bool, on_click: impl Fn(&mut App) + 'static) -> E {
    let t_ = theme();
    let glyph = if revealing { spinner(12.0) } else { icon(if visible { "Eye" } else { "EyeOff" }, 12.0).color(t_.text_secondary) };
    row().key(key).center().size(24.0, 24.0).radius(6.0).shrink0().hover_bg(t_.subtle_hover).tooltip(label).keep_focus().disabled(disabled).on_click(on_click).child(glyph)
}

/// A label, then a value, as `<dl>` rows are drawn.
pub fn fact_row(label: impl Into<Arc<str>>, value: E) -> E {
    let t_ = theme();
    row().gap(10.0).items_start().child(text(label).ty(CAPTION).color(t_.text_tertiary).nowrap().w(96.0).shrink0()).child(value.flex1())
}

// ---------------------------------------------------------------- overlays

/// The body under a dialog's title bar: name and close.
fn title_bar(key: impl Hash, title: &str, close: Handler<App>) -> E {
    let t_ = theme();
    let label = tf("关闭 {title}", "Close {title}", &[("title", &title)]);
    let _ = t_;
    row().pt(9.0).pb(8.0).pl(16.0).pr(10.0).items_center().justify_between().gap(12.0).child(text(title.to_owned()).ty(BODY_STRONG).ellipsis().flex1().role(Role::Heading).aria_level(2)).child(icon_button::<App>("X", label).key(key).on_click(move |a: &mut App| close(a)))
}

/// `Dialog` of Common.tsx and `DialogShell`: a one-line title, a way out in the corner, a scrolling body, a one-line button row, and no
/// rules fencing the body off from either. Escape and the backdrop close it.
pub fn modal(key: impl Hash + Clone, title: &str, description: Option<E>, width: f32, body: E, footer: Option<E>, on_close: impl Fn(&mut App) + 'static) -> E {
    let close: Handler<App> = Rc::new(on_close);
    let close_btn = close.clone();
    let hkey = hash_key(key);
    let described = description.is_some();
    let inner = col()
        .max_h(window_size().1 - 48.0)
        .child(title_bar(("modal-close", hkey), title, close_btn))
        // Not `flex1`: the box is as tall as its content up to its maximum, so a zero basis would collapse the body to nothing.
        .child(col().key(("modal-body", hkey)).min_h(0.0).scroll_y().pt(4.0).px(20.0).pb(16.0).child(description.map(|d| col().key(("modal-description", hkey)).role(Role::Group).mb(14.0).child(d))).child(body))
        .child(footer.map(|f| row().pt(6.0).pb(12.0).px(12.0).gap(8.0).justify_end().items_center().child(f)));
    dialog(width, inner, Some(close)).aria_label(title).when(described, |e| e.aria_describedby(("modal-description", hkey)))
}

/// `Drawer`: floats on the right so the list beside it stays visible.
pub fn drawer(key: impl Hash + Clone, title: &str, subtitle: Option<String>, wide: bool, body: E, footer: Option<E>, on_close: impl Fn(&mut App) + 'static) -> E {
    let t_ = theme();
    let close: Handler<App> = Rc::new(on_close);
    let (close_btn, close_backdrop, close_esc) = (close.clone(), close.clone(), close);
    let close_label = tf("关闭 {title}", "Close {title}", &[("title", &title)]);
    let hkey = hash_key(key);
    let w = (if wide { 520.0f32 } else { 400.0 }).min(window_size().0 - 24.0);
    let panel = col()
        .key(("drawer", hkey))
        .w(w)
        .my(10.0)
        .mr(10.0)
        .bg(t_.flyout)
        .border(t_.flyout_stroke)
        .radius(OVERLAY_RADIUS)
        .shadow(24.0, 12.0, t_.shadow)
        .clip()
        .on_click(|_| {})
        .role(Role::GenericContainer)
        .child(
            row().min_h(54.0).pt(13.0).pb(11.0).pl(16.0).pr(12.0).gap(12.0).items_start().border_bottom(t_.divider).child(col().flex1().min_w(0.0).gap(3.0).child(text(title.to_owned()).ty(BODY_STRONG).role(Role::Heading).aria_level(3)).child(subtitle.map(|s| text(s).ty(FINE).color(t_.text_secondary)))).child(icon_button::<App>("X", close_label).key(("drawer-close", hkey)).on_click(move |a: &mut App| close_btn(a))),
        )
        .child(col().key(("drawer-body", hkey)).flex1().min_h(0.0).scroll_y().py(14.0).px(16.0).child(body))
        .child(footer.map(|f| row().min_h(50.0).py(9.0).pl(16.0).pr(12.0).gap(7.0).justify_end().items_center().border_top(t_.divider).child(f)));
    row().justify_end().items_stretch().bg(t_.smoke).on_click(move |a: &mut App| close_backdrop(a)).child(panel).modal_layer(Some(close_esc)).aria_label(title)
}

// ---------------------------------------------------------------- the rail

pub struct RailSearch {
    pub key: &'static str,
    pub value: String,
    pub label: &'static str,
    pub placeholder: &'static str,
    pub on_change: Rc<dyn Fn(&mut App, String)>,
}

pub struct RailFilter {
    pub label: &'static str,
    pub value: usize,
    pub neutral: usize,
    pub options: Vec<&'static str>,
    pub on_change: Rc<dyn Fn(&mut App, usize)>,
}

/// `SettingsRail`: search and filtering, a scrolling list, and a fixed footer.
pub fn settings_rail(scroller_key: &'static str, search: Option<RailSearch>, filter: Option<RailFilter>, rows: Vec<E>, footer: Option<E>) -> E {
    let t_ = theme();
    let search_el = search.map(|s| {
        let RailSearch { key, value, label, placeholder, on_change } = s;
        let has = !value.is_empty();
        let clear = on_change.clone();
        let change = on_change.clone();
        let escape = on_change;
        let filter_el = filter.map(rail_filter);
        col().pt(9.0).px(9.0).child(
            row()
                .key(("rail-search-box", key))
                .h(30.0)
                .items_center()
                .gap(4.0)
                .pl(8.0)
                .pr(3.0)
                .radius(CONTROL_RADIUS)
                .bg(t_.control_fill)
                .border(t_.control_stroke)
                .child(icon("Search", 13.0).color(t_.text_tertiary))
                .child(
                    field::<App>(("rail-search", key), value)
                        .bare()
                        .placeholder(if placeholder.is_empty() { label } else { placeholder })
                        .on_change(move |a, v| change(a, v))
                        .build()
                        .flex1()
                        .h(18.0)
                        .aria_label(label)
                        // Escape clears the search before anything else hears it.
                        .on_key(move |a: &mut App, k: &KeyPress| {
                            if k.key == Key::Escape && has {
                                escape(a, String::new());
                                return true;
                            }
                            false
                        }),
                )
                .when(has, |e| e.child(icon_button::<App>("X", t("清空搜索", "Clear search")).key(("rail-search-clear", key)).size(20.0, 20.0).on_click(move |a: &mut App| clear(a, String::new()))))
                .child(filter_el),
        )
    });
    col()
        .w(216.0)
        .shrink0()
        .h_full()
        .border_right(t_.divider)
        .child(search_el)
        .child(col().key(("rail-scroller", scroller_key)).flex1().min_h(0.0).scroll_y().pt(8.0).pb(2.0).px(9.0).gap(3.0).children(rows))
        .child(footer.map(|f| col().pt(8.0).px(9.0).pb(9.0).child(f)))
}

fn rail_filter(filter: RailFilter) -> E {
    let t_ = theme();
    let RailFilter { label, value, neutral, options, on_change } = filter;
    let open = use_state(("rail-filter-open", label), || false);
    let is_open = open.get();
    let toggle = open.clone();
    let closer = open.clone();
    let active = value != neutral;
    let mut button = row().key(("rail-filter", label)).center().size(20.0, 20.0).radius(CONTROL_RADIUS).tooltip(label).hover_bg(t_.subtle_hover).when(active, |e| e.bg(t_.subtle_pressed)).aria_haspopup(HasPopup::Menu).aria_expanded(is_open).on_click(move |_: &mut App| toggle.update(|v| *v = !*v)).child(icon("Filter", 12.0).color(if active { t_.text_primary } else { t_.text_tertiary }));
    if is_open {
        let close: Handler<App> = Rc::new(move |_| closer.set(false));
        let rows = options
            .into_iter()
            .enumerate()
            .map(|(i, s)| {
                let f = on_change.clone();
                MenuItem::new(s, move |a| f(a, i)).checked(i == value).radio()
            })
            .collect();
        let dismiss = close.clone();
        button = button.child(menu_panel(rows, close).min_w(152.0).key(("rail-filter-menu", label)).popover(("rail-filter", label), Side::Below, Align::End, 4.0, move |a| dismiss(a)));
    }
    button
}

/// `ProviderAvatar`: the first character, never a vendor logo.
pub fn avatar(name: &str, round: bool) -> E {
    let t_ = theme();
    let initial: String = name.trim().chars().next().map_or_else(|| "?".to_owned(), |c| c.to_uppercase().collect());
    let (size, radius) = if round { (20.0, 10.0) } else { (22.0, 6.0) };
    row().center().size(size, size).radius(radius).shrink0().bg(t_.subtle_pressed).aria_hidden(true).child(text(initial).ty(FINE).color(t_.text_secondary).nowrap())
}

/// `SettingsRailRow`: a row with an avatar and a status dot.
pub fn rail_row(key: impl Hash, label: &str, selected: bool, active: bool, on_select: impl Fn(&mut App) + 'static) -> E {
    let t_ = theme();
    row()
        .key(key)
        .w_full()
        .min_h(30.0)
        .pl(5.0)
        .pr(6.0)
        .gap(7.0)
        .items_center()
        .radius(CONTROL_RADIUS)
        .when(selected, |e| e.bg(t_.subtle_pressed).aria_current(AriaCurrent::True))
        .hover_bg(t_.subtle_hover)
        .on_click(on_select)
        .child(avatar(label, false))
        .child(text(label.to_owned()).ty(if selected { CAPTION_STRONG } else { CAPTION }).ellipsis().flex1())
        .child(row().center().size(20.0, 20.0).shrink0().when(active, |e| e.child(status_dot())))
}

pub fn status_dot() -> E {
    div().size(6.0, 6.0).radius(3.0).bg(theme().success)
}

/// `SettingsRailEmpty`.
pub fn rail_empty(icon_name: &'static str, title: &str, description: &str) -> E {
    let t_ = theme();
    col().py(24.0).px(8.0).items_center().gap(4.0).child(icon(icon_name, 18.0).color(t_.text_tertiary)).child(text(title.to_owned()).ty(FINE_STRONG).color(t_.text_secondary).text_center()).child(text(description.to_owned()).ty(FINE).color(t_.text_tertiary).text_center())
}

/// `SettingsNavigation`: the rail of pages under the window's name.
pub fn settings_nav<V: Copy + PartialEq + 'static>(label: &str, window_title: &str, groups: Vec<(Option<&str>, Vec<(V, &'static str, &str)>)>, selected: V, key_of: impl Fn(V) -> u64, on_select: impl Fn(&mut App, V) + 'static) -> E {
    let t_ = theme();
    let on_select = Rc::new(on_select);
    // At `max-width: 860px` the rail narrows; at 720px it is a strip across the top that scrolls sideways, whose far end stays clear of the
    // close button.
    let strip = settings_strip();
    let width = if window_size().0 <= 860.0 { 154.0 } else { 190.0 };
    let frame = if strip { row().items_center().min_h(36.0) } else { col().w(width).shrink0().h_full().pt(0.0).pb(11.0).px(9.0).border_right(t_.divider) };
    let window_name = text(window_title.to_owned()).ty(BODY_SMALL_STRONG).role(Role::Heading).aria_level(2);
    let mut nav = frame.gap(2.0).when(!strip, |e| e.bg(t_.card_secondary)).child(if strip { row().px(9.0).items_center().child(window_name.nowrap()) } else { row().h(48.0).px(9.0).items_center().child(window_name.ellipsis().flex1()) });
    for (title, items) in groups {
        if let Some(s) = title {
            nav = nav.child(text(s.to_owned()).ty(FINE).color(t_.text_tertiary).pt(9.0).pb(2.0).px(9.0).nowrap());
        }
        for (view, icon_name, label) in items {
            let active = view == selected;
            let f = on_select.clone();
            nav = nav.child(
                row()
                    .key(("settings-nav", key_of(view)))
                    .h(36.0)
                    .px(9.0)
                    .gap(8.0)
                    .items_center()
                    .radius(OVERLAY_RADIUS - 1.0)
                    .when(active, |e| e.bg(t_.subtle_pressed).aria_current(AriaCurrent::True))
                    .hover_bg(t_.subtle_hover)
                    .on_click(move |a: &mut App| f(a, view))
                    .child(icon(icon_name, 16.0).color(if active { t_.text_primary } else { t_.text_secondary }))
                    .child({
                        let name = text(label.to_owned()).ty(if active { BODY_SMALL_STRONG } else { BODY_SMALL }).color(if active { t_.text_primary } else { t_.text_secondary });
                        if strip { name.nowrap() } else { name.ellipsis().flex1() }
                    }),
            );
        }
    }
    let nav = nav.role(Role::Navigation).aria_label(label);
    if strip { row().items_center().min_h(48.0).mr(48.0).py(6.0).px(9.0).child(sideways("settings-strip", nav)) } else { nav }
}
