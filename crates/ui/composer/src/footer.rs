//! Under the composer: security level, plan mode and the add menu on the left, then the model, the reasoning effort and the context
//! meter on the right. The `composer__footer` of `App.tsx`, with `ComposerAddMenu.tsx` and `ModelCacheMark` of `LockTone.tsx`.

use crate::attach::pick_and_attach;
use crate::meter::{context_usage_is_open, context_usage_meter, ContextUsageCounts, ContextUsageProps};
use crate::{Active, ModelChoice};
use chrono::TimeZone;
use mewrk_client::auto_compact::{compaction_method_in_effect, default_compaction_method};
use mewrk_client::model::{CompactionMethod, ReasoningEffort, SecurityLevel};
use mewrk_client::prelude::*;
use mewrk_client::runs::wire_view;
use mewrk_client::tool_lock::{model_cache_warm_until, plan_mode_tone, restore_locked_settings, tool_lock_model_of, tool_lock_state};
use mewrk_ui_common::popover_menu::probe::say;
use mewrk_ui_common::popover_menu::{menu_is_open, option_box, option_label, PopoverMenu, PopoverMenuItem, PopoverMenuSection};
use std::borrow::Cow;

pub const SECURITY_MENU: &str = "composer-security-menu";
pub const MODEL_MENU: &str = "composer-model-menu";
pub const EFFORT_MENU: &str = "composer-effort-menu";
pub const ADD_MENU: &str = "composer-add-menu";

pub const SECURITY_LEVEL_OPTIONS: [SecurityLevel; 3] = [SecurityLevel::RequestApproval, SecurityLevel::AllowEdits, SecurityLevel::FullAccess];
pub const REASONING_EFFORTS: [ReasoningEffort; 5] = [ReasoningEffort::Low, ReasoningEffort::Medium, ReasoningEffort::High, ReasoningEffort::Extra, ReasoningEffort::Max];

pub fn plan_toggle_key() -> u64 {
    hash_key("composer-plan-toggle")
}

pub fn security_level_label(level: SecurityLevel) -> &'static str {
    match level {
        SecurityLevel::RequestApproval => t("手动", "Manual"),
        SecurityLevel::AllowEdits => t("允许编辑", "Accept edits"),
        SecurityLevel::FullAccess => t("完全访问", "Full access"),
    }
}

/// The levels are named the same in every language: they are the providers' own words.
pub fn effort_label(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Extra => "extra",
        ReasoningEffort::Max => "max",
    }
}

fn effort_id(effort: ReasoningEffort) -> &'static str {
    effort_label(effort)
}

fn security_id(level: SecurityLevel) -> &'static str {
    match level {
        SecurityLevel::RequestApproval => "request_approval",
        SecurityLevel::AllowEdits => "allow_edits",
        SecurityLevel::FullAccess => "full_access",
    }
}

/// `ModelCacheMark`: a model whose prompt cache for this conversation is still warm, in the cache tone, with the moment it runs out.
pub fn model_cache_mark(until: i64) -> E {
    let t_ = theme();
    let time = chrono::Local.timestamp_millis_opt(until).single().map_or_else(String::new, |at| at.format("%H:%M").to_string());
    let label = tf("提示缓存有效，{time} 过期", "Prompt cache warm until {time}", &[("time", &time)]);
    say(&label);
    div().role(Role::Image).aria_label(label.clone()).tooltip(label).child(icon("DatabaseZap", 12.0).color(t_.cache_warm))
}

/// `selectModel`: the choice is the document's, not the conversation's. Picking a model puts back what a conversation's lock holds for
/// it (`restoreLockedSettings`).
pub fn select_model(app: &mut App, provider_id: &str, model_id: &str) {
    let chosen = app.update_document_if(|doc| {
        let Some(provider) = doc.assets.api_providers.iter_mut().find(|provider| provider.id == provider_id && provider.enabled) else { return false };
        if !provider.models.iter().any(|model| model.id == model_id) {
            return false;
        }
        provider.active_model_id = Some(model_id.to_owned());
        doc.global_settings.active_provider_id = Some(provider_id.to_owned());
        true
    });
    if chosen {
        restore_locks_for_model(app, provider_id, model_id);
    }
}

