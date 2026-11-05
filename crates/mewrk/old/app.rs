//! The application: documents, conversations in memory, and one frame's worth of coordination.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

use fluent::theme::*;
use fluent::ui::{rect, Key, Scroll, Ui};
use fluent::TextEdit;
use mewrk_core::*;

use crate::docks::QuestionState;
use crate::layout::{PaneId, PaneLayout};
use crate::live::{apply_event, Child, LiveConv, LoadState};
use crate::settings::SettingsPage;

pub const TOPBAR_H: f32 = 44.0;
const TEMP_PROJECT: &str = "temporary";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Chat,
    Settings,
}

/// Things that block or need the window, done between frames by whoever owns it.
#[derive(Default)]
pub struct WindowRequests {
    pub drag: bool,
    pub zoom: bool,
    pub pick_project: bool,
    pub pick_images: bool,
}

pub enum Confirm {
    RemoveProject(String),
}

/// An item being edited in its dialog.
pub struct ItemEditor {
    pub conv: String,
    pub item: ItemId,
    pub edit: TextEdit,
}

pub struct Search {
    pub edit: TextEdit,
    pub current: usize,
}

pub struct App {
    pub store: Store,
    pub doc: Document,
    pub engine: Arc<Engine>,
    pub notifier: Arc<Notifier>,
    pub wake: Arc<dyn Fn() + Send + Sync>,
    pub system_dark: bool,
    pub fullscreen: bool,
    pub live: HashMap<String, LiveConv>,
    pub active: Option<String>,
    /// A new task that has not sent anything yet. It only joins the document when it does.
    pub draft: Option<ConvMeta>,
    pub layouts: HashMap<String, PaneLayout>,
    pub page: Page,
    pub history: Vec<String>,
    pub history_pos: usize,
    pub sidebar_scroll: Scroll,
    pub sidebar_resizing: bool,
    pub rename: Option<(String, TextEdit)>,
    pub title_edit: Option<(String, TextEdit)>,
    pub armed_delete: Option<String>,
    pub search: Option<Search>,
    pub confirm: Option<Confirm>,
    pub item_editor: Option<ItemEditor>,
    pub settings: SettingsPage,
    pub requests: WindowRequests,
    pub question_state: QuestionState,
    /// The window's content view, for native child views. Null when headless.
    pub native_view: *mut std::ffi::c_void,
    pub frame_no: u64,
    doc_dirty: bool,
    loaded_tx: Sender<(String, std::io::Result<Vec<Item>>)>,
    loaded_rx: Receiver<(String, std::io::Result<Vec<Item>>)>,
    events: Vec<RunEvent>,
    pub keys_cache: HashMap<String, Option<String>>,
}

impl App {
    pub fn new(store: Store, engine: Arc<Engine>, notifier: Arc<Notifier>, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let mut doc = store.load_document();
        if !doc.projects.iter().any(|p| p.path.is_none()) {
            doc.projects.push(Project { id: TEMP_PROJECT.to_owned(), name: "临时项目".to_owned(), path: None, collapsed: false });
        }
        let (loaded_tx, loaded_rx) = mpsc::channel();
        let mut app = Self {
            store,
            doc,
            engine,
            notifier,
            wake,
            system_dark: true,
            fullscreen: false,
            live: HashMap::new(),
            active: None,
            draft: None,
            layouts: HashMap::new(),
            page: Page::Chat,
            history: Vec::new(),
            history_pos: 0,
            sidebar_scroll: Scroll::from_top(),
            sidebar_resizing: false,
            rename: None,
            title_edit: None,
            armed_delete: None,
            search: None,
            confirm: None,
            item_editor: None,
            settings: SettingsPage::new(),
            requests: WindowRequests::default(),
            question_state: QuestionState::default(),
            native_view: std::ptr::null_mut(),
            frame_no: 0,
            doc_dirty: false,
            loaded_tx,
            loaded_rx,
            events: Vec::new(),
            keys_cache: HashMap::new(),
        };
        // Start where the user left off: the conversation touched last, or a new task.
        let last = app.doc.conversations.iter().max_by_key(|c| c.updated_at).map(|c| c.id.clone());
        match last {
            Some(id) => app.select_conversation(&id),
            None => app.new_task(None),
        }
        app
    }

