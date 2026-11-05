//! The terminal pane's tab strip, one entry per PTY: upstream's `lib/terminalTabs.ts`.
//!
//! The pane is a window onto a set of shells the way a browser window is a window onto a set of pages: the tabs own the
//! sessions, not the pane. Closing the pane only takes the window away; every shell keeps running and comes back with
//! the pane. Ending a shell is a per-tab act, and the last tab leaving is what takes the pane with it.
//!
//! Live session state is not here: `controller` holds that, and it drops a session the moment it stops being live. A
//! tab has to outlive its session (a shell that failed sits on screen with its verdict until the user retries or closes
//! it), so the two are separate. Like upstream's, this state lasts for the app's lifetime and is not saved: the host
//! ends every shell on exit, so a saved tab would have nothing behind it.
//!
//! Beside the shells the strip can hold one read-only page: the output of a command the model ran, opened from its task
//! row. There is never more than one, it sits at the strip's start ahead of every shell whatever order they are dragged
//! into, and it has no process of its own, so closing it ends nothing.

use std::collections::{BTreeMap, HashMap};

use crate::api::{terminal_shell_wire_name, TerminalLaunchChoice};

/// The read-only page's tab id. There is one page at most, so it needs no number, and no shell's `terminal-{ordinal}`
/// id can take this shape.
pub const READ_ONLY_TERMINAL_TAB_ID: &str = "read-only";

const ID_PREFIX: &str = "terminal-";

