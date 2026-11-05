//! The HTML rendered Markdown may carry: `lib/markdownHtml.ts` and what `rehype-raw` and `rehype-sanitize` do with it.
//!
//! READMEs and model replies use HTML for what Markdown cannot say: `<br>` in a table cell, `<details>`, `<kbd>`, `<sub>`,
//! a centred logo. It goes through GitHub's allow-list, so it can say exactly what it says on GitHub and no more: no scripts,
//! no event handlers, no styles, no frames, no forms. Markdown between the tags keeps being Markdown: what was already parsed
//! is carried through the HTML parser as a marker element and put back, which is what `rehype-raw`'s stitches are.

use super::parse::{classify_link, Block, Float, Inline, InlineStyle, Link, ListItem, ParseOptions, Raw, TableCell, TableRow};
use fluent::el::TextAlign;
use html5ever::tendril::TendrilSink;
use html5ever::{local_name, ns, ParseOpts, QualName};
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use regex::Regex;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::{Arc, LazyLock};

pub(super) const BLOCK_MARK: &str = "mw-block";
const INLINE_MARK: &str = "mw-inline";

#[derive(Clone, Debug)]
pub(super) enum Node {
    Text(String),
    Element(Element),
}

#[derive(Clone, Debug, Default)]
pub(super) struct Element {
    pub tag: String,
    pub props: BTreeMap<String, String>,
    pub children: Vec<Node>,
}

/// Whether `content` has anything that could be an HTML tag. The raw-HTML pass costs a second parse, which a streamed reply
/// would pay for on every token; most replies have no HTML at all. A false positive only costs the pass.
pub fn may_contain_html(content: &str) -> bool {
    static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<(?:[A-Za-z][\w-]*[\s/>]|/[A-Za-z]|!--)").expect("regex"));
    content.contains('<') && TAG.is_match(content)
}

/// GitHub's allow-list (`rehype-sanitize`'s default schema) with what `MARKDOWN_HTML_SCHEMA` adds.
const ALLOWED_TAGS: &[&str] = &[
    "a", "b", "blockquote", "br", "code", "dd", "del", "details", "div", "dl", "dt", "em", "h1", "h2", "h3", "h4", "h5", "h6", "hr", "i", "img", "input", "ins", "kbd", "li", "ol", "p", "picture", "pre", "q", "rp", "rt", "ruby", "s", "samp",
    "section", "source", "span", "strike", "strong", "sub", "summary", "sup", "table", "tbody", "td", "tfoot", "th", "thead", "tr", "tt", "ul", "var", "abbr", "button", "caption", "cite", "col", "colgroup", "dfn", "figcaption", "figure", "mark", "small",
    "time", "u", "wbr",
];
/// Removed along with everything inside them. Anything else not on the list keeps its children.
const STRIPPED_TAGS: &[&str] = &["script"];
/// The attributes anything here reads. The schema allows more, and they would all be dropped on the way to drawing anyway.
const KEPT_PROPS: &[&str] = &["href", "src", "alt", "title", "align", "width", "height", "id", "name", "open", "start", "checked", "type", "class", "i", "colspan", "rowspan"];
const HREF_PROTOCOLS: &[&str] = &["http", "https", "irc", "ircs", "mailto", "xmpp", "file"];
const SRC_PROTOCOLS: &[&str] = &["http", "https", "data"];

/// What the HTML table model allows of these.
const MAX_COLSPAN: u32 = 1000;
const MAX_ROWSPAN: u32 = 65534;

/// How deep HTML is followed. A message is untrusted, and everything that walks this tree recurses.
const MAX_DEPTH: usize = 64;

fn convert(handle: &Handle, depth: usize, out: &mut Vec<Node>) {
    if depth > MAX_DEPTH {
        return;
    }
    for child in handle.children.borrow().iter() {
        match &child.data {
            NodeData::Text { contents } => out.push(Node::Text(contents.borrow().to_string())),
            NodeData::Element { name, attrs, .. } => {
                let mut props = BTreeMap::new();
                for attr in attrs.borrow().iter() {
                    props.insert(attr.name.local.to_string(), attr.value.to_string());
                }
                let mut children = Vec::new();
                convert(child, depth + 1, &mut children);
                out.push(Node::Element(Element { tag: name.local.to_string(), props, children }));
            }
            _ => {}
        }
    }
}

