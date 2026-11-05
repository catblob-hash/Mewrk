//! New-task drafts (`lib/draftConversation.ts`, and `openDraftConversation`, `materializeDraft`, `redeemDraft`,
//! `forgetDraft` of `App.tsx`).
//!
//! Every project has a draft of its own, its next new task. A draft id is never persisted or sent to the host; it's the
//! prefix plus the project's id. A draft owns its settings from the moment it is opened, the way any conversation does,
//! so settings the user chose on it are kept with its project in the document: a restart brings them back rather than
//! rebuilding the draft from the preset it was copied from. A draft that only follows its project keeps nothing, and a
//! restart rebuilds it from the project as it is then.

use std::collections::{BTreeMap, HashSet};

use serde_json::json;

use crate::conversation_bodies::conversation_has_no_contexts;
use crate::doc::{create_id, now_iso, NewConversation, SurfaceCloser};
use crate::model::{AttachedWorkspace, ContextItem, Conversation, ConversationPreset, ConversationSettings, DraftConversationSnapshot, RunTarget, Workspace, WorkspaceKind};
use crate::presets::{apply_conversation_preset_settings, capture_conversation_preset_settings, clone_conversation_settings, conversation_preset_by_id, default_conversation_preset, same_conversation_preset_settings};
use crate::side_panes::SidePanesAction;
use crate::workspaces::{registered_project_workspaces, with_conversation_worktree, NewConversationSource, TEMPORARY_WORKSPACE_ID};
use crate::App;

/// The prefix of the renderer-owned draft conversation ids.
pub const DRAFT_CONVERSATION_ID: &str = "__draft__";

const DRAFT_ID_PREFIX: &str = "__draft__:";

/// The id of `workspace_id`'s draft, which everything the renderer keys by conversation uses.
pub fn draft_conversation_id(workspace_id: &str) -> String {
    format!("{DRAFT_ID_PREFIX}{workspace_id}")
}

/// The project whose draft `conversation_id` names, or `None` when it names no draft.
pub fn draft_workspace_id_of(conversation_id: &str) -> Option<&str> {
    conversation_id.strip_prefix(DRAFT_ID_PREFIX).filter(|rest| !rest.is_empty())
}

/// Whether an id names a draft.
pub fn is_draft_conversation_id(conversation_id: &str) -> bool {
    draft_workspace_id_of(conversation_id).is_some()
}

/// Renderer-owned draft state, one per project.
#[derive(Clone, Debug, PartialEq)]
pub struct DraftConversationState {
    /// The project the draft is the next new task of, and materializes into.
    pub workspace_id: String,
    /// The conversation id the draft will materialize as, minted up front. What the draft opens at the host before then
    /// (its terminals and its preview page) is owned by this id from the start, so becoming a real conversation hands
    /// them over without moving anything.
    pub materializes_as: String,
    pub settings: ConversationSettings,
    pub created_at: String,
    /// The project workspaces (1-based) the user ticked the worktree box for. Intent only: worktrees belong to persisted
    /// conversation ids, created after the conversation materializes and before the first message.
    pub worktree_members: Vec<u32>,
    /// The draft's run location, persisted directly in `Conversation::run_target` when materialized.
    pub run_target: Option<RunTarget>,
    /// Workspaces the draft may work in besides its primary one. Unlike `worktree_members` this is not just intent: the
    /// host authorized each one when its picker returned it, so the list is already the real grant.
    pub attached_workspaces: Vec<AttachedWorkspace>,
    /// Preset the draft's settings came from; empty means an unnamed draft.
    pub preset_id: String,
    /// Conversation template whose message queue this draft is showing; empty means none.
    pub template_id: String,
    /// Content the user wrote by hand before sending anything. It travels into the conversation at materialization.
    pub contexts: Vec<ContextItem>,
    /// The opening messages its preset's template laid down. While `contexts` still equals them nobody has touched them,
    /// so a change of the project's default preset may lay its own in their place. (Upstream compares by identity; a
    /// hand edit put back to the same text counts as untouched here, the harmless direction.)
    pub template_contexts: Option<Vec<ContextItem>>,
    /// The settings its project resolved it to when it was opened, or when the project's default preset last changed
    /// under it. Absent for a draft brought back from the document after a restart: only settings the user chose are kept
    /// there, so those are the user's.
    pub resolved_settings: Option<ConversationSettings>,
}

/// Whether the draft still holds what its project resolved it to, nobody having changed a preset-owned setting since, so
/// a change of the project's default preset carries it along. Lists compare as sets, as the preset trace does.
pub fn draft_follows_project(draft: &DraftConversationState) -> bool {
    draft.resolved_settings.as_ref().is_some_and(|resolved| same_conversation_preset_settings(&capture_conversation_preset_settings(&draft.settings), &capture_conversation_preset_settings(resolved)))
}

/// Whether the draft's settings are worth keeping across a restart: the user chose some, or switched plan mode on. One
/// that only follows its project is rebuilt from the project instead, so it follows a default changed meanwhile.
pub fn draft_holds_own_settings(draft: &DraftConversationState) -> bool {
    !draft_follows_project(draft) || draft.settings.plan_mode_enabled
}

