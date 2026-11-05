//! `AppearanceSettings/LocalModelSettings.tsx`: the uses of the local helper model, the build it runs and its prompts.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use mewrk_client::model::LocalModelPreferences;
use mewrk_client::prelude::*;

use crate::logic::local_model::{percent_of, DefaultPrompts, DownloadSource, LocalModelStatus, Machine, Phase, PrepareStep, PromptReport, Task, Unavailable, VariantId, VariantStatus};
use crate::logic::store;
use crate::providers_format::format_bytes;
use crate::widgets::*;

/// Quiet time after typing before a prompt's KV cache is recomputed.
const PROMPT_SETTLE: Duration = Duration::from_millis(800);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Use {
    Titles,
    ShellExplanations,
    ErrorExplanations,
}

/// Lives as long as the app: the host keeps downloading whether or not this page is open.
#[derive(Default)]
struct Controller {
    status: Option<LocalModelStatus>,
    push_installed: bool,
    refreshing: bool,
}

#[derive(Default)]
struct PromptState {
    signature: Option<String>,
    report: Option<PromptReport>,
    computing: bool,
    error: Option<String>,
    token: u64,
    edited: bool,
    timer: Option<mewrk_client::timers::TimerId>,
}

#[derive(Default)]
pub struct LocalModelPage {
    generation: u64,
    started: bool,
    confirming: Option<Use>,
    choice: Option<VariantId>,
    managing: bool,
    manage_started: bool,
    error: Option<String>,
    dialog_error: Option<String>,
    unreadable: bool,
    asking: Option<(VariantId, Option<Use>, bool)>,
    defaults: Option<DefaultPrompts>,
    confirm_remove: Option<VariantId>,
    prompts: HashMap<&'static str, PromptState>,
}

fn page_state(app: &mut App) -> &mut LocalModelPage {
    app.local::<LocalModelPage>()
}

pub fn unmount(app: &mut App) {
    let s = page_state(app);
    let generation = s.generation + 1;
    *s = LocalModelPage { generation, ..Default::default() };
}

/// The last status the host gave.
pub fn local_model_status(app: &mut App) -> Option<LocalModelStatus> {
    app.local::<Controller>().status.clone()
}

fn set_status(app: &mut App, next: LocalModelStatus) {
    app.local::<Controller>().status = Some(next);
    repaint();
}

fn install_push(app: &mut App) {
    if app.local::<Controller>().push_installed {
        return;
    }
    app.local::<Controller>().push_installed = true;
    app.on_push_event(|app, event| {
        if event.get("type").and_then(Value::as_str) == Some("localModelChanged")
            && let Some(status) = event.get("status").and_then(|s| serde_json::from_value::<LocalModelStatus>(s.clone()).ok()) {
                set_status(app, status);
            }
    });
}

/// Asks for the whole status; calls made while one is in flight share it.
fn refresh(app: &mut App) {
    install_push(app);
    if app.local::<Controller>().refreshing {
        return;
    }
    app.local::<Controller>().refreshing = true;
    app.invoke::<LocalModelStatus>("local_model_status", json!({}), |app, result| {
        app.local::<Controller>().refreshing = false;
        match result {
            Ok(status) => set_status(app, status),
            Err(_) => page_state(app).unreadable = true,
        }
    });
}

/// A verb whose answer is the new status, reporting a failure into the dialog when one is up.
fn run_verb(app: &mut App, command: &'static str, args: Value) {
    page_state(app).dialog_error = None;
    app.invoke::<LocalModelStatus>(command, args, |app, result| match result {
        Ok(status) => set_status(app, status),
        Err(e) => page_state(app).dialog_error = Some(e),
    });
}