fn restore_locks_for_model(app: &mut App, provider_id: &str, model_id: &str) {
    let provider = app.doc.assets.api_providers.iter().find(|provider| provider.id == provider_id);
    let model = provider.and_then(|provider| provider.models.iter().find(|model| model.id == model_id));
    let Some(lock_model) = tool_lock_model_of(provider, model) else { return };
    let now = chrono::Utc::now().timestamp_millis();
    let mut restored = Vec::new();
    for workspace in &app.doc.workspaces {
        for conversation in &workspace.conversations {
            if let Cow::Owned(settings) = restore_locked_settings(&conversation.settings, &tool_lock_state(&conversation.settings, Some(&lock_model), now).with_called_tools(&conversation.contexts)) {
                restored.push((workspace.id.clone(), conversation.id.clone(), settings));
            }
        }
    }
    for (workspace_id, conversation_id, settings) in restored {
        app.update_conversation(&workspace_id, &conversation_id, move |conversation| conversation.settings = settings);
    }
}

/// `enabledModelChoices` as a menu: one section per provider, so the provider reads as a heading rather than a repeated subtitle.
pub(crate) fn model_menu_sections(app: &App, active: &Active, chosen: Option<&ModelChoice>) -> Vec<PopoverMenuSection> {
    let now = chrono::Utc::now().timestamp_millis();
    app.doc
        .assets
        .api_providers
        .iter()
        .filter(|provider| provider.enabled && !provider.models.is_empty())
        .map(|provider| {
            let items = provider.models.iter().map(|model| {
                let (provider_id, model_id) = (provider.id.clone(), model.id.clone());
                let checked = chosen.is_some_and(|choice| choice.provider.id == provider.id && choice.model.id == model.id);
                let cached = model_cache_warm_until(&active.settings, &provider.id, &model.id, model.cache_ttl_minutes, now);
                let mut item = PopoverMenuItem::new(format!("{}\u{0}{}", provider.id, model.id), model.id.clone()).checked(checked).on(move |app: &mut App| select_model(app, &provider_id, &model_id));
                if let Some(until) = cached {
                    item = item.badge(model_cache_mark(until));
                }
                item
            });
            PopoverMenuSection::new(provider.id.clone()).label(provider.name.clone()).items(items)
        })
        .collect()
}

/// `onSelect` of the effort menu: updates the global last effort with the conversation setting and its workspace snapshot; a draft
/// updates only the global side until materialization.
pub fn set_reasoning_effort(app: &mut App, is_draft: bool, workspace_id: Option<String>, conversation_id: String, effort: ReasoningEffort) {
    let changed_at = mewrk_client::doc::now_iso();
    if is_draft
        && let Some(workspace_id) = &workspace_id
    {
        app.update_draft(workspace_id, |current| {
            let mut next = current.clone();
            next.settings.reasoning_effort = effort;
            Some(next)
        });
    }
    app.update_document(|doc| {
        doc.global_settings.last_reasoning_effort = effort;
        if is_draft {
            return;
        }
        let Some(workspace) = doc.workspaces.iter_mut().find(|workspace| Some(&workspace.id) == workspace_id.as_ref()) else { return };
        let mut remembered = None;
        if let Some(conversation) = workspace.conversations.iter_mut().find(|conversation| conversation.id == conversation_id) {
            conversation.settings.reasoning_effort = effort;
            conversation.updated_at = changed_at.clone();
            remembered = Some(conversation.settings.clone());
        }
        if remembered.is_some() {
            workspace.last_conversation_settings = remembered;
        }
    });
}

/// The tooltip of the plan toggle.
pub fn plan_toggle_title(cache_tone: bool, enabled: bool) -> &'static str {
    if cache_tone {
        t(
            "开启计划模式：模型改动任何东西之前先写计划并请你批准。缓存还热：当前模型不能中途追加工具，开启要加入 plan 和 exit_plan_mode，会让缓存失效。",
            "Turn on plan mode: the model writes a plan and asks you to approve it before changing anything. The cache is still warm: the current model cannot take tools mid-conversation, so adding plan and exit_plan_mode throws it away.",
        )
    } else if enabled {
        t("计划模式已开启：模型先写计划、请你批准，批准后自动关闭", "Plan mode is on: the model writes a plan and asks you to approve it; approval turns it off")
    } else {
        t("开启计划模式：模型改动任何东西之前先写计划并请你批准", "Turn on plan mode: the model writes a plan and asks you to approve it before changing anything")
    }
}

