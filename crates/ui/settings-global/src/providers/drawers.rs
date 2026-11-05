//! `AddProviderDialog.tsx`, `ProviderOptionsDrawer.tsx`, `ModelManagerDrawer.tsx` and `ModelProfileDrawer.tsx`.

use std::collections::{BTreeSet, HashSet};

use mewrk_client::model::{ApiProvider, ModelCapability, ModelProfile, ProviderFamily, ReasoningContent};
use mewrk_client::prelude::*;
use mewrk_client::tool_lock::DEFAULT_CACHE_TTL_MINUTES;

use super::model_section::{capability_icon, capability_icons, capability_label, group_models};
use super::{add_custom_provider, discover_models, install_models, replace_provider, state, uninstall_models};
use crate::logic::codex_provider::{is_builtin_provider, is_codex_provider, CODEX_DEFAULT_BASE_URL};
use crate::logic::endpoint_meta::{builtin_family_option, default_base_url, family_setting_meta, API_FORMAT_OPTIONS};
use crate::logic::model_caps::*;
use crate::widgets::*;

/// What drawer and dialog is up over the page.
pub fn overlays(app: &mut App, selected: Option<&ApiProvider>) -> Vec<E> {
    let mut out = Vec::new();
    if state(app).adding_provider {
        out.push(add_provider_dialog(app));
    }
    if let Some(provider) = selected {
        if state(app).options_open {
            out.push(provider_options_drawer(provider));
        }
        if state(app).model_manager_open {
            out.push(model_manager_drawer(app, provider));
        }
    }
    if let Some(editor) = state(app).model_editor.clone()
        && let Some(provider) = app.doc.assets.api_providers.iter().find(|p| p.id == editor.provider_id).cloned() {
            out.push(model_profile_drawer(&provider, &editor));
        }
    out
}

fn family_index(family: ProviderFamily) -> usize {
    API_FORMAT_OPTIONS.iter().position(|o| o.value == family).unwrap_or(0)
}

fn add_provider_dialog(app: &mut App) -> E {
    let s = state(app);
    let name = s.add_name.clone();
    // OpenAI Chat Completions is the default for relays and most upstream providers.
    let family = s.add_family.unwrap_or(ProviderFamily::OpenaiChat);
    let trimmed = name.trim().to_owned();
    let can_add = !trimmed.is_empty();
    let submit = move |app: &mut App, value: &str| {
        let value = value.trim();
        if !value.is_empty() {
            let family = state(app).add_family.unwrap_or(ProviderFamily::OpenaiChat);
            add_custom_provider(app, value, family);
        }
    };
    let close = |app: &mut App| {
        let s = state(app);
        s.adding_provider = false;
        s.add_name.clear();
        s.add_family = None;
    };
    let name_field = field::<App>("add-provider-name", name)
        .placeholder(t("例如 我的中转站", "Example: My relay"))
        .max_chars(32)
        .on_change(|app, v| state(app).add_name = v)
        .on_submit(move |app, v| submit(app, &v))
        .build()
        .aria_label(t("提供商名称", "Provider name"));
    let options: Vec<Arc<str>> = API_FORMAT_OPTIONS.iter().map(|o| Arc::from(o.label)).collect();
    let protocol = select::<App>("add-provider-family", options, family_index(family), |app, i| state(app).add_family = Some(API_FORMAT_OPTIONS[i].value)).aria_label(t("对话协议", "Chat protocol"));
    let body = col()
        .child(field_row(t("提供商名称", "Provider name"), None, name_field))
        .child(field_row(t("对话协议", "Chat protocol"), Some(help_with_docs(t("决定对话请求走哪个端点，建好后仍可改。", "Picks the chat endpoint; you can change it later."), "providers", "protocols")), protocol));
    let footer = row()
        .gap(8.0)
        .child(subtle_button::<App>(t("取消", "Cancel")).key("add-provider-cancel").on_click(close))
        .child(accent_button::<App>(t("添加", "Add")).key("add-provider-submit").disabled(!can_add).on_click(move |app: &mut App| submit(app, &trimmed)));
    modal("add-provider", t("添加提供商", "Add provider"), Some(help(t("填名称、选协议，地址与 Key 稍后填。", "Name it and pick a protocol; address and key come next."))), 560.0, body, Some(footer), close)
}

