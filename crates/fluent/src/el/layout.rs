//! The tree a frame is laid out in: taffy's algorithms over a tree of our own, for the sake of its cache.
//!
//! Taffy's own tree keeps one answer for each kind of size question a node is asked, and a flex row asks its children two of
//! the same kind: how big they are with room to spare, then how big at the row's own height. Each answer pushes the other out,
//! so the next time the row is asked about, everything under it is worked out again, and so on for every row above it. A tool
//! row's icon sixteen levels down was measured 29 times a frame for 9 different questions.
//!
//! So an answer pushed out is kept, and given again for the very same question. Taffy also takes the answer in a slot for a
//! question it does not exactly match (a known width that is the width it came out at), and which answer is in the slot decides
//! what such a question gets; those keep taffy's slots as they are, so the layout comes out as taffy's own tree has it. A node's
//! final layout keeps one answer, as taffy's does: laying a node out places its children as a side effect, so an older one
//! taken from a cache would leave them where a later one put them.
//!
//! The tree is rebuilt every frame, but most of it comes out the same as the frame before. A subtree that is the same, style for
//! style and leaf for leaf, takes over what the last frame worked out for it: every node's answers and where it put its children.
//! That is what taffy's own tree does with a node nothing marked dirty, so the layout comes out as a tree kept between frames would
//! have it, and a timeline row that did not change is not laid out again for a token streamed into the row below it.

use taffy::prelude::{AvailableSpace, Display, Layout, NodeId, Size, Style};
use taffy::tree::{LayoutInput, LayoutOutput, RunMode};
use taffy::{compute_block_layout, compute_cached_layout, compute_flexbox_layout, compute_grid_layout, compute_hidden_layout, compute_leaf_layout, compute_root_layout, round_layout};
use taffy::{CacheTree, LayoutBlockContainer, LayoutFlexboxContainer, LayoutGridContainer, LayoutPartialTree, RoundTree, TraversePartialTree, TraverseTree};

/// Taffy's slots: one for each kind of question.
const SLOTS: usize = 9;
/// How many answers pushed out of their slot a node keeps. A node deep in rows of rows is asked a dozen questions a frame.
const KEPT: usize = 16;

#[derive(Clone, Copy)]
struct Entry<T> {
    known: Size<Option<f32>>,
    available: Size<AvailableSpace>,
    content: T,
}

impl<T> Entry<T> {
    /// Taffy's own test for an answer that still holds: the same known sizes, or ones the answer came out at, and room that is
    /// the same where no size is known.
    fn answers(&self, known: Size<Option<f32>>, available: Size<AvailableSpace>, size: Size<f32>) -> bool {
        (known.width == self.known.width || known.width == Some(size.width))
            && (known.height == self.known.height || known.height == Some(size.height))
            && (known.width.is_some() || self.available.width.is_roughly_equal(available.width))
            && (known.height.is_some() || self.available.height.is_roughly_equal(available.height))
    }

    fn asked(&self, known: Size<Option<f32>>, available: Size<AvailableSpace>) -> bool {
        self.known == known && self.available == available
    }
}

/// Taffy's `Cache::compute_cache_slot`.
fn slot_of(known: Size<Option<f32>>, available: Size<AvailableSpace>) -> usize {
    use AvailableSpace::{Definite, MaxContent, MinContent};
    match (known.width.is_some(), known.height.is_some()) {
        (true, true) => 0,
        (true, false) => 1 + (available.height == MinContent) as usize,
        (false, true) => 3 + (available.width == MinContent) as usize,
        (false, false) => match (available.width, available.height) {
            (MaxContent | Definite(_), MaxContent | Definite(_)) => 5,
            (MaxContent | Definite(_), MinContent) => 6,
            (MinContent, MaxContent | Definite(_)) => 7,
            (MinContent, MinContent) => 8,
        },
    }
}

#[derive(Clone, Default)]
struct Cache {
    layout: Option<Entry<LayoutOutput>>,
    /// Taffy's own, answered as taffy answers.
    slots: [Option<Entry<Size<f32>>>; SLOTS],
    /// What was pushed out of a slot, answered only for the question it was the answer to.
    kept: Vec<Entry<Size<f32>>>,
    /// The kept answer to give way next once there is no room for more.
    next: usize,
}

