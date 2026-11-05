//! A device's UI tree as `preview_snapshot` prints it.
//!
//! The format is the web snapshot's — `[uid] role: "name" (value: "v")`, two
//! spaces per level — so the model reads a phone's screen the way it reads a
//! page, plus each element's box `@x,y wxh` in the **coordinate frame** of
//! `preview_screenshot`, so a uid and a pair of coordinates from the picture
//! always agree. Uids come from the session's own counter, which starts at
//! [`FIRST_UID`]: far above anything a page's counter reaches, so a uid from
//! one never names an element of the other.
//!
//! Layout containers without a name, value or identifier are noise the model
//! cannot act on; their children are printed in their place. What a device
//! sends is bounded here, not trusted: names are cut, depth is cut, the whole
//! text is cut, and a pathologically nested tree is walked only so deep.

use std::fmt::Write as _;

use super::{
    driver::{is_hidden_format, Rect, UiNode},
    frame::screen_to_frame,
};

/// The first uid a session hands out.
pub const FIRST_UID: u64 = 1_000_001;
/// What `preview_snapshot` answers for a screen with no element tree.
pub const NO_TREE: &str = "This screen (VNC) has no element tree; use preview_screenshot and act by coordinates with preview_input.";
/// Printed levels; an element this deep shows how much it holds instead.
const MAX_DEPTH: usize = 12;
/// Characters of a name or value.
const TEXT_CHARS: usize = 200;
/// Characters of a role.
const ROLE_CHARS: usize = 60;
/// Characters of the whole snapshot.
const MAX_CHARS: usize = 12_000;
/// Levels of the device's tree walked at all, hoisted wrappers included. Real
/// trees are a few dozen deep; this keeps a hostile one from exhausting the
/// stack.
const MAX_TREE_DEPTH: usize = 256;

/// One printed element, as a click on its uid needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UidTarget {
    /// The element's middle in screen pixels, `None` when it has no area.
    pub center: Option<(u32, u32)>,
    /// The element's rectangle in screen pixels (the space `center` is in),
    /// `None` when the device gave it none.
    pub bounds: Option<Rect>,
    pub role: String,
    pub name: String,
}

/// A rendered snapshot and the uids it printed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub text: String,
    pub uids: Vec<(u64, UidTarget)>,
    /// The first uid the next snapshot may use.
    pub next_uid: u64,
}

/// Renders `root`, numbering printed elements from `first_uid`. `screen` is the
/// screen's size in pixels and `frame` the model's coordinate frame for it.
pub fn render(root: &UiNode, first_uid: u64, screen: (u32, u32), frame: (u32, u32)) -> Snapshot {
    let mut renderer = Renderer {
        lines: Vec::new(),
        chars: 0,
        truncated: false,
        printed: 0,
        next_uid: first_uid,
        uids: Vec::new(),
        screen,
        frame,
    };
    renderer.node(root, 0, 0);
    let mut text = format!(
        "Screen {}x{} (the coordinate frame of preview_input and preview_screenshot; @x,y wxh are its pixels).\n",
        frame.0, frame.1
    );
    if renderer.lines.is_empty() {
        text.push_str("(no elements with a name, value or identifier on this screen)");
    } else {
        text.push_str(&renderer.lines.join("\n"));
    }
    if renderer.truncated {
        let total = count_nodes(root);
        let _ = write!(
            text,
            "\n\n... (truncated at {MAX_CHARS} characters, showing {} of {total} elements; act on the rest by coordinates from preview_screenshot, or scroll and take a new snapshot)",
            renderer.printed
        );
    }
    Snapshot {
        text,
        uids: renderer.uids,
        next_uid: renderer.next_uid,
    }
}

struct Renderer {
    lines: Vec<String>,
    chars: usize,
    truncated: bool,
    printed: usize,
    next_uid: u64,
    uids: Vec<(u64, UidTarget)>,
    screen: (u32, u32),
    frame: (u32, u32),
}

