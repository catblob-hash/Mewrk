//! A document in a pane: the shared `MarkdownContent` with what this surface gives it (upstream's props on it in `FilesPane.tsx`,
//! `FileAttachmentPreview.tsx` and `NotebookView.tsx`), and landing on a heading from a link followed elsewhere.

use crate::browser::BrowseMachine;
use crate::images::{picture_for_task, Picture};
use crate::links::PathOpenRequest;
use mewrk_client::prelude::*;
use mewrk_ui_common::markdown::{markdown_content, markdown_fragment_rect, MarkdownImage, MarkdownOptions, PathLink};
use std::hash::Hash;

/// What a document needs from where it is shown.
#[derive(Clone)]
pub struct DocContext {
    pub key: String,
    /// The surface's scroller, for a heading jump.
    pub scroller: String,
    /// What the document's own paths are written against.
    pub base_dir: Option<String>,
    /// The machine a document's paths are on, when the surface says so (outer `None`: it does not).
    pub machine: Option<BrowseMachine>,
    pub linkify_paths: bool,
    pub resolve_image: Rc<dyn Fn(&str) -> Option<Arc<str>>>,
    pub on_link: Rc<dyn Fn(&mut App, &str)>,
    pub on_path: Rc<dyn Fn(&mut App, PathOpenRequest)>,
}

impl DocContext {
    fn path_request(&self) -> impl Fn(PathLink) -> PathOpenRequest + Clone + use<> {
        let machine = self.machine.clone();
        move |link: PathLink| PathOpenRequest { path: link.path, base_dir: link.base_dir, line: link.line, machine: machine.clone(), anchor: Some([link.anchor.x, link.anchor.y, link.anchor.right(), link.anchor.bottom()]), ..Default::default() }
    }
}

/// `.files-pane__document`'s type: 12.5 px at 1.6.
pub const DOCUMENT_TY: TypeStyle = TypeStyle { size: 12.5, line: 20.0, weight: Weight::Regular, mono: false, italic: false, serif: false };

/// A Markdown source with the HTML it carries, as one leaf. `key` tells apart the leaves of one document.
pub fn document_view(app: &App, ctx: &DocContext, key: impl Hash, source: &str) -> E {
    document_view_as(app, ctx, key, source, DOCUMENT_TY)
}

pub fn document_view_as(app: &App, ctx: &DocContext, key: impl Hash, source: &str, ty: TypeStyle) -> E {
    let task = app.task();
    let resolve = ctx.resolve_image.clone();
    let request = ctx.path_request();
    let (open, open_request) = (ctx.on_path.clone(), request.clone());
    let on_link = ctx.on_link.clone();
    let scroller = ctx.scroller.clone();
    markdown_content((ctx.key.as_str(), key), source, MarkdownOptions {
        linkify_paths: ctx.linkify_paths,
        path_base_dir: ctx.base_dir.clone(),
        render_html: true,
        document_links: true,
        single_dollar_math: app.doc.global_settings.appearance.single_dollar_math,
        ty,
        // Asked on every frame the leaf is drawn, which is also what keeps an animated picture in it moving.
        resolve_image_src: Some(Rc::new(move |source: &str| {
            let url = resolve(source)?;
            match picture_for_task(&task, &url) {
                Picture::Ready { id, width, height } => Some(MarkdownImage { id, size: (width, height) }),
                _ => None,
            }
        })),
        on_open_path: Some(Rc::new(move |app: &mut App, link: PathLink| open(app, open_request(link)))),
        on_hover_path: Some(Rc::new(move |app: &mut App, link: &PathLink| crate::path_open::prefetch_path(app, &request(link.clone())))),
        on_document_link: Some(Rc::new(move |app: &mut App, href: &str| on_link(app, href))),
        on_reveal: Some(Rc::new(move |_: &mut App, target: Rect| reveal_in_scroller(&scroller, target))),
        ..MarkdownOptions::default()
    })
}

/// Brings a window rectangle into the scroller. A heading goes to the top, a little below the edge; the sliver a selection
/// drag asks for moves only as far as it must.
fn reveal_in_scroller(scroller: &str, target: Rect) {
    let Some(frame) = rect_of(scroller) else { return };
    let offset = scroll_state(scroller).map_or(0.0, |(offset, _, _)| offset);
    let y = offset + target.y - frame.y;
    scroll_request(scroller, if target.h > 4.0 { ScrollRequest::To((y - 8.0).max(0.0)) } else { ScrollRequest::Reveal { y, h: target.h } });
}

/// Scrolls to what a fragment names in one of the document's leaves. Answers whether it was found: until the leaf has been laid
/// out, it has not been.
pub fn land_on_fragment(ctx: &DocContext, key: impl Hash, fragment: &str) -> bool {
    let Some(target) = markdown_fragment_rect((ctx.key.as_str(), key), fragment) else { return false };
    reveal_in_scroller(&ctx.scroller, target);
    true
}

/// A block of code the way a fence would be: the front matter above a document. The fence is longer than any run in the source.
pub fn fenced(source: &str, language: &str) -> String {
    let longest = source.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}{language}\n{source}\n{fence}\n")
}

/// What stands in for something a page would have run or loaded.
pub fn embed_label(kind: &str) -> String {
    match kind {
        "video" => t("视频未在预览中播放", "Video is not played in the preview"),
        "audio" => t("音频未在预览中播放", "Audio is not played in the preview"),
        "canvas" => t("画布需要脚本绘制", "A canvas is drawn by scripts"),
        _ => t("嵌入内容未在预览中显示", "Embedded content is not shown in the preview"),
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fence_outruns_the_backticks_in_what_it_holds() {
        assert_eq!(fenced("a: 1", "yaml"), "```yaml\na: 1\n```\n");
        assert_eq!(fenced("x ```` y", "toml"), "`````toml\nx ```` y\n`````\n");
    }
}