/// `ComposerAddFiles`: pictures are one kind of file, so there is one way in. The picker takes anything, and what the message cannot
/// carry is turned away with a reason once the files are read, the same as a paste or a drop.
pub fn composer_add_files(key: impl std::hash::Hash, disabled: bool, image_input: bool, unavailable_reason: Option<String>, on_choose: Rc<dyn Fn(&mut App)>) -> E {
    let t_ = theme();
    let label = t("添加内容", "Add content");
    PopoverMenu::new(key, label, move |open| {
        row().center().size(25.0, 25.0).radius(13.0).bg(if open { t_.subtle_pressed } else { t_.control_fill }).when(!disabled, |e| e.hover_bg(t_.subtle_hover)).child(icon("Plus", 16.0).color(if disabled { t_.text_disabled } else { t_.text_secondary }))
    })
    .disabled(disabled)
    .menu_label(label)
    .dense()
    .sections(move || {
        let title = unavailable_reason.clone().unwrap_or_else(|| {
            if image_input { t("图片、PDF 或文本文件；也可粘贴或拖入", "Images, PDFs, or text files; paste and drop also work").to_owned() } else { t("PDF 或文本文件（当前模型不支持图片）；也可粘贴或拖入", "PDFs or text files (the current model takes no images); paste and drop also work").to_owned() }
        });
        say(&title);
        vec![PopoverMenuSection::new("attach").item(PopoverMenuItem::new("files", t("上传文件", "Upload files")).title(title).disabled(unavailable_reason.is_some()).on(move |app: &mut App| on_choose(app)))]
    })
    .build()
}

/// `.composer-option`, tightened in a narrow window.
fn composer_option(open: bool, disabled: bool) -> E {
    let option = option_box(open, disabled);
    if crate::narrow_window() { option.px(6.0) } else { option }
}

/// `.composer-option`'s word, at the size the window allows it.
fn composer_option_label(label: String, disabled: bool) -> E {
    option_label(label, disabled).ty(crate::chips::chip_type())
}

/// The conversation's context meter, with what it measures.
fn meter(app: &mut App, active: &Active, chosen: Option<&ModelChoice>) -> E {
    let target = chosen.map(|choice| (&choice.provider, &choice.model));
    let usage = crate::context_usage_of(app, active, chosen);
    let settings = &active.settings;
    let (contexts, compaction) = if context_usage_is_open() {
        // Copied only while someone can see the breakdown.
        let view = match active.draft_contexts.as_deref() {
            Some(contexts) => wire_view(contexts, target),
            None => wire_view(app.conversation(&active.id).map_or(&[][..], |conversation| conversation.contexts.as_slice()), target),
        };
        (view.contexts.to_vec(), view.compaction.cloned())
    } else {
        (Vec::new(), None)
    };
    let attached: Vec<_> = active.attached_workspaces.clone();
    let workspace_keys: Vec<String> = mewrk_client::workspaces::capability_workspaces(active.workspace_id.as_deref().and_then(|id| app.workspace(id)), &attached).into_iter().map(|workspace| workspace.key).collect();
    let agent_roles = crate::selected_role_count(app, &settings.agent_ids, &workspace_keys);
    let chosen_method = settings.compaction_method.unwrap_or(if active.is_draft { default_compaction_method(target) } else { CompactionMethod::Handoff });
    let (workspace_id, conversation_id, can_compact) = (active.workspace_id.clone(), active.id.clone(), !active.is_draft);
    context_usage_meter(ContextUsageProps {
        contexts,
        compaction,
        tokens: usage.tokens,
        estimated: usage.estimated,
        unprojectable: usage.unprojectable,
        context_window: chosen.and_then(|choice| choice.model.context_window),
        counts: ContextUsageCounts { tools: settings.enabled_tools.len(), mcp_servers: settings.mcp_ids.len(), skills: settings.skill_ids.len(), agent_roles },
        auto_compact: Some(app.doc.global_settings.auto_compact.clone()),
        on_auto_compact_change: Some(Rc::new(|app, auto_compact| app.update_global_settings(move |doc| doc.global_settings.auto_compact = auto_compact))),
        auto_compact_unavailable: chosen.is_some_and(|choice| !mewrk_client::model_capabilities::appends_tools(&choice.provider, &choice.model)),
        native_compaction_available: chosen.is_some_and(|choice| mewrk_client::model_capabilities::takes_native_compaction(&choice.provider, &choice.model)),
        compaction_method: Some(compaction_method_in_effect(chosen_method, target)),
        on_compaction_method_change: Some(Rc::new(|app, method| app.update_active_conversation_settings(move |settings| settings.compaction_method = Some(method)))),
        on_compact_now: Some(Rc::new(move |app| {
            if let (true, Some(workspace_id)) = (can_compact, &workspace_id) {
                app.compact_conversation_now(workspace_id, &conversation_id, |_, _| {});
            }
        })),
        compact_now_blocked: crate::compact_now_blocked(app, active, chosen),
    })
}