/// Whether the system is set to Chinese for mainland China, where Hugging Face is often slow or unreachable.
fn is_mainland_china_locale() -> bool {
    static LOCALE: OnceLock<String> = OnceLock::new();
    let locale = LOCALE.get_or_init(|| {
        let env = std::env::var("MEWRK_LANG").or_else(|_| std::env::var("LANG")).unwrap_or_default();
        if !env.is_empty() && !env.starts_with('C') {
            return env;
        }
        std::process::Command::new("defaults").args(["read", "-g", "AppleLocale"]).output().ok().map_or_else(String::new, |o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    });
    let lowered = locale.to_lowercase().replace('-', "_");
    lowered.starts_with("zh_cn") || lowered.starts_with("zh_hans_cn")
}

fn set_use(app: &mut App, what: Use, on: bool) {
    store::change(app, move |doc| {
        let p: &mut LocalModelPreferences = &mut doc.global_settings.appearance.local_model;
        match what {
            Use::Titles => p.titles = on,
            Use::ShellExplanations => p.shell_explanations = on,
            Use::ErrorExplanations => p.error_explanations = on,
        }
    });
}

fn install(app: &mut App, variant: VariantId, china_mirror: bool, enable: Option<Use>, in_dialog: bool) {
    if let Some(what) = enable {
        set_use(app, what, true);
    }
    install_push(app);
    app.invoke::<LocalModelStatus>("local_model_install", json!({ "variant": variant.wire(), "chinaMirror": china_mirror }), move |app, result| match result {
        Ok(status) => set_status(app, status),
        Err(e) => {
            let s = page_state(app);
            if in_dialog {
                s.dialog_error = Some(e);
            } else {
                s.error = Some(e);
            }
        }
    });
}

fn start_download(app: &mut App, variant: VariantId, enable: Option<Use>, in_dialog: bool) {
    if is_mainland_china_locale() {
        page_state(app).asking = Some((variant, enable, in_dialog));
    } else {
        install(app, variant, false, enable, in_dialog);
    }
}

fn variant_name(id: VariantId) -> &'static str {
    match id {
        VariantId::Ane => t("神经网络引擎版（Core ML）", "Neural Engine build (Core ML)"),
        VariantId::Mlx => t("GPU 版（MLX）", "GPU build (MLX)"),
        VariantId::Llama => t("llama.cpp 版", "llama.cpp build"),
    }
}

fn variant_description(id: VariantId) -> &'static str {
    match id {
        VariantId::Ane => t("省电，不占显卡；首次加载约需两分钟。", "Low power, leaves the GPU free; first load takes about 2 minutes."),
        VariantId::Mlx => t("加载快，但生成时占用显卡。", "Loads fast, but uses the GPU while generating."),
        VariantId::Llama => t("运行 GGUF 版，有显卡时用显卡。", "Runs the GGUF build, on the GPU when there is one."),
    }
}

pub fn unavailable_reason(reason: Unavailable) -> &'static str {
    match reason {
        Unavailable::NeedsAppleSilicon => t("需要 Apple 芯片的 Mac", "Needs a Mac with Apple silicon"),
        Unavailable::NoNeuralEngine => t("这台 Mac 上没有可用的神经网络引擎", "No usable Neural Engine on this Mac"),
        Unavailable::NeedsMacos14 => t("需要 macOS 14 或更高版本", "Needs macOS 14 or later"),
        Unavailable::NeedsMacos15 => t("需要 macOS 15 或更高版本", "Needs macOS 15 or later"),
        Unavailable::NotInThisBuild => t("这个版本的 Mewrk 没有包含它", "Not included in this build of Mewrk"),
    }
}

/// "Apple M4 Pro (Mac16,7) · 16-core Neural Engine · macOS 15.4", or nothing off a Mac.
fn machine_summary(machine: &Machine) -> Option<String> {
    if machine.chip.is_none() && machine.os_version.is_none() {
        return None;
    }
    let mut parts = Vec::new();
    if let Some(chip) = &machine.chip {
        parts.push(match &machine.model {
            Some(model) => tf("{chip}（{model}）", "{chip} ({model})", &[("chip", chip), ("model", model)]),
            None => chip.clone(),
        });
    }
    parts.push(match machine.neural_engine_cores {
        Some(cores) if cores > 0 => tf("{cores} 核神经网络引擎", "{cores}-core Neural Engine", &[("cores", &cores)]),
        _ => t("无神经网络引擎", "No Neural Engine").to_owned(),
    });
    if let Some(os) = &machine.os_version {
        parts.push(format!("macOS {os}"));
    }
    Some(parts.join(" · "))
}

fn download_source(source: DownloadSource) -> &'static str {
    match source {
        DownloadSource::HuggingFace => "Hugging Face",
        DownloadSource::HfMirror => "hf-mirror.com",
        DownloadSource::Mirror => t("镜像", "mirror"),
        DownloadSource::Release => t("官方发布页", "the official release"),
    }
}