/// Whether a persisted conversation holds nothing yet. New tasks start as the draft, so this is the rare real
/// conversation that is still empty. The sidebar withholds it until it holds something. Nesting counts as content because
/// hiding a parent would orphan its children in the tree.
pub fn is_unsent_conversation(conversation: &Conversation, has_children: bool, unloaded: &HashSet<String>) -> bool {
    // A body that is not loaded is not an empty one.
    !has_children && conversation_has_no_contexts(conversation, unloaded) && conversation.queued_messages.is_empty()
}

/// The workspace's conversations minus the ones that are still empty.
pub fn visible_conversations<'a>(conversations: &'a [Conversation], unloaded: &HashSet<String>) -> Vec<&'a Conversation> {
    let parent_ids: HashSet<&str> = conversations.iter().filter_map(|conversation| conversation.parent_conversation_id.as_deref()).filter(|parent| !parent.is_empty()).collect();
    conversations.iter().filter(|conversation| !is_unsent_conversation(conversation, parent_ids.contains(conversation.id.as_str()), unloaded)).collect()
}

/// Projects a draft as a `Conversation`, so the normal timeline, composer, workspace, model, reasoning and security
/// controls render unchanged. A draft can already hold hand-written content; it materializes on the first request, or on
/// the first tool the user runs or records in it, not on the first message.
pub fn draft_as_conversation(draft: &DraftConversationState, title: &str) -> Conversation {
    let mut conversation: Conversation = serde_json::from_value(json!({ "id": draft_conversation_id(&draft.workspace_id), "title": title, "createdAt": draft.created_at, "updatedAt": draft.created_at, "settings": { "enabledTools": [] }, "contexts": [] })).expect("the fixed shape parses");
    conversation.settings = draft.settings.clone();
    conversation.contexts = draft.contexts.clone();
    conversation.run_target = draft.run_target.clone();
    conversation.attached_workspaces = draft.attached_workspaces.clone();
    conversation.preset_id = draft.preset_id.clone();
    conversation.template_id = draft.template_id.clone();
    conversation
}

/// What [`App::redeem_draft`] made of a draft.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedeemedDraft {
    pub conversation_id: String,
    pub workspace_id: String,
}

/// What [`App::materialize_draft`] made of a draft, and the worktrees still to create.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterializedDraft {
    pub conversation_id: String,
    pub workspace_id: String,
    pub worktree_members: Vec<u32>,
}

#[derive(Default)]
struct DraftsState {
    drafts: BTreeMap<String, DraftConversationState>,
}

fn state(app: &mut App) -> &mut DraftsState {
    app.local::<DraftsState>()
}

impl App {
    /// `drafts`: every open draft, by project.
    pub fn drafts(&mut self) -> Vec<DraftConversationState> {
        state(self).drafts.values().cloned().collect()
    }

    /// `drafts[workspaceId]`.
    pub fn draft_of_workspace(&mut self, workspace_id: &str) -> Option<DraftConversationState> {
        state(self).drafts.get(workspace_id).cloned()
    }

    /// `draftOf`: the open draft `conversation_id` names, if it names one.
    pub fn draft_of(&mut self, conversation_id: &str) -> Option<DraftConversationState> {
        let workspace_id = draft_workspace_id_of(conversation_id)?.to_owned();
        self.draft_of_workspace(&workspace_id)
    }

    /// `updateDraft`: rewrites `workspace_id`'s draft, if it has one; `None` from `update` removes it. `update` answers
    /// with the state to keep; answering the same value changes nothing.
    pub fn update_draft(&mut self, workspace_id: &str, update: impl FnOnce(&DraftConversationState) -> Option<DraftConversationState>) {
        let Some(draft) = state(self).drafts.get(workspace_id) else { return };
        let next = update(draft);
        if next.as_ref() == Some(draft) {
            return;
        }
        match next {
            Some(next) => state(self).drafts.insert(workspace_id.to_owned(), next),
            None => state(self).drafts.remove(workspace_id),
        };
        self.schedule_doc_effects();
        crate::prelude::repaint();
    }

    /// `hostConversationId`: the id the host knows a conversation's terminals and preview page by: its own, or for a draft
    /// the id it will materialize as, so the draft becoming real moves none of them. Pane layout is the exception: it
    /// follows the placeholder and is adopted on send.
    pub fn host_conversation_id(&mut self, conversation_id: &str) -> String {
        self.draft_of(conversation_id).map_or_else(|| conversation_id.to_owned(), |draft| draft.materializes_as)
    }

    /// `ownerWorksIn`: whether `owner_id`, a conversation or a draft by the id it will materialize as, works in the
    /// workspace. A draft is in no workspace's list, yet its shells sit in its project like any conversation's.
    pub fn owner_works_in(&mut self, owner_id: &str, workspace_id: &str) -> bool {
        if self.find_conversation(workspace_id, owner_id).is_some() {
            return true;
        }
        self.draft_of_workspace(workspace_id).is_some_and(|draft| draft.materializes_as == owner_id)
    }

    /// `draftWorksIn`: whether the workspace has a draft open, which shares the root checkout with its conversations.
    pub fn draft_works_in(&mut self, workspace_id: &str) -> bool {
        state(self).drafts.contains_key(workspace_id)
    }

    /// `resolveNewConversationSettings`: drafts and persisted conversations resolve settings identically, so
    /// materialization preserves draft edits. A project's new task prefers the project's preset, then its remembered
    /// settings ("Last used"), then the global default; the `Global` source always uses the global default.
    pub fn resolve_new_conversation_settings(&mut self, workspace_id: &str, source: NewConversationSource) -> Option<(ConversationSettings, String)> {
        let target = self.workspace(workspace_id);
        resolve_for(&self.doc, target, source)
    }

