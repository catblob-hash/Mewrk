//! The top bar of `App.tsx` and `PaneToolbar.tsx`: the conversation's title, its project, the save chip and the pane toggles.

use mewrk_client::doc::DocumentSaveStatus;
use mewrk_client::prelude::*;
use mewrk_client::side_panes::{open_preview_session, pane_is_open, shown_tasks_tab, side_pane_layout_for, SidePaneLayout, TasksPaneTab};
use mewrk_client::workspaces::is_temporary_workspace;
use mewrk_pane_terminal::new_terminal_sections;

use crate::actions::{close_pane, open_git_review, open_pane, toggle_pane, toggle_preview, toggle_tasks_tab};
use crate::chrome::{drag_region, shell_nav_closed_width, shell_nav_inset, TOPBAR_HEIGHT};
use crate::effects::review_pages;
use crate::menu::{MenuItem, MenuSection, PopoverMenu, Submenu};
use crate::panes::{new_terminal_menu, preview_workspace_menu_items};
use crate::state::{shell, TitleDraft};
use crate::widgets::spinner;

/// `.pane-toolbar__button`.
const BUTTON: f32 = 32.0;

/// `saveStatusChip`.
fn save_status_chip(app: &mut App) -> Option<E> {
    // `@media (max-width: 720px)`: the toolbar needs the room.
    if window_size().0 <= 720.0 {
        return None;
    }
    let t_ = theme();
    let status = app.save_status();
    let reason = app.save_failure_reason();
    let (lead, label, color): (E, &str, Color) = match status {
        DocumentSaveStatus::Saving => (spinner(12.0), t("保存中", "Saving"), t_.text_tertiary),
        DocumentSaveStatus::Error => (icon("CircleAlert", 12.0).color(t_.critical), t("保存失败", "Save failed"), t_.critical),
        _ => return None,
    };
    let mut chip: E = row().aria_live(Live::Polite).h(26.0).shrink0().items_center().gap(4.0).px(5.0).child(lead).child(text(label).ty(FINE).color(color).nowrap());
    if status == DocumentSaveStatus::Error
        && let Some(reason) = reason
    {
        chip = chip.tooltip(reason.clone()).child(text(reason).ty(FINE).color(color).ellipsis().max_w(240.0));
    }
    Some(chip)
}

/// `finishTitleRename`.
fn finish_title_rename(app: &mut App) {
    let Some(draft) = shell(app).title_draft.take() else { return };
    let title = draft.value.trim().to_owned();
    let Some(workspace) = app.active_workspace_id() else { return };
    let current = app.find_conversation(&workspace, &draft.conversation_id).map(|c| c.title.clone());
    if app.active_conversation_id().as_deref() == Some(draft.conversation_id.as_str())
        && let Some(current) = current
        && !title.is_empty()
        && title != current
    {
        app.rename_conversation(&workspace, &draft.conversation_id, &title);
    }
    repaint();
}

