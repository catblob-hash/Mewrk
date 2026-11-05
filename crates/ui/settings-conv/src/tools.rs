//! `ToolSelectionGroups.tsx`: the tool picker, grouped by category, shared by a conversation, a preset and a role.

use std::collections::{HashMap, HashSet};

use mewrk_client::model::{ToolCategory, ToolDescriptor};
use mewrk_client::prelude::*;

use crate::lock::{lock_hint, LockTone};
use crate::logic::{is_host_derived_tool_name, is_preview_lifecycle_tool_name, tool_docs_href, with_preview_lifecycle_tools};
use crate::widgets::*;

/// Render order: screens and Android sit right after the preview group, whose `preview_start` connects the screens they act on.
const GROUPS: [ToolCategory; 8] = [ToolCategory::Filesystem, ToolCategory::Shell, ToolCategory::Web, ToolCategory::Screen, ToolCategory::Android, ToolCategory::Orchestration, ToolCategory::Memory, ToolCategory::Mcp];

fn group_icon(category: ToolCategory) -> &'static str {
    match category {
        ToolCategory::Filesystem => "FileCode2",
        ToolCategory::Shell => "Command",
        ToolCategory::Web => "Globe2",
        ToolCategory::Screen => "MonitorSmartphone",
        ToolCategory::Android => "Smartphone",
        ToolCategory::Orchestration => "BrainCircuit",
        ToolCategory::Memory => "Database",
        _ => "Plug",
    }
}

pub fn group_label(category: ToolCategory) -> &'static str {
    match category {
        ToolCategory::Filesystem => t("文件与搜索", "Files and search"),
        ToolCategory::Web => t("预览", "Preview"),
        ToolCategory::Screen => t("屏幕", "Screens"),
        ToolCategory::Android => t("Android", "Android"),
        ToolCategory::Memory => t("长期记忆", "Long-term memory"),
        ToolCategory::Mcp => "MCP",
        ToolCategory::Orchestration => t("代理编排", "Agent orchestration"),
        _ => "Shell",
    }
}

/// A stable name for a category, for keys.
pub fn category_key(category: ToolCategory) -> &'static str {
    match category {
        ToolCategory::Filesystem => "filesystem",
        ToolCategory::Shell => "shell",
        ToolCategory::Web => "web",
        ToolCategory::Screen => "screen",
        ToolCategory::Android => "android",
        ToolCategory::Orchestration => "orchestration",
        ToolCategory::Memory => "memory",
        ToolCategory::Mcp => "mcp",
        _ => "unknown",
    }
}

/// The rows the picker can draw, grouped. Memory, task-runtime and skill tools are host-derived and cannot be individually toggled, and the
/// preview lifecycle tools follow the other preview tools, so none has a row.
pub fn grouped_tools(tools: &[ToolDescriptor]) -> Vec<(ToolCategory, Vec<&ToolDescriptor>)> {
    let mut seen = HashSet::new();
    let pickable: Vec<&ToolDescriptor> = tools.iter().filter(|tool| seen.insert(tool.name.as_str())).filter(|tool| tool.category != ToolCategory::Memory && !is_host_derived_tool_name(&tool.name) && !is_preview_lifecycle_tool_name(&tool.name)).collect();
    GROUPS.iter().copied().filter(|c| *c != ToolCategory::Memory).map(|c| (c, pickable.iter().copied().filter(|tool| tool.category == c).collect::<Vec<_>>())).filter(|(_, g)| !g.is_empty()).collect()
}

pub struct ToolGroups {
    pub scope: String,
    pub tools: Rc<Vec<ToolDescriptor>>,
    pub enabled_tools: Vec<String>,
    pub on_change: Rc<dyn Fn(&mut App, Vec<String>)>,
    /// Collapse state is forgotten when this changes.
    pub expansion_key: String,
    /// How the conversation's lock draws a row. An orange row still moves; the caller warns first.
    pub tone_of: Option<Rc<dyn Fn(&str, bool) -> Option<LockTone>>>,
}

#[derive(Default)]
struct Collapse {
    key: String,
    closed: HashMap<&'static str, bool>,
}

pub fn tool_row_key(scope: &str, name: &str) -> u64 {
    hash_key(("tool-row", scope, name))
}