fn parse_raw(source: &str) -> Vec<Node> {
    let context = QualName::new(None, ns!(html), local_name!("body"));
    let dom: RcDom = html5ever::parse_fragment(RcDom::default(), ParseOpts::default(), context, Vec::new(), false).from_utf8().one(source.as_bytes());
    let mut nodes = Vec::new();
    convert(&dom.document, 0, &mut nodes);
    // A fragment parse answers with one `html` element around what it read.
    match nodes.pop() {
        Some(Node::Element(root)) if nodes.is_empty() && root.tag == "html" => root.children,
        Some(other) => {
            nodes.push(other);
            nodes
        }
        None => nodes,
    }
}

/// `hast-util-sanitize`'s protocol test: no protocol is relative and fine, and a colon after `?`, `#` or `/` is not one.
fn safe_protocol(value: &str, allowed: &[&str]) -> bool {
    let Some(colon) = value.find(':') else { return true };
    let before = |c: char| value.find(c).is_some_and(|at| colon > at);
    if before('/') || before('?') || before('#') {
        return true;
    }
    allowed.contains(&value[..colon].to_ascii_lowercase().as_str())
}

fn sanitize_props(tag: &str, props: BTreeMap<String, String>) -> BTreeMap<String, String> {
    props
        .into_iter()
        .filter_map(|(key, value)| {
            if !KEPT_PROPS.contains(&key.as_str()) {
                return None;
            }
            match key.as_str() {
                "href" if !safe_protocol(&value, HREF_PROTOCOLS) => return None,
                "src" if !safe_protocol(&value, SRC_PROTOCOLS) => return None,
                // The tag decides what an `input` is, and the class that names a code block's language is the only one kept.
                "class" if !(tag == "code" && value.starts_with("language-") && value.len() > "language-".len()) => return None,
                "i" if !tag.starts_with("mw-") => return None,
                "type" => return None,
                "id" | "name" => return Some((key, format!("user-content-{value}"))),
                _ => {}
            }
            Some((key, value))
        })
        .collect()
}

fn sanitize(nodes: Vec<Node>) -> Vec<Node> {
    let mut out = Vec::with_capacity(nodes.len());
    for node in nodes {
        match node {
            Node::Text(t) => out.push(Node::Text(t)),
            Node::Element(e) => {
                if STRIPPED_TAGS.contains(&e.tag.as_str()) {
                    continue;
                }
                let children = sanitize(e.children);
                if e.tag == BLOCK_MARK || e.tag == INLINE_MARK || ALLOWED_TAGS.contains(&e.tag.as_str()) {
                    // The schema's `required`: whatever an `input` said it was, it comes out a disabled checkbox.
                    let props = sanitize_props(&e.tag, e.props);
                    out.push(Node::Element(Element { tag: e.tag, props, children }));
                } else {
                    out.extend(children);
                }
            }
        }
    }
    out
}

/// Elements that make a block of their own, as far as what is inline around them goes.
const BLOCK_TAGS: &[&str] = &["blockquote", "caption", "dd", "details", "div", "dl", "dt", "figcaption", "figure", "h1", "h2", "h3", "h4", "h5", "h6", "hr", "li", "ol", "p", "pre", "section", "summary", "table", "ul", BLOCK_MARK];

/// Elements a following Markdown block can land inside when an HTML block leaves them open: `<details>` with a blank line
/// before its contents, the way every README writes it.
const CONTAINER_TAGS: &[&str] = &["blockquote", "center", "dd", "details", "div", "dl", "figure", "li", "ol", "section", "article", "aside", "main", "nav", "header", "footer", "table", "tbody", "td", "th", "thead", "tr", "tfoot", "ul"];