    pub fn theme(&self) -> Theme {
        let dark = match self.doc.settings.theme {
            ThemeChoice::System => self.system_dark,
            ThemeChoice::Light => false,
            ThemeChoice::Dark => true,
        };
        if dark { Theme::dark() } else { Theme::light() }
    }

    pub fn mark_dirty(&mut self) {
        self.doc_dirty = true;
    }

    pub fn meta(&self, id: &str) -> Option<&ConvMeta> {
        self.draft.as_ref().filter(|d| d.id == id).or_else(|| self.doc.conversation(id))
    }

    pub fn meta_mut(&mut self, id: &str) -> Option<&mut ConvMeta> {
        if self.draft.as_ref().is_some_and(|d| d.id == id) {
            return self.draft.as_mut();
        }
        self.doc.conversation_mut(id)
    }

    pub fn is_draft(&self, id: &str) -> bool {
        self.draft.as_ref().is_some_and(|d| d.id == id)
    }

    /// The folders a conversation works in, the first being its working directory.
    pub fn workspaces_of(&self, meta: &ConvMeta) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Some(path) = self.doc.project(&meta.project_id).and_then(|p| p.path.clone()) {
            out.push(path);
        }
        out.extend(meta.settings.extra_workspaces.iter().cloned());
        if out.is_empty() {
            out.push(self.store.dir().join("temporary").join(&meta.id));
        }
        out
    }

    // ------------------------------------------------------------ conversations

    fn settings_for_new_task(&self) -> ConvSettings {
        let s = &self.doc.settings;
        let provider = if self.doc.provider(&s.default_provider).is_some() { s.default_provider.clone() } else { s.providers.first().map_or_else(String::new, |p| p.id.clone()) };
        let model = self.doc.provider(&provider).and_then(|p| p.models.iter().find(|m| m.id == s.default_model).or(p.models.first())).map_or_else(String::new, |m| m.id.clone());
        ConvSettings { provider_id: provider, model_id: model, effort: s.default_effort.clone(), permission: s.default_permission, ..Default::default() }
    }

    pub fn new_task(&mut self, project: Option<String>) {
        let project_id = project
            .or_else(|| self.active.as_deref().and_then(|a| self.meta(a)).map(|m| m.project_id.clone()))
            .or_else(|| self.doc.projects.first().map(|p| p.id.clone()))
            .unwrap_or_else(|| TEMP_PROJECT.to_owned());
        let now = now_ms();
        let meta = ConvMeta { id: new_id(), project_id, title: String::new(), created_at: now, updated_at: now, settings: self.settings_for_new_task(), usage: Usage::default(), context_tokens: 0 };
        let id = meta.id.clone();
        if let Some(old) = self.draft.take() {
            self.live.remove(&old.id);
            self.layouts.remove(&old.id);
        }
        self.draft = Some(meta);
        let mut live = LiveConv::new();
        live.load = LoadState::Loaded;
        self.live.insert(id.clone(), live);
        self.push_history(&id);
        self.active = Some(id);
        self.page = Page::Chat;
    }

    fn push_history(&mut self, id: &str) {
        if self.history.get(self.history_pos.wrapping_sub(1)).is_some_and(|h| h == id) {
            return;
        }
        self.history.truncate(self.history_pos);
        self.history.push(id.to_owned());
        self.history_pos = self.history.len();
    }

    pub fn can_go_back(&self) -> bool {
        self.history_pos > 1
    }

    pub fn can_go_forward(&self) -> bool {
        self.history_pos < self.history.len()
    }

    pub fn go_back(&mut self) {
        if self.can_go_back() {
            self.history_pos -= 1;
            let id = self.history[self.history_pos - 1].clone();
            self.show_conversation(&id);
        }
    }

    pub fn go_forward(&mut self) {
        if self.can_go_forward() {
            self.history_pos += 1;
            let id = self.history[self.history_pos - 1].clone();
            self.show_conversation(&id);
        }
    }

    pub fn select_conversation(&mut self, id: &str) {
        if self.active.as_deref() != Some(id) {
            self.push_history(id);
        }
        self.show_conversation(id);
    }

    fn show_conversation(&mut self, id: &str) {
        // A draft nobody typed anything into simply goes away.
        if self.draft.as_ref().is_some_and(|d| d.id != id) {
            let empty = self.live.get(&self.draft.as_ref().unwrap().id).is_none_or(|l| l.composer.edit.text.is_empty());
            if empty {
                let old = self.draft.take().unwrap();
                self.live.remove(&old.id);
                self.layouts.remove(&old.id);
            }
        }
        if self.meta(id).is_none() {
            return;
        }
        self.active = Some(id.to_owned());
        self.page = Page::Chat;
        let live = self.live.entry(id.to_owned()).or_insert_with(LiveConv::new);
        live.unseen_done = false;
        if live.load == LoadState::NotLoaded {
            live.load = LoadState::Loading;
            // Off the UI thread: a long conversation should not cost a frame to open.
            let (tx, store, key, wake) = (self.loaded_tx.clone(), self.store.clone(), id.to_owned(), self.wake.clone());
            std::thread::spawn(move || {
                let items = store.load_items(&key);
                let _ = tx.send((key, items));
                wake();
            });
        }
    }

    pub fn delete_conversation(&mut self, id: &str) {
        if let Some(l) = self.live.get(id)
            && let Some(h) = &l.handle
        {
            h.stop();
            l.session.stop_all();
        }
        self.live.remove(id);
        self.layouts.remove(id);
        self.doc.conversations.retain(|c| c.id != id);
        self.store.delete_conversation(id);
        self.history.retain(|h| h != id);
        self.history_pos = self.history.len();
        self.mark_dirty();
        if self.active.as_deref() == Some(id) {
            self.active = None;
            let next = self.doc.conversations.iter().max_by_key(|c| c.updated_at).map(|c| c.id.clone());
            match next {
                Some(n) => self.select_conversation(&n),
                None => self.new_task(None),
            }
        }
    }

    pub fn remove_project(&mut self, id: &str) {
        let convs: Vec<String> = self.doc.conversations.iter().filter(|c| c.project_id == id).map(|c| c.id.clone()).collect();
        for c in convs {
            self.delete_conversation(&c);
        }
        self.doc.projects.retain(|p| p.id != id);
        self.mark_dirty();
    }

    pub fn add_project(&mut self, path: PathBuf) {
        if let Some(existing) = self.doc.projects.iter().find(|p| p.path.as_ref() == Some(&path)) {
            let id = existing.id.clone();
            self.new_task(Some(id));
            return;
        }
        let name = path.file_name().map_or_else(|| path.display().to_string(), |s| s.to_string_lossy().into_owned());
        let id = new_id();
        // Above the temporary project, which stays last.
        let at = self.doc.projects.iter().position(|p| p.path.is_none()).unwrap_or(self.doc.projects.len());
        self.doc.projects.insert(at, Project { id: id.clone(), name, path: Some(path), collapsed: false });
        self.mark_dirty();
        self.new_task(Some(id));
    }

    pub fn api_key(&mut self, provider: &str) -> Option<String> {
        if let Some(k) = self.keys_cache.get(provider) {
            return k.clone();
        }
        let k = secrets::get(provider);
        self.keys_cache.insert(provider.to_owned(), k.clone());
        k
    }

    /// Sends what is in the active composer, or queues it behind the run in progress.
    pub fn send(&mut self) {
        let Some(id) = self.active.clone() else { return };
        let Some(live) = self.live.get_mut(&id) else { return };
        let text = live.composer.edit.text.trim().to_owned();
        let images = std::mem::take(&mut live.composer.images);
        if text.is_empty() && images.is_empty() {
            return;
        }
        live.composer.edit.clear();
        live.composer.want_focus = true;
        if live.is_running() {
            live.queue.push(text);
            return;
        }
        self.start_run(&id, text, images);
    }

    pub fn begin_edit(&mut self, conv: &str, item: ItemId) {
        let Some(l) = self.live.get(conv) else { return };
        let text = l.items.iter().find(|i| i.id() == item).and_then(|i| match i {
            Item::User { text, .. } | Item::Text { text, .. } => Some(text.clone()),
            _ => None,
        });
        if let Some(text) = text {
            let mut edit = TextEdit::new();
            edit.set_text(text);
            self.item_editor = Some(ItemEditor { conv: conv.to_owned(), item, edit });
        }
    }

    pub fn commit_edit(&mut self) {
        let Some(editor) = self.item_editor.take() else { return };
        let Some(l) = self.live.get_mut(&editor.conv) else { return };
        if l.is_running() {
            return;
        }
        for i in &mut l.items {
            if i.id() == editor.item {
                match i {
                    Item::User { text, .. } => text.clone_from(&editor.edit.text),
                    Item::Text { text, tokens, .. } => {
                        text.clone_from(&editor.edit.text);
                        *tokens = estimate_tokens(text);
                    }
                    _ => {}
                }
            }
        }
        l.view.item_edited(editor.item);
        self.store.rewrite(&editor.conv, l.items.clone());
    }

    pub fn delete_item(&mut self, conv: &str, item: ItemId) {
        let Some(l) = self.live.get_mut(conv) else { return };
        if l.is_running() {
            return;
        }
        l.items.retain(|i| i.id() != item);
        l.view.item_edited(item);
        self.store.rewrite(conv, l.items.clone());
    }

    /// Drops everything after this user message and asks the model again.
    pub fn regenerate(&mut self, conv: &str, item: ItemId) {
        let Some(l) = self.live.get_mut(conv) else { return };
        if l.is_running() {
            return;
        }
        let Some(at) = l.items.iter().position(|i| i.id() == item) else { return };
        l.items.truncate(at + 1);
        l.view.rolled_back(item + 1);
        l.view.structure_changed();
        self.store.rewrite(conv, self.live[conv].items.clone());
        self.launch(conv);
    }

    /// Asks again after a failure. The notice about the failure goes away first.
    pub fn retry(&mut self, conv: &str) {
        let Some(l) = self.live.get_mut(conv) else { return };
        if l.is_running() {
            return;
        }
        if matches!(l.items.last(), Some(Item::Notice { .. })) {
            l.items.pop();
            l.view.structure_changed();
            self.store.rewrite(conv, self.live[conv].items.clone());
        }
        self.launch(conv);
    }

    pub fn start_run(&mut self, id: &str, text: String, images: Vec<Attachment>) {
        let Some(meta) = self.meta(id).cloned() else { return };
        let Some(provider) = self.doc.provider(&meta.settings.provider_id).cloned() else {
            if let Some(live) = self.live.get_mut(id) {
                live.composer.edit.set_text(text);
            }
            self.page = Page::Settings;
            self.settings.message = Some("先添加一个模型供应商，才能开始对话。".to_owned());
            return;
        };
        if provider.models.is_empty() {
            if let Some(live) = self.live.get_mut(id) {
                live.composer.edit.set_text(text);
            }
            self.page = Page::Settings;
            self.settings.message = Some(format!("{} 还没有配置模型。", provider.name));
            return;
        }
        // The draft becomes a real conversation the moment something is said in it.
        if self.is_draft(id) {
            let mut promoted = self.draft.take().unwrap();
            promoted.title = title_from(&text);
            self.doc.conversations.push(promoted);
        }
        let live = self.live.get_mut(id).unwrap();
        let next_id = live.items.iter().map(Item::id).max().unwrap_or(0) + 1;
        let item = Item::User { id: next_id, text, images, at: now_ms() };
        live.items.push(item.clone());
        self.store.append(id, vec![item]);
        self.launch(id);
    }

    /// Sends the conversation as it stands to the model.
    fn launch(&mut self, id: &str) {
        let Some(meta) = self.meta(id).cloned() else { return };
        let Some(provider) = self.doc.provider(&meta.settings.provider_id).cloned() else { return };
        let Some(model) = provider.models.iter().find(|m| m.id == meta.settings.model_id).or(provider.models.first()).cloned() else { return };
        let key = self.api_key(&provider.id);
        let workspaces = self.workspaces_of(&meta);
        if let Some(first) = workspaces.first() {
            let _ = std::fs::create_dir_all(first);
        }
        let global_instructions = self.doc.settings.global_instructions.clone();
        let Some(live) = self.live.get_mut(id) else { return };
        live.view.structure_changed();
        live.view.scroll.scroll_to_end();
        live.error = None;
        live.status = Some(RunStatus::Connecting);
        let session = live.session.clone();
        session.plan_mode.store(meta.settings.plan_mode, std::sync::atomic::Ordering::Relaxed);
        let spec = RunSpec { conv_id: id.to_owned(), store_key: id.to_owned(), items: live.items.clone(), settings: meta.settings.clone(), provider, model, api_key: key, workspaces, global_instructions, session, child: None };
        let handle = self.engine.start(spec, meta.title.clone());
        self.live.get_mut(id).unwrap().handle = Some(handle);
        if let Some(m) = self.meta_mut(id) {
            m.updated_at = now_ms();
        }
        self.mark_dirty();
    }

    // ------------------------------------------------------------ per frame

    fn drain_runs(&mut self) {
        while let Ok((id, result)) = self.loaded_rx.try_recv() {
            if let Some(l) = self.live.get_mut(&id) {
                l.load = LoadState::Loaded;
                match result {
                    Ok(items) => {
                        l.context_tokens = self.doc.conversation(&id).map_or(0, |m| m.context_tokens);
                        l.items = items;
                        l.view.structure_changed();
                    }
                    Err(e) => l.error = Some(format!("对话内容载入失败：{e}")),
                }
            }
        }

        let active = self.active.clone();
        let ids: Vec<String> = self.live.iter().filter(|(_, l)| l.handle.is_some() || !l.children.is_empty()).map(|(k, _)| k.clone()).collect();
        let mut events = std::mem::take(&mut self.events);
        for id in ids {
            let mut finished: Option<RunEnd> = None;
            let mut usage: Option<Usage> = None;
            let mut file_changed = false;
            {
                let Some(live) = self.live.get_mut(&id) else { continue };
                if let Some(handle) = live.handle.clone() {
                    events.clear();
                    handle.inbox.drain(&mut events);
                    for ev in &events {
                        apply_event(&mut live.items, &mut live.view, ev);
                        match ev {
                            RunEvent::Status(s) => live.status = Some(s.clone()),
                            RunEvent::Usage(u) => {
                                let total = usage.get_or_insert_with(Usage::default);
                                total.add(u);
                                live.context_tokens = u.context();
                            }
                            RunEvent::Approval(a) => {
                                if active.as_deref() != Some(id.as_str()) {
                                    live.unseen_done = true;
                                }
                                live.approvals.push(a.clone());
                            }
                            RunEvent::Question(q) => live.questions.push(q.clone()),
                            RunEvent::Child(h) => live.children.push(Child { handle: h.clone(), items: Vec::new(), view: crate::timeline::Timeline::new(), status: None, end: None }),
                            RunEvent::Tool { status: ToolStatus::Done, meta: Some(m), .. } if m.file.is_some() => file_changed = true,
                            RunEvent::Tool { status: ToolStatus::Done | ToolStatus::Error, .. } => file_changed = true,
                            RunEvent::Finished(end) => finished = Some(end.clone()),
                            _ => {}
                        }
                    }
                }
                for child in &mut live.children {
                    events.clear();
                    child.handle.inbox.drain(&mut events);
                    for ev in &events {
                        apply_event(&mut child.items, &mut child.view, ev);
                        match ev {
                            RunEvent::Status(s) => child.status = Some(s.clone()),
                            RunEvent::Finished(end) => {
                                child.end = Some(end.clone());
                                child.status = None;
                            }
                            // A subagent's own approvals surface on its parent's composer.
                            RunEvent::Approval(a) => {
                                let handle = child.handle.clone();
                                let _ = &handle;
                                live.approvals.push(a.clone());
                            }
                            _ => {}
                        }
                    }
                }
            }
            if let Some(u) = usage
                && let Some(meta) = self.meta_mut(&id)
            {
                meta.usage.add(&u);
                meta.context_tokens = u.context();
                meta.updated_at = now_ms();
                self.doc_dirty = true;
            }
            if file_changed && let Some(l) = self.live.get_mut(&id) {
                l.files_touched = true;
            }
            if let Some(end) = finished {
                self.on_run_finished(&id, end);
            }
        }
        self.events = events;

        // A background task that finished while nothing was running still has something to say.
        let waking: Vec<String> = self.live.iter().filter(|(_, l)| l.handle.is_none() && l.load == LoadState::Loaded && l.session.has_notes()).map(|(k, _)| k.clone()).collect();
        for id in waking {
            self.launch(&id);
        }
    }

    fn on_run_finished(&mut self, id: &str, end: RunEnd) {
        let active = self.active.as_deref() == Some(id);
        let store = self.store.clone();
        let plan_now = {
            let Some(live) = self.live.get_mut(id) else { return };
            live.handle = None;
            live.status = None;
            live.approvals.clear();
            live.questions.clear();
            live.unseen_done = !active;
            if let RunEnd::Failed(message) = end {
                let next = live.items.iter().map(Item::id).max().unwrap_or(0) + 1;
                let item = Item::Notice { id: next, level: NoticeLevel::Error, text: message };
                live.items.push(item.clone());
                live.view.structure_changed();
                store.append(id, vec![item]);
            }
            live.view.structure_changed();
            if let Some(r) = &mut live.review {
                r.refresh();
            }
            live.session.plan_mode.load(std::sync::atomic::Ordering::Relaxed)
        };
        if let Some(meta) = self.meta_mut(id) {
            meta.settings.plan_mode = plan_now;
        }
        let queued = self.live.get_mut(id).and_then(|l| if l.queue.is_empty() { None } else { Some(l.queue.remove(0)) });
        if let Some(text) = queued {
            self.start_run(id, text, Vec::new());
        }
    }

    fn shortcuts(&mut self, ui: &mut Ui<'_>) {
        for press in &ui.input.keys {
            match press.key {
                Key::Char('n') if press.cmd => self.new_task(None),
                Key::Char('k') if press.cmd && !crate::chat::TERM_IDS.contains(&ui.mem.focus) => {
                    if self.search.is_none() {
                        let mut edit = TextEdit::new();
                        edit.clear();
                        self.search = Some(Search { edit, current: 0 });
                    } else {
                        self.search = None;
                    }
                }
                Key::Char(',') if press.cmd => self.page = if self.page == Page::Settings { Page::Chat } else { Page::Settings },
                Key::Char('b') if press.cmd => {
                    self.doc.settings.sidebar_open = !self.doc.settings.sidebar_open;
                    self.doc_dirty = true;
                }
                Key::Char('j') if press.cmd => {
                    if let Some(id) = self.active.clone() {
                        self.layouts.entry(id).or_default().toggle(PaneId::Terminal);
                    }
                }
                Key::Escape => {
                    if self.search.is_some() {
                        self.search = None;
                    } else if self.item_editor.is_some() {
                        self.item_editor = None;
                    } else if self.confirm.is_some() {
                        self.confirm = None;
                    } else if self.page == Page::Settings {
                        self.page = Page::Chat;
                    }
                }
                _ => {}
            }
        }
    }

    pub fn show(&mut self, ui: &mut Ui<'_>) {
        self.notifier.frame_started();
        self.drain_runs();
        self.shortcuts(ui);

        let (w, h) = ui.size;
        let sidebar_w = if self.doc.settings.sidebar_open { self.doc.settings.sidebar_width.clamp(200.0, (w * 0.5).max(200.0)) } else { 0.0 };
        crate::shell::topbar(self, ui, sidebar_w, w);
        if self.doc.settings.sidebar_open {
            crate::shell::sidebar(self, ui, rect(0.0, 0.0, sidebar_w, h));
        }
        crate::shell::nav(self, ui, sidebar_w);

        let field_left = sidebar_w + 8.0;
        let field = rect(field_left, TOPBAR_H + 8.0, (w - field_left).max(1.0), (h - TOPBAR_H - 8.0).max(1.0));
        match self.page {
            Page::Chat => crate::chat::field(self, ui, field),
            Page::Settings => crate::settings::show(self, ui, field),
        }
        crate::shell::overlays(self, ui);

        // A preview nobody drew this frame belongs to a pane that is closed or to a conversation out of sight.
        self.frame_no += 1;
        let drawn = self.frame_no - 1;
        for l in self.live.values_mut() {
            if let Some(p) = &mut l.preview
                && p.shown_frame != drawn
            {
                p.set_hidden(true);
            }
        }

        // A press that landed on nothing in the window's top row moves the window.
        if ui.press_unclaimed()
            && let Some((_, py)) = ui.input.pointer
            && py < TOPBAR_H
        {
            if ui.input.double_click {
                self.requests.zoom = true;
            } else {
                self.requests.drag = true;
            }
        }
        if self.doc_dirty {
            self.doc_dirty = false;
            self.store.save_document(&self.doc);
        }
    }

    pub fn active_layout(&mut self) -> Option<&mut PaneLayout> {
        let id = self.active.clone()?;
        Some(self.layouts.entry(id).or_default())
    }

    pub fn quit_cleanly(&self) {
        self.store.save_document(&self.doc);
        self.store.flush();
    }
}

fn title_from(text: &str) -> String {
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("新任务").trim();
    let mut out: String = first.chars().take(40).collect();
    if first.chars().count() > 40 {
        out.push('…');
    }
    out
}