impl Cache {
    fn get(&self, known: Size<Option<f32>>, available: Size<AvailableSpace>, run_mode: RunMode) -> Option<LayoutOutput> {
        match run_mode {
            RunMode::PerformLayout => self.layout.filter(|entry| entry.answers(known, available, entry.content.size)).map(|entry| entry.content),
            RunMode::ComputeSize => self
                .slots
                .iter()
                .flatten()
                .find(|entry| entry.answers(known, available, entry.content))
                .or_else(|| self.kept.iter().find(|entry| entry.asked(known, available)))
                .map(|entry| LayoutOutput::from_outer_size(entry.content)),
            RunMode::PerformHiddenLayout => None,
        }
    }

    fn store(&mut self, known: Size<Option<f32>>, available: Size<AvailableSpace>, run_mode: RunMode, output: LayoutOutput) {
        match run_mode {
            RunMode::PerformLayout => self.layout = Some(Entry { known, available, content: output }),
            RunMode::ComputeSize => {
                let entry = Entry { known, available, content: output.size };
                if let Some(out) = self.slots[slot_of(known, available)].replace(entry)
                    && !out.asked(known, available)
                {
                    self.keep(out);
                }
                // A question answered afresh is not answered from what was kept.
                self.kept.retain(|held| !held.asked(known, available));
            }
            RunMode::PerformHiddenLayout => {}
        }
    }

