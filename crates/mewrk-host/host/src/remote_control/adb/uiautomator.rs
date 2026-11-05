//! `uiautomator dump` output, read into [`UiNode`]s.
//!
//! The dump is a small, regular XML document: a `<hierarchy>` of nested
//! `<node ...attributes.../>` elements, nothing else. That is read here with a
//! scanner of its own rather than an XML crate, because everything about it is
//! known (and because the text around it is not XML: the tool appends
//! "UI hierchary dumped to: /dev/tty" after the closing tag, and a device
//! sometimes prints a warning before the opening one).
//!
//! The tree is bounded in nodes and depth. A WebView can expose thousands of
//! nodes and arbitrarily deep ones, and the dump came off a device we do not
//! control.

use crate::remote_control::driver::{Rect, UiNode};

/// Nodes kept; the rest of the dump is dropped.
pub(super) const MAX_NODES: usize = 5000;
/// Nesting kept; deeper elements are dropped with their children.
pub(super) const MAX_DEPTH: usize = 128;

/// The part of `output` that is the dump: from `<?xml` (or `<hierarchy`)
/// through `</hierarchy>`. `None` when the tool printed something else — an
/// error, usually, which is left to the caller to read.
pub(super) fn extract_hierarchy(output: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(output);
    let start = match (text.find("<?xml"), text.find("<hierarchy")) {
        (Some(declaration), Some(root)) => declaration.min(root),
        (Some(at), None) | (None, Some(at)) => at,
        (None, None) => return None,
    };
    const END: &str = "</hierarchy>";
    let end = text[start..].find(END)? + start + END.len();
    Some(text[start..end].to_owned())
}

/// Whether the tool failed in a way that trying again fixes: the window had not
/// settled when the dump began.
pub(super) fn is_transient(output: &str) -> bool {
    output.contains("null root node") || output.contains("could not get idle state")
}

/// What an element's attributes say, for the one element kind we care about.
fn node_from(attributes: &[(&str, String)]) -> UiNode {
    let attribute = |name: &str| {
        attributes
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    };
    let flag = |name: &str, default: bool| attribute(name).map_or(default, |value| value == "true");
    let text = attribute("text").unwrap_or_default();
    let description = attribute("content-desc").unwrap_or_default();
    let class = attribute("class").unwrap_or_default();
    // A switch or a checkbox reads as its state; there is no field for it.
    let value = flag("checkable", false).then(|| {
        let state = if flag("checked", false) {
            "checked"
        } else {
            "unchecked"
        };
        state.to_owned()
    });
    UiNode {
        role: class
            .rsplit('.')
            .next()
            .filter(|role| !role.is_empty())
            .unwrap_or("node")
            .to_owned(),
        name: (if text.is_empty() { description } else { text }).to_owned(),
        value,
        id: attribute("resource-id")
            .filter(|id| !id.is_empty())
            .map(str::to_owned),
        bounds: attribute("bounds").and_then(parse_bounds),
        enabled: flag("enabled", true),
        focused: flag("focused", false),
        selected: flag("selected", false),
        package: attribute("package")
            .filter(|package| !package.is_empty())
            .map(str::to_owned),
        clickable: flag("clickable", false),
        text: (!text.is_empty()).then(|| text.to_owned()),
        // Stock uiautomator does not write `hint`; a dump that has one (newer
        // Android versions, some forks) gets it.
        hint: attribute("hint")
            .filter(|hint| !hint.is_empty())
            .map(str::to_owned),
        password: flag("password", false),
        // The dump's text is kept as the device wrote it: no length cap, no cleaning.
        truncated: false,
        inexact: false,
        children: Vec::new(),
    }
}

/// `[l,t][r,b]`, in screen pixels.
fn parse_bounds(value: &str) -> Option<Rect> {
    let inner = value.trim().strip_prefix('[')?.strip_suffix(']')?;
    let (top_left, bottom_right) = inner.split_once("][")?;
    let (left, top) = top_left.split_once(',')?;
    let (right, bottom) = bottom_right.split_once(',')?;
    let left: i64 = left.trim().parse().ok()?;
    let top: i64 = top.trim().parse().ok()?;
    let right: i64 = right.trim().parse().ok()?;
    let bottom: i64 = bottom.trim().parse().ok()?;
    Some(Rect {
        x: i32::try_from(left).ok()?,
        y: i32::try_from(top).ok()?,
        width: u32::try_from((right - left).max(0)).ok()?,
        height: u32::try_from((bottom - top).max(0)).ok()?,
    })
}