/// One build's state in a line.
pub fn variant_summary(variant: &VariantStatus, status: &LocalModelStatus) -> String {
    match &variant.phase {
        Phase::Missing => tf("未下载 · 约 {size}", "Not downloaded · about {size}", &[("size", &format_bytes(variant.download_bytes))]),
        Phase::Unsupported { reason } => unavailable_reason(*reason).to_owned(),
        Phase::Downloading { received, total, source } => tf("正在下载 {percent}%（{source}）", "Downloading {percent}% ({source})", &[("percent", &percent_of(*received, *total)), ("source", &download_source(*source))]),
        Phase::Preparing { step, .. } => match step {
            PrepareStep::Compile => t("正在编译模型…", "Compiling the model…").to_owned(),
            PrepareStep::Verify => t("正在校验编译结果…", "Checking the compiled model…").to_owned(),
            PrepareStep::Unpack => t("正在解压 llama.cpp 运行库…", "Unpacking llama.cpp…").to_owned(),
            PrepareStep::Other => t("正在准备模型…", "Preparing the model…").to_owned(),
        },
        Phase::Ready => {
            if status.active != Some(variant.id) {
                return tf("已下载 · 占用 {size}", "Downloaded · {size} on disk", &[("size", &format_bytes(variant.disk_bytes))]);
            }
            let ane = variant.id == VariantId::Ane;
            if status.warming {
                return if ane { t("正在加载并缓存提示词（系统里没有为这台 Mac 编译好的副本时要先编译，约两分钟）…", "Loading and caching the prompts (compiled for this Mac first when the system has no compiled copy, about two minutes)…") } else { t("正在加载并缓存提示词…", "Loading and caching the prompts…") }.to_owned();
            }
            if status.loading {
                return if ane { t("正在加载（系统里没有为这台 Mac 编译好的副本时要先编译，约两分钟）…", "Loading (compiled for this Mac first when the system has no compiled copy, about two minutes)…") } else { t("正在加载…", "Loading…") }.to_owned();
            }
            let mut parts = vec![t("使用中", "In use").to_owned()];
            if let Some(device) = &status.device {
                parts.push(device.clone());
            }
            parts.push(tf("占用 {size}", "{size} on disk", &[("size", &format_bytes(variant.disk_bytes))]));
            parts.join(" · ")
        }
        Phase::Failed { message } => tf("安装失败：{message}", "Install failed: {message}", &[("message", message)]),
    }
}

/// The card's summary: the build in use, or what is happening instead.
pub fn status_summary(status: Option<&LocalModelStatus>) -> String {
    let Some(status) = status else { return t("正在读取状态…", "Reading status…").to_owned() };
    let line = |v: &VariantStatus| format!("{} · {}", variant_name(v.id), variant_summary(v, status));
    if let Some(busy) = status.variants.iter().find(|v| matches!(v.phase, Phase::Downloading { .. } | Phase::Preparing { .. })) {
        return line(busy);
    }
    if let Some(active) = status.variants.iter().find(|v| Some(v.id) == status.active && v.phase == Phase::Ready) {
        return line(active);
    }
    if let Some(failed) = status.variants.iter().find(|v| matches!(v.phase, Phase::Failed { .. })) {
        return line(failed);
    }
    if status.recommended.is_none() {
        let reason = status.variants.iter().find_map(|v| match v.phase {
            Phase::Unsupported { reason } => Some(reason),
            _ => None,
        });
        return match reason {
            Some(r) => tf("本机不能运行本地小模型：{reason}", "This machine can't run the local model: {reason}", &[("reason", &unavailable_reason(r))]),
            None => t("本机不能运行本地小模型", "This machine can't run the local model").to_owned(),
        };
    }
    t("未下载模型", "No model downloaded").to_owned()
}