    fn keep(&mut self, entry: Entry<Size<f32>>) {
        if let Some(same) = self.kept.iter_mut().find(|held| held.asked(entry.known, entry.available)) {
            *same = entry;
        } else if self.kept.len() < KEPT {
            self.kept.push(entry);
        } else {
            self.kept[self.next] = entry;
            self.next = (self.next + 1) % KEPT;
        }
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

struct Node {
    style: Style,
    /// The style the node was built with, when it has since been given another ([`LayoutTree::set_capped_style`]).
    built_style: Option<Box<Style>>,
    children: Vec<NodeId>,
    parent: Option<NodeId>,
    /// What the measure function is handed for a leaf.
    context: Option<usize>,
    /// Everything the measure function's answers depend on besides the question, or `None` when nobody can say.
    leaf_key: Option<u64>,
    /// Took over the last frame's layout of the same subtree.
    adopted: bool,
    cache: Cache,
    unrounded: Layout,
    layout: Layout,
}

/// One frame's layout: built, laid out once (or twice), read, and dropped.
#[derive(Default)]
pub(crate) struct LayoutTree {
    nodes: Vec<Node>,
}

impl LayoutTree {
    pub fn with_capacity(capacity: usize) -> Self {
        Self { nodes: Vec::with_capacity(capacity) }
    }

    fn push(&mut self, style: Style, context: Option<usize>, leaf_key: Option<u64>) -> NodeId {
        let id = NodeId::from(self.nodes.len());
        self.nodes.push(Node { style, built_style: None, children: Vec::new(), parent: None, context, leaf_key, adopted: false, cache: Cache::default(), unrounded: Layout::new(), layout: Layout::new() });
        id
    }

    pub fn new_leaf(&mut self, style: Style) -> NodeId {
        self.push(style, None, Some(0))
    }

    /// A leaf the measure function sizes. `leaf_key` says what its answers depend on besides the question, and a leaf with none
    /// keeps whatever it is in from taking over an earlier frame's layout.
    pub fn new_leaf_with_context(&mut self, style: Style, context: usize, leaf_key: Option<u64>) -> NodeId {
        self.push(style, Some(context), leaf_key)
    }

    fn node(&self, id: NodeId) -> &Node {
        &self.nodes[usize::from(id)]
    }

    fn node_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[usize::from(id)]
    }

    pub fn set_children(&mut self, parent: NodeId, children: &[NodeId]) {
        for &child in children {
            self.node_mut(child).parent = Some(parent);
        }
        self.node_mut(parent).children = children.to_vec();
        self.dirty(parent);
    }

    pub fn style(&self, id: NodeId) -> &Style {
        &self.node(id).style
    }

    /// A new style, and what was worked out from the old one forgotten, here and in everything around it.
    #[cfg(test)]
    pub fn set_style(&mut self, id: NodeId, style: Style) {
        self.node_mut(id).style = style;
        self.dirty(id);
    }

    /// [`Self::set_style`] once the tree is built, keeping the style it was built with: what tells whether the next frame's
    /// tree is the same.
    pub fn set_capped_style(&mut self, id: NodeId, style: Style) {
        let node = self.node_mut(id);
        if node.built_style.is_none() {
            node.built_style = Some(Box::new(std::mem::replace(&mut node.style, style)));
        } else {
            node.style = style;
        }
        self.dirty(id);
    }

    /// Whether `id` took over an earlier frame's layout, itself or as part of something around it.
    pub fn adopted(&self, id: NodeId) -> bool {
        self.node(id).adopted
    }

    /// When the subtree under `id` is built as the one under `old_id` in `old` was, takes over everything `old` worked out for it:
    /// each node's answers and where it placed its children. `old` is the last frame's tree, done with: what is taken over is
    /// moved out of it.
    ///
    /// A node a width limit was imposed on last frame ([`Self::set_capped_style`]) has its built style again, since whether the
    /// limit applies depends on the room this frame offers, and the answers worked out under the limit are dropped for it and
    /// what is around it. The rest of the subtree keeps its own.
    pub fn adopt(&mut self, id: NodeId, old: &mut LayoutTree, old_id: NodeId) -> bool {
        if !self.same_as(id, old, old_id) {
            return false;
        }
        let mut capped = Vec::new();
        let mut stack = vec![(id, old_id)];
        while let Some((at, was)) = stack.pop() {
            let before = &mut old.nodes[usize::from(was)];
            let node = &mut self.nodes[usize::from(at)];
            node.cache = std::mem::take(&mut before.cache);
            node.unrounded = before.unrounded;
            if before.built_style.is_some() {
                capped.push(at);
            }
            node.adopted = true;
            stack.extend(node.children.iter().copied().zip(before.children.iter().copied()));
        }
        for at in capped {
            self.dirty(at);
        }
        true
    }

    /// Built alike: the same styles as built, the same leaves, the same shape.
    fn same_as(&self, id: NodeId, old: &LayoutTree, old_id: NodeId) -> bool {
        let mut stack = vec![(id, old_id)];
        while let Some((at, was)) = stack.pop() {
            let (a, b) = (self.node(at), old.node(was));
            if a.leaf_key.is_none() || a.leaf_key != b.leaf_key || a.children.len() != b.children.len() || a.style != *b.built_style.as_deref().unwrap_or(&b.style) {
                return false;
            }
            stack.extend(a.children.iter().copied().zip(b.children.iter().copied()));
        }
        true
    }

    fn dirty(&mut self, id: NodeId) {
        let mut at = Some(id);
        while let Some(id) = at {
            let node = self.node_mut(id);
            node.cache.clear();
            at = node.parent;
        }
    }

    /// The node's layout as last worked out, rounded to whole pixels.
    pub fn layout(&self, id: NodeId) -> &Layout {
        &self.node(id).layout
    }

    /// The node's width before rounding: what its leaf was measured at.
    pub fn unrounded_width(&self, id: NodeId) -> f32 {
        self.node(id).unrounded.size.width
    }

    /// Lays out the tree under `root` in `available`, asking `measure` the size of each leaf that has a context.
    pub fn compute_layout_with_measure<M>(&mut self, root: NodeId, available: Size<AvailableSpace>, measure: M)
    where
        M: FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut usize>, &Style) -> Size<f32>,
    {
        let mut view = View { tree: self, measure };
        compute_root_layout(&mut view, root, available);
        round_layout(&mut view, root);
    }
}

/// The tree with the measure function the algorithms reach leaves through.
struct View<'t, M> {
    tree: &'t mut LayoutTree,
    measure: M,
}