/// Undoes the XML escapes of an attribute value: the five named ones and
/// numeric references. Anything else is left as written.
pub(super) fn decode_entities(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_owned();
    }
    let mut decoded = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(at) = rest.find('&') {
        decoded.push_str(&rest[..at]);
        rest = &rest[at..];
        let window = &rest.as_bytes()[..rest.len().min(12)];
        let Some(end) = window.iter().position(|byte| *byte == b';') else {
            decoded.push('&');
            rest = &rest[1..];
            continue;
        };
        let name = &rest[1..end];
        let character = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => name.strip_prefix('#').and_then(|number| {
                let code = match number.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                    None => number.parse().ok()?,
                };
                Some(char::from_u32(code).unwrap_or('\u{fffd}'))
            }),
        };
        match character {
            Some(character) => {
                decoded.push(character);
                rest = &rest[end + 1..];
            }
            None => {
                decoded.push('&');
                rest = &rest[1..];
            }
        }
    }
    decoded.push_str(rest);
    decoded
}

/// One start, end or empty-element tag.
struct Tag<'a> {
    name: &'a str,
    attributes: Vec<(&'a str, String)>,
    closing: bool,
    self_closing: bool,
}

/// Reads the tag whose `<` is at `open`; returns it and the index after its
/// `>`.
fn read_tag(xml: &str, open: usize) -> Result<(Tag<'_>, usize), String> {
    let bytes = xml.as_bytes();
    let mut index = open + 1;
    let closing = bytes.get(index) == Some(&b'/');
    if closing {
        index += 1;
    }
    let name_start = index;
    while index < bytes.len() && !matches!(bytes[index], b' ' | b'\t' | b'\r' | b'\n' | b'/' | b'>')
    {
        index += 1;
    }
    let name = &xml[name_start..index];
    let mut attributes = Vec::new();
    loop {
        while index < bytes.len() && matches!(bytes[index], b' ' | b'\t' | b'\r' | b'\n') {
            index += 1;
        }
        match bytes.get(index) {
            None => return Err("the UI dump ends inside a tag".to_owned()),
            Some(b'>') => {
                return Ok((
                    Tag {
                        name,
                        attributes,
                        closing,
                        self_closing: false,
                    },
                    index + 1,
                ))
            }
            Some(b'/') if bytes.get(index + 1) == Some(&b'>') => {
                return Ok((
                    Tag {
                        name,
                        attributes,
                        closing,
                        self_closing: true,
                    },
                    index + 2,
                ))
            }
            Some(b'/') => index += 1,
            Some(_) => {
                let key_start = index;
                while index < bytes.len()
                    && !matches!(
                        bytes[index],
                        b'=' | b' ' | b'\t' | b'\r' | b'\n' | b'/' | b'>'
                    )
                {
                    index += 1;
                }
                let key = &xml[key_start..index];
                while index < bytes.len() && matches!(bytes[index], b' ' | b'\t' | b'\r' | b'\n') {
                    index += 1;
                }
                if bytes.get(index) != Some(&b'=') {
                    attributes.push((key, String::new()));
                    continue;
                }
                index += 1;
                while index < bytes.len() && matches!(bytes[index], b' ' | b'\t' | b'\r' | b'\n') {
                    index += 1;
                }
                let quote = match bytes.get(index) {
                    Some(quote @ (b'"' | b'\'')) => *quote,
                    _ => return Err(format!("attribute {key} has no quoted value")),
                };
                index += 1;
                let value_start = index;
                while index < bytes.len() && bytes[index] != quote {
                    index += 1;
                }
                if index >= bytes.len() {
                    return Err(format!("attribute {key} is never closed"));
                }
                attributes.push((key, decode_entities(&xml[value_start..index])));
                index += 1;
            }
        }
    }
}

