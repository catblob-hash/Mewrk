//! Markdown into blocks: pulldown-cmark's events shaped into what upstream's remark pipeline makes (GFM, `remark-breaks`,
//! `remark-math`, `remarkPathLinks`, the URL filter and the link rules of `MarkdownContent.tsx`), one top-level block at a
//! time so that a stream only pays for the block that is still growing.

use super::html::{self, open_containers};
use super::math::{MATH_CLOSE, MATH_OPEN};
use crate::path_text::{external_http_url, inline_code_path, local_link_target, split_path_runs, PathRun};
use fluent::el::TextAlign;
use fluent::text::FxHasher;
use mewrk_panes::highlight::{lang_of, Lang};
use pulldown_cmark::{Alignment, BrokenLink, CodeBlockKind, CowStr, Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd};
use regex::Regex;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::{Arc, LazyLock};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ParseOptions {
    pub linkify_paths: bool,
    pub render_html: bool,
    pub document_links: bool,
    pub single_dollar_math: bool,
}

/// Where a link goes, decided the way the `a` override in `MarkdownContent.tsx` decides it.
#[derive(Clone, Debug, PartialEq)]
pub enum LinkTarget {
    /// `#heading`: somewhere in this message.
    Fragment(String),
    /// The system browser.
    External(String),
    /// `mailto:` and the like: kept as a link, with nowhere to go in this app.
    Protocol(String),
    /// A detected path or a link to a file.
    Path { target: String, line: Option<u32> },
    /// A relative link in a document, which only the caller can resolve.
    Document(String),
    /// Names no file, or the surface does not open files: drawn as a link and must not navigate anything.
    Inert,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub target: LinkTarget,
    pub title: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct InlineStyle {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub underline: bool,
    pub code: bool,
    pub kbd: bool,
    pub mark: bool,
    pub sup: bool,
    pub sub: bool,
    pub small: bool,
    /// `<samp>`, `<tt>`: monospace with none of inline code's box.
    pub mono: bool,
}

/// `<img align="left">`: which side the text leaves it, running down the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Float {
    Left,
    Right,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Inline {
    Text { text: String, style: InlineStyle, link: Option<Rc<Link>> },
    /// `$…$`, as its TeX source.
    Math { source: String, style: InlineStyle, link: Option<Rc<Link>> },
    Image { src: String, alt: String, width: Option<f32>, float: Option<Float>, link: Option<Rc<Link>> },
    /// `<ruby>`: a base, and its reading over it.
    Ruby { base: String, reading: String, style: InlineStyle, link: Option<Rc<Link>> },
    Break,
    /// `<a name="x"></a>` in a line of text: somewhere a fragment can go, with nothing to show.
    Anchor(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ListItem {
    /// `Some(done)` for a task item.
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TableCell {
    pub inlines: Vec<Inline>,
    /// What a cell of raw HTML holds in place of a line of text: a paragraph, a list.
    pub blocks: Option<Vec<Block>>,
    pub align: TextAlign,
    /// As the attributes say it, at least 1.
    pub colspan: u32,
    /// 0 is to the end of its section.
    pub rowspan: u32,
    /// `<th>`.
    pub header: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TableRow {
    pub header: bool,
    pub cells: Vec<TableCell>,
    /// Its `thead`, `tbody` or `tfoot`, which a `rowspan` does not reach past.
    pub section: u16,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    /// `tight`: a list item's or an HTML box's own text, with no `<p>` and so no margins around it.
    Paragraph { inlines: Vec<Inline>, tight: bool },
    Heading { level: u8, inlines: Vec<Inline>, id: Option<String> },
    /// `chrome`: a fence, with its corner of controls. A bare `<pre>` has none.
    Code { uid: u32, label: Option<String>, language: Option<Lang>, code: Arc<str>, chrome: bool },
    /// Display math, as its TeX source.
    Math { uid: u32, source: String },
    Quote(Vec<Block>),
    List { start: Option<u64>, items: Vec<ListItem> },
    Table { uid: u32, rows: Vec<TableRow> },
    Rule,
    /// `align="…"` from HTML.
    Aligned { align: TextAlign, blocks: Vec<Block> },
    /// `<dd>` and `<figure>`: a default indent from the page's own stylesheet.
    Indented { by: f32, blocks: Vec<Block> },
    Details { uid: u32, open: bool, summary: Vec<Inline>, blocks: Vec<Block> },
    /// A GFM footnote's definition, gathered at the end of the message as upstream's plugin does. `number` is where its first
    /// reference comes, and a definition nothing refers to is not shown.
    Footnote { number: u32, label: String, blocks: Vec<Block> },
    /// A place with nothing to show: `<a name="x">`.
    Anchor(String),
}

/// One thing at the top of the message: usually one block, sometimes what one HTML tag held open.
pub(super) struct TopItem {
    pub hash: u64,
    pub start: usize,
    /// A footnote's number, or 0 for everything else.
    pub footnote: u32,
    /// `None`: the caller said it already has this block.
    pub blocks: Option<Vec<Block>>,
}

pub(super) type Defs = HashMap<String, (String, String)>;

/// What a parse found out beyond its blocks.
pub(super) struct Parsed {
    pub items: Vec<TopItem>,
    /// Every link reference definition in the text parsed, by its normalized label.
    pub defs: Defs,
}

pub(super) fn cmark_options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_FOOTNOTES
}

fn normalize_label(label: &str) -> String {
    label.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// A block's identity. A block with a bracket in it can read differently once a definition elsewhere changes, so those
/// carry the definitions' hash too.
pub(super) fn block_hash(slice: &str, defs_hash: u64) -> u64 {
    let mut h = FxHasher::default();
    h.write(slice.as_bytes());
    h.write_u64(slice.len() as u64);
    if slice.contains('[') {
        h.write_u64(defs_hash);
    }
    h.finish()
}

/// Hash of a set of definitions, whatever order they came in.
pub(super) fn defs_hash(defs: &Defs) -> u64 {
    defs.iter().fold(0, |all, (label, (dest, title))| {
        let mut h = FxHasher::default();
        label.hash(&mut h);
        dest.hash(&mut h);
        title.hash(&mut h);
        all ^ h.finish()
    })
}

/// Footnote labels defined in `source`, and every reference to one in order: a number depends on where the first reference to
/// it comes, so all of them have to be read again when one moves. Footnotes turn off the tail-only parse altogether, so this only has
/// to say when a whole parse should rebuild blocks it would otherwise keep.
pub(super) fn footnote_labels_hash(source: &str) -> u64 {
    if !source.contains("[^") {
        return 0;
    }
    let mut h = FxHasher::default();
    let mut rest = source;
    while let Some(at) = rest.find("[^") {
        rest = &rest[at + 2..];
        let label = rest.find(']').map_or(rest, |end| &rest[..end]);
        label.hash(&mut h);
        rest.starts_with(label).hash(&mut h);
        rest = &rest[label.len()..];
        rest.starts_with("]:").hash(&mut h);
    }
    h.finish() | 1
}

/// `footnoteOrder`: which footnotes are shown, numbered as `mdast-util-to-hast` numbers them. The references in the text go first, then
/// the ones those notes make, as each note is reached. Answers each label's number and how many references it has.
fn footnote_numbers(source: &str) -> HashMap<String, (u32, u32)> {
    let mut in_body = Vec::new();
    let mut in_note: HashMap<String, Vec<String>> = HashMap::new();
    let mut current: Option<String> = None;
    for event in Parser::new_ext(source, cmark_options()) {
        match event {
            Event::Start(Tag::FootnoteDefinition(label)) => current = Some(label.to_lowercase()),
            Event::End(TagEnd::FootnoteDefinition) => current = None,
            Event::FootnoteReference(label) => {
                let label = label.to_lowercase();
                match &current {
                    Some(note) => in_note.entry(note.clone()).or_default().push(label),
                    None => in_body.push(label),
                }
            }
            _ => {}
        }
    }
    let mut order: Vec<String> = Vec::new();
    let mut numbers: HashMap<String, (u32, u32)> = HashMap::new();
    let mut count = |label: &String, order: &mut Vec<String>| {
        let next = numbers.len() as u32 + 1;
        let entry = numbers.entry(label.clone()).or_insert_with(|| {
            order.push(label.clone());
            (next, 0)
        });
        entry.1 += 1;
    };
    for label in &in_body {
        count(label, &mut order);
    }
    let mut at = 0;
    while at < order.len() {
        if let Some(references) = in_note.get(&order[at]) {
            for label in references {
                count(label, &mut order);
            }
        }
        at += 1;
    }
    numbers
}

/// Reads `source`, which starts at `base` in the whole text. `known` answers references to definitions before `base`.
/// `have(hash)` says the caller already holds that block, whose events are then skipped rather than built into a tree.
pub(super) fn parse_top(source: &str, base: usize, known: &Defs, extra_hash: u64, options: ParseOptions, html_on: bool, mut have: impl FnMut(u64) -> bool) -> Parsed {
    let mut callback = |broken: BrokenLink<'_>| {
        known.get(&normalize_label(&broken.reference)).map(|(dest, title)| (CowStr::from(dest.clone()), CowStr::from(title.clone())))
    };
    let parser = Parser::new_with_broken_link_callback(source, cmark_options(), Some(&mut callback));
    let mut defs: Defs = HashMap::new();
    for (label, def) in parser.reference_definitions().iter() {
        defs.insert(normalize_label(label), (def.dest.to_string(), def.title.as_ref().map_or(String::new(), |t| t.to_string())));
    }
    // What a block is hashed against is every definition the whole text has, whichever side of `base` it is on.
    let mut all = known.clone();
    all.extend(defs.iter().map(|(k, v)| (k.clone(), v.clone())));
    let hash_of_defs = defs_hash(&all) ^ extra_hash;

    let mut items = Vec::new();
    let mut builder = Builder::new(options, html_on);
    // What a block's footnote numbers are depends on all the others, and the count of them on going through every one.
    let has_notes = source.contains("[^");
    if has_notes {
        builder.notes = footnote_numbers(source);
    }
    let mut depth = 0usize;
    let mut skipping = false;
    let mut current: Option<(usize, u64, u32)> = None;
    let mut group: Option<HtmlGroup> = None;
    let finish_group = |group: HtmlGroup, end: usize, builder: &mut Builder, items: &mut Vec<TopItem>, source: &str| {
        let blocks = html::html_to_blocks(&group.html, group.children, &mut builder.next_uid, options);
        let hash = block_hash(&source[group.start..end], hash_of_defs);
        items.push(TopItem { hash, start: group.start + base, footnote: 0, blocks: Some(blocks) });
    };
    for (event, range) in parser.into_offset_iter() {
        let starts = matches!(event, Event::Start(_));
        let ends = matches!(event, Event::End(_));
        if ends {
            depth = depth.saturating_sub(1);
        }
        if depth == 0 && !ends {
            // A block begins at the top. Everything it is, is known from its span before any of it is built.
            let hash = block_hash(&source[range.clone()], hash_of_defs);
            let is_html = matches!(event, Event::Start(Tag::HtmlBlock));
            let footnote = match &event {
                Event::Start(Tag::FootnoteDefinition(label)) => builder.notes.get(&label.to_lowercase()).map_or(u32::MAX, |n| n.0),
                _ => 0,
            };
            current = Some((range.start, hash, footnote));
            if group.is_none() {
                builder.next_uid = 0;
            }
            if group.is_none() && !(is_html && html_on) && !has_notes && have(hash) {
                if starts {
                    skipping = true;
                    depth += 1;
                } else {
                    items.push(TopItem { hash, start: range.start + base, footnote, blocks: None });
                }
                continue;
            }
        }
        if starts {
            depth += 1;
        }
        if skipping {
            if depth == 0 {
                skipping = false;
                let (start, hash, footnote) = current.take().expect("set at the start of the block");
                items.push(TopItem { hash, start: start + base, footnote, blocks: None });
            }
            continue;
        }
        let html_ended = matches!(event, Event::End(TagEnd::HtmlBlock));
        builder.event(event);
        if depth == 0 {
            let (start, hash, footnote) = current.take().expect("set at the start of the block");
            if html_ended && html_on {
                let text = std::mem::take(&mut builder.html_text);
                let delta = open_containers(&text);
                match &mut group {
                    Some(g) => {
                        g.html.push_str(&text);
                        g.html.push('\n');
                        g.depth += delta;
                        if g.depth <= 0 {
                            let done = group.take().expect("just matched");
                            finish_group(done, range.end, &mut builder, &mut items, source);
                        }
                    }
                    None if delta > 0 => group = Some(HtmlGroup { html: format!("{text}\n"), children: Vec::new(), depth: delta, start }),
                    None => {
                        let blocks = html::html_to_blocks(&text, Vec::new(), &mut builder.next_uid, options);
                        items.push(TopItem { hash, start: start + base, footnote: 0, blocks: Some(blocks) });
                    }
                }
            } else {
                let blocks = builder.take_root();
                match &mut group {
                    Some(g) => {
                        // A marker the HTML parser carries through, standing where these blocks go: `rehype-raw`'s stitches.
                        let at = g.children.len();
                        g.children.push(Some(blocks));
                        g.html.push_str(&format!("<{tag} i=\"{at}\"></{tag}>\n", tag = html::BLOCK_MARK));
                    }
                    // A definition nothing refers to is not shown.
                    None if footnote == u32::MAX => {}
                    None => items.push(TopItem { hash, start: start + base, footnote, blocks: Some(blocks) }),
                }
            }
        }
    }
    // A tag that never closed holds everything to the end of the text, as a browser would.
    if let Some(g) = group.take() {
        finish_group(g, source.len(), &mut builder, &mut items, source);
    }
    Parsed { items, defs }
}

struct HtmlGroup {
    html: String,
    children: Vec<Option<Vec<Block>>>,
    depth: i32,
    start: usize,
}

/// Everything as blocks, for a caller with no use for reuse.
pub fn parse_markdown(content: &str, options: ParseOptions) -> Vec<Block> {
    let normalized = super::math::normalized(content);
    let prepared = super::math::prepare_math(&normalized, options.single_dollar_math);
    let html_on = options.render_html && html::may_contain_html(content);
    let parsed = parse_top(&prepared, 0, &HashMap::new(), footnote_labels_hash(&prepared), options, html_on, |_| false);
    let (mut body, mut notes) = (Vec::new(), Vec::new());
    for item in parsed.items {
        let blocks = item.blocks.unwrap_or_default();
        if item.footnote > 0 { notes.push((item.footnote, blocks)) } else { body.extend(blocks) }
    }
    notes.sort_by_key(|(number, _)| *number);
    body.extend(notes.into_iter().flat_map(|(_, blocks)| blocks));
    body
}

// ---------------------------------------------------------------- the URL filter and link rules

/// `defaultUrlTransform`: a relative address, or one of the protocols the web and mail use. Anything else is dropped.
fn safe_url(value: &str) -> bool {
    let Some(colon) = value.find(':') else { return true };
    let before = |c: char| value.find(c).is_some_and(|at| colon > at);
    if before('/') || before('?') || before('#') {
        return true;
    }
    matches!(value[..colon].to_ascii_lowercase().as_str(), "http" | "https" | "irc" | "ircs" | "mailto" | "xmpp")
}

/// `markdownUrlTransform` for an `href`: `file:` names a file this app can open.
fn href_filter(url: &str) -> Option<&str> {
    (url.trim().to_ascii_lowercase().starts_with("file:") || safe_url(url)).then_some(url)
}

/// `markdownUrlTransform` for a `src`: a `data:` image is already a picture, `attachment:` a notebook cell's.
pub(super) fn src_filter(url: &str) -> Option<&str> {
    let lower = url.trim().to_ascii_lowercase();
    (lower.starts_with("data:image/") || lower.starts_with("attachment:") || safe_url(url)).then_some(url)
}

/// What the `a` override does with an `href`.
pub(super) fn classify_link(href: &str, title: Option<String>, options: ParseOptions) -> Rc<Link> {
    static OTHER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(?:mailto|xmpp|ircs?):").expect("regex"));
    let inert = |title| Rc::new(Link { target: LinkTarget::Inert, title });
    let Some(href) = href_filter(href).filter(|h| !h.is_empty()) else { return inert(title) };
    if let Some(fragment) = href.strip_prefix('#') {
        return Rc::new(Link { target: LinkTarget::Fragment(fragment.to_owned()), title });
    }
    if external_http_url(href).is_some() {
        return Rc::new(Link { target: LinkTarget::External(href.to_owned()), title });
    }
    if OTHER.is_match(href) {
        return Rc::new(Link { target: LinkTarget::Protocol(href.to_owned()), title });
    }
    let local = local_link_target(href);
    if options.document_links {
        return Rc::new(Link { target: LinkTarget::Document(href.to_owned()), title });
    }
    let Some(local) = local.filter(|l| !l.path.is_empty()) else { return inert(title) };
    if !options.linkify_paths {
        return inert(title);
    }
    // A link to a file is a detected path with a label of its own.
    let title = match local.line {
        Some(line) => format!("{}:{line}", local.path),
        None => local.path.clone(),
    };
    Rc::new(Link { target: LinkTarget::Path { target: local.path, line: local.line }, title: Some(title) })
}

// ---------------------------------------------------------------- autolink literals

/// A run of prose after autolinking.
enum Auto<'a> {
    Text(&'a str),
    Link { text: String, href: String },
}

fn before_is_boundary(text: &str, at: usize, email: bool) -> bool {
    match text[..at].chars().next_back() {
        None => true,
        Some('/') if email => false,
        Some(c) => c.is_whitespace() || c.is_ascii_punctuation() || (!c.is_ascii() && !c.is_alphanumeric()),
    }
}

fn correct_domain(domain: &str) -> bool {
    let parts: Vec<&str> = domain.split('.').collect();
    if parts.len() < 2 {
        return false;
    }
    let bad = |part: &str| !part.is_empty() && (part.contains('_') || !part.bytes().any(|b| b.is_ascii_alphanumeric()));
    !bad(parts[parts.len() - 1]) && !bad(parts[parts.len() - 2])
}

/// The link and whatever trailing punctuation belongs to the sentence instead.
fn split_url(url: &str) -> (&str, &str) {
    let trail_start = url.rfind(|c: char| !"!\"&'),.:;<>?]}".contains(c)).map_or(0, |at| at + url[at..].chars().next().map_or(0, char::len_utf8));
    if trail_start == url.len() {
        return (url, "");
    }
    let mut end = trail_start;
    let trail = &url[trail_start..];
    let opening = url[..trail_start].matches('(').count();
    let mut closing = url[..trail_start].matches(')').count();
    let mut consumed = 0;
    while opening > closing {
        let Some(paren) = trail[consumed..].find(')') else { break };
        consumed += paren + 1;
        end = trail_start + consumed;
        closing += 1;
    }
    (&url[..end], &url[end..])
}

/// `mdast-util-gfm-autolink-literal`: bare URLs and `www.` first, then email addresses in what is left.
fn autolink(text: &str) -> Vec<Auto<'_>> {
    static URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)(https?://|www)([-.A-Za-z0-9_]+)([^ \t\r\n]*)").expect("regex"));
    static EMAIL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([-.A-Za-z0-9_+]+)@([-A-Za-z0-9_]+(?:\.[-A-Za-z0-9_]+)+)").expect("regex"));
    let mut out = Vec::new();
    let emails = |slice: &'_ str, out: &mut Vec<(usize, usize, String, String)>, base: usize| {
        if !slice.contains('@') {
            return;
        }
        for c in EMAIL.captures_iter(slice) {
            let whole = c.get(0).expect("whole");
            let label = c.get(2).expect("label").as_str();
            if !before_is_boundary(slice, whole.start(), true) || label.ends_with(['-', '_']) || label.ends_with(|ch: char| ch.is_ascii_digit()) {
                continue;
            }
            out.push((base + whole.start(), base + whole.end(), whole.as_str().to_owned(), format!("mailto:{}", whole.as_str())));
        }
    };
    let mut found: Vec<(usize, usize, String, String)> = Vec::new();
    let mut plain_from = 0;
    if text.contains("://") || text.contains("www.") || text.contains("WWW.") {
        for c in URL.captures_iter(text) {
            let whole = c.get(0).expect("whole");
            let (protocol, domain, path) = (&c[1], &c[2], &c[3]);
            if !before_is_boundary(text, whole.start(), false) {
                continue;
            }
            let www = protocol.eq_ignore_ascii_case("www");
            if www && !domain.starts_with('.') {
                continue;
            }
            let domain_full = if www { format!("{protocol}{domain}") } else { domain.to_owned() };
            if !correct_domain(&domain_full) {
                continue;
            }
            let joined = format!("{domain_full}{path}");
            let (kept, _) = split_url(&joined);
            if kept.is_empty() {
                continue;
            }
            let (shown, href) = if www { (kept.to_owned(), format!("http://{kept}")) } else { (format!("{protocol}{kept}"), format!("{protocol}{kept}")) };
            let end = whole.start() + shown.len();
            emails(&text[plain_from..whole.start()], &mut found, plain_from);
            found.push((whole.start(), end, shown, href));
            plain_from = end;
        }
    }
    emails(&text[plain_from..], &mut found, plain_from);
    let mut at = 0;
    for (start, end, shown, href) in found {
        if start > at {
            out.push(Auto::Text(&text[at..start]));
        }
        out.push(Auto::Link { text: shown, href });
        at = end;
    }
    if at < text.len() {
        out.push(Auto::Text(&text[at..]));
    }
    out
}

// ---------------------------------------------------------------- events into blocks

/// An inline as a paragraph is being read: raw HTML is kept in order until the paragraph ends and the HTML parser can see
/// all of it.
pub(super) enum Raw {
    Inline(Inline),
    Html(String),
}

#[derive(Default)]
struct Frame {
    blocks: Vec<Block>,
    raw: Vec<Raw>,
}

/// Past this a quote in a quote is drawn at the depth it has. A message is untrusted input, and everything that walks the
/// tree recurses.
const MAX_NESTING: usize = 48;

struct Builder {
    options: ParseOptions,
    html_on: bool,
    frames: Vec<Frame>,
    overflow: usize,
    styles: Vec<InlineStyle>,
    links: Vec<Rc<Link>>,
    /// Adjacent text events: pulldown-cmark splits at every character that might have been markup, and an address has to be
    /// seen whole to be recognized.
    text: String,
    images: Vec<(String, String)>,
    code: Option<(Option<String>, String)>,
    html_text: String,
    in_html: bool,
    lists: Vec<(Option<u64>, Vec<ListItem>)>,
    tasks: Vec<Option<bool>>,
    table: Option<TableBuild>,
    footnotes: Vec<String>,
    /// Each footnote's number and how many references it has, and how many of those have been read.
    notes: HashMap<String, (u32, u32)>,
    note_refs: HashMap<String, u32>,
    next_uid: u32,
}

struct TableBuild {
    aligns: Vec<TextAlign>,
    rows: Vec<TableRow>,
    cells: Vec<TableCell>,
    head: bool,
}

fn align_of(a: Alignment) -> TextAlign {
    match a {
        Alignment::Center => TextAlign::Center,
        Alignment::Right => TextAlign::End,
        Alignment::Left | Alignment::None => TextAlign::Start,
    }
}

/// A fence's language, as the fence says it: the first word of its info string.
pub(super) fn fence_label(info: &str) -> Option<String> {
    info.split_whitespace().next().map(str::to_owned)
}

/// `fenceLanguage`: the grammar a label names. `{.rust}` and `rust,ignore` are how some tools write one.
pub(super) fn fence_grammar(label: &str) -> Option<Lang> {
    let first = label.trim().split(|c: char| c.is_whitespace() || c == '{' || c == ',').find(|s| !s.is_empty())?;
    lang_of(first.strip_prefix('.').unwrap_or(first))
}

impl Builder {
    fn new(options: ParseOptions, html_on: bool) -> Self {
        Self { options, html_on, frames: vec![Frame::default()], overflow: 0, styles: vec![InlineStyle::default()], links: Vec::new(), text: String::new(), images: Vec::new(), code: None, html_text: String::new(), in_html: false, lists: Vec::new(), tasks: Vec::new(), table: None, footnotes: Vec::new(), notes: HashMap::new(), note_refs: HashMap::new(), next_uid: 0 }
    }

    fn uid(&mut self) -> u32 {
        self.next_uid += 1;
        self.next_uid
    }

    fn style(&self) -> InlineStyle {
        *self.styles.last().expect("the base style stays")
    }

    fn push_style(&mut self, f: impl FnOnce(&mut InlineStyle)) {
        let mut s = self.style();
        f(&mut s);
        self.styles.push(s);
    }

    fn frame(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("the root frame stays")
    }

    fn open_frame(&mut self) {
        self.flush_tight();
        if self.frames.len() >= MAX_NESTING {
            self.overflow += 1;
        } else {
            self.frames.push(Frame::default());
        }
    }

    fn close_frame(&mut self) -> Vec<Block> {
        self.flush_tight();
        if self.overflow > 0 {
            self.overflow -= 1;
            return Vec::new();
        }
        self.frames.pop().map_or(Vec::new(), |f| f.blocks)
    }

    fn take_root(&mut self) -> Vec<Block> {
        self.flush_tight();
        std::mem::take(&mut self.frame().blocks)
    }

    fn push_inline(&mut self, inline: Inline) {
        self.frame().raw.push(Raw::Inline(inline));
    }

    fn flush_text(&mut self) {
        if self.text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.text);
        let style = self.style();
        if let Some(link) = self.links.last().cloned() {
            self.push_inline(Inline::Text { text, style, link: Some(link) });
            return;
        }
        // Cheap rejection first: this runs on every paragraph of every parse.
        if !text.contains(['/', '\\', '@']) && !text.contains("www.") {
            self.push_inline(Inline::Text { text, style, link: None });
            return;
        }
        let options = self.options;
        for run in autolink(&text) {
            match run {
                Auto::Link { text, href } => {
                    let link = classify_link(&href, None, options);
                    self.push_inline(Inline::Text { text, style, link: Some(link) });
                }
                Auto::Text(prose) => self.prose(prose, style),
            }
        }
    }

    fn prose(&mut self, text: &str, style: InlineStyle) {
        if !self.options.linkify_paths || !text.contains(['/', '\\']) {
            self.push_inline(Inline::Text { text: text.to_owned(), style, link: None });
            return;
        }
        for run in split_path_runs(text) {
            match run {
                PathRun::Text(text) => self.push_inline(Inline::Text { text, style, link: None }),
                PathRun::Path { display, target, line } => {
                    let link = Rc::new(Link { title: Some(target.clone()), target: LinkTarget::Path { target, line } });
                    self.push_inline(Inline::Text { text: display, style, link: Some(link) });
                }
            }
        }
    }

    fn code_span(&mut self, code: &str) {
        let style = self.style();
        let link = self.links.last().cloned();
        if let Some(source) = code.strip_prefix(MATH_OPEN).and_then(|s| s.strip_suffix(MATH_CLOSE)) {
            self.push_inline(Inline::Math { source: source.trim().to_owned(), style, link });
            return;
        }
        let style = InlineStyle { code: true, ..style };
        if self.options.linkify_paths && link.is_none() && code.contains(['/', '\\'])
            && let Some((display, target, line)) = inline_code_path(code)
        {
            let link = Rc::new(Link { title: Some(target.clone()), target: LinkTarget::Path { target, line } });
            self.push_inline(Inline::Text { text: display, style, link: Some(link) });
            return;
        }
        self.push_inline(Inline::Text { text: code.to_owned(), style, link });
    }

    /// What has gathered in the current frame, with raw HTML resolved.
    fn take_inlines(&mut self) -> Vec<Inline> {
        self.flush_text();
        let raw = std::mem::take(&mut self.frame().raw);
        let mut inlines = if raw.iter().any(|r| matches!(r, Raw::Html(_))) {
            html::stitch_inlines(raw, self.options)
        } else {
            raw.into_iter().filter_map(|r| if let Raw::Inline(i) = r { Some(i) } else { None }).collect()
        };
        while matches!(inlines.last(), Some(Inline::Break)) {
            inlines.pop();
        }
        while matches!(inlines.first(), Some(Inline::Break)) {
            inlines.remove(0);
        }
        inlines
    }

    /// A tight item's text arrives with no paragraph around it, and ends where the next block starts.
    fn flush_tight(&mut self) {
        let inlines = self.take_inlines();
        if !inlines.is_empty() {
            self.frame().blocks.push(Block::Paragraph { inlines, tight: true });
        }
    }

    fn push_block(&mut self, block: Block) {
        self.flush_tight();
        self.frame().blocks.push(block);
    }

    fn event(&mut self, event: Event<'_>) {
        if !matches!(event, Event::Text(_)) {
            self.flush_text();
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if let Some((_, code)) = &mut self.code {
                    code.push_str(&text);
                } else if self.in_html {
                    self.html_text.push_str(&text);
                } else if let Some((_, alt)) = self.images.last_mut() {
                    alt.push_str(&text);
                } else {
                    self.text.push_str(&text);
                }
            }
            Event::Code(code) => {
                if let Some((_, alt)) = self.images.last_mut() {
                    alt.push_str(&code);
                } else {
                    self.code_span(&code);
                }
            }
            Event::InlineMath(math) | Event::DisplayMath(math) => {
                let (style, link) = (self.style(), self.links.last().cloned());
                self.push_inline(Inline::Math { source: math.trim().to_owned(), style, link });
            }
            Event::Html(html) => self.html_text.push_str(&html),
            Event::InlineHtml(html) => {
                if self.html_on {
                    self.frame().raw.push(Raw::Html(html.to_string()));
                } else {
                    // Text a person typed renders exactly as typed.
                    let style = self.style();
                    let link = self.links.last().cloned();
                    self.push_inline(Inline::Text { text: html.to_string(), style, link });
                }
            }
            // `remark-breaks`: a line break in the source is one on the page.
            Event::SoftBreak | Event::HardBreak => {
                if self.images.is_empty() {
                    self.push_inline(Inline::Break);
                }
            }
            Event::Rule => self.push_block(Block::Rule),
            Event::TaskListMarker(done) => {
                if let Some(task) = self.tasks.last_mut() {
                    *task = Some(done);
                }
            }
            // `<sup><a href="#user-content-fn-x" id="user-content-fnref-x">1</a></sup>`, showing where it comes among the notes.
            Event::FootnoteReference(label) => {
                let label = label.to_lowercase();
                let number = self.notes.get(&label).map_or(0, |n| n.0);
                let seen = self.note_refs.entry(label.clone()).or_insert(0);
                *seen += 1;
                let id = if *seen > 1 { format!("user-content-fnref-{label}-{seen}") } else { format!("user-content-fnref-{label}") };
                self.push_inline(Inline::Anchor(id));
                let style = InlineStyle { sup: true, ..self.style() };
                let link = Rc::new(Link { target: LinkTarget::Fragment(format!("user-content-fn-{label}")), title: None });
                self.push_inline(Inline::Text { text: number.to_string(), style, link: Some(link) });
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph | Tag::Heading { .. } => self.flush_tight(),
            Tag::BlockQuote(_) => self.open_frame(),
            Tag::FootnoteDefinition(label) => {
                self.footnotes.push(label.to_lowercase());
                self.open_frame();
            }
            Tag::CodeBlock(kind) => {
                self.flush_tight();
                let label = match kind {
                    CodeBlockKind::Fenced(info) => fence_label(&info),
                    CodeBlockKind::Indented => None,
                };
                self.code = Some((label, String::new()));
            }
            Tag::List(start) => {
                self.flush_tight();
                self.lists.push((start, Vec::new()));
            }
            Tag::Item => {
                self.tasks.push(None);
                self.open_frame();
            }
            Tag::Table(aligns) => {
                self.flush_tight();
                self.table = Some(TableBuild { aligns: aligns.into_iter().map(align_of).collect(), rows: Vec::new(), cells: Vec::new(), head: false });
            }
            Tag::TableHead => {
                if let Some(t) = &mut self.table {
                    t.head = true;
                }
            }
            Tag::Emphasis => self.push_style(|s| s.italic = true),
            Tag::Strong => self.push_style(|s| s.bold = true),
            Tag::Strikethrough => self.push_style(|s| s.strike = true),
            Tag::Link { link_type, dest_url, title, .. } => {
                let href = if matches!(link_type, LinkType::Email) { format!("mailto:{dest_url}") } else { dest_url.to_string() };
                let title = Some(title.to_string()).filter(|t| !t.is_empty());
                self.links.push(classify_link(&href, title, self.options));
            }
            Tag::Image { dest_url, .. } => self.images.push((dest_url.to_string(), String::new())),
            Tag::HtmlBlock => {
                self.flush_tight();
                self.in_html = true;
                self.html_text.clear();
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                let inlines = self.take_inlines();
                // A line with nothing on it but an anchor takes no room on the page.
                if inlines.iter().all(|i| matches!(i, Inline::Anchor(_))) {
                    let anchors = inlines.into_iter().filter_map(|i| if let Inline::Anchor(a) = i { Some(Block::Anchor(a)) } else { None });
                    self.frame().blocks.extend(anchors);
                } else {
                    self.frame().blocks.push(Block::Paragraph { inlines, tight: false });
                }
            }
            TagEnd::Heading(level) => {
                let inlines = self.take_inlines();
                let level = match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    HeadingLevel::H4 => 4,
                    HeadingLevel::H5 => 5,
                    HeadingLevel::H6 => 6,
                };
                self.frame().blocks.push(Block::Heading { level, inlines, id: None });
            }
            TagEnd::BlockQuote(_) => {
                let blocks = self.close_frame();
                self.push_block(Block::Quote(blocks));
            }
            TagEnd::FootnoteDefinition => {
                let mut blocks = self.close_frame();
                let label = self.footnotes.pop().unwrap_or_default();
                let (number, references) = self.notes.get(&label).copied().unwrap_or((0, 0));
                back_references(&mut blocks, &label, references);
                self.push_block(Block::Footnote { number, label, blocks });
            }
            TagEnd::CodeBlock => {
                if let Some((label, code)) = self.code.take() {
                    // `\n` at the end is the fence's own line ending, not a line of the code.
                    let source = code.strip_suffix('\n').unwrap_or(&code);
                    let uid = self.uid();
                    let block = if label.as_deref() == Some("math") {
                        Block::Math { uid, source: source.trim().to_owned() }
                    } else {
                        let language = label.as_deref().and_then(fence_grammar);
                        Block::Code { uid, label, language, code: Arc::from(source), chrome: true }
                    };
                    self.push_block(block);
                }
            }
            TagEnd::List(_) => {
                if let Some((start, items)) = self.lists.pop() {
                    self.push_block(Block::List { start, items });
                }
            }
            TagEnd::Item => {
                let blocks = self.close_frame();
                let task = self.tasks.pop().flatten();
                if let Some((_, items)) = self.lists.last_mut() {
                    items.push(ListItem { task, blocks });
                }
            }
            TagEnd::TableCell => {
                let inlines = self.take_inlines();
                if let Some(t) = &mut self.table {
                    let align = t.aligns.get(t.cells.len()).copied().unwrap_or(TextAlign::Start);
                    let header = t.head;
                    t.cells.push(TableCell { inlines, blocks: None, align, colspan: 1, rowspan: 1, header });
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(t) = &mut self.table {
                    let cells = std::mem::take(&mut t.cells);
                    t.rows.push(TableRow { header: t.head, cells, section: 0 });
                    t.head = false;
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    let uid = self.uid();
                    self.push_block(Block::Table { uid, rows: t.rows });
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.links.pop();
            }
            TagEnd::Image => {
                if let Some((src, alt)) = self.images.pop() {
                    if let Some((_, outer)) = self.images.last_mut() {
                        outer.push_str(&alt);
                    } else {
                        let link = self.links.last().cloned();
                        // A filtered address leaves the description standing, as an empty `src` does upstream.
                        let src = src_filter(&src).unwrap_or("").to_owned();
                        self.push_inline(Inline::Image { src, alt, width: None, float: None, link });
                    }
                }
            }
            TagEnd::HtmlBlock => {
                self.in_html = false;
                if self.html_on {
                    // At the top of the message `parse_top` takes it from here, and what it holds open can reach past it. Inside a
                    // quote or a list it ends where its container does.
                    if self.frames.len() > 1 {
                        let text = std::mem::take(&mut self.html_text);
                        let mut uid = self.next_uid;
                        let blocks = html::html_to_blocks(&text, Vec::new(), &mut uid, self.options);
                        self.next_uid = uid;
                        self.flush_tight();
                        self.frame().blocks.extend(blocks);
                    }
                } else {
                    // Shown as the text it is, with no `<p>` of its own and whitespace collapsed as a page collapses it.
                    let text = std::mem::take(&mut self.html_text);
                    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
                    if !collapsed.is_empty() {
                        let inline = Inline::Text { text: collapsed, style: InlineStyle::default(), link: None };
                        self.push_block(Block::Paragraph { inlines: vec![inline], tight: true });
                    }
                }
            }
            _ => {}
        }
    }
}

/// The way back: `↩`, one for each reference to the note, at the end of its last paragraph, or on its own after anything else.
fn back_references(blocks: &mut Vec<Block>, label: &str, references: u32) {
    let mut links = Vec::new();
    for n in 1..=references.max(1) {
        let id = if n > 1 { format!("user-content-fnref-{label}-{n}") } else { format!("user-content-fnref-{label}") };
        let link = Some(Rc::new(Link { target: LinkTarget::Fragment(id), title: None }));
        links.push(Inline::Text { text: " ".to_owned(), style: InlineStyle::default(), link: None });
        // `↩` is drawn from the emoji font by the text layer, where a page's fonts draw it plain: this is the same arrow with no such
        // font to go to.
        links.push(Inline::Text { text: "⤶".to_owned(), style: InlineStyle::default(), link: link.clone() });
        if n > 1 {
            links.push(Inline::Text { text: n.to_string(), style: InlineStyle { sup: true, ..Default::default() }, link });
        }
    }
    if let Some(Block::Paragraph { inlines, .. }) = blocks.last_mut() {
        inlines.extend(links);
    } else {
        links.remove(0);
        blocks.push(Block::Paragraph { inlines: links, tight: true });
    }
}