impl<M> TraversePartialTree for View<'_, M> {
    type ChildIter<'a>
        = std::iter::Copied<std::slice::Iter<'a, NodeId>>
    where
        Self: 'a;

    fn child_ids(&self, parent: NodeId) -> Self::ChildIter<'_> {
        self.tree.node(parent).children.iter().copied()
    }

    fn child_count(&self, parent: NodeId) -> usize {
        self.tree.node(parent).children.len()
    }

    fn get_child_id(&self, parent: NodeId, index: usize) -> NodeId {
        self.tree.node(parent).children[index]
    }
}

impl<M> TraverseTree for View<'_, M> {}

impl<M> LayoutPartialTree for View<'_, M>
where
    M: FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut usize>, &Style) -> Size<f32>,
{
    type CoreContainerStyle<'a>
        = &'a Style
    where
        Self: 'a;

    type CustomIdent = String;

    fn get_core_container_style(&self, id: NodeId) -> Self::CoreContainerStyle<'_> {
        &self.tree.node(id).style
    }

    fn set_unrounded_layout(&mut self, id: NodeId, layout: &Layout) {
        self.tree.node_mut(id).unrounded = *layout;
    }

    /// As taffy's own tree has it: what the node's display asks for, through the cache.
    fn compute_child_layout(&mut self, id: NodeId, inputs: LayoutInput) -> LayoutOutput {
        // Under something with `display: none`, whatever this one's own display.
        if inputs.run_mode == RunMode::PerformHiddenLayout {
            return compute_hidden_layout(self, id);
        }
        compute_cached_layout(self, id, inputs, |view, id, inputs| {
            let node = view.tree.node(id);
            match (node.style.display, !node.children.is_empty()) {
                (Display::None, _) => compute_hidden_layout(view, id),
                (Display::Block, true) => compute_block_layout(view, id, inputs),
                (Display::Flex, true) => compute_flexbox_layout(view, id, inputs),
                (Display::Grid, true) => compute_grid_layout(view, id, inputs),
                (_, false) => {
                    let View { tree, measure } = view;
                    let node = &tree.nodes[usize::from(id)];
                    let mut context = node.context;
                    let style = &node.style;
                    compute_leaf_layout(inputs, style, |_, _| 0.0, |known, available| measure(known, available, id, context.as_mut(), style))
                }
            }
        })
    }
}

impl<M> CacheTree for View<'_, M> {
    fn cache_get(&self, id: NodeId, known: Size<Option<f32>>, available: Size<AvailableSpace>, run_mode: RunMode) -> Option<LayoutOutput> {
        self.tree.node(id).cache.get(known, available, run_mode)
    }

    fn cache_store(&mut self, id: NodeId, known: Size<Option<f32>>, available: Size<AvailableSpace>, run_mode: RunMode, output: LayoutOutput) {
        self.tree.node_mut(id).cache.store(known, available, run_mode, output);
    }

    fn cache_clear(&mut self, id: NodeId) {
        self.tree.node_mut(id).cache.clear();
    }
}

impl<M> LayoutFlexboxContainer for View<'_, M>
where
    M: FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut usize>, &Style) -> Size<f32>,
{
    type FlexboxContainerStyle<'a>
        = &'a Style
    where
        Self: 'a;
    type FlexboxItemStyle<'a>
        = &'a Style
    where
        Self: 'a;

    fn get_flexbox_container_style(&self, id: NodeId) -> Self::FlexboxContainerStyle<'_> {
        &self.tree.node(id).style
    }

    fn get_flexbox_child_style(&self, id: NodeId) -> Self::FlexboxItemStyle<'_> {
        &self.tree.node(id).style
    }
}

impl<M> LayoutGridContainer for View<'_, M>
where
    M: FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut usize>, &Style) -> Size<f32>,
{
    type GridContainerStyle<'a>
        = &'a Style
    where
        Self: 'a;
    type GridItemStyle<'a>
        = &'a Style
    where
        Self: 'a;

    fn get_grid_container_style(&self, id: NodeId) -> Self::GridContainerStyle<'_> {
        &self.tree.node(id).style
    }

    fn get_grid_child_style(&self, id: NodeId) -> Self::GridItemStyle<'_> {
        &self.tree.node(id).style
    }
}

