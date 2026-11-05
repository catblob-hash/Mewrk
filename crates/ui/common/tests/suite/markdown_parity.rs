//! What `MarkdownContent.tsx` makes of a table, a footnote and a task list, past the parse of them.

use super::*;

fn view(key: &'static str, source: &'static str) -> impl Fn(&mut App) -> E {
    move |_| col().w_full().child(markdown_content(key, source, MarkdownOptions { render_html: true, ..MarkdownOptions::default() }))
}

fn table_rows(source: &str) -> Vec<TableRow> {
    let blocks = parse_markdown(source, ParseOptions { render_html: true, single_dollar_math: true, ..ParseOptions::default() });
    blocks.into_iter().find_map(|b| if let Block::Table { rows, .. } = b { Some(rows) } else { None }).expect("a table")
}

#[test]
fn a_cell_says_what_it_spans_and_whether_it_is_a_heading() {
    let rows = table_rows("<table><tr><th colspan=\"2\">a</th><td rowspan=\"3\">b</td></tr><tr><td colspan=0>c</td><td rowspan=nonsense>d</td></tr></table>");
    assert_eq!((rows[0].cells[0].colspan, rows[0].cells[0].rowspan, rows[0].cells[0].header), (2, 1, true));
    assert_eq!((rows[0].cells[1].colspan, rows[0].cells[1].rowspan, rows[0].cells[1].header), (1, 3, false));
    assert_eq!(rows[1].cells[0].colspan, 1, "0 is not a colspan");
    assert_eq!(rows[1].cells[1].rowspan, 1, "and this is not a rowspan");
    assert!(table_rows("<table><tr><td rowspan=\"0\">a</td></tr></table>")[0].cells[0].rowspan == 0, "to the end of its section");
    assert!(table_rows("<table><tr><td colspan=\"99999\">a</td></tr></table>")[0].cells[0].colspan == 1000);
}