impl ToolGroups {
    pub fn view(self) -> E {
        let t_ = theme();
        let scope = self.scope.clone();
        let collapse = use_state(("tool-groups", scope.as_str()), Collapse::default);
        // Collapse state changes only by user action, so disabling the final tool does not hide the group needed to re-enable it.
        collapse.update(|c| {
            if c.key != self.expansion_key {
                c.key = self.expansion_key.clone();
                c.closed.clear();
            }
        });
        let available: Rc<HashSet<String>> = Rc::new(self.tools.iter().map(|t| t.name.clone()).collect());
        let enabled = Rc::new(self.enabled_tools.clone());
        let on_change = self.on_change.clone();
        // Every list the picker writes keeps the preview lifecycle tools in step with the rows it shows.
        let write: Rc<dyn Fn(&mut App, Vec<String>)> = Rc::new(move |app, next| on_change(app, with_preview_lifecycle_tools(&next, &available)));
        let mut out = col().min_w(0.0);
        for (index, (category, group)) in grouped_tools(&self.tools).into_iter().enumerate() {
            let label_text = group_label(category);
            let cat = category_key(category);
            let expanded = !collapse.with(|c| c.closed.get(cat).copied().unwrap_or(false));
            let enabled_count = group.iter().filter(|tool| enabled.contains(&tool.name)).count();
            let total = group.len();
            let names: Rc<Vec<String>> = Rc::new(group.iter().map(|tool| tool.name.clone()).collect());
            let toggle_group: Rc<dyn Fn(&mut App)> = {
                let collapse = collapse.clone();
                Rc::new(move |_| collapse.update(|c| {
                    let v = c.closed.entry(cat).or_insert(false);
                    *v = !*v;
                }))
            };
            say(label_text);
            say(&format!("{label_text} {enabled_count} / {total}"));
            let (plus_label, minus_label) = (tf("全选{label}", "Select all in {label}", &[("label", &label_text)]), tf("全不选{label}", "Clear all in {label}", &[("label", &label_text)]));
            let on_plus = {
                let (enabled, names, write, collapse) = (enabled.clone(), names.clone(), write.clone(), collapse.clone());
                move |app: &mut App| {
                    // Turning a group on is also a way of asking to see it: a collapsed group would otherwise report a new count with nothing to show for it.
                    collapse.update(|c| {
                        c.closed.insert(cat, false);
                    });
                    let mut next = (*enabled).clone();
                    for n in names.iter() {
                        if !next.contains(n) {
                            next.push(n.clone());
                        }
                    }
                    write(app, next);
                }
            };
            let on_minus = {
                let (enabled, names, write) = (enabled.clone(), names.clone(), write.clone());
                // Turning one off has no second job, so it leaves the disclosure alone.
                move |app: &mut App| write(app, enabled.iter().filter(|n| !names.contains(n)).cloned().collect())
            };
            let tg1 = toggle_group.clone();
            let tg2 = toggle_group;
            // The heading's own 4 px is the whole indent: its icon is the top of the column the tool rows' documentation links read down, and its chevron the top of the column their signs do.
            let heading = row()
                .items_center()
                .w_full()
                .min_h(35.0)
                .gap(7.0)
                .px(4.0)
                .child(
                    row()
                        .key(("tool-group", scope.as_str(), cat))
                        .items_center()
                        .flex1()
                        .min_w(0.0)
                        .gap(6.0)
                        .min_h(35.0)
                        .aria_label(label_text)
                        .aria_expanded(expanded)
                        .aria_controls(("tool-group-region", scope.as_str(), cat))
                        .on_click(move |app: &mut App| tg1(app))
                        .child(icon(group_icon(category), 14.0).color(t_.text_secondary))
                        .child(text(label_text).ty(CAPTION_STRONG).ellipsis().flex1().min_w(0.0))
                        .child(text(format!("{enabled_count} / {total}")).ty(FINE).color(t_.text_tertiary).nowrap()),
                )
                .child(row().items_center().gap(1.0).shrink0().child(icon_btn(("tool-group-plus", scope.as_str(), cat), "Plus", plus_label).size(18.0, 18.0).disabled(enabled_count == total).when(enabled_count == total, |e| e.opacity(0.4)).on_click(on_plus)).child(icon_btn(("tool-group-minus", scope.as_str(), cat), "Minus", minus_label).size(18.0, 18.0).disabled(enabled_count == 0).when(enabled_count == 0, |e| e.opacity(0.4)).on_click(on_minus)))
                // A second way to work the same disclosure, for the chevron itself, the part of a heading people aim at. The labelled one above is what a keyboard and a reader get.
                .child(row().key(("tool-group-chevron", scope.as_str(), cat)).center().size(14.0, 14.0).shrink0().aria_hidden(true).on_pointer(move |app: &mut App, e: PointerEvent| {
                    if e.phase == Phase::Up && e.rect.contains(e.x, e.y) {
                        tg2(app);
                    }
                }).child(icon("ChevronRight", 14.0).color(t_.text_tertiary).rotate(if expanded { 90.0 } else { 0.0 })));
            // Rules between groups only: the page's own edges already bound the list.
            let mut group_el = col().min_w(0.0).when(index > 0, |e| e.border_top(t_.divider)).child(heading);
            if expanded {
                let mut region = col().key(("tool-group-region", scope.as_str(), cat)).min_w(0.0).role(Role::Group);
                for tool in group {
                    let on = enabled.contains(&tool.name);
                    let tone = self.tone_of.as_ref().and_then(|f| f(&tool.name, on));
                    let (name, enabled, write) = (tool.name.clone(), enabled.clone(), write.clone());
                    region = region.child(tool_pick_row(&scope, tool, on, tone, move |app: &mut App| {
                        let next: Vec<String> = if on {
                            enabled.iter().filter(|n| **n != name).cloned().collect()
                        } else {
                            let mut next = (*enabled).clone();
                            if !next.contains(&name) {
                                next.push(name.clone());
                            }
                            next
                        };
                        write(app, next);
                    }));
                }
                group_el = group_el.child(region);
            }
            out = out.child(group_el);
        }
        out
    }
}

