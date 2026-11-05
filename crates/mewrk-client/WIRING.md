# What the shell must wire

Collected from the surface crates' reports, as they stood when each crate finished. Public view signatures are in each crate's `src/lib.rs` header.

The shell has wired everything below except what "Still open" names, and later sections record decisions rather than work left.

## Still open
- Duplicates to unify: `image_strip` (chat, composer), `shell_task_panel` (tasks, terminal), `rolling_number` (tasks, composer), `elide_path` and `path.rs` (files, tasks, git, terminal), attachment intake (`mewrk_client::runs::attachments`, `mewrk_pane_files::attachments`).
- `web.rs` in settings-conv: search provider rules should move to one client module with `settings-global/logic/web_search.rs`.
- Accessibility: `El` has its tree and the window serves it (`crates/fluent/src/el/a11y.rs`, `accesskit_winit` in `crates/mewrk/src/main.rs`), but surfaces carry few of upstream's `aria-label`s yet. `PORTING.md` says how to add them. The window showing the fatal error has no tree.
- Spell check has no effect outside macOS, which has the only checker. Custom CSS is left out on purpose: the appearance page has no card for it, and `custom_css` stays in the document for upstream's sake with nothing reading it.
- Taffy drops a row's `gap` when an auto margin takes free space.

## crates/panes/tasks
- Start the stores named in its `lib.rs` header; keep `derive_subagent_views`' result rather than calling it every frame.
- `TranscriptView` closure: the chat timeline. `StopHooks`: `requestBrowserSessionClose`, `closePagesServedAt`, `stopBrowserAutomation`, `destroyTerminalSession`.

## crates/panes/files
- Set `app.native_view` to the window's content view; set `Input::zoom_modifier` while Cmd or Ctrl is held.
- Call `path_open::set_path_open_handler`.
- Call `attachment_drop::handle_native_drag` after `host.drag_entered` and `host.drag_dropped`.
- `files_pane` is called every frame with `active:false` while off screen; `drop_files_pane` is what unmounting did.
- `files_pane` returns `FilesPaneView { header, menu_sections, body }` for the shell's `side_pane`; menus, `PathText` and Markdown are `mewrk_ui_common`'s, `FileMention` is `mewrk_client::file_mention`'s.

## Shared widgets to unify later
`page_tabs` exists in tasks, files, and terminal (`page_tabs.rs`). `elide_path` is duplicated in tasks and git.

## crates/ui/settings-conv
- `conversation_settings_pane(app) -> Option<E>` is the live pane. Set `ops::live(app).on_remote_reveal` so remote config folders open in the Files pane.
- Call `mewrk_client::doc::init` at startup.
- `template_editor.rs`: swap the stand-in timeline for the chat crate's `ConversationView` (right-click menu, image and file paste, `InlineToolEditor`).
- `web.rs`: move search provider rules to a client web-search module once one exists (`lib/webSearch.ts`).
- Open: `writes_a_new_role_as_a_real_file_selects_it_and_edits_it_in_place` timed out once at 20 s while the disk was full; passed 18 reruns. Upstream's rescan has no guard against an older catalog reply overwriting a newer one, which is another possible cause.

## crates/mewrk-client/src/runs
- Call `runs_reconcile()` once per frame (or from `run_doc_effects`), or a document change the run code didn't make won't start waiting queue heads or wakes.
- Drain `run_signals()`: `open_provider_settings`, `scroll_timeline_to_bottom`, `register_preview_sessions`, `branch_chip_error`.
- `save_conversation_turns()` before `flush_ui_store` on quit. `note_conversation_activated(conv)` when a conversation is opened.
- `extract_pdf_text` is a stub, so PDF attachments are refused as unreadable: upstream uses pdf.js.
- `stop_browser_automation` discards the status the host returns until the browser controller exists.