/// How many more containers `html` opens than it closes, for telling that the blocks after it belong inside.
pub(super) fn open_containers(html: &str) -> i32 {
    static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?s)<!--.*?-->|<(/?)([A-Za-z][A-Za-z0-9-]*)(?:[^>"']|"[^"]*"|'[^']*')*?(/?)>"#).expect("regex"));
    let mut depth = 0;
    for c in TAG.captures_iter(html) {
        let Some(name) = c.get(2) else { continue };
        let name = name.as_str().to_ascii_lowercase();
        if !CONTAINER_TAGS.contains(&name.as_str()) {
            continue;
        }
        if &c[1] == "/" {
            depth -= 1;
        } else if &c[3] != "/" {
            depth += 1;
        }
    }
    depth
}

/// A run of source text as a page lays it out: whitespace collapsed, with a no-break space left alone.
fn collapse(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for c in text.chars() {
        if matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{c}') {
            space = true;
        } else {
            if space {
                out.push(' ');
                space = false;
            }
            out.push(c);
        }
    }
    if space {
        out.push(' ');
    }
    out
}

fn text_of(nodes: &[Node], out: &mut String) {
    for node in nodes {
        match node {
            Node::Text(t) => out.push_str(t),
            Node::Element(e) => text_of(&e.children, out),
        }
    }
}

struct Convert<'a> {
    options: ParseOptions,
    uid: &'a mut u32,
    inline_marks: &'a [Inline],
    block_marks: Vec<Option<Vec<Block>>>,
}

