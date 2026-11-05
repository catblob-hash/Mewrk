//! `DependencySettings/index.tsx`: checks whether command line tools are on PATH.
//!
//! Nothing here installs anything: upstream detects only.

use std::collections::HashSet;

use mewrk_client::model::EnvironmentToolDefinition;
use mewrk_client::prelude::*;
use serde::Deserialize;

use crate::logic::store;
use crate::logic::workspace_env::{compact_url_label, is_tool_name};
use crate::widgets::*;

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentToolSnapshot {
    pub name: String,
    pub executable: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub repo_url: String,
    #[serde(default)]
    pub homepage: String,
    #[serde(default)]
    pub builtin: bool,
}

#[derive(Default)]
pub struct AddToolDraft {
    pub name: String,
    pub executable: String,
    pub version_args: String,
    pub name_touched: bool,
}

#[derive(Default)]
pub struct DependenciesState {
    pub generation: u64,
    pub snapshots: Option<Vec<EnvironmentToolSnapshot>>,
    pub probing: bool,
    pub probe_error: Option<String>,
    pub add: Option<AddToolDraft>,
    pub request: u64,
    pub signature: Option<String>,
}

fn page_state(app: &mut App) -> &mut DependenciesState {
    app.local::<DependenciesState>()
}

pub fn unmount(app: &mut App) {
    let s = page_state(app);
    let generation = s.generation + 1;
    *s = DependenciesState { generation, ..Default::default() };
}

pub fn probe(app: &mut App) {
    let s = page_state(app);
    s.request += 1;
    let (request, generation) = (s.request, s.generation);
    s.probing = true;
    s.probe_error = None;
    app.invoke::<Vec<EnvironmentToolSnapshot>>("environment_tool_snapshots", json!({}), move |app, result| {
        let s = page_state(app);
        if s.generation != generation || s.request != request {
            return;
        }
        s.probing = false;
        match result {
            Ok(next) => s.snapshots = Some(next),
            Err(_) => s.probe_error = Some(t("环境依赖检测失败。", "Dependency detection failed.").to_owned()),
        }
    });
}

/// Empty when the name can be added.
pub fn tool_name_error(name: &str, known: &HashSet<String>) -> &'static str {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        t("请输入名称。", "Enter a name.")
    } else if !is_tool_name(trimmed) {
        t("名称必须以字母开头，并且只能包含字母、数字、下划线或连字符。", "The name must start with a letter and contain only letters, numbers, underscores, or hyphens.")
    } else if known.contains(&trimmed.to_lowercase()) {
        t("已有同名工具。", "A tool with this name already exists.")
    } else {
        ""
    }
}

fn add_tool_dialog(app: &mut App, known: HashSet<String>) -> E {
    let draft = page_state(app).add.as_ref().expect("the caller checked");
    let error = tool_name_error(&draft.name, &known);
    let touched = draft.name_touched;
    let valid = error.is_empty() && !draft.executable.trim().is_empty();
    let (name, executable, version_args) = (draft.name.clone(), draft.executable.clone(), draft.version_args.clone());
    let edit = |f: fn(&mut AddToolDraft, String)| move |app: &mut App, v: String| {
        if let Some(d) = page_state(app).add.as_mut() {
            f(d, v);
        }
    };
    let name_field = field::<App>("tool-name", name)
        .on_change(edit(|d, v| {
            if !v.is_empty() {
                d.name_touched = true;
            }
            d.name = v;
        }))
        .on_blur(|app, _| {
            if let Some(d) = page_state(app).add.as_mut() {
                d.name_touched = true;
            }
        })
        .on_submit(|app, _| submit_tool(app))
        .build()
        .aria_label(t("名称", "Name"))
        .aria_invalid(touched && !error.is_empty())
        .aria_describedby("tool-name-error");
    let body = col()
        .child(field_row(t("名称", "Name"), (touched && !error.is_empty()).then(|| error_line(error).key("tool-name-error")), name_field))
        // One form: Enter in any of its fields submits it.
        .child(field_row(t("可执行文件名", "Executable name"), None, field::<App>("tool-executable", executable).on_change(edit(|d, v| d.executable = v)).on_submit(|app, _| submit_tool(app)).build().aria_label(t("可执行文件名", "Executable name"))))
        .child(field_row(t("版本参数", "Version arguments"), Some(help(t("用空格分隔多个参数，留空即 --version。", "Separate with spaces; blank uses --version."))), field::<App>("tool-version-args", version_args).placeholder("--version").on_change(edit(|d, v| d.version_args = v)).on_submit(|app, _| submit_tool(app)).build().aria_label(t("版本参数", "Version arguments"))));
    let footer = row().gap(8.0).child(button::<App>(t("取消", "Cancel")).key("tool-cancel").on_click(|app: &mut App| page_state(app).add = None)).child(accent_button::<App>(t("添加工具", "Add tool")).key("tool-submit").disabled(!valid).on_click(|app: &mut App| submit_tool(app)));
    modal("add-tool", t("添加工具", "Add tool"), Some(help(t("添加要在 PATH 上检测的工具。", "Add a tool to detect on PATH."))), 480.0, body, Some(footer), |app| page_state(app).add = None)
}

