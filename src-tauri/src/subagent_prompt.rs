//! The host-authored addendum every child agent's system prompt carries.
//!
//! # Why V1 is frozen
//!
//! V1's exact bytes are inside every conversation fork's persisted
//! `system_prompt_snapshot`. That snapshot is signed by
//! `MemoryStore::sign_fork_system_prompt_receipt` and, through the frozen
//! `ForkModelBindingV1` projection, is also one of the fields inside
//! `canonical_subagent_execution_mode_payload`. Neither receipt has an update
//! path, so editing V1 would not merely invalidate stored forks: for the
//! execution-mode receipt it would permanently burn the
//! `(conversation_id, name)` pair it was reserved under. V1 therefore never
//! changes for any reason — a new shape lands as a new variant.
//!
//! `PromptVersion::V1` deliberately carries no language. V1 renders Chinese
//! unconditionally, and making its bytes a function of anything would silently
//! change frozen output. Because the variant has no fields, that is enforced by
//! the type rather than by a comment. V2 is the first shape whose text is
//! selected: it is `subagent.addendum` of the run's prompt profile
//! (`prompt_profile::PromptKey::SubagentAddendum`), so the built-in profile
//! and a user-authored file each render exactly one variant — never two at
//! once, which would double the tokens on every child turn and double the bytes
//! inside a signed fork snapshot.
//!
//! V3 is V2 with two of its notes made conditional on the child's tools. The
//! browser note and the background-shell note used to be items of
//! `subagent.addendum`'s notes list; they are keys of their own now
//! (`subagent.addendum.browser_note`, `subagent.addendum.shell_note`), and V3
//! appends each — `"\n- "` and its text, browser first and shell last — only
//! when the child's final `enabled_tools` holds a tool it is about. The shell
//! note stays last so the list still ends on "Any command still running when
//! you give your final reply is stopped." V2 still renders `subagent.addendum`
//! alone, so it now means that addendum WITHOUT those two notes: the key's
//! built-in text lost them when they moved. Production no longer selects V2,
//! and a fork it rendered keeps its own snapshot, so nothing re-renders it.
//!
//! # The version selects only the addendum
//!
//! `render` never rewrites, summarises, truncates or inspects `base`. Every
//! version applies the same `str::trim` to the caller's prompt (Unicode
//! `White_Space`, so U+3000 and U+00A0 count) and the same
//! `\n\n---\n\n` section separator that `lib::append_system_prompt_section`
//! uses crate-wide. A `base` that already contains that separator, or that
//! quotes a memory delimiter literally, is passed through untouched: presence of
//! host-owned context is never inferred from arbitrary system-prompt text.
//!
//! # Deliberate product limitation
//!
//! `api::restore_conversation_fork_binding` overwrites the freshly rendered
//! template prompt with the persisted `system_prompt_snapshot`, which remains
//! authoritative for the rest of that fork's life. An existing conversation fork
//! therefore receives neither V2's instruction-source-boundary paragraph nor
//! V3's tool-dependent notes; the product does not re-render or re-sign a fork
//! on resume.
//!
//! Named and ordinary children are the opposite case:
//! `api::build_rehydrated_agent_template` re-renders them from the definition or
//! from the parent prompt on every resume, and the rendered text appears in
//! neither receipt payload (the definition receipt's `systemPrompt` is the raw
//! document prompt, and `AgentDefinitionBindingV1` has no prompt field at all),
//! so they pick a version bump up retroactively with no receipt impact.

use crate::prompt_profile::{PromptKey, PromptProfile};

/// Which subagent-prompt shape to render.
///
/// V1 has no fields on purpose: see the module doc. New shapes are added as new
/// variants; existing ones are never edited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PromptVersion {
    /// Frozen forever. Production never selects V1 — every fresh child renders
    /// the current shape — but the golden test does, and so would any future
    /// migration that has to reproduce a historical render byte-for-byte.
    #[cfg_attr(not(test), allow(dead_code))]
    V1,
    /// The instruction-source boundary shape, with its text taken from the
    /// run's prompt profile (`subagent.addendum`). Superseded by V3 and no
    /// longer selected in production; see the module doc for what it renders
    /// now that the two tool-dependent notes left that key.
    #[cfg_attr(not(test), allow(dead_code))]
    V2,
    /// V2's addendum, plus the browser and background-shell notes for a child
    /// that holds a tool each is about.
    V3,
}