/// The footer.
pub(crate) fn composer_footer(app: &mut App, active: &Active, chosen: Option<&ModelChoice>) -> E {
    let t_ = theme();
    let held = active.lifecycle_running;

    // ---- security: movable at any time, a streaming turn and a waiting approval card included. The host moves the running turn's
    // level at once.
    let security = active.settings.security_level;
    let security_label = security_level_label(security).to_owned();
    say(&security_label);
    let shown = security_label.clone();
    let security_menu = PopoverMenu::new(SECURITY_MENU, tf("安全层级：{name}", "Security level: {name}", &[("name", &security_label)]), move |open| composer_option(open, held).child(composer_option_label(shown, held)))
        .disabled(held)
        .menu_label(t("安全层级", "Security level"))
        .width(256.0)
        .sections(move || {
            vec![PopoverMenuSection::new("security").items(SECURITY_LEVEL_OPTIONS.into_iter().map(|option| {
                // Security options are conversation-only settings, not preset composition; update them directly without detaching a preset.
                PopoverMenuItem::new(security_id(option), security_level_label(option)).checked(security == option).on(move |app: &mut App| app.update_active_conversation(|conversation| conversation.settings.security_level = option))
            }))]
        })
        .build();

    // ---- plan mode is a switch of its own, not a level: on, the model plans and asks for approval before it changes anything; an
    // approved plan turns it off. Movable at any time, a streaming turn included: the next round boundary tells the model.
    let plan_enabled = active.settings.plan_mode_enabled;
    let lock_model = tool_lock_model_of(chosen.map(|choice| &choice.provider), chosen.map(|choice| &choice.model));
    let tone = chosen.and_then(|_| plan_mode_tone(&tool_lock_state(&active.settings, lock_model.as_ref(), chrono::Utc::now().timestamp_millis()), plan_enabled)).is_some();
    let plan_label = t("计划", "Plan");
    let plan_title = plan_toggle_title(tone, plan_enabled);
    say(plan_label);
    say(plan_title);
    if plan_enabled {
        say("plan-on");
    }
    let plan_color = if held { t_.text_disabled } else if tone { t_.cache_warm } else { t_.text_secondary };
    let plan_toggle = composer_option(false, held)
        .key(plan_toggle_key())
        .tooltip(plan_title)
        .aria_pressed(plan_enabled)
        .child(text(plan_label).ty(crate::chips::chip_type()).color(plan_color).nowrap())
        .child(div().size(7.0, 7.0).radius(3.5).shrink0().border(plan_color).when(plan_enabled, |e| e.bg(plan_color)));
    let plan_toggle = if held { plan_toggle.disabled(true).on_click(|_: &mut App| {}) } else { plan_toggle.on_click(move |app: &mut App| app.update_active_conversation(|conversation| conversation.settings.plan_mode_enabled = !plan_enabled)) };

    let conversation = active.id.clone();
    let add_menu = composer_add_files((ADD_MENU, active.id.as_str()), held, active.image_input, None, Rc::new(move |app| pick_and_attach(app, &conversation)));

    // ---- the model
    let any_enabled = app.doc.assets.api_providers.iter().any(|provider| provider.enabled && !provider.models.is_empty());
    let model_label = match chosen {
        Some(choice) => format!("{} · {}", choice.provider.name, choice.model.id),
        None if any_enabled => t("选择模型", "Select a model").to_owned(),
        None => t("没有已启用的模型", "No enabled models").to_owned(),
    };
    say(&model_label);
    let model_held = active.model_running || held || !any_enabled;
    let shown = model_label.clone();
    let sections = if menu_is_open(MODEL_MENU) { model_menu_sections(app, active, chosen) } else { Vec::new() };
    let (window_w, _) = window_size();
    let narrow = crate::narrow_window();
    let model_menu = PopoverMenu::new(MODEL_MENU, tf("模型：{name}", "Model: {name}", &[("name", &model_label)]), move |open| composer_option(open, model_held).max_w(if narrow { 180.0f32 } else { 300.0f32 }.min(window_w * 0.34)).child(composer_option_label(shown, model_held)))
        .title(model_label)
        .disabled(model_held)
        .menu_label(t("模型", "Model"))
        .width(300.0)
        .align_end()
        .dense()
        .search(t("搜索模型…", "Search models…"))
        .empty_label(if any_enabled { t("没有匹配的模型", "No matching models") } else { t("没有已启用的模型", "No enabled models") })
        .sections(move || sections)
        .build();

    // ---- the effort
    let effort = active.settings.reasoning_effort;
    let effort_held = active.model_running || held;
    say(effort_label(effort));
    let (draft_effort, effort_workspace, effort_conversation) = (active.is_draft, active.workspace_id.clone(), active.id.clone());
    let effort_menu = PopoverMenu::new(EFFORT_MENU, tf("思考程度：{name}", "Reasoning effort: {name}", &[("name", &effort_label(effort))]), move |open| composer_option(open, effort_held).child(composer_option_label(effort_label(effort).to_owned(), effort_held)))
        .disabled(effort_held)
        .menu_label(t("思考程度", "Reasoning effort"))
        .width(232.0)
        .align_end()
        .sections(move || {
            vec![PopoverMenuSection::new("effort").items(REASONING_EFFORTS.into_iter().map(|level| {
                let (workspace, conversation) = (effort_workspace.clone(), effort_conversation.clone());
                PopoverMenuItem::new(effort_id(level), effort_label(level)).checked(effort == level).on(move |app: &mut App| set_reasoning_effort(app, draft_effort, workspace.clone(), conversation.clone(), level))
            }))]
        })
        .build();

    // A warm cache runs out with nothing else happening, and with it go the marks and the plan switch's tone.
    let now = chrono::Utc::now().timestamp_millis();
    let soonest = if menu_is_open(MODEL_MENU) {
        app.doc.assets.api_providers.iter().filter(|provider| provider.enabled).flat_map(|provider| provider.models.iter().filter_map(|model| model_cache_warm_until(&active.settings, &provider.id, &model.id, model.cache_ttl_minutes, now))).min()
    } else {
        chosen.and_then(|choice| model_cache_warm_until(&active.settings, &choice.provider.id, &choice.model.id, choice.model.cache_ttl_minutes, now))
    };
    let clock = soonest.map(|until| custom(move |_: &mut App, ui: &mut Ui<'_>, _: Rect| ui.wake_after((until - now).max(0) as f32 / 1000.0 + 0.25)).w(0.0).h(0.0).pass_through());

    let meter = meter(app, active, chosen);
    let (footer_gap, group_gap) = if crate::narrow_window() { (4.0, 3.0) } else { (7.0, 6.0) };
    row()
        .key("composer-footer")
        .w_full()
        .max_w(820.0)
        .mx_auto()
        .min_h(29.0)
        .mt(6.0)
        .px(2.0)
        .items_center()
        .gap(footer_gap)
        .child(row().min_w(0.0).items_center().gap(group_gap).shrink0().child(security_menu).child(plan_toggle).child(add_menu))
        .child(row().flex1().min_w(0.0).items_center().justify_end().gap(group_gap).child(model_menu).child(effort_menu).child(meter))
        .child(clock)
}
