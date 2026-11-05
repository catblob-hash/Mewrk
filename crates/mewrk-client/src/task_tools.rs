//! Which catalog tools are host-derived, produce tasks or act on the preview (lib/taskTools.ts, lib/memoryTools.ts).

use std::collections::HashSet;

/// The six long-term memory tools. Governed by the two memory-layer toggles rather than the enabled-tool list: the host
/// injects them per enabled layer, so the renderer keeps them out of selection and persisted lists. Mirrors Rust
/// `mewrk_memory::MEMORY_TOOL_NAMES`.
pub const MEMORY_TOOL_NAMES: [&str; 6] = [
    "read_global_memory",
    "read_project_memory",
    "create_global_memory",
    "create_project_memory",
    "edit_global_memory",
    "edit_project_memory",
];

/// Upstream's `MEMORY_TOOL_NAME_SET.has(name)`.
pub fn is_memory_tool_name(name: &str) -> bool {
    MEMORY_TOOL_NAMES.contains(&name)
}

/// Task-runtime tools. Host-derived: conversations that produce tasks must also be able to list, await and stop them,
/// and `box` carries the host's messages wherever a conversation chose it as their container, so every run of such a
/// conversation declares it. Mirrors Rust `agents::TASK_RUNTIME_TOOL_NAMES`.
const TASK_RUNTIME_TOOL_NAMES: [&str; 4] = ["task_wait", "task_list", "task_stop", "box"];

/// The tools that can put a row in the task list. Only a system prompt assembled for one of them says how background
/// results and the host's messages arrive. Mirrors Rust `agents::TASK_PRODUCING_TOOL_NAMES`.
const TASK_PRODUCING_TOOL_NAMES: [&str; 8] =
    ["agent_spawn", "workflow", "bash", "zsh", "sh", "pwsh", "powershell", "preview_start"];

/// Whether this tool can put a row in the task list.
pub fn produces_tasks(name: &str) -> bool {
    TASK_PRODUCING_TOOL_NAMES.contains(&name)
}

/// The tool the subagent-communication switch grants; never a picker row or in a persisted list. Mirrors Rust
/// `api::SEND_MESSAGE_TOOL`.
const SUBAGENT_MESSAGING_TOOL_NAME: &str = "send_message";

/// The preview tools that bring the conversation's page into existence.
///
/// `preview_stop`, `preview_list` and `preview_logs` only read or kill a process, so they never produce a page.
/// `preview_input` and the screen tools that drive or look at a screen are listed because a call of them is the model
/// using the conversation's preview surface; the ones that read a clipboard, package data or the device log leave the
/// picture alone, so they don't open a pane. A page roster, not a task roster: only `preview_start` also appears in
/// Rust `agents::TASK_PRODUCING_TOOL_NAMES`, because only it starts the process a `preview:<serverId>` row stands for.
const PREVIEW_PAGE_TOOL_NAMES: [&str; 18] = [
    "preview_start",
    "preview_console_logs",
    "preview_screenshot",
    "preview_snapshot",
    "preview_inspect",
    "preview_click",
    "preview_fill",
    "preview_eval",
    "preview_network",
    "preview_resize",
    "preview_upload_image",
    "preview_dialog",
    "preview_input",
    "preview_zoom",
    "preview_wait",
    "screen_app",
    "screen_device",
    "android_shell",
];

/// Whether a call of this tool brings the conversation's page into existence.
pub fn is_preview_page_tool_name(name: &str) -> bool {
    PREVIEW_PAGE_TOOL_NAMES.contains(&name)
}

/// The tools that act only on a remote-control screen. The `screen_` and `android_` names belong to a screen and not to
/// the preview pane, but they act on the screens `preview_start` connects, so everything keyed to the preview tools
/// follows them. `preview_input`, `preview_dialog` and the rest also reach screens, but are page tools first. Mirrors
/// Rust `tool_executor::screen_tools::SCREEN_TOOLS`.
pub const SCREEN_TOOL_NAMES: [&str; 8] = [
    "preview_zoom",
    "preview_wait",
    "screen_app",
    "screen_device",
    "screen_clipboard",
    "screen_package",
    "android_logcat",
    "android_shell",
];