fn choose_variant_dialog(app: &mut App, status: &LocalModelStatus) -> E {
    let t_ = theme();
    let choice = page_state(app).choice.or(status.recommended);
    let mut options = col().gap(8.0).role(Role::RadioGroup).aria_label(t("模型版本", "Model build"));
    for variant in &status.variants {
        let disabled = matches!(variant.phase, Phase::Unsupported { .. });
        let id = variant.id;
        let meta = match variant.phase {
            Phase::Unsupported { reason } => unavailable_reason(reason).to_owned(),
            _ => tf("下载约 {size}", "About {size} to download", &[("size", &format_bytes(variant.download_bytes))]),
        };
        options = options.child(
            row()
                .key(("variant-choice", id.wire()))
                .gap(10.0)
                .py(10.0)
                .px(12.0)
                .items_start()
                .radius(8.0)
                .border(if choice == Some(id) { t_.accent } else { t_.card_stroke })
                .when(disabled, |e| e.opacity(0.55))
                .disabled(disabled)
                // The row is the control, a `<label>` and its dot being one stop: the dot only shows its state.
                .role(Role::RadioButton)
                .aria_checked(choice == Some(id))
                .aria_label(variant_name(id))
                .aria_description(variant_description(id))
                .on_click(move |app: &mut App| page_state(app).choice = Some(id))
                .child(radio::<App>(choice == Some(id), |_: &mut App| {}).inert().aria_hidden(true))
                .child(
                    col()
                        .flex1()
                        .min_w(0.0)
                        .gap(2.0)
                        .child(row().gap(6.0).items_center().child(text(variant_name(id)).ty(CAPTION_STRONG)).child((status.recommended == Some(id)).then(|| text(t("推荐", "Recommended")).ty(FINE).color(t_.accent_text).nowrap())))
                        .child(help(variant_description(id)))
                        .child(text(meta).ty(FINE).color(t_.text_tertiary)),
                ),
        );
    }
    let machine = machine_summary(&status.machine).map(|m| help(tf("本机：{machine}", "This Mac: {machine}", &[("machine", &m)])).mb(10.0));
    let footer = row()
        .gap(8.0)
        .child(button::<App>(t("取消", "Cancel")).key("variant-cancel").on_click(|app: &mut App| page_state(app).confirming = None))
        .child(accent_button::<App>(t("下载", "Download")).key("variant-download").disabled(choice.is_none()).on_click(move |app: &mut App| {
            let Some(variant) = choice else { return };
            let s = page_state(app);
            let what = s.confirming.take();
            s.error = None;
            if what.is_some() {
                start_download(app, variant, what, false);
            }
        }));
    modal("local-model-choose", t("下载本地模型？", "Download the local model?"), Some(help_with_docs(t("Qwen3.5-0.8B 在本机运行，不发送任何内容。", "Qwen3.5-0.8B runs on this computer and sends nothing out."), "settings", "local-model")), 520.0, col().child(machine).child(options), Some(footer), |app| page_state(app).confirming = None)
}

fn china_mirror_dialog() -> E {
    let footer = row()
        .gap(8.0)
        .child(button::<App>(t("直接下载", "Download directly")).key("mirror-direct").on_click(|app: &mut App| {
            if let Some((v, e, d)) = page_state(app).asking.take() {
                install(app, v, false, e, d);
            }
        }))
        .child(accent_button::<App>(t("使用国内镜像", "Use the mirror")).key("mirror-use").on_click(|app: &mut App| {
            if let Some((v, e, d)) = page_state(app).asking.take() {
                install(app, v, true, e, d);
            }
        }));
    modal("local-model-mirror", t("使用国内镜像下载？", "Download from the mirror in mainland China?"), Some(help(t("hf-mirror.com 文件相同，逐个校验。", "hf-mirror.com serves the same files; each one is checked."))), 480.0, div(), Some(footer), |app| page_state(app).asking = None)
}