impl Convert<'_> {
    fn next_uid(&mut self) -> u32 {
        *self.uid += 1;
        *self.uid
    }

    fn inlines(&mut self, nodes: &[Node], style: InlineStyle, link: &Option<Rc<Link>>, out: &mut Vec<Inline>) {
        for node in nodes {
            match node {
                Node::Text(t) => {
                    let mut text = collapse(t);
                    if text.is_empty() {
                        continue;
                    }
                    // A line starts with no space, whatever the source had there.
                    if (out.is_empty() || matches!(out.last(), Some(Inline::Break))) && text.starts_with(' ') {
                        text.remove(0);
                        if text.is_empty() {
                            continue;
                        }
                    }
                    out.push(Inline::Text { text, style, link: link.clone() });
                }
                Node::Element(e) => self.inline_element(e, style, link, out),
            }
        }
    }

    /// What elements say, with what was already parsed before the HTML parser saw it put back as its text.
    fn text_with_marks(&self, nodes: &[Node], out: &mut String) {
        for node in nodes {
            match node {
                Node::Text(text) => out.push_str(text),
                Node::Element(e) if e.tag == INLINE_MARK => {
                    let at: usize = e.props.get("i").and_then(|i| i.parse().ok()).unwrap_or(usize::MAX);
                    if let Some(Inline::Text { text, .. }) = self.inline_marks.get(at) {
                        out.push_str(text);
                    }
                }
                Node::Element(e) => self.text_with_marks(&e.children, out),
            }
        }
    }

    fn inline_element(&mut self, e: &Element, style: InlineStyle, link: &Option<Rc<Link>>, out: &mut Vec<Inline>) {
        // Anything with an `id` is somewhere a fragment can go.
        if e.tag != "a"
            && let Some(id) = e.props.get("id")
        {
            out.push(Inline::Anchor(id.clone()));
        }
        let with = |f: fn(&mut InlineStyle)| {
            let mut s = style;
            f(&mut s);
            s
        };
        let style = match e.tag.as_str() {
            INLINE_MARK => {
                let at: usize = e.props.get("i").and_then(|i| i.parse().ok()).unwrap_or(usize::MAX);
                let Some(inline) = self.inline_marks.get(at) else { return };
                out.push(merged(inline.clone(), style, link));
                return;
            }
            BLOCK_MARK => {
                // Markdown inside something flat, a table cell or a summary: its text, lines kept apart.
                let at: usize = e.props.get("i").and_then(|i| i.parse().ok()).unwrap_or(usize::MAX);
                if let Some(blocks) = self.block_marks.get_mut(at).and_then(Option::take) {
                    flatten(&blocks, out);
                }
                return;
            }
            "br" => {
                out.push(Inline::Break);
                return;
            }
            "wbr" => return,
            "img" => {
                let width = e.props.get("width").and_then(|w| w.trim_end_matches("px").parse().ok());
                // The attribute's own meaning: left and right float it, and the rest only move it in a line it isn't in.
                let float = e.props.get("align").and_then(|align| match align.trim().to_ascii_lowercase().as_str() {
                    "left" => Some(Float::Left),
                    "right" => Some(Float::Right),
                    _ => None,
                });
                out.push(Inline::Image { src: e.props.get("src").cloned().unwrap_or_default(), alt: e.props.get("alt").cloned().unwrap_or_default(), width, float, link: link.clone() });
                return;
            }
            "input" => {
                out.push(Inline::Text { text: if e.props.contains_key("checked") { "☑ " } else { "☐ " }.to_owned(), style, link: link.clone() });
                return;
            }
            "a" if !e.props.contains_key("href") && (e.props.contains_key("id") || e.props.contains_key("name")) && e.children.iter().all(|c| matches!(c, Node::Text(t) if t.trim().is_empty())) => {
                out.push(Inline::Anchor(e.props.get("id").or_else(|| e.props.get("name")).cloned().unwrap_or_default()));
                return;
            }
            "a" => {
                let target = e.props.get("href").map(|href| classify_link(href, e.props.get("title").cloned(), self.options)).or_else(|| link.clone());
                self.inlines(&e.children, style, &target, out);
                return;
            }
            "q" => {
                out.push(Inline::Text { text: "“".to_owned(), style, link: link.clone() });
                self.inlines(&e.children, style, link, out);
                out.push(Inline::Text { text: "”".to_owned(), style, link: link.clone() });
                return;
            }
            "rp" => return,
            "ruby" => {
                // Each `<rt>` reads the run of text before it. `<rp>` is for what cannot draw this.
                let mut base = String::new();
                for child in &e.children {
                    match child {
                        Node::Element(c) if c.tag == "rt" => {
                            let mut reading = String::new();
                            self.text_with_marks(&c.children, &mut reading);
                            out.push(Inline::Ruby { base: std::mem::take(&mut base).trim().to_owned(), reading: reading.trim().to_owned(), style, link: link.clone() });
                        }
                        Node::Element(c) if c.tag == "rp" => {}
                        other => self.text_with_marks(std::slice::from_ref(other), &mut base),
                    }
                }
                if !base.trim().is_empty() {
                    out.push(Inline::Text { text: base, style, link: link.clone() });
                }
                return;
            }
            // `MarkdownCode`: math, however it got its class.
            "code" if e.props.get("class").map(String::as_str) == Some("language-math") => {
                let mut source = String::new();
                self.text_with_marks(&e.children, &mut source);
                out.push(Inline::Math { source: source.trim().to_owned(), style, link: link.clone() });
                return;
            }
            "b" | "strong" => with(|s| s.bold = true),
            "i" | "em" | "cite" | "dfn" | "var" => with(|s| s.italic = true),
            "s" | "strike" | "del" => with(|s| s.strike = true),
            "u" | "ins" => with(|s| s.underline = true),
            "code" => with(|s| s.code = true),
            "samp" | "tt" => with(|s| s.mono = true),
            "kbd" => with(|s| s.kbd = true),
            "mark" => with(|s| s.mark = true),
            "sup" => with(|s| s.sup = true),
            "sub" => with(|s| s.sub = true),
            "small" => with(|s| s.small = true),
            _ => style,
        };
        self.inlines(&e.children, style, link, out);
    }

    fn cell(&mut self, e: &Element) -> TableCell {
        // The attributes' rules in the HTML table model: nonsense is 1, and neither is allowed to go on for ever.
        let span = |name: &str, least: u32, most: u32| e.props.get(name).and_then(|v| v.trim().parse::<u32>().ok()).filter(|n| *n >= least).map_or(1, |n| n.min(most));
        let (colspan, rowspan) = (span("colspan", 1, MAX_COLSPAN), span("rowspan", 0, MAX_ROWSPAN));
        let align = align_attr(e).unwrap_or(TextAlign::Start);
        let header = e.tag == "th";
        let has_blocks = e.children.iter().any(|c| matches!(c, Node::Element(c) if BLOCK_TAGS.contains(&c.tag.as_str())));
        if has_blocks {
            let mut blocks = Vec::new();
            self.blocks(&e.children, &mut blocks);
            return TableCell { inlines: Vec::new(), blocks: Some(blocks), align, colspan, rowspan, header };
        }
        let mut inlines = Vec::new();
        self.inlines(&e.children, InlineStyle::default(), &None, &mut inlines);
        trim_inlines(&mut inlines);
        TableCell { inlines, blocks: None, align, colspan, rowspan, header }
    }

    /// A table's rows in the order they draw: `thead` first and `tfoot` last, wherever they stand in the source.
    fn table_rows(&mut self, e: &Element, rows: &mut Vec<TableRow>) {
        let (mut head, mut body, mut foot) = (Vec::new(), Vec::new(), Vec::new());
        for child in &e.children {
            let Node::Element(c) = child else { continue };
            match c.tag.as_str() {
                "thead" => head.push(c),
                "tbody" | "tr" => body.push(c),
                "tfoot" => foot.push(c),
                _ => {}
            }
        }
        let mut section = 0u16;
        for group in head.into_iter().chain(body).chain(foot) {
            if group.tag == "tr" {
                rows.push(self.table_row(group, section));
            } else {
                for row in &group.children {
                    if let Node::Element(row) = row
                        && row.tag == "tr"
                    {
                        rows.push(self.table_row(row, section));
                    }
                }
            }
            section = section.saturating_add(1);
        }
    }

    fn table_row(&mut self, e: &Element, section: u16) -> TableRow {
        let mut cells = Vec::new();
        for cell in &e.children {
            let Node::Element(cell) = cell else { continue };
            if cell.tag == "td" || cell.tag == "th" {
                cells.push(self.cell(cell));
            }
        }
        TableRow { header: cells.iter().any(|c: &TableCell| c.header), cells, section }
    }

    fn blocks(&mut self, nodes: &[Node], out: &mut Vec<Block>) {
        let mut run: Vec<&Node> = Vec::new();
        for node in nodes {
            let block_element = matches!(node, Node::Element(e) if BLOCK_TAGS.contains(&e.tag.as_str()));
            if !block_element {
                // An anchor with no content is a place, not a word.
                if let Node::Element(e) = node
                    && e.tag == "a"
                    && !e.props.contains_key("href")
                    && (e.props.contains_key("id") || e.props.contains_key("name"))
                    && e.children.iter().all(|c| matches!(c, Node::Text(t) if t.trim().is_empty()))
                {
                    self.flush_run(&mut run, out);
                    out.push(Block::Anchor(e.props.get("id").or_else(|| e.props.get("name")).cloned().unwrap_or_default()));
                    continue;
                }
                run.push(node);
                continue;
            }
            self.flush_run(&mut run, out);
            let Node::Element(e) = node else { continue };
            self.block_element(e, out);
        }
        self.flush_run(&mut run, out);
    }

    fn flush_run(&mut self, run: &mut Vec<&Node>, out: &mut Vec<Block>) {
        if run.is_empty() {
            return;
        }
        let mut inlines = Vec::new();
        let owned: Vec<Node> = run.drain(..).cloned().collect();
        self.inlines(&owned, InlineStyle::default(), &None, &mut inlines);
        trim_inlines(&mut inlines);
        if !inlines.is_empty() {
            out.push(Block::Paragraph { inlines, tight: true });
        }
    }

    fn block_element(&mut self, e: &Element, out: &mut Vec<Block>) {
        let align = align_attr(e);
        let mut own = Vec::new();
        // A heading carries its own; anything else with an `id` is somewhere a fragment can go.
        if !matches!(e.tag.as_str(), "h1" | "h2" | "h3" | "h4" | "h5" | "h6")
            && let Some(id) = e.props.get("id")
        {
            own.push(Block::Anchor(id.clone()));
        }
        match e.tag.as_str() {
            BLOCK_MARK => {
                let at: usize = e.props.get("i").and_then(|i| i.parse().ok()).unwrap_or(usize::MAX);
                if let Some(blocks) = self.block_marks.get_mut(at).and_then(Option::take) {
                    own.extend(blocks);
                }
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let mut inlines = Vec::new();
                self.inlines(&e.children, InlineStyle::default(), &None, &mut inlines);
                trim_inlines(&mut inlines);
                own.push(Block::Heading { level: e.tag.as_bytes()[1] - b'0', inlines, id: e.props.get("id").cloned() });
            }
            "p" | "figcaption" | "caption" | "summary" | "dt" => {
                let mut inlines = Vec::new();
                self.inlines(&e.children, InlineStyle::default(), &None, &mut inlines);
                trim_inlines(&mut inlines);
                if !inlines.is_empty() {
                    own.push(Block::Paragraph { inlines, tight: e.tag != "p" });
                }
            }
            "hr" => own.push(Block::Rule),
            "pre" => {
                let mut code = String::new();
                text_of(&e.children, &mut code);
                let class = e.children.iter().find_map(|c| match c {
                    Node::Element(c) if c.tag == "code" => c.props.get("class").cloned(),
                    _ => None,
                });
                let label = class.as_deref().and_then(|c| c.strip_prefix("language-")).map(str::to_owned);
                let language = label.as_deref().and_then(super::parse::fence_grammar);
                let source = code.strip_suffix('\n').unwrap_or(&code);
                let uid = self.next_uid();
                if label.as_deref() == Some("math") {
                    own.push(Block::Math { uid, source: source.trim().to_owned() });
                } else {
                    let chrome = label.is_some() || e.children.iter().any(|c| matches!(c, Node::Element(c) if c.tag == "code"));
                    own.push(Block::Code { uid, label, language, code: Arc::from(source), chrome });
                }
            }
            "blockquote" => {
                let mut inner = Vec::new();
                self.blocks(&e.children, &mut inner);
                own.push(Block::Quote(inner));
            }
            "ul" | "ol" => {
                let mut items = Vec::new();
                for child in &e.children {
                    let Node::Element(li) = child else { continue };
                    if li.tag != "li" {
                        continue;
                    }
                    let mut blocks = Vec::new();
                    self.blocks(&li.children, &mut blocks);
                    items.push(ListItem { task: None, blocks });
                }
                let start = (e.tag == "ol").then(|| e.props.get("start").and_then(|s| s.parse().ok()).unwrap_or(1));
                own.push(Block::List { start, items });
            }
            "table" => {
                for child in &e.children {
                    if let Node::Element(c) = child
                        && c.tag == "caption"
                    {
                        self.block_element(c, &mut own);
                    }
                }
                let mut rows = Vec::new();
                self.table_rows(e, &mut rows);
                let uid = self.next_uid();
                own.push(Block::Table { uid, rows });
            }
            "details" => {
                let mut summary = Vec::new();
                let mut rest: Vec<Node> = Vec::new();
                let mut seen = false;
                for child in &e.children {
                    match child {
                        Node::Element(s) if s.tag == "summary" && !seen => {
                            seen = true;
                            self.inlines(&s.children, InlineStyle::default(), &None, &mut summary);
                            trim_inlines(&mut summary);
                        }
                        other => rest.push(other.clone()),
                    }
                }
                let mut blocks = Vec::new();
                self.blocks(&rest, &mut blocks);
                let uid = self.next_uid();
                own.push(Block::Details { uid, open: e.props.contains_key("open"), summary, blocks });
            }
            "dd" | "figure" => {
                let mut inner = Vec::new();
                self.blocks(&e.children, &mut inner);
                own.push(Block::Indented { by: 40.0, blocks: inner });
            }
            _ => self.blocks(&e.children, &mut own),
        }
        match align {
            Some(align) if align != TextAlign::Start => out.push(Block::Aligned { align, blocks: own }),
            _ => out.extend(own),
        }
    }
}