fn provider_options_drawer(provider: &ApiProvider) -> E {
    let id = provider.id.clone();
    let family = provider.family;
    let builtin = is_builtin_provider(family);
    let settings = known_family_settings(family);
    let required = required_family_settings(family);
    let name_id = id.clone();
    let name_field = input(("provider-name", id.as_str()), provider.name.clone(), "", move |app, name| replace_provider(app, &name_id, |p| p.name = name, None)).aria_label(t("提供商名称", "Provider name"));
    let format_id = id.clone();
    let base_url = provider.base_url.clone();
    let format: E = if let Some(fixed) = builtin_family_option(family) {
        select::<App>(("provider-format", id.as_str()), vec![Arc::from(fixed.label)], 0, |_, _| {}).disabled(true)
    } else {
        let options: Vec<Arc<str>> = API_FORMAT_OPTIONS.iter().map(|o| Arc::from(o.label)).collect();
        select::<App>(("provider-format", id.as_str()), options, family_index(family), move |app, i| {
            let next = API_FORMAT_OPTIONS[i].value;
            // Preserve an entered URL when changing protocol; replace only an empty URL or the old default.
            let url = if base_url.trim().is_empty() || base_url == default_base_url(family) { default_base_url(next).to_owned() } else { base_url.clone() };
            // Remove identity fields irrelevant to the new family so they cannot be sent with its requests.
            let keep: Vec<_> = known_family_settings(next).to_vec();
            replace_provider(
                app,
                &format_id,
                move |p| {
                    p.family = next;
                    p.base_url = url;
                    p.family_settings.retain(|k, _| keep.contains(k));
                },
                None,
            );
        })
    }
    .aria_label(t("API 格式", "API format"));
    let mut body = col()
        .child(field_row(t("提供商名称", "Provider name"), None, name_field))
        .child(field_row(t("API 格式", "API format"), Some(help_with_docs(t("决定对话请求走哪个端点。", "Picks the endpoint chat requests use."), "providers", "protocols")), format));
    if !settings.is_empty() {
        let mut section = col().mt(4.0).child(col().gap(2.0).mb(9.0).child(text(t("身份字段", "Identity fields")).ty(CAPTION_STRONG)).child(help(t("地址由这些字段推导，缺项会拒绝请求。", "The address comes from these; a missing one blocks requests."))));
        for &setting in settings {
            let meta = family_setting_meta(setting);
            let label = if required.contains(&setting) { format!("{} *", meta.label) } else { meta.label.to_owned() };
            let value = provider.family_settings.get(&setting).cloned().unwrap_or_default();
            let sid = id.clone();
            let input_el = code_input(("provider-setting", id.as_str(), meta.label), value, meta.placeholder, move |app, value| {
                replace_provider(
                    app,
                    &sid,
                    |p| {
                        if value.trim().is_empty() {
                            p.family_settings.remove(&setting);
                        } else {
                            p.family_settings.insert(setting, value);
                        }
                    },
                    None,
                );
            })
            .aria_label(meta.label);
            section = section.child(field_row(label, Some(help(meta.hint)), input_el));
        }
        body = body.child(section);
    }
    // Codex's chat address belongs to the ChatGPT backend; this field exists only to point it at a local test stub.
    if is_codex_provider(family) {
        let cid = id.clone();
        let input_el = code_input(("provider-stub", id.as_str()), provider.base_url.clone(), CODEX_DEFAULT_BASE_URL, move |app, url| replace_provider(app, &cid, |p| p.base_url = url, None)).aria_label(t("本机测试桩地址", "Local test stub address"));
        body = body.child(col().mt(4.0).child(field_row(t("本机测试桩地址", "Local test stub address"), Some(help(t("留空使用 ChatGPT 后端，仅限本机地址。", "Blank uses the ChatGPT backend; local addresses only."))), input_el)));
    }
    drawer("provider-options", t("提供商设置", "Provider settings"), Some(if builtin { t("内置提供商", "Built-in provider").to_owned() } else { t("自定义提供商", "Custom provider").to_owned() }), false, body, None, |app| state(app).options_open = false)
}