fn variant_row(app: &mut App, variant: &VariantStatus, status: &LocalModelStatus, other_installing: bool) -> E {
    let t_ = theme();
    let id = variant.id;
    let in_use = status.active == Some(id) && variant.phase == Phase::Ready;
    let confirm_remove = page_state(app).confirm_remove == Some(id);
    let progress_value = match &variant.phase {
        Phase::Downloading { received, total, .. } => Some(percent_of(*received, *total)),
        Phase::Preparing { done, total, .. } if *total > 0 => Some(percent_of(*done, *total)),
        _ => None,
    };
    let mut copy = col()
        .flex1()
        .min_w(0.0)
        .gap(2.0)
        .child(
            row()
                .gap(6.0)
                .items_center()
                .child(text(variant_name(id)).ty(CAPTION_STRONG))
                .child(in_use.then(|| text(t("使用中", "In use")).ty(FINE).color(t_.success).nowrap()))
                .child((!in_use && status.recommended == Some(id) && variant.phase != Phase::Ready).then(|| text(t("推荐", "Recommended")).ty(FINE).color(t_.text_tertiary).nowrap())),
        )
        .child(help(variant_description(id)))
        .child(text(variant_summary(variant, status)).ty(FINE).color(t_.text_secondary));
    if in_use && status.loaded && !status.warming {
        copy = copy.child(help(tf("已加载 · 最多同时 {slots} 个请求 · 每个请求 {context} 个位置", "Loaded · up to {slots} requests at once · {context} positions each", &[("slots", &status.slots), ("context", &status.context)])));
    }
    let mut actions = row().gap(6.0).items_center().shrink0();
    match &variant.phase {
        Phase::Missing | Phase::Failed { .. } => {
            let failed = matches!(variant.phase, Phase::Failed { .. });
            actions = actions.child(small_accent_button(if failed { t("重试", "Retry") } else { t("下载", "Download") }).key(("variant-download", id.wire())).disabled(other_installing).on_click(move |app: &mut App| {
                page_state(app).dialog_error = None;
                start_download(app, id, None, true);
            }));
        }
        Phase::Ready if confirm_remove => {
            actions = actions.child(small_button(t("取消", "Cancel")).key(("variant-remove-cancel", id.wire())).on_click(|app: &mut App| page_state(app).confirm_remove = None)).child(small_danger_button(t("确定删除", "Remove")).key(("variant-remove-confirm", id.wire())).on_click(move |app: &mut App| {
                page_state(app).confirm_remove = None;
                run_verb(app, "local_model_remove", json!({ "variant": id.wire() }));
            }));
        }
        Phase::Ready => {
            if !in_use {
                actions = actions.child(small_accent_button(t("使用", "Use")).key(("variant-use", id.wire())).on_click(move |app: &mut App| run_verb(app, "local_model_activate", json!({ "variant": id.wire() }))));
            }
            actions = actions.child(small_button(t("删除", "Remove")).key(("variant-remove", id.wire())).on_click(move |app: &mut App| page_state(app).confirm_remove = Some(id)));
        }
        _ => {}
    }
    let progress_row = progress_value.map(|percent| {
        let label = match &variant.phase {
            Phase::Downloading { received, total, .. } => format!("{} / {}", format_bytes(*received), format_bytes(*total)),
            _ => format!("{percent}%"),
        };
        row()
            .gap(10.0)
            .items_center()
            .child(progress(Some(percent as f32)).aria_label(tf("{name}的安装进度", "{name} install progress", &[("name", &variant_name(id))])))
            .child(text(label).ty(MONO_SMALL).color(t_.text_secondary).nowrap().min_w(110.0).text_end())
            .child(matches!(variant.phase, Phase::Downloading { .. }).then(|| {
                icon_button::<App>("X", t("取消下载", "Cancel download")).key(("variant-cancel", id.wire())).size(26.0, 26.0).on_click(|app: &mut App| {
                    page_state(app).dialog_error = None;
                    app.invoke::<Value>("local_model_cancel_install", json!({}), |app, result| {
                        if let Err(e) = result {
                            page_state(app).dialog_error = Some(e);
                        }
                    });
                })
            }))
    });
    col().key(("variant-row", id.wire())).role(Role::Group).aria_label(variant_name(id)).gap(8.0).py(10.0).px(12.0).radius(8.0).border(if in_use { t_.accent.with_alpha(0.5) } else { t_.card_stroke }).child(row().gap(16.0).items_start().child(copy).child(actions)).child(progress_row)
}

fn write_prompt(app: &mut App, task: Task, text_value: String) {
    store::change(app, move |doc| {
        let p = &mut doc.global_settings.appearance.local_model;
        match task {
            Task::Title => p.title_prompt = text_value,
            Task::Shell => p.shell_prompt = text_value,
            Task::Error => p.error_prompt = text_value,
        }
    });
}

/// One system prompt. The line under it reports its KV cache for the build in use. Opening the dialog only reads a cache
/// already on disk; once the text is edited, what is in effect is cached when typing settles, loading the model if needed.
fn prompt_editor(app: &mut App, task: Task, title: &str, value: &str, fallback: Option<&str>, backend: Option<VariantId>) -> E {
    let shown = if value.trim().is_empty() { fallback.unwrap_or("").to_owned() } else { value.to_owned() };
    let signature = format!("{}\u{0}{}\u{0}{shown}", backend.map_or("", VariantId::wire), task.wire());
    let generation = page_state(app).generation;
    let state = page_state(app).prompts.entry(task.wire()).or_default();
    if state.signature.as_deref() != Some(signature.as_str()) {
        let usable = backend.is_some() && !shown.trim().is_empty();
        state.signature = Some(signature);
        // The effect's cleanup: whatever changed, a wait for the old text ends here.
        let previous = state.timer.take();
        if usable {
            state.token += 1;
            state.computing = true;
        }
        let (token, build) = (state.token, state.edited);
        if let Some(timer) = previous {
            app.clear_timer(timer);
        }
        if !usable {
            return prompt_editor_view(app, task, title, value, fallback, backend, shown);
        }
        let prompt = shown.clone();
        let timer = app.set_timer(PROMPT_SETTLE, move |app| {
            app.invoke::<PromptReport>("local_model_prompt_info", json!({ "task": task.wire(), "prompt": prompt, "build": build }), move |app, result| {
                let page = page_state(app);
                if page.generation != generation || !page.managing {
                    return;
                }
                let s = page.prompts.entry(task.wire()).or_default();
                if s.token != token {
                    return;
                }
                s.computing = false;
                match result {
                    Ok(next) => {
                        s.report = Some(next);
                        s.error = None;
                    }
                    Err(e) => {
                        s.report = None;
                        s.error = Some(e);
                    }
                }
            });
        });
        page_state(app).prompts.entry(task.wire()).or_default().timer = Some(timer);
    }
    prompt_editor_view(app, task, title, value, fallback, backend, shown)
}

