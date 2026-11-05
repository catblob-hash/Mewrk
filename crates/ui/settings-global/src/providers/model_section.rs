//! `ModelSection.tsx`: the selected provider's models, by group. Every listed model is an enabled model, so rows expose
//! properties and removal only.

use std::collections::BTreeSet;

use mewrk_client::model::{ApiProvider, ModelCapability, ModelProfile};
use mewrk_client::prelude::*;

use super::{open_model_manager, open_new_model_editor, replace_provider, state, ModelEditor};
use crate::logic::model_caps::{has_model_catalog, model_display_name, model_group};
use crate::widgets::*;

#[derive(Default)]
pub struct SectionState {
    pub query: String,
    pub search_open: bool,
    pub collapsed: BTreeSet<String>,
}

/// `capabilityLabel`.
pub fn capability_label(capability: ModelCapability) -> &'static str {
    match capability {
        ModelCapability::ImageRecognition => t("视觉输入", "Vision"),
        ModelCapability::ToolAppend => t("中途追加工具", "Mid-conversation tools"),
        ModelCapability::SystemAppend => t("中途追加系统提示词", "Mid-conversation system prompts"),
        ModelCapability::AsyncTools => t("异步工具调用", "Asynchronous tool calls"),
        ModelCapability::NativeCompaction => t("原生压缩", "Native compaction"),
    }
}

pub fn capability_icon(capability: ModelCapability) -> &'static str {
    match capability {
        ModelCapability::ImageRecognition => "Eye",
        ModelCapability::ToolAppend => "Wrench",
        ModelCapability::SystemAppend => "MessageSquarePlus",
        ModelCapability::AsyncTools => "Hourglass",
        ModelCapability::NativeCompaction => "Shrink",
    }
}

/// `CapabilityIcons`: icon-only chips with a tooltip, which fit without displacing names.
pub fn capability_icons(model: &ModelProfile) -> Option<E> {
    let t_ = theme();
    (!model.capabilities.is_empty()).then(|| row().gap(3.0).items_center().shrink0().children(model.capabilities.iter().map(|c| row().center().size(18.0, 18.0).radius(CONTROL_RADIUS).bg(t_.subtle_hover).tooltip(capability_label(*c)).role(Role::Image).aria_label(capability_label(*c)).child(icon(capability_icon(*c), 11.0).color(t_.text_secondary)))))
}

/// Groups by label, models in their list order. Sorted as `localeCompare` reads names, so a lowercase vendor doesn't land after every capitalized one.
pub fn group_models<'a>(models: impl Iterator<Item = &'a ModelProfile>) -> Vec<(String, Vec<&'a ModelProfile>)> {
    let mut buckets: Vec<(String, Vec<&ModelProfile>)> = Vec::new();
    for model in models {
        let group = model_group(model);
        let label = if group.is_empty() { t("其他", "Other").to_owned() } else { group };
        match buckets.iter_mut().find(|(name, _)| *name == label) {
            Some((_, bucket)) => bucket.push(model),
            None => buckets.push((label, vec![model])),
        }
    }
    buckets.sort_by(|left, right| left.0.to_lowercase().cmp(&right.0.to_lowercase()).then_with(|| right.0.cmp(&left.0)));
    buckets
}

fn remove_model(app: &mut App, provider_id: &str, model_id: &str) {
    let id = model_id.to_owned();
    replace_provider(app, provider_id, move |p| p.models.retain(|m| m.id != id), None);
}