impl Renderer {
    fn node(&mut self, node: &UiNode, indent: usize, tree_depth: usize) {
        if self.truncated || tree_depth > MAX_TREE_DEPTH {
            return;
        }
        if is_wrapper(node) {
            for child in &node.children {
                self.node(child, indent, tree_depth + 1);
            }
            return;
        }
        let uid = self.next_uid;
        self.next_uid += 1;
        let role = clean(&node.role, ROLE_CHARS);
        let name = clean(&node.name, TEXT_CHARS);
        let mut line = format!("{}[{uid}] {role}", "  ".repeat(indent));
        if !name.is_empty() {
            let _ = write!(line, ": \"{name}\"");
        }
        if let Some(value) = node.value.as_deref().map(|value| clean(value, TEXT_CHARS)) {
            if !value.is_empty() {
                let _ = write!(line, " (value: \"{value}\")");
            }
        }
        if let Some(bounds) = node.bounds {
            line.push_str(&self.frame_box(bounds));
        }
        if node.focused {
            line.push_str(" (focused)");
        }
        if node.selected {
            line.push_str(" (selected)");
        }
        let collapsed = indent >= MAX_DEPTH && !node.children.is_empty();
        if collapsed {
            let _ = write!(line, " ... ({} descendants)", count_nodes(node) - 1);
        }
        let cost = line.chars().count() + 1;
        if self.chars + cost > MAX_CHARS {
            self.truncated = true;
            return;
        }
        self.chars += cost;
        self.lines.push(line);
        self.printed += 1;
        self.uids.push((
            uid,
            UidTarget {
                center: node.bounds.and_then(|bounds| bounds.center()),
                bounds: node.bounds,
                role,
                name,
            },
        ));
        if collapsed {
            return;
        }
        for child in &node.children {
            self.node(child, indent + 1, tree_depth + 1);
        }
    }

    fn frame_box(&self, bounds: Rect) -> String {
        if self.screen.0 == 0 || self.screen.1 == 0 {
            return String::new();
        }
        let (x, y) = screen_to_frame(
            (i64::from(bounds.x), i64::from(bounds.y)),
            self.screen,
            self.frame,
        );
        let (width, height) = screen_to_frame(
            (i64::from(bounds.width), i64::from(bounds.height)),
            self.screen,
            self.frame,
        );
        format!(" @{x},{y} {width}x{height}")
    }
}

/// A container that says nothing: no name, value or identifier, and children
/// to print in its place. A leaf is kept unless it is a bare layout, because
/// an unlabelled icon button is still something to tap.
fn is_wrapper(node: &UiNode) -> bool {
    let silent = node.name.trim().is_empty()
        && node
            .value
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        && node.id.as_deref().is_none_or(|id| id.trim().is_empty())
        && !node.focused
        && !node.selected;
    silent && (!node.children.is_empty() || is_layout(&node.role))
}

fn is_layout(role: &str) -> bool {
    role.ends_with("Layout")
        || matches!(
            role,
            "" | "View" | "ViewGroup" | "Other" | "Group" | "Window" | "Any" | "hierarchy"
        )
}

/// Every element under and including `node`, counted without recursion.
fn count_nodes(node: &UiNode) -> usize {
    let mut count = 0;
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        count += 1;
        stack.extend(current.children.iter());
    }
    count
}