fn known_names(app: &mut App) -> HashSet<String> {
    let mut names: HashSet<String> = app.doc.global_settings.environment_tools.iter().map(|t| t.name.trim().to_lowercase()).collect();
    for snapshot in page_state(app).snapshots.iter().flatten() {
        names.insert(snapshot.name.trim().to_lowercase());
    }
    names
}

pub fn submit_tool(app: &mut App) {
    let known = known_names(app);
    let Some(draft) = page_state(app).add.as_mut() else { return };
    draft.name_touched = true;
    if !tool_name_error(&draft.name, &known).is_empty() || draft.executable.trim().is_empty() {
        return;
    }
    let tool = EnvironmentToolDefinition { name: draft.name.trim().to_owned(), executable: draft.executable.trim().to_owned(), version_args: draft.version_args.split_whitespace().map(str::to_owned).collect() };
    page_state(app).add = None;
    store::change(app, move |doc| doc.global_settings.environment_tools.push(tool));
    // The catalog is what the host probes, so what was just typed has to reach it first.
    store::flush(app, |app, _| probe(app));
}

fn remove_tool(app: &mut App, name: &str) {
    let target = name.trim().to_lowercase();
    store::change(app, move |doc| doc.global_settings.environment_tools.retain(|t| t.name.trim().to_lowercase() != target));
    store::flush(app, |app, _| probe(app));
}