/// Parses a `<hierarchy>` document. A single top-level node is the root; several
/// (or a dump of more than one window) hang under a `hierarchy` root of their
/// own.
pub(super) fn parse(xml: &str) -> Result<UiNode, String> {
    let bytes = xml.as_bytes();
    let mut root = UiNode {
        role: "hierarchy".to_owned(),
        enabled: true,
        ..UiNode::default()
    };
    // Open nodes, outermost first; `root` takes the top-level ones.
    let mut open: Vec<UiNode> = Vec::new();
    // Elements being skipped because the caps were reached, still open.
    let mut skipped = 0usize;
    let mut count = 0usize;
    let mut index = 0usize;
    while let Some(offset) = bytes[index..].iter().position(|byte| *byte == b'<') {
        let at = index + offset;
        let rest = &xml[at..];
        if rest.starts_with("<!--") {
            index = rest.find("-->").map_or(xml.len(), |end| at + end + 3);
            continue;
        }
        if rest.starts_with("<?") {
            index = rest.find("?>").map_or(xml.len(), |end| at + end + 2);
            continue;
        }
        if rest.starts_with("<!") {
            index = rest.find('>').map_or(xml.len(), |end| at + end + 1);
            continue;
        }
        let (tag, next) = read_tag(xml, at)?;
        index = next;
        if tag.name != "node" {
            continue;
        }
        if tag.closing {
            if skipped > 0 {
                skipped -= 1;
            } else if let Some(finished) = open.pop() {
                match open.last_mut() {
                    Some(parent) => parent.children.push(finished),
                    None => root.children.push(finished),
                }
            }
            continue;
        }
        if skipped > 0 || count >= MAX_NODES || open.len() >= MAX_DEPTH {
            if !tag.self_closing {
                skipped += 1;
            }
            continue;
        }
        count += 1;
        let node = node_from(&tag.attributes);
        if tag.self_closing {
            match open.last_mut() {
                Some(parent) => parent.children.push(node),
                None => root.children.push(node),
            }
        } else {
            open.push(node);
        }
    }
    // A dump cut short still yields what it held.
    while let Some(finished) = open.pop() {
        match open.last_mut() {
            Some(parent) => parent.children.push(finished),
            None => root.children.push(finished),
        }
    }
    match root.children.len() {
        0 => Err("the UI dump holds no elements".to_owned()),
        1 => Ok(root.children.remove(0)),
        _ => Ok(root),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str = r#"<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation="0"><node index="0" text="" resource-id="" class="android.widget.FrameLayout" package="com.example" content-desc="" checkable="false" checked="false" clickable="false" enabled="true" focusable="false" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[0,0][1080,2400]"><node index="0" text="Tom &amp; Jerry &lt;3 &quot;quoted&quot; it&apos;s" resource-id="com.example:id/title" class="android.widget.TextView" package="com.example" content-desc="Title" checkable="false" checked="false" clickable="true" enabled="true" focusable="true" focused="true" scrollable="false" long-clickable="false" password="false" selected="true" bounds="[100,200][500,260]" /><node index="1" text="" resource-id="com.example:id/wifi" class="android.widget.Switch" package="com.example" content-desc="Wi-Fi" checkable="true" checked="true" clickable="true" enabled="false" focusable="true" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[0,300][1080,420]"><node index="0" text="&#x4e2d;&#25991; &#128512;" resource-id="" class="TextView" package="com.example" content-desc="" checkable="false" checked="false" clickable="false" enabled="true" focusable="false" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[10,310][20,320]" /></node></node></hierarchy>UI hierchary dumped to: /dev/tty"#;

    #[test]
    fn a_dump_becomes_a_tree_of_ui_nodes() {
        let xml = extract_hierarchy(DUMP.as_bytes()).unwrap();
        assert!(xml.starts_with("<?xml"));
        assert!(xml.ends_with("</hierarchy>"));
        let root = parse(&xml).unwrap();
        assert_eq!(root.role, "FrameLayout");
        assert_eq!(
            root.bounds,
            Some(Rect {
                x: 0,
                y: 0,
                width: 1080,
                height: 2400
            })
        );
        assert_eq!(root.children.len(), 2);

        let title = &root.children[0];
        assert_eq!(title.role, "TextView");
        assert_eq!(title.name, "Tom & Jerry <3 \"quoted\" it's");
        assert_eq!(title.id.as_deref(), Some("com.example:id/title"));
        assert_eq!(
            title.bounds,
            Some(Rect {
                x: 100,
                y: 200,
                width: 400,
                height: 60
            })
        );
        assert!(title.enabled && title.focused && title.selected);
        assert_eq!(title.value, None);

        let switch = &root.children[1];
        assert_eq!(switch.role, "Switch");
        assert_eq!(switch.name, "Wi-Fi");
        assert_eq!(switch.value.as_deref(), Some("checked"));
        assert!(!switch.enabled);
        assert_eq!(switch.id.as_deref(), Some("com.example:id/wifi"));
        assert_eq!(switch.children.len(), 1);
        assert_eq!(switch.children[0].id, None);
        assert_eq!(root.id, None);
        assert_eq!(switch.children[0].role, "TextView");
        assert_eq!(switch.children[0].name, "\u{4e2d}\u{6587} \u{1f600}");
    }

    #[test]
    fn the_dump_is_found_among_the_text_around_it() {
        let noisy = format!("WARNING: linker: something\n{DUMP}\nmore");
        let xml = extract_hierarchy(noisy.as_bytes()).unwrap();
        assert!(xml.starts_with("<?xml"));
        assert!(xml.ends_with("</hierarchy>"));
        let bare = b"junk <hierarchy rotation=\"1\"><node class=\"a.B\" bounds=\"[1,2][3,4]\"/></hierarchy> tail";
        assert_eq!(
            extract_hierarchy(bare).unwrap(),
            "<hierarchy rotation=\"1\"><node class=\"a.B\" bounds=\"[1,2][3,4]\"/></hierarchy>"
        );
        assert_eq!(
            extract_hierarchy(b"ERROR: null root node returned by UiTestAutomationBridge."),
            None
        );
        assert_eq!(extract_hierarchy(b"<hierarchy>"), None);
        assert!(is_transient(
            "ERROR: null root node returned by UiTestAutomationBridge."
        ));
        assert!(is_transient("ERROR: could not get idle state."));
        assert!(!is_transient("java.io.FileNotFoundException: /dev/tty"));
    }

    #[test]
    fn several_top_level_nodes_share_a_hierarchy_root() {
        let xml = r#"<hierarchy><node class="a.One" bounds="[0,0][1,1]"/><node class="a.Two" bounds="[0,0][1,1]"/></hierarchy>"#;
        let root = parse(xml).unwrap();
        assert_eq!(root.role, "hierarchy");
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.children[1].role, "Two");
        assert!(parse("<hierarchy></hierarchy>").is_err());
    }

    #[test]
    fn entities_decode_and_unknown_ones_are_kept() {
        assert_eq!(decode_entities("a &amp; b"), "a & b");
        assert_eq!(decode_entities("&lt;&gt;&quot;&apos;"), "<>\"'");
        assert_eq!(decode_entities("&#65;&#x42;&#X43;"), "ABC");
        assert_eq!(decode_entities("&#10;"), "\n");
        assert_eq!(decode_entities("&bogus; & &amp"), "&bogus; & &amp");
        assert_eq!(
            decode_entities("&#xD800; &#99999999999;"),
            "\u{fffd} &#99999999999;"
        );
        assert_eq!(
            decode_entities("caf\u{e9} &amp; cr\u{e8}me"),
            "caf\u{e9} & cr\u{e8}me"
        );
        assert_eq!(decode_entities("&"), "&");
        assert_eq!(
            decode_entities("\u{4e2d}&\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}"),
            "\u{4e2d}&\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}"
        );
    }

    #[test]
    fn bounds_are_read_or_dropped() {
        assert_eq!(
            parse_bounds("[10,20][110,70]"),
            Some(Rect {
                x: 10,
                y: 20,
                width: 100,
                height: 50
            })
        );
        // Inverted corners make an empty box, not a huge one.
        assert_eq!(
            parse_bounds("[10,20][5,5]"),
            Some(Rect {
                x: 10,
                y: 20,
                width: 0,
                height: 0
            })
        );
        assert_eq!(parse_bounds(""), None);
        assert_eq!(parse_bounds("[1,2]"), None);
        assert_eq!(parse_bounds("[a,2][3,4]"), None);
        assert_eq!(parse_bounds("[99999999999,0][1,1]"), None);
    }

    #[test]
    fn attribute_values_may_hold_angle_brackets_and_slashes() {
        let xml = r#"<hierarchy><node class="x.Y" text="a > b / c" content-desc='single "quoted"' bounds="[0,0][2,2]"/></hierarchy>"#;
        let node = parse(xml).unwrap();
        assert_eq!(node.name, "a > b / c");
        let xml = r#"<hierarchy><node class="x.Y" text="" content-desc='single "quoted"' bounds="[0,0][2,2]"/></hierarchy>"#;
        assert_eq!(parse(xml).unwrap().name, "single \"quoted\"");
    }

    #[test]
    fn a_node_carries_its_package_text_hint_and_flags() {
        let xml = r#"<hierarchy><node class="android.widget.EditText" package="com.example.app" text="hunter2" hint="Password" content-desc="Secret" clickable="true" password="true" bounds="[0,0][10,10]"/></hierarchy>"#;
        let node = parse(xml).unwrap();
        assert_eq!(node.package.as_deref(), Some("com.example.app"));
        assert!(node.clickable);
        assert_eq!(node.text.as_deref(), Some("hunter2"));
        assert_eq!(node.hint.as_deref(), Some("Password"));
        assert!(node.password);
        assert!(!node.truncated);
        assert_eq!(node.name, "hunter2", "the text still wins for the name");

        // Without any of them, the new fields stay empty.
        let xml = r#"<hierarchy><node class="android.view.View" package="" text="" hint="" bounds="[0,0][10,10]"/></hierarchy>"#;
        let bare = parse(xml).unwrap();
        assert_eq!(bare.package, None);
        assert!(!bare.clickable);
        assert_eq!(bare.text, None);
        assert_eq!(bare.hint, None);
        assert!(!bare.password);
    }

    #[test]
    fn an_empty_text_leaves_the_name_to_the_content_description() {
        let xml = r#"<hierarchy><node class="android.widget.ImageButton" package="com.example" text="" content-desc="Back" clickable="true" bounds="[0,0][10,10]"/></hierarchy>"#;
        let node = parse(xml).unwrap();
        assert_eq!(node.name, "Back");
        assert_eq!(node.text, None);
        assert!(node.clickable);
    }

    #[test]
    fn malformed_markup_is_an_error_not_a_panic() {
        assert!(parse("<hierarchy><node class=\"a").is_err());
        assert!(parse("<hierarchy><node class=a/></hierarchy>").is_err());
        assert!(parse("<hierarchy><node class=\"a\" ").is_err());
        assert!(parse("").is_err());
        assert!(parse("no markup at all").is_err());
    }

    #[test]
    fn scrambled_markup_never_panics() {
        const PIECES: &[&str] = &[
            "<hierarchy>",
            "</hierarchy>",
            "<node ",
            "</node>",
            "<node class=\"a.B\" bounds=\"[1,2][3,4]\"",
            "/>",
            ">",
            "<",
            "</",
            "\"",
            "'",
            "=",
            " ",
            "/",
            "&amp;",
            "&#x",
            "&#",
            ";",
            "\u{4e2d}",
            "\u{1f600}",
            "text=\"x\"",
            "bounds=\"[",
            "<!--",
            "-->",
            "<?",
            "?>",
        ];
        // A fixed pseudo-random walk, so a failure reproduces.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..3000 {
            let mut xml = String::new();
            for _ in 0..(next() % 24) {
                xml.push_str(PIECES[(next() % PIECES.len() as u64) as usize]);
            }
            let _ = parse(&xml);
            let _ = extract_hierarchy(xml.as_bytes());
            let _ = decode_entities(&xml);
        }
    }

    #[test]
    fn a_cut_short_dump_keeps_what_it_held() {
        let xml = r#"<hierarchy><node class="a.Outer" bounds="[0,0][9,9]"><node class="a.Inner" bounds="[1,1][2,2]"/>"#;
        let root = parse(xml).unwrap();
        assert_eq!(root.role, "Outer");
        assert_eq!(root.children[0].role, "Inner");
    }

    #[test]
    fn the_tree_is_capped_in_nodes() {
        let mut xml = String::from("<hierarchy><node class=\"a.Root\" bounds=\"[0,0][1,1]\">");
        for _ in 0..(MAX_NODES + 500) {
            xml.push_str("<node class=\"a.Leaf\" bounds=\"[0,0][1,1]\"/>");
        }
        xml.push_str("</node></hierarchy>");
        let root = parse(&xml).unwrap();
        assert_eq!(root.children.len(), MAX_NODES - 1);
    }

    #[test]
    fn the_tree_is_capped_in_depth_and_closing_tags_stay_matched() {
        let mut xml = String::from("<hierarchy>");
        for _ in 0..(MAX_DEPTH + 40) {
            xml.push_str("<node class=\"a.Deep\" bounds=\"[0,0][1,1]\">");
        }
        for _ in 0..(MAX_DEPTH + 40) {
            xml.push_str("</node>");
        }
        // After the deep run a sibling of the root must still land at the top.
        xml.push_str("<node class=\"a.Sibling\" bounds=\"[0,0][1,1]\"/></hierarchy>");
        let root = parse(&xml).unwrap();
        assert_eq!(root.role, "hierarchy");
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.children[1].role, "Sibling");
        let mut depth = 0;
        let mut node = &root.children[0];
        while let Some(child) = node.children.first() {
            depth += 1;
            node = child;
        }
        assert_eq!(depth + 1, MAX_DEPTH);
    }
}