#[derive(Default)]
pub struct ManagerState {
    pub query: String,
    pub collapsed: BTreeSet<String>,
    /// Row order is whatever this drawer saw first and is never re-derived, so clicking a row only recolours it.
    pub seen: Vec<ModelProfile>,
}

/// `catalog`: the installed records take precedence, to preserve curation, and existing rows keep their place.
pub fn merge_seen(seen: &mut Vec<ModelProfile>, discovered: &[ModelProfile], installed: &[ModelProfile]) {
    for model in discovered {
        if !seen.iter().any(|m| m.id == model.id) {
            seen.push(model.clone());
        }
    }
    for model in installed {
        match seen.iter_mut().find(|m| m.id == model.id) {
            Some(slot) => *slot = model.clone(),
            None => seen.push(model.clone()),
        }
    }
}

fn model_manager_drawer(app: &mut App, provider: &ApiProvider) -> E {
    let t_ = theme();
    let s = state(app);
    let discovering = s.busy_action == Some("models");
    let discovery_error = s.discovery_errors.get(&provider.id).cloned();
    let discovered = s.discovered.get(&provider.id).cloned().unwrap_or_default();
    merge_seen(&mut s.manager.seen, &discovered, &provider.models);
    let query = s.manager.query.clone();
    let collapsed = s.manager.collapsed.clone();
    let catalog = s.manager.seen.clone();
    let installed_ids: HashSet<&str> = provider.models.iter().map(|m| m.id.as_str()).collect();
    let needle = query.trim().to_lowercase();
    let visible: Vec<&ModelProfile> = catalog.iter().filter(|m| needle.is_empty() || m.id.to_lowercase().contains(&needle) || model_display_name(m).to_lowercase().contains(&needle)).collect();
    let groups = group_models(visible.iter().copied());
    let all_visible_installed = !visible.is_empty() && visible.iter().all(|m| installed_ids.contains(m.id.as_str()));
    let visible_owned: Vec<ModelProfile> = visible.iter().map(|m| (*m).clone()).collect();

    let toolbar = row()
        .gap(6.0)
        .items_center()
        .mb(9.0)
        .child(
            row()
                .min_h(30.0)
                .flex1()
                .min_w(0.0)
                .items_center()
                .gap(4.0)
                .pl(8.0)
                .pr(3.0)
                .radius(CONTROL_RADIUS)
                .bg(t_.control_fill)
                .border(t_.control_stroke)
                .child(icon("Search", 13.0).color(t_.text_tertiary))
                .child(
                    field::<App>("manager-search", query.clone())
                        .bare()
                        .placeholder(t("按模型 ID 或名称搜索", "Search by model ID or name"))
                        .on_change(|app, v| state(app).manager.query = v)
                        .build()
                        .flex1()
                        .h(18.0)
                        .aria_label(t("搜索模型", "Search models"))
                        .on_key(|app: &mut App, k: &KeyPress| {
                            if k.key == Key::Escape && !state(app).manager.query.is_empty() {
                                state(app).manager.query.clear();
                                return true;
                            }
                            false
                        }),
                ),
        )
        .child(small_button(if all_visible_installed { t("移除全部结果", "Remove all results") } else { t("添加全部结果", "Add all results") }).key("manager-all").disabled(visible_owned.is_empty()).on_click(move |app: &mut App| {
            if all_visible_installed {
                uninstall_models(app, visible_owned.iter().map(|m| m.id.clone()).collect());
            } else {
                install_models(app, visible_owned.clone());
            }
        }));

    let mut list = col();
    if let Some(error) = discovery_error.as_ref().filter(|_| !discovering) {
        list = list.child(row().key("manager-error").role(Role::Alert).mb(9.0).gap(7.0).py(8.0).px(10.0).items_start().radius(7.0).bg(t_.critical_bg).border(t_.critical.with_alpha(0.3)).child(icon("AlertTriangle", 14.0).color(t_.critical)).child(col().flex1().min_w(0.0).gap(2.0).child(text(t("拉取模型列表失败", "Fetching the model list failed")).ty(CAPTION_STRONG).color(t_.critical)).child(text(error.clone()).ty(FINE).color(t_.text_secondary).selectable())));
    }
    for (group, models) in &groups {
        let is_collapsed = collapsed.contains(group);
        let group_installed = models.iter().all(|m| installed_ids.contains(m.id.as_str()));
        let group_key = group.clone();
        let group_models_owned: Vec<ModelProfile> = models.iter().map(|m| (*m).clone()).collect();
        let head = row()
            .h(28.0)
            .items_center()
            .gap(6.0)
            .child(row().key(("manager-group", group.as_str())).flex1().min_w(0.0).h_full().items_center().gap(4.0).aria_expanded(!is_collapsed).on_click(move |app: &mut App| {
                let collapsed = &mut state(app).manager.collapsed;
                if !collapsed.remove(&group_key) {
                    collapsed.insert(group_key.clone());
                }
            }).child(icon(if is_collapsed { "ChevronRight" } else { "ChevronDown" }, 13.0).color(t_.text_tertiary)).child(text(group.clone()).ty(CAPTION_STRONG).ellipsis()).child(row().px(5.0).radius(8.0).bg(t_.subtle_hover).shrink0().child(text(models.len().to_string()).ty(FINE).color(t_.text_secondary).nowrap())))
            .child(small_subtle_button(if group_installed { t("整组移除", "Remove group") } else { t("整组添加", "Add group") }).key(("manager-group-action", group.as_str())).on_click(move |app: &mut App| {
                if group_installed {
                    uninstall_models(app, group_models_owned.iter().map(|m| m.id.clone()).collect());
                } else {
                    install_models(app, group_models_owned.clone());
                }
            }));
        let mut section = col().mb(8.0).child(head);
        if !is_collapsed {
            for model in models {
                let installed = installed_ids.contains(model.id.as_str());
                let display = model_display_name(model).to_owned();
                let owned = (*model).clone();
                let tip: Arc<str> = Arc::from(if installed { tf("从提供商移除 {id}", "Remove {id} from the provider", &[("id", &model.id)]) } else { tf("添加到提供商 {id}", "Add {id} to the provider", &[("id", &model.id)]) });
                section = section.child(
                    row()
                        .key(("manager-row", model.id.as_str()))
                        .min_h(34.0)
                        .py(4.0)
                        .pl(6.0)
                        .pr(8.0)
                        .gap(7.0)
                        .items_center()
                        .radius(7.0)
                        .tooltip(tip.clone())
                        .aria_label(tip)
                        .aria_pressed(installed)
                        .when(installed, |e| e.bg(t_.success_bg).border(t_.success.with_alpha(0.34)))
                        .hover_bg(t_.subtle_hover)
                        .on_click(move |app: &mut App| {
                            if installed {
                                uninstall_models(app, [owned.id.clone()].into_iter().collect());
                            } else {
                                install_models(app, vec![owned.clone()]);
                            }
                        })
                        .child(avatar(&display, true))
                        .child(col().flex1().min_w(0.0).child(text(model.id.clone()).ty(MONO_SMALL).ellipsis()).child((display != model.id).then(|| text(display.clone()).ty(FINE).color(t_.text_tertiary).ellipsis())))
                        .child(capability_icons(model)),
                );
            }
        }
        list = list.child(section);
    }
    if groups.is_empty() {
        let has_query = !query.trim().is_empty();
        let (title, body) = if discovering {
            (t("正在拉取模型列表", "Fetching the model list"), t("宿主正在向该提供商请求 GET /models。", "The host is calling GET /models on this provider."))
        } else if discovery_error.is_some() {
            (t("没有拿到模型目录", "No catalog was returned"), t("上面写着这次请求失败的原因；也可以直接手动添加模型。", "The reason for the failed request is above; you can also add models manually."))
        } else if has_query {
            (t("没有匹配的模型", "No matching model"), t("换个关键词，或者重新拉取一次。", "Try another keyword, or fetch again."))
        } else {
            (t("这家上游还没有目录", "No catalog for this upstream yet"), t("重新拉取一次，或者直接手动添加模型。", "Fetch again, or add models manually."))
        };
        let glyph = if discovering { spinner(18.0) } else { icon("Search", 18.0).color(t_.text_tertiary) };
        list = list.child(col().key("manager-empty").py(24.0).items_center().gap(4.0).child(glyph).child(text(title).ty(CAPTION_STRONG).color(t_.text_secondary).text_center()).child(text(body).ty(FINE).color(t_.text_tertiary).text_center()));
    }
    let glyph = if discovering { spinner(13.0) } else { icon("RefreshCw", 13.0).color(t_.text_secondary) };
    let footer = with_icon(glyph, t("重新拉取", "Fetch again"), false).key("manager-refetch").disabled(discovering || !has_model_catalog(provider)).on_click(discover_models);
    let subtitle = tf("{installed} 个已装 · 目录共 {total} 个 · 点一行添加或移除", "{installed} installed · {total} in the catalog · click a row to add or remove", &[("installed", &provider.models.len()), ("total", &catalog.len())]);
    // The drawer's query, disclosure and row order are its own: closing it lets them go, as unmounting does.
    drawer("model-manager", t("发现模型", "Discover models"), Some(subtitle), true, col().child(toolbar).child(list), Some(footer), |app| {
        let s = state(app);
        s.model_manager_open = false;
        s.manager = Default::default();
    })
}

