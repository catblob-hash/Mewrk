//! An HTML file, reduced to what can be shown without running it (upstream's `lib/htmlPreview.ts`). Parsed here, stripped
//! of everything that runs or reaches out (scripts, handlers, frames, forms that post, `javascript:` addresses), with
//! everything it references in the workspace collected for the caller to read. Nothing is fetched by the engine.

use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use regex::{Captures, Regex};
use std::collections::BTreeMap;
use std::sync::LazyLock;

#[derive(Clone, Debug, PartialEq)]
pub enum HtmlNode {
    Text(String),
    Element(HtmlElement),
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct HtmlElement {
    pub tag: String,
    pub props: BTreeMap<String, String>,
    pub children: Vec<HtmlNode>,
}

impl HtmlElement {
    pub fn text(&self) -> String {
        let mut out = String::new();
        collect_text(&self.children, &mut out);
        out
    }

    pub fn find(&self, tag: &str) -> Option<&HtmlElement> {
        find_in(&self.children, tag)
    }
}

pub fn find_in<'a>(nodes: &'a [HtmlNode], tag: &str) -> Option<&'a HtmlElement> {
    for node in nodes {
        if let HtmlNode::Element(e) = node {
            if e.tag == tag {
                return Some(e);
            }
            if let Some(found) = find_in(&e.children, tag) {
                return Some(found);
            }
        }
    }
    None
}