### App methods
- Sending and queue: `send_message`, `delete_queued_message(ws, conv, msg)`, `steer_queued_message(conv, msg, done)`, `dispatch_next_queued_message(ws, conv)`, `retry_failed_queued_promotion(ws, conv, msg)`, `queue_is_paused(conv)`
- Composer: `composer()`, `composer_draft(conv)`, `set_composer_draft`, `set_composer_pasted_texts`, `set_composer_element_picks`, `add_composer_images(conv, files, done)`, `add_composer_attachments(conv, files, pre_rejected)`, `remove_composer_image`, `remove_composer_file`, `invalidate_composer_images`, `put_message_in_composer(conv, item)`
- Reading a run: `run_view(conv) -> Option<Arc<RunView>>` (built at most once per commit, 10 per second), `rendered_turns(conv)`, `conversation_turns(conv)`, `conversation_turns_revision()`, `live_context_tokens(conv, fallback)`, `context_usage_view(conv)`, `model_run_summaries()`
- Controlling a run: `cancel_run(conv, done)`, `retry_model_run()`, `model_run_error(conv)`, `dismiss_model_run_error(conv)`, `model_run_is_active(conv)`, `model_is_stopping(conv)`, `stop_browser_automation(conv, done)`, `compact_conversation_now(ws, conv, done)`, `wake_conversation(ws, conv, done)`, `start_forked_conversation_run(ws, conv, done)`
- Status: `conversation_status(conv, other_activity)`, `conversation_is_blocked(conv)`
- Tool prompts: `tool_prompts(conv)`, `active_tool_prompt(conv)`, `navigate_tool_prompt(conv, delta)`, `set_tool_prompt_cursor`, `next_tool_prompt_navigation(conv, find_agent)`, `question_is_pending(conv)`, `decide_tool_prompt(conv, prompt, decision, feedback)`, `remember_question_draft(prompt, response)`, `answer_question_prompt(conv, prompt, response, done)`, `run_manual_tool(conv, request, done)`
- Plans, forks, workflows: `conversation_plan(conv)`, `load_conversation_plan`, `fork_requests()`, `decide_fork_request(id, approved)`, `fork_decisions(conv)`, `load_fork_decisions`, `fork_start_error()`, `retry_fork_starts()`, `control_workflow_step(conv, run, step, action)`, `workflow_step_record(conv, run, step, done)`
- Explanations and titles: `tool_explanation(ctx, conv, call)`, `tool_error_explanation(ctx)`, `tool_explanation_version()`, `load_tool_explanations`, `settle_conversation_title(conv)`
- Timeline: `commit_timeline_edit(conv, patch, label, notice)`, `step_timeline_history(conv, action)`, `timeline_contexts_of(conv)`, `timeline_notice()`, `show_timeline_notice(...)`, `timeline_history()`
- Lifecycle: `init_runs()` (automatic), `runs_reconcile()`, `note_conversation_activated(conv)`, `run_signals()`, `save_conversation_turns()`, `runs()`, `set_run_backend`

### View-state types in `mewrk_client::runs`
- Live timeline: `RunView` (`contexts`, `thinking`, `retry`, `workflow_progress_by_call`, `workflow_run_id_by_call`, `browser_automation_tool`, `revision`). `RunView::rendered_contexts(&persisted)` walks the timeline as `RenderedContext` (`item`, `streaming`, `stream_status`, `live`, `started_at`) without copying it.
- Streaming rows: `LiveContext`, `SubagentLiveState`, `SubagentLiveStatus`, `StreamingToolPhase`, `LiveReasoningView`, `ModelRunRetryState`
- Turns and runs: `ConversationTurn`, `ConversationTurnStatus`, `ConversationTurnError`, `ModelRunSummary`, `ModelRunErrorState`, `ConversationStatus`
- Usage: `ContextUsage`, `LiveContextTokens`, `ContextBreakdown`
- Prompts and forks: `PendingToolPrompt`, `PromptKind`, `QuestionResponse`, `QuestionAction`, `ToolPromptDecision`, `ToolPromptNavigation`, `PendingForkRequest`, `ForkDecisionRecord`
- Composer: `ComposerState`, `ComposerFile`, `AttachmentRejection`, `PastedText`, `ElementPick`
- Other: `WorkflowProgressEntry`, `WorkflowStepAction`, `TimelinePatch`, `TimelineNotice`, `TimelineHistoryAction`, `RunSignals`, `StreamRevealSplit`

## crates/mewrk-client doc and selection
- `mewrk_client::doc::init(&mut app)` in `root::init`. `app.flush_ui_store()` (after `save_conversation_turns()`) before `host.request_exit`.
- `app.doc_hooks()`: register Git mutation state, terminal and browser closers, dev servers, notices, editors. Until registered they read as "nothing there", so deleting a conversation closes no terminals.
- `ErrorView`'s reset: `reset_document` exists but `App::start` has no path for a host that can't load.
- Keys: `KeyInput::from_key_press` derives the key from the typed character; `main.rs` only sends letters with Cmd or Ctrl held and never sends Space. Fix in the driver if a shortcut needs it.
- Not moved to `ui_store`, owners should: `mewrk.api-key-length.v1.*`, `mewrk.search-api-key-length.v2.*`, `mewrk.usage-backfilled.v1`, `mewrk.browser-lifecycle-intent-epoch.v1`.
- Upstream bug ported as is: the delete-project dialog says "permanently" but `deleteWorkspace` sends no delete commands, so the host adopts its conversations into the temporary project at next start.