    /// `openDraftConversation`: opens a new task in a project: its draft, made from the project's preset the first time.
    /// An unsent draft outlives a visit to another conversation, so a project's new task returns to it. With no project
    /// asked for, the new task is the last project's: the one on screen, or the temporary project when there is none.
    pub fn open_draft_conversation(&mut self, workspace_id: Option<&str>) {
        let asked = workspace_id.map(str::to_owned).or_else(|| self.active_workspace_id());
        let available = |app: &mut Self, id: Option<&str>| -> Option<String> {
            let id = id?;
            if app.workspace(id).is_some() && !app.workspace_is_deleting(id) { Some(id.to_owned()) } else { None }
        };
        let Some(target) = available(self, asked.as_deref()).or_else(|| available(self, Some(TEMPORARY_WORKSPACE_ID))) else { return };
        if !self.draft_works_in(&target) {
            // A draft left unsent by an earlier run of the app comes back with the settings it had: it copied its preset
            // once, when it was opened, and has owned them since. Only a task that has never been opened is made from a preset.
            let stored = self.workspace(&target).and_then(|workspace| workspace.draft_conversation.clone());
            let resolved = match &stored {
                Some(stored) => {
                    let known: HashSet<String> = self.doc.tools.iter().map(|tool| tool.name.clone()).collect();
                    Some((clone_conversation_settings(&stored.settings, Some(&known)), stored.preset_id.clone()))
                }
                None => self.resolve_new_conversation_settings(&target, NewConversationSource::Workspace),
            };
            let Some((settings, preset_id)) = resolved else { return };
            let materializes_as = create_id("conv");
            // A draft brought back from the document holds settings the user chose.
            let resolved_settings = stored.is_none().then(|| settings.clone());
            state(self).drafts.insert(target.clone(), DraftConversationState { workspace_id: target.clone(), materializes_as: materializes_as.clone(), settings, created_at: now_iso(), worktree_members: Vec::new(), run_target: None, attached_workspaces: Vec::new(), preset_id: preset_id.clone(), template_id: String::new(), contexts: Vec::new(), template_contexts: None, resolved_settings });
            let preset = if stored.is_some() { None } else { self.doc.presets.conversation_presets.iter().find(|candidate| candidate.id == preset_id).cloned() };
            if let Some(preset) = preset.filter(|preset| !preset.template_id.is_empty()) {
                self.seed_draft_template(preset, materializes_as, None);
            }
        }
        let draft_id = draft_conversation_id(&target);
        self.set_active(Some(&target), Some(&draft_id));
        self.close_editors();
    }

    /// `seedDraftTemplate`: lays a preset's opening messages onto a draft made from it, so a new task starts as the preset
    /// says. It lands only while the draft is still made from this preset and its opening messages are still untouched:
    /// empty, or still `replacing`, the previous preset's, when the project's default changed under the draft. A tool card
    /// can only live in a conversation the host vouches for, so a template holding one makes a draft just opened on screen
    /// a conversation now. A default changing under a draft never does that: the draft is left with no opening messages.
    fn seed_draft_template(&mut self, preset: ConversationPreset, materializes_as: String, replacing: Option<Vec<ContextItem>>) {
        let request = json!({ "templateId": preset.template_id });
        self.invoke::<Vec<ContextItem>>("preview_conversation_template", request, move |app, result| {
            let Ok(body) = result else { return };
            if body.is_empty() && replacing.is_none() {
                return;
            }
            let untouched = |candidate: &DraftConversationState| candidate.materializes_as == materializes_as && candidate.preset_id == preset.id && (candidate.contexts.is_empty() || Some(&candidate.contexts) == replacing.as_ref());
            let Some(draft) = app.drafts().into_iter().find(|candidate| candidate.materializes_as == materializes_as) else { return };
            if !untouched(&draft) {
                return;
            }
            let tool_cards = body.iter().any(|context| matches!(context, ContextItem::Tool { .. }));
            if tool_cards && replacing.is_none() {
                if app.active_conversation_id() == Some(draft_conversation_id(&draft.workspace_id)) {
                    app.apply_preset_body(preset);
                }
                return;
            }
            let contexts: Vec<ContextItem> = if tool_cards { Vec::new() } else { body.into_iter().map(with_fresh_id).collect() };
            let template_id = preset.template_id.clone();
            app.update_draft(&draft.workspace_id, |current| {
                if !untouched(current) {
                    return Some(current.clone());
                }
                let mut next = current.clone();
                if contexts.is_empty() {
                    next.contexts = Vec::new();
                    next.template_id = String::new();
                    next.template_contexts = None;
                } else {
                    next.contexts = contexts.clone();
                    next.template_id = template_id.clone();
                    next.template_contexts = Some(contexts.clone());
                }
                Some(next)
            });
        });
    }

