//! What a message's DOM gave a screen reader for nothing: its headings, links, lists, code blocks and pictures as things to stop on.
//!
//! One leaf paints a whole message, so here it describes what it painted. What is there comes from the parsed blocks, and where it is
//! from their layout. Built only while someone is listening, kept until the message or its layout changes, and a press goes where a
//! click on the same thing goes.

use super::layout::{plain_text, Laid, Outlined};
use super::parse::TableRow;
use super::*;

type Node = AriaChild<App>;

/// What a built tree was read from.
#[derive(Clone, Copy, PartialEq)]
pub(super) struct ReadingShape {
    generation: u64,
    /// The width laid out at, while there is one: until then nothing has a place.
    width: Option<f32>,
    /// That layout is of what the message holds now. A stream's next words have none yet, and only they go without a place.
    current: bool,
    /// What boxes that scroll sideways have moved their contents by.
    scrolled: f32,
}

fn address<T>(thing: &T) -> usize {
    thing as *const T as usize
}

fn nth(key: u64, n: usize) -> u64 {
    hash_key((key, n as u64))
}

fn hull(a: Rect, b: Rect) -> Rect {
    let (x, y) = (a.x.min(b.x), a.y.min(b.y));
    rect(x, y, a.right().max(b.right()) - x, a.bottom().max(b.bottom()) - y)
}

fn hull_of(nodes: &[Node]) -> Option<Rect> {
    nodes.iter().filter_map(|node| node.rect).reduce(hull)
}

/// Words, the way a label holds them: `<p>` is read from its first word to its last.
fn words_node(key: u64, words: &str, at: Option<Rect>) -> Option<Node> {
    let words = words.trim();
    (!words.is_empty()).then(|| Node { value: Some(Arc::from(words)), rect: at, ..Node::new(key, Role::Label) })
}

fn link_of(inline: &Inline) -> Option<&Rc<Link>> {
    match inline {
        Inline::Text { link, .. } | Inline::Math { link, .. } | Inline::Image { link, .. } | Inline::Ruby { link, .. } => link.as_ref(),
        Inline::Break | Inline::Anchor(_) => None,
    }
}

/// One top-level block, with what it takes to say where its parts are.
struct At<'a> {
    state: &'a MdState,
    this: &'a Weak<RefCell<MdState>>,
    laid: Option<&'a Laid>,
    top: u32,
    /// Where the block starts in the leaf.
    y: f32,
}

/// Where in the block's pieces the next link is looked for: they come in reading order.
struct Links {
    pieces: std::ops::Range<u32>,
    link: usize,
}