fn prompt_editor_view(app: &mut App, task: Task, title: &str, value: &str, fallback: Option<&str>, backend: Option<VariantId>, shown: String) -> E {
    let t_ = theme();
    let state = page_state(app).prompts.entry(task.wire()).or_default();
    let info = if backend.is_none() {
        t("安装模型后显示这段提示词的 token 数与 KV 缓存大小。", "Install the model to see this prompt's token count and KV cache size.").to_owned()
    } else if state.computing {
        t("正在计算 KV 缓存…", "Computing the KV cache…").to_owned()
    } else {
        match &state.report {
            Some(r) => match r.cache_bytes {
                Some(bytes) => tf("相当于 {tokens} 个 token · KV 缓存 {size}（上限 {max} 个 token）", "{tokens} tokens · KV cache {size} (limit {max} tokens)", &[("tokens", &r.tokens), ("size", &format_bytes(bytes)), ("max", &r.max_tokens)]),
                None => tf("相当于 {tokens} 个 token（上限 {max} 个 token）· KV 缓存在模型下次用到这段提示词时生成", "{tokens} tokens (limit {max} tokens) · the KV cache is built the next time the model uses this prompt", &[("tokens", &r.tokens), ("max", &r.max_tokens)]),
            },
            None => String::new(),
        }
    };
    let error = state.error.clone();
    let can_reset = !value.trim().is_empty();
    let fallback_owned = fallback.map(str::to_owned);
    let area = field::<App>(("local-prompt", task.wire()), shown)
        .multiline(180.0, 360.0)
        .ty(MONO_BODY)
        .placeholder(if fallback.is_none() { t("正在读取默认提示词…", "Reading the default prompt…") } else { "" })
        .on_change(move |app, next| {
            page_state(app).prompts.entry(task.wire()).or_default().edited = true;
            let stored = if fallback_owned.as_deref() == Some(next.as_str()) { String::new() } else { next };
            write_prompt(app, task, stored);
        })
        .build()
        .aria_label(title);
    col()
        .gap(8.0)
        .py(12.0)
        .border_top(t_.divider)
        .role(Role::Region)
        .aria_label(title)
        .child(row().items_center().justify_between().gap(12.0).child(text(title.to_owned()).ty(CAPTION_STRONG)).child(small_subtle_button(t("恢复默认", "Reset to default")).key(("local-prompt-reset", task.wire())).disabled(!can_reset).on_click(move |app: &mut App| {
            page_state(app).prompts.entry(task.wire()).or_default().edited = true;
            write_prompt(app, task, String::new());
        })))
        .child(area)
        .child(if let Some(e) = error { Some(error_line(e)) } else { (!info.is_empty()).then(|| text(info).ty(FINE).color(t_.text_tertiary)) })
}