fn tool_card(snapshot: &EnvironmentToolSnapshot) -> E {
    let t_ = theme();
    let installed = !snapshot.path.is_empty();
    let name = snapshot.name.clone();
    let mut copy = col().flex1().min_w(0.0).gap(4.0).child(row().gap(4.0).items_baseline().wrap().child(text(snapshot.name.clone()).ty(BODY_SMALL_STRONG)).child(text(tf("（{executable}）", "({executable})", &[("executable", &snapshot.executable)])).ty(FINE).color(t_.text_tertiary)));
    if installed {
        let badge = |s: String, tip: Option<String>, color: Color| row().h(18.0).px(6.0).gap(3.0).items_center().radius(3.0).bg(t_.subtle_pressed).when(tip.is_some(), |e| e.tooltip(tip.clone().unwrap_or_default())).child(text(s).ty(FINE).color(color).nowrap());
        copy = copy.child(
            row()
                .gap(4.0)
                .wrap()
                .child((!snapshot.version.is_empty()).then(|| badge(tf("v{version}", "v{version}", &[("version", &snapshot.version)]), None, t_.text_secondary)))
                .child(badge(t("系统", "System").to_owned(), Some(snapshot.path.clone()), t_.text_secondary))
                .child((!snapshot.error.is_empty()).then(|| row().h(18.0).px(6.0).gap(3.0).items_center().radius(3.0).bg(t_.critical_bg).tooltip(snapshot.error.clone()).child(icon("AlertTriangle", 10.0).color(t_.critical)).child(text(t("版本检测失败", "Version check failed")).ty(FINE).color(t_.critical).nowrap()))),
        );
    }
    copy = copy.child((installed && !snapshot.error.is_empty()).then(|| text(snapshot.error.clone()).ty(FINE).color(t_.critical)));
    copy = copy.child(text(if snapshot.description.is_empty() { t("没有描述。", "No description.").to_owned() } else { snapshot.description.clone() }).ty(CAPTION).color(t_.text_secondary));
    let delete = (!snapshot.builtin).then(|| icon_button::<App>("Trash2", tf("删除 {name}", "Delete {name}", &[("name", &snapshot.name)])).key(("tool-delete", snapshot.name.as_str())).on_click(move |app: &mut App| remove_tool(app, &name)));
    let mut links = row().gap(7.0).wrap().flex1().min_w(0.0);
    if !snapshot.repo_url.is_empty() {
        links = links.child(link(("tool-repo", snapshot.name.as_str()), compact_url_label(&snapshot.repo_url), snapshot.repo_url.clone()).max_w(130.0).tooltip(tf("打开 {name} 的代码仓库", "Open the {name} repository", &[("name", &snapshot.name)])));
    }
    if !snapshot.homepage.is_empty() {
        links = links.child(link(("tool-home", snapshot.name.as_str()), compact_url_label(&snapshot.homepage), snapshot.homepage.clone()).max_w(130.0).tooltip(tf("打开 {name} 的主页", "Open the {name} homepage", &[("name", &snapshot.name)])));
    }
    let executable = snapshot.executable.clone();
    let reveal = installed.then(|| {
        row()
            .key(("tool-reveal", snapshot.name.as_str()))
            .gap(4.0)
            .items_center()
            .shrink0()
            .on_click(move |app: &mut App| {
                app.invoke::<Value>("reveal_environment_tool", json!({ "executable": executable }), |_, result| {
                    if let Err(e) = result {
                        eprintln!("Failed to reveal environment tool: {e}");
                    }
                });
            })
            .child(icon("FolderOpen", 12.0).color(t_.text_secondary))
            .child(text(t("打开所在目录", "Open containing folder")).ty(FINE).color(t_.text_secondary).nowrap())
    });
    // Runs to the card's edges along its foot.
    let missing = (!installed).then(|| row().min_h(43.0).mt(2.0).mx(-14.0).mb(-14.0).py(7.0).px(14.0).gap(8.0).items_center().border_top(t_.card_stroke).bg(t_.subtle_hover).radii([0.0, 0.0, OVERLAY_RADIUS + 2.0, OVERLAY_RADIUS + 2.0]).child(text(t("未检测到", "Not detected")).ty(FINE_STRONG).color(t_.text_secondary).nowrap().shrink0()).child(help(t("请自行安装，然后重启应用再检测。", "Install it yourself, then restart the app and detect again.")).flex1().min_w(0.0)));
    col()
        .key(("tool-card", snapshot.name.as_str()))
        .min_w(0.0)
        .min_h(178.0)
        .p(14.0)
        .gap(8.0)
        .radius(OVERLAY_RADIUS + 2.0)
        .border(if installed { t_.success.with_alpha(0.3) } else { t_.card_stroke })
        .bg(t_.card)
        .child(row().gap(11.0).items_start().flex1().min_h(82.0).child(row().center().size(40.0, 40.0).shrink0().radius(OVERLAY_RADIUS).bg(if installed { t_.success_bg } else { t_.subtle_pressed }).child(icon("Package", 20.0).color(if installed { t_.success } else { t_.text_tertiary }))).child(copy).child(delete))
        .child(row().min_h(28.0).gap(8.0).items_center().child(links).child(reveal))
        .child(missing)
}

fn skeleton_card() -> E {
    let t_ = theme();
    let line = |w: f32| div().h(10.0).w_pct(w).radius(3.0).bg(t_.subtle_pressed);
    row().min_h(178.0).p(14.0).gap(11.0).radius(OVERLAY_RADIUS + 2.0).border(t_.divider).child(div().size(40.0, 40.0).shrink0().radius(OVERLAY_RADIUS).bg(t_.subtle_pressed)).child(col().flex1().gap(8.0).child(line(50.0)).child(line(100.0)).child(line(70.0)))
}