/// One pickable tool. The row is the control: it fills in when the tool is on, and the trailing sign says which way a click moves it. A row the
/// lock tones fills orange and still clicks, through the caller's warning. The way in to the tool's documentation lies inside the fill, in the slot
/// the group heading puts its icon, and keeps its own click.
fn tool_pick_row(scope: &str, tool: &ToolDescriptor, on: bool, tone: Option<LockTone>, toggle: impl Fn(&mut App) + 'static) -> E {
    let t_ = theme();
    let href = tool_docs_href(&tool.name);
    let doc_label = tf("{label}的说明文档", "Documentation for {label}", &[("label", &tool.label)]);
    let state_label = if on { tf("{label}已启用", "{label} enabled", &[("label", &tool.label)]) } else { tf("{label}已关闭", "{label} disabled", &[("label", &tool.label)]) };
    say(&state_label);
    say(&format!("docs:{href}"));
    let fill = cache_fill(tone).or(on.then_some(t_.success_bg));
    let mut label = row().items_center().gap(7.0).flex1().min_w(0.0).child(strong(tool.label.clone()).ty(CAPTION).ellipsis().flex1().min_w(0.0));
    if tool.dangerous {
        let mark = t("需审查", "Reviewed");
        say(mark);
        label = label.child(text(mark).ty(FINE).color(t_.caution).px(4.0).py(1.0).radius(4.0).nowrap());
    }
    label = label.child(icon(if on { "Minus" } else { "Plus" }, 14.0).color(t_.text_tertiary).shrink0());
    // The left padding and the gap are the heading's: a tool's name and its group's name read down one column.
    let mut body = row()
        .key(tool_row_key(scope, &tool.name))
        .items_center()
        .gap(6.0)
        .min_w(0.0)
        .min_h(26.0)
        .py(5.0)
        .px(4.0)
        .radius(7.0)
        .hover_bg(t_.subtle_hover)
        // A button whatever it holds beside its name, as a catalog row is.
        .role(Role::Button)
        .aria_pressed(on)
        .aria_label(state_label)
        .on_click(toggle)
        .child(row().key(("tool-docs", scope, tool.name.as_str())).center().size(14.0, 14.0).radius(4.0).shrink0().hover_bg(t_.subtle_hover).tooltip(doc_label).role(Role::Link).on_click(move |app: &mut App| open_url(app, href.clone())).child(icon("BookOpen", 14.0).color(t_.text_tertiary)))
        .child(label);
    if let Some(fill) = fill {
        body = body.bg(fill);
    }
    if let Some(tone) = tone {
        body = body.tip(lock_hint(tone));
    }
    body
}