    /// `setWorkspaceDefaultPreset`'s second half: the project's draft follows, if it still holds what the project resolved it to.
    pub(crate) fn draft_follow_default_preset(&mut self, workspace: &Workspace) {
        let Some(draft) = self.draft_of_workspace(&workspace.id).filter(draft_follows_project) else { return };
        let Some((mut settings, preset_id)) = resolve_for(&self.doc, Some(workspace), NewConversationSource::Workspace) else { return };
        // The composer's own switches are the draft's, not the preset's.
        settings.reasoning_effort = draft.settings.reasoning_effort;
        settings.plan_mode_enabled = draft.settings.plan_mode_enabled;
        let preset = conversation_preset_by_id(&self.doc.presets, &preset_id).cloned();
        let has_template = preset.as_ref().is_some_and(|preset| !preset.template_id.is_empty());
        let opening_untouched = draft.contexts.is_empty() || Some(&draft.contexts) == draft.template_contexts.as_ref();
        let materializes_as = draft.materializes_as.clone();
        self.update_draft(&workspace.id, |latest| {
            if latest.materializes_as != materializes_as {
                return Some(latest.clone());
            }
            let mut next = latest.clone();
            next.settings = settings.clone();
            next.resolved_settings = Some(settings);
            next.preset_id = preset_id;
            // A preset with opening messages of its own replaces these once they are fetched.
            if opening_untouched && !has_template {
                next.contexts = Vec::new();
                next.template_id = String::new();
                next.template_contexts = None;
            }
            Some(next)
        });
        if let Some(preset) = preset.filter(|_| opening_untouched && has_template) {
            self.seed_draft_template(preset, materializes_as, Some(draft.contexts));
        }
    }

    /// `updateActiveConversation`: changes the conversation on screen, draft or not, so settings and timeline surfaces
    /// need not special-case drafts. A preset-owned edit turns the conversation into an unnamed draft, derived here so
    /// every surface agrees; an updater that sets `preset_id` itself is how applying a preset re-establishes the trace.
    pub fn update_active_conversation(&mut self, updater: impl FnOnce(&mut Conversation)) {
        fn with_preset_trace(conversation: &mut Conversation, updater: impl FnOnce(&mut Conversation)) {
            let preset_before = conversation.preset_id.clone();
            let settings_before = conversation.settings.clone();
            updater(conversation);
            if !preset_before.is_empty() && conversation.preset_id == preset_before && conversation.settings != settings_before && !same_conversation_preset_settings(&capture_conversation_preset_settings(&settings_before), &capture_conversation_preset_settings(&conversation.settings)) {
                conversation.preset_id = String::new();
            }
        }
        let Some(active) = self.active_conversation_id() else { return };
        if let Some(draft) = self.draft_of(&active) {
            // Settings and content are the draft's own; a worktree is not, because it belongs to a persisted conversation
            // id and the draft carries `worktree_members`.
            let mut projected = draft_as_conversation(&draft, "");
            with_preset_trace(&mut projected, updater);
            self.update_draft(&draft.workspace_id, |current| {
                let mut next = current.clone();
                next.settings = projected.settings;
                next.contexts = projected.contexts;
                next.preset_id = projected.preset_id;
                next.template_id = projected.template_id;
                Some(next)
            });
            return;
        }
        let Some(workspace_id) = self.active_workspace_id() else { return };
        self.update_conversation(&workspace_id, &active, |conversation| {
            with_preset_trace(conversation, updater);
            conversation.updated_at = now_iso();
        });
    }

    /// `saveActiveConversationComposition`: presets are templates, so composition edits are written directly to conversation settings.
    pub fn save_active_conversation_composition(&mut self, settings: ConversationSettings) {
        self.update_active_conversation(|conversation| conversation.settings = settings);
    }

    /// `updateActiveConversationSettingsOnly`: the patch is a function here.
    pub fn update_active_conversation_settings(&mut self, patch: impl FnOnce(&mut ConversationSettings)) {
        self.update_active_conversation(|conversation| patch(&mut conversation.settings));
    }

    /// The effect keeping the draft's settings in the document: only the settings and their preset trace are kept; its
    /// text, grants and shells belong to this run of the app. A draft that is sent or forgotten clears its own.
    pub(crate) fn keep_draft_settings_in_document(&mut self) {
        // Taken out rather than cloned: this runs after every document change, and a draft can hold a timeline.
        let drafts = std::mem::take(&mut state(self).drafts);
        if drafts.is_empty() {
            return;
        }
        self.update_document_if(|doc| {
            let mut changed = false;
            for draft in drafts.values() {
                let Some(workspace) = doc.workspaces.iter_mut().find(|workspace| workspace.id == draft.workspace_id) else { continue };
                if !draft_holds_own_settings(draft) {
                    if workspace.draft_conversation.take().is_some() {
                        changed = true;
                    }
                    continue;
                }
                let same = workspace.draft_conversation.as_ref().is_some_and(|stored| stored.settings == draft.settings && stored.preset_id == draft.preset_id);
                if !same {
                    workspace.draft_conversation = Some(DraftConversationSnapshot { settings: draft.settings.clone(), preset_id: draft.preset_id.clone() });
                    changed = true;
                }
            }
            changed
        });
        state(self).drafts = drafts;
    }

    /// `draftPreviewServers` and `releaseDraftSurfaces` as jobs: ends everything a project's draft opened at the host: its
    /// shells and dev servers are killed and its preview page closed. The draft itself stays until [`App::forget_draft`]:
    /// a project deletion ends these first, since a running command would hold it up.
    pub(crate) fn draft_surface_closers(&mut self, workspace_id: &str) -> Vec<SurfaceCloser> {
        let Some(draft) = self.draft_of_workspace(workspace_id) else { return Vec::new() };
        let mut closers = self.conversation_surface_closers(std::slice::from_ref(&draft.materializes_as));
        let hooks = self.doc_hooks().draft_server_closers.clone();
        for closers_of in hooks {
            closers.extend(closers_of(self, &draft.materializes_as, workspace_id));
        }
        closers
    }