fn align_attr(e: &Element) -> Option<TextAlign> {
    match e.props.get("align")?.trim().to_ascii_lowercase().as_str() {
        "center" | "middle" => Some(TextAlign::Center),
        "right" => Some(TextAlign::End),
        "left" => Some(TextAlign::Start),
        _ => None,
    }
}

fn merged(inline: Inline, outer: InlineStyle, outer_link: &Option<Rc<Link>>) -> Inline {
    let merge = |s: InlineStyle| InlineStyle {
        bold: s.bold || outer.bold,
        italic: s.italic || outer.italic,
        strike: s.strike || outer.strike,
        underline: s.underline || outer.underline,
        code: s.code || outer.code,
        kbd: s.kbd || outer.kbd,
        mark: s.mark || outer.mark,
        sup: s.sup || outer.sup,
        sub: s.sub || outer.sub,
        small: s.small || outer.small,
        mono: s.mono || outer.mono,
    };
    match inline {
        Inline::Text { text, style, link } => Inline::Text { text, style: merge(style), link: link.or_else(|| outer_link.clone()) },
        Inline::Math { source, style, link } => Inline::Math { source, style: merge(style), link: link.or_else(|| outer_link.clone()) },
        Inline::Image { src, alt, width, float, link } => Inline::Image { src, alt, width, float, link: link.or_else(|| outer_link.clone()) },
        Inline::Ruby { base, reading, style, link } => Inline::Ruby { base, reading, style: merge(style), link: link.or_else(|| outer_link.clone()) },
        other => other,
    }
}