/// The conversation's title and its project.
fn conversation_title(app: &mut App, conversation_id: &str, title: &str) -> E {
    let t_ = theme();
    // A draft has no conversation to rename yet, and a project being deleted takes no edits.
    let locked = app.draft_is_active() || crate::effects::active_workspace_deleting(app);
    let project = app.active_workspace().map(|workspace| if is_temporary_workspace(workspace) { t("临时项目", "Temporary project").to_owned() } else { workspace.name.clone() }).unwrap_or_else(|| t("未选择项目", "No project selected").to_owned());
    let draft = shell(app).title_draft.clone().filter(|draft| draft.conversation_id == conversation_id);
    let title_type = fluent::TypeStyle { size: 13.5, line: 20.0, weight: fluent::Weight::Semibold, mono: false, italic: false, serif: false };
    let heading: E = match draft {
        Some(draft) => {
            let field_key = ("conversation-title", conversation_id);
            let sizer_text = draft.value.clone();
            let input = field::<App>(field_key, draft.value)
                .ty(title_type)
                .select_on_focus(true)
                .on_change(|app, value| {
                    if let Some(draft) = &mut shell(app).title_draft {
                        draft.value = value;
                    }
                })
                .on_submit(|app, _| finish_title_rename(app))
                .on_key(|app, press| {
                    if press.key == Key::Escape {
                        shell(app).title_draft = None;
                        return true;
                    }
                    false
                })
                .on_blur(|app, _| finish_title_rename(app))
                .build()
                .aria_label(t("对话标题", "Conversation title"));
            // Grows with what it holds, up to a limit. What sizes it is the same text, drawn clear beside the field.
            let sizer = row().px(12.0).opacity(0.0).pass_through().child(text(sizer_text).ty(title_type).nowrap());
            div().key("conversation-title-field").min_w(120.0).max_w(420.0).h(26.0).child(sizer).child(div().absolute().inset(0.0).child(input))
        }
        None => {
            let mut name: E = row().key("conversation-title-name").h(26.0).px(6.0).radius(6.0).items_center().min_w(0.0).with_style(|s| s.flex_shrink = 1.0).child(text(title.to_owned()).ty(title_type).ellipsis());
            if !locked {
                let (id, value) = (conversation_id.to_owned(), title.to_owned());
                name = name.hover_bg(t_.subtle_hover).cursor(CursorKind::Text).tooltip(t("点击重命名", "Click to rename")).on_click(move |app: &mut App| {
                    shell(app).title_draft = Some(TitleDraft { conversation_id: id.clone(), value: value.clone() });
                    focus_request(("field", ("conversation-title", id.as_str())));
                });
            }
            if a11y_active() {
                name = name.role(Role::Button).aria_disabled(locked).aria_label(if locked { title.to_owned() } else { tf("对话标题：{title}，点击重命名", "Conversation title: {title}. Click to rename", &[("title", &title)]) });
            }
            name
        }
    };
    // `<h1>`, around the button or the field.
    let heading: E = row().role(Role::Heading).aria_level(1).aria_label(title).min_w(0.0).with_style(|s| s.flex_shrink = 1.0).child(heading);
    row().min_w(0.0).ml(-6.0).items_center().gap(3.0).child(heading).child(text(project).ty(CAPTION).color(t_.text_tertiary).ellipsis().with_style(|s| s.flex_shrink = 1.0))
}

fn indicator() -> E {
    let t_ = theme();
    // Placed against the button box rather than the icon, so it cannot drift with whatever glyph the button carries.
    div().absolute().top(5.0).right(5.0).size(6.0, 6.0).radius(3.0).bg(t_.accent).pass_through()
}

/// One toggle's look: `pressed` is "the pane is open", not "the surface is busy".
fn toggle_look(key: impl std::hash::Hash, name: &'static str, tooltip: &str, pressed: bool, disabled: bool, activity: bool) -> E {
    let t_ = theme();
    let fg = if disabled { t_.text_disabled } else if pressed { t_.text_primary } else { t_.text_secondary };
    let mut b: E = row().key(key).center().size(BUTTON, BUTTON).radius(CONTROL_RADIUS.max(6.0)).shrink0().tooltip(tooltip.to_owned()).child(icon(name, 18.0).color(fg));
    if pressed {
        b = b.bg(t_.subtle_hover);
    }
    if !disabled {
        b = b.hover_bg(if pressed { t_.subtle_pressed } else { t_.subtle_hover }).press_bg(t_.subtle_pressed);
    }
    if activity {
        b = b.child(indicator());
    }
    b.disabled(disabled)
}

fn convert_terminal_entry(entry: mewrk_pane_terminal::MenuEntry) -> MenuItem {
    let mut item = MenuItem::new(entry.id, entry.label.to_string());
    item.title = entry.title.map(|title| title.to_string());
    item.icon = entry.icon.map(Into::into);
    item.disabled = entry.disabled;
    item.label_is_path = entry.is_path;
    item.on_select = entry.on_select;
    item.children = entry.children.into_iter().map(convert_terminal_entry).collect();
    item
}