### App methods (docs in source; `done` = completion closure)
- Store: `update_document`, `update_document_if`, `document_changed`, `publish_document`, `flush_document_store`, `flush_latest_document`, `flush_document_saves`, `persist_document_immediately`, `save_status`, `save_failure_reason`, `report_backend_save_result`, `dispose_document_store`, `resume_document_store`, `await_workspace_at_host`, `reset_document`, `schedule_doc_effects`, `doc_hooks`, `set_doc_backend`
- Lookups: `find_conversation(ws, id)`, `conversation(id)`, `workspace(id)`, `workspace_of_conversation(id)`
- Conversations: `create_conversation(NewConversation)`, `update_conversation`, `update_conversation_if`, `update_conversation_local`, `update_active_conversation`, `update_active_conversation_settings`, `save_active_conversation_composition`, `rename_conversation`, `delete_conversation(ws, id)`, `reorder_sidebar_conversation(ws, id, target, after)`, `move_active_conversation`, `branch_from_user_context`, `fork_conversation_at(index)`, `select_conversation_branch`, `apply_authoritative_conversation`
- Guards: `conversation_is_busy`, `conversation_operation_is_active`, `context_mutation_is_blocked`, `conversation_is_in_locked_workspace`, `conversation_is_deleting`, `conversation_deletion_is_active`, `conversation_move_is_blocked`, `workspace_is_deleting`
- Bodies: `ensure_conversation_loaded`, `load_active_body`, `body_load_failure`, `is_body_unloaded`, `conversation_has_no_contexts`, `conversation_body_is_loading`, `hold_conversation_body`, `release_conversation_body`, `touch_conversation_body`
- Selection: `active_workspace_id`, `active_conversation_id`, `set_active`, `select_conversation(ws, id)`, `open_conversation(id)`, `active_workspace`, `active_conversation_view` (clones; not for every frame on a long timeline), `draft_is_active`, `active_body_unloaded`, `composer_is_unsent_task`, `active_conversation_started`, `active_project_workspaces`, `active_registered_workspaces`, `active_conversation_workspaces`, `active_workspace_member`, `select_workspace_member`, `review_page_member`, `set_review_page_member`, `review_page_order`, `set_review_page_order`, `context_scroll`, `set_context_scroll`, `conversation_history_target(step)`, `step_conversation_history(step)`, `step_conversation(delta)`, `sidebar_conversation_status`, `draft_landing_tasks`
- Drafts: `open_draft_conversation(Option<ws>)`, `drafts`, `draft_of`, `draft_of_workspace`, `update_draft`, `materialize_draft`, `redeem_draft(persist, done)`, `promote_draft(done)`, `forget_draft`, `host_conversation_id`, `owner_works_in`, `draft_works_in`, `resolve_new_conversation_settings`
- Projects and machines: `add_workspace`, `update_project`, `delete_workspace`, `reorder_workspace`, `set_workspace_default_preset`, `workspace_dialog_open`, `set_workspace_dialog_open`, `project_editor`, `set_project_editor`, `pick_workspace_directory`, `list_remote_directory`, `authorize_remote_workspace`, `list_wsl_distros`, `machine_label`, `remote_link`, `remote_link_notice`, `machine_shells`, `probe_machine_shells`, `terminal_shells_on`
- Presets and templates: `update_global_settings`, `apply_preset_to_active_conversation`, `apply_preset_body`, `template_switch_needs_confirmation`, `template_switch_prompt`, `dismiss_template_switch_prompt`, `conversation_templates`, `refresh_conversation_templates`, `template_error`, `clear_template_error`, `read_template_body`, `write_template_body`, `create_conversation_preset`, `save_conversation_preset`, `save_conversation_preset_copy`, `rename_conversation_preset`, `delete_conversation_preset`, `bind_preset_template`, `rescan_capabilities`, `capability_error`, `set_capability_error`
- Layout and preferences: `side_panes`, `dispatch_side_panes(SidePanesAction)`, `ui_store`, `ui_store_update`, `ui_store_changed`, `flush_ui_store`, `sidebar_width`, `update_sidebar_width`, `write_catalog_order`, `should_warn_cache_break`, `acknowledge_cache_break`, `appearance_snapshot`, `publish_appearance`, `set_timer`, `clear_timer`