impl At<'_> {
    /// From the block's own coordinates to the leaf's.
    fn placed(&self, r: Rect, region: Option<u16>) -> Rect {
        let moved = self.laid.map_or(0.0, |laid| self.state.region_scroll(laid, region));
        rect(r.x - moved, r.y + self.y, r.w, r.h)
    }

    fn outlined(&self, thing: usize) -> Option<&Outlined> {
        self.laid?.outline.get(&thing)
    }

    fn place_of(&self, thing: usize, fallback: Option<Rect>) -> Option<Rect> {
        self.outlined(thing).map(|outlined| self.placed(outlined.r, outlined.region)).or(fallback)
    }

    fn region_of(&self, uid: u32) -> Option<&Region> {
        self.laid?.regions.iter().find(|region| region.key == (self.top, uid))
    }

    /// The boxes of the next `runs` runs carrying this link, which is one box for each line.
    fn link_place(&self, links: &mut Links, link: &Link, runs: usize) -> Option<Rect> {
        let laid = self.laid?;
        let mut found: Option<Rect> = None;
        let mut taken = 0;
        while taken < runs && links.pieces.start < links.pieces.end {
            let piece = laid.pieces.get(links.pieces.start as usize)?;
            let Some(span) = piece.lay.links.get(links.link) else {
                links.pieces.start += 1;
                links.link = 0;
                continue;
            };
            links.link += 1;
            if *span.link != *link {
                continue;
            }
            taken += 1;
            let scale = self.state.scale;
            for r in &span.rects {
                let dx = piece.line_dx_at(r[1], scale);
                let shown = self.placed(rect(piece.x + dx + r[0], piece.y + r[1], r[2], r[3]), piece.region);
                found = Some(found.map_or(shown, |so_far| hull(so_far, shown)));
            }
        }
        found
    }

    fn picture_place(&self, images: &std::ops::Range<u32>, src: &str) -> Option<Rect> {
        let laid = self.laid?;
        let image = laid.images.get(images.start as usize..images.end as usize)?.iter().find(|image| image.src == src)?;
        Some(self.placed(image.r, image.region))
    }

    /// `<a>`: its words, where it goes, and a press going where a click goes.
    fn link_node(&self, key: u64, link: &Rc<Link>, words: &str, at: Option<Rect>) -> Node {
        let (url, title) = match &link.target {
            LinkTarget::External(href) | LinkTarget::Protocol(href) | LinkTarget::Document(href) => (Some(href.clone()), None),
            LinkTarget::Fragment(fragment) => (Some(format!("#{fragment}")), None),
            LinkTarget::Path { target, line } => (Some(target.clone()), Some(line.map_or_else(|| target.clone(), |line| format!("{target}:{line}")))),
            LinkTarget::Inert => (None, None),
        };
        let mut node = Node { label: Some(Arc::from(words.trim())), url: url.map(Arc::from), description: link.title.clone().or(title).map(Arc::from), rect: at, ..Node::new(key, Role::Link) };
        let shown = at.unwrap_or(rect(0.0, 0.0, 0.0, 0.0));
        if self.state.link_action(link, shown).is_some() {
            let (this, link) = (self.this.clone(), link.clone());
            node.on_press = Some(Rc::new(move |app: &mut App| {
                let Some(state) = this.upgrade() else { return };
                let action = {
                    let s = state.borrow();
                    let leaf = s.leaf_rect.get();
                    s.link_action(&link, rect(leaf.x + shown.x, leaf.y + shown.y, shown.w, shown.h))
                };
                if let Some(action) = action {
                    run_action(app, action, &state);
                }
            }));
        }
        node
    }

    /// A line of text: its words between its links, its formulas and its pictures.
    fn inline_nodes(&self, inlines: &[Inline], key: u64, thing: usize, at: Option<Rect>) -> Vec<Node> {
        let outlined = self.outlined(thing);
        let mut links = Links { pieces: outlined.map_or(0..0, |outlined| outlined.pieces.clone()), link: 0 };
        let images = outlined.map_or(0..0, |outlined| outlined.images.clone());
        let mut out = Vec::new();
        let mut words = String::new();
        let flush = |words: &mut String, out: &mut Vec<Node>| {
            out.extend(words_node(nth(key, out.len()), words, at));
            words.clear();
        };
        let mut i = 0;
        while i < inlines.len() {
            let inline = &inlines[i];
            if let Some(link) = link_of(inline) {
                flush(&mut words, &mut out);
                // `[**bold** words](…)` is one link in several runs.
                let run = inlines[i..].iter().take_while(|next| link_of(next).is_some_and(|other| Rc::ptr_eq(other, link))).count();
                let group = &inlines[i..i + run];
                let picture = group.iter().find_map(|inline| match inline {
                    Inline::Image { src, .. } => self.picture_place(&images, src),
                    _ => None,
                });
                // A picture that was found is no run of text.
                let place = self.link_place(&mut links, link, group.iter().filter(|inline| !matches!(inline, Inline::Image { src, .. } if self.picture_place(&images, src).is_some())).count());
                out.push(self.link_node(nth(key, out.len()), link, &plain_text(group), place.or(picture).or(at)));
                i += run;
                continue;
            }
            match inline {
                Inline::Text { text, .. } => words.push_str(text),
                Inline::Ruby { base, reading, .. } => {
                    words.push_str(base);
                    words.push('(');
                    words.push_str(reading);
                    words.push(')');
                }
                Inline::Break => words.push('\n'),
                Inline::Anchor(_) => {}
                Inline::Math { source, .. } => {
                    flush(&mut words, &mut out);
                    out.push(Node { label: Some(Arc::from(source.as_str())), rect: at, ..Node::new(nth(key, out.len()), Role::Math) });
                }
                Inline::Image { src, alt, .. } => {
                    flush(&mut words, &mut out);
                    out.push(Node { label: Some(Arc::from(alt.as_str())), rect: self.picture_place(&images, src).or(at), ..Node::new(nth(key, out.len()), Role::Image) });
                }
            }
            i += 1;
        }
        flush(&mut words, &mut out);
        out
    }

    fn blocks(&self, blocks: &[Block], key: u64, parent: Option<Rect>, out: &mut Vec<Node>) {
        for (i, block) in blocks.iter().enumerate() {
            self.block(block, nth(key, i), parent, out);
        }
    }

    fn block(&self, block: &Block, key: u64, parent: Option<Rect>, out: &mut Vec<Node>) {
        let at = self.place_of(address(block), parent);
        match block {
            Block::Paragraph { inlines, .. } => {
                let mut parts = self.inline_nodes(inlines, key, address(block), at);
                match parts.len() {
                    0 => {}
                    1 => out.extend(parts.pop()),
                    _ => out.push(Node { children: parts, rect: at, ..Node::new(key, Role::Paragraph) }),
                }
            }
            Block::Heading { level, inlines, .. } => {
                // Its words are its name, and only what can be done inside it stands apart.
                let parts = self.inline_nodes(inlines, key, address(block), at).into_iter().filter(|part| part.role != Role::Label).collect();
                out.push(Node { label: Some(Arc::from(plain_text(inlines).trim())), level: Some(*level as usize), children: parts, rect: at, ..Node::new(key, Role::Heading) });
            }
            Block::Code { uid, label, code, chrome, .. } => out.push(self.code_node(*uid, label.as_deref(), code, *chrome, key, at)),
            Block::Math { uid, source } => {
                let at = self.region_of(*uid).map_or(at, |region| Some(self.placed(region.r, region.parent)));
                out.push(Node { label: Some(Arc::from(source.as_str())), rect: at, ..Node::new(key, Role::Math) });
            }
            Block::Quote(blocks) => {
                let mut children = Vec::new();
                self.blocks(blocks, key, at, &mut children);
                out.push(Node { children, rect: at, ..Node::new(key, Role::Blockquote) });
            }
            Block::List { start, items } => {
                let mut children = Vec::new();
                let mut number = start.unwrap_or(1);
                for (i, item) in items.iter().enumerate() {
                    let item_key = nth(key, i);
                    let mut parts = Vec::new();
                    match (item.task, start) {
                        (Some(done), _) => parts.push(Node { toggled: Some(done.into()), disabled: true, ..Node::new(nth(item_key, usize::MAX), Role::CheckBox) }),
                        (None, Some(_)) => {
                            parts.push(Node { label: Some(Arc::from(format!("{number}."))), ..Node::new(nth(item_key, usize::MAX), Role::ListMarker) });
                            number += 1;
                        }
                        (None, None) => {}
                    }
                    self.blocks(&item.blocks, item_key, at, &mut parts);
                    let place = hull_of(&parts).or(at);
                    parts.iter_mut().filter(|part| part.rect.is_none()).for_each(|part| part.rect = place);
                    children.push(Node { children: parts, rect: place, ..Node::new(item_key, Role::ListItem) });
                }
                out.push(Node { children, rect: at, ..Node::new(key, Role::List) });
            }
            Block::Table { uid, rows } => {
                let at = self.region_of(*uid).map_or(at, |region| Some(self.placed(region.r, region.parent)));
                out.push(Node { children: rows.iter().enumerate().map(|(i, row)| self.row_node(row, nth(key, i), at)).collect(), rect: at, ..Node::new(key, Role::Table) });
            }
            Block::Details { uid, open, summary, blocks } => out.push(self.details_node(*uid, *open, summary, blocks, key, at)),
            Block::Footnote { number, blocks, .. } => {
                let mut children = vec![Node { label: Some(Arc::from(format!("{number}."))), rect: at, ..Node::new(nth(key, usize::MAX), Role::ListMarker) }];
                self.blocks(blocks, key, at, &mut children);
                out.push(Node { children, rect: at, ..Node::new(key, Role::Group) });
            }
            // A `div` around blocks says nothing of its own.
            Block::Aligned { blocks, .. } | Block::Indented { blocks, .. } => self.blocks(blocks, key, at, out),
            Block::Rule | Block::Anchor(_) => {}
        }
    }

    /// `MarkdownCodeBlock`: the code, its language, and the copy button in its corner.
    fn code_node(&self, uid: u32, label: Option<&str>, code: &Arc<str>, chrome: bool, key: u64, at: Option<Rect>) -> Node {
        let region = self.region_of(uid);
        let at = region.map_or(at, |region| Some(self.placed(region.r, region.parent)));
        let mut children = Vec::new();
        children.extend(label.filter(|_| chrome).and_then(|language| words_node(nth(key, 0), language, at)));
        children.push(Node { value: Some(code.clone()), rect: at, ..Node::new(nth(key, 1), Role::Label) });
        if chrome {
            let (this, ui_key, code) = (self.this.clone(), (self.top, uid), code.clone());
            let corner = region.map(|region| self.placed(copy_button(region.r, 0.0, 0.0), region.parent));
            children.push(Node {
                label: Some(Arc::from(t("复制代码", "Copy code"))),
                rect: corner.or(at),
                on_press: Some(Rc::new(move |_: &mut App| {
                    if let Some(state) = this.upgrade() {
                        state.borrow_mut().pending_copy = Some((ui_key, code.clone()));
                        repaint();
                    }
                })),
                ..Node::new(nth(key, 2), Role::Button)
            });
        }
        Node { children, rect: at, ..Node::new(key, Role::Code) }
    }

    fn row_node(&self, row: &TableRow, key: u64, at: Option<Rect>) -> Node {
        let cells: Vec<Node> = row
            .cells
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                let cell_key = nth(key, i);
                let place = self.place_of(address(cell), at);
                let mut children = Vec::new();
                match &cell.blocks {
                    None => children = self.inline_nodes(&cell.inlines, cell_key, address(cell), place),
                    Some(blocks) => self.blocks(blocks, cell_key, place, &mut children),
                }
                Node { children, rect: place, ..Node::new(cell_key, if cell.header || row.header { Role::ColumnHeader } else { Role::Cell }) }
            })
            .collect();
        Node { rect: hull_of(&cells).or(at), children: cells, ..Node::new(key, Role::Row) }
    }

    /// `<details>`: its summary opens and closes it, and what it holds is there while it is open.
    fn details_node(&self, uid: u32, open: bool, summary: &[Inline], blocks: &[Block], key: u64, at: Option<Rect>) -> Node {
        let ui_key = (self.top, uid);
        let open = self.state.ui.get(&ui_key).and_then(|ui| ui.open).unwrap_or(open);
        let words = if summary.is_empty() { t("详情", "Details").to_owned() } else { plain_text(summary) };
        let toggle = self.laid.and_then(|laid| laid.toggles.iter().find(|toggle| toggle.key == ui_key)).map(|toggle| self.placed(toggle.r, toggle.region));
        let this = self.this.clone();
        let mut children = vec![Node {
            label: Some(Arc::from(words.trim())),
            expanded: Some(open),
            rect: toggle.or(at),
            on_press: Some(Rc::new(move |_: &mut App| {
                if let Some(state) = this.upgrade() {
                    let mut s = state.borrow_mut();
                    // What is selected may be about to stop being laid out.
                    s.clear_selection();
                    s.toggle_ui(ui_key, |ui| ui.open = Some(!open));
                    repaint();
                }
            })),
            ..Node::new(nth(key, usize::MAX), Role::DisclosureTriangle)
        }];
        if open {
            self.blocks(blocks, key, at, &mut children);
        }
        Node { children, rect: at, ..Node::new(key, Role::Details) }
    }
}