#[derive(Clone, Debug, PartialEq)]
pub struct TerminalTab {
    /// The host's terminal id, unique within the conversation and never reused.
    pub id: String,
    /// Creation order within the conversation. Ids are minted from it and never reused, so a torn-down shell's address
    /// can never come back.
    pub ordinal: u32,
    /// The number the derived name carries: how many terminals of this tab's shell the conversation had opened when
    /// this one was, itself included. Each shell counts on its own and never counts down.
    pub number: u32,
    /// The user's name for this terminal; `None` follows the derived one.
    pub name: Option<String>,
    /// The workspace and shell this tab's terminal was asked for. `None` leaves both to the host.
    pub launch: Option<TerminalLaunchChoice>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TerminalTabsLayout {
    /// The shells, in strip order after the read-only page.
    pub tabs: Vec<TerminalTab>,
    /// The shell task the read-only page shows, or `None` when the strip has no read-only page.
    pub read_only: Option<String>,
    /// A shell's id, `READ_ONLY_TERMINAL_TAB_ID`, or `None` with nothing in the strip.
    pub active_id: Option<String>,
    /// Monotonic: an ordinal is spent when its tab is created and never minted again.
    pub next_ordinal: u32,
    /// The number each shell's next tab takes, by `terminal_shell_key`; absent is 1.
    pub next_numbers: BTreeMap<String, u32>,
}

impl Default for TerminalTabsLayout {
    /// What a conversation starts with: no terminal until one is asked for.
    fn default() -> Self {
        Self { tabs: Vec::new(), read_only: None, active_id: None, next_ordinal: 1, next_numbers: BTreeMap::new() }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum TerminalTabsAction {
    /// Gives the conversation a tab, started with `launch`, if it has none; the way opening the pane finds something
    /// to show.
    Ensure { conversation_id: String, launch: Option<TerminalLaunchChoice> },
    Add { conversation_id: String, launch: Option<TerminalLaunchChoice> },
    /// Points the read-only page at a shell task, opening the page if there is none, and selects it.
    ShowReadOnly { conversation_id: String, shell_task_id: String },
    /// Closes the read-only page if it shows this shell task: the host let go of its output.
    ForgetReadOnly { conversation_id: String, shell_task_id: String },
    Close { conversation_id: String, terminal_id: String },
    Activate { conversation_id: String, terminal_id: String },
    Rename { conversation_id: String, terminal_id: String, name: String },
    /// Puts the tabs in the order the strip was dragged into. An id the conversation does not hold is ignored, and a
    /// tab the request leaves out keeps its relative place after the rest.
    Reorder { conversation_id: String, terminal_ids: Vec<String> },
    RemoveConversation { conversation_id: String },
}

/// Ids are per conversation, as the host's `(conversationId, terminalId)` key already is.
pub fn terminal_tab_id(ordinal: u32) -> String {
    format!("{ID_PREFIX}{ordinal}")
}

/// Every tab id the strip draws, in strip order: the read-only page first, then the shells.
pub fn terminal_strip_ids(layout: &TerminalTabsLayout) -> Vec<String> {
    let shells = layout.tabs.iter().map(|tab| tab.id.clone());
    if layout.read_only.is_some() { std::iter::once(READ_ONLY_TERMINAL_TAB_ID.to_owned()).chain(shells).collect() } else { shells.collect() }
}

/// Which count a tab's number is drawn from: its shell's, or for a tab that left the shell to the host, the count of
/// those.
pub fn terminal_shell_key(launch: Option<&TerminalLaunchChoice>) -> String {
    launch.and_then(|l| l.shell).map_or_else(String::new, terminal_shell_wire_name)
}

/// `items` put into the order `ids` names, for a list the user has just rearranged by hand: `lib/reorder.ts`.
///
/// The request comes from a view that can be a frame behind the list it rearranges. So an id the list does not hold is
/// ignored, a repeated id counts once, and an item the request leaves out keeps its place relative to the other
/// left-out items, after every named one. `None` when the order would not change.
pub fn arrange_by_ids<T: Clone>(items: &[T], ids: &[String], id_of: impl Fn(&T) -> &str) -> Option<Vec<T>> {
    let by_id: HashMap<&str, &T> = items.iter().map(|item| (id_of(item), item)).collect();
    let mut placed: Vec<&str> = Vec::new();
    let mut next: Vec<T> = Vec::with_capacity(items.len());
    for id in ids {
        if placed.iter().any(|p| *p == id) {
            continue;
        }
        if let Some((key, item)) = by_id.get_key_value(id.as_str()) {
            placed.push(key);
            next.push((*item).clone());
        }
    }
    for item in items {
        if !placed.contains(&id_of(item)) {
            next.push(item.clone());
        }
    }
    let same = next.iter().map(&id_of).eq(items.iter().map(&id_of));
    (!same).then_some(next)
}

fn added(layout: &mut TerminalTabsLayout, launch: Option<TerminalLaunchChoice>) {
    let ordinal = layout.next_ordinal;
    let id = terminal_tab_id(ordinal);
    let key = terminal_shell_key(launch.as_ref());
    let number = layout.next_numbers.get(&key).copied().unwrap_or(1);
    layout.tabs.push(TerminalTab { id: id.clone(), ordinal, number, name: None, launch });
    layout.active_id = Some(id);
    layout.next_ordinal = ordinal + 1;
    layout.next_numbers.insert(key, number + 1);
}

#[derive(Default, Clone, Debug, PartialEq)]
pub struct TerminalTabsState {
    pub by_conversation: HashMap<String, TerminalTabsLayout>,
}

impl TerminalTabsState {
    /// A conversation's layout. One that has none has the opening layout, which is not stored.
    pub fn layout(&self, conversation_id: &str) -> TerminalTabsLayout {
        self.by_conversation.get(conversation_id).cloned().unwrap_or_default()
    }

    /// Applies an action, answering whether anything changed: upstream's reducer handing back the same state.
    pub fn apply(&mut self, action: TerminalTabsAction) -> bool {
        use TerminalTabsAction as A;
        match action {
            A::Ensure { conversation_id, launch } => {
                let mut layout = self.layout(&conversation_id);
                // The read-only page counts: a pane showing it already has something to show.
                if !terminal_strip_ids(&layout).is_empty() {
                    return false;
                }
                added(&mut layout, launch);
                self.by_conversation.insert(conversation_id, layout);
                true
            }
            A::Add { conversation_id, launch } => {
                let mut layout = self.layout(&conversation_id);
                added(&mut layout, launch);
                self.by_conversation.insert(conversation_id, layout);
                true
            }
            A::ShowReadOnly { conversation_id, shell_task_id } => {
                let mut layout = self.layout(&conversation_id);
                if layout.read_only.as_deref() == Some(&shell_task_id) && layout.active_id.as_deref() == Some(READ_ONLY_TERMINAL_TAB_ID) {
                    return false;
                }
                layout.read_only = Some(shell_task_id);
                layout.active_id = Some(READ_ONLY_TERMINAL_TAB_ID.to_owned());
                self.by_conversation.insert(conversation_id, layout);
                true
            }
            A::ForgetReadOnly { conversation_id, shell_task_id } => {
                if self.layout(&conversation_id).read_only.as_deref() != Some(&shell_task_id) {
                    return false;
                }
                self.apply(A::Close { conversation_id, terminal_id: READ_ONLY_TERMINAL_TAB_ID.to_owned() })
            }
            A::Close { conversation_id, terminal_id } => {
                let mut layout = self.layout(&conversation_id);
                let strip = terminal_strip_ids(&layout);
                let Some(index) = strip.iter().position(|id| *id == terminal_id) else { return false };
                let rest: Vec<&String> = strip.iter().filter(|id| **id != terminal_id).collect();
                // Focus falls to the right, as a browser's does, and to the left off the end.
                if layout.active_id.as_deref() == Some(&terminal_id) {
                    layout.active_id = if rest.is_empty() { None } else { Some(rest[index.min(rest.len() - 1)].clone()) };
                }
                if terminal_id == READ_ONLY_TERMINAL_TAB_ID {
                    layout.read_only = None;
                } else {
                    layout.tabs.retain(|tab| tab.id != terminal_id);
                }
                self.by_conversation.insert(conversation_id, layout);
                true
            }
            A::Activate { conversation_id, terminal_id } => {
                let mut layout = self.layout(&conversation_id);
                if layout.active_id.as_deref() == Some(&terminal_id) || !terminal_strip_ids(&layout).contains(&terminal_id) {
                    return false;
                }
                layout.active_id = Some(terminal_id);
                self.by_conversation.insert(conversation_id, layout);
                true
            }
            A::Rename { conversation_id, terminal_id, name } => {
                let mut layout = self.layout(&conversation_id);
                let trimmed = name.trim();
                let name = (!trimmed.is_empty()).then(|| trimmed.to_owned());
                let Some(tab) = layout.tabs.iter_mut().find(|tab| tab.id == terminal_id) else { return false };
                if tab.name == name {
                    return false;
                }
                tab.name = name;
                self.by_conversation.insert(conversation_id, layout);
                true
            }
            A::Reorder { conversation_id, terminal_ids } => {
                // Only the shells move: the read-only page is not among them, so its id is ignored and it keeps the
                // strip's start.
                let mut layout = self.layout(&conversation_id);
                let Some(tabs) = arrange_by_ids(&layout.tabs, &terminal_ids, |tab| tab.id.as_str()) else { return false };
                layout.tabs = tabs;
                self.by_conversation.insert(conversation_id, layout);
                true
            }
            A::RemoveConversation { conversation_id } => self.by_conversation.remove(&conversation_id).is_some(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::TerminalShell::{Bash, Zsh};

    const CONV: &str = "conversation-1";

    fn zsh() -> Option<TerminalLaunchChoice> {
        Some(TerminalLaunchChoice { workspace: Some(1), shell: Some(Zsh) })
    }
    fn bash() -> Option<TerminalLaunchChoice> {
        Some(TerminalLaunchChoice { workspace: Some(1), shell: Some(Bash) })
    }
    fn add(s: &mut TerminalTabsState, launch: Option<TerminalLaunchChoice>) {
        s.apply(TerminalTabsAction::Add { conversation_id: CONV.into(), launch });
    }
    fn close(s: &mut TerminalTabsState, id: &str) -> bool {
        s.apply(TerminalTabsAction::Close { conversation_id: CONV.into(), terminal_id: id.into() })
    }
    fn show_ro(s: &mut TerminalTabsState, id: &str) -> bool {
        s.apply(TerminalTabsAction::ShowReadOnly { conversation_id: CONV.into(), shell_task_id: id.into() })
    }
    fn ids(s: &TerminalTabsState) -> Vec<String> {
        s.layout(CONV).tabs.iter().map(|t| t.id.clone()).collect()
    }
    fn strip(s: &TerminalTabsState) -> Vec<String> {
        terminal_strip_ids(&s.layout(CONV))
    }
    fn active(s: &TerminalTabsState) -> Option<String> {
        s.layout(CONV).active_id
    }
    fn numbered(s: &TerminalTabsState) -> Vec<String> {
        s.layout(CONV).tabs.iter().map(|t| format!("{} {}", t.launch.and_then(|l| l.shell).map_or("?".to_owned(), terminal_shell_wire_name), t.number)).collect()
    }

    #[test]
    fn no_conversation_has_a_terminal_until_one_is_asked_for() {
        let s = TerminalTabsState::default();
        assert!(ids(&s).is_empty());
        assert_eq!(s.layout("constructor"), TerminalTabsLayout::default());
    }

    #[test]
    fn numbers_new_tabs_monotonically_and_never_mints_an_ordinal_twice() {
        let mut s = TerminalTabsState::default();
        add(&mut s, zsh());
        add(&mut s, zsh());
        close(&mut s, "terminal-2");
        add(&mut s, zsh());
        assert_eq!(ids(&s), ["terminal-1", "terminal-3"]);
    }

    #[test]
    fn selects_the_tab_it_just_created_and_keeps_conversations_apart() {
        let mut s = TerminalTabsState::default();
        add(&mut s, zsh());
        add(&mut s, zsh());
        assert_eq!(active(&s).as_deref(), Some("terminal-2"));
        s.apply(TerminalTabsAction::Add { conversation_id: "other".into(), launch: zsh() });
        assert_eq!(s.layout("other").tabs[0].id, "terminal-1");
        assert_eq!(ids(&s).len(), 2);
    }

    #[test]
    fn closing_moves_the_selection_right_and_off_the_end_to_the_left() {
        let mut s = TerminalTabsState::default();
        for _ in 0..3 {
            add(&mut s, zsh());
        }
        s.apply(TerminalTabsAction::Activate { conversation_id: CONV.into(), terminal_id: "terminal-2".into() });
        close(&mut s, "terminal-2");
        assert_eq!(active(&s).as_deref(), Some("terminal-3"));
        close(&mut s, "terminal-3");
        assert_eq!(active(&s).as_deref(), Some("terminal-1"));
        close(&mut s, "terminal-1");
        assert_eq!(active(&s), None);
        assert!(ids(&s).is_empty(), "empties the conversation once the last tab goes");
    }

    #[test]
    fn closing_another_tab_leaves_the_selection_and_a_missing_one_changes_nothing() {
        let mut s = TerminalTabsState::default();
        add(&mut s, zsh());
        add(&mut s, zsh());
        close(&mut s, "terminal-1");
        assert_eq!(active(&s).as_deref(), Some("terminal-2"));
        assert!(!close(&mut s, "terminal-9"));
    }

    #[test]
    fn ensure_is_a_no_op_while_the_conversation_has_a_tab_and_starts_one_as_asked() {
        let mut s = TerminalTabsState::default();
        assert!(s.apply(TerminalTabsAction::Ensure { conversation_id: CONV.into(), launch: bash() }));
        assert_eq!(s.layout(CONV).tabs[0].launch, bash());
        assert!(!s.apply(TerminalTabsAction::Ensure { conversation_id: CONV.into(), launch: zsh() }));
        // An emptied conversation gets a fresh terminal, keeping the spent ordinals.
        close(&mut s, "terminal-1");
        s.apply(TerminalTabsAction::Ensure { conversation_id: CONV.into(), launch: zsh() });
        assert_eq!(ids(&s), ["terminal-2"]);
    }

    #[test]
    fn ensure_counts_the_read_only_page() {
        let mut s = TerminalTabsState::default();
        show_ro(&mut s, "shell-1");
        assert!(!s.apply(TerminalTabsAction::Ensure { conversation_id: CONV.into(), launch: zsh() }));
        assert!(ids(&s).is_empty());
    }

    #[test]
    fn each_shell_counts_on_its_own_and_never_gives_a_number_back() {
        let mut s = TerminalTabsState::default();
        add(&mut s, zsh());
        add(&mut s, bash());
        add(&mut s, zsh());
        assert_eq!(numbered(&s), ["zsh 1", "bash 1", "zsh 2"]);
        close(&mut s, "terminal-3");
        add(&mut s, zsh());
        assert_eq!(numbered(&s), ["zsh 1", "bash 1", "zsh 3"]);
        // Terminals that left the shell to the host count together, and a shell counts across workspaces.
        add(&mut s, None);
        add(&mut s, None);
        add(&mut s, Some(TerminalLaunchChoice { workspace: Some(2), shell: Some(Zsh) }));
        assert_eq!(numbered(&s)[3..], ["? 1", "? 2", "zsh 4"]);
    }

    #[test]
    fn rename_keeps_a_trimmed_name_and_drops_an_empty_one() {
        let mut s = TerminalTabsState::default();
        add(&mut s, zsh());
        let rename = |s: &mut TerminalTabsState, name: &str| s.apply(TerminalTabsAction::Rename { conversation_id: CONV.into(), terminal_id: "terminal-1".into(), name: name.into() });
        assert!(rename(&mut s, "  build  "));
        assert_eq!(s.layout(CONV).tabs[0].name.as_deref(), Some("build"));
        assert!(!rename(&mut s, "build"));
        assert!(rename(&mut s, "   "));
        assert_eq!(s.layout(CONV).tabs[0].name, None);
        assert!(!s.apply(TerminalTabsAction::Rename { conversation_id: CONV.into(), terminal_id: "terminal-9".into(), name: "x".into() }));
    }

    #[test]
    fn reorder_ignores_strangers_and_keeps_what_it_leaves_out() {
        let mut s = TerminalTabsState::default();
        for _ in 0..3 {
            add(&mut s, zsh());
        }
        let reorder = |s: &mut TerminalTabsState, ids: &[&str]| s.apply(TerminalTabsAction::Reorder { conversation_id: CONV.into(), terminal_ids: ids.iter().map(|i| (*i).to_owned()).collect() });
        assert!(reorder(&mut s, &["terminal-3", "gone", "terminal-1"]));
        assert_eq!(ids(&s), ["terminal-3", "terminal-1", "terminal-2"]);
        assert!(!reorder(&mut s, &["terminal-3", "terminal-1", "terminal-2"]), "the same order changes nothing");
    }

    #[test]
    fn the_read_only_page_leads_the_strip_is_one_page_and_closes_as_itself() {
        let mut s = TerminalTabsState::default();
        add(&mut s, zsh());
        assert!(show_ro(&mut s, "shell-1"));
        assert_eq!(strip(&s), [READ_ONLY_TERMINAL_TAB_ID, "terminal-1"]);
        assert_eq!(active(&s).as_deref(), Some(READ_ONLY_TERMINAL_TAB_ID));
        assert!(!show_ro(&mut s, "shell-1"));
        assert!(show_ro(&mut s, "shell-2"), "points the one page at the other command");
        assert_eq!(strip(&s).len(), 2);
        assert_eq!(s.layout(CONV).read_only.as_deref(), Some("shell-2"));

        // A drag cannot move anything ahead of it.
        add(&mut s, zsh());
        s.apply(TerminalTabsAction::Reorder { conversation_id: CONV.into(), terminal_ids: vec!["terminal-2".into(), READ_ONLY_TERMINAL_TAB_ID.into(), "terminal-1".into()] });
        assert_eq!(strip(&s), [READ_ONLY_TERMINAL_TAB_ID, "terminal-2", "terminal-1"]);

        // Forgetting another command does nothing, forgetting its own closes the page and focus falls to the right.
        s.apply(TerminalTabsAction::Activate { conversation_id: CONV.into(), terminal_id: READ_ONLY_TERMINAL_TAB_ID.into() });
        assert!(!s.apply(TerminalTabsAction::ForgetReadOnly { conversation_id: CONV.into(), shell_task_id: "shell-1".into() }));
        assert!(s.apply(TerminalTabsAction::ForgetReadOnly { conversation_id: CONV.into(), shell_task_id: "shell-2".into() }));
        assert_eq!(s.layout(CONV).read_only, None);
        assert_eq!(active(&s).as_deref(), Some("terminal-2"));
    }

    #[test]
    fn removing_a_conversation_drops_its_layout_and_nothing_else() {
        let mut s = TerminalTabsState::default();
        add(&mut s, zsh());
        s.apply(TerminalTabsAction::Add { conversation_id: "other".into(), launch: zsh() });
        assert!(s.apply(TerminalTabsAction::RemoveConversation { conversation_id: CONV.into() }));
        assert!(ids(&s).is_empty());
        assert_eq!(s.layout("other").tabs.len(), 1);
        assert!(!s.apply(TerminalTabsAction::RemoveConversation { conversation_id: CONV.into() }));
    }

    #[test]
    fn shell_key_is_the_shell_and_tab_ids_are_the_ordinal() {
        assert_eq!(terminal_shell_key(zsh().as_ref()), "zsh");
        assert_eq!(terminal_shell_key(None), "");
        assert_eq!(terminal_tab_id(7), "terminal-7");
    }
}