/// `PaneToolbar`: the topbar's pane controls: three toggles plus an overflow menu.
fn pane_toolbar(app: &mut App, layout: &SidePaneLayout, conversation_id: &str) -> E {
    let terminal_open = pane_is_open(layout, "terminal");
    let review_open = pane_is_open(layout, "review");
    let preview_session = open_preview_session(layout).map(str::to_owned);
    let preview_open = preview_session.is_some();

    // ---- terminal
    let owner = app.host_conversation_id(conversation_id);
    let terminal_activity = mewrk_pane_terminal::terminal_busy_in(app, &[owner.as_str()]);
    let terminal_label = if terminal_activity { t("终端（命令运行中）", "Terminal (command running)") } else { t("终端", "Terminal") };
    let menu = new_terminal_menu(app);
    let flyout = menu.workspaces.len() > 1;
    let mut sections: Vec<MenuSection> = new_terminal_sections(menu, &owner, Rc::new(|app: &mut App| open_pane(app, "terminal"))).into_iter().map(|section| MenuSection { id: section.id, label: section.label.map(|label| label.to_string()), action: None, items: section.items.into_iter().map(convert_terminal_entry).collect() }).collect();
    // A new shell, as the terminal pane's `+` offers it; showing or hiding the terminals already open is the row after them.
    sections.push(MenuSection::new("pane").item(MenuItem::new("toggle", if terminal_open { t("收起终端面板", "Hide the terminal pane") } else { t("显示终端面板", "Show the terminal pane") }).icon("PanelRight").on(|app: &mut App| toggle_pane(app, "terminal"))));
    let long_heading = sections.iter().any(|section| section.label.as_deref().is_some_and(|label| label.chars().count() > 56));
    let terminal = PopoverMenu::new("pane-toolbar-terminal", terminal_label, move |_| toggle_look("pane-toolbar-terminal-look", "SquareTerminal", terminal_label, terminal_open, false, terminal_activity)).menu_label(t("新建终端", "New terminal")).sections(move || sections).align_end().dense().submenu(if flyout { Submenu::Flyout } else { Submenu::Inline });
    // A heading naming a deep path wraps at the panel's 420, and Taffy takes a column clamped by `max-width` at its unwrapped
    // height, which would leave the last rows outside the panel and out of reach. A known width measures it properly.
    let terminal = if long_heading { terminal.width(420.0) } else { terminal }.build();

    // ---- review
    let pages = review_pages(app);
    let no_repository = pages.is_empty();
    let review_activity = pages.iter().any(|page| !page.snapshot.is_clean);
    let review_label = if review_activity { t("审阅（有未提交改动）", "Review (uncommitted changes)") } else { t("审阅", "Review") };
    // Without a repository among the conversation's workspaces there is nothing to review.
    let review_tip = if no_repository { t("这个任务的工作区都不是 Git 仓库", "None of this task's workspaces is a Git repository") } else { review_label };
    let mut review = toggle_look("pane-toolbar-review", "GitCompareArrows", review_tip, review_open, no_repository, review_activity).aria_label(review_label).aria_pressed(review_open).when(no_repository, |e| e.disabled_reason(review_tip));
    if !no_repository {
        review = review.on_click(move |app: &mut App| if review_open { close_pane(app, "review") } else { open_git_review(app, None) });
    }

    // ---- preview
    let automation = app.model_run_summaries().iter().any(|(id, summary)| id == conversation_id && summary.browser_automation_tool.is_some());
    let preview_label = if automation { t("预览（模型正在操作页面）", "Preview (the model is driving the page)") } else { t("预览", "Preview") };
    let workspaces = crate::effects::terminal_workspaces(app);
    // With one workspace the button is the pane's toggle. With more, a page is opened for a workspace: the menu lists them,
    // each opening that workspace's start page, and the row after them shows or hides the pane.
    let preview: E = if workspaces.len() > 1 {
        let disabled = crate::effects::lifecycle_operation_running(app);
        let items = preview_workspace_menu_items(app, disabled);
        let sections = vec![MenuSection::new("workspaces").label(t("在哪个工作区打开", "Open in which workspace")).items(items), MenuSection::new("pane").item(MenuItem::new("toggle", if preview_open { t("收起预览面板", "Hide the preview pane") } else { t("打开预览面板", "Open the preview pane") }).icon("PanelRight").on(toggle_preview))];
        PopoverMenu::new("pane-toolbar-preview", preview_label, move |_| toggle_look("pane-toolbar-preview-look", "Globe", preview_label, preview_open, false, automation)).menu_label(t("打开预览", "Open a preview")).sections(move || sections).align_end().dense().build()
    } else {
        toggle_look("pane-toolbar-preview", "Globe", preview_label, preview_open, false, automation).aria_pressed(preview_open).on_click(toggle_preview)
    };

    // ---- the overflow menu
    let shown_tab = shown_tasks_tab(layout);
    let views = vec![
        MenuItem::new("files", t("文件", "Files")).icon("Folder").checked(pane_is_open(layout, "files")).on(|app: &mut App| toggle_pane(app, "files")),
        // Two rows for the one pane: each is that tab on show.
        MenuItem::new("tasks", t("任务", "Tasks")).icon("ListChecks").checked(shown_tab == Some(TasksPaneTab::Tasks)).on(|app: &mut App| toggle_tasks_tab(app, TasksPaneTab::Tasks)),
        MenuItem::new("history", t("历史记录", "History")).icon("History").checked(shown_tab == Some(TasksPaneTab::History)).on(|app: &mut App| toggle_tasks_tab(app, TasksPaneTab::History)),
        MenuItem::new("settings", t("对话设置", "Conversation settings")).icon("SlidersHorizontal").checked(pane_is_open(layout, "settings")).on(|app: &mut App| toggle_pane(app, "settings")),
    ];
    let more_label = t("更多选项", "More options");
    let more = PopoverMenu::new("pane-toolbar-more", more_label, move |_| toggle_look("pane-toolbar-more-look", "EllipsisVertical", more_label, false, false, false)).menu_label(t("视图", "Views")).sections(move || vec![MenuSection::new("views").items(views)]).align_end().dense().build();

    row().shrink0().items_center().gap(2.0).child(terminal).child(review).child(preview).child(more)
}