/// One line of text, at most `max_chars` characters: control characters
/// (line breaks above all, which would break the tree) become spaces, and bidirectional
/// controls and zero-width characters, which reorder or hide what a person reads, are dropped.
fn clean(value: &str, max_chars: usize) -> String {
    let visible: String = value
        .chars()
        .filter(|character| !is_hidden_format(*character))
        .collect();
    let mut cleaned: String = visible
        .trim()
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(max_chars + 1)
        .collect();
    if cleaned.chars().count() > max_chars {
        cleaned = cleaned.chars().take(max_chars).collect();
        cleaned.push_str("...");
    }
    cleaned
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(role: &str, name: &str, bounds: (i32, i32, u32, u32)) -> UiNode {
        UiNode {
            role: role.into(),
            name: name.into(),
            bounds: Some(Rect {
                x: bounds.0,
                y: bounds.1,
                width: bounds.2,
                height: bounds.3,
            }),
            enabled: true,
            ..UiNode::default()
        }
    }

    fn layout(children: Vec<UiNode>) -> UiNode {
        UiNode {
            role: "FrameLayout".into(),
            bounds: Some(Rect {
                x: 0,
                y: 0,
                width: 1080,
                height: 2400,
            }),
            children,
            ..UiNode::default()
        }
    }

    #[test]
    fn elements_print_with_uids_and_boxes_in_the_coordinate_frame() {
        let mut field = element("EditText", "Search", (100, 200, 880, 120));
        field.value = Some("cats".into());
        field.id = Some("com.example:id/search".into());
        field.focused = true;
        let tree = layout(vec![
            element("TextView", "Settings", (0, 0, 1080, 200)),
            layout(vec![field, element("Button", "Go", (980, 200, 100, 120))]),
        ]);
        let snapshot = render(&tree, FIRST_UID, (1080, 2400), (576, 1280));
        assert_eq!(
            snapshot.text,
            "Screen 576x1280 (the coordinate frame of preview_input and preview_screenshot; @x,y wxh are its pixels).\n\
             [1000001] TextView: \"Settings\" @0,0 576x107\n\
             [1000002] EditText: \"Search\" (value: \"cats\") @53,107 469x64 (focused)\n\
             [1000003] Button: \"Go\" @523,107 53x64"
        );
        assert_eq!(snapshot.next_uid, 1_000_004);
        assert_eq!(snapshot.uids.len(), 3);
        // The uid table keeps screen pixels: that is what a tap needs.
        assert_eq!(snapshot.uids[2].0, 1_000_003);
        assert_eq!(snapshot.uids[2].1.center, Some((1030, 260)));
        assert_eq!(snapshot.uids[2].1.name, "Go");
    }

    #[test]
    fn a_uid_target_keeps_its_nodes_screen_bounds() {
        let mut unsized_button = element("Button", "Nowhere", (0, 0, 0, 0));
        unsized_button.bounds = None;
        let tree = layout(vec![
            element("Button", "Go", (980, 200, 100, 120)),
            unsized_button,
        ]);
        // The screen is larger than the frame: bounds stay in screen pixels.
        let snapshot = render(&tree, FIRST_UID, (1080, 2400), (576, 1280));
        let go = &snapshot.uids[0].1;
        assert_eq!(go.name, "Go");
        assert_eq!(
            go.bounds,
            Some(Rect {
                x: 980,
                y: 200,
                width: 100,
                height: 120
            })
        );
        assert_eq!(go.center, Some((1030, 260)));
        let nowhere = &snapshot.uids[1].1;
        assert_eq!(nowhere.name, "Nowhere");
        assert_eq!(nowhere.bounds, None);
        assert_eq!(nowhere.center, None);
    }

    #[test]
    fn a_named_container_keeps_its_children_indented_under_it() {
        let mut list = element("RecyclerView", "", (0, 0, 100, 100));
        list.id = Some("list".into());
        list.children = vec![element("TextView", "one", (0, 0, 100, 50))];
        let snapshot = render(&layout(vec![list]), 7, (100, 100), (100, 100));
        let lines: Vec<&str> = snapshot.text.lines().skip(1).collect();
        assert_eq!(
            lines,
            [
                "[7] RecyclerView @0,0 100x100",
                "  [8] TextView: \"one\" @0,0 100x50"
            ]
        );
    }

    #[test]
    fn an_unlabelled_button_is_kept_but_an_empty_layout_is_not() {
        let tree = layout(vec![
            element("ImageButton", "", (0, 0, 10, 10)),
            element("LinearLayout", "", (0, 0, 10, 10)),
        ]);
        let snapshot = render(&tree, FIRST_UID, (10, 10), (10, 10));
        assert_eq!(snapshot.uids.len(), 1);
        assert!(
            snapshot.text.contains("ImageButton @0,0 10x10"),
            "{}",
            snapshot.text
        );
        assert!(!snapshot.text.contains("LinearLayout"));
    }

    #[test]
    fn deep_trees_collapse_and_names_are_cut_to_one_line() {
        let mut deepest = element("TextView", "leaf", (0, 0, 1, 1));
        for level in 0..20 {
            let mut parent = element("Group", &format!("level {level}"), (0, 0, 1, 1));
            parent.children = vec![deepest];
            deepest = parent;
        }
        let snapshot = render(&deepest, 1, (1, 1), (1, 1));
        let lines: Vec<&str> = snapshot.text.lines().skip(1).collect();
        assert_eq!(lines.len(), 13);
        assert!(lines[12].ends_with("... (8 descendants)"), "{}", lines[12]);
        assert!(lines[12].starts_with(&"  ".repeat(12)));

        let long = element(
            "TextView",
            &format!("a\nb{}", "x".repeat(500)),
            (0, 0, 1, 1),
        );
        let snapshot = render(&long, 1, (1, 1), (1, 1));
        let line = snapshot.text.lines().nth(1).unwrap();
        assert!(line.starts_with("[1] TextView: \"a b"), "{line}");
        assert!(
            line.contains(&format!("b{}...\"", "x".repeat(197))),
            "{line}"
        );
    }

    /// What a device calls its elements is shown to the model: control characters become spaces,
    /// and text-direction controls and zero-width characters, which reorder or hide what a
    /// person reads, are dropped.
    #[test]
    fn hidden_format_characters_are_dropped_from_what_is_printed() {
        let mut field = element(
            "EditText",
            "Pay\u{202e}Now\u{200b} \u{2066}here\u{2069}",
            (0, 0, 100, 50),
        );
        field.value = Some("\u{feff}12\u{200f}34\n5".into());
        let snapshot = render(&layout(vec![field]), FIRST_UID, (1080, 2400), (1080, 2400));
        assert!(
            snapshot
                .text
                .contains("[1000001] EditText: \"PayNow here\" (value: \"1234 5\")"),
            "{}",
            snapshot.text
        );
        for hidden in [
            '\u{202e}', '\u{200b}', '\u{2066}', '\u{2069}', '\u{feff}', '\u{200f}',
        ] {
            assert!(!snapshot.text.contains(hidden), "{hidden:?}");
        }
        assert_eq!(snapshot.uids[0].1.name, "PayNow here");
    }

    #[test]
    fn a_huge_tree_is_cut_at_12000_characters_with_a_footer() {
        let children = (0..2000)
            .map(|index| element("TextView", &format!("row {index}"), (0, index, 100, 1)))
            .collect();
        let snapshot = render(&layout(children), FIRST_UID, (1080, 2400), (576, 1280));
        assert!(snapshot.text.chars().count() < 12_400);
        assert!(
            snapshot
                .text
                .contains("... (truncated at 12000 characters, showing"),
            "{}",
            &snapshot.text[snapshot.text.len() - 300..]
        );
        assert!(snapshot.text.contains("of 2001 elements"));
        // Only what was printed can be clicked.
        let printed = snapshot
            .text
            .lines()
            .filter(|line| line.contains("] TextView"))
            .count();
        assert_eq!(snapshot.uids.len(), printed);
    }

    #[test]
    fn a_hostile_nesting_depth_is_walked_only_so_far() {
        let mut node = element("TextView", "bottom", (0, 0, 1, 1));
        for _ in 0..100_000 {
            node = layout(vec![node]);
        }
        let snapshot = render(&node, 1, (1, 1), (1, 1));
        assert!(snapshot.uids.is_empty());
        // Dropping that tree recursively is the test's problem, not the renderer's.
        std::mem::forget(node);
    }
}