    /// `forgetDraft`: throws a project's draft away: pane layout, composer text and attachments, workspace chip selection
    /// and timeline history are forgotten, along with the settings kept for it in the document. Only a project that is
    /// going away loses its draft.
    pub fn forget_draft(&mut self, workspace_id: &str) {
        let Some(draft) = self.draft_of_workspace(workspace_id) else { return };
        let draft_id = draft_conversation_id(workspace_id);
        self.dispatch_side_panes(SidePanesAction::RemoveConversation { conversation_id: draft_id.clone() });
        self.invalidate_composer_images(std::slice::from_ref(&draft_id));
        let composer = self.composer();
        composer.drafts.remove(&draft_id);
        composer.pasted_texts.remove(&draft_id);
        composer.attachment_notices.remove(&draft_id);
        self.forget_selection_of(&draft_id);
        self.timeline_history().forget(&draft.materializes_as);
        let hooks = self.doc_hooks().draft_forgotten.clone();
        for forget in hooks {
            forget(self, &draft_id, &draft.materializes_as);
        }
        self.update_draft(workspace_id, |_| None);
        self.clear_stored_draft(workspace_id);
    }

    fn clear_stored_draft(&mut self, workspace_id: &str) {
        self.update_document_if(|doc| doc.workspaces.iter_mut().find(|workspace| workspace.id == workspace_id).is_some_and(|workspace| workspace.draft_conversation.take().is_some()));
    }

    /// `adoptDraftConversationId`: moves everything the renderer keyed by the draft's placeholder id onto the real
    /// conversation: pane layout, composer text, staged images, the workspace chip's choice. Its terminals and preview page
    /// need nothing: they are already keyed by the id it became.
    fn adopt_draft_conversation_id(&mut self, draft_id: &str, conversation_id: &str, worktree_members: &[u32]) {
        self.dispatch_side_panes(SidePanesAction::AdoptConversation { conversation_id: conversation_id.to_owned(), from: draft_id.to_owned() });
        fn adopt_non_empty<T>(map: &mut std::collections::HashMap<String, Vec<T>>, from: &str, to: &str) {
            // Each keeps an empty entry as it is: an empty list has nothing to carry over.
            if map.get(from).is_some_and(|pending| !pending.is_empty()) {
                if let Some(pending) = map.remove(from) {
                    map.insert(to.to_owned(), pending);
                }
            }
        }
        let composer = self.composer();
        if let Some(pending) = composer.drafts.remove(draft_id) {
            composer.drafts.insert(conversation_id.to_owned(), pending);
        }
        adopt_non_empty(&mut composer.pasted_texts, draft_id, conversation_id);
        adopt_non_empty(&mut composer.image_drafts, draft_id, conversation_id);
        adopt_non_empty(&mut composer.file_drafts, draft_id, conversation_id);
        adopt_non_empty(&mut composer.attachment_notices, draft_id, conversation_id);
        adopt_non_empty(&mut composer.element_picks, draft_id, conversation_id);
        self.adopt_selection_of(draft_id, conversation_id);
        let hooks = self.doc_hooks().draft_adopted.clone();
        for adopted in hooks {
            adopted(self, draft_id, conversation_id, worktree_members);
        }
    }

    /// `materializeDraft`: turns the draft on screen into a conversation in its project, synchronously. `None` when no
    /// draft is on screen. Nothing has reached the host's saved document yet; see [`App::redeem_draft`].
    pub fn materialize_draft(&mut self) -> Option<MaterializedDraft> {
        let active = self.active_conversation_id()?;
        let draft = self.draft_of(&active)?;
        let workspace_id = draft.workspace_id.clone();
        let created = self.create_conversation(NewConversation {
            workspace_id: Some(workspace_id.clone()),
            source: Some(NewConversationSource::Global),
            settings_override: Some(draft.settings.clone()),
            run_target_override: draft.run_target.clone(),
            initial_contexts: draft.contexts.clone(),
            preset_id_override: draft.preset_id.clone(),
            template_id_override: draft.template_id.clone(),
            attached_workspaces_override: draft.attached_workspaces.clone(),
            conversation_id_override: Some(draft.materializes_as.clone()),
            ..NewConversation::default()
        })?;
        // The draft is this conversation now; clear it so it cannot regain the active view. A new task in its project
        // opened before the next render must not find it still waiting either.
        self.update_draft(&workspace_id, |_| None);
        // Its settings live on the conversation now, so there is no draft left to bring back.
        self.clear_stored_draft(&workspace_id);
        // Only a directory project's own workspaces can have worktrees; discard an unrealizable request (a temporary
        // project, a workspace removed since it was ticked) before it can block sending.
        let worktree_members: Vec<u32> = match self.workspace(&workspace_id).filter(|project| project.kind == WorkspaceKind::Directory) {
            Some(project) => {
                let count = registered_project_workspaces(Some(project)).len() as u32;
                draft.worktree_members.iter().copied().filter(|member| *member >= 1 && *member <= count).collect()
            }
            None => Vec::new(),
        };
        self.adopt_draft_conversation_id(&draft_conversation_id(&workspace_id), &created, &worktree_members);
        Some(MaterializedDraft { conversation_id: created, workspace_id, worktree_members })
    }