fn flatten(blocks: &[Block], out: &mut Vec<Inline>) {
    for block in blocks {
        match block {
            Block::Paragraph { inlines, .. } | Block::Heading { inlines, .. } => {
                if !out.is_empty() && !matches!(out.last(), Some(Inline::Break)) {
                    out.push(Inline::Break);
                }
                out.extend(inlines.iter().cloned());
            }
            Block::Quote(inner) | Block::Aligned { blocks: inner, .. } | Block::Indented { blocks: inner, .. } => flatten(inner, out),
            Block::List { items, .. } => {
                for item in items {
                    flatten(&item.blocks, out);
                }
            }
            Block::Code { code, .. } => {
                if !out.is_empty() && !matches!(out.last(), Some(Inline::Break)) {
                    out.push(Inline::Break);
                }
                out.push(Inline::Text { text: code.to_string(), style: InlineStyle { code: true, ..Default::default() }, link: None });
            }
            Block::Math { source, .. } => out.push(Inline::Math { source: source.clone(), style: InlineStyle::default(), link: None }),
            _ => {}
        }
    }
}

/// Spaces and line breaks a block starts or ends with take no room on the page.
fn trim_inlines(inlines: &mut Vec<Inline>) {
    while matches!(inlines.last(), Some(Inline::Break)) {
        inlines.pop();
    }
    if let Some(Inline::Text { text, .. }) = inlines.last_mut() {
        let kept = text.trim_end_matches(' ').len();
        text.truncate(kept);
        if text.is_empty() {
            inlines.pop();
        }
    }
    while matches!(inlines.first(), Some(Inline::Break)) {
        inlines.remove(0);
    }
}