/// `.topbar`: this is the window's top row, so its empty stretches move the window.
pub fn topbar(app: &mut App, sidebar_open: bool) -> E {
    let t_ = theme();
    let conversation = app.active_conversation_id();
    let title = app.active_conversation_view().map(|conversation| conversation.title);
    let leading: E = match (&conversation, &title) {
        (Some(id), Some(title)) => conversation_title(app, id, title),
        _ => row().min_w(0.0).items_baseline().gap(7.0).child(text("Mewrk").role(Role::Heading).aria_level(1).ty(fluent::TypeStyle { size: 15.0, line: 20.0, weight: fluent::Weight::Semibold, mono: false, italic: false, serif: false })).child(text(t("喵。开工。", "Mew. Work.")).ty(FINE).color(t_.text_tertiary).ellipsis()),
    };
    let mut actions: E = row().shrink0().items_center().gap(7.0).child(save_status_chip(app));
    if let (Some(id), Some(_)) = (&conversation, &title) {
        let layout = side_pane_layout_for(app.side_panes(), Some(id)).clone();
        actions = actions.child(pane_toolbar(app, &layout, id));
    }
    let left = if sidebar_open { 16.0 } else { shell_nav_inset() + shell_nav_closed_width() + 14.0 };
    drag_region(row().key("topbar").h(TOPBAR_HEIGHT).shrink0().pl(left).pr(12.0).items_center().justify_between().gap(12.0).child(row().flex1().min_w(0.0).items_center().gap(7.0).child(leading)).child(actions))
}