fn collect_text(nodes: &[HtmlNode], out: &mut String) {
    for node in nodes {
        match node {
            HtmlNode::Text(t) => out.push_str(t),
            HtmlNode::Element(e) => collect_text(&e.children, out),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum HtmlStyleSource {
    Inline(String),
    Link { href: String, media: Option<String> },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HtmlPreviewDocument {
    pub title: Option<String>,
    /// Attributes the document put on `<html>` and `<body>`.
    pub root_props: BTreeMap<String, String>,
    pub body_props: BTreeMap<String, String>,
    pub body: Vec<HtmlNode>,
    /// Every stylesheet, in the order the cascade has to see them.
    pub styles: Vec<HtmlStyleSource>,
    /// Workspace images the body refers to, as written.
    pub images: Vec<String>,
    /// Whether the page had scripts, which the preview never runs.
    pub had_scripts: bool,
}

/// Elements that are removed along with everything inside them.
const DROPPED: &[&str] = &["script", "template", "base", "meta", "link", "style", "title", "noframes", "param", "source", "track", "slot"];
/// Elements that would load or run something of their own; each leaves a placeholder behind.
const EMBEDS: &[&str] = &["iframe", "frame", "frameset", "object", "embed", "applet", "portal", "video", "audio", "canvas"];
/// Attributes that act rather than describe.
const DROPPED_PROPS: &[&str] = &["action", "formaction", "srcdoc", "ping", "http-equiv", "autofocus", "nonce", "integrity", "crossorigin", "referrerpolicy", "background", "srcset", "sizes", "target", "is", "popovertarget", "popovertargetaction", "commandfor", "command", "dynsrc", "lowsrc", "manifest"];

static DANGEROUS_URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^\s*(?:javascript|vbscript|data)\s*:").expect("regex"));
static REMOTE_URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^\s*(?:[a-z][a-z0-9+.-]*:|//)").expect("regex"));
static DATA_IMAGE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^\s*data:image/").expect("regex"));

pub fn is_remote(reference: &str) -> bool {
    REMOTE_URL.is_match(reference)
}

/// The parser's tree, with nothing taken out yet.
fn raw_children(handle: &Handle) -> Vec<HtmlNode> {
    let mut out = Vec::new();
    for child in handle.children.borrow().iter() {
        match &child.data {
            NodeData::Text { contents } => out.push(HtmlNode::Text(contents.borrow().to_string())),
            NodeData::Element { name, attrs, .. } => {
                let mut props = BTreeMap::new();
                for attr in attrs.borrow().iter() {
                    let key = match &attr.name.prefix {
                        Some(prefix) => format!("{prefix}:{}", attr.name.local),
                        None => attr.name.local.to_string(),
                    };
                    props.insert(key, attr.value.to_string());
                }
                out.push(HtmlNode::Element(HtmlElement { tag: name.local.to_string(), props, children: raw_children(child) }));
            }
            _ => {}
        }
    }
    out
}

fn parse(source: &str) -> Vec<HtmlNode> {
    let dom = html5ever::parse_document(RcDom::default(), Default::default()).from_utf8().one(source.as_bytes());
    raw_children(&dom.document)
}

#[derive(Default)]
struct Collector {
    styles: Vec<HtmlStyleSource>,
    images: Vec<String>,
    title: Option<String>,
    had_scripts: bool,
}

impl Collector {
    fn image(&mut self, source: &str) {
        if !self.images.iter().any(|i| i == source) {
            self.images.push(source.to_owned());
        }
    }
}

/// Records what a dropped element contributes before it goes.
fn collect(element: &HtmlElement, c: &mut Collector) {
    match element.tag.as_str() {
        "script" => c.had_scripts = true,
        "style" => c.styles.push(HtmlStyleSource::Inline(element.text())),
        "link" => {
            let stylesheet = element.props.get("rel").is_some_and(|r| r.to_lowercase().split_whitespace().any(|w| w == "stylesheet"));
            if let Some(href) = element.props.get("href").filter(|h| !h.is_empty() && stylesheet) {
                c.styles.push(HtmlStyleSource::Link { href: href.clone(), media: element.props.get("media").cloned() });
            }
        }
        "title" => {
            if c.title.is_none() {
                let t = element.text();
                c.title = Some(t.trim().to_owned()).filter(|t| !t.is_empty());
            }
        }
        _ => {}
    }
}

fn clean_props(element: &HtmlElement, c: &mut Collector) -> BTreeMap<String, String> {
    let mut props: BTreeMap<String, String> = element.props.iter().filter(|(k, _)| !k.to_lowercase().starts_with("on") && !DROPPED_PROPS.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
    for key in ["href", "xlink:href"] {
        let Some(value) = props.get(key).cloned() else { continue };
        // A link is followed by the preview's own click handler, never by the engine; one that would execute is not kept.
        if DANGEROUS_URL.is_match(&value) || (key == "xlink:href" && !value.starts_with('#') && element.tag != "a") {
            props.remove(key);
        }
    }
    if let Some(source) = props.remove("src") {
        let is_image = element.tag == "img" || (element.tag == "input" && props.get("type").map(String::as_str) == Some("image"));
        if is_image && DATA_IMAGE.is_match(&source) {
            props.insert("src".to_owned(), source);
        } else if is_image && !REMOTE_URL.is_match(&source) {
            // Read from the workspace by the caller; drawn once it has the bytes.
            c.image(&source);
            props.insert("data-mw-src".to_owned(), source);
        } else if is_image {
            props.insert("data-mw-remote-src".to_owned(), source);
        }
    }
    // SVG `<image href="…">` is a picture too.
    if element.tag == "image" {
        let href = props.remove("href").or_else(|| props.remove("xlink:href")).or_else(|| element.props.get("href").cloned());
        props.remove("xlink:href");
        if let Some(href) = href {
            if DATA_IMAGE.is_match(&href) {
                props.insert("href".to_owned(), href);
            } else if !REMOTE_URL.is_match(&href) {
                c.image(&href);
                props.insert("data-mw-src".to_owned(), href);
            }
        }
    }
    props
}

/// An SVG animation that rewrites a link could make a static `href` execute when clicked.
fn animates_link(e: &HtmlElement) -> bool {
    (e.tag == "animate" || e.tag == "set") && e.props.get("attributename").or_else(|| e.props.get("attributeName")).is_some_and(|a| a.to_lowercase().contains("href"))
}

fn sanitize(children: &[HtmlNode], c: &mut Collector) -> Vec<HtmlNode> {
    let mut result = Vec::new();
    for child in children {
        let HtmlNode::Element(e) = child else {
            result.push(child.clone());
            continue;
        };
        if DROPPED.contains(&e.tag.as_str()) {
            collect(e, c);
            continue;
        }
        if animates_link(e) {
            continue;
        }
        if EMBEDS.contains(&e.tag.as_str()) {
            result.push(HtmlNode::Element(HtmlElement { tag: "mw-embed".to_owned(), props: BTreeMap::from([("data-kind".to_owned(), e.tag.clone())]), children: Vec::new() }));
            continue;
        }
        // Scripts never run here, so what a page says in their absence is exactly what it should show. The parser reads
        // `noscript` as text when scripting is on and as elements when off; either way what is inside is kept.
        if e.tag == "noscript" {
            if let [HtmlNode::Text(markup)] = e.children.as_slice() {
                let fragment = parse(markup);
                if let Some(body) = find_in(&fragment, "body") {
                    result.extend(sanitize(&body.children, c));
                }
            } else {
                result.extend(sanitize(&e.children, c));
            }
            continue;
        }
        let props = clean_props(e, c);
        result.push(HtmlNode::Element(HtmlElement { tag: e.tag.clone(), props, children: sanitize(&e.children, c) }));
    }
    result
}

/// Parses and strips a document, and lists what it needs from the workspace.
pub fn prepare_html_preview(source: &str) -> HtmlPreviewDocument {
    let tree = parse(source);
    let mut c = Collector::default();
    let html = find_in(&tree, "html");
    let head = html.and_then(|h| h.find("head"));
    let body = html.and_then(|h| h.find("body"));
    // The head is walked only for what it contributes: its stylesheets and title.
    if let Some(head) = head {
        sanitize(&head.children, &mut c);
    }
    let content = sanitize(body.map_or(tree.as_slice(), |b| b.children.as_slice()), &mut c);
    let root_props = html.map_or_else(BTreeMap::new, |h| clean_props(h, &mut c));
    let body_props = body.map_or_else(BTreeMap::new, |b| clean_props(b, &mut c));
    HtmlPreviewDocument { title: c.title, root_props, body_props, body: content, styles: c.styles, images: c.images, had_scripts: c.had_scripts }
}

/// Class names that stand in for `<html>` and `<body>`, which cannot exist inside a shadow root.
pub const HTML_ROOT_CLASS: &str = "mw-html-root";
pub const HTML_BODY_CLASS: &str = "mw-html-body";

/// Points a selector at the stand-ins: `html`, `:root` and `body` become the classes of the elements that play them.
pub fn rewrite_selector(selector: &str) -> String {
    let root = Regex::new(r"(?i):root\b").expect("regex").replace_all(selector, format!(".{HTML_ROOT_CLASS}").as_str()).into_owned();
    let swap = |text: &str, word: &str, class: &str| -> String {
        let lower = text.to_lowercase();
        let mut out = String::new();
        let mut at = 0;
        while let Some(found) = lower[at..].find(word) {
            let start = at + found;
            let end = start + word.len();
            let before = text[..start].chars().last();
            let after = text[end..].chars().next();
            let open = before.is_none_or(|c| c.is_whitespace() || ">+~,(".contains(c));
            let close = after.is_none_or(|c| c.is_whitespace() || ">+~,.:#[)".contains(c));
            out.push_str(&text[at..start]);
            if open && close {
                out.push('.');
                out.push_str(class);
            } else {
                out.push_str(&text[start..end]);
            }
            at = end;
        }
        out.push_str(&text[at..]);
        out
    };
    swap(&swap(&root, "html", HTML_ROOT_CLASS), "body", HTML_BODY_CLASS)
}

static CSS_URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)url\(\s*(?:"([^"]*)"|'([^']*)'|([^)\s]*))\s*\)"#).expect("regex"));
static CSS_IMPORT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)@import\s+(?:url\(\s*)?(?:"([^"]*)"|'([^']*)'|([^)\s;]+))\s*\)?\s*([^;]*);"#).expect("regex"));

fn group<'a>(c: &'a Captures<'_>) -> &'a str {
    c.get(1).or_else(|| c.get(2)).or_else(|| c.get(3)).map_or("", |m| m.as_str()).trim()
}

/// Every `url(…)` in a stylesheet that names something in the workspace.
pub fn stylesheet_references(css: &str) -> Vec<String> {
    let mut references: Vec<String> = Vec::new();
    for c in CSS_URL.captures_iter(css) {
        let reference = group(&c);
        if !reference.is_empty() && !REMOTE_URL.is_match(reference) && !reference.starts_with('#') && !references.iter().any(|r| r == reference) {
            references.push(reference.to_owned());
        }
    }
    references
}

/// Every `@import` a stylesheet makes, with the media it was made for.
pub fn stylesheet_imports(css: &str) -> Vec<(String, String)> {
    CSS_IMPORT.captures_iter(css).filter_map(|c| {
        let reference = group(&c);
        (!reference.is_empty()).then(|| (reference.to_owned(), c.get(4).map_or("", |m| m.as_str()).trim().to_owned()))
    }).collect()
}

/// Rewrites a stylesheet's addresses: a workspace reference becomes what `resolve` answers, and anything remote is removed.
pub fn rewrite_stylesheet_urls(css: &str, resolve: &dyn Fn(&str) -> Option<String>) -> String {
    CSS_URL.replace_all(css, |c: &Captures<'_>| {
        let reference = group(c);
        let lower = reference.to_lowercase();
        if reference.is_empty() || reference.starts_with('#') || lower.starts_with("data:") {
            return c[0].to_owned();
        }
        if REMOTE_URL.is_match(reference) {
            return "none".to_owned();
        }
        resolve(reference).map_or_else(|| "none".to_owned(), |r| format!("url(\"{r}\")"))
    }).into_owned()
}

/// Replaces each `@import` with what `inline` answers for it, or with nothing.
pub fn inline_stylesheet_imports(css: &str, inline: &dyn Fn(&str, &str) -> Option<String>) -> String {
    CSS_IMPORT.replace_all(css, |c: &Captures<'_>| inline(group(c), c.get(4).map_or("", |m| m.as_str()).trim()).unwrap_or_default()).into_owned()
}

fn without_query(reference: &str) -> &str {
    reference.split(['?', '#']).next().unwrap_or("")
}

static IMAGE_EXTENSIONS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\.(?:png|apng|jpe?g|jfif|gif|webp|avif|bmp|ico|cur|svg|tiff?)(?:[?#].*)?$").expect("regex"));

/// What a page reads, on its file's machine: stylesheets and pictures, sequential because they run on a task thread.
pub trait HtmlResources {
    fn read_text(&self, path: &str) -> Option<String>;
    fn read_image(&self, path: &str) -> Option<String>;
}

/// One stylesheet, with its imports read in and its pictures turned into `data:` URLs, so nothing in it asks the engine.
pub fn resolve_stylesheet(css: &str, sheet_path: &str, resources: &dyn HtmlResources, depth: u32) -> String {
    let mut imports: BTreeMap<String, Option<String>> = BTreeMap::new();
    if depth < 4 {
        for (reference, _) in stylesheet_imports(css) {
            if REMOTE_URL.is_match(&reference) {
                continue;
            }
            let Some(path) = crate::viewers::resolve_document_reference(sheet_path, without_query(&reference)) else { continue };
            let text = resources.read_text(&path).map(|t| resolve_stylesheet(&t, &path, resources, depth + 1));
            imports.insert(reference, text);
        }
    }
    let inlined = inline_stylesheet_imports(css, &|reference, media| {
        let text = imports.get(reference)?.as_ref()?;
        Some(if media.is_empty() { text.clone() } else { format!("@media {media} {{\n{text}\n}}") })
    });
    let mut pictures: BTreeMap<String, Option<String>> = BTreeMap::new();
    for reference in stylesheet_references(&inlined) {
        if !IMAGE_EXTENSIONS.is_match(&reference) {
            continue;
        }
        let picture = crate::viewers::resolve_document_reference(sheet_path, without_query(&reference)).and_then(|p| resources.read_image(&p));
        pictures.insert(reference, picture);
    }
    rewrite_stylesheet_urls(&inlined, &|reference| pictures.get(reference).cloned().flatten())
}

pub fn load_stylesheets(styles: &[HtmlStyleSource], document_path: &str, resources: &dyn HtmlResources) -> Vec<String> {
    styles.iter().map(|style| match style {
        HtmlStyleSource::Inline(text) => resolve_stylesheet(text, document_path, resources, 0),
        HtmlStyleSource::Link { href, media } => {
            if REMOTE_URL.is_match(href) {
                return String::new();
            }
            let Some(path) = crate::viewers::resolve_document_reference(document_path, without_query(href)) else { return String::new() };
            let Some(text) = resources.read_text(&path) else { return String::new() };
            let resolved = resolve_stylesheet(&text, &path, resources, 0);
            match media {
                Some(media) => format!("@media {media} {{\n{resolved}\n}}"),
                None => resolved,
            }
        }
    }).collect()
}

fn escape_text(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
}

fn escape_attr(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
}

const VOID: &[&str] = &["area", "br", "col", "hr", "img", "input", "wbr"];

fn write_attrs(props: &BTreeMap<String, String>, out: &mut String) {
    for (k, v) in props {
        out.push(' ');
        out.push_str(k);
        out.push_str("=\"");
        escape_attr(v, out);
        out.push('"');
    }
}

fn write_nodes(nodes: &[HtmlNode], pictures: &BTreeMap<String, Option<String>>, embed_label: &dyn Fn(&str) -> String, out: &mut String) {
    for node in nodes {
        match node {
            HtmlNode::Text(t) => escape_text(t, out),
            HtmlNode::Element(e) if e.tag == "mw-embed" => {
                let label = embed_label(e.props.get("data-kind").map_or("", String::as_str));
                out.push_str("<span class=\"mw-embed\">");
                escape_text(&label, out);
                out.push_str("</span>");
            }
            HtmlNode::Element(e) => {
                let mut props = e.props.clone();
                if props.remove("data-mw-remote-src").is_some() {
                    // The page's own words stand in for a picture from elsewhere.
                    if let Some(alt) = props.get("alt") {
                        escape_text(alt, out);
                    }
                    continue;
                }
                if let Some(source) = props.remove("data-mw-src") {
                    match pictures.get(&source).cloned().flatten() {
                        Some(data) => {
                            props.insert(if e.tag == "image" { "href".to_owned() } else { "src".to_owned() }, data);
                        }
                        None => {
                            if let Some(alt) = props.get("alt") {
                                escape_text(alt, out);
                            }
                            continue;
                        }
                    }
                }
                out.push('<');
                out.push_str(&e.tag);
                write_attrs(&props, out);
                out.push('>');
                if VOID.contains(&e.tag.as_str()) {
                    continue;
                }
                write_nodes(&e.children, pictures, embed_label, out);
                out.push_str("</");
                out.push_str(&e.tag);
                out.push('>');
            }
        }
    }
}

/// The page as one self-contained document for a web view with scripts off: nothing in it asks for anything. The base
/// sheet is what upstream's shadow root started from, and a CSP in the document turns away what the sanitizer missed.
pub fn compose_document(page: &HtmlPreviewDocument, sheets: &[String], pictures: &BTreeMap<String, Option<String>>, embed_label: &dyn Fn(&str) -> String) -> String {
    let mut out = String::from("<!doctype html><html");
    write_attrs(&page.root_props, &mut out);
    out.push_str("><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src data:; style-src 'unsafe-inline'; font-src data:\">");
    out.push_str("<style>.mw-embed{display:inline-flex;align-items:center;justify-content:center;box-sizing:border-box;min-width:160px;min-height:60px;max-width:100%;padding:8px;border:1px dashed GrayText;color:GrayText;font:12px system-ui,sans-serif}</style>");
    for sheet in sheets {
        out.push_str("<style>");
        out.push_str(&sheet.replace("</style", "<\\/style"));
        out.push_str("</style>");
    }
    out.push_str("</head><body");
    write_attrs(&page.body_props, &mut out);
    out.push('>');
    write_nodes(&page.body, pictures, embed_label, &mut out);
    out.push_str("</body></html>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r##"<!doctype html>
<html lang="en"><head>
  <title> Home </title>
  <meta http-equiv="refresh" content="0;url=https://evil.example">
  <link rel="stylesheet" href="css/site.css" media="screen">
  <style>h1 { color: teal }</style>
  <script src="app.js"></script>
</head>
<body class="page" onload="boot()">
  <h1 style="margin: 0" onclick="steal()">Hi</h1>
  <a href="javascript:alert(1)">bad</a> <a href="docs/next.html#top" target="_blank">next</a>
  <img src="img/logo.png" alt="logo" srcset="a.png 2x"> <img src="https://cdn.example/x.png" alt="remote">
  <iframe src="https://example.com"></iframe>
  <noscript><p>Enable JS</p></noscript>
  <form action="https://evil.example/post"><button formaction="https://evil.example">Go</button></form>
  <svg><a href="#x"><set attributeName="href" to="javascript:alert(1)"/></a><image href="icons/a.svg"/></svg>
</body></html>"##;

    fn props(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn collects_what_the_head_contributes_and_drops_the_rest_of_it() {
        let page = prepare_html_preview(PAGE);
        assert_eq!(page.title.as_deref(), Some("Home"));
        assert!(page.had_scripts);
        assert_eq!(page.styles, vec![HtmlStyleSource::Link { href: "css/site.css".into(), media: Some("screen".into()) }, HtmlStyleSource::Inline("h1 { color: teal }".into())]);
        assert_eq!(page.root_props.get("lang").map(String::as_str), Some("en"));
        assert_eq!(page.body_props.get("class").map(String::as_str), Some("page"));
        assert!(!page.body_props.contains_key("onload"));
    }

    #[test]
    fn strips_handlers_executable_addresses_frames_and_posting_forms() {
        let page = prepare_html_preview(PAGE);
        assert_eq!(find_in(&page.body, "h1").unwrap().props, props(&[("style", "margin: 0")]));
        let links: Vec<&HtmlElement> = page.body.iter().filter_map(|n| if let HtmlNode::Element(e) = n { (e.tag == "a").then_some(e) } else { None }).collect();
        assert!(!links[0].props.contains_key("href"));
        assert_eq!(links[1].props, props(&[("href", "docs/next.html#top")]));
        assert!(find_in(&page.body, "iframe").is_none());
        assert_eq!(find_in(&page.body, "mw-embed").unwrap().props["data-kind"], "iframe");
        assert!(!find_in(&page.body, "form").unwrap().props.contains_key("action"));
        assert!(!find_in(&page.body, "button").unwrap().props.contains_key("formaction"));
        assert!(find_in(&page.body, "set").is_none());
        // What a page says without scripts is what the preview shows.
        assert!(find_in(&page.body, "noscript").is_none());
        assert!(find_in(&page.body, "p").is_some());
    }

    #[test]
    fn hands_workspace_pictures_to_the_caller_and_keeps_remote_ones_out_of_the_engines_reach() {
        let page = prepare_html_preview(PAGE);
        assert_eq!(page.images, ["img/logo.png", "icons/a.svg"]);
        let images: Vec<&HtmlElement> = page.body.iter().filter_map(|n| if let HtmlNode::Element(e) = n { (e.tag == "img").then_some(e) } else { None }).collect();
        assert_eq!(images[0].props, props(&[("alt", "logo"), ("data-mw-src", "img/logo.png")]));
        assert_eq!(images[1].props, props(&[("alt", "remote"), ("data-mw-remote-src", "https://cdn.example/x.png")]));
    }

    #[test]
    fn points_html_body_and_root_at_the_elements_that_stand_in_for_them() {
        assert_eq!(rewrite_selector("html, body > main, :root, .body, #html, html.dark"), ".mw-html-root, .mw-html-body > main, .mw-html-root, .body, #html, .mw-html-root.dark");
    }

    #[test]
    fn finds_and_rewrites_the_addresses_a_stylesheet_uses() {
        let css = "a { background: url(img/a.png) } b { background: url('https://x.example/b.png') } @font-face { src: url(\"f.woff2\") }";
        assert_eq!(stylesheet_references(css), ["img/a.png", "f.woff2"]);
        assert_eq!(rewrite_stylesheet_urls(css, &|r| (r == "img/a.png").then(|| "data:image/png;base64,AA".to_owned())), "a { background: url(\"data:image/png;base64,AA\") } b { background: none } @font-face { src: none }");
    }

    #[test]
    fn inlines_imports_keeping_the_media_they_were_made_for() {
        let css = "@import url(\"base.css\") screen;\n@import 'print.css';\nbody{}";
        assert_eq!(stylesheet_imports(css), [("base.css".to_owned(), "screen".to_owned()), ("print.css".to_owned(), String::new())]);
        assert_eq!(inline_stylesheet_imports(css, &|r, m| (r == "base.css").then(|| format!("/*base {m}*/"))), "/*base screen*/\n\nbody{}");
    }

    #[test]
    fn composes_a_document_that_asks_for_nothing() {
        let page = prepare_html_preview(PAGE);
        let pictures = BTreeMap::from([("img/logo.png".to_owned(), Some("data:image/png;base64,AA".to_owned())), ("icons/a.svg".to_owned(), None)]);
        let html = compose_document(&page, &["h1{color:teal}".to_owned()], &pictures, &|kind| format!("embed {kind}"));
        assert!(html.contains("src=\"data:image/png;base64,AA\""));
        assert!(html.contains("embed iframe"));
        assert!(html.contains("<body class=\"page\">"));
        for banned in ["<script", "javascript:", "onclick", "evil.example", "cdn.example"] {
            assert!(!html.contains(banned), "{banned}");
        }
    }
}