impl MdState {
    /// What says where things are: the positions of the width last painted, of this generation if there are any. Blocks a stream kept
    /// are where they were, and keep their layouts, until the new ones are worked out.
    fn reading_positions(&self) -> Option<&Positions> {
        let painted = self.leaf_rect.get().w;
        let current = |p: &&Positions| p.generation == self.generation;
        let wide = |p: &&Positions| same_width(p.width, painted);
        self.positions.iter().find(|p| current(p) && wide(p)).or_else(|| self.positions.iter().rfind(current)).or_else(|| self.positions.iter().find(wide)).or_else(|| self.positions.last())
    }

    pub(super) fn reading_shape(&self) -> ReadingShape {
        let positions = self.reading_positions();
        ReadingShape { generation: self.generation, width: positions.map(|p| p.width), current: positions.is_some_and(|p| p.generation == self.generation), scrolled: self.ui.values().map(|ui| ui.scroll_x).sum() }
    }

    /// The message as a screen reader walks it, relative to the leaf.
    pub(super) fn reading_tree(this: &Rc<RefCell<MdState>>) -> Rc<[Node]> {
        let weak = Rc::downgrade(this);
        let mut guard = this.borrow_mut();
        let shape = guard.reading_shape();
        if let Some((held, tree)) = &guard.reading
            && *held == shape
        {
            return tree.clone();
        }
        let s = &*guard;
        let positions = s.reading_positions();
        let mut out = Vec::new();
        for (i, top) in s.blocks.iter().enumerate() {
            let placed = positions.and_then(|p| Some((top.laid_at(p.width)?, *p.ys.get(i)?)));
            let at = At { state: s, this: &weak, laid: placed.map(|(laid, _)| laid), top: top.index, y: placed.map_or(0.0, |(_, y)| y) };
            at.blocks(&top.blocks, hash_key(("markdown-reading", top.index)), None, &mut out);
        }
        let tree: Rc<[Node]> = out.into();
        guard.reading = Some((shape, tree.clone()));
        tree
    }
}