impl<M> LayoutBlockContainer for View<'_, M>
where
    M: FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut usize>, &Style) -> Size<f32>,
{
    type BlockContainerStyle<'a>
        = &'a Style
    where
        Self: 'a;
    type BlockItemStyle<'a>
        = &'a Style
    where
        Self: 'a;

    fn get_block_container_style(&self, id: NodeId) -> Self::BlockContainerStyle<'_> {
        &self.tree.node(id).style
    }

    fn get_block_child_style(&self, id: NodeId) -> Self::BlockItemStyle<'_> {
        &self.tree.node(id).style
    }
}

impl<M> RoundTree for View<'_, M> {
    fn get_unrounded_layout(&self, id: NodeId) -> Layout {
        self.tree.node(id).unrounded
    }

    fn set_final_layout(&mut self, id: NodeId, layout: &Layout) {
        self.tree.node_mut(id).layout = *layout;
    }
}

#[cfg(test)]
mod tests {
    use taffy::prelude::{auto, length, percent, AlignItems, FlexDirection, JustifyContent, TaffyTree};
    use taffy::style::Dimension;

    use super::*;

    /// A few random numbers, the same every run.
    struct Dice(u64);

    impl Dice {
        fn roll(&mut self, sides: u64) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) % sides
        }
    }

    /// A leaf as the interface has them: a fixed icon, a line of text, or a line that gives way to an ellipsis.
    #[derive(Clone, Copy)]
    enum Leaf {
        Icon(f32),
        Line(f32),
        Ellipsis(f32),
    }

    fn leaf_size(leaf: Leaf, known: Size<Option<f32>>, available: Size<AvailableSpace>) -> Size<f32> {
        let (width, height) = match leaf {
            Leaf::Icon(side) => (side, side),
            Leaf::Line(width) => (width, 20.0),
            Leaf::Ellipsis(width) => (if available.width == AvailableSpace::MinContent { 0.0 } else { width }, 20.0),
        };
        Size { width: known.width.unwrap_or(width), height: known.height.unwrap_or(height) }
    }

    fn random_style(dice: &mut Dice, container: bool) -> Style {
        let mut style = Style::default();
        if container {
            style.flex_direction = if dice.roll(2) == 0 { FlexDirection::Row } else { FlexDirection::Column };
            style.align_items = Some([AlignItems::Stretch, AlignItems::Center, AlignItems::Start][dice.roll(3) as usize]);
            style.justify_content = Some([JustifyContent::Start, JustifyContent::SpaceBetween, JustifyContent::Center][dice.roll(3) as usize]);
            style.gap = Size { width: length(dice.roll(3) as f32 * 4.0), height: length(dice.roll(3) as f32 * 4.0) };
            let pad = dice.roll(3) as f32 * 3.0;
            style.padding = taffy::geometry::Rect { left: length(pad), right: length(pad), top: length(pad * 0.5), bottom: length(pad * 0.5) };
        }
        match dice.roll(5) {
            0 => style.size.width = percent(1.0),
            1 => style.size.width = length(40.0 + dice.roll(200) as f32),
            2 => style.flex_grow = 1.0,
            _ => {}
        }
        if dice.roll(3) == 0 {
            style.min_size.width = length(0.0);
        }
        if dice.roll(5) == 0 {
            style.max_size.width = length(60.0 + dice.roll(300) as f32);
        }
        if dice.roll(4) == 0 {
            style.min_size.height = length(10.0 + dice.roll(30) as f32);
        }
        if dice.roll(5) == 0 {
            style.flex_shrink = 0.0;
        }
        style
    }

    /// The same random tree built in both; each pair is ours and taffy's, the root last.
    fn build(seed: u64, ours: &mut LayoutTree, theirs: &mut TaffyTree<usize>, leaves: &mut Vec<Leaf>) -> Vec<(NodeId, NodeId)> {
        fn node(dice: &mut Dice, depth: usize, ours: &mut LayoutTree, theirs: &mut TaffyTree<usize>, leaves: &mut Vec<Leaf>, pairs: &mut Vec<(NodeId, NodeId)>) -> (NodeId, NodeId) {
            let container = depth < 8 && dice.roll(4) != 0;
            let style = random_style(dice, container);
            let pair = if container {
                let children: Vec<(NodeId, NodeId)> = (0..1 + dice.roll(4)).map(|_| node(dice, depth + 1, ours, theirs, leaves, pairs)).collect();
                let (a, b) = (ours.new_leaf(style.clone()), theirs.new_leaf(style).unwrap());
                ours.set_children(a, &children.iter().map(|pair| pair.0).collect::<Vec<_>>());
                theirs.set_children(b, &children.iter().map(|pair| pair.1).collect::<Vec<_>>()).unwrap();
                (a, b)
            } else {
                let width = 8.0 * (1 + dice.roll(40)) as f32;
                leaves.push(match dice.roll(3) {
                    0 => Leaf::Icon(12.0 + dice.roll(3) as f32 * 2.0),
                    1 => Leaf::Line(width),
                    _ => Leaf::Ellipsis(width),
                });
                (ours.new_leaf_with_context(style.clone(), leaves.len() - 1, Some(leaves.len() as u64)), theirs.new_leaf_with_context(style, leaves.len() - 1).unwrap())
            };
            pairs.push(pair);
            pair
        }
        let mut dice = Dice(seed);
        let mut pairs = Vec::new();
        let (a, b) = node(&mut dice, 0, ours, theirs, leaves, &mut pairs);
        let mut root = ours.style(a).clone();
        root.size = Size { width: length(700.0), height: Dimension::auto() };
        ours.set_style(a, root.clone());
        theirs.set_style(b, root).unwrap();
        pairs
    }

    /// Where text wraps to the width it is asked about, taffy answers some questions from whatever its slots happen to hold, so its
    /// layout depends on the order the questions came in, and no other cache can promise to match it (nor does it match itself laid
    /// out with no cache at all). With leaves whose size does not follow the width offered, this tree has to match taffy's exactly.
    #[test]
    fn lays_out_as_taffys_own_tree_does() {
        for seed in 0..500u64 {
            let (mut ours, mut theirs, mut leaves) = (LayoutTree::default(), TaffyTree::new(), Vec::new());
            let pairs = build(seed, &mut ours, &mut theirs, &mut leaves);
            let (a, b) = *pairs.last().unwrap();
            let available = Size { width: AvailableSpace::Definite(700.0), height: AvailableSpace::MaxContent };
            ours.compute_layout_with_measure(a, available, |known, available, _, context, _| context.map_or(Size::ZERO, |at| leaf_size(leaves[*at], known, available)));
            theirs.compute_layout_with_measure(b, available, |known, available, _, context, _| context.map_or(Size::ZERO, |at| leaf_size(leaves[*at], known, available))).unwrap();
            for (at, &(a, b)) in pairs.iter().enumerate() {
                let (mine, theirs) = (ours.layout(a), theirs.layout(b).unwrap());
                assert_eq!((mine.location, mine.size, mine.content_size), (theirs.location, theirs.size, theirs.content_size), "seed {seed}, node {at}");
            }
        }
    }

    /// Last frame's tree, then this frame's: the same but for one leaf. Whatever did not change takes over last frame's layout,
    /// and the tree comes out exactly as it does laid out from nothing.
    #[test]
    fn what_did_not_change_keeps_its_layout_and_the_tree_comes_out_the_same() {
        let available = Size { width: AvailableSpace::Definite(700.0), height: AvailableSpace::MaxContent };
        let mut adopted_any = false;
        for seed in 0..400u64 {
            let (mut old, mut scratch, mut leaves) = (LayoutTree::default(), TaffyTree::new(), Vec::new());
            let pairs = build(seed, &mut old, &mut scratch, &mut leaves);
            let root = pairs.last().unwrap().0;
            old.compute_layout_with_measure(root, available, |known, available, _, context, _| context.map_or(Size::ZERO, |at| leaf_size(leaves[*at], known, available)));

            // This frame changes one leaf, and so its key.
            let mut changed = leaves.clone();
            let at = (seed as usize * 7) % changed.len();
            changed[at] = match changed[at] {
                Leaf::Icon(side) => Leaf::Icon(side + 2.0),
                Leaf::Line(width) => Leaf::Line(width + 24.0),
                Leaf::Ellipsis(width) => Leaf::Line(width),
            };
            let rebuild = |tree: &mut LayoutTree| {
                let (mut scratch, mut unused) = (TaffyTree::new(), Vec::new());
                build(seed, tree, &mut scratch, &mut unused);
                let node = tree.nodes.iter_mut().find(|node| node.context == Some(at)).expect("the leaf");
                node.leaf_key = Some(10_000 + at as u64);
            };
            let (mut new, mut fresh) = (LayoutTree::default(), LayoutTree::default());
            rebuild(&mut new);
            rebuild(&mut fresh);

            // Outermost first, as a frame takes them: what is under a subtree taken over is taken over with it.
            fn take_over(new: &mut LayoutTree, old: &mut LayoutTree, id: NodeId, any: &mut bool) {
                if new.adopt(id, old, id) {
                    *any = true;
                    return;
                }
                for child in new.node(id).children.clone() {
                    take_over(new, old, child, any);
                }
            }
            take_over(&mut new, &mut old, root, &mut adopted_any);
            assert!(!new.adopted(root), "seed {seed}: the root has a changed leaf under it");

            let measure = |known: Size<Option<f32>>, available: Size<AvailableSpace>, _: NodeId, context: Option<&mut usize>, _: &Style| context.map_or(Size::ZERO, |at| leaf_size(changed[*at], known, available));
            new.compute_layout_with_measure(root, available, measure);
            fresh.compute_layout_with_measure(root, available, measure);
            for at in 0..fresh.nodes.len() {
                let id = NodeId::from(at);
                let (mine, theirs) = (new.layout(id), fresh.layout(id));
                assert_eq!((mine.location, mine.size, mine.content_size), (theirs.location, theirs.size, theirs.content_size), "seed {seed}, node {at}");
            }
        }
        assert!(adopted_any, "some subtree was taken over");
    }

    #[test]
    fn a_leaf_nobody_can_key_is_never_taken_over() {
        let mut old = LayoutTree::default();
        let leaf = old.new_leaf_with_context(Style::default(), 0, None);
        let parent = old.new_leaf(Style::default());
        old.set_children(parent, &[leaf]);
        old.compute_layout_with_measure(parent, Size { width: AvailableSpace::Definite(100.0), height: AvailableSpace::MaxContent }, |_, _, _, _, _| Size { width: 10.0, height: 10.0 });
        let mut new = LayoutTree::default();
        let leaf = new.new_leaf_with_context(Style::default(), 0, None);
        let parent = new.new_leaf(Style::default());
        new.set_children(parent, &[leaf]);
        assert!(!new.adopt(parent, &mut old, parent));
    }

    #[test]
    fn asks_a_leaf_deep_in_rows_each_question_once() {
        // Rows in columns in rows, as a tool row sits in a timeline: each level asks what is under it two questions of one kind.
        let mut tree = LayoutTree::default();
        let leaf = tree.new_leaf_with_context(Style { min_size: Size { width: length(0.0), height: auto() }, ..Style::default() }, 0, Some(1));
        let mut top = leaf;
        for depth in 0..12 {
            let direction = if depth % 2 == 0 { FlexDirection::Row } else { FlexDirection::Column };
            let style = Style { flex_direction: direction, min_size: Size { width: length(0.0), height: length(30.0) }, padding: taffy::geometry::Rect { left: length(2.0), right: length(2.0), top: length(1.0), bottom: length(1.0) }, ..Style::default() };
            let parent = tree.new_leaf(style);
            tree.set_children(parent, &[top]);
            top = parent;
        }
        let mut asked = Vec::new();
        tree.compute_layout_with_measure(top, Size { width: AvailableSpace::Definite(500.0), height: AvailableSpace::Definite(400.0) }, |known, available, _, _, _| {
            asked.push(format!("{known:?} {available:?}"));
            leaf_size(Leaf::Line(240.0), known, available)
        });
        let distinct: std::collections::HashSet<&String> = asked.iter().collect();
        assert_eq!(asked.len(), distinct.len(), "asked {} times for {} questions: {asked:#?}", asked.len(), distinct.len());
    }
}