    /// `redeemDraft`: makes the draft a conversation the host can act on, fixed from then on in the project it names. A
    /// worktree the draft asked for is created here, before anything can run at the workspace root in its place; failing
    /// to create one answers an error, with the conversation already real. `persist` waits until the host holds the
    /// conversation; a send skips it only because the send persists the conversation together with its first message.
    /// `Ok(None)` when no draft is on screen.
    pub fn redeem_draft(&mut self, persist: bool, done: impl FnOnce(&mut App, Result<Option<RedeemedDraft>, String>) + Send + 'static) {
        let Some(redeemed) = self.materialize_draft() else { return done(self, Ok(None)) };
        let result = RedeemedDraft { conversation_id: redeemed.conversation_id.clone(), workspace_id: redeemed.workspace_id.clone() };
        if redeemed.worktree_members.is_empty() {
            if !persist {
                return done(self, Ok(Some(result)));
            }
            return self.flush_latest_document(true, move |app, flushed| done(app, flushed.map(|()| Some(result))));
        }
        self.spawn(move |task| {
            let outcome = create_draft_worktrees(&task, &redeemed);
            task.post(move |app| done(app, outcome.map(|()| Some(result))));
        });
    }

    /// `promote_draft`: what sending does to a draft, up to the send: the draft becomes a conversation, its worktrees
    /// created if it asked for any. The send persists the conversation together with its first message, so with no
    /// worktree nothing waits here. `Ok(None)` when no draft is on screen.
    pub fn promote_draft(&mut self, done: impl FnOnce(&mut App, Result<Option<RedeemedDraft>, String>) + Send + 'static) {
        self.redeem_draft(false, done);
    }
}

impl crate::Task {
    /// [`App::redeem_draft`], for a task thread: what the send pipeline wants.
    pub fn redeem_draft(&self, persist: bool) -> Result<Option<RedeemedDraft>, String> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.post(move |app| {
            app.redeem_draft(persist, move |_, result| {
                let _ = tx.send(result);
            });
        });
        rx.recv().unwrap_or_else(|_| Err("the app is closing".to_owned()))
    }
}

fn create_draft_worktrees(task: &crate::Task, redeemed: &MaterializedDraft) -> Result<(), String> {
    // The host resolves a conversation only from its saved document, worktree requests included.
    task.flush_latest_document(true)?;
    let (workspace_id, conversation_id) = (redeemed.workspace_id.clone(), redeemed.conversation_id.clone());
    let registered = {
        let workspace_id = workspace_id.clone();
        task.update(move |app| registered_project_workspaces(app.workspace(&workspace_id)))
    };
    // One after another: two worktrees of one repository must not race for a name.
    for member in &redeemed.worktree_members {
        let Some(workspace) = registered.get(*member as usize - 1).cloned() else { continue };
        let worktree = task.invoke("create_conversation_worktree", json!({ "conversationId": conversation_id, "member": member, "fromBranch": null }))?;
        let (ws, id, member) = (workspace_id.clone(), conversation_id.clone(), *member as usize);
        task.update(move |app| {
            app.update_conversation(&ws, &id, |conversation| {
                conversation.worktrees = with_conversation_worktree(&conversation.worktrees, member, &workspace, Some(worktree));
            });
        });
    }
    // The host also reads the persisted worktree records for trusted path resolution; without them, the work would run
    // at the workspace roots.
    task.flush_latest_document(true)?;
    // Refresh the Git snapshots after moving from the workspace roots to the worktree checkouts.
    let members = redeemed.worktree_members.clone();
    task.post(move |app| {
        let hooks = app.doc_hooks().git_refresh.clone();
        for member in members {
            for refresh in &hooks {
                refresh(app, &conversation_id, &workspace_id, member);
            }
        }
    });
    Ok(())
}

/// A template's text card as the draft's own: fresh id, everything else kept.
fn with_fresh_id(context: ContextItem) -> ContextItem {
    let mut value = serde_json::to_value(&context).expect("a context serializes");
    value["id"] = json!(create_id("ctx"));
    serde_json::from_value(value).unwrap_or(context)
}