fn local_model_dialog(app: &mut App, status: Option<&LocalModelStatus>, preferences: &LocalModelPreferences) -> E {
    let t_ = theme();
    if !page_state(app).manage_started {
        page_state(app).manage_started = true;
        // Pushed statuses leave out the runtime (device, slots); ask for it once.
        refresh(app);
        let generation = page_state(app).generation;
        app.invoke::<DefaultPrompts>("local_model_default_prompts", json!({}), move |app, result| {
            let s = page_state(app);
            if s.generation != generation {
                return;
            }
            match result {
                Ok(prompts) => s.defaults = Some(prompts),
                Err(e) => s.dialog_error = Some(e),
            }
        });
    }
    let (defaults, error) = {
        let s = page_state(app);
        (s.defaults.clone(), s.dialog_error.clone())
    };
    let machine = status.and_then(|s| machine_summary(&s.machine));
    let installing = status.is_some_and(|s| s.variants.iter().any(|v| matches!(v.phase, Phase::Downloading { .. } | Phase::Preparing { .. })));
    let active = status.and_then(|s| s.variants.iter().find(|v| Some(v.id) == s.active && v.phase == Phase::Ready).map(|v| v.id));
    let mut models = col().gap(8.0).pb(12.0).role(Role::Region).aria_label(t("模型", "Models")).child(col().gap(3.0).child(text("Qwen3.5-0.8B").ty(BODY_SMALL_STRONG)).child(help(t("可下载多个版本，同一时间只用其中一个。", "Download any of the builds; only one is used at a time."))).child(machine.map(|m| text(tf("本机：{machine}", "This Mac: {machine}", &[("machine", &m)])).ty(FINE).color(t_.text_secondary))));
    match status {
        None => models = models.child(text(t("正在读取状态…", "Reading status…")).ty(FINE).color(t_.text_tertiary)),
        Some(s) => {
            for variant in &s.variants {
                let other = installing && !matches!(variant.phase, Phase::Downloading { .. } | Phase::Preparing { .. });
                models = models.child(variant_row(app, variant, s, other));
            }
        }
    }
    models = models.child(error.map(error_line));
    let body = col()
        .child(models)
        .child(prompt_editor(app, Task::Title, t("会话标题的前置提示词", "Prompt for conversation titles"), &preferences.title_prompt, defaults.as_ref().map(|d| d.title.as_str()), active))
        .child(prompt_editor(app, Task::Shell, t("命令说明的前置提示词", "Prompt for command explanations"), &preferences.shell_prompt, defaults.as_ref().map(|d| d.shell.as_str()), active))
        .child(prompt_editor(app, Task::Error, t("错误解释的前置提示词", "Prompt for error explanations"), &preferences.error_prompt, defaults.as_ref().map(|d| d.error.as_str()), active));
    let done = button::<App>(t("完成", "Done")).key("local-model-done").on_click(close_manage);
    modal("local-model", t("本地小模型", "Local model"), None, 760.0, body, Some(done), close_manage)
}

/// The dialog unmounting: a prompt's wait ends with it, so closing within the settle time loads no model, and opening it again starts over.
fn close_manage(app: &mut App) {
    let s = page_state(app);
    s.managing = false;
    s.manage_started = false;
    s.defaults = None;
    s.dialog_error = None;
    s.confirm_remove = None;
    let prompts = std::mem::take(&mut s.prompts);
    for timer in prompts.into_values().filter_map(|p| p.timer) {
        app.clear_timer(timer);
    }
}