/// One HTML block, or several with Markdown between them, as blocks.
pub(super) fn html_to_blocks(html: &str, children: Vec<Option<Vec<Block>>>, uid: &mut u32, options: ParseOptions) -> Vec<Block> {
    let nodes = sanitize(parse_raw(html));
    let mut cx = Convert { options, uid, inline_marks: &[], block_marks: children };
    let mut out = Vec::new();
    cx.blocks(&nodes, &mut out);
    out
}

/// A paragraph with inline HTML in it, as inlines: everything already parsed goes through as a marker.
pub(super) fn stitch_inlines(raw: Vec<Raw>, options: ParseOptions) -> Vec<Inline> {
    let mut html = String::new();
    let mut marks = Vec::new();
    for item in raw {
        match item {
            Raw::Html(h) => html.push_str(&h),
            Raw::Inline(inline) => {
                html.push_str(&format!("<{INLINE_MARK} i=\"{}\"></{INLINE_MARK}>", marks.len()));
                marks.push(inline);
            }
        }
    }
    let nodes = sanitize(parse_raw(&html));
    let mut uid = 0;
    let mut cx = Convert { options, uid: &mut uid, inline_marks: &marks, block_marks: Vec::new() };
    let mut out = Vec::new();
    cx.inlines(&nodes, InlineStyle::default(), &None, &mut out);
    out
}