fn resolve_for(document: &crate::model::AppDocument, target: Option<&Workspace>, source: NewConversationSource) -> Option<(ConversationSettings, String)> {
    let known: HashSet<String> = document.tools.iter().map(|tool| tool.name.clone()).collect();
    // Presets applied below decide everything else. Off until a preset says otherwise: reaching the network and the
    // memory tiers are capabilities a conversation is given, not ones it starts with, and security has no global default,
    // so this is the safest base value.
    let mut blank: ConversationSettings = serde_json::from_value(json!({ "enabledTools": [], "securityLevel": "request_approval" })).ok()?;
    blank.reasoning_effort = document.global_settings.last_reasoning_effort;
    let by_workspace = if source == NewConversationSource::Workspace { target } else { None };
    if let Some(preset) = by_workspace.and_then(|target| conversation_preset_by_id(&document.presets, &target.default_conversation_preset_id)) {
        return Some((apply_conversation_preset_settings(&blank, &preset.settings, Some(&known)), preset.id.clone()));
    }
    // A remembered snapshot is the workspace's own unnamed draft: it has no preset identity. Plan mode is not remembered:
    // it belongs to the task it was switched on for. Nor is the auto-compact method: a new conversation chooses it by its model.
    if let Some(remembered) = by_workspace.and_then(|target| target.last_conversation_settings.as_ref()) {
        let mut settings = clone_conversation_settings(remembered, Some(&known));
        settings.compaction_method = None;
        settings.plan_mode_enabled = false;
        return Some((settings, String::new()));
    }
    let Some(fallback) = default_conversation_preset(&document.presets) else { return Some((blank, String::new())) };
    Some((apply_conversation_preset_settings(&blank, &fallback.settings, Some(&known)), fallback.id.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::test_support::{app_with, conversation, pump_until, text};
    use serde_json::Value;
    use std::sync::{Arc, Mutex};

    fn app(tag: &str) -> App {
        app_with(Arc::new(|command, args| if command == "create_conversation" { Ok(args["conversation"].clone()) } else if command == "update_conversation" { Ok(args["conversation"].clone()) } else { Ok(Value::Null) }), tag)
    }

    fn settle(app: &mut App) {
        for _ in 0..4 {
            app.tick();
        }
    }

    #[test]
    fn a_draft_id_names_its_project_and_nothing_else_does() {
        assert_eq!(draft_conversation_id("ws"), "__draft__:ws");
        assert_eq!(draft_workspace_id_of("__draft__:ws"), Some("ws"));
        assert_eq!(draft_workspace_id_of("__draft__:"), None);
        assert_eq!(draft_workspace_id_of("conv_1"), None);
        assert!(is_draft_conversation_id("__draft__:ws"));
        assert!(!is_draft_conversation_id("__draft__"));
    }

    #[test]
    fn the_sidebar_withholds_an_empty_conversation_unless_it_has_children() {
        let unloaded = HashSet::new();
        let empty = conversation("empty");
        let mut full = conversation("full");
        full.contexts = vec![text("u", "user")];
        let mut parent = conversation("parent");
        parent.title = "p".into();
        let mut child = conversation("child");
        child.parent_conversation_id = Some("parent".into());
        child.contexts = vec![text("c", "user")];
        let list = vec![empty, full, parent, child];
        let visible: Vec<&str> = visible_conversations(&list, &unloaded).iter().map(|conversation| conversation.id.as_str()).collect();
        assert_eq!(visible, vec!["full", "parent", "child"]);
        // A body that is not loaded is not an empty one.
        let hollow: HashSet<String> = ["empty".to_owned()].into();
        assert_eq!(visible_conversations(&list, &hollow).len(), 4);
    }

    #[test]
    fn opens_the_active_projects_draft_and_returns_to_it() {
        let mut app = app("open");
        let workspace_id = app.doc.workspaces[0].id.clone();
        app.open_draft_conversation(Some(&workspace_id));
        let id = draft_conversation_id(&workspace_id);
        assert_eq!(app.active_conversation_id().as_deref(), Some(id.as_str()));
        let first = app.draft_of(&id).unwrap();
        assert!(app.draft_is_active());
        app.update_active_conversation(|conversation| conversation.contexts = vec![text("mine", "user")]);
        app.set_active(None, None);
        app.open_draft_conversation(Some(&workspace_id));
        let again = app.draft_of(&id).unwrap();
        assert_eq!(again.materializes_as, first.materializes_as);
        assert_eq!(again.contexts.len(), 1);
    }

    #[test]
    fn falls_back_to_the_temporary_project_when_none_is_on_screen() {
        let mut app = app("fallback");
        app.set_active(None, None);
        app.open_draft_conversation(None);
        assert_eq!(app.active_workspace_id().as_deref(), Some(TEMPORARY_WORKSPACE_ID));
    }

    #[test]
    fn a_preset_owned_edit_leaves_the_preset_and_a_trace_setting_update_keeps_it() {
        let mut app = app("trace");
        let workspace_id = app.doc.workspaces[0].id.clone();
        app.open_draft_conversation(Some(&workspace_id));
        let id = draft_conversation_id(&workspace_id);
        app.update_draft(&workspace_id, |draft| {
            let mut next = draft.clone();
            next.preset_id = "p".into();
            Some(next)
        });
        // Reasoning effort isn't owned by the preset.
        app.update_active_conversation_settings(|settings| settings.plan_mode_enabled = true);
        assert_eq!(app.draft_of(&id).unwrap().preset_id, "p");
        app.update_active_conversation_settings(|settings| settings.global_memory_enabled = !settings.global_memory_enabled);
        assert_eq!(app.draft_of(&id).unwrap().preset_id, "");
    }

    #[test]
    fn keeps_a_drafts_own_settings_in_the_document_and_forgets_them_when_it_stops_holding_any() {
        let mut app = app("keep");
        let workspace_id = app.doc.workspaces[0].id.clone();
        app.open_draft_conversation(Some(&workspace_id));
        settle(&mut app);
        assert!(app.workspace(&workspace_id).unwrap().draft_conversation.is_none());
        app.update_active_conversation_settings(|settings| settings.global_memory_enabled = !settings.global_memory_enabled);
        settle(&mut app);
        assert!(app.workspace(&workspace_id).unwrap().draft_conversation.is_some());
        app.update_active_conversation_settings(|settings| settings.global_memory_enabled = !settings.global_memory_enabled);
        settle(&mut app);
        assert!(app.workspace(&workspace_id).unwrap().draft_conversation.is_none());
    }

    #[test]
    fn a_draft_brought_back_from_the_document_keeps_its_settings_and_does_not_follow_the_project() {
        let mut app = app("restore");
        let workspace_id = app.doc.workspaces[0].id.clone();
        let mut settings: ConversationSettings = serde_json::from_value(json!({ "enabledTools": [] })).unwrap();
        settings.project_memory_enabled = true;
        app.doc.workspaces[0].draft_conversation = Some(DraftConversationSnapshot { settings, preset_id: "kept".into() });
        app.open_draft_conversation(Some(&workspace_id));
        let draft = app.draft_of(&draft_conversation_id(&workspace_id)).unwrap();
        assert!(draft.settings.project_memory_enabled);
        assert_eq!(draft.preset_id, "kept");
        assert!(draft.resolved_settings.is_none());
        assert!(!draft_follows_project(&draft));
    }

    #[test]
    fn materializing_makes_the_conversation_under_the_minted_id_and_carries_what_the_draft_held() {
        let mut app = app("materialize");
        let workspace_id = app.doc.workspaces[0].id.clone();
        app.open_draft_conversation(Some(&workspace_id));
        let draft_id = draft_conversation_id(&workspace_id);
        let minted = app.draft_of(&draft_id).unwrap().materializes_as;
        app.update_active_conversation(|conversation| conversation.contexts = vec![text("mine", "user")]);
        app.set_composer_draft(&draft_id, "typed");
        app.select_workspace_member(&draft_id, 1);
        let redeemed = app.materialize_draft().unwrap();
        assert_eq!(redeemed.conversation_id, minted);
        assert!(app.draft_of(&draft_id).is_none());
        assert_eq!(app.active_conversation_id().as_deref(), Some(minted.as_str()));
        assert_eq!(app.find_conversation(&workspace_id, &minted).unwrap().contexts.len(), 1);
        assert_eq!(app.composer_draft(&minted), "typed");
        assert_eq!(app.composer_draft(&draft_id), "");
        // A new task now is a new draft with a new id.
        app.open_draft_conversation(Some(&workspace_id));
        assert_ne!(app.draft_of(&draft_id).unwrap().materializes_as, minted);
    }

    #[test]
    fn promoting_a_draft_without_worktrees_answers_at_once_and_redeeming_with_persist_waits_for_the_save() {
        let saves = Arc::new(Mutex::new(0));
        let counted = saves.clone();
        let mut app = app_with(Arc::new(move |command, args| match command {
            "save_document" => {
                *counted.lock().unwrap() += 1;
                Ok(Value::Null)
            }
            "create_conversation" => Ok(args["conversation"].clone()),
            _ => Ok(Value::Null),
        }), "promote");
        let workspace_id = app.doc.workspaces[0].id.clone();
        app.open_draft_conversation(Some(&workspace_id));
        let answer = Arc::new(Mutex::new(None));
        let slot = answer.clone();
        app.promote_draft(move |_, result| *slot.lock().unwrap() = Some(result));
        let promoted = answer.lock().unwrap().take().expect("no wait").unwrap().unwrap();
        assert_eq!(promoted.workspace_id, workspace_id);
        assert_eq!(*saves.lock().unwrap(), 0);
        app.open_draft_conversation(Some(&workspace_id));
        let slot = answer.clone();
        app.redeem_draft(true, move |_, result| *slot.lock().unwrap() = Some(result));
        pump_until(&mut app, "the redemption", |_| answer.lock().unwrap().is_some());
        assert!(*saves.lock().unwrap() >= 1);
        // With no draft on screen there's nothing to redeem.
        let slot = answer.clone();
        app.redeem_draft(true, move |_, result| *slot.lock().unwrap() = Some(result));
        assert_eq!(answer.lock().unwrap().clone().unwrap(), Ok(None));
    }

    #[test]
    fn forgetting_a_draft_clears_what_was_kept_for_it() {
        let mut app = app("forget");
        let workspace_id = app.doc.workspaces[0].id.clone();
        app.open_draft_conversation(Some(&workspace_id));
        let draft_id = draft_conversation_id(&workspace_id);
        app.set_composer_draft(&draft_id, "typed");
        app.update_active_conversation_settings(|settings| settings.global_memory_enabled = !settings.global_memory_enabled);
        settle(&mut app);
        app.forget_draft(&workspace_id);
        assert!(app.draft_of(&draft_id).is_none());
        assert_eq!(app.composer_draft(&draft_id), "");
        assert!(app.workspace(&workspace_id).unwrap().draft_conversation.is_none());
    }

    #[test]
    fn creating_a_conversation_resolves_a_compaction_method_only_for_a_new_task() {
        let mut app = app("compaction");
        let workspace_id = app.doc.workspaces[0].id.clone();
        let id = app.create_conversation(NewConversation { workspace_id: Some(workspace_id.clone()), ..NewConversation::default() }).unwrap();
        assert!(app.find_conversation(&workspace_id, &id).unwrap().settings.compaction_method.is_some());
        assert_eq!(app.active_conversation_id().as_deref(), Some(id.as_str()));
        assert_eq!(app.doc.workspaces[0].conversations[0].id, id);
        assert!(app.workspace(&workspace_id).unwrap().last_conversation_settings.is_some());
        let child = app.create_conversation(NewConversation { workspace_id: Some(workspace_id.clone()), parent_conversation_id: Some(id), settings_override: Some(serde_json::from_value(json!({ "enabledTools": [] })).unwrap()), ..NewConversation::default() }).unwrap();
        assert!(app.find_conversation(&workspace_id, &child).unwrap().settings.compaction_method.is_none());
    }
}