impl PromptVersion {
    /// The shape every freshly spawned child renders.
    ///
    /// Changing what this returns is not a prompt-only edit. For a conversation
    /// fork, `api::agent_child_template` renders it into the child's
    /// `system_prompt`, `api::inherit_parent_model_memory_for_fork` snapshots
    /// that verbatim as `system_prompt_snapshot` and signs
    /// `system_prompt_receipt` over it, and BOTH are fields of the frozen
    /// `model::ForkModelBindingV1` projection — so this value reaches
    /// `model::canonical_subagent_execution_mode_payload` and the durable
    /// reservation keyed by `(conversation_id, name)`. Editing V3's own bytes
    /// has the same reach, and so does the child's tool set, which decides
    /// V3's notes. Named and ordinary children are unaffected: their
    /// rendered prompt appears in no receipt payload (see the module doc).
    /// `tests::the_current_prompt_version_is_pinned_by_the_fork_reservation`
    /// makes a change here fail loudly and states the consequence.
    pub(crate) fn current() -> Self {
        Self::V3
    }
}

/// FROZEN. Do not edit a single byte; see the module doc. Kept in the original
/// backslash-continued form it was moved out of `api::subagent_system_prompt`
/// in, so the move is provably byte-preserving. The continuation lines start at
/// column 0 because Rust's `\`-newline escape also eats the next line's leading
/// whitespace.
const ADDENDUM_V1_ZH: &str =
    "你是主代理派生的子代理。专注完成给定任务；除任务描述（以及派生时可能附带的对话历史副本）外，\
你看不到主对话的其他内容。主代理可能随时发来新的用户消息补充指令。\
可以使用更新工具向主代理报告重要进展；仍需用最后一条回复输出完整结论。";

fn addendum(version: PromptVersion, profile: &PromptProfile, enabled_tools: &[String]) -> String {
    match version {
        PromptVersion::V1 => ADDENDUM_V1_ZH.to_owned(),
        PromptVersion::V2 => profile.text(PromptKey::SubagentAddendum).to_owned(),
        PromptVersion::V3 => {
            let mut addendum = profile.text(PromptKey::SubagentAddendum).to_owned();
            // The notes are items of the addendum's own list, so a profile that
            // emptied the addendum has no list for them to join.
            if addendum.is_empty() {
                return addendum;
            }
            for key in tool_notes(enabled_tools) {
                let note = profile.text(key);
                // A file written before the notes became keys of their own may
                // still carry one inside its addendum; it is not said twice.
                if !note.is_empty() && !addendum.contains(note) {
                    addendum.push_str("\n- ");
                    addendum.push_str(note);
                }
            }
            addendum
        }
    }
}

/// The tool-dependent notes V3 appends for a child holding `enabled_tools`, in
/// the order it appends them: the browser note for any tool acting on the
/// conversation's browser session, the shell note — last — for any shell
/// command tool, which is also what derives the `task_wait` that note names
/// (`agents::apply_task_runtime_tools`).
fn tool_notes(enabled_tools: &[String]) -> impl Iterator<Item = PromptKey> {
    let holds = |test: fn(&str) -> bool| enabled_tools.iter().any(|name| test(name));
    let browser = holds(crate::browser::is_browser_session_tool);
    let shell = holds(|name| crate::shell_backend::ShellBackend::of_tool(name).is_some());
    [
        (browser, PromptKey::SubagentAddendumBrowserNote),
        (shell, PromptKey::SubagentAddendumShellNote),
    ]
    .into_iter()
    .filter_map(|(applies, key)| applies.then_some(key))
}