/// Whether this tool acts only on a remote-control screen.
pub fn is_screen_tool_name(name: &str) -> bool {
    SCREEN_TOOL_NAMES.contains(&name)
}

/// Every catalog tool that acts on the conversation's preview: page, server and screen tools alike.
///
/// The prefixes are the test rather than a list, so a tool added under either is covered the day it lands; the two
/// Android-only tools have to be named, since the `android_` prefix is no promise of a screen tool.
pub fn is_preview_tool_name(name: &str) -> bool {
    name.starts_with("preview_") || name.starts_with("screen_") || is_screen_tool_name(name)
}

/// The preview tools that start, stop and list dev servers. The picker has no row for them: they are on exactly while
/// any other preview tool is, because every other one needs a server and none of the three helps a model without one.
const PREVIEW_LIFECYCLE_TOOL_NAMES: [&str; 3] = ["preview_start", "preview_stop", "preview_list"];

/// Whether this tool starts, stops or lists dev servers.
pub fn is_preview_lifecycle_tool_name(name: &str) -> bool {
    PREVIEW_LIFECYCLE_TOOL_NAMES.contains(&name)
}

/// `enabled_tools` with the lifecycle tools in step with the other preview tools: each one in `available` added while
/// any other preview tool is on, all of them taken off otherwise.
pub fn with_preview_lifecycle_tools(enabled_tools: &[String], available: &HashSet<String>) -> Vec<String> {
    let preview_on = enabled_tools.iter().any(|name| is_preview_tool_name(name) && !is_preview_lifecycle_tool_name(name));
    if !preview_on {
        return enabled_tools.iter().filter(|name| !is_preview_lifecycle_tool_name(name)).cloned().collect();
    }
    let lifecycle = PREVIEW_LIFECYCLE_TOOL_NAMES.iter().filter(|name| available.contains(**name)).map(|name| (*name).to_owned());
    unique_names(enabled_tools.iter().cloned().chain(lifecycle))
}

/// `[...new Set(names)]`: the first occurrence of each, in order.
pub(crate) fn unique_names(names: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    names.into_iter().filter(|name| seen.insert(name.clone())).collect()
}

/// Derived from `skill_tool_enabled` and the number of resolved skills.
const SKILL_TOOL_NAME: &str = "skill";

/// Derived from `mcp_tool_discovery_enabled` and the number of tools a run withheld. Mirrors Rust
/// `capabilities::TOOL_SEARCH_TOOL`.
const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";

/// Derived from the conversation's Plan mode switch, so neither is a picker row.
const PLAN_TOOL_NAMES: [&str; 2] = ["plan", "exit_plan_mode"];

/// Derived once the context crosses the auto-compact threshold (`read_handoff_note` also for a conversation that
/// inherited notes), so none is a user setting. Mirrors Rust `handoff::TOOL_NAMES`.
const HANDOFF_TOOL_NAMES: [&str; 4] = ["read_handoff_note", "create_handoff_note", "edit_handoff_note", "handoff"];

/// The two web tools. They follow one web-access switch, because upstreams don't agree on how many web tools there are:
/// DeepSeek and OpenAI keep page retrieval inside search, Anthropic separates them. The host decides from the resolved
/// backend, so offering them individually would promise a shape the upstream may not have. Mirrors Rust
/// `web_search::WEB_TOOL_NAMES`.
const WEB_TOOL_NAMES: [&str; 2] = ["web_search", "web_fetch"];

/// Whether this is one of the two web tools.
pub fn is_web_tool_name(name: &str) -> bool {
    WEB_TOOL_NAMES.contains(&name)
}

