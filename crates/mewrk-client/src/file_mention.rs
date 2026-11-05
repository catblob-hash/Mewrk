//! What "mention in the conversation" writes at the end of the composer: a line of a file as `path:line:`, left open for what
//! the reader has to say about it (upstream's `lib/fileMention.ts`).

/// A line a pane was asked to mention.
#[derive(Clone, Debug, PartialEq)]
pub struct FileMention {
    /// Relative to its workspace's root and `/` separated; absolute when the file is in no workspace.
    pub path: String,
    /// The number of the workspace the file is in, or `None` when it is in none.
    pub workspace: Option<u32>,
    pub line: u32,
}

/// `fileMentionText`. A conversation with several workspaces says which one after the path, as `@<number>`; with one there is
/// nothing to tell apart and the path stands alone.
pub fn file_mention_text(mention: &FileMention, several_workspaces: bool) -> String {
    match mention.workspace {
        Some(number) if several_workspaces => format!("{}@{number}:{}:", mention.path, mention.line),
        _ => format!("{}:{}:", mention.path, mention.line),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_which_workspace_only_when_there_are_several() {
        let mention = FileMention { path: "src/a.rs".to_owned(), workspace: Some(2), line: 7 };
        assert_eq!(file_mention_text(&mention, true), "src/a.rs@2:7:");
        assert_eq!(file_mention_text(&mention, false), "src/a.rs:7:");
        assert_eq!(file_mention_text(&FileMention { workspace: None, ..mention }, true), "src/a.rs:7:");
    }
}