pub fn model_section(app: &mut App, provider: &ApiProvider) -> E {
    let t_ = theme();
    let s = state(app);
    let query = s.section.query.clone();
    let search_expanded = s.section.search_open || !query.is_empty();
    let busy = s.busy_action.is_some();
    let discovering = s.busy_action == Some("models");
    let collapsed = s.section.collapsed.clone();
    let has_models = !provider.models.is_empty();
    let needle = query.trim().to_lowercase();
    let matching = provider.models.iter().filter(|m| needle.is_empty() || m.id.to_lowercase().contains(&needle) || model_display_name(m).to_lowercase().contains(&needle));
    let groups = group_models(matching);
    // Force matching groups open while searching so results remain visible.
    let searching = !needle.is_empty();
    let all_expanded = collapsed.is_empty();
    let group_names: Vec<String> = groups.iter().map(|(name, _)| name.clone()).collect();

    let toggle_all = icon_button::<App>(if all_expanded { "ChevronsDownUp" } else { "ChevronsUpDown" }, if all_expanded { t("全部收起", "Collapse all") } else { t("全部展开", "Expand all") })
        .key("models-toggle-all")
        .size(24.0, 24.0)
        .disabled(!has_models)
        .on_click(move |app: &mut App| state(app).section.collapsed = if all_expanded { group_names.iter().cloned().collect() } else { BTreeSet::new() });

    let search_el = if search_expanded {
        let has_query = !query.is_empty();
        row()
            .min_h(30.0)
            .w(158.0)
            .shrink0()
            .items_center()
            .gap(4.0)
            .pl(8.0)
            .pr(3.0)
            .radius(CONTROL_RADIUS)
            .bg(t_.control_fill)
            .border(t_.control_stroke)
            .child(icon("Search", 13.0).color(t_.text_tertiary))
            .child(
                field::<App>("models-search", query.clone())
                    .bare()
                    .placeholder(t("搜索模型", "Search models"))
                    .on_change(|app, v| state(app).section.query = v)
                    .on_blur(|app, _| {
                        if state(app).section.query.is_empty() {
                            state(app).section.search_open = false;
                        }
                    })
                    .build()
                    .flex1()
                    .h(18.0)
                    .aria_label(t("搜索模型", "Search models"))
                    .on_key(|app: &mut App, k: &KeyPress| {
                        if k.key != Key::Escape {
                            return false;
                        }
                        let section = &mut state(app).section;
                        section.query.clear();
                        section.search_open = false;
                        true
                    }),
            )
            .when(has_query, |e| {
                e.child(icon_button::<App>("X", t("清空搜索", "Clear search")).key("models-search-clear").size(20.0, 20.0).on_click(|app: &mut App| {
                    let section = &mut state(app).section;
                    section.query.clear();
                    section.search_open = false;
                }))
            })
    } else {
        icon_button::<App>("Search", t("搜索模型", "Search models")).key("models-search-open").size(24.0, 24.0).on_click(|app: &mut App| {
            state(app).section.search_open = true;
            focus_request(("field", "models-search"));
        })
    };

    let provider_for_add = provider.clone();
    let mut actions = row().items_center().radius(CONTROL_RADIUS).border(t_.control_stroke).clip();
    // Bedrock and Vertex with a blank address have no catalog to read, so their IDs are added by hand.
    if has_model_catalog(provider) {
        let spin = if discovering { spinner(12.0) } else { icon("RefreshCw", 12.0).color(t_.text_secondary) };
        actions = actions.child(row().key("models-fetch").h(30.0).px(10.0).gap(6.0).items_center().hover_bg(t_.subtle_hover).border_right(t_.control_stroke).disabled(busy).on_click(open_model_manager).child(spin).child(text(t("拉取模型", "Fetch models")).ty(CAPTION).nowrap()));
    }
    actions = actions.child(row().key("models-add").center().size(30.0, 30.0).hover_bg(t_.subtle_hover).tooltip(t("手动添加模型", "Add a model manually")).on_click(move |app: &mut App| open_new_model_editor(app, &provider_for_add)).child(icon("Plus", 13.0).color(t_.text_secondary)));

    let head = row().wrap().items_center().justify_between().gap(10.0).child(row().min_w(0.0).items_center().gap(3.0).child(text(t("模型", "Models")).ty(CAPTION_STRONG).nowrap().mr(5.0)).child(toggle_all).child(search_el)).child(actions);

    let mut list = col().gap(7.0);
    for (group, models) in &groups {
        let open = searching || !collapsed.contains(group);
        let toggle_group = group.clone();
        let remove_ids: Vec<String> = models.iter().map(|m| m.id.clone()).collect();
        let remove_provider_id = provider.id.clone();
        let remove_group = icon_button::<App>("Minus", tf("移除 {group} 分组下的全部模型", "Remove every model in {group}", &[("group", group)]))
            .key(("model-group-remove", group.as_str()))
            .size(24.0, 24.0)
            .reveal_on_parent_hover()
            .on_click(move |app: &mut App| {
                for id in &remove_ids {
                    remove_model(app, &remove_provider_id, id);
                }
            });
        let head = row()
            .key(("model-group", group.as_str()))
            .min_h(30.0)
            .pl(9.0)
            .pr(6.0)
            .gap(6.0)
            .items_center()
            .hover_bg(t_.subtle_hover)
            .when(open, |e| e.border_bottom(t_.divider))
            .role(Role::Button)
            .aria_expanded(open)
            .on_click(move |app: &mut App| {
                let collapsed = &mut state(app).section.collapsed;
                if !collapsed.remove(&toggle_group) {
                    collapsed.insert(toggle_group.clone());
                }
            })
            .child(icon("ChevronRight", 13.0).color(t_.text_tertiary).rotate(if open { 90.0 } else { 0.0 }))
            .child(text(group.clone()).ty(CAPTION).ellipsis().flex1())
            .child(remove_group);
        let mut card = col().radius(OVERLAY_RADIUS).border(t_.card_stroke).clip().child(head);
        if open {
            for (i, model) in models.iter().enumerate() {
                let name = model_display_name(model).to_owned();
                let (edit_model, remove_id, remove_pid, edit_pid) = ((*model).clone(), model.id.clone(), provider.id.clone(), provider.id.clone());
                card = card.child(
                    row()
                        .key(("model-row", model.id.as_str()))
                        .min_h(34.0)
                        .py(3.0)
                        .pl(9.0)
                        .pr(6.0)
                        .gap(8.0)
                        .items_center()
                        .when(i > 0, |e| e.border_top(t_.divider))
                        .child(avatar(&name, true))
                        .child(text(name).ty(MONO_SMALL).ellipsis().flex1().full_text(model.id.clone()))
                        .child(capability_icons(model))
                        .child(icon_button::<App>("Settings2", tf("模型 {name} 的属性", "Properties for model {name}", &[("name", &model.id)])).key(("model-edit", model.id.as_str())).size(26.0, 26.0).on_click(move |app: &mut App| {
                            super::open_model_editor(app, ModelEditor { create: false, provider_id: edit_pid.clone(), original_id: Some(edit_model.id.clone()), draft: edit_model.clone(), append_touched: true });
                        }))
                        .child(icon_button::<App>("Minus", tf("移除模型 {name}", "Remove model {name}", &[("name", &model.id)])).key(("model-remove", model.id.as_str())).size(26.0, 26.0).on_click(move |app: &mut App| remove_model(app, &remove_pid, &remove_id))),
                );
            }
        }
        list = list.child(card);
    }
    let empty = |icon_name: &'static str, title: &str, body: &str| col().min_h(104.0).py(18.0).px(12.0).center().radius(8.0).border(t_.control_stroke).bg(t_.card_secondary).child(icon(icon_name, 20.0).color(t_.text_tertiary)).child(text(title.to_owned()).ty(CAPTION_STRONG).color(t_.text_secondary).mt(7.0).text_center()).child(text(body.to_owned()).ty(FINE).color(t_.text_tertiary).mt(3.0).text_center());
    if !has_models {
        list = list.child(empty("Bot", t("还没有模型", "No models yet"), t("拉取模型，或手动添加一个。", "Fetch models, or add one manually.")));
    } else if groups.is_empty() {
        list = list.child(empty("Search", t("没有匹配的模型", "No matching model"), t("换个关键词试试。", "Try another keyword.")));
    }
    col().gap(9.0).child(head).child(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, group: &str) -> ModelProfile {
        ModelProfile { id: id.into(), name: String::new(), group: group.into(), context_window: None, max_output_tokens: None, capabilities: Default::default(), reasoning_content: mewrk_client::model::ReasoningContent::Plaintext, prompt_cache: true, cache_ttl_minutes: None }
    }

    // `localeCompare`: a lowercase vendor sits where its letter does, not after every capitalized one.
    #[test]
    fn groups_sort_by_their_letters_and_models_keep_their_list_order() {
        let models = [model("z1", "Zeta"), model("a1", "alpha"), model("b1", "Beta"), model("a2", "alpha")];
        let groups = group_models(models.iter());
        let names: Vec<&str> = groups.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["alpha", "Beta", "Zeta"]);
        let alpha: Vec<&str> = groups[0].1.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(alpha, ["a1", "a2"]);
    }
}