/// Two per row: `grid-template-columns: repeat(2, minmax(0, 1fr))`, or one at `max-width: 720px`.
fn two_columns(cards: Vec<E>) -> E {
    if settings_strip() {
        return col().gap(10.0).mt(8.0).children(cards);
    }
    let mut grid = col().gap(10.0).mt(8.0);
    let mut iter = cards.into_iter();
    while let Some(first) = iter.next() {
        let second = iter.next();
        grid = grid.child(row().gap(10.0).items_stretch().child(col().flex1().min_w(0.0).child(first.flex1())).child(col().flex1().min_w(0.0).child(second.map(E::flex1))));
    }
    grid
}

/// The page.
pub fn dependency_settings(app: &mut App) -> E {
    let t_ = theme();
    // Probes on mount and when the catalog changes, keyed on what it holds rather than on identity.
    let signature = serde_json::to_string(&app.doc.global_settings.environment_tools.iter().map(|t| (&t.name, &t.executable, &t.version_args)).collect::<Vec<_>>()).unwrap_or_default();
    if page_state(app).signature.as_deref() != Some(signature.as_str()) {
        page_state(app).signature = Some(signature);
        probe(app);
    }
    let total = app.doc.global_settings.environment_tools.len();
    let s = page_state(app);
    let total = s.snapshots.as_ref().map_or(total, Vec::len);
    let (probing, probe_error) = (s.probing, s.probe_error.clone());
    let snapshots = s.snapshots.clone();
    let adding = s.add.is_some();
    let title = row().gap(6.0).items_baseline().child(page_title(t("环境依赖", "Dependencies"))).child(text(total.to_string()).ty(CAPTION).color(t_.text_tertiary).nowrap());
    let refresh_glyph = if probing { spinner(15.0) } else { icon("RefreshCw", 15.0).color(t_.text_secondary) };
    let actions = row()
        .gap(6.0)
        .items_center()
        .shrink0()
        .child(row().key("tool-refresh").center().size(28.0, 28.0).radius(CONTROL_RADIUS).hover_bg(t_.subtle_hover).tooltip(t("刷新", "Refresh")).disabled(probing).aria_busy(probing).on_click(probe).child(refresh_glyph))
        .child(with_icon(icon("Plus", 14.0).color(t_.text_on_accent), t("添加工具", "Add tool"), true).key("tool-add").on_click(|app: &mut App| {
            page_state(app).add = Some(AddToolDraft::default());
            focus_request(("field", "tool-name"));
        }));
    let error = probe_error.map(|e| row().role(Role::Alert).min_h(44.0).mt(8.0).mb(2.0).py(6.0).pl(11.0).pr(7.0).gap(8.0).items_center().radius(8.0).bg(t_.critical_bg).border(t_.critical).child(icon("AlertTriangle", 15.0).color(t_.critical)).child(text(e).ty(CAPTION).color(t_.critical).flex1().min_w(0.0)).child(small_button(t("重试", "Retry")).key("tool-retry").on_click(probe)));
    let grid = if snapshots.is_none() && probing { two_columns((0..4).map(|_| skeleton_card()).collect()) } else { two_columns(snapshots.iter().flatten().map(tool_card).collect()) };
    let known = known_names(app);
    page()
        .child(page_heading(Some(title), help_with_docs(t("检测这些命令行工具是否在 PATH 上。", "Checks whether these tools are on PATH."), "settings", "dependencies"), Some(actions)))
        .child(error)
        .child(grid)
        .child(adding.then(|| add_tool_dialog(app, known)))
}

#[cfg(test)]
mod tests {
    use super::*;

    // DependencySettings.test.tsx
    #[test]
    fn names_are_checked_as_identifiers_and_case_insensitively_against_what_exists() {
        let _language = crate::language_for_test(false);
        let known: HashSet<String> = ["node".to_owned()].into_iter().collect();
        assert_eq!(tool_name_error("NODE", &known), "已有同名工具。");
        assert_eq!(tool_name_error("bad name", &known), "名称必须以字母开头，并且只能包含字母、数字、下划线或连字符。");
        assert_eq!(tool_name_error(" ", &known), "请输入名称。");
        assert_eq!(tool_name_error("custom-cli", &known), "");
    }
}