/// The card on the Appearance page.
pub fn local_model_settings(app: &mut App) -> E {
    if !page_state(app).started {
        page_state(app).started = true;
        refresh(app);
    }
    let status = app.local::<Controller>().status.clone();
    let preferences = app.doc.global_settings.appearance.local_model.clone();
    let unsupported = status.as_ref().is_some_and(|s| s.recommended.is_none());
    let present = status.as_ref().is_some_and(|s| s.variants.iter().any(|v| matches!(v.phase, Phase::Ready | Phase::Downloading { .. } | Phase::Preparing { .. })));
    let use_switch = |key: &'static str, label: &'static str, what: Use, on: bool| {
        labeled_switch(("local-use", key), on, unsupported, move |app| {
            // Turning a use on needs a model: pick a build and agree to the download.
            if !on && !present {
                let s = page_state(app);
                s.confirming = Some(what);
                s.choice = None;
                return;
            }
            set_use(app, what, !on);
        })
        .aria_label(label)
    };
    let s = page_state(app);
    let (managing, confirming, error, unreadable, asking) = (s.managing, s.confirming, s.error.clone(), s.unreadable, s.asking.is_some());
    let summary = if status.is_none() && unreadable { t("无法读取模型状态", "Couldn't read the model's status").to_owned() } else { status_summary(status.as_ref()) };
    let subagents = preferences.subagents;
    let mut body = card(Some(t("本地小模型", "Local model")))
        .child(first_row(setting_row(t("自动生成会话标题", "Name conversations automatically"), Some(help_with_docs(t("用本地模型为新会话起标题。", "Names new conversations with the local model."), "settings", "local-model")), use_switch("titles", t("自动生成会话标题", "Name conversations automatically"), Use::Titles, preferences.titles), false)))
        .child(setting_row(t("解释 Shell 命令", "Explain shell commands"), Some(help(t("用一句话说明 shell 命令，作为卡片标题。", "Explains each shell command in one line, as the card title."))), use_switch("shell", t("解释 Shell 命令", "Explain shell commands"), Use::ShellExplanations, preferences.shell_explanations), false))
        .child(setting_row(t("解释错误", "Explain errors"), Some(help_with_docs(t("用几个词说明失败原因，作为卡片标题。", "Says why a call failed in a few words, as the card title."), "settings", "local-model")), use_switch("errors", t("解释错误", "Explain errors"), Use::ErrorExplanations, preferences.error_explanations), false))
        // Changes where the uses above reach, not whether any runs: nothing to download for it.
        .child(setting_row(t("也用于子代理", "Also for subagents"), Some(help_with_docs(t("上面几项也用在子代理和工作流上。", "Extends the uses above to subagents and workflows."), "settings", "local-model")), labeled_switch(("local-use", "subagents"), subagents, unsupported, move |app| store::change(app, move |doc| doc.global_settings.appearance.local_model.subagents = !subagents)).aria_label(t("也用于子代理", "Also for subagents")), false))
        .child(setting_row(t("模型与提示词", "Model and prompts"), Some(help(summary)), small_button(t("管理…", "Manage…")).key("local-manage").on_click(|app: &mut App| page_state(app).managing = true), false))
        .child(error.map(error_line));
    if let (Some(_), Some(st)) = (confirming, status.as_ref()) {
        body = body.child(choose_variant_dialog(app, st));
    }
    if managing {
        body = body.child(local_model_dialog(app, status.as_ref(), &preferences));
    }
    if asking {
        body = body.child(china_mirror_dialog());
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variant(id: VariantId, phase: Phase) -> VariantStatus {
        VariantStatus { id, phase, download_bytes: 1024, disk_bytes: 2048 }
    }

    fn status(variants: Vec<VariantStatus>, active: Option<VariantId>, recommended: Option<VariantId>) -> LocalModelStatus {
        LocalModelStatus { machine: Machine::default(), variants, active, recommended, warming: false, loading: false, device: None, loaded: false, running: 0, queued: 0, slots: 0, context: 0, disk_bytes: 0, last_error: None }
    }

    #[test]
    fn the_summary_says_what_matters_most_first() {
        let _language = crate::language_for_test(true);
        assert_eq!(status_summary(None), "Reading status…");
        let busy = status(vec![variant(VariantId::Mlx, Phase::Ready), variant(VariantId::Ane, Phase::Downloading { received: 1, total: 2, source: DownloadSource::Release })], Some(VariantId::Mlx), Some(VariantId::Ane));
        assert!(status_summary(Some(&busy)).starts_with("Neural Engine build (Core ML) · Downloading 50%"), "{}", status_summary(Some(&busy)));
        let active = status(vec![variant(VariantId::Mlx, Phase::Ready)], Some(VariantId::Mlx), Some(VariantId::Mlx));
        assert!(status_summary(Some(&active)).contains("In use"));
        let none = status(vec![variant(VariantId::Mlx, Phase::Missing)], None, Some(VariantId::Mlx));
        assert_eq!(status_summary(Some(&none)), "No model downloaded");
        let unsupported = status(vec![variant(VariantId::Ane, Phase::Unsupported { reason: Unavailable::NeedsMacos14 })], None, None);
        assert_eq!(status_summary(Some(&unsupported)), "This machine can't run the local model: Needs macOS 14 or later");
    }

    // A step the host adds later reads as what upstream's fallback says, rather than failing to parse the whole status.
    #[test]
    fn a_step_this_build_has_no_words_for_is_still_preparing() {
        let _language = crate::language_for_test(true);
        let phase: Phase = serde_json::from_value(json!({ "phase": "preparing", "step": "somethingNew", "done": 0, "total": 0 })).unwrap();
        let v = variant(VariantId::Mlx, phase);
        assert_eq!(variant_summary(&v, &status(vec![], None, None)), "Preparing the model…");
    }

    #[test]
    fn the_machine_line_reads_off_a_mac_as_nothing() {
        let _language = crate::language_for_test(true);
        assert_eq!(machine_summary(&Machine::default()), None);
        let m = Machine { chip: Some("Apple M4".into()), model: Some("Mac16,7".into()), os_version: Some("15.4".into()), apple_silicon: true, neural_engine_cores: Some(16) };
        assert_eq!(machine_summary(&m).unwrap(), "Apple M4 (Mac16,7) · 16-core Neural Engine · macOS 15.4");
    }
}