/// Host-derived tools never enter persisted enabled lists: each follows its own switch, or the conversation's context
/// for the handoff tools. Normalization and preset application share this rule.
pub fn is_host_derived_tool_name(name: &str) -> bool {
    is_memory_tool_name(name)
        || TASK_RUNTIME_TOOL_NAMES.contains(&name)
        || PLAN_TOOL_NAMES.contains(&name)
        || HANDOFF_TOOL_NAMES.contains(&name)
        || WEB_TOOL_NAMES.contains(&name)
        || name == SUBAGENT_MESSAGING_TOOL_NAME
        || name == SKILL_TOOL_NAME
        || name == TOOL_SEARCH_TOOL_NAME
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ToolCategory;
    use crate::tool_defaults::test_catalog::tool_catalog;

    const LIFECYCLE: [&str; 3] = ["preview_start", "preview_stop", "preview_list"];

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn available() -> HashSet<String> {
        tool_catalog().into_iter().map(|tool| tool.name).collect()
    }

    fn lifecycle_after(enabled: &[&str]) -> Vec<String> {
        with_preview_lifecycle_tools(&names(enabled), &available())
    }

    mod screen_tool_names {
        use super::*;

        #[test]
        fn is_exactly_the_screen_and_android_groups_of_the_catalog_in_the_hosts_order() {
            assert_eq!(SCREEN_TOOL_NAMES.iter().collect::<HashSet<_>>().len(), 8);
            let grouped: Vec<String> = tool_catalog()
                .into_iter()
                .filter(|tool| matches!(tool.category, ToolCategory::Screen | ToolCategory::Android))
                .map(|tool| tool.name)
                .collect();
            assert_eq!(names(&SCREEN_TOOL_NAMES), grouped);
            for name in SCREEN_TOOL_NAMES {
                let (head, tail) = name.split_once('_').unwrap();
                assert!(matches!(head, "preview" | "screen" | "android"), "{name}");
                assert!(!tail.is_empty() && tail.bytes().all(|byte| byte.is_ascii_lowercase()), "{name}");
                assert!(is_screen_tool_name(name), "{name}");
            }
        }

        #[test]
        fn names_no_page_tool_and_no_other_catalog_tool() {
            for name in [
                "preview_input", "preview_dialog", "preview_screenshot", "bash", "read", "agent_spawn",
                "android", "android_unknown", "screen", "screen_find",
            ] {
                assert!(!is_screen_tool_name(name), "{name}");
            }
        }

        #[test]
        fn are_neither_host_derived_nor_task_producing_the_person_switches_them_on_and_nothing_waits_on_them() {
            for name in SCREEN_TOOL_NAMES {
                assert!(!is_host_derived_tool_name(name), "{name}");
                assert!(!produces_tasks(name), "{name}");
            }
        }
    }

    mod preview_tool_name {
        use super::*;

        #[test]
        fn covers_every_preview_and_screen_tool_by_its_prefix_and_the_two_android_only_tools_by_name() {
            for tool in tool_catalog() {
                if tool.name.starts_with("preview_") || tool.name.starts_with("screen_") {
                    assert!(is_preview_tool_name(&tool.name), "{}", tool.name);
                }
            }
            for name in SCREEN_TOOL_NAMES {
                assert!(is_preview_tool_name(name), "{name}");
            }
            assert!(is_preview_tool_name("android_logcat"));
            assert!(is_preview_tool_name("android_shell"));
            // A tool added later under either prefix is covered the day it lands.
            assert!(is_preview_tool_name("preview_future_tool"));
            assert!(is_preview_tool_name("screen_find"));
        }

        #[test]
        fn leaves_every_other_tool_alone_whatever_it_is_named_after() {
            for name in [
                "read", "bash", "web_search", "agent_spawn", "workflow",
                "android_future_tool", "ios", "desktop_", "screenshot", "screen", "mcp__x__screen_app__0f",
            ] {
                assert!(!is_preview_tool_name(name), "{name}");
            }
        }
    }

    mod preview_page_tool_name {
        use super::*;

        #[test]
        fn lists_the_screen_tools_that_drive_a_screen_or_look_at_it_so_a_call_opens_the_pane_and_shows_the_model_driving() {
            for name in ["preview_zoom", "preview_wait", "screen_app", "screen_device", "android_shell"] {
                assert!(is_preview_page_tool_name(name), "{name}");
            }
        }

        #[test]
        fn leaves_out_the_screen_tools_that_read_a_clipboard_packages_or_the_device_log() {
            for name in ["screen_clipboard", "screen_package", "android_logcat"] {
                assert!(!is_preview_page_tool_name(name), "{name}");
            }
        }

        #[test]
        fn keeps_the_preview_roster_as_it_was_page_tools_and_input_in_process_tools_out() {
            for name in [
                "preview_start", "preview_console_logs", "preview_screenshot", "preview_snapshot", "preview_inspect",
                "preview_click", "preview_fill", "preview_eval", "preview_network", "preview_resize",
                "preview_upload_image", "preview_dialog", "preview_input",
            ] {
                assert!(is_preview_page_tool_name(name), "{name}");
            }
            for name in ["preview_stop", "preview_list", "preview_logs", "read", "bash"] {
                assert!(!is_preview_page_tool_name(name), "{name}");
            }
        }
    }

    mod preview_lifecycle_tools {
        use super::*;

        #[test]
        fn brings_the_lifecycle_tools_along_with_a_screen_tool_as_it_does_with_a_preview_tool() {
            for name in SCREEN_TOOL_NAMES {
                assert_eq!(lifecycle_after(&["read", name]), names(&["read", name, LIFECYCLE[0], LIFECYCLE[1], LIFECYCLE[2]]), "{name}");
            }
            assert_eq!(
                lifecycle_after(&["read", "preview_click"]),
                names(&["read", "preview_click", LIFECYCLE[0], LIFECYCLE[1], LIFECYCLE[2]])
            );
        }

        #[test]
        fn takes_the_lifecycle_tools_off_when_the_last_screen_tool_or_preview_tool_goes() {
            let [start, stop, list] = LIFECYCLE;
            assert_eq!(lifecycle_after(&["read", start, stop, list]), names(&["read"]));
            assert_eq!(
                lifecycle_after(&["android_shell", "preview_click", start, stop, list]),
                names(&["android_shell", "preview_click", start, stop, list])
            );
            assert_eq!(lifecycle_after(&["android_shell", start, stop, list]), names(&["android_shell", start, stop, list]));
            assert_eq!(lifecycle_after(&LIFECYCLE), names(&[]));
        }

        #[test]
        fn does_not_count_the_lifecycle_tools_as_a_reason_to_keep_themselves() {
            assert!(LIFECYCLE.iter().all(|name| is_preview_tool_name(name)));
            assert!(LIFECYCLE.iter().all(|name| is_preview_lifecycle_tool_name(name)));
            assert_eq!(lifecycle_after(&["preview_start"]), names(&[]));
        }

        #[test]
        fn adds_only_the_lifecycle_tools_the_catalog_offers_and_never_one_twice() {
            let offered: HashSet<String> = names(&["screen_app", "preview_start", "preview_list"]).into_iter().collect();
            assert_eq!(
                with_preview_lifecycle_tools(&names(&["screen_app"]), &offered),
                names(&["screen_app", "preview_start", "preview_list"])
            );
            assert_eq!(
                lifecycle_after(&["preview_wait", "preview_stop", "preview_zoom"]),
                names(&["preview_wait", "preview_stop", "preview_zoom", "preview_start", "preview_list"])
            );
        }
    }

    /// Not an upstream case: every derived family is named once here.
    #[test]
    fn keeps_every_derived_family_out_of_persisted_lists() {
        for name in MEMORY_TOOL_NAMES {
            assert!(is_memory_tool_name(name) && is_host_derived_tool_name(name), "{name}");
        }
        for name in ["task_wait", "box", "plan", "exit_plan_mode", "handoff", "web_search", "web_fetch", "send_message", "skill", "tool_search"] {
            assert!(is_host_derived_tool_name(name), "{name}");
        }
        assert!(!is_host_derived_tool_name("read"));
        assert!(is_web_tool_name("web_fetch") && !is_web_tool_name("read"));
    }
}
