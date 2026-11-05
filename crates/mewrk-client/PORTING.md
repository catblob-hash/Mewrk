# Porting Mewrk's renderer to Rust

Upstream is [catblob-hash/Mewrk-previous](https://github.com/catblob-hash/Mewrk-previous): `src/` is the React renderer, `src-tauri/` the host. The host is already
here unchanged as `crates/mewrk-host/host`, so every `invoke("x", {...})` in upstream's TypeScript works here with the
same command name and argument JSON (camelCase, exactly what the TypeScript sends).

**What parity means.** Same structure, same layout, same features, same behaviour, same strings (`t(zh, en)` with both
languages copied from upstream). The look is not copied: colours, shadows and fonts stay Fluent (`fluent::el::theme()`).
Sizes and spacing that decide layout (a 264 px sidebar, a 28 px row) do come from upstream's CSS. Don't drop features,
edge cases or error messages to save time. If something truly can't be done, say so in your report.

## Writing a view

A view is a function returning an element. It runs every frame, so it only reads; handlers change state.

```rust
use mewrk_client::prelude::*;

pub fn sidebar_row(app: &mut App, conv_id: &str, title: &str) -> E {
    let t_ = theme();
    let open = use_state(("row-menu", conv_id), || false);   // React's useState. The key is global.
    let id = conv_id.to_owned();
    row().key(("conv", conv_id)).items_center().gap(7.0).h(28.0).px(7.0).radius(7.0)
        .hover_bg(t_.subtle_hover)
        .on_click(move |app: &mut App| app.select_conversation(&id))
        .child(icon("Folder", 14.0))                         // Lucide name as imported upstream
        .child(text(title.to_owned()).ty(BODY_SMALL).ellipsis().flex1())
        .child(icon_button("Ellipsis", t("更多", "More")).key(("conv-menu", conv_id)).reveal_unless(open.get()).on_click({
            let open = open.clone(); move |_| open.set(true)
        }))
}
```

- `div()`/`col()` stack vertically, `row()` is `display:flex`. Flex and grid come from taffy; methods mirror CSS names
  (`gap`, `items_center`, `justify_between`, `flex1`, `grow`, `shrink0`, `w`, `h`, `min_w`, `max_h`, `p`, `px`, `mt`,
  `absolute`, `top`, ...). `.with_style(|s| ...)` reaches any other taffy property, grid tracks for one.
- Text: `text(s).ty(BODY_SMALL).color(c)`, `.nowrap()`, `.ellipsis()`, `.selectable()`; rich text with `spans(vec![...])`.
- Scrolling: `.scroll_y()` with a `.key(...)`; `scroll_request(key, ScrollRequest::ToEnd)`, `scroll_state(key)`,
  `visible_rows` for long uniform lists. `.scroll_stick()` follows the end like a chat.
- Overlays: `.popover(anchor_key, Side::Below, Align::Start, gap, on_dismiss)`, `.popover_at(x, y, on_dismiss)`, `dialog(...)`.
  These lift out of the tree wherever you write them. Anchor with the key of the trigger. `menu_panel(rows, close)` is a menu.
- Widgets: `button`, `accent_button`, `subtle_button`, `danger_button`, `icon_button`, `switch`, `checkbox`, `radio`,
  `select`, `divider`, `spacer`, `field(key, value).on_change(...).build()`, `flyout_panel`. If you need a widget used
  across surfaces, add it to `crates/fluent/src/el.rs` (keep additions small; others build on it).
- Anything that isn't boxes and text: `custom(|app, ui, rect| ...)` paints with the immediate-mode `fluent::Ui`
  (`TextEdit`, `MdStream`, `mewrk_term`, ...). `.measure(|app, ui, known_w, avail_w| (w, h))` sizes it.
- Handlers get `&mut App` and may not borrow from the view, so clone what they need into `move` closures.
- Events: `on_click`, `on_double_click`, `on_context_menu`, `on_pointer` (press, moves, release; captured), `on_key`
  (while focused inside), `on_key_global`, `on_hover_change`. A click goes to the deepest element with a handler.
- `rect_of(key)` is an element's rectangle last frame, what `getBoundingClientRect` was for.
- `repaint()` asks for another frame; handlers and `Local::set` do so on their own. A running timer needs `repaint()`
  or `ui.wake_after`. Nothing renders while idle, so don't poll in a view: post from a task instead.

## Reaching the host

```rust
app.invoke::<Vec<model::ForkDecisionRecord>>("list_fork_decisions", json!({"conversationId": id}), |app, result| { ... });
// `await`-heavy logic reads best on a task thread, sequentially:
app.spawn(move |task| {
    let doc: Value = match task.invoke("load_conversation", json!({...})) { Ok(v) => v, Err(e) => return task.post(move |app| app.report(e)) };
    task.update(move |app| app.adopt(doc));
});
let channel = app.open_channel(|app, event| { ... });        // new Channel(); onmessage runs on the UI thread
app.invoke_logged("run_model", json!({"onEvent": channel.arg(), ...}));
app.on_push_event(|app, event| match event["type"].as_str() { ... });
```

`mewrk_client::model` is upstream's `model.rs`: `AppDocument`, `Conversation`, `ContextItem`, and so on. Prefer these over
new types, and `Value` only for shapes with no Rust type. `app.local::<T>()` holds a module's own state for the app's
lifetime; `use_state` holds a view's while it's on screen.

## Rules for working here

- You own only the files you're told you own. Others are working in the same workspace at the same time, and a broken
  build in your crate stops only you. If something you need from another area doesn't exist yet, call a small function
  in your own crate with a TODO naming what it should call, and list it in your report.
- Comments are English, and say why rather than what. Match the code around you. Don't add dependencies with C code.
- Build with `cargo check -p <your crate>`; cargo's lock may make you wait for others, which is normal. Write tests
  (scripted, headless, like `crates/fluent/tests/el.rs`) for logic and interactions. Run them before reporting.
- Report honestly: what's done, what's verified by running it, and what's left, with upstream file names.

## Accessibility

`El` has an accessibility tree (`crates/fluent/src/el/a11y.rs`, on AccessKit), built only while an assistive client is listening. Most
of it needs nothing from a surface: text is a label; `on_click` is a button named by what it says, or by its `.tooltip` (a reader's words only, nothing is drawn for it: hover draws only `.disabled_reason`, and what an ellipsis cut, or `.full_text` for that line) when it says nothing;
a tooltip beside words is a description; `field` and any `TextEdit` are text inputs with their value, caret and selection; a scroller
scrolls; icons are decoration; layout stays out; portals are children of the window and a modal hides what is behind it. `button`,
`icon_button`, `switch`, `checkbox`, `radio`, `select`, `menu_panel`, `dialog`, `popover_menu`, `context_menu` and `page_tabs` carry their
roles and state already.

What a surface adds is what upstream's markup says and the element can't. Copy it from the upstream file, with upstream's wording:

| Upstream | Here |
|---|---|
| `aria-label={t("关闭", "Close")}` | `.aria_label(t("关闭", "Close"))` |
| `role="menu"`, `role="tab"`, ... | `.role(Role::Menu)`, `.role(Role::Tab)`, ... |
| `role="presentation"`, or a `div` with an `onClick` that only stops the click | `.role(Role::GenericContainer)` |
| `aria-expanded`, `aria-selected`, `aria-checked`, `aria-pressed`, `aria-disabled`, `aria-busy`, `aria-invalid` | `.aria_expanded(open)`, and so on with a `bool` |
| `aria-valuenow` with `aria-valuemin`, `aria-valuemax`; `aria-valuetext` | `.aria_value(now, min, max)`; `.aria_value_text(words)` |
| `aria-current`, `aria-haspopup`, `aria-orientation`, `aria-level`, `aria-keyshortcuts` | `.aria_current(AriaCurrent::Page)`, `.aria_haspopup(HasPopup::Menu)`, `.aria_orientation(..)`, `.aria_level(2)`, `.aria_keyshortcuts(..)` |
| `aria-labelledby`, `aria-describedby`, `aria-controls` | `.aria_labelledby(key)`, `.aria_describedby(key)`, `.aria_controls(key)`, by the other element's `.key` |
| `aria-hidden` on something that says words | `.aria_hidden(true)`; on an icon it is already so |
| `role="alert"`, `role="status"`, `aria-live` | `.role(Role::Alert)`, `.role(Role::Status)`, `.aria_live(Live::Polite)` |
| a hidden `aria-live` span that only says a message | `announce(t("项目顺序已更新", "Project order updated"))`, `announce_assertive(..)` |

- These calls are cheap and keep nothing while nobody listens, so write them in the view. What goes into them is still worked out: guard a string
  that takes work to build with `a11y_active()`.
- Controls with no words of their own have no name until they are given one: `switch(..).aria_label(title)`, `field(..).build().aria_label(label)`.
  A `custom` leaf says nothing unless told: `.aria_label(words)` makes it a label, and `.role(..)` something else.
- A `custom` leaf with a `TextEdit` needs only its label. Giving the leaf `.key(id)` with the id the edit holds focus under makes them one node, as `field`
  and the composer do.
- A slider's `on_key` gets arrow keys for increment and decrement. A slider with no keys gives `.on_aria_value(|app, value| ...)`.
- A leaf that paints things a reader should stop on says what they are with `.aria_children(tree)`, built under `a11y_active()`: `AriaChild` has a role, words, a
  rectangle in the leaf's coordinates and `on_press`, reached the way `on_click` is. They stand where the leaf does. A message does this
  (`crates/ui/common/src/markdown/reading.rs`): headings, links, lists, tables, code with its copy button, formulas, pictures and disclosures.
- A table of code (`mewrk_pane_git::code_table::table_leaf`) adds its rows near the view as labels over the leaf, built only while a client listens: upstream's
  are text in the DOM with their numbers `aria-hidden`. The terminal's cells stay out because they stay out upstream: its xterm has no `screenReaderMode`, so
  its rows are `aria-hidden` and a reader has its one input, `.role(Role::TextInput)` on the leaf here.
- To test: `set_a11y_active(true)` before the first frame, then `take_a11y_update()` after one; `crates/fluent/tests/a11y.rs` reads updates through
  AccessKit's own consumer, and `crates/ui/common/tests/suite.rs` through the nodes. `mewrk --a11y-probe out.json --a11y-steps 'press:NAME'` asks a real
  window through AppKit.