#[test]
fn the_head_of_a_table_is_on_top_and_its_foot_at_the_bottom_wherever_they_were_written() {
    let rows = table_rows("<table><tfoot><tr><td>foot</td></tr></tfoot><tbody><tr><td>body</td></tr></tbody><thead><tr><th>head</th></tr></thead></table>");
    let first = |row: &TableRow| match &row.cells[0].inlines[0] {
        Inline::Text { text, .. } => text.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(rows.iter().map(first).collect::<Vec<_>>(), ["head", "body", "foot"]);
    assert!(rows[0].section != rows[1].section && rows[1].section != rows[2].section);
}

#[test]
fn a_spanning_cell_takes_the_room_of_what_it_spans() {
    let mut rig = Rig::new();
    let source = "<table><tr><td colspan=\"2\">wide</td><td>c</td></tr><tr><td rowspan=\"2\">tall</td><td>e</td><td>f</td></tr><tr><td>g</td><td>h</td></tr></table>";
    let view = view("spans", source);
    rig.settle(&view);
    let pieces = probe::pieces("spans");
    let at = |text: &str| pieces.iter().find(|p| p.0 == text).unwrap_or_else(|| panic!("{text} in {pieces:?}"));
    // "c" is past both columns of "wide"; "e" starts where "wide"'s second column does.
    assert!(at("c").1 > at("e").1, "{pieces:?}");
    assert_eq!(at("c").1, at("f").1);
    assert_eq!(at("c").1, at("h").1);
    // "g" stands beside the rowspan, not under it, in the column of "e".
    assert_eq!(at("g").1, at("e").1);
    assert!(at("g").2 > at("e").2);
    assert_eq!(at("tall").1, at("wide").1);
    let (level, upright) = probe::table_rules("spans", 0).expect("rules");
    // No rule runs through what a cell spans: the row between "wide"'s two columns has one upright line less than the others.
    let x_between = at("e").1 - 0.62 * 13.0 * 0.94 - 1.0;
    let through = |y: f32| upright.iter().any(|r| (r[0] - x_between).abs() < 1.0 && r[1] <= y && y <= r[1] + r[2]);
    assert!(!through(at("wide").2), "{upright:?}");
    assert!(through(at("e").2), "{upright:?}");
    // And none between "tall"'s two rows, where the level rule starts past it.
    let y_between = at("g").2 - 0.38 * 13.0 * 0.94 - 1.0;
    let across = level.iter().find(|r| (r[1] - y_between).abs() < 1.5).unwrap_or_else(|| panic!("{level:?} at {y_between}"));
    assert!(across[0] > at("tall").1, "{across:?}");
}

#[test]
fn what_a_span_needs_past_its_columns_goes_to_them() {
    let mut rig = Rig::new();
    let source = "<table><tr><td colspan=\"2\">a very long heading across two columns</td></tr><tr><td>x</td><td>y</td></tr></table>";
    let view = view("wide", source);
    rig.settle(&view);
    let pieces = probe::pieces("wide");
    let at = |text: &str| pieces.iter().find(|p| p.0 == text).unwrap();
    // The second column has moved over from where "x" alone would put it.
    assert!(at("y").1 - at("x").1 > 40.0, "{pieces:?}");
    let rows = table_rows(source);
    assert_eq!(rows.len(), 2);
}

#[test]
fn a_rowspan_stops_at_the_end_of_its_section() {
    let mut rig = Rig::new();
    let source = "<table><thead><tr><th rowspan=\"5\">a</th><th>b</th></tr></thead><tbody><tr><td>c</td><td>d</td></tr></tbody></table>";
    let view = view("section", source);
    rig.settle(&view);
    let pieces = probe::pieces("section");
    let at = |text: &str| pieces.iter().find(|p| p.0 == text).unwrap();
    assert_eq!(at("c").1, at("a").1, "the body starts in the first column again: {pieces:?}");
}

#[test]
fn a_cell_can_hold_a_picture_and_blocks() {
    let mut rig = Rig::new();
    let pixels = MarkdownImage { id: fluent::gpu::ImageId(1), size: (40, 20) };
    let source = "| a | b |\n|---|---|\n| ![pic](x.png) | text |\n| ![none](y.png) | more |";
    let view = move |_: &mut App| {
        let resolve: ImageResolver = Rc::new(move |src: &str| (src == "x.png").then_some(pixels));
        col().w_full().child(markdown_content("cell-image", source, MarkdownOptions { resolve_image_src: Some(resolve), ..MarkdownOptions::default() }))
    };
    rig.settle(&view);
    let images = probe::images("cell-image");
    assert_eq!(images.len(), 1, "the one that resolves is drawn");
    assert_eq!((images[0].w, images[0].h), (40.0, 20.0));
    let pieces = probe::pieces("cell-image");
    let at = |text: &str| pieces.iter().find(|p| p.0 == text).unwrap_or_else(|| panic!("{text} in {pieces:?}"));
    // Its margins are inside the cell, so the row is as tall as the picture and more, and the next row is under it.
    assert!(at("more").2 > images[0].bottom(), "{pieces:?} {images:?}");
    assert!(at("text").2 < images[0].bottom());
    // The other cell keeps its description in the line, as a picture that cannot be drawn does anywhere.
    assert!(pieces.iter().any(|p| p.0 == "none"), "{pieces:?}");
    // The column is as wide as the picture.
    assert!(at("text").1 >= images[0].right(), "{pieces:?} {images:?}");

    let rows = table_rows("<table><tr><td><ul><li>one</li><li>two</li></ul></td><td><p>para</p><p>graph</p></td><td>beside</td></tr><tr><td>next</td></tr></table>");
    assert!(rows[0].cells[0].blocks.is_some() && rows[0].cells[1].blocks.is_some() && rows[0].cells[2].blocks.is_none());
    let source = "<table><tr><td><ul><li>one</li><li>two</li></ul></td><td><p>para</p><p>graph</p></td><td>beside</td></tr><tr><td>next</td></tr></table>";
    let view = super::markdown_parity::view("blocks", source);
    rig.settle(&view);
    let pieces = probe::pieces("blocks");
    let at = |text: &str| pieces.iter().find(|p| p.0 == text).unwrap_or_else(|| panic!("{text} in {pieces:?}"));
    assert!(at("two").2 > at("one").2);
    assert!(at("graph").2 > at("para").2);
    assert_eq!(at("graph").1, at("para").1);
    assert!(at("para").1 > at("one").1);
    assert!(at("beside").1 > at("para").1);
    assert!(at("next").2 > at("two").2.max(at("graph").2));
    // Text in a cell of several pieces is still found under the pointer, though the pieces do not go down in order.
    rig.input.pointer = Some((at("beside").1 + 2.0, at("beside").2 + 4.0));
    rig.frame(&view);
    assert_eq!(rig.cursor, fluent::ui::CursorKind::Text);
}

/// A wide table with a code block in its second cell.
const TABLE_WITH_CODE: &str = "<table><tr><td>a very long cell that goes on past the edge of a narrow column, and then goes on a little further still</td><td><pre><code>let x = 1;</code></pre></td></tr></table>";

#[test]
fn a_code_block_in_a_cell_scrolls_with_its_table() {
    let mut rig = Rig::new();
    rig.size = (360.0, 300.0);
    let view = view("in-cell", TABLE_WITH_CODE);
    rig.settle(&view);
    // The table comes first in the list, and what it holds after it.
    let (table_scroll, table_max) = probe::region_scroll("in-cell", 0, 0).expect("the table");
    assert_eq!(table_scroll, 0.0);
    assert!(table_max > 100.0, "{table_max}");
    assert_eq!(probe::region_scroll("in-cell", 0, 1).expect("the code").0, 0.0);
    let table = probe::region_rect("in-cell", 0, 0).unwrap();
    assert!(table.w <= rig.size.0);
    // The pieces of the first cell and the code block go through the same scroll: the code block's own is the table's and its.
    rig.input.pointer = Some((table.x + 20.0, table.y + table.h * 0.5));
    rig.frame(&view);
    rig.input.scroll_x = -60.0;
    rig.frame(&view);
    rig.frame(&view);
    assert_eq!(probe::region_scroll("in-cell", 0, 0).unwrap().0, 60.0, "the table moved");
    assert_eq!(probe::region_scroll("in-cell", 0, 1).unwrap().0, 60.0, "and took the code block with it");
    assert_eq!(probe::region_scroll_own("in-cell", 0, 1).unwrap(), 0.0, "which has no scroll of its own");
    // What is drawn moved: a glyph of the code block is 60 further left than it was.
    let drawn = |rig: &Rig| rig.scene.layers[0].rects.iter().filter(|inst| inst.params[2] == fluent::gpu::KIND_RECT).map(|inst| inst.rect[0]).collect::<Vec<_>>();
    let scrolled = drawn(&rig);
    rig.input.scroll_x = 60.0;
    rig.frame(&view);
    rig.frame(&view);
    let back = drawn(&rig);
    assert_eq!(probe::region_scroll("in-cell", 0, 0).unwrap().0, 0.0);
    let code_box = |rects: &[f32]| rects.iter().copied().fold(f32::MIN, f32::max);
    assert_eq!(code_box(&back) - code_box(&scrolled), 60.0, "its box goes with it");
}

fn float_resolver() -> ImageResolver {
    let pixels: Vec<u8> = vec![200; 4 * 40 * 40];
    let picture = MarkdownImage { id: fluent::gpu::queue_image(pixels, 40, 40), size: (40, 40) };
    Rc::new(move |src: &str| (src == "pic.png").then_some(picture))
}

fn float_view(key: &'static str, source: &'static str) -> impl Fn(&mut App) -> E {
    let resolve = float_resolver();
    move |_| col().w_full().child(markdown_content(key, source, MarkdownOptions { render_html: true, resolve_image_src: Some(resolve.clone()), ..MarkdownOptions::default() }))
}

const LONG_TEXT: &str = "word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word";

#[test]
fn the_sanitiser_lets_a_pictures_side_through_and_nothing_else_of_align_floats() {
    let picture = |align: &str| {
        let source = format!("<img src=\"pic.png\" alt=\"x\" align=\"{align}\">");
        let blocks = parse_markdown(&source, ParseOptions { render_html: true, single_dollar_math: true, ..ParseOptions::default() });
        let inlines = para(&blocks[0]).to_vec();
        match &inlines[0] {
            Inline::Image { float, .. } => *float,
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(picture("left"), Some(Float::Left));
    assert_eq!(picture("RIGHT"), Some(Float::Right));
    assert_eq!(picture("middle"), None);
    assert_eq!(picture("center"), None);
}

#[test]
fn a_floated_picture_takes_the_side_and_the_text_runs_around_it_then_returns() {
    let mut rig = Rig::new();
    rig.size = (420.0, 600.0);
    let source = Box::leak(format!("<p><img src=\"pic.png\" align=\"right\" width=\"120\">{LONG_TEXT} {LONG_TEXT}</p>").into_boxed_str());
    let view = float_view("float-right", source);
    rig.settle(&view);
    let picture = probe::images("float-right")[0];
    assert_eq!(picture.w, 120.0);
    assert!((picture.right() - 420.0).abs() < 0.5, "against the right: {picture:?}");
    assert_eq!(picture.y, 0.0, "`> :first-child` takes its margin away");
    let lines = probe::line_rects("float-right");
    let beside: Vec<_> = lines.iter().filter(|line| line.y < picture.bottom()).collect();
    let below: Vec<_> = lines.iter().filter(|line| line.y >= picture.bottom() + 14.0).collect();
    assert!(!beside.is_empty() && !below.is_empty(), "{lines:?}");
    assert!(beside.iter().all(|line| line.right() <= picture.x + 0.5), "stays clear of it: {beside:?} {picture:?}");
    assert!(below.iter().any(|line| line.right() > picture.x + 20.0), "and takes the width back under it: {below:?}");
}

#[test]
fn a_float_reaches_into_the_blocks_after_it_and_stops_at_its_bottom() {
    let mut rig = Rig::new();
    rig.size = (420.0, 700.0);
    let source = Box::leak(format!("<img src=\"pic.png\" align=\"left\" width=\"120\" height=\"200\">\n\n# Title\n\n{LONG_TEXT}\n\n{LONG_TEXT}").into_boxed_str());
    let view = float_view("float-left", source);
    rig.settle(&view);
    let picture = probe::images("float-left")[0];
    assert_eq!(picture.x, 0.0);
    let lines = probe::line_rects("float-left");
    let beside: Vec<_> = lines.iter().filter(|line| line.y < picture.bottom() - 1.0).collect();
    assert!(beside.len() > 2, "{lines:?}");
    assert!(beside.iter().all(|line| line.x >= picture.right() - 0.5), "the heading and the paragraph start after it: {beside:?}");
    let far_below = lines.iter().filter(|line| line.y > picture.bottom() + 12.0).collect::<Vec<_>>();
    assert!(!far_below.is_empty() && far_below.iter().all(|line| line.x == 0.0), "{far_below:?}");
    // The float takes no room of its own: what follows starts where it would have without it.
    assert!(lines[0].y < 10.0, "{:?}", lines[0]);
}

#[test]
fn a_float_as_wide_as_the_column_sends_the_text_under_it_and_a_tall_one_counts_for_the_height() {
    let mut rig = Rig::new();
    rig.size = (300.0, 800.0);
    let source = "<p><img src=\"pic.png\" align=\"left\">word</p>";
    let view = float_view("float-wide", source);
    rig.settle(&view);
    let picture = probe::images("float-wide")[0];
    let line = probe::line_rects("float-wide")[0];
    assert!(picture.w >= 40.0);
    assert!(line.x > 0.0 || line.y >= picture.bottom(), "beside it or under it, never over it: {line:?} {picture:?}");
    let source = "<img src=\"pic.png\" align=\"left\" width=\"300\">\n\nword";
    let view = float_view("float-full", source);
    rig.settle(&view);
    let (picture, line) = (probe::images("float-full")[0], probe::line_rects("float-full")[0]);
    assert!(line.y >= picture.bottom(), "no word fits beside it: {line:?} {picture:?}");
}

#[test]
fn a_box_that_keeps_its_own_width_stands_beside_a_float() {
    let mut rig = Rig::new();
    rig.size = (420.0, 600.0);
    let source = "<img src=\"pic.png\" align=\"right\" width=\"120\" height=\"200\">\n\n```\ncode\n```";
    let view = float_view("float-code", source);
    rig.settle(&view);
    let (picture, code) = (probe::images("float-code")[0], probe::region_rect("float-code", 1, 0).expect("the code"));
    assert!(code.right() <= picture.x + 0.5, "{code:?} {picture:?}");
}

#[test]
fn a_ruby_has_its_reading_over_its_base_and_the_line_makes_room() {
    let blocks = parse_markdown("<ruby>漢<rp>(</rp><rt>kan</rt><rp>)</rp>字<rt>ji</rt></ruby> plain", ParseOptions { render_html: true, single_dollar_math: true, ..ParseOptions::default() });
    let inlines = para(&blocks[0]);
    assert_eq!(plain(inlines), "漢(kan)字(ji) plain");
    let mut rig = Rig::new();
    let ruby = "<ruby>漢<rp>(</rp><rt>kan</rt><rp>)</rp>字<rt>ji</rt></ruby> plain";
    let view = view("ruby", ruby);
    rig.settle(&view);
    let placed = probe::placed_rubies("ruby");
    assert_eq!(placed.iter().map(|(base, reading, ..)| (base.as_str(), reading.as_str())).collect::<Vec<_>>(), [("漢", "kan"), ("字", "ji")]);
    assert!(placed[1].2 > placed[0].2, "one after the other: {placed:?}");
    assert_eq!(placed[0].3, placed[1].3, "on one baseline");
    let with = probe::line_rects("ruby")[0].h;
    let plain_view = self::view("ruby-plain", "漢字 plain");
    rig.settle(&plain_view);
    assert!(with > probe::line_rects("ruby-plain")[0].h, "taller than a line without one");
}

#[test]
fn a_ruby_with_nothing_to_read_is_its_text() {
    let blocks = parse_markdown("<ruby>漢字</ruby>", ParseOptions { render_html: true, single_dollar_math: true, ..ParseOptions::default() });
    assert_eq!(plain(para(&blocks[0])), "漢字");
}

#[test]
#[ignore]
fn visual_floats_ruby_and_code_in_a_cell() {
    let dir = std::env::var("MD_SHOT").expect("where to put the pictures");
    let mut rig = Rig::new();
    rig.size = (620.0, 900.0);
    rig.scale = 2.0;
    rig.text = TextCtx::new(2.0);
    let pixels: Vec<u8> = (0..64 * 40).flat_map(|i| [(i % 64 * 4) as u8, 120, (i / 64 * 6) as u8, 255]).collect();
    let picture = MarkdownImage { id: fluent::gpu::queue_image(pixels, 64, 40), size: (64, 40) };
    let source = Box::leak(format!("<img src=\"pic.png\" align=\"right\" width=\"200\">\n\n# A heading beside a float\n\n{LONG_TEXT}\n\nA second paragraph that starts beside it. {LONG_TEXT}\n\n<p><img src=\"pic.png\" align=\"left\" width=\"110\">Text down the other side of a smaller picture. {LONG_TEXT}</p>\n\nReading: <ruby>漢<rp>(</rp><rt>kan</rt><rp>)</rp>字<rt>ji</rt></ruby> and <ruby>振<rt>ふ</rt>仮<rt>が</rt>名<rt>な</rt></ruby> in a line of text.\n\n{TABLE_WITH_CODE}\n").into_boxed_str());
    let view = move |_: &mut App| {
        let resolve: ImageResolver = Rc::new(move |src: &str| (src == "pic.png").then_some(picture));
        col().w_full().h_full().bg(Theme::dark().layer).p(20.0).child(markdown_content("visual-floats", source, MarkdownOptions { render_html: true, resolve_image_src: Some(resolve), ..MarkdownOptions::default() }))
    };
    rig.settle(&view);
    rig.shot(&format!("{dir}/floats.png"));
}

#[test]
fn footnotes_go_by_their_first_reference_and_lead_back_to_it() {
    let source = "Second[^b] then first[^a] and second again[^b].\n\n[^a]: note a\n[^b]: note b\n[^never]: nobody asked for this\n";
    let blocks = parse_markdown(source, ParseOptions { single_dollar_math: true, ..ParseOptions::default() });
    let notes: Vec<(u32, &str)> = blocks.iter().filter_map(|b| if let Block::Footnote { number, label, .. } = b { Some((*number, label.as_str())) } else { None }).collect();
    assert_eq!(notes, [(1, "b"), (2, "a")], "{blocks:?}");
    let Block::Paragraph { inlines, .. } = &blocks[0] else { panic!("{blocks:?}") };
    let shown: Vec<&str> = inlines.iter().filter_map(|i| if let Inline::Text { text, style, .. } = i { style.sup.then_some(text.as_str()) } else { None }).collect();
    assert_eq!(shown, ["1", "2", "1"], "numbers, not labels");
    let anchors: Vec<&str> = inlines.iter().filter_map(|i| if let Inline::Anchor(a) = i { Some(a.as_str()) } else { None }).collect();
    assert_eq!(anchors, ["user-content-fnref-b", "user-content-fnref-a", "user-content-fnref-b-2"]);
    // Twice referred to, two ways back, the second one numbered.
    let Block::Footnote { blocks: note, .. } = blocks.iter().find(|b| matches!(b, Block::Footnote { number: 1, .. })).unwrap() else { unreachable!() };
    let Block::Paragraph { inlines, .. } = &note[0] else { panic!("{note:?}") };
    let back: Vec<&Link> = inlines.iter().filter_map(|i| if let Inline::Text { text, link: Some(link), .. } = i { text.starts_with('⤶').then_some(&**link) } else { None }).collect();
    assert_eq!(back.len(), 2);
    assert_eq!(back[1].target, LinkTarget::Fragment("user-content-fnref-b-2".to_owned()));
    assert!(inlines.iter().any(|i| matches!(i, Inline::Text { text, style, .. } if text == "2" && style.sup)));

    // A note that ends in a list has its way back after it.
    let blocks = parse_markdown("x[^a]\n\n[^a]: text\n\n    - item\n", ParseOptions::default());
    let Block::Footnote { blocks: note, .. } = blocks.last().unwrap() else { panic!("{blocks:?}") };
    assert!(matches!(note.last(), Some(Block::Paragraph { inlines, .. }) if inlines.iter().any(|i| matches!(i, Inline::Text { text, .. } if text.starts_with('⤶')))), "{note:?}");

    // On the page: at the end, in that order, and a click on a reference reaches its note.
    let mut rig = Rig::new();
    let revealed = Rc::new(RefCell::new(Vec::new()));
    let sink = revealed.clone();
    let view = move |_: &mut App| {
        let sink = sink.clone();
        col().w_full().child(markdown_content("notes", source, MarkdownOptions { on_reveal: Some(Rc::new(move |_, r| sink.borrow_mut().push(r))), ..MarkdownOptions::default() }))
    };
    rig.settle(&view);
    let texts = probe::piece_texts("notes");
    assert!(texts[1].starts_with("note b") && texts[2].starts_with("note a"), "{texts:?}");
    assert!(!texts.iter().any(|t| t.contains("nobody")), "{texts:?}");
    let (x, y) = probe::point_of("notes", 0, 0, 6).expect("the reference");
    rig.click(&view, x, y);
    assert_eq!(revealed.borrow().len(), 1, "it asked to be brought into view");
}

#[test]
fn a_change_in_the_order_of_references_renumbers_notes_it_did_not_touch() {
    let mut rig = Rig::new();
    let first = "One[^a] two[^b].\n\n[^a]: A\n[^b]: B\n";
    let second = "One[^b] two[^a].\n\n[^a]: A\n[^b]: B\n";
    let content = Rc::new(RefCell::new(first.to_owned()));
    let view = {
        let content = content.clone();
        move |_: &mut App| col().w_full().child(markdown_content("renumber", &content.borrow(), MarkdownOptions::default()))
    };
    rig.settle(&view);
    assert!(probe::piece_texts("renumber")[1].starts_with('A'));
    *content.borrow_mut() = second.to_owned();
    rig.settle(&view);
    assert!(probe::piece_texts("renumber")[1].starts_with('B'), "{:?}", probe::piece_texts("renumber"));
}

#[test]
fn a_task_in_a_numbered_list_has_its_box_and_no_number() {
    let mut rig = Rig::new();
    let numbered = view("tasks", "1. plain\n2. [x] done\n3. after\n");
    rig.settle(&numbered);
    // `.task-list-item` is a flex box wherever it is: no marker, and it does not count.
    assert_eq!(probe::list_marks("tasks"), ["1.", "check true", "2."]);
    // `ul.contains-task-list` has no bullets at all, on a plain item either, and hardly any indent.
    let bulleted = view("bullets", "- plain\n- [ ] open\n");
    rig.settle(&bulleted);
    assert_eq!(probe::list_marks("bullets"), ["check false"]);
    let plain = view("plain-list", "- one\n- two\n");
    rig.settle(&plain);
    assert_eq!(probe::list_marks("plain-list"), ["disc", "disc"]);
    let at = |key: &str, text: &str| probe::pieces(key).into_iter().find(|p| p.0 == text).map(|p| p.1).unwrap();
    assert!(at("bullets", "plain") < at("plain-list", "one"));
    // In an `ol` the padding stays.
    assert_eq!(at("tasks", "plain"), at("tasks", "after"));
}

#[test]
fn nested_lists_change_their_mark_with_depth_as_a_browser_does() {
    let mut rig = Rig::new();
    let nested = view("nested-list", "- a\n  - b\n    - c\n      - d\n");
    rig.settle(&nested);
    assert_eq!(probe::list_marks("nested-list"), ["disc", "circle", "square", "square"]);
}

#[test]
fn raw_html_says_what_the_schema_lets_it() {
    let options = ParseOptions { render_html: true, single_dollar_math: true, ..ParseOptions::default() };
    // Whatever an `input` said it was, it is a checkbox.
    let blocks = parse_markdown("a <input type=\"text\" checked> b", options);
    let Block::Paragraph { inlines, .. } = &blocks[0] else { panic!("{blocks:?}") };
    assert!(inlines.iter().any(|i| matches!(i, Inline::Text { text, .. } if text.contains('☑'))), "{inlines:?}");
    // `<samp>` is monospace with no box around it.
    let blocks = parse_markdown("<samp>out</samp>\n", options);
    let found = format!("{blocks:?}");
    assert!(found.contains("mono: true") && found.contains("code: false"), "{found}");
    // Anything with an id is somewhere to go, and takes the prefix the sanitizer gives it.
    let blocks = parse_markdown("<div id=\"place\">text</div>\n\nsome <span id=\"here\">words</span>\n", options);
    let found = format!("{blocks:?}");
    assert!(found.contains("Anchor(\"user-content-place\")") && found.contains("Anchor(\"user-content-here\")"), "{found}");
    // `MarkdownCode`: a class of math is math.
    let blocks = parse_markdown("<code class=\"language-math\">x^2</code>\n", options);
    assert!(format!("{blocks:?}").contains("Math { source: \"x^2\""), "{blocks:?}");
}

#[test]
fn a_picture_in_a_data_address_is_drawn_with_no_resolver() {
    let mut rig = Rig::new();
    // A red pixel, as a data address a reply could carry: the page's `img-src` allows `data:`.
    let url = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==";
    let source = format!("![dot]({url})");
    let view = move |_: &mut App| col().w_full().child(markdown_content("data-image", &source, MarkdownOptions::default()));
    rig.settle(&view);
    for _ in 0..200 {
        rig.frame(&view);
        if !probe::images("data-image").is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let images = probe::images("data-image");
    assert_eq!(images.len(), 1, "{:?}", probe::piece_texts("data-image"));
    assert_eq!((images[0].w, images[0].h), (1.0, 1.0));
    // A remote one is what the page's `img-src` refuses: its description.
    let remote = view_remote();
    rig.settle(&remote);
    assert_eq!(probe::piece_texts("remote-image"), ["far away"]);
}

fn view_remote() -> impl Fn(&mut App) -> E {
    |_| col().w_full().child(markdown_content("remote-image", "![far away](https://example.com/a.png)", MarkdownOptions::default()))
}

#[test]
#[ignore]
fn visual_tables() {
    let dir = std::env::var("MD_SHOT").expect("where to put the pictures");
    let mut rig = Rig::new();
    rig.size = (820.0, 900.0);
    rig.scale = 2.0;
    rig.text = TextCtx::new(2.0);
    let pixels: Vec<u8> = (0..64 * 40).flat_map(|i| [(i % 64 * 4) as u8, 120, (i / 64 * 6) as u8, 255]).collect();
    let picture = MarkdownImage { id: fluent::gpu::queue_image(pixels, 64, 40), size: (64, 40) };
    let source = r#"Pictures in a Markdown table:

| Name | Picture | Note |
|:-----|:-------:|-----:|
| first | ![one](pic.png) | right |
| second | ![none](missing.png) | a longer note to the right |
| third | before ![one](pic.png) after | $x^2$ |

Spans in an HTML one:

<table>
<caption>Quarterly numbers</caption>
<thead><tr><th rowspan="2">Region</th><th colspan="2">First half</th><th colspan="2">Second half</th></tr>
<tr><th>Q1</th><th>Q2</th><th>Q3</th><th>Q4</th></tr></thead>
<tbody>
<tr><td>North</td><td>10</td><td>20</td><td colspan="2" align="center">not reported</td></tr>
<tr><td rowspan="2">South</td><td>30</td><td>40</td><td>50</td><td>60</td></tr>
<tr><td colspan="4" align="right">a note across the rest of the row</td></tr>
<tr><td><ul><li>a list</li><li>in a cell</li></ul></td><td colspan="2"><p>Two</p><p>paragraphs</p></td><td rowspan="2"><img src="pic.png" alt="a picture" width="48"></td><td>x</td></tr>
<tr><td>y</td><td>z</td><td>w</td><td>v</td></tr>
</tbody>
<tfoot><tr><th>Total</th><td colspan="4">written first in no particular order</td></tr></tfoot>
</table>

Footnotes[^later] come out in the order they are referred to[^first], and each leads back[^later].

[^first]: The second one.
[^later]: The first one, referred to twice.
[^unused]: Never shown.
"#;
    let view = move |_: &mut App| {
        let resolve: ImageResolver = Rc::new(move |src: &str| (src == "pic.png").then_some(picture));
        col().w_full().h_full().bg(Theme::dark().layer).p(20.0).child(markdown_content("tables", source, MarkdownOptions { render_html: true, resolve_image_src: Some(resolve), ..MarkdownOptions::default() }))
    };
    rig.settle(&view);
    rig.shot(&format!("{dir}/tables.png"));
}