## crates/ui/settings-global (finished; signatures in its lib.rs header)
- `crates/mewrk/Cargo.toml`: depend on `mewrk-settings-global`.
- `root.rs::theme`: end with `mewrk_settings_global::themed(if dark { Theme::dark() } else { Theme::light() }, &app.doc.global_settings.appearance)`.
- `root.rs::root`: `window_backdrop(app)` as the first child and `global_settings_window(app)` as a child; `on_scheme_changed(app, dark)` each frame; open settings with `open(app, view)`; wire the zoom shortcuts.
- `main.rs` `Core::frame`: use `let scale = self.scale * zoom(&app)` for the logical size, `text.set_scale`, `Ui::new` and pointer conversion; call `self.text.set_font_families(&a.ui_font_family, &a.mono_font_family)`; on `WindowEvent::Focused(true)` after a blur, call `window_refocused(app)`.
- Chat: `message_type(&app)` as the `MdStream` base. Wide messages and code wrap/collapse are for the chat surface to read.
- `web_search_behavior.rs` pickers, lock tones and domain row are slots for `settings-conv`; `settings-conv/web.rs` should unify with `logic/web_search.rs`.

## crates/panes/terminal and crates/panes/git (finished; signatures in each lib.rs header)
- Call `terminal_pane` every frame with `pane_open: false` while closed: stopping the calls detaches the shells, like an upstream unmount.
- `app.invoke` doesn't preserve order (commands run concurrently). Anyone sending ordered commands must send them one at a time, as the terminal now does.
- Two read-only shell task pages exist: `mewrk_pane_tasks::shell_task_panel` and `mewrk_pane_terminal::shell_task_panel`. Pick one; `Terminal::feed` makes the tasks crate's own emulator (`ansi_view.rs`) unnecessary.
- `mewrk_pane_git::set_file_icon(mewrk_pane_files::icons::file_icon)`.
- Key `git_review_panel` by member plus `git_review_snapshot_cache_key(snapshot)`.
- `branch_chips` is the composer row's branch and worktree UI; `git_status_card` lives in the git crate.
- `path.rs` is duplicated in terminal and git, and repeats files' `elide_path`.

## crates/panes/browser (finished; signatures in its lib.rs header)
- `attach_window(app)` once at startup: without its heartbeat the host hides every page after 12 s.
- `present_page` and `hide_built_in_browser` when showing and hiding pages. `note_preview_pane_closed()` when a pane closes.
- `pages::servers_to_stop_for_closed_page` and `pages_served_by` with the roster. The tab roster stays with the shell.
- `StopHooks` in tasks: wire to `stopBrowserAutomation` etc. here.

## crates/ui/composer (finished; the shell's `composer_stub.rs` already fills `ComposerProps`)
- No callers yet outside the crate: `focus_composer` and `put_sent_message_in_composer`. The timeline's edit-message flow and the `actions.rs` TODO should call them.
- Two `image_strip` ports: `crates/ui/chat` built its own because the composer's takes `&mut App`. Pick one.
- `rolling.rs` copies the tasks pane's `rolling_number`.
- `landing::conversation_status_mark` waits for the sidebar to publish one.
- `MemberMemo` in `lib.rs` exists because `active_workspace_member` clones the conversation.
- `ElementPick` has no sequence or label, so chips parse the block and removal is by index.

## crates/mewrk (finished; every pane kind draws its real crate)
- `withoutProbe` needs a small accessor in `mewrk-client`; the one TODO, in `dialogs/machines.rs`.
- `crates/fluent/src/shaders.wgsl:153` cuts the shadow's hole with the offset rectangle, leaving a hard band under every dialog.
- `popover_menu` under-measures a wrapped section heading when its width comes only from `max_w`. The terminal menu pins its width for now.
- Chat crate: hover actions on a user message overlap the text.
- `template_editor.rs` still has its stand-in timeline.
- Second launch on the same data exits 1 when launched directly (instance signal is Windows-only). The bundle reopens through LaunchServices.
- Host `reset_document` never ran for real: `Host::start` banks a bad document itself.

## crates/ui/chat (finished; signatures in its lib.rs header)
- `Input.scroll_x` must be fed from the wheel, or tables only scroll by dragging the bar.
- `zoom_modifier` stands in for the selection modifier, so Ctrl on a Mac also starts a selection box.
- Drops need a pointer position during the drag.
- `CursorKind` has no crosshair, so a selection box keeps the default cursor. `el.rs` has no Tab order and no Enter on a focused button, so a row opens by pointer only.
- `crates/ui/common` has `[lib] test = false`, so the `#[cfg(test)]` modules in `page_tabs.rs` and `popover_menu.rs` stopped running.
- Taffy drops a row's `gap` when an auto margin takes free space. `mewrk_pane_tasks::widgets::fitted_label` draws spaces at zero width when pulsing and always elides.
- `serde_json` sorts keys here, so uncurated tool arguments list alphabetically (`preserve_order` would change every crate).
- Differences left: a code-block collapse clips; pictures in table cells show alt text; no `colspan`/`rowspan`; math is Unicode text, not typeset.

## crates/ui/settings-conv template editor (finished)
- Draws `mewrk_ui_chat::conversation_view` with upstream's prop list. Move, copy, paste, undo and fork are absent because upstream passes none there.