/// Blank or a duplicate of another model in this provider.
pub fn model_id_error(provider: &ApiProvider, draft_id: &str, original_id: Option<&str>) -> Option<&'static str> {
    let normalized = draft_id.trim();
    if normalized.is_empty() {
        return Some(t("模型 ID 不能为空", "Model ID is required"));
    }
    if provider.models.iter().any(|m| m.id.trim() == normalized && Some(m.id.as_str()) != original_id) {
        return Some(t("模型 ID 不能与同一提供商中的其他模型重复", "Model ID must be unique within this provider"));
    }
    None
}

/// `optionalPositiveInteger`.
pub fn optional_positive_integer(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parsed: f64 = trimmed.parse().ok()?;
    parsed.is_finite().then(|| parsed.floor().max(1.0) as u64)
}

fn known_hint(known: Option<bool>) -> &'static str {
    match known {
        None => t("Mewrk 不认得这个模型或这个端点（比如中转站），请按它的实际能力勾选。", "Mewrk doesn't know this model or this endpoint (a relay, say); tick it according to what the endpoint actually takes."),
        Some(true) => t("Mewrk 认得这个模型：支持，获取模型时会自动勾上。", "Mewrk knows this model: it takes it, and fetching the model ticks this."),
        Some(false) => t("Mewrk 认得这个模型：不支持。", "Mewrk knows this model: it does not take it."),
    }
}