/// Appends the host-authored child addendum to `base`.
///
/// Infallible on purpose: unlike a signed payload, a prompt has no persisted
/// version that could name a shape this build cannot produce, so there is no
/// error to report. `base` is the caller's own prompt — a named definition's
/// document prompt or the parent conversation's assembled prompt — and is only
/// trimmed, never rewritten. `enabled_tools` is the child's final tool set,
/// after every host adjustment and allowlist; only V3 reads it.
pub(crate) fn render(
    version: PromptVersion,
    base: &str,
    profile: &PromptProfile,
    enabled_tools: &[String],
) -> String {
    let addendum = addendum(version, profile, enabled_tools);
    // Model-owned and project memory now live only in host-owned ephemeral
    // contexts. Never infer their presence from arbitrary system-prompt text:
    // a user is allowed to quote either delimiter literally.
    let base = base.trim();
    if base.is_empty() {
        addendum
    } else {
        format!("{base}\n\n---\n\n{addendum}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frozen V1 addendum, written here as ONE unbroken literal so the test
    /// is independent of how the production constant is line-wrapped. This is a
    /// golden, not a re-derivation: never regenerate it from `render`.
    const V1_GOLDEN: &str = "你是主代理派生的子代理。专注完成给定任务；除任务描述（以及派生时可能附带的对话历史副本）外，你看不到主对话的其他内容。主代理可能随时发来新的用户消息补充指令。可以使用更新工具向主代理报告重要进展；仍需用最后一条回复输出完整结论。";

    const SEPARATOR: &str = "\n\n---\n\n";

    /// A child holding both kinds of tool the V3 notes are about.
    fn all_note_tools() -> Vec<String> {
        ["read", "preview_click", "bash", "task_wait"].map(String::from).to_vec()
    }

    /// A child holding neither.
    fn no_note_tools() -> Vec<String> {
        ["read", "edit", "web_search"].map(String::from).to_vec()
    }

    /// Tripwire, not a property — the sibling of
    /// `api::tests::receipt_payload_dispatchers_support_exactly_their_issued_versions`.
    ///
    /// It lives here rather than folded into that test on purpose: a prompt
    /// version is not a receipt-issuance version (nothing dispatches a payload
    /// on it), its blast radius is fork-only, and this module is where someone
    /// who wants V4 will actually be editing. The two doc comments cross-refer
    /// so neither can be found without the other.
    ///
    /// The pin protects fork-wide receipt re-issuance. `run_agent_spawn` reads
    /// `MemoryStore::reserved_subagent_execution_mode_names` into its dedupe gate,
    /// so an orphaned row cannot be re-selected by `auto_name`, and an explicit
    /// request for one receives the ordinary "pick another name" message.
    /// Changing this value still requires a deliberate decision: an interrupted
    /// fork can retain a reservation under the previous bytes.
    #[test]
    fn the_current_prompt_version_is_pinned_by_the_fork_reservation() {
        const WHY: &str = "\
PromptVersion::current no longer selects V3. That may be right, but it is not a prompt-only edit.\n\
api::agent_child_template renders this version into the child's system_prompt; for a conversation \
fork, api::inherit_parent_model_memory_for_fork snapshots it verbatim as system_prompt_snapshot and \
signs system_prompt_receipt over it, and BOTH are fields of the frozen model::ForkModelBindingV1 \
projection. So this value moves canonical_subagent_execution_mode_payload's bytes, and \
memory::MemoryStore::reserve_subagent_execution_mode_receipt has insert / identical-no-op / \
hard-error branches only — no UPDATE, no production DELETE. Each selected version therefore \
reserves forks under its exact bytes and differs from existing reservations under other bytes.\n\
Editing V3's own bytes has the same effect as changing the selected variant. Named and ordinary \
children are NOT affected — their prompt is in no receipt payload.\n\
api::run_agent_spawn reads memory::MemoryStore::reserved_subagent_execution_mode_names into its \
dedupe gate, so a name whose reservation row outlived its persisted record remains visible: auto_name \
skips it and an explicit request for it gets the ordinary rename advice. Do NOT read that as permission \
to churn this value: a fork that was interrupted mid-turn still loses its name to the change.\n\
WHAT TO DO: add the new variant, confirm the gate change above is still in place \
(api::tests::orphan_reservation_rows_are_visible_to_the_spawn_dedupe_gate), then re-pin here.";
        assert_eq!(PromptVersion::current(), PromptVersion::V3, "{WHY}");
    }

    #[test]
    fn v1_addendum_is_byte_frozen() {
        assert_eq!(ADDENDUM_V1_ZH, V1_GOLDEN);
        assert_eq!(V1_GOLDEN.len(), 342);
        assert_eq!(V1_GOLDEN.chars().count(), 114);
    }

    /// The V3 addenda are not merely product copy: `agent_child_template` renders
    /// them into `system_prompt`, `inherit_parent_model_memory_for_fork`
    /// snapshots that verbatim as `system_prompt_snapshot`, and that field is
    /// projected by `model::ForkModelBindingV1` into the canonical execution-mode
    /// payload. So editing one word here moves the reserved bytes for every fork
    /// issued afterwards, and a conversation holding an orphaned reservation row
    /// under the old bytes can no longer re-reserve that name.
    ///
    /// The other guards do not cover this. `PromptVersion::current` is pinned to
    /// the VARIANT, not to the text it selects, and the render tests assert
    /// substrings. Without these counts a one-word edit passes the whole suite.
    /// The counts are taken with both notes applying, so they cover the two
    /// note keys as well as `subagent.addendum`.
    ///
    /// This is a tripwire, not a freeze: unlike `ADDENDUM_V1_ZH` these strings MAY
    /// be edited. Re-run, take the reported lengths, and update them here in the
    /// same commit — the point is that the change is deliberate and reviewed
    /// against the reservation consequence, not that it never happens.
    #[test]
    fn v3_addenda_are_byte_pinned() {
        const WHY: &str = "V3 addendum bytes reach `system_prompt_snapshot`, which \
`ForkModelBindingV1` projects into the canonical execution-mode payload. Changing \
them re-reserves every new fork under different bytes. If this edit is intended, \
update the counts here in the same commit; see the module doc and \
`model::canonical_subagent_execution_mode_payload`.";

        let english = addendum(
            PromptVersion::V3,
            &PromptProfile::builtin_english(),
            &all_note_tools(),
        );
        assert_eq!(english.len(), 2324, "{WHY}");
        assert_eq!(english.chars().count(), 2316, "{WHY}");
    }

    /// Without a shell tool there is nothing to run in the background and no
    /// `task_wait` to wait with, so neither the note nor that name appears.
    #[test]
    fn v3_leaves_the_shell_note_to_a_child_with_a_shell() {
        let browser_only = ["read", "preview_screenshot"].map(String::from).to_vec();
        for profile in PromptProfile::builtins() {
            for tools in [browser_only.clone(), no_note_tools()] {
                let rendered = render(PromptVersion::V3, "Be useful.", &profile, &tools);
                let note = profile.text(PromptKey::SubagentAddendumShellNote);
                assert!(!note.is_empty());
                assert!(!rendered.contains(note), "{rendered}");
                assert!(!rendered.contains("task_wait"), "{rendered}");
                assert!(!rendered.contains("run_in_background"), "{rendered}");
                assert!(!rendered.contains("Any command still running"), "{rendered}");
            }
        }
    }

    #[test]
    fn v3_leaves_the_browser_note_to_a_child_with_a_browser_tool() {
        let shell_only = ["read", "powershell", "task_wait"].map(String::from).to_vec();
        for profile in PromptProfile::builtins() {
            for tools in [shell_only.clone(), no_note_tools()] {
                let rendered = render(PromptVersion::V3, "Be useful.", &profile, &tools);
                let note = profile.text(PromptKey::SubagentAddendumBrowserNote);
                assert!(!note.is_empty());
                assert!(!rendered.contains(note), "{rendered}");
                assert!(!rendered.contains("browser session"), "{rendered}");
            }
            // Neither applies: the addendum is `subagent.addendum` alone.
            assert_eq!(
                addendum(PromptVersion::V3, &profile, &no_note_tools()),
                profile.text(PromptKey::SubagentAddendum)
            );
        }
    }

    /// Both apply: each is one more item of the notes list, browser first, and
    /// the shell note — with the sentence about commands still running — last.
    #[test]
    fn v3_appends_both_notes_with_the_shell_note_last() {
        // Any `preview_*` tool and any shell backend count, not just one name.
        let tool_sets = [
            all_note_tools(),
            ["preview_start", "zsh"].map(String::from).to_vec(),
            ["sh", "preview_list", "read"].map(String::from).to_vec(),
        ];
        for profile in PromptProfile::builtins() {
            let browser = profile.text(PromptKey::SubagentAddendumBrowserNote);
            let shell = profile.text(PromptKey::SubagentAddendumShellNote);
            for tools in &tool_sets {
                let rendered = addendum(PromptVersion::V3, &profile, tools);
                assert_eq!(
                    rendered,
                    format!(
                        "{}\n- {browser}\n- {shell}",
                        profile.text(PromptKey::SubagentAddendum)
                    )
                );
                assert!(rendered.contains("\n\nNotes:\n- "));
                assert!(rendered.ends_with(
                    "Any command still running when you give your final reply is stopped."
                ));
            }
            let shell_only = addendum(PromptVersion::V3, &profile, &["bash".to_owned()]);
            assert!(shell_only.ends_with(&format!("\n- {shell}")));
            let browser_only = addendum(PromptVersion::V3, &profile, &["preview_eval".to_owned()]);
            assert!(browser_only.ends_with(&format!("\n- {browser}")));
        }
    }

    /// A user file written before the notes were keys of their own overrides
    /// the addendum with text that still carries them: a note it already says
    /// is not appended a second time, and one it lacks still is.
    #[test]
    fn v3_does_not_repeat_a_note_the_addendum_already_carries() {
        let guided = PromptProfile::builtin_english();
        let shell = guided.text(PromptKey::SubagentAddendumShellNote).to_owned();
        let browser = guided.text(PromptKey::SubagentAddendumBrowserNote).to_owned();
        let older = format!("{}\n- {shell}", guided.text(PromptKey::SubagentAddendum));
        let file = PromptProfile::from_file(
            "test".into(),
            "Test".into(),
            crate::model::ResolvedLanguage::EnUs,
            [(PromptKey::SubagentAddendum, older.clone())].into(),
            Vec::new(),
        );
        let rendered = addendum(PromptVersion::V3, &file, &all_note_tools());
        assert_eq!(rendered.matches(shell.as_str()).count(), 1);
        assert_eq!(rendered, format!("{older}\n- {browser}"));
    }

    /// A profile that empties a note key drops that note; one that empties the
    /// addendum itself drops the notes with it, since they are its list's items.
    #[test]
    fn v3_appends_no_empty_note_and_no_note_to_an_empty_addendum() {
        let file = |overrides: &[(PromptKey, &str)]| {
            PromptProfile::from_file(
                "test".into(),
                "Test".into(),
                crate::model::ResolvedLanguage::EnUs,
                overrides
                    .iter()
                    .map(|(key, text)| (*key, (*text).to_owned()))
                    .collect(),
                Vec::new(),
            )
        };
        let no_shell_note = file(&[(PromptKey::SubagentAddendumShellNote, "")]);
        assert_eq!(
            addendum(PromptVersion::V3, &no_shell_note, &all_note_tools()),
            format!(
                "{}\n- {}",
                no_shell_note.text(PromptKey::SubagentAddendum),
                no_shell_note.text(PromptKey::SubagentAddendumBrowserNote)
            )
        );
        let no_addendum = file(&[(PromptKey::SubagentAddendum, "")]);
        assert_eq!(addendum(PromptVersion::V3, &no_addendum, &all_note_tools()), "");
    }

    /// V1 and V2 render the same bytes whatever the child holds: only V3 reads
    /// the tool set. V2 now lacks both notes, which moved out of its key.
    #[test]
    fn only_v3_reads_the_tool_set() {
        for profile in PromptProfile::builtins() {
            for version in [PromptVersion::V1, PromptVersion::V2] {
                assert_eq!(
                    render(version, "Be useful.", &profile, &all_note_tools()),
                    render(version, "Be useful.", &profile, &[])
                );
            }
            let v2 = render(PromptVersion::V2, "", &profile, &all_note_tools());
            assert!(!v2.contains(profile.text(PromptKey::SubagentAddendumBrowserNote)));
            assert!(!v2.contains(profile.text(PromptKey::SubagentAddendumShellNote)));
        }
        assert!(render(PromptVersion::V1, "", &PromptProfile::builtin_english(), &all_note_tools())
            .ends_with(V1_GOLDEN));
    }

    /// Acceptance (c): `render(V1, x)` is byte-identical to the pre-change
    /// `api::subagent_system_prompt(x)` across empty, whitespace-only, short and
    /// multi-KB inputs. Every expectation is spelled out from the golden.
    #[test]
    fn v1_render_matches_the_pre_change_subagent_system_prompt() {
        let long_ascii = "x".repeat(4096);
        let long_multibyte = "阿".repeat(4096);
        let cases: Vec<(String, String)> = vec![
            // Empty and whitespace-only collapse to the bare addendum with no
            // separator on either side.
            (String::new(), V1_GOLDEN.to_owned()),
            ("   ".to_owned(), V1_GOLDEN.to_owned()),
            ("\n\t \r\n".to_owned(), V1_GOLDEN.to_owned()),
            // `str::trim` is Unicode `White_Space`: IDEOGRAPHIC SPACE and
            // NO-BREAK SPACE are trimmed too. Do not "modernize" this to ASCII.
            ("\u{3000}\u{00a0}".to_owned(), V1_GOLDEN.to_owned()),
            (
                "Be useful.".to_owned(),
                format!("Be useful.{SEPARATOR}{V1_GOLDEN}"),
            ),
            // Both ends are trimmed, and the inner text is untouched.
            (
                "  Be useful.  ".to_owned(),
                format!("Be useful.{SEPARATOR}{V1_GOLDEN}"),
            ),
            (
                "中文提示词\n".to_owned(),
                format!("中文提示词{SEPARATOR}{V1_GOLDEN}"),
            ),
            // A base that already contains the separator is passed through
            // verbatim: there is no de-duplication and never was.
            (
                "before\n\n---\n\nafter".to_owned(),
                format!("before\n\n---\n\nafter{SEPARATOR}{V1_GOLDEN}"),
            ),
            (
                long_ascii.clone(),
                format!("{long_ascii}{SEPARATOR}{V1_GOLDEN}"),
            ),
            (
                long_multibyte.clone(),
                format!("{long_multibyte}{SEPARATOR}{V1_GOLDEN}"),
            ),
        ];
        for (base, expected) in cases {
            // With every tool V3 words a note for: V1 ignores the tool set.
            let rendered = render(
                PromptVersion::V1,
                &base,
                &PromptProfile::builtin_english(),
                &all_note_tools(),
            );
            assert_eq!(
                rendered.as_bytes(),
                expected.as_bytes(),
                "V1 render drifted for a {}-byte base",
                base.len()
            );
        }
        // The multi-KB cases really are multi-KB.
        assert_eq!(long_ascii.len(), 4096);
        assert_eq!(long_multibyte.len(), 12288);
    }

    #[test]
    fn v1_carries_none_of_v2s_additions() {
        let v1 = render(
            PromptVersion::V1,
            "Be useful.",
            &PromptProfile::builtin_english(),
            &all_note_tools(),
        );
        for marker in [
            "指令来源边界",
            "注意事项：",
            "Instruction-source boundary",
            "Notes:",
            "browser session",
            "task_wait",
        ] {
            assert!(
                !v1.contains(marker),
                "V1 must not have grown {marker:?}; V1 is frozen"
            );
        }
        assert_eq!(v1.len(), "Be useful.".len() + SEPARATOR.len() + 342);
    }

    #[test]
    fn v2_english_is_a_separate_variant_not_a_second_copy() {
        let profile = PromptProfile::builtin_english();
        for version in [PromptVersion::V2, PromptVersion::V3] {
            let en = render(version, "", &profile, &all_note_tools());
            assert!(en.starts_with("You are a child agent spawned by the main agent."));
            assert!(en.contains("Instruction-source boundary:"));
            assert!(en.contains("\n\nNotes:\n- "));
            assert!(en.contains("you have no tool for asking"));
            assert!(en.contains("cannot spawn or direct further child agents"));
            assert!(en.contains("unless the host assigned you a partition of your own"));
            assert!(en.contains("You have no round or time limit"));
            // V2 replaces V1's Chinese paragraph rather than following it, and
            // V3 replaces V2's: emitting both would double every child turn's
            // prompt tokens.
            assert!(!en.contains("你是主代理派生的子代理"));
            assert!(!en.contains("注意事项"));
            assert_eq!(en.matches("Instruction-source boundary:").count(), 1);
        }
        let v3 = render(PromptVersion::V3, "", &profile, &all_note_tools());
        assert!(v3.contains("browser session and web authorization are shared"));
        assert!(v3.ends_with("Any command still running when you give your final reply is stopped."));
    }

    /// The version selects the addendum and nothing else: `base` reaches the
    /// output byte-for-byte (after the shared trim) under every version.
    #[test]
    fn no_version_rewrites_the_caller_prompt() {
        let bases = [
            "Be useful.",
            "  padded  ",
            "中文提示词",
            "before\n\n---\n\nafter",
            "指令来源边界：a user is allowed to quote this",
            "<mewrk-memory-context>\nquoted delimiter\n</mewrk-memory-context>",
        ];
        let file = PromptProfile::from_file(
            "test".into(),
            "Test".into(),
            crate::model::ResolvedLanguage::ZhCn,
            [(PromptKey::SubagentAddendum, "子代理附录".to_owned())].into(),
            Vec::new(),
        );
        for (version, profile, tools) in [
            (PromptVersion::V1, PromptProfile::builtin_english(), all_note_tools()),
            (PromptVersion::V2, file.clone(), all_note_tools()),
            (PromptVersion::V2, PromptProfile::builtin_english(), all_note_tools()),
            (PromptVersion::V3, file, all_note_tools()),
            (PromptVersion::V3, PromptProfile::builtin_english(), all_note_tools()),
            (PromptVersion::V3, PromptProfile::builtin_english(), no_note_tools()),
            (PromptVersion::V3, PromptProfile::builtin_concise(), all_note_tools()),
            (PromptVersion::V3, PromptProfile::builtin_concise(), no_note_tools()),
        ] {
            let tail = render(version, "", &profile, &tools);
            for base in bases {
                let rendered = render(version, base, &profile, &tools);
                assert_eq!(rendered, format!("{}{SEPARATOR}{tail}", base.trim()));
                // Nothing was inserted into, removed from, or reordered inside
                // the caller's own text.
                assert!(rendered.starts_with(base.trim()));
                assert!(rendered.ends_with(&tail));
            }
            // Whitespace-only input yields the addendum alone under every
            // version, with no dangling separator.
            assert_eq!(render(version, " \u{3000}\n", &profile, &tools), tail);
            assert!(!tail.starts_with('\n'));
            // `api::combined_system_prompt` trims the wire prompt, so a
            // trailing newline here would diverge from the signed snapshot.
            assert!(!tail.ends_with('\n'));
        }
    }
}