const PROTOCOL_CAPABILITIES: [ModelCapability; 4] = [ModelCapability::ToolAppend, ModelCapability::SystemAppend, ModelCapability::AsyncTools, ModelCapability::NativeCompaction];

fn update_editor(app: &mut App, f: impl FnOnce(&mut super::ModelEditor)) {
    if let Some(editor) = state(app).model_editor.as_mut() {
        f(editor);
    }
}

/// What one capability switch's hint says. Sentences run on without a space in Chinese and with one in English.
fn capability_hint(capability: ModelCapability, family: ProviderFamily, base_url: &str, model_id: &str) -> (String, bool) {
    let known = |f: fn(ProviderFamily, &str, &str) -> Option<bool>| (!model_id.is_empty()).then(|| f(family, base_url, model_id));
    let with_known = |summary: &str, known: Option<Option<bool>>| match known {
        Some(k) => format!("{summary}{}{}", t("", " "), known_hint(k)),
        None => summary.to_owned(),
    };
    match capability {
        ModelCapability::ImageRecognition => (t("允许把图片作为输入发给这个模型。", "Lets images be sent to this model as input.").to_owned(), false),
        ModelCapability::ToolAppend => {
            if tool_append_takes_effect(family) {
                (with_known(t("让工具能在对话中途加入，且不丢提示词缓存。", "Lets tools join mid-conversation, keeping the prompt cache."), known(known_tool_append)), true)
            } else {
                (t("此项无效：这个协议不能中途追加工具。", "No effect: this protocol can't add tools mid-conversation.").to_owned(), false)
            }
        }
        ModelCapability::SystemAppend => {
            if system_append_takes_effect(family) {
                (with_known(t("让中途生效的指令以 system 消息送达。", "Sends mid-conversation instructions as system messages."), known(known_system_append)), true)
            } else {
                (t("此项无效：这个协议不能中途带 system 消息。", "No effect: this protocol can't take system messages mid-conversation.").to_owned(), false)
            }
        }
        ModelCapability::AsyncTools => {
            if async_tools_take_effect(family) {
                (with_known(t("子代理和工作流运行时，模型无需等待。", "The model needn't wait while subagents and workflows run."), known(known_async_tools)), true)
            } else {
                (t("此项无效：这个协议没有异步工具调用。", "No effect: this protocol has no asynchronous tool calls.").to_owned(), false)
            }
        }
        ModelCapability::NativeCompaction => {
            if native_compaction_takes_effect(family) {
                (with_known(t("超过阈值时，由模型自己压缩上下文。", "Past the threshold, the model compacts the context itself."), known(known_native_compaction)), true)
            } else {
                (t("此项无效：这个协议没有原生压缩。", "No effect: this protocol has no native compaction.").to_owned(), false)
            }
        }
    }
}

/// `drawer-switch-row`: each switch a box of its own.
fn switch_row(title: E, hint: E, control: E) -> E {
    let t_ = theme();
    row().min_h(44.0).mb(12.0).py(8.0).px(10.0).gap(12.0).items_center().justify_between().radius(8.0).border(t_.card_stroke).bg(t_.card_secondary).child(col().flex1().min_w(0.0).gap(2.0).child(title).child(hint)).child(control)
}

fn model_profile_drawer(provider: &ApiProvider, editor: &super::ModelEditor) -> E {
    let t_ = theme();
    let family = provider.family;
    let base_url = provider.base_url.clone();
    let draft = &editor.draft;
    let id_error = model_id_error(provider, &draft.id, editor.original_id.as_deref());
    let model_id = draft.id.trim().to_owned();

    let id_base = base_url.clone();
    let id_field = code_input("model-id", draft.id.clone(), t("例如 gpt-5", "For example, gpt-5"), move |app, id| {
        let base = id_base.clone();
        update_editor(app, move |e| {
            // A model being added takes Mewrk's protocol declarations as its ID is typed, until a switch is moved.
            if !e.append_touched {
                e.draft.capabilities.retain(|c| !PROTOCOL_CAPABILITIES.contains(c));
                e.draft.capabilities.extend(known_protocol_capabilities(family, &base, id.trim()));
            }
            e.draft.id = id;
        });
    })
    .aria_label(t("模型 ID", "Model ID"))
    .aria_invalid(id_error.is_some());
    let id_hint = match id_error {
        Some(e) => error_line(e),
        None => help(t("请求 API 时发送的 model 值", "The model value sent in API requests")),
    };
    let number = |key: &'static str, label: &'static str, value: Option<u64>, placeholder: String, set: fn(&mut ModelProfile, Option<u64>)| {
        input(key, value.map_or(String::new(), |v| v.to_string()), placeholder, move |app, v| {
            let parsed = optional_positive_integer(&v);
            update_editor(app, |e| set(&mut e.draft, parsed));
        })
        .aria_label(label)
    };
    let unknown = t("留空表示未知", "Leave blank if unknown").to_owned();
    let grid = col()
        .child(row().gap(10.0).child(field_row(t("显示名", "Display name"), Some(help(t("留空则显示模型 ID", "Falls back to the model ID when blank"))), input("model-name", draft.name.clone(), "", |app, v| update_editor(app, |e| e.draft.name = v)).aria_label(t("显示名", "Display name"))).flex1()).child(field_row(t("分组", "Group"), Some(help(t("模型列表里的分组；留空则由 ID 推断", "Group in the model list; inferred from the ID if blank"))), input("model-group-name", draft.group.clone(), "", |app, v| update_editor(app, |e| e.draft.group = v)).aria_label(t("分组", "Group"))).flex1()))
        .child(row().gap(10.0).child(field_row(t("上下文窗口", "Context window"), None, number("model-context", t("上下文窗口", "Context window"), draft.context_window, unknown.clone(), |m, v| m.context_window = v)).flex1()).child(field_row(t("单次最大输出", "Maximum output"), None, number("model-output", t("最大输出 Token", "Maximum output tokens"), draft.max_output_tokens, unknown, |m, v| m.max_output_tokens = v)).flex1()))
        .child(row().gap(10.0).child(field_row(t("缓存失效时间（分钟）", "Cache lifetime (minutes)"), Some(help_with_docs(t("这段时间内，破坏缓存的改动会先提醒。", "Within this time, cache-breaking changes ask first."), "working", "tool-lock")), number("model-cache-ttl", t("缓存失效时间（分钟）", "Cache lifetime (minutes)"), draft.cache_ttl_minutes.map(u64::from), tf("默认 {minutes}", "Default {minutes}", &[("minutes", &DEFAULT_CACHE_TTL_MINUTES)]), |m, v| m.cache_ttl_minutes = v.map(|n| n.min(u32::MAX as u64) as u32))).flex1()).child(div().flex1()));

    let switch_name = if draft.id.is_empty() { t("新模型", "New model") } else { draft.id.as_str() };
    let switch_label = |capability: &str| tf("{name} {capability}", "{name} {capability}", &[("name", &switch_name), ("capability", &capability)]);
    let mut switches = col();
    for capability in [ModelCapability::ImageRecognition, ModelCapability::ToolAppend, ModelCapability::SystemAppend, ModelCapability::AsyncTools, ModelCapability::NativeCompaction] {
        let (hint_text, linked) = capability_hint(capability, family, &base_url, &model_id);
        let on = draft.capabilities.contains(&capability);
        let title = row().gap(5.0).items_center().child(icon(capability_icon(capability), 12.0).color(t_.text_secondary)).child(text(capability_label(capability)).ty(CAPTION_STRONG));
        let hint = if linked { help_with_docs(hint_text, "providers", "model-properties") } else { help(hint_text) };
        let control = labeled_switch(("model-capability", capability as u8), on, false, move |app| {
            update_editor(app, |e| {
                if PROTOCOL_CAPABILITIES.contains(&capability) {
                    e.append_touched = true;
                }
                if on {
                    e.draft.capabilities.remove(&capability);
                } else {
                    e.draft.capabilities.insert(capability);
                }
            });
        })
        .aria_label(switch_label(capability_label(capability)));
        switches = switches.child(switch_row(title, hint, control));
    }
    // Only the Messages protocol takes client-placed cache breakpoints; elsewhere the value is stored but idle.
    let cache_on = draft.prompt_cache;
    let cache_hint = if prompt_cache_takes_effect(family) { help_with_docs(t("为请求打缓存断点；关闭后完全不缓存。", "Marks cache breakpoints; turning it off means no caching."), "providers", "model-properties") } else { help(t("此项只对 Anthropic Messages 有效。", "Only takes effect on Anthropic Messages.")) };
    switches = switches.child(switch_row(text(t("提示词缓存", "Prompt caching")).ty(CAPTION_STRONG), cache_hint, labeled_switch("model-prompt-cache", cache_on, false, move |app| update_editor(app, |e| e.draft.prompt_cache = !cache_on)).aria_label(switch_label(t("提示词缓存", "Prompt caching")))));

    // The form never changes what is asked for: it only decides how the thinking that streams back is read and shown.
    let reasoning_hint = if reasoning_content_takes_effect(family) { t("端点流式返回可读的思考文字时，选「明文」。", "Choose Plaintext if the endpoint streams readable reasoning.") } else { t("这个协议下此项无效。", "No effect on this protocol.") };
    let reasoning_options: Vec<Arc<str>> = vec![Arc::from(t("明文思考字段", "Plaintext reasoning field")), Arc::from(t("加密思考字段", "Encrypted reasoning field"))];
    let reasoning_index = if draft.reasoning_content == ReasoningContent::Plaintext { 0 } else { 1 };
    let reasoning = select::<App>("model-reasoning", reasoning_options, reasoning_index, |app, i| update_editor(app, |e| e.draft.reasoning_content = if i == 0 { ReasoningContent::Plaintext } else { ReasoningContent::Encrypted })).aria_label(t("思考字段形态", "Reasoning field form"));

    let body = col().child(field_row(t("模型 ID", "Model ID"), Some(id_hint), id_field)).child(grid).child(switches).child(col().child(field_row(t("思考字段形态", "Reasoning field form"), Some(help_with_docs(reasoning_hint, "providers", "model-properties")), reasoning)));
    let footer = row().gap(8.0).child(subtle_button::<App>(t("取消", "Cancel")).key("model-cancel").on_click(|app: &mut App| state(app).model_editor = None)).child(accent_button::<App>(t("保存", "Save")).key("model-save").disabled(id_error.is_some()).on_click(save_model_draft));
    drawer("model-editor", if editor.create { t("添加模型", "Add model") } else { t("模型属性", "Model properties") }, Some(tf("{provider} · 保存后生效", "{provider} · Applies on save", &[("provider", &provider.name)])), false, body, Some(footer), |app| state(app).model_editor = None)
}

pub fn save_model_draft(app: &mut App) {
    let Some(editor) = state(app).model_editor.clone() else { return };
    let Some(provider) = app.doc.assets.api_providers.iter().find(|p| p.id == editor.provider_id).cloned() else { return };
    if model_id_error(&provider, &editor.draft.id, editor.original_id.as_deref()).is_some() {
        return;
    }
    let id = editor.draft.id.trim().to_owned();
    let saved = ModelProfile { id: id.clone(), ..editor.draft.clone() };
    if editor.create {
        let new_id = id.clone();
        replace_provider(
            app,
            &editor.provider_id,
            move |p| {
                p.models.push(saved);
                if p.active_model_id.is_none() {
                    p.active_model_id = Some(new_id);
                }
            },
            None,
        );
    } else if let Some(original) = editor.original_id.clone() {
        let matched = original.clone();
        replace_provider(
            app,
            &editor.provider_id,
            move |p| {
                if let Some(slot) = p.models.iter_mut().find(|m| m.id == matched) {
                    *slot = saved;
                }
            },
            Some((original, Some(id))),
        );
    }
    state(app).model_editor = None;
}
