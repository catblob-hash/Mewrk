//! Every test of this crate, in one binary: the disk runs full, and each test binary links the whole host.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use fluent::el::*;
use fluent::theme::{Color, BODY_SMALL};
use fluent::ui::{Clipboard, Input, Key, KeyPress, Rect, UiMemory};
use fluent::{Scene, TextCtx, Theme, Ui};
use mewrk_client::prelude::E;
use mewrk_client::{App, StartOptions};
use mewrk_ui_common::context_menu::*;
use mewrk_ui_common::markdown::{clear_text_selection, *};
use mewrk_ui_common::path_text::*;

#[path = "suite/math_packages.rs"]
mod math_packages;
#[path = "suite/markdown_parity.rs"]
mod markdown_parity;

// ---------------------------------------------------------------- the rig

#[derive(Default)]
struct TestClipboard(Option<String>);

impl Clipboard for TestClipboard {
    fn get(&mut self) -> Option<String> {
        self.0.clone()
    }
    fn set(&mut self, text: &str) {
        self.0 = Some(text.to_owned());
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("mewrk-ui-common-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Rig {
    app: App,
    scene: Scene,
    text: TextCtx,
    mem: UiMemory,
    input: Input,
    clipboard: TestClipboard,
    size: (f32, f32),
    scale: f32,
    theme: Theme,
    time: f32,
    animating: bool,
    /// Something asked to be painted again with nothing else run ([`Ui::repaint_after`]).
    repainting: bool,
    wake_in: Option<f32>,
    cursor: fluent::ui::CursorKind,
    renderer: Option<fluent::gpu::Renderer>,
    _data: TempDir,
}

impl Rig {
    fn new() -> Self {
        static LANGUAGE: std::sync::Once = std::sync::Once::new();
        // SAFETY: the first thing any test does, before a host thread exists to read the environment.
        LANGUAGE.call_once(|| unsafe { std::env::set_var("MEWRK_LANG", "en") });
        let data = TempDir::new();
        let app = App::start(StartOptions {
            identifier: "dev.mewrk.tests".to_owned(),
            version: "0.0.0".to_owned(),
            data_dir: data.0.clone(),
            resource_dir: None,
            wake: Arc::new(|| {}),
            exit: Arc::new(|_| {}),
            show_window: Arc::new(|| {}),
        })
        .map_err(|e| format!("{e:?}"))
        .expect("the host starts");
        Self { app, scene: Scene::default(), text: TextCtx::new(1.0), mem: UiMemory::default(), input: Input::default(), clipboard: TestClipboard::default(), size: (800.0, 600.0), scale: 1.0, theme: Theme::dark(), time: 1.0, animating: false, repainting: false, wake_in: None, cursor: Default::default(), renderer: None, _data: data }
    }

    fn frame(&mut self, view: &dyn Fn(&mut App) -> E) {
        self.app.tick();
        self.scene.clear();
        set_window_size(self.size.0, self.size.1);
        set_theme(self.theme);
        self.time += 0.016;
        let root = view(&mut self.app);
        let mut ui = Ui::new(&mut self.scene, &mut self.text, self.theme, &self.input, &mut self.mem, &mut self.clipboard, self.scale, self.size, 0.016, self.time);
        render(root, &mut self.app, &mut ui);
        self.animating = ui.animating;
        self.repainting = ui.paint_wake_in.is_some();
        self.wake_in = ui.wake_in;
        self.cursor = ui.cursor;
        self.input.end_frame();
    }

    fn settle(&mut self, view: &dyn Fn(&mut App) -> E) {
        for _ in 0..4 {
            self.frame(view);
        }
    }

    fn press(&mut self, view: &dyn Fn(&mut App) -> E, x: f32, y: f32) {
        self.input.pointer = Some((x, y));
        self.frame(view);
        self.input.pressed = true;
        self.input.down = true;
        self.frame(view);
    }

    fn release(&mut self, view: &dyn Fn(&mut App) -> E, x: f32, y: f32) {
        self.input.pointer = Some((x, y));
        self.input.down = false;
        self.input.released = true;
        self.frame(view);
    }

    fn click(&mut self, view: &dyn Fn(&mut App) -> E, x: f32, y: f32) {
        self.press(view, x, y);
        self.release(view, x, y);
    }

    fn drag(&mut self, view: &dyn Fn(&mut App) -> E, from: (f32, f32), to: (f32, f32)) {
        self.press(view, from.0, from.1);
        self.input.pointer = Some(((from.0 + to.0) * 0.5, (from.1 + to.1) * 0.5));
        self.frame(view);
        self.input.pointer = Some(to);
        self.frame(view);
        self.release(view, to.0, to.1);
    }

    fn key(&mut self, view: &dyn Fn(&mut App) -> E, key: Key, cmd: bool) {
        self.input.keys.push(KeyPress { key, shift: false, cmd, alt: false, ctrl: false });
        self.frame(view);
    }

    fn copy(&mut self, view: &dyn Fn(&mut App) -> E) -> Option<String> {
        self.clipboard.0 = None;
        self.key(view, Key::Char('c'), true);
        self.clipboard.0.clone()
    }

    /// What the last frame drew, as a picture.
    fn shot(&mut self, path: &str) {
        use fluent::gpu::{Gpu, Renderer};
        const FORMAT: wgpu::TextureFormat = fluent::gpu::SURFACE_FORMAT;
        let renderer = self.renderer.get_or_insert_with(|| Renderer::new(Arc::new(Gpu::new(wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle()), None).unwrap())));
        let gpu = renderer.gpu.clone();
        self.text.flush_atlases(renderer);
        let size = ((self.size.0 * self.scale) as u32, (self.size.1 * self.scale) as u32);
        let target = gpu.device.create_texture(&wgpu::TextureDescriptor { label: Some("shot"), size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 }, mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2, format: FORMAT, usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC, view_formats: &[] });
        let view = target.create_view(&Default::default());
        renderer.render(&self.scene, &self.theme, &view, size);
        let row = size.0 * 4;
        let aligned = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor { label: None, size: (aligned * size.1) as u64, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &target, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo { buffer: &buffer, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(aligned), rows_per_image: None } },
            wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
        );
        gpu.queue.submit([encoder.finish()]);
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        gpu.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let data = slice.get_mapped_range().expect("mapped");
        let mut rgba = Vec::with_capacity((row * size.1) as usize);
        for y in 0..size.1 {
            let line = &data[(y * aligned) as usize..(y * aligned + row) as usize];
            match FORMAT {
                wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb => rgba.extend(line.chunks(4).flat_map(|p| [p[2], p[1], p[0], 255])),
                _ => rgba.extend(line.chunks(4).flat_map(|p| [p[0], p[1], p[2], 255])),
            }
        }
        image::save_buffer(path, &rgba, size.0, size.1, image::ColorType::Rgba8).unwrap();
    }

    /// Glyphs of the base layer that are at less than full opacity.
    fn faded_glyphs(&self) -> usize {
        self.scene.layers[0].glyphs.iter().filter(|g| g.color.a < 250).count()
    }
}

const MD_KEY: &str = "message";

fn md_view(content: &'static str, options: MarkdownOptions) -> impl Fn(&mut App) -> E {
    move |_| col().w_full().p(10.0).child(markdown_content(MD_KEY, content, options.clone()))
}

/// A point in the message, given a point relative to it: the view above pads by 10.
fn at(p: (f32, f32)) -> (f32, f32) {
    (p.0 + 10.0, p.1 + 10.0)
}

fn opts() -> ParseOptions {
    ParseOptions { linkify_paths: false, render_html: false, document_links: false, single_dollar_math: true }
}

fn plain(inlines: &[Inline]) -> String {
    inlines
        .iter()
        .map(|i| match i {
            Inline::Text { text, .. } => text.clone(),
            Inline::Math { source, .. } => format!("${source}$"),
            Inline::Image { alt, .. } => alt.clone(),
            Inline::Ruby { base, reading, .. } => format!("{base}({reading})"),
            Inline::Break => "\n".to_owned(),
            Inline::Anchor(_) => String::new(),
        })
        .collect()
}

fn para(block: &Block) -> &[Inline] {
    match block {
        Block::Paragraph { inlines, .. } => inlines,
        other => panic!("not a paragraph: {other:?}"),
    }
}

fn first_para(source: &str, options: ParseOptions) -> Vec<Inline> {
    let blocks = parse_markdown(source, options);
    para(&blocks[0]).to_vec()
}

/// Everything a parse says as text, for looking at what a tree keeps without caring how it is nested.
fn all_text(blocks: &[Block], out: &mut String) {
    for block in blocks {
        match block {
            Block::Paragraph { inlines, .. } | Block::Heading { inlines, .. } => {
                out.push_str(&plain(inlines));
                out.push('\n');
            }
            Block::Code { code, .. } => {
                out.push_str(code);
                out.push('\n');
            }
            Block::Math { source, .. } => {
                out.push_str(source);
                out.push('\n');
            }
            Block::Quote(inner) | Block::Aligned { blocks: inner, .. } | Block::Indented { blocks: inner, .. } | Block::Footnote { blocks: inner, .. } => all_text(inner, out),
            Block::List { items, .. } => {
                for item in items {
                    all_text(&item.blocks, out);
                }
            }
            Block::Table { rows, .. } => {
                for row in rows {
                    for cell in &row.cells {
                        out.push_str(&plain(&cell.inlines));
                        out.push('\t');
                    }
                    out.push('\n');
                }
            }
            Block::Details { summary, blocks, .. } => {
                out.push_str(&plain(summary));
                out.push('\n');
                all_text(blocks, out);
            }
            Block::Rule | Block::Anchor(_) => {}
        }
    }
}

// ---------------------------------------------------------------- math delimiters (MarkdownContent.test.tsx)

#[test]
fn escapes_only_the_single_dollars_that_do_not_delimit_math() {
    assert_eq!(normalize_math_delimiters("$4.50 vs $4.75"), "\\$4.50 vs \\$4.75");
    assert_eq!(normalize_math_delimiters("from $5 to $x$"), "from \\$5 to $x$");
    assert_eq!(normalize_math_delimiters("$a$ and \\$b `$c$`"), "$a$ and \\$b `$c$`");
    // A formula does not run on past a blank line.
    assert_eq!(normalize_math_delimiters("$a\n\nb$"), "\\$a\n\nb\\$");
}

#[test]
fn does_not_rewrite_math_like_delimiters_inside_code() {
    let source = "文本 \\(x\\) `\\(inline\\)`\n\n```txt\n\\[block\\]\n```\n之后 \\[y\\]";
    assert_eq!(normalize_math_delimiters(source), "文本 $x$ `\\(inline\\)`\n\n```txt\n\\[block\\]\n```\n之后 \n$$\ny\n$$\n");
}

#[test]
fn renders_inline_and_display_formulas_with_dollar_and_latex_delimiters() {
    let blocks = parse_markdown("行内 $E=mc^2$ 与 \\(a^2+b^2=c^2\\)。\n\n$$\\int_0^1 x^2\\,dx=\\frac{1}{3}$$\n\n\\[\\sum_{i=1}^{n} i=\\frac{n(n+1)}{2}\\]", opts());
    let math: Vec<&str> = para(&blocks[0]).iter().filter_map(|i| if let Inline::Math { source, .. } = i { Some(source.as_str()) } else { None }).collect();
    assert_eq!(math, ["E=mc^2", "a^2+b^2=c^2"]);
    let display: Vec<&str> = blocks.iter().filter_map(|b| if let Block::Math { source, .. } = b { Some(source.as_str()) } else { None }).collect();
    assert_eq!(display, ["\\int_0^1 x^2\\,dx=\\frac{1}{3}", "\\sum_{i=1}^{n} i=\\frac{n(n+1)}{2}"]);
}

#[test]
fn a_fence_that_says_math_is_display_math() {
    let blocks = parse_markdown("```math\n\\frac{a}{b}\n```", opts());
    assert!(matches!(&blocks[0], Block::Math { source, .. } if source == "\\frac{a}{b}"));
}

#[test]
fn keeps_prices_as_text_while_dollars_around_a_formula_still_make_math() {
    let inlines = first_para("The mug is $4.50 vs $4.75 in town, $5-$10 online, and $x^2$ stays math.", opts());
    let math: Vec<_> = inlines.iter().filter(|i| matches!(i, Inline::Math { .. })).collect();
    assert_eq!(math.len(), 1);
    assert!(matches!(math[0], Inline::Math { source, .. } if source == "x^2"));
    assert!(plain(&inlines).starts_with("The mug is $4.50 vs $4.75 in town, $5-$10 online, and"));
}

#[test]
fn a_formula_still_arriving_is_shown_as_far_as_it_has_come() {
    // Display math runs to the end of the text until its closing line arrives.
    let blocks = parse_markdown("$$\n\\frac{a}{", opts());
    assert!(matches!(&blocks[0], Block::Math { source, .. } if source == "\\frac{a}{"));
}

#[test]
fn single_dollar_math_can_be_turned_off() {
    let options = ParseOptions { single_dollar_math: false, ..opts() };
    let inlines = first_para("cost $x$ here", options);
    assert_eq!(plain(&inlines), "cost $x$ here");
    assert!(inlines.iter().all(|i| !matches!(i, Inline::Math { .. })));
    // Display math does not depend on it.
    assert!(matches!(&parse_markdown("$$x$$", options)[0], Block::Math { .. }));
}

// ---------------------------------------------------------------- typeset math

/// Formulas as replies write them, for everything that should work on any of them.
const REAL_FORMULAS: &[&str] = &[
    r"E=mc^2",
    r"e^{i\pi}+1=0",
    r"x=\frac{-b\pm\sqrt{b^2-4ac}}{2a}",
    r"\int_{-\infty}^{\infty} e^{-x^2}\,dx=\sqrt{\pi}",
    r"\sum_{i=1}^{n} i=\frac{n(n+1)}{2}",
    r"\prod_{p\ \text{prime}}\frac{1}{1-p^{-s}}=\zeta(s)",
    r"\lim_{n\to\infty}\left(1+\frac{1}{n}\right)^{n}=e",
    r"\max_{x\in\mathcal{X}} f(x)",
    r"\sqrt[3]{x+1}+\sqrt{\frac{a}{b}}",
    r"\binom{n}{k}=\frac{n!}{k!(n-k)!},\ \dbinom{n}{2},\ \tbinom{n}{2},\ \dfrac{a}{b},\ \tfrac{a}{b}",
    r"\hat{x}\bar{x}\vec{x}\tilde{x}\dot{x}\ddot{x}\overline{abc}\underline{abc}\widehat{abc}\widetilde{abc}",
    r"\mathbf{A}\mathrm{d}\mathbb{R}\mathcal{L}\mathsf{S}\mathtt{m}\mathfrak{g}\boldsymbol{\alpha}\text{ if }x",
    r"\bigl(x\bigr)\Bigl[x\Bigr]\biggl\{x\biggr\}\Biggl|x\Biggr|\big\langle x\big\rangle",
    r"a\,b\:c\;d\!e\quad f\qquad g\hspace{1em}h",
    r"\begin{pmatrix}a&b\\c&d\end{pmatrix}\begin{bmatrix}1&0\\0&1\end{bmatrix}\begin{vmatrix}a\\b\end{vmatrix}\begin{matrix}a\end{matrix}",
    r"f(x)=\begin{cases}x^2&\text{if }x\ge 0\\-x&\text{otherwise}\end{cases}",
    r"\begin{aligned}(a+b)^2&=a^2+2ab+b^2\\&=a^2+b^2+2ab\end{aligned}",
    r"\begin{align}a&=b\\c&=d\end{align}",
    r"\begin{gather}a\\b\end{gather}",
    r"\begin{array}{l|c|r}a&b&c\\\hline 1&2&3\end{array}",
    r"\alpha\beta\gamma\Gamma\Delta\Omega\varepsilon\varphi\infty\partial\nabla\forall\exists\leq\geq\neq\approx\equiv\in\subset\cup\cap\to\Rightarrow\iff",
    r"\operatorname{argmax}_\theta L(\theta)+\operatorname*{argmin}_\theta L(\theta)",
    r"\underbrace{a+\cdots+z}_{26}\overbrace{x+y}^{\text{sum}}",
    r"\not=\not\in\not\alpha",
    r"\textcolor{red}{x}+{\color{blue}y}",
    r"\frac{\partial \mathcal{L}}{\partial \theta}=\frac{1}{n}\sum_{i=1}^{n}\left(y_i-\hat{y}_i\right)\frac{\partial \hat{y}_i}{\partial \theta}",
    r"x'+y''_n+f'(x)^2",
    r"\sum_{\substack{0\le i\le m\\0<j<n}}P(i,j)",
    r"\left.\frac{df}{dx}\right|_{x=0},\ \left\{x\,\middle|\,x>0\right\}",
    r"\ce{2H2 + O2 -> 2H2O}\quad\ket{\psi}\bra{\phi}\braket{a|b}\abs{x}\norm{v}\dv{f}{x}\pdv{f}{x}",
    "中文\\text{混排 text}x=1",
    "a=b\\\\c=d",
];

fn parsed(source: &str) -> String {
    probe::tex_tree(source).unwrap_or_else(|e| panic!("{source}: {e}"))
}

fn text_ctx() -> TextCtx {
    TextCtx::new(2.0)
}

/// One formula's run of this text.
fn run_of(text: &mut TextCtx, source: &str, display: bool, shown: &str) -> (f32, f32, f32) {
    let runs = probe::formula_runs(text, source, display, 16.0);
    runs.iter().find(|r| r.0 == shown).map(|r| (r.1, r.2, r.3)).unwrap_or_else(|| panic!("no {shown:?} in {runs:?}"))
}

#[test]
fn what_replies_write_reads_without_an_unknown_command() {
    for source in REAL_FORMULAS {
        let tree = parsed(source);
        assert!(!tree.contains("Undefined"), "{source}: {tree}");
    }
}

#[test]
fn a_parse_error_says_what_mathjax_says() {
    for (source, message) in [
        (r"\frac{a}{", "Missing close brace"),
        (r"\frac{a}", r"Missing argument for \frac"),
        (r"\frac", r"Missing argument for \frac"),
        (r"\sqrt", r"Missing argument for \sqrt"),
        ("{x", "Extra open brace or missing close brace"),
        ("x}", "Extra close brace or missing open brace"),
        ("x^2^3", "Double exponent: use braces to clarify"),
        ("x_1_2", "Double subscripts: use braces to clarify"),
        ("x^", "Missing superscript or subscript argument"),
        ("x^^2", "Missing open brace for superscript"),
        ("x^2'", "Prime causes double exponent: use braces to clarify"),
        ("a&b", "Misplaced &"),
        ("#", "You can't use 'macro parameter character #' in math mode"),
        (r"\left(x", r"Extra \left or missing \right"),
        (r"x\right)", r"Missing \left or extra \right"),
        (r"\middle|", r"Extra \middle"),
        (r"\left?x\right)", r"Missing or unrecognized delimiter for \left"),
        (r"\begin{pmatrix}a", r"Missing \end{pmatrix}"),
        (r"\begin{pmatrix}a\end{bmatrix}", r"\begin{pmatrix} ended with \end{bmatrix}"),
        (r"a\end{pmatrix}", r"Missing \begin{pmatrix} or extra \end{pmatrix}"),
        (r"\begin{nothing}a\end{nothing}", "Unknown environment 'nothing'"),
        (r"x\limits", r"\limits is allowed only on operators"),
        (r"a\over b\over c", r"Ambiguous use of \over"),
        (r"\text{max_value}", "'_' allowed only in math mode"),
        (r"\text{a&b}", "Misplaced &"),
        (r"\def\a{\a}\a", "MathJax maximum macro substitution count exceeded; is there a recursive macro call?"),
    ] {
        assert_eq!(probe::tex_tree(source).err().as_deref(), Some(message), "{source}");
    }
}

#[test]
fn an_unknown_command_does_not_take_the_formula_down() {
    let mut text = text_ctx();
    let tree = parsed(r"\foo{x}+\frac{a}{b}");
    assert!(tree.contains(r#"Undefined("\\foo")"#), "{tree}");
    assert!(tree.contains("Frac"), "{tree}");
    // `noundefined`: its own name, in red, whatever the theme, while the rest stays the theme's.
    let runs = probe::formula_runs(&mut text, r"\foo{x}+1", false, 16.0);
    let red = runs.iter().find(|r| r.0 == r"\foo").expect("its name shows");
    assert_eq!(red.4, Some(Color::rgb(0xFF0000)));
    assert!(runs.iter().filter(|r| r.0 != r"\foo").all(|r| r.4.is_none()), "{runs:?}");
}

#[test]
fn reads_tex_the_way_mathjax_does() {
    // Numbers stay whole, with the `{,}` a thousands separator takes.
    assert!(parsed("3.14+1{,}000").contains(r#"text: "3.14""#));
    assert!(parsed("3.14+1{,}000").contains(r#"text: "1,000""#));
    // A word in a font is one run; in math italics each letter stands alone.
    assert!(parsed(r"\mathrm{abc}").contains(r#"text: "abc""#));
    assert!(parsed("abc").contains(r#"text: "a""#));
    // An argument can be one token, however it looks.
    assert_eq!(parsed(r"\frac12").matches("Sym").count(), 2);
    assert!(parsed(r"x^\frac12").contains("Frac"));
    // `\not` takes the negated symbol where there is one, and draws a slash where there is not.
    assert!(parsed(r"\not=").contains('≠'));
    assert!(parsed(r"\not\in").contains('∉'));
    assert!(parsed(r"\not\alpha").contains("Not("));
    // `\color` is a switch to the end of its group, as the `color` package has it.
    let tree = parsed(r"{\color{red}x}y");
    assert!(tree.contains("SetColor"), "{tree}");
    assert_eq!(probe::formula_runs(&mut text_ctx(), r"{\color{red}x y}z", false, 16.0).iter().filter(|r| r.4 == Some(Color::rgb(0xFF0000))).count(), 2);
    assert_eq!(probe::formula_runs(&mut text_ctx(), r"\color[rgb]{0,1,0}x", false, 16.0)[0].4, Some(Color::rgb(0x00FF00)));
    assert_eq!(probe::formula_runs(&mut text_ctx(), r"\color{#F00}x", false, 16.0)[0].4, Some(Color::rgb(0xFF0000)));
    assert_eq!(probe::formula_runs(&mut text_ctx(), r"\color{Apricot}x", false, 16.0)[0].4, Some(Color::rgb(0xF8B878)));
    // A macro of a formula's own, with arguments, and text with math in it.
    assert!(parsed(r"\newcommand{\R}[1]{\mathbb{#1}}\R{R}").contains("DoubleStruck"));
    assert!(parsed(r"\text{for $x_i$ here}").contains("Scripts"));
    // `\dots` looks at what comes next.
    assert!(parsed(r"a+\dots+b").contains('⋯'));
    assert!(parsed(r"a,\dots,b").contains('…'));
    // `\ce`'s charges and counts.
    let water = parsed(r"\ce{H2O}");
    assert!(water.contains(r#"text: "H""#) && water.contains("Scripts") && water.contains(r#"text: "2""#), "{water}");
    // Its parser takes the digit for a subscript and only the sign for a charge: `Ca^2+` is the charge.
    assert_eq!(parsed(r"\ce{Ca2+}").matches("Scripts").count(), 2);
    assert_eq!(parsed(r"\ce{Ca^2+}").matches("Scripts").count(), 1);
    assert!(parsed(r"\ce{Na+ + Cl-}").contains("Bin"));
    assert!(parsed(r"\ce{A ->[x][y] B}").contains("Arrow"));
    // A comment and a space between a command and its argument are nothing.
    assert_eq!(parsed("x % not this\n+1"), parsed("x +1"));
    assert_eq!(parsed(r"\frac {a} {b}"), parsed(r"\frac{a}{b}"));
    // `\limits` says where the scripts go.
    assert!(parsed(r"\int\limits_0^1").contains("limits: Some(true)"));
    assert!(parsed(r"\sum\nolimits_0^1").contains("limits: Some(false)"));
    // An `align`'s right side starts as if something stood before it.
    assert!(parsed(r"\begin{aligned}a&=b\end{aligned}").contains("[Group([]), Sym"));
    // A row of nothing after the last `\\` is no row.
    let rows = parsed(r"\begin{matrix}a\\b\\\end{matrix}");
    assert_eq!(rows.matches("Row {").count(), 2, "{rows}");
    assert_eq!(parsed(r"\begin{array}{|c|}\hline a\\\hline\end{array}").matches("rule_above: true").count(), 1);
    assert!(parsed(r"\begin{array}{|c|}\hline a\\\hline\end{array}").contains("rule_below: true"));
    assert!(parsed(r"\begin{array}{*{3}{c}}a\end{array}").contains("columns: ['c', 'c', 'c']"));
}

#[test]
fn a_fraction_stands_taller_than_its_parts_with_its_bar_on_the_axis() {
    let mut text = text_ctx();
    let (_, a_h, a_d) = probe::formula_metrics(&mut text, "a", false, 16.0).unwrap();
    let (w, h, d) = probe::formula_metrics(&mut text, r"\frac{a}{b}", false, 16.0).unwrap();
    assert!(h + d > (a_h + a_d) * 1.6, "{h}+{d} against {a_h}+{a_d}");
    let (eq_w, ..) = probe::formula_metrics(&mut text, "a", true, 16.0).unwrap();
    assert!(w > eq_w);
    let rules = probe::formula_rules(&mut text, r"\frac{a}{b}", false, 16.0);
    assert_eq!(rules.len(), 1);
    let bar = rules[0];
    assert!(bar[1] < 0.0, "the bar is above the baseline, at the axis: {bar:?}");
    let num = run_of(&mut text, r"\frac{a}{b}", false, "\u{1D44E}");
    let den = run_of(&mut text, r"\frac{a}{b}", false, "\u{1D44F}");
    assert!(num.1 < bar[1], "numerator above the bar");
    assert!(den.1 > bar[1], "denominator below the bar");
    assert!(num.2 < 16.0 && den.2 < 16.0, "a text fraction's parts are script size");
    // In display the parts stay at text size, and `\dfrac` and `\tfrac` say so anywhere.
    assert_eq!(run_of(&mut text, r"\frac{a}{b}", true, "\u{1D44E}").2, num.2 / 0.7);
    assert_eq!(run_of(&mut text, r"\dfrac{a}{b}", false, "\u{1D44E}").2, num.2 / 0.7);
    assert_eq!(run_of(&mut text, r"\tfrac{a}{b}", true, "\u{1D44E}").2, num.2);
    // `\binom` has no bar and has brackets.
    assert!(probe::formula_rules(&mut text, r"\binom{a}{b}", false, 16.0).is_empty());
    assert!(probe::formula_metrics(&mut text, r"\binom{a}{b}", false, 16.0).unwrap().0 > w);
}

#[test]
fn scripts_sit_off_the_baseline_and_shrink_until_they_stop() {
    let mut text = text_ctx();
    let sup = run_of(&mut text, "x^2", false, "2");
    let sub = run_of(&mut text, "x_2", false, "2");
    assert!(sup.1 < 0.0, "a superscript's baseline is above the line: {sup:?}");
    assert!(sub.1 > 0.0, "a subscript's is below it: {sub:?}");
    let (x, ..) = run_of(&mut text, "x^2", false, "\u{1D465}");
    assert!(sup.0 > x, "and past the letter");
    // Both at once: the same place across, with room between them.
    let both_up = run_of(&mut text, "x^2_3", false, "2");
    let both_down = run_of(&mut text, "x^2_3", false, "3");
    assert!(both_up.1 < 0.0 && both_down.1 > 0.0);
    assert!(both_down.1 - both_up.1 > sup.2 * 0.5);
    // Text, script, scriptscript, and no smaller.
    let runs = probe::formula_runs(&mut text, "a^{b^{c^{d^{e}}}}", false, 16.0);
    let sizes: Vec<f32> = runs.iter().map(|r| r.3).collect();
    assert_eq!(sizes.len(), 5);
    assert!((sizes[1] / sizes[0] - 0.7).abs() < 0.01, "{sizes:?}");
    assert!((sizes[2] / sizes[0] - 0.5).abs() < 0.01, "{sizes:?}");
    assert_eq!(sizes[2], sizes[3]);
    assert_eq!(sizes[3], sizes[4]);
    assert!(runs.windows(2).all(|r| r[1].2 < r[0].2), "each is higher than the last: {runs:?}");
    // A prime is a script, and an exponent after one shares its line of work.
    assert!(run_of(&mut text, "x'", false, "′").1 < 0.0);
    assert!(run_of(&mut text, "x'^2", false, "2").0 > run_of(&mut text, "x'^2", false, "′").0);
    // An empty base still holds a script up.
    assert!(run_of(&mut text, "{}^{14}C", false, "14").1 < 0.0);
}

#[test]
fn a_large_operator_takes_its_limits_over_and_under_in_display_and_beside_it_in_text() {
    let mut text = text_ctx();
    let (.., y_up) = (0, run_of(&mut text, r"\sum_{i}^{n}", true, "\u{1D45B}").1);
    let display_up = run_of(&mut text, r"\sum_{i}^{n}", true, "\u{1D45B}");
    let display_down = run_of(&mut text, r"\sum_{i}^{n}", true, "\u{1D456}");
    let sigma_d = run_of(&mut text, r"\sum_{i}^{n}", true, "∑");
    let text_up = run_of(&mut text, r"\sum_{i}^{n}", false, "\u{1D45B}");
    let sigma_t = run_of(&mut text, r"\sum_{i}^{n}", false, "∑");
    assert_eq!(y_up, display_up.1);
    assert!(sigma_d.2 > sigma_t.2, "a display operator is larger: {sigma_d:?} {sigma_t:?}");
    assert!((display_up.0 - display_down.0).abs() < 8.0, "over and under are centred on one another");
    assert!(display_down.1 > 0.0 && display_up.1 < text_up.1, "{display_up:?} {text_up:?}");
    assert!(text_up.0 > sigma_t.0 + 4.0, "in text the limit comes after the operator");
    // `\int` keeps them beside it even in display, unless told otherwise; `\lim` and `\max` take them under.
    assert!(run_of(&mut text, r"\int_0^1", true, "1").0 > run_of(&mut text, r"\int_0^1", true, "∫").0 + 4.0);
    assert!((run_of(&mut text, r"\int\limits_0^1", true, "1").0 - run_of(&mut text, r"\int\limits_0^1", true, "0").0).abs() < 8.0);
    assert!(run_of(&mut text, r"\lim_{x}", true, "\u{1D465}").1 > 0.0);
    assert!(run_of(&mut text, r"\lim_{x}", true, "\u{1D465}").0 < run_of(&mut text, r"\lim_{x}", false, "\u{1D465}").0);
    assert!(run_of(&mut text, r"\sin^2", true, "2").1 < 0.0 && run_of(&mut text, r"\sin^2", true, "2").0 > 8.0, "`\\sin` takes no limits");
    assert!(run_of(&mut text, r"\operatorname*{f}_x", true, "\u{1D465}").1 > 0.0);
    assert!(run_of(&mut text, r"\operatorname{f}_x", true, "\u{1D465}").0 > 2.0);
}

#[test]
fn spacing_goes_by_the_class_of_what_is_on_either_side() {
    let mut text = text_ctx();
    let mut w = |source: &str| probe::formula_metrics(&mut text, source, false, 16.0).unwrap().0;
    assert!(w("a+b") > w("a{+}b") + 4.0, "a binary operator has room around it");
    assert!(w("a=b") > w("a+b") - 1.0, "and a relation a little more");
    // A minus at the start, or after a relation, is a sign.
    assert!(w("-a") < w("b-a") - w("b") + 0.5);
    assert!(w("a=-b") < w("a=b") + w("-") + 1.0);
    // `\,` and friends are spaces, and negative ones take away.
    assert!(w(r"a\,b") > w("ab"));
    assert!(w(r"a\!b") < w("ab"));
    assert!(w(r"a\quad b") > w(r"a\;b"));
    assert!(w(r"a\hspace{2em}b") > w(r"a\quad b"));
    assert!(w(r"\sin x") > w(r"\sin{}x") - 0.5);
}

#[test]
fn delimiters_stretch_to_what_they_hold() {
    let mut text = text_ctx();
    let mut metrics = |source: &str| probe::formula_metrics(&mut text, source, true, 16.0).unwrap();
    let plain = metrics(r"\left(x\right)");
    let tall = metrics(r"\left(\frac{\frac{a}{b}}{\frac{c}{d}}\right)");
    assert!(tall.1 + tall.2 > (plain.1 + plain.2) * 2.0, "{tall:?} {plain:?}");
    let big = |m: (f32, f32, f32)| m.1 + m.2;
    assert!(big(metrics(r"\Bigg(")) > big(metrics(r"\bigg(")));
    assert!(big(metrics(r"\bigg(")) > big(metrics(r"\Big(")));
    assert!(big(metrics(r"\Big(")) > big(metrics(r"(")));
    // An invisible delimiter takes no ink and leaves the other side to do its job.
    let half = metrics(r"\left.\frac{a}{b}\right|");
    assert!(half.0 < metrics(r"\left|\frac{a}{b}\right|").0);
    // A tall delimiter is drawn, one of the font's size is not.
    assert!(probe::formula_paths(&mut text_ctx(), r"\left(x\right)", true, 16.0).is_empty());
    assert_eq!(probe::formula_paths(&mut text_ctx(), r"\left(\frac{a}{b}\right)", true, 16.0).len(), 2);
    assert!(!probe::formula_rules(&mut text_ctx(), r"\left[\frac{a}{b}\right]", true, 16.0).is_empty());
    // Even a height no atlas could hold still draws something, and fits.
    let huge = format!("\\left({}\\right)", vec!["a"; 400].join("\\\\"));
    let huge = format!("\\begin{{matrix}}{huge}\\end{{matrix}}");
    let _ = probe::formula_metrics(&mut text_ctx(), &huge, true, 16.0);
    let tall_matrix = format!("\\left(\\begin{{matrix}}{}\\end{{matrix}}\\right)", vec!["a"; 400].join("\\\\"));
    let mut text = text_ctx();
    assert!(probe::formula_metrics(&mut text, &tall_matrix, true, 16.0).unwrap().1 > 1000.0);
    assert!(probe::formula_paths(&mut text, &tall_matrix, true, 16.0).is_empty(), "it falls back to rules");
    assert!(probe::formula_rules(&mut text, &tall_matrix, true, 16.0).len() >= 6);
}

#[test]
fn roots_accents_and_rules_clear_what_is_under_them() {
    let mut text = text_ctx();
    let x = probe::formula_metrics(&mut text, "x", true, 16.0).unwrap();
    for source in [r"\sqrt{x}", r"\hat{x}", r"\vec{x}", r"\overline{x}", r"\widehat{x}", r"\dot{x}", r"\overbrace{x}", r"\overrightarrow{x}"] {
        let m = probe::formula_metrics(&mut text, source, true, 16.0).unwrap();
        assert!(m.1 > x.1 + 1.0, "{source}: {m:?} against {x:?}");
    }
    for source in [r"\underline{x}", r"\underbrace{x}", r"\underrightarrow{x}"] {
        let m = probe::formula_metrics(&mut text, source, true, 16.0).unwrap();
        assert!(m.2 > x.2 + 1.0, "{source}: {m:?} against {x:?}");
    }
    // A taller body lifts the mark with it.
    assert!(probe::formula_metrics(&mut text, r"\hat{d}", true, 16.0).unwrap().1 > probe::formula_metrics(&mut text, r"\hat{a}", true, 16.0).unwrap().1);
    // An index shows at a smaller size, up and to the left of the root's body.
    let index = run_of(&mut text, r"\sqrt[3]{x}", true, "3");
    let body = run_of(&mut text, r"\sqrt[3]{x}", true, "\u{1D465}");
    assert!(index.2 < body.2 && index.0 < body.0 && index.1 < body.1, "{index:?} {body:?}");
    // A brace's script goes over it, not after it.
    let label = run_of(&mut text, r"\overbrace{xyz}^{n}", true, "\u{1D45B}");
    let first = run_of(&mut text, r"\overbrace{xyz}^{n}", true, "\u{1D465}");
    assert!(label.1 < first.1 - 8.0 && label.0 > first.0, "{label:?} {first:?}");
}

#[test]
fn an_environment_lines_its_columns_up() {
    let mut text = text_ctx();
    let aligned = r"\begin{aligned}aaaa&=b\\c&=dddd\end{aligned}";
    let runs = probe::formula_runs(&mut text, aligned, true, 16.0);
    let equals: Vec<_> = runs.iter().filter(|r| r.0 == "=").collect();
    assert_eq!(equals.len(), 2);
    assert_eq!(equals[0].1, equals[1].1, "the equals signs stand over one another");
    assert!(equals[1].2 > equals[0].2 + 16.0, "on rows below one another");
    let (a_x, c_x) = (runs.iter().find(|r| r.0 == "\u{1D44E}").unwrap().1, runs.iter().find(|r| r.0 == "\u{1D450}").unwrap().1);
    assert!(c_x > a_x, "the left side is set flush right");
    // A matrix's columns are centred, its rows at one baseline, and its brackets about as tall as it.
    let matrix = probe::formula_metrics(&mut text, r"\begin{pmatrix}a&b\\c&d\end{pmatrix}", true, 16.0).unwrap();
    let bare = probe::formula_metrics(&mut text, r"\begin{matrix}a&b\\c&d\end{matrix}", true, 16.0).unwrap();
    assert!(matrix.0 > bare.0 + 4.0 && matrix.1 + matrix.2 >= bare.1 + bare.2);
    let (a, b) = (run_of(&mut text, r"\begin{matrix}a&b\\c&d\end{matrix}", true, "\u{1D44E}"), run_of(&mut text, r"\begin{matrix}a&b\\c&d\end{matrix}", true, "\u{1D44F}"));
    assert_eq!(a.1, b.1);
    assert!(b.0 > a.0 + 8.0);
    // `cases` has a brace on one side only; `array` takes its alignment and its rules from its spec.
    assert!(!probe::formula_paths(&mut text, r"\begin{cases}a\\b\end{cases}", true, 16.0).is_empty());
    assert_eq!(probe::formula_rules(&mut text, r"\begin{array}{c|c}a&b\\\hline c&d\end{array}", true, 16.0).len(), 2);
    let spec = r"\begin{array}{lr}aaaa&b\\c&dddd\end{array}";
    let (a, c) = (run_of(&mut text, spec, true, "\u{1D44E}"), run_of(&mut text, spec, true, "\u{1D450}"));
    assert_eq!(a.0, c.0, "`l` keeps them flush left");
    // Set on the axis: a one-row matrix is about as high above the baseline as a fraction's middle.
    let m = probe::formula_metrics(&mut text, r"\begin{matrix}a\\b\\c\end{matrix}", true, 16.0).unwrap();
    assert!((m.1 - m.2).abs() < m.1 + m.2, "{m:?}");
}

#[test]
fn a_formula_scales_with_the_text_and_with_the_window() {
    let mut text = text_ctx();
    let source = r"\frac{a+b}{\sqrt{c}}+\sum_{i=1}^n x_i";
    let small = probe::formula_metrics(&mut text, source, true, 13.0).unwrap();
    let large = probe::formula_metrics(&mut text, source, true, 26.0).unwrap();
    assert!((large.0 / small.0 - 2.0).abs() < 0.08, "{small:?} {large:?}");
    assert!((large.1 / small.1 - 2.0).abs() < 0.15, "{small:?} {large:?}");
    // In logical pixels a zoomed window comes to about the same, up to whole pixels.
    let mut one = TextCtx::new(1.0);
    let mut two = TextCtx::new(2.0);
    let at_one = probe::formula_metrics(&mut one, source, true, 16.0).unwrap();
    let at_two = probe::formula_metrics(&mut two, source, true, 16.0).unwrap();
    assert!((at_one.0 - at_two.0).abs() < 4.0 && (at_one.1 - at_two.1).abs() < 4.0, "{at_one:?} {at_two:?}");
    // A formula asks for no colour of its own, so one set once follows the theme.
    assert!(probe::formula_runs(&mut text, source, true, 16.0).iter().all(|r| r.4.is_none()));
}

#[test]
fn every_prefix_of_a_real_formula_parses_or_fails_without_a_panic() {
    let mut text = TextCtx::new(1.0);
    let mut typeset_all = |source: &str| {
        let _ = probe::formula_metrics(&mut text, source, true, 13.0);
        let _ = probe::formula_metrics(&mut text, source, false, 13.0);
    };
    for source in REAL_FORMULAS {
        for (at, _) in source.char_indices() {
            typeset_all(&source[..at]);
            // And with a character gone, which makes for other ways of being wrong.
            let next = source[at..].chars().next().map_or(0, char::len_utf8);
            typeset_all(&format!("{}{}", &source[..at], &source[at + next..]));
        }
    }
    for nasty in [r"\", r"\\", "^", "_", "'", "&", "%", "$", "{", "}", "#", "~", "\u{0}", r"\left", r"\right", r"\begin", r"\end", r"\begin{", r"\begin{array}", r"\begin{array}{", r"\begin{array}{c", r"\text{", r"\text{$", r"\text{\\", r"\sqrt[", r"\sqrt[3", r"\color", r"\color{", r"\rule{", r"\rule{1em}{", r"\verb", r"\verb|", r"\unicode{", r"\unicode{xZZ}", r"\hspace{", r"\hspace{x}", r"\kern", r"\kern1", r"\def", r"\def\a", r"\newcommand", r"\newcommand{\a}", r"\newcommand{\a}[", r"\ce{", r"\ce{->[", r"\qty(", r"\dv", r"\root", r"\root 3", r"\not", r"\not\foo", r"\substack{", r"\xrightarrow[", r"\genfrac", r"\operatorname{", r"\tag{", r"\middle", r"\cancel{", r"\boxed", r"\rule{1000000em}{1000000em}", r"\hspace{100000000em}"] {
        typeset_all(nasty);
    }
}

#[test]
fn nesting_of_any_depth_ends_in_an_answer_rather_than_the_end_of_the_stack() {
    let run = || {
        let mut text = TextCtx::new(1.0);
        for piece in ["{", r"\frac{", r"\sqrt{", "x^{", r"\left(", r"\begin{matrix}", r"\hat{", r"\underbrace{", r"\text{", r"\text{\textbf{", r"\color{red}{", r"\mathbf{", r"\boxed{", r"\begin{cases}\frac{", r"\sum_{", r"\not", r"x^\not", r"\text{$"] {
            for depth in [38, 39, 40, 41, 2000] {
                let source = piece.repeat(depth);
                let _ = probe::formula_metrics(&mut text, &source, true, 13.0);
                let closed = format!("{source}x{}", "}".repeat(depth));
                let _ = probe::formula_metrics(&mut text, &closed, true, 13.0);
            }
        }
        // As deep as a formula may go, closed properly, with the most on the stack at each level.
        let deep = format!("{}x{}", r"\frac{\sqrt{".repeat(19), "}{y}}".repeat(19).replace("}{y}}", "}}{y}"));
        probe::formula_metrics(&mut text, &deep, true, 13.0).expect("this deep is allowed");
    };
    // Less than a test thread has, which is less than the window's: there is room to spare.
    std::thread::Builder::new().stack_size(1024 * 1024).spawn(run).unwrap().join().unwrap();
}

#[test]
fn a_recursive_macro_stops_on_its_own() {
    let mut text = TextCtx::new(1.0);
    for source in [r"\def\a{\a\a}\a", r"\newcommand{\a}{\a x}\a", r"\def\a#1{\a{#1#1}}\a{x}", r"\def\a{\a}x^\a", r"\def\a{\a}\not\a"] {
        assert!(probe::formula_metrics(&mut text, source, true, 13.0).is_err(), "{source}");
    }
    // A column spec that multiplies does not get to fill the memory.
    let spec = r"\begin{array}{*{99}{*{99}{*{99}{*{99}{c}}}}}a\end{array}";
    assert!(parsed(spec).matches('\'').count() <= 256 * 2 + 16);
}

#[test]
fn a_macro_as_a_script_gives_the_script_all_of_itself() {
    let mut text = text_ctx();
    // `\abs` is a macro: its bars are the exponent, not the next thing on the line.
    let bars: Vec<_> = probe::formula_runs(&mut text, r"x^\abs{y}", false, 16.0).into_iter().filter(|r| r.0 == "|").collect();
    assert_eq!(bars.len(), 2);
    assert!(bars.iter().all(|r| r.2 < 0.0 && r.3 < 16.0), "{bars:?}");
    assert!(run_of(&mut text, r"\newcommand{\r}{y}x^\r", false, "\u{1D466}").1 < 0.0);
}

#[test]
fn soup_of_real_tokens_does_not_panic() {
    const TOKENS: &[&str] = &["x", "1", "+", "=", "^", "_", "{", "}", "&", "'", "(", ")", "[", "]", "|", ".", ",", " ", r"\\", r"\frac", r"\sqrt", r"\left", r"\right", r"\middle", r"\begin{matrix}", r"\end{matrix}", r"\begin{cases}", r"\end{cases}", r"\begin{array}{c|c}", r"\end{array}", r"\hline", r"\text{", r"\mathbf", r"\hat", r"\sum", r"\int", r"\limits", r"\nolimits", r"\over", r"\atop", r"\not", r"\color{red}", r"\displaystyle", r"\scriptstyle", r"\phantom", r"\smash", r"\rule{1em}{1em}", r"\underbrace", r"\overbrace", r"\xrightarrow", r"\boxed", r"\cancel", r"\big", r"\quad", r"\,", r"\!", r"\alpha", r"\foo", r"\def\a{x}", r"\a", r"\tag{1}", r"\ce{", r"\binom", r"\substack", "$", "%", "#", "中"];
    let mut text = TextCtx::new(2.0);
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed as usize
    };
    let (mut good, mut bad) = (0, 0);
    for _ in 0..4000 {
        let length = next() % 24;
        let source: String = (0..length).map(|_| TOKENS[next() % TOKENS.len()]).collect();
        match probe::formula_metrics(&mut text, &source, next() % 2 == 0, 13.0) {
            Ok((w, h, d)) => {
                good += 1;
                assert!(w.is_finite() && h.is_finite() && d.is_finite(), "{source}: {w} {h} {d}");
                assert!(w >= 0.0 && h >= 0.0 && d >= 0.0, "{source}: {w} {h} {d}");
            }
            Err(_) => bad += 1,
        }
    }
    assert!(good > 200 && bad > 200, "the soup should find both ends: {good} {bad}");
}

#[test]
fn without_a_math_face_a_formula_still_stands() {
    let mut text = TextCtx::new(2.0);
    text.forget_math_face();
    assert!(!text.has_math_face());
    assert_eq!(text.math_scale(), 1.0);
    let mut with = TextCtx::new(2.0);
    assert!(with.has_math_face(), "this system has one, or the test says nothing");
    for source in REAL_FORMULAS {
        let (w, h, d) = probe::formula_metrics(&mut text, source, true, 15.0).unwrap();
        assert!(w > 0.0 && h + d > 0.0, "{source}");
    }
    // Letters are the plain ones, slanted by the renderer rather than taken from a mathematical alphabet.
    let runs = probe::formula_runs(&mut text, "x+y", false, 15.0);
    assert!(runs.iter().any(|r| r.0 == "x"), "{runs:?}");
    assert!(probe::formula_runs(&mut with, "x+y", false, 15.0).iter().any(|r| r.0 == "\u{1D465}"));
    // What the Letterlike Symbols block has is still there for the taking.
    assert!(probe::formula_runs(&mut text, r"\mathbb{R}", false, 15.0).iter().any(|r| r.0 == "ℝ"));
}

#[test]
fn every_real_formula_paints_inline_and_on_its_own() {
    let mut rig = Rig::new();
    let source: String = REAL_FORMULAS.iter().map(|f| format!("Inline ${f}$ here.\n\n$$\n{f}\n$$\n\n")).collect();
    let source: &'static str = Box::leak(source.into_boxed_str());
    rig.size = (800.0, 12_000.0);
    let view = md_view(source, MarkdownOptions::default());
    rig.settle(&view);
    assert_eq!(probe::math_notes(MD_KEY), Vec::<String>::new());
    assert_eq!(probe::placed_formulas(MD_KEY).len(), REAL_FORMULAS.len() * 2);
    assert!(rig.scene.layers[0].glyphs.len() > 1000, "{}", rig.scene.layers[0].glyphs.len());
    // A window width that wraps everything, for the same things in another order.
    rig.size = (240.0, 12_000.0);
    rig.settle(&view);
    assert_eq!(probe::placed_formulas(MD_KEY).len(), REAL_FORMULAS.len() * 2);
}

#[test]
fn a_formula_is_set_once_for_its_source_and_size() {
    let mut text = TextCtx::new(1.0);
    let source = "cached_formula_one^2";
    let (before, _) = probe::math_cache();
    let first = probe::formula_metrics(&mut text, source, true, 13.0);
    assert_eq!(probe::math_cache().0, before + 1);
    for _ in 0..5 {
        assert_eq!(probe::formula_metrics(&mut text, source, true, 13.0), first);
    }
    assert_eq!(probe::math_cache().0, before + 1, "found, not set again");
    probe::formula_metrics(&mut text, source, false, 13.0).unwrap();
    assert_eq!(probe::math_cache().0, before + 2, "inline is another drawing");
    probe::formula_metrics(&mut text, source, true, 14.0).unwrap();
    assert_eq!(probe::math_cache().0, before + 3, "so is another size");
    // A failure is an answer too: a half-streamed formula on screen does not parse again every frame.
    let bad = r"\frac{cached";
    assert!(probe::formula_metrics(&mut text, bad, true, 13.0).is_err());
    let set = probe::math_cache().0;
    assert!(probe::formula_metrics(&mut text, bad, true, 13.0).is_err());
    assert_eq!(probe::math_cache().0, set);
}

#[test]
fn the_cache_is_bounded_and_drops_what_has_gone_longest_unused() {
    let mut text = TextCtx::new(1.0);
    let keep = "x_{kept}";
    probe::formula_metrics(&mut text, keep, true, 13.0).unwrap();
    let dropped = "x_{dropped}";
    probe::formula_metrics(&mut text, dropped, true, 13.0).unwrap();
    for i in 0..probe::MATH_CACHE_LIMIT + 50 {
        probe::formula_metrics(&mut text, &format!("{i}"), true, 13.0).unwrap();
        // One formula stays in use all the way through, as one on screen does.
        probe::formula_metrics(&mut text, keep, true, 13.0).unwrap();
        assert!(probe::math_cache().1 <= probe::MATH_CACHE_LIMIT);
    }
    assert_eq!(probe::math_cache().1, probe::MATH_CACHE_LIMIT);
    let set = probe::math_cache().0;
    probe::formula_metrics(&mut text, keep, true, 13.0).unwrap();
    assert_eq!(probe::math_cache().0, set, "what is in use stays");
    probe::formula_metrics(&mut text, dropped, true, 13.0).unwrap();
    assert_eq!(probe::math_cache().0, set + 1, "what is not goes");
}

// The formula in a message.

#[test]
fn an_inline_formula_sits_on_the_baseline_of_its_line() {
    let mut rig = Rig::new();
    let view = md_view("word $x^2$ word", MarkdownOptions::default());
    rig.settle(&view);
    assert_eq!(probe::piece_texts(MD_KEY), ["word \u{FFFC} word"]);
    let placed = probe::placed_formulas(MD_KEY);
    assert_eq!(placed.len(), 1);
    let (x, baseline, w, ..) = placed[0];
    // The words' glyphs around it share its baseline: their bottoms are about where it is, for letters with no descenders.
    let before = probe::point_of(MD_KEY, 0, 0, 0).unwrap();
    let after = probe::point_of(MD_KEY, 0, 0, "word \u{FFFC} ".len()).unwrap();
    assert!(x > before.0 && x + w < after.0, "between the words: {placed:?} {before:?} {after:?}");
    assert!((baseline - before.1).abs() < 13.0, "{baseline} against a line through {}", before.1);
    // Nothing tall, nothing changes: the line is as high as a line.
    let plain = {
        let mut rig = Rig::new();
        let view = md_view("word word word", MarkdownOptions::default());
        rig.settle(&view);
        probe::height(MD_KEY)
    };
    assert_eq!(probe::height(MD_KEY), plain);
}

#[test]
fn a_tall_formula_grows_its_line_as_it_would_on_a_page() {
    let plain = {
        let mut rig = Rig::new();
        rig.settle(&md_view("before\nafter", MarkdownOptions::default()));
        probe::height(MD_KEY)
    };
    let mut rig = Rig::new();
    let view = md_view(r"before $\begin{pmatrix}a\\b\\c\\d\end{pmatrix}$ end\
after", MarkdownOptions::default());
    rig.settle(&view);
    let (_, baseline, _, h, d) = probe::placed_formulas(MD_KEY)[0];
    assert!(probe::height(MD_KEY) > plain + 20.0, "{} against {plain}", probe::height(MD_KEY));
    assert!(baseline - h >= -1.0, "nothing sticks out of the top of the message: {baseline} {h}");
    let next_line = probe::point_of(MD_KEY, 0, 0, probe::piece_texts(MD_KEY)[0].find("after").unwrap()).unwrap();
    assert!(next_line.1 > baseline + d, "and the next line clears it: {next_line:?}");
}

#[test]
fn an_inline_formula_wraps_as_one_item_and_keeps_its_comma() {
    let mut rig = Rig::new();
    let source = r"some words before it $\sum_{i=1}^{n} x_i^2 + \int_0^1 f(x)\,dx + \frac{a}{b}$, and more";
    rig.size = (220.0, 600.0);
    let view = md_view(source, MarkdownOptions::default());
    rig.settle(&view);
    let placed = probe::placed_formulas(MD_KEY);
    let (x, ..) = placed[0];
    assert_eq!(x, 0.0, "not enough room after the words, so it starts its own line: {placed:?}");
    let text = &probe::piece_texts(MD_KEY)[0];
    let comma = probe::point_of(MD_KEY, 0, 0, text.find(',').unwrap()).unwrap();
    let first = probe::point_of(MD_KEY, 0, 0, 0).unwrap();
    assert!(comma.1 > first.1, "the formula is on a later line");
    assert!(comma.0 > 0.0, "and the comma comes after it rather than starting a line: {comma:?}");
}

#[test]
fn a_selection_over_a_formula_copies_its_source_with_its_delimiters() {
    let mut rig = Rig::new();
    let view = md_view("start $x^2$ end", MarkdownOptions::default());
    rig.settle(&view);
    let from = at(probe::point_of(MD_KEY, 0, 0, 0).unwrap());
    let to = at(probe::point_of(MD_KEY, 0, 0, "start \u{FFFC} en".len()).unwrap());
    rig.drag(&view, from, to);
    assert_eq!(rig.copy(&view).as_deref(), Some("start $x^2$ en"));
    clear_text_selection();
    // A double click on it takes the formula, whole.
    let (x, baseline, w, ..) = probe::placed_formulas(MD_KEY)[0];
    let on = at((x + w * 0.25, baseline - 2.0));
    rig.click(&view, on.0, on.1);
    rig.input.pressed = true;
    rig.input.down = true;
    rig.input.double_click = true;
    rig.frame(&view);
    rig.release(&view, on.0, on.1);
    assert_eq!(rig.copy(&view).as_deref(), Some("$x^2$"));
}

#[test]
fn a_formula_that_will_not_parse_shows_its_source_and_says_why() {
    let mut rig = Rig::new();
    let view = md_view("a $\\frac{x}{$ b\n\n$$\\left(y$$\n\nc", MarkdownOptions::default());
    rig.settle(&view);
    let texts = probe::piece_texts(MD_KEY);
    assert_eq!(texts[0], "a \\frac{x}{ b");
    assert_eq!(texts[1], "\\left(y");
    assert!(probe::placed_formulas(MD_KEY).is_empty());
    assert_eq!(probe::math_notes(MD_KEY), ["Missing close brace", "Extra \\left or missing \\right"]);
    // `<code>` and nothing else: a copy of a formula that did not draw has no delimiters to carry.
    let from = at(probe::point_of(MD_KEY, 0, 0, 0).unwrap());
    let to = at(probe::point_of(MD_KEY, 2, 0, 1).unwrap());
    rig.drag(&view, from, to);
    assert_eq!(rig.copy(&view).as_deref(), Some("a \\frac{x}{ b\n\\left(y\nc"));
    clear_text_selection();
    // Hovering the formula draws nothing over it: no hover text anywhere.
    rig.input.pointer = Some(at(probe::point_of(MD_KEY, 0, 0, 3).unwrap()));
    rig.frame(&view);
    assert_eq!(rig.scene.layers[fluent::gpu::LAYER_TOOLTIP].glyphs.len(), 0);
}

#[test]
fn display_math_is_its_own_block_and_scrolls_sideways_inside_itself() {
    let mut rig = Rig::new();
    let long = format!("before\n\n$${}$$\n\nafter", vec![r"\sum_{i=1}^{n}x_i"; 40].join("+"));
    let source: &'static str = Box::leak(long.into_boxed_str());
    let view = md_view(source, MarkdownOptions::default());
    rig.settle(&view);
    let texts = probe::piece_texts(MD_KEY);
    assert_eq!(texts, ["before", "\u{FFFC}", "after"]);
    let (scroll, max) = probe::region_scroll(MD_KEY, 1, 0).expect("the formula's box");
    assert_eq!(scroll, 0.0);
    assert!(max > 100.0, "{max}");
    let region = probe::region_rect(MD_KEY, 1, 0).unwrap();
    assert!(region.w <= rig.size.0, "it stays in the column");
    let placed = probe::placed_formulas(MD_KEY);
    assert!(placed[0].2 > region.w, "while it is wider than that: {placed:?}");
    // A sideways wheel over it moves it, and it alone.
    rig.input.pointer = Some(at((region.x + 50.0, region.y + region.h * 0.3)));
    rig.frame(&view);
    rig.input.scroll_x = -60.0;
    rig.frame(&view);
    assert_eq!(probe::region_scroll(MD_KEY, 1, 0).unwrap().0, 60.0);
    // Left-aligned, as `.math-formula--display`'s `text-align: left` has it, whatever the width.
    let mut rig = Rig::new();
    let view = md_view("$$x$$", MarkdownOptions::default());
    rig.settle(&view);
    assert_eq!(probe::placed_formulas(MD_KEY)[0].0, 0.0);
    // No wider than the column, no scrolling.
    assert_eq!(probe::region_scroll(MD_KEY, 0, 0).unwrap().1, 0.0);
    // `margin: .72em 0` between its neighbours, as for any block.
    let mut rig = Rig::new();
    let view = md_view("one\n\n$$x$$\n\ntwo", MarkdownOptions::default());
    rig.settle(&view);
    let region = probe::region_rect(MD_KEY, 1, 0).unwrap();
    let (first_y, last_y) = (probe::point_of(MD_KEY, 0, 0, 0).unwrap().1, probe::point_of(MD_KEY, 2, 0, 0).unwrap().1);
    assert!(region.y > first_y && region.bottom() < last_y, "{region:?} {first_y} {last_y}");
}

#[test]
fn a_formula_follows_the_theme_without_being_set_again() {
    let mut rig = Rig::new();
    let view = md_view("$x+y$", MarkdownOptions::default());
    rig.settle(&view);
    let dark = rig.scene.layers[0].glyphs.last().expect("the formula's glyphs").color;
    assert_eq!(dark, Theme::dark().text_primary);
    let set = probe::math_cache().0;
    rig.theme = Theme::light();
    rig.settle(&view);
    let light = rig.scene.layers[0].glyphs.last().unwrap().color;
    assert_eq!(light, Theme::light().text_primary);
    assert_ne!(light, dark);
    assert_eq!(probe::math_cache().0, set, "only its paint changed");
}

#[test]
fn zooming_the_window_lays_a_formula_out_for_the_new_scale() {
    let mut rig = Rig::new();
    let view = md_view("$$\\frac{a}{b}$$", MarkdownOptions::default());
    rig.settle(&view);
    let at_one = probe::placed_formulas(MD_KEY)[0];
    rig.scale = 2.0;
    rig.text = TextCtx::new(2.0);
    rig.settle(&view);
    let at_two = probe::placed_formulas(MD_KEY)[0];
    assert!((at_one.2 - at_two.2).abs() < 3.0 && (at_one.3 - at_two.3).abs() < 3.0, "the same size in logical pixels: {at_one:?} {at_two:?}");
    let glyphs = &rig.scene.layers[0].glyphs;
    assert!(glyphs.iter().all(|g| g.size[1] > 8), "drawn at twice the pixels");
}

/// Streams a message through its steps, frames between, and answers what each showed.
fn stream_steps(rig: &mut Rig, steps: &[String]) -> Vec<(Vec<String>, usize, Vec<String>)> {
    let mut out = Vec::new();
    let options = MarkdownOptions { streaming: true, ..Default::default() };
    for step in steps {
        let source = step.clone();
        let options = options.clone();
        let view = move |_: &mut App| col().w_full().p(10.0).child(markdown_content(MD_KEY, &source, options.clone()));
        rig.settle(&view);
        out.push((probe::piece_texts(MD_KEY), probe::placed_formulas(MD_KEY).len(), probe::math_notes(MD_KEY)));
    }
    out
}

#[test]
fn a_formula_still_arriving_does_not_flicker() {
    let mut rig = Rig::new();
    let whole = r"\frac{a}{b}+\frac{c}{d}+\sqrt{\begin{pmatrix}e\\f\end{pmatrix}}";
    let steps: Vec<String> = whole.char_indices().map(|(at, c)| format!("$$\n{}", &whole[..at + c.len_utf8()])).collect();
    let shown = stream_steps(&mut rig, &steps);
    // Every step of it: nothing is marked as wrong and a typeset formula never goes back to source.
    let mut seen_drawing = false;
    for (i, (texts, formulas, notes)) in shown.iter().enumerate() {
        assert!(notes.is_empty(), "step {i}: {notes:?}");
        if seen_drawing {
            assert_eq!(*formulas, 1, "step {i}: {:?} {texts:?}", steps[i]);
        }
        seen_drawing |= *formulas == 1;
    }
    assert!(seen_drawing);
    // No red flash on the way: at no step does the formula hold an unknown command that is only half a known one.
    let opening = stream_steps(&mut Rig::new(), &["$$\n\\fra".to_owned()]);
    assert_eq!(opening[0].1, 1, "`\\fra` waits for the rest of its name");
    // Copying while it streams gets all there is of it.
    let options = MarkdownOptions { streaming: true, ..Default::default() };
    let view = move |_: &mut App| col().w_full().p(10.0).child(markdown_content(MD_KEY, "$$\n\\frac{a}{b}+\\frac{c}{", options.clone()));
    let mut rig = Rig::new();
    let start = stream_steps(&mut rig, &["$$\n\\frac{a}{b}".to_owned()]);
    assert_eq!(start[0].1, 1);
    rig.settle(&view);
    assert_eq!(probe::placed_formulas(MD_KEY).len(), 1, "the last good drawing stays");
    let (x, baseline, w, ..) = probe::placed_formulas(MD_KEY)[0];
    rig.drag(&view, at((x + 1.0, baseline - 2.0)), at((x + w + 1.0, baseline - 2.0)));
    assert_eq!(rig.copy(&view).as_deref(), Some("$$\\frac{a}{b}+\\frac{c}{$$"));
}

#[test]
fn the_end_of_a_stream_settles_what_a_formula_really_is() {
    let mut rig = Rig::new();
    let shown = stream_steps(&mut rig, &["$$\n\\frac{a}{b}".to_owned(), "$$\n\\frac{a}{b}+\\frac{c}{".to_owned()]);
    assert_eq!(shown[1].1, 1, "held while it may yet finish");
    // The stream ends with it still broken: now it is what upstream shows for a broken formula.
    let view = md_view("$$\n\\frac{a}{b}+\\frac{c}{", MarkdownOptions::default());
    rig.settle(&view);
    assert!(probe::placed_formulas(MD_KEY).is_empty());
    assert_eq!(probe::math_notes(MD_KEY), ["Missing close brace"]);
    assert_eq!(probe::piece_texts(MD_KEY), ["\\frac{a}{b}+\\frac{c}{"]);
}

#[test]
fn a_formula_the_stream_has_moved_past_shows_what_it_is() {
    let mut rig = Rig::new();
    let shown = stream_steps(&mut rig, &["$$\n\\frac{a}{b}+\\frac{c}{".to_owned(), "$$\n\\frac{a}{b}+\\frac{c}{\n$$\n\nand on".to_owned()]);
    assert_eq!(shown[0].2.len(), 0, "at the tail it is only unfinished");
    assert_eq!(shown[1].2, ["Missing close brace"], "with text after it, it is broken");
}

#[test]
fn a_formula_that_has_never_parsed_waits_quietly_as_its_source() {
    let mut rig = Rig::new();
    let shown = stream_steps(&mut rig, &["$$\n\\begin{pmatrix}a&b".to_owned()]);
    assert_eq!(shown[0].0, ["\\begin{pmatrix}a&b"]);
    assert_eq!(shown[0].1, 0);
    assert!(shown[0].2.is_empty(), "no reason to give yet");
}

#[test]
fn a_stream_only_sets_the_formula_that_changed() {
    let mut rig = Rig::new();
    let before: String = (0..40).map(|i| format!("paragraph $a_{{{i}}}$ with a formula\n\n")).collect();
    let mut steps = Vec::new();
    for end in ["x", "x+", "x+y", "x+y+", "x+y+z"] {
        steps.push(format!("{before}$$\n{end}"));
    }
    stream_steps(&mut rig, &steps[..1]);
    let (set, shaped) = (probe::math_cache().0, probe::shaped_count());
    stream_steps(&mut rig, &steps[1..]);
    assert!(probe::math_cache().0 - set <= 8, "{} formulas set for four new versions of one", probe::math_cache().0 - set);
    assert!(probe::shaped_count() - shaped <= 8, "{} runs shaped: the paragraphs before it stay as they are", probe::shaped_count() - shaped);
}

#[test]
fn a_formula_in_a_link_a_heading_a_table_and_a_list_finds_its_place() {
    let mut rig = Rig::new();
    let view = md_view("# Head $x$\n\n- item $y$\n\n| a |\n|---|\n| $z^2$ |\n\n[link $w$](https://example.com)", MarkdownOptions::default());
    rig.settle(&view);
    let placed = probe::placed_formulas(MD_KEY);
    assert_eq!(placed.len(), 4, "{placed:?}");
    assert!(placed.windows(2).all(|p| p[1].1 > p[0].1), "in reading order: {placed:?}");
    // A heading's is larger, as its text is.
    assert!(placed[0].3 > placed[1].3, "{placed:?}");
    // The link's box covers the formula: a click on it follows the link.
    let (x, baseline, w, ..) = placed[3];
    let opened = Rc::new(RefCell::new(None::<String>));
    let saw = opened.clone();
    let options = MarkdownOptions { on_open_external: Some(Rc::new(move |_: &mut App, url: &str| *saw.borrow_mut() = Some(url.to_owned()))), ..Default::default() };
    let view = md_view("# Head $x$\n\n- item $y$\n\n| a |\n|---|\n| $z^2$ |\n\n[link $w$](https://example.com)", options);
    rig.settle(&view);
    let on = at((x + w * 0.5, baseline - 2.0));
    rig.click(&view, on.0, on.1);
    assert_eq!(opened.borrow().as_deref(), Some("https://example.com/"));
}

// ---------------------------------------------------------------- parsing

#[test]
fn renders_common_markdown_without_executing_embedded_html() {
    let source = "# 标题\n\n**重点**与~~删除~~\n\n- [x] 已完成\n\n| 项目 | 值 |\n| --- | ---: |\n| A | 1 |\n\n`行内代码`\n\n```ts\nconst answer = 42;\n```\n\n[外部链接](https://example.com)\n\n<script>window.__unsafe = true</script>";
    let blocks = parse_markdown(source, ParseOptions { render_html: true, ..opts() });
    assert!(matches!(&blocks[0], Block::Heading { level: 1, inlines, .. } if plain(inlines) == "标题"));
    let styled = para(&blocks[1]);
    assert!(styled.iter().any(|i| matches!(i, Inline::Text { text, style, .. } if text == "重点" && style.bold)));
    assert!(styled.iter().any(|i| matches!(i, Inline::Text { text, style, .. } if text == "删除" && style.strike)));
    assert!(matches!(&blocks[2], Block::List { start: None, items } if items[0].task == Some(true) && matches!(&items[0].blocks[0], Block::Paragraph { inlines, tight: true } if plain(inlines) == "已完成")));
    let Block::Table { rows, .. } = &blocks[3] else { panic!("a table") };
    assert!(rows[0].header && !rows[1].header);
    assert_eq!(plain(&rows[1].cells[0].inlines), "A");
    assert_eq!(rows[1].cells[1].align, TextAlign::End);
    assert!(matches!(&para(&blocks[4])[0], Inline::Text { text, style, .. } if text == "行内代码" && style.code));
    assert!(matches!(&blocks[5], Block::Code { label: Some(l), language: Some(_), code, .. } if l == "ts" && &**code == "const answer = 42;"));
    assert!(matches!(&para(&blocks[6])[0], Inline::Text { link: Some(link), .. } if matches!(&link.target, LinkTarget::External(u) if u == "https://example.com")));
    let mut text = String::new();
    all_text(&blocks, &mut text);
    assert!(!text.contains("__unsafe"), "{text}");
}

#[test]
fn lists_nest_and_number() {
    let blocks = parse_markdown("3. one\n   - nested\n     - deeper\n4. two\n\n   more of two\n", opts());
    let Block::List { start: Some(3), items } = &blocks[0] else { panic!("{blocks:?}") };
    assert_eq!(items.len(), 2);
    let Block::List { start: None, items: nested } = &items[0].blocks[1] else { panic!("{:?}", items[0].blocks) };
    assert!(matches!(&nested[0].blocks[1], Block::List { .. }));
    assert_eq!(items[1].blocks.len(), 2);
}

#[test]
fn a_line_break_in_the_source_is_one_on_the_page() {
    assert_eq!(plain(&first_para("one\ntwo", opts())), "one\ntwo");
}

#[test]
fn quotes_rules_and_images() {
    let blocks = parse_markdown("> quoted\n\n---\n\n![alt](pic.png)", opts());
    assert!(matches!(&blocks[0], Block::Quote(inner) if matches!(&inner[0], Block::Paragraph { .. })));
    assert!(matches!(blocks[1], Block::Rule));
    assert!(matches!(&para(&blocks[2])[0], Inline::Image { src, alt, .. } if src == "pic.png" && alt == "alt"));
}

#[test]
fn bare_urls_become_links_and_trailing_punctuation_stays_text() {
    let inlines = first_para("see https://example.com/a_b_c/d?x=1. Then www.example.org, or me@example.com!", opts());
    let links: Vec<(String, LinkTarget)> = inlines.iter().filter_map(|i| if let Inline::Text { text, link: Some(l), .. } = i { Some((text.clone(), l.target.clone())) } else { None }).collect();
    assert_eq!(links[0], ("https://example.com/a_b_c/d?x=1".to_owned(), LinkTarget::External("https://example.com/a_b_c/d?x=1".to_owned())));
    assert_eq!(links[1], ("www.example.org".to_owned(), LinkTarget::External("http://www.example.org".to_owned())));
    assert_eq!(links[2], ("me@example.com".to_owned(), LinkTarget::Protocol("mailto:me@example.com".to_owned())));
    assert_eq!(plain(&inlines), "see https://example.com/a_b_c/d?x=1. Then www.example.org, or me@example.com!");
}

#[test]
fn link_rules_follow_the_a_override() {
    let target = |source: &str, options: ParseOptions| -> LinkTarget {
        let inlines = first_para(source, options);
        inlines.iter().find_map(|i| if let Inline::Text { link: Some(l), .. } = i { Some(l.target.clone()) } else { None }).expect("a link")
    };
    let paths = ParseOptions { linkify_paths: true, ..opts() };
    assert_eq!(target("[a](#安装)", opts()), LinkTarget::Fragment("安装".to_owned()));
    assert_eq!(target("[a](https://x.y/z)", opts()), LinkTarget::External("https://x.y/z".to_owned()));
    assert_eq!(target("[a](mailto:a@b.c)", opts()), LinkTarget::Protocol("mailto:a@b.c".to_owned()));
    // Turning a link to a file into a path the file pane can open, line included.
    assert_eq!(target("[App](src/App.tsx#L12)", paths), LinkTarget::Path { target: "src/App.tsx".to_owned(), line: Some(12) });
    assert_eq!(target("[配置](file:///w/Cargo.toml:3)", paths), LinkTarget::Path { target: "/w/Cargo.toml".to_owned(), line: Some(3) });
    // Leaving document links for the caller to resolve against the document.
    assert_eq!(target("[下一篇](other.md)", ParseOptions { document_links: true, ..paths }), LinkTarget::Document("other.md".to_owned()));
    // A surface that opens no files keeps the link and sends it nowhere.
    assert_eq!(target("[a](b/c.md)", opts()), LinkTarget::Inert);
    // What `defaultUrlTransform` refuses.
    assert_eq!(target("[a](javascript:alert(1))", paths), LinkTarget::Inert);
    assert_eq!(target("[a](vbscript:x)", paths), LinkTarget::Inert);
}

#[test]
fn links_only_what_qualifies_as_a_path() {
    let sample = "见 `src/App.tsx:12` 与 C:\\Windows\\notepad.exe，注意 and/or 与 https://example.com/a/b";
    let collect = |options: ParseOptions| -> Vec<(String, Option<u32>)> {
        first_para(sample, options).iter().filter_map(|i| match i {
            Inline::Text { link: Some(l), .. } => match &l.target {
                LinkTarget::Path { target, line } => Some((target.clone(), *line)),
                _ => None,
            },
            _ => None,
        }).collect()
    };
    assert_eq!(collect(opts()), []);
    assert_eq!(collect(ParseOptions { linkify_paths: true, ..opts() }), [("src/App.tsx".to_owned(), Some(12)), ("C:\\Windows\\notepad.exe".to_owned(), None)]);
    let inlines = first_para(sample, ParseOptions { linkify_paths: true, ..opts() });
    // The line reference stays visible even though it is not sent to the host.
    assert!(inlines.iter().any(|i| matches!(i, Inline::Text { text, style, .. } if text == "src/App.tsx:12" && style.code)));
    // The address stays an address.
    assert!(inlines.iter().any(|i| matches!(i, Inline::Text { link: Some(l), .. } if matches!(&l.target, LinkTarget::External(u) if u == "https://example.com/a/b"))));
    assert!(plain(&inlines).contains("and/or"));
}

#[test]
fn keeps_paths_inside_fenced_code_blocks_as_plain_code() {
    let blocks = parse_markdown("```\nsrc/App.tsx\n```", ParseOptions { linkify_paths: true, ..opts() });
    assert!(matches!(&blocks[0], Block::Code { code, .. } if &**code == "src/App.tsx"));
}

#[test]
fn paths_in_math_are_left_alone() {
    let inlines = first_para("$/usr/bin/env$", ParseOptions { linkify_paths: true, ..opts() });
    assert!(matches!(&inlines[0], Inline::Math { source, link: None, .. } if source == "/usr/bin/env"));
}

#[test]
fn a_reference_defined_elsewhere_resolves() {
    let blocks = parse_markdown("[a][b]\n\n[b]: https://x.y/z", opts());
    assert!(matches!(&para(&blocks[0])[0], Inline::Text { link: Some(l), .. } if l.target == LinkTarget::External("https://x.y/z".to_owned())));
}

#[test]
fn footnotes_go_to_the_end() {
    let blocks = parse_markdown("one[^n]\n\n[^n]: note\n\ntwo", opts());
    assert!(matches!(&blocks[2], Block::Footnote { label, .. } if label == "n"), "{blocks:?}");
    assert!(para(&blocks[0]).iter().any(|i| matches!(i, Inline::Text { style, link: Some(l), .. } if style.sup && l.target == LinkTarget::Fragment("user-content-fn-n".to_owned()))));
}

// ---------------------------------------------------------------- HTML (MarkdownContent.test.tsx)

const HTML_SAMPLE: &str = "| a | b |\n| --- | --- |\n| 一<br>二 | <kbd>Ctrl</kbd> |\n\n<details><summary>更多</summary>\n\n内容 H<sub>2</sub>O\n\n</details>\n\n<p align=\"center\" style=\"color: red\" onclick=\"alert(1)\">居中</p>\n\n<script>window.__unsafe = true</script><iframe src=\"https://example.com\"></iframe>";

#[test]
fn shows_html_as_the_text_it_is_unless_the_surface_opts_in() {
    let blocks = parse_markdown(HTML_SAMPLE, opts());
    assert!(!blocks.iter().any(|b| matches!(b, Block::Details { .. })));
    let mut text = String::new();
    all_text(&blocks, &mut text);
    assert!(text.contains("<kbd>Ctrl</kbd>"), "{text}");
}

#[test]
fn renders_the_allowed_html_and_drops_everything_that_could_run_or_restyle() {
    let blocks = parse_markdown(HTML_SAMPLE, ParseOptions { render_html: true, ..opts() });
    let Block::Table { rows, .. } = &blocks[0] else { panic!("{blocks:?}") };
    assert_eq!(plain(&rows[1].cells[0].inlines), "一\n二");
    assert!(rows[1].cells[1].inlines.iter().any(|i| matches!(i, Inline::Text { text, style, .. } if text == "Ctrl" && style.kbd)));
    let details = blocks.iter().find_map(|b| if let Block::Details { summary, blocks, open, .. } = b { Some((summary, blocks, *open)) } else { None }).expect("details");
    assert_eq!(plain(details.0), "更多");
    assert!(!details.2);
    let inner = para(&details.1[0]);
    assert!(inner.iter().any(|i| matches!(i, Inline::Text { text, style, .. } if text == "2" && style.sub)), "{inner:?}");
    assert_eq!(plain(inner), "内容 H2O");
    assert!(blocks.iter().any(|b| matches!(b, Block::Aligned { align: TextAlign::Center, blocks } if plain(para(&blocks[0])) == "居中")));
    let mut text = String::new();
    all_text(&blocks, &mut text);
    assert!(!text.contains("__unsafe") && !text.contains("iframe"), "{text}");
}

#[test]
fn keeps_math_and_detected_paths_working_alongside_html() {
    let inlines = first_para("见 `src/a.ts:3`<br>和 $x^2$", ParseOptions { render_html: true, linkify_paths: true, ..opts() });
    assert!(inlines.iter().any(|i| matches!(i, Inline::Text { link: Some(l), .. } if l.target == LinkTarget::Path { target: "src/a.ts".to_owned(), line: Some(3) })), "{inlines:?}");
    assert!(inlines.iter().any(|i| matches!(i, Inline::Break)));
    assert!(inlines.iter().any(|i| matches!(i, Inline::Math { source, .. } if source == "x^2")));
}

#[test]
fn html_inside_a_quote_stays_in_the_quote() {
    let blocks = parse_markdown("> <div align=\"right\">inside</div>\n\nout", ParseOptions { render_html: true, ..opts() });
    assert!(matches!(&blocks[0], Block::Quote(inner) if matches!(&inner[0], Block::Aligned { align: TextAlign::End, .. })), "{blocks:?}");
}

#[test]
fn html_details_kbd_mark_and_friends() {
    let options = ParseOptions { render_html: true, ..opts() };
    let inlines = first_para("<mark>m</mark> <sup>s</sup> <b>b</b> <i>i</i> <u>u</u> <del>d</del> <code>c</code> <small>x</small>", options);
    let has = |want: &str, f: &dyn Fn(&InlineStyle) -> bool| inlines.iter().any(|i| matches!(i, Inline::Text { text, style, .. } if text == want && f(style)));
    assert!(has("m", &|s| s.mark));
    assert!(has("s", &|s| s.sup));
    assert!(has("b", &|s| s.bold));
    assert!(has("i", &|s| s.italic));
    assert!(has("u", &|s| s.underline));
    assert!(has("d", &|s| s.strike));
    assert!(has("c", &|s| s.code));
    assert!(has("x", &|s| s.small));
    // A link keeps only the protocols the list allows.
    let bad = first_para("<a href=\"javascript:alert(1)\">x</a>", options);
    assert!(matches!(&bad[0], Inline::Text { link: None, .. }), "{bad:?}");
    let good = first_para("<a href=\"https://x.y/\">x</a>", options);
    assert!(matches!(&good[0], Inline::Text { link: Some(l), .. } if l.target == LinkTarget::External("https://x.y/".to_owned())));
    // An element off the list keeps its text, and `<script>` keeps nothing.
    let blocks = parse_markdown("<div><foo>kept</foo><script>gone</script></div>", options);
    let mut text = String::new();
    all_text(&blocks, &mut text);
    assert_eq!(text.trim(), "kept");
    // An HTML anchor is a place, with the prefix the sanitizer gives ids.
    assert!(parse_markdown("<a name=\"x\"></a>\n\ntext", options).iter().any(|b| matches!(b, Block::Anchor(a) if a == "user-content-x")));
    assert!(may_contain_html("a <b>c</b>") && !may_contain_html("a < b and c > d"));
}

#[test]
fn nesting_too_deep_to_follow_does_not_take_the_app_down() {
    let quotes = ">".repeat(5000) + " deep";
    let blocks = parse_markdown(&quotes, opts());
    let mut text = String::new();
    all_text(&blocks, &mut text);
    assert!(text.contains("deep"));
    let divs = "<div>".repeat(5000) + "x";
    parse_markdown(&divs, ParseOptions { render_html: true, ..opts() });
}

// ---------------------------------------------------------------- the leaf: layout

#[test]
fn every_block_kind_gets_a_height_and_measure_answers_for_the_width_painted() {
    let mut rig = Rig::new();
    let source = "# Title\n\nA paragraph that is long enough to want more than one line when the column is not very wide at all.\n\n- one\n- [x] two\n  - nested\n\n> quote\n\n---\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```rust\nfn main() {}\n```\n\n$$x^2$$\n";
    let view = md_view(source, MarkdownOptions::default());
    rig.settle(&view);
    let wide = rect_of(markdown_key(MD_KEY)).expect("laid out");
    assert_eq!(wide.w, 780.0);
    let height = probe::height(MD_KEY);
    assert!((wide.h - height).abs() < 0.5, "{wide:?} against {height}");
    assert!(height > 200.0, "{height}");
    // Narrower wraps, so taller, and the leaf says so.
    rig.size = (300.0, 600.0);
    rig.settle(&view);
    let narrow = rect_of(markdown_key(MD_KEY)).expect("laid out");
    assert_eq!(narrow.w, 280.0);
    assert!(narrow.h > wide.h, "{narrow:?} against {wide:?}");
    assert!((narrow.h - probe::height(MD_KEY)).abs() < 0.5);
}

#[test]
fn a_shrink_to_fit_parent_gets_the_widest_line() {
    let mut rig = Rig::new();
    let view = |_: &mut App| div::<App>().child(row().child(markdown_content("fit", "short", MarkdownOptions::default())));
    rig.settle(&view);
    let r = rect_of(markdown_key("fit")).expect("laid out");
    assert!(r.w > 10.0 && r.w < 100.0, "{r:?}");
}

#[test]
fn margins_collapse_like_css() {
    let mut rig = Rig::new();
    let view = md_view("para one\n\npara two", MarkdownOptions::default());
    rig.settle(&view);
    // Two lines with `.48em` between them, and the first's top and the last's bottom taken away.
    let expected = 20.5f32.round() * 2.0 + 13.0 * 0.48;
    assert!((probe::height(MD_KEY) - expected).abs() < 1.0, "{} against {expected}", probe::height(MD_KEY));
}

#[test]
fn a_file_ending_in_a_newline_has_no_extra_last_line() {
    assert_eq!(split_code_lines("a\nb\n"), ["a", "b"]);
    assert_eq!(split_code_lines("a\r\nb"), ["a", "b"]);
    assert_eq!(split_code_lines("a\n\n"), ["a", ""]);
    assert_eq!(split_code_lines(""), [""]);
}

#[test]
fn tabs_draw_as_spaces_and_copy_as_tabs() {
    let mut rig = Rig::new();
    let view = md_view("```\n\ta\tb\n```", MarkdownOptions::default());
    rig.settle(&view);
    assert_eq!(probe::piece_texts(MD_KEY), ["        a       b"]);
    let from = at(probe::point_of(MD_KEY, 0, 0, 0).unwrap());
    let to = at(probe::point_of(MD_KEY, 0, 0, 17).unwrap());
    rig.drag(&view, from, (to.0 + 5.0, to.1));
    assert_eq!(rig.copy(&view).as_deref(), Some("\ta\tb"));
}

#[test]
fn estimate_is_bounded_and_sensible() {
    assert_eq!(estimate_markdown_height("", 600.0), 20.0);
    assert_eq!(estimate_markdown_height("a line", 448.0), 22.0);
    let long = "word ".repeat(100_000);
    let started = std::time::Instant::now();
    let estimate = estimate_markdown_height(&long, 448.0);
    assert!(started.elapsed().as_millis() < 50);
    assert!(estimate > 100_000.0 && estimate <= 2_000_000.0, "{estimate}");
}

// ---------------------------------------------------------------- block reuse

#[test]
fn only_the_tail_is_shaped_again_when_only_the_tail_changed() {
    let mut rig = Rig::new();
    let content = Rc::new(RefCell::new(String::new()));
    for i in 0..200 {
        content.borrow_mut().push_str(&format!("Paragraph number {i} with some words in it.\n\n"));
    }
    let view = {
        let content = content.clone();
        move |_: &mut App| col().w_full().child(markdown_content("reuse", &content.borrow(), MarkdownOptions::default()))
    };
    rig.settle(&view);
    assert_eq!(probe::block_count("reuse"), 200);
    let hashes = probe::block_hashes("reuse");

    let before = probe::shaped_count();
    content.borrow_mut().push_str("The tail grows");
    rig.frame(&view);
    assert_eq!(probe::shaped_count() - before, 1, "one new block");
    let before = probe::shaped_count();
    content.borrow_mut().push_str(" and grows");
    rig.frame(&view);
    assert_eq!(probe::shaped_count() - before, 1, "only the block that changed");
    assert_eq!(probe::block_count("reuse"), 201);
    assert_eq!(probe::block_hashes("reuse")[..200], hashes[..200]);

    // The same content again is no work at all.
    let before = probe::shaped_count();
    rig.frame(&view);
    assert_eq!(probe::shaped_count(), before);
}

#[test]
fn a_change_in_the_middle_keeps_everything_else_too() {
    let mut rig = Rig::new();
    let content = Rc::new(RefCell::new(String::from("one\n\ntwo\n\nthree\n\nfour\n\n")));
    let view = {
        let content = content.clone();
        move |_: &mut App| col().w_full().child(markdown_content("edit", &content.borrow(), MarkdownOptions::default()))
    };
    rig.settle(&view);
    let before = probe::shaped_count();
    *content.borrow_mut() = String::from("one\n\nTWO\n\nthree\n\nfour\n\n");
    rig.frame(&view);
    assert_eq!(probe::shaped_count() - before, 1);
    assert_eq!(probe::piece_texts("edit"), ["one", "TWO", "three", "four"]);
}

#[test]
fn a_growing_list_shapes_its_last_item_and_a_growing_code_block_its_last_run() {
    let mut rig = Rig::new();
    let content = Rc::new(RefCell::new(String::new()));
    for i in 0..100 {
        content.borrow_mut().push_str(&format!("- item {i}\n"));
    }
    let view = {
        let content = content.clone();
        move |_: &mut App| col().w_full().child(markdown_content("grow", &content.borrow(), MarkdownOptions::default()))
    };
    rig.settle(&view);
    let before = probe::shaped_count();
    content.borrow_mut().push_str("- item 100\n");
    rig.frame(&view);
    assert_eq!(probe::shaped_count() - before, 1, "the list is one block, and all but one item comes back from the pool");
    assert_eq!(probe::piece_texts("grow").len(), 101);

    let mut code = String::from("```rust\n");
    for i in 0..1000 {
        code.push_str(&format!("let x{i} = {i};\n"));
    }
    *content.borrow_mut() = code;
    rig.frame(&view);
    let before = probe::shaped_count();
    content.borrow_mut().push_str("let more = 1;\n");
    rig.frame(&view);
    assert_eq!(probe::shaped_count() - before, 1, "one run of lines");
}

#[test]
fn a_stream_parses_the_same_as_the_whole_text() {
    // However the text is cut, what is on the page at the end is the same, indented lists and all.
    let source = "Intro\n\n  - indented\n  - list\n    - nested\n\n> quote\ncontinues\n\n```rust\nfn a() {}\n\nfn b() {}\n```\n\n1. one\n\n   two\n2. three\n\ntext with [a ref][r] and $x$\n\n$$\ny\n\nz\n$$\n\n[r]: https://x.y/z\n\nlast line";
    let mut rig = Rig::new();
    let whole = {
        let view = move |_: &mut App| col().w_full().child(markdown_content("whole", source, MarkdownOptions::default()));
        rig.settle(&view);
        probe::piece_texts("whole")
    };
    let content = Rc::new(RefCell::new(String::new()));
    let view = {
        let content = content.clone();
        move |_: &mut App| col().w_full().child(markdown_content("piecemeal", &content.borrow(), MarkdownOptions { streaming: true, ..Default::default() }))
    };
    let chars: Vec<char> = source.chars().collect();
    for chunk in chars.chunks(7) {
        content.borrow_mut().extend(chunk);
        rig.frame(&view);
    }
    rig.settle(&view);
    assert_eq!(probe::piece_texts("piecemeal"), whole);
    assert_eq!(probe::height("piecemeal"), probe::height("whole"));
}

#[test]
fn a_definition_arriving_later_reaches_back() {
    let mut rig = Rig::new();
    let content = Rc::new(RefCell::new(String::from("[a][b]\n\nmore\n\n")));
    let link: Rc<Cell<u32>> = Rc::new(Cell::new(0));
    let view = {
        let (content, link) = (content.clone(), link.clone());
        move |_: &mut App| {
            let link = link.clone();
            let options = MarkdownOptions { on_open_external: Some(Rc::new(move |_, _| link.set(link.get() + 1))), ..Default::default() };
            col().w_full().child(markdown_content("late", &content.borrow(), options))
        }
    };
    rig.settle(&view);
    assert_eq!(probe::piece_texts("late"), ["[a][b]", "more"]);
    content.borrow_mut().push_str("[b]: https://x.y/z\n");
    rig.frame(&view);
    assert_eq!(probe::piece_texts("late"), ["a", "more"]);
}

#[test]
fn state_outlives_the_view_and_the_table_stays_bounded() {
    let mut rig = Rig::new();
    let content = "```\nlong\n```";
    let on = Rc::new(Cell::new(true));
    let view = {
        let on = on.clone();
        move |_: &mut App| col().w_full().when(on.get(), |e| e.child(markdown_content("back", content, MarkdownOptions::default())))
    };
    rig.settle(&view);
    let shaped = probe::shaped_count();
    on.set(false);
    rig.settle(&view);
    on.set(true);
    rig.settle(&view);
    assert_eq!(probe::shaped_count(), shaped, "scrolled away and back: nothing to do again");

    let many = |_: &mut App| {
        let mut list = col().w_full();
        for i in 0..600 {
            list = list.child(markdown_content(("many", i), "x", MarkdownOptions::default()));
        }
        list
    };
    rig.frame(&many);
    assert!(probe::entry_count() <= 512, "{}", probe::entry_count());
}

// ---------------------------------------------------------------- selection

const SEL_SOURCE: &str = "first paragraph\n\n| a | b |\n|---|---|\n| c | d |\n\n```\ncode line\n```\n\nlast `span` here";

#[test]
fn a_drag_selects_across_blocks_table_cells_and_code_and_copies_with_the_right_separators() {
    let mut rig = Rig::new();
    let view = md_view(SEL_SOURCE, MarkdownOptions::default());
    rig.settle(&view);
    assert_eq!(probe::piece_texts(MD_KEY), ["first paragraph", "a", "b", "c", "d", "code line", "last \u{a0}span\u{a0} here"]);
    let from = at(probe::point_of(MD_KEY, 0, 0, 6).expect("first"));
    let to = at(probe::point_of(MD_KEY, 3, 0, 4).expect("last"));
    rig.drag(&view, from, to);
    assert!(has_text_selection());
    assert_eq!(rig.copy(&view).as_deref(), Some("paragraph\na\tb\nc\td\ncode line\nlast"));
}

#[test]
fn inline_code_copies_without_its_padding_and_a_formula_as_its_source() {
    let mut rig = Rig::new();
    let view = md_view("a `b` c $x^2$ d\n\n$$y^2$$\n\nend", MarkdownOptions::default());
    rig.settle(&view);
    let texts = probe::piece_texts(MD_KEY);
    assert!(texts[0].contains("c \u{FFFC} d"), "{texts:?}");
    assert_eq!(texts[1], "\u{FFFC}");
    let from = at(probe::point_of(MD_KEY, 0, 0, 0).expect("first"));
    let to = at(probe::point_of(MD_KEY, 2, 0, 3).expect("last"));
    rig.drag(&view, from, to);
    assert_eq!(rig.copy(&view).as_deref(), Some("a b c $x^2$ d\n$$y^2$$\nend"));
}

#[test]
fn double_click_selects_a_word_and_triple_click_a_block() {
    let mut rig = Rig::new();
    let view = md_view("hello brave world\n\nnext", MarkdownOptions::default());
    rig.settle(&view);
    let p = at(probe::point_of(MD_KEY, 0, 0, 8).expect("brave"));
    rig.click(&view, p.0, p.1);
    assert_eq!(rig.copy(&view), None, "a click is not a selection");
    let again = |rig: &mut Rig| {
        rig.input.pressed = true;
        rig.input.down = true;
        rig.input.double_click = true;
        rig.frame(&view);
        rig.release(&view, p.0, p.1);
    };
    again(&mut rig);
    assert_eq!(rig.copy(&view).as_deref(), Some("brave"));
    again(&mut rig);
    assert_eq!(rig.copy(&view).as_deref(), Some("hello brave world"));
}

#[test]
fn a_press_elsewhere_clears_it_and_a_press_on_another_message_moves_it() {
    let mut rig = Rig::new();
    let clicked = Rc::new(Cell::new(0));
    let view = {
        let clicked = clicked.clone();
        move |_: &mut App| {
            let clicked = clicked.clone();
            col().w_full()
                .child(markdown_content("left", "the first message", MarkdownOptions::default()))
                .child(markdown_content("right", "the second message", MarkdownOptions::default()))
                .child(div().key("elsewhere").h(40.0).w(100.0).on_click(move |_: &mut App| clicked.set(clicked.get() + 1)))
        }
    };
    rig.settle(&view);
    let left = rect_of(markdown_key("left")).expect("left");
    let right = rect_of(markdown_key("right")).expect("right");
    let select = |rig: &mut Rig, r: Rect| {
        let (a, b) = ((r.x + 2.0, r.y + 10.0), (r.x + 60.0, r.y + 10.0));
        rig.drag(&view, a, b);
    };
    select(&mut rig, left);
    assert!(has_text_selection());
    let first_copy = rig.copy(&view).expect("selected");
    assert!(first_copy.starts_with("the"), "{first_copy}");

    // On the other message: the first one lets go.
    select(&mut rig, right);
    assert!(has_text_selection());
    assert!(rig.copy(&view).is_some());
    let elsewhere = rect_of("elsewhere").expect("button");
    rig.click(&view, elsewhere.x + 5.0, elsewhere.y + 5.0);
    assert_eq!(clicked.get(), 1, "the press landed as well");
    assert!(!has_text_selection());
    assert_eq!(rig.copy(&view), None);

    select(&mut rig, left);
    assert!(has_text_selection());
    clear_text_selection();
    assert!(!has_text_selection());
    assert_eq!(rig.copy(&view), None);
}

#[test]
fn a_selection_scrolled_out_of_view_still_copies_and_still_lets_go() {
    let mut rig = Rig::new();
    let shown = Rc::new(Cell::new(true));
    let view = {
        let shown = shown.clone();
        move |_: &mut App| {
            col().w_full()
                .when(shown.get(), |e| e.child(markdown_content("gone", "the first message", MarkdownOptions::default())))
                .child(markdown_content("stays", "the second message", MarkdownOptions::default()))
                .child(div().key("below").h(40.0).w(100.0).on_click(|_: &mut App| {}))
        }
    };
    rig.settle(&view);
    let r = rect_of(markdown_key("gone")).expect("first");
    rig.drag(&view, (r.x + 2.0, r.y + 10.0), (r.x + 60.0, r.y + 10.0));
    let selected = rig.copy(&view).expect("selected");
    shown.set(false);
    rig.settle(&view);
    assert_eq!(rig.copy(&view).as_deref(), Some(selected.as_str()), "through the message that is on screen");
    assert!(has_text_selection());
    let below = rect_of("below").expect("below");
    rig.click(&view, below.x + 5.0, below.y + 25.0);
    assert!(!has_text_selection());
}

#[test]
fn a_press_takes_focus_away_from_a_field() {
    let mut rig = Rig::new();
    let view = md_view("words to press on", MarkdownOptions::default());
    rig.settle(&view);
    rig.mem.focus = 12345;
    let p = at(probe::point_of(MD_KEY, 0, 0, 2).expect("text"));
    rig.click(&view, p.0, p.1);
    assert_eq!(rig.mem.focus, 0);
}

#[test]
fn a_captured_press_is_not_heard_and_dragging_asks_for_frames() {
    let mut rig = Rig::new();
    let captured = Rc::new(Cell::new(false));
    let view = {
        let captured = captured.clone();
        move |_: &mut App| {
            let captured = captured.clone();
            col().w_full().on_press_capture(move |_: &mut App, _| captured.get()).child(markdown_content("cap", "some words here", MarkdownOptions::default()))
        }
    };
    rig.settle(&view);
    let r = rect_of(markdown_key("cap")).expect("laid out");
    captured.set(true);
    rig.drag(&view, (r.x + 2.0, r.y + 10.0), (r.x + 60.0, r.y + 10.0));
    assert!(!has_text_selection());
    captured.set(false);
    rig.press(&view, r.x + 2.0, r.y + 10.0);
    rig.input.pointer = Some((r.x + 60.0, r.y + 10.0));
    rig.frame(&view);
    assert!(rig.animating, "a drag keeps frames coming");
    rig.release(&view, r.x + 60.0, r.y + 10.0);
    assert!(has_text_selection());
    rig.frame(&view);
    assert!(!rig.animating, "nothing renders while idle");
    clear_text_selection();
}

// ---------------------------------------------------------------- code block chrome

fn long_code(lines: usize) -> String {
    let mut code = String::from("```rust\n");
    for i in 0..lines {
        code.push_str(&format!("let value_{i} = {i}; // a line of code that is quite long so that it needs to wrap in a narrow column of text\n"));
    }
    code.push_str("```");
    code
}

fn code_view(key: &'static str, source: String) -> impl Fn(&mut App) -> E {
    move |_| col().w_full().child(markdown_content(key, &source, MarkdownOptions::default()))
}

#[test]
fn the_copy_button_copies_the_whole_block() {
    let mut rig = Rig::new();
    let source = "```rust\nfn main() {}\n```".to_owned();
    let view = code_view("copy", source);
    rig.settle(&view);
    let copy = probe::copy_button("copy", 0, 0).expect("copy");
    rig.click(&view, copy.x + 5.0, copy.y + 5.0);
    assert_eq!(rig.clipboard.0.as_deref(), Some("fn main() {}"));
    assert!(rig.wake_in.is_some(), "the check mark goes away on its own");
    assert!(!has_text_selection());
}

#[test]
fn the_wrap_setting_changes_the_height_and_the_scroll() {
    let mut rig = Rig::new();
    rig.size = (400.0, 600.0);
    let view = code_view("wrap", long_code(3));
    rig.settle(&view);
    let (_, max) = probe::region_scroll("wrap", 0, 0).expect("region");
    assert!(max > 0.0, "code that does not wrap scrolls sideways");
    let before = probe::height("wrap");
    rig.app.doc.global_settings.appearance.code_block_wrappable = true;
    rig.settle(&view);
    assert!(probe::height("wrap") > before, "{} against {before}", probe::height("wrap"));
    assert_eq!(probe::region_scroll("wrap", 0, 0).expect("region").1, 0.0);
    rig.app.doc.global_settings.appearance.code_block_wrappable = false;
    rig.settle(&view);
    assert_eq!(probe::height("wrap"), before);
}

#[test]
fn a_tall_block_held_back_by_the_setting_lets_go_while_the_pointer_is_on_it() {
    let mut rig = Rig::new();
    let source = format!("{}\n\nAfter the code.", long_code(40));
    let view = code_view("collapse", source);
    rig.settle(&view);
    let full = probe::height("collapse");
    rig.app.doc.global_settings.appearance.code_block_collapsible = true;
    rig.settle(&view);
    let held = probe::height("collapse");
    assert!(held < full, "{held} against {full}");
    let region = probe::region_rect("collapse", 0, 0).expect("the block");
    assert_eq!(region.h, 13.0 * 22.0);

    // `pre:hover`: all of it, for as long as the pointer stays, with the words below it moved down in the same frame.
    rig.input.pointer = Some((region.x + 40.0, region.y + 40.0));
    rig.frame(&view);
    assert!(rig.animating, "it asks for the frame that opens it");
    rig.frame(&view);
    assert_eq!(probe::height("collapse"), full);
    assert!(probe::region_rect("collapse", 0, 0).expect("the block").h > 13.0 * 22.0);
    rig.settle(&view);
    assert_eq!(probe::height("collapse"), full, "it stays open under a pointer at rest");

    // The bar of controls stands beside the code, not in it: the block is held back again under the copy button, which stays put.
    let copy = probe::copy_button("collapse", 0, 0).expect("copy");
    rig.input.pointer = Some((copy.x + 5.0, copy.y + 5.0));
    rig.settle(&view);
    assert_eq!(probe::height("collapse"), held);
    assert_eq!(probe::copy_button("collapse", 0, 0).expect("copy"), copy);

    rig.input.pointer = Some((region.x + 40.0, region.y + 40.0));
    rig.settle(&view);
    assert_eq!(probe::height("collapse"), full);
    rig.input.pointer = Some((region.x + 40.0, full + 500.0));
    rig.settle(&view);
    assert_eq!(probe::height("collapse"), held, "gone again when the pointer leaves");

    // Short code never had anything to hold back, and a pointer on it asks for nothing.
    let short = code_view("short", "```\nshort\n```".to_owned());
    rig.settle(&short);
    let before = probe::height("short");
    let region = probe::region_rect("short", 0, 0).expect("the block");
    rig.input.pointer = Some((region.x + 10.0, region.y + 10.0));
    rig.settle(&short);
    assert_eq!(probe::height("short"), before);
    assert!(!rig.animating);
}

#[test]
fn a_wide_code_block_scrolls_sideways_by_its_bar_and_by_the_wheel() {
    let mut rig = Rig::new();
    rig.size = (400.0, 600.0);
    let view = code_view("bar", long_code(2));
    rig.settle(&view);
    let r = probe::region_rect("bar", 0, 0).expect("region");
    let (_, max) = probe::region_scroll("bar", 0, 0).expect("region");
    let start = (r.x + 40.0, r.bottom() - 4.0);
    rig.press(&view, start.0, start.1);
    rig.input.pointer = Some((start.0 + 40.0, start.1));
    rig.frame(&view);
    rig.release(&view, start.0 + 40.0, start.1);
    rig.frame(&view);
    let (scrolled, _) = probe::region_scroll("bar", 0, 0).expect("region");
    assert!(scrolled > 0.0 && scrolled <= max, "{scrolled} of {max}");
    assert!(!has_text_selection(), "the bar is not text");
    rig.input.pointer = Some((r.x + 40.0, r.y + 10.0));
    rig.input.scroll_x = -30.0;
    rig.frame(&view);
    let (wheeled, _) = probe::region_scroll("bar", 0, 0).expect("region");
    assert!((wheeled - (scrolled + 30.0)).abs() < 0.5, "{wheeled} after {scrolled}");
}

#[test]
fn a_wide_table_scrolls_sideways_and_a_narrow_one_stretches() {
    let mut rig = Rig::new();
    rig.size = (300.0, 600.0);
    let source = format!("| {} | {} |\n|---|---|\n| 1 | 2 |", "a very long heading that does not wrap at all", "another very long heading that does not wrap either");
    let view = code_view("table", source);
    rig.settle(&view);
    let (_, max) = probe::region_scroll("table", 0, 0).expect("region");
    assert!(max > 0.0);
    let small = code_view("small", "| a | b |\n|---|---|\n| 1 | 2 |".to_owned());
    rig.settle(&small);
    let r = probe::region_rect("small", 0, 0).expect("region");
    assert_eq!(probe::region_scroll("small", 0, 0).expect("region").1, 0.0);
    assert_eq!(r.w, 300.0);
}

#[test]
fn details_collapse_and_open() {
    let mut rig = Rig::new();
    let options = MarkdownOptions { render_html: true, ..Default::default() };
    let view = md_view("<details><summary>More</summary>\n\nhidden words\n\n</details>", options);
    rig.settle(&view);
    assert_eq!(probe::piece_texts(MD_KEY), ["More"]);
    let closed = probe::height(MD_KEY);
    let toggle = at(probe::toggle_rect(MD_KEY, 0, 0).map(|r| (r.x + 5.0, r.y + 5.0)).expect("toggle"));
    rig.click(&view, toggle.0, toggle.1);
    rig.settle(&view);
    assert_eq!(probe::piece_texts(MD_KEY), ["More", "hidden words"]);
    assert!(probe::height(MD_KEY) > closed);
    rig.click(&view, toggle.0, toggle.1);
    rig.settle(&view);
    assert_eq!(probe::height(MD_KEY), closed);
}

#[test]
fn a_resolved_image_is_drawn_and_a_late_answer_is_noticed() {
    let mut rig = Rig::new();
    let answer: Rc<Cell<Option<MarkdownImage>>> = Rc::new(Cell::new(None));
    let view = {
        let answer = answer.clone();
        move |_: &mut App| {
            let answer = answer.clone();
            let options = MarkdownOptions { resolve_image_src: Some(Rc::new(move |_: &str| answer.get())), ..Default::default() };
            col().w_full().child(markdown_content("img", "![the alt](pic.png)", options))
        }
    };
    rig.settle(&view);
    // Nothing yet: the description stands.
    assert_eq!(probe::piece_texts("img"), ["the alt"]);
    answer.set(Some(MarkdownImage { id: fluent::ImageId(7), size: (200, 100) }));
    rig.settle(&view);
    assert!(probe::piece_texts("img").is_empty());
    assert_eq!(probe::height("img"), 100.0);
    assert_eq!(rig.scene.layers[0].images.len(), 1);
}

// ---------------------------------------------------------------- links

type Calls = Rc<RefCell<Vec<String>>>;

fn link_options(calls: &Calls) -> MarkdownOptions {
    let (a, b, c, d, e) = (calls.clone(), calls.clone(), calls.clone(), calls.clone(), calls.clone());
    MarkdownOptions {
        linkify_paths: true,
        path_base_dir: Some("/w".to_owned()),
        on_open_path: Some(Rc::new(move |_, link: PathLink| a.borrow_mut().push(format!("open {} {:?} {:?}", link.path, link.line, link.base_dir)))),
        on_document_link: Some(Rc::new(move |_, href: &str| b.borrow_mut().push(format!("doc {href}")))),
        on_hover_path: Some(Rc::new(move |_, link: &PathLink| c.borrow_mut().push(format!("hover {}", link.path)))),
        on_open_external: Some(Rc::new(move |_, url: &str| d.borrow_mut().push(format!("web {url}")))),
        on_reveal: Some(Rc::new(move |_, r: Rect| e.borrow_mut().push(format!("reveal {}", r.y.round())))),
        ..Default::default()
    }
}

#[test]
fn link_clicks_call_back() {
    let mut rig = Rig::new();
    let calls: Calls = Rc::default();
    let view = md_view("see `src/a.ts:3` and [web](https://x.y/z) and [none](javascript:x)", link_options(&calls));
    rig.settle(&view);
    let text = &probe::piece_texts(MD_KEY)[0];
    let path = at(probe::point_of(MD_KEY, 0, 0, text.find("src").unwrap() + 1).unwrap());
    rig.click(&view, path.0, path.1);
    let web = at(probe::point_of(MD_KEY, 0, 0, text.find("web").unwrap() + 1).unwrap());
    rig.click(&view, web.0, web.1);
    let none = at(probe::point_of(MD_KEY, 0, 0, text.find("none").unwrap() + 1).unwrap());
    rig.click(&view, none.0, none.1);
    let log = calls.borrow().clone();
    assert!(log.contains(&"open src/a.ts Some(3) Some(\"/w\")".to_owned()), "{log:?}");
    assert!(log.contains(&"web https://x.y/z".to_owned()), "{log:?}");
    assert_eq!(log.iter().filter(|l| !l.starts_with("hover")).count(), 2, "{log:?}");
}

#[test]
fn a_drag_over_a_link_selects_instead_of_following_it() {
    let mut rig = Rig::new();
    let calls: Calls = Rc::default();
    let view = md_view("[a link with words in it](https://x.y/z)", link_options(&calls));
    rig.settle(&view);
    let from = at(probe::point_of(MD_KEY, 0, 0, 2).unwrap());
    let to = at(probe::point_of(MD_KEY, 0, 0, 12).unwrap());
    rig.drag(&view, from, to);
    assert!(has_text_selection());
    assert!(calls.borrow().is_empty());
    clear_text_selection();
}

#[test]
fn a_modifier_click_keeps_its_platform_meaning() {
    let mut rig = Rig::new();
    let calls: Calls = Rc::default();
    let view = md_view("[link](https://x.y/z)", link_options(&calls));
    rig.settle(&view);
    let p = at(probe::point_of(MD_KEY, 0, 0, 2).unwrap());
    rig.input.zoom_modifier = true;
    rig.click(&view, p.0, p.1);
    rig.input.zoom_modifier = false;
    assert!(calls.borrow().is_empty());
}

#[test]
fn the_pointer_resting_on_a_path_asks_once_after_a_dwell() {
    let mut rig = Rig::new();
    let calls: Calls = Rc::default();
    let view = md_view("`src/main.rs` here", link_options(&calls));
    rig.settle(&view);
    let p = at(probe::point_of(MD_KEY, 0, 0, 4).unwrap());
    rig.input.pointer = Some(p);
    rig.frame(&view);
    assert!(calls.borrow().is_empty(), "not on arrival");
    assert!(rig.wake_in.is_some(), "the wait asks to be woken");
    assert_eq!(rig.cursor, fluent::ui::CursorKind::Hand);
    rig.time += 0.1;
    rig.frame(&view);
    rig.frame(&view);
    assert_eq!(*calls.borrow(), ["hover src/main.rs"]);
    // Clicking it finds the answer waiting, and does not ask again.
    rig.click(&view, p.0, p.1);
    assert_eq!(calls.borrow().iter().filter(|c| c.starts_with("hover")).count(), 1);
    assert!(calls.borrow().iter().any(|c| c.starts_with("open src/main.rs")));
}

#[test]
fn a_fragment_finds_an_id_before_a_heading_and_only_in_its_own_message() {
    let mut rig = Rig::new();
    let calls: Calls = Rc::default();
    let options = MarkdownOptions { render_html: true, ..link_options(&calls) };
    let (a, b) = (options.clone(), options);
    let view = move |_: &mut App| {
        col().w_full()
            .child(markdown_content("first", "[go](#fn-1)\n\n[other](#other)\n\n## fn-1\n\nwords\n\n<a name=\"fn-1\"></a>\n\nnote", a.clone()))
            .child(markdown_content("second", "## Other", b.clone()))
    };
    rig.settle(&view);
    let r = rect_of(markdown_key("first")).unwrap();
    let go = probe::point_of("first", 0, 0, 1).unwrap();
    rig.click(&view, r.x + go.0, r.y + go.1);
    let other = probe::point_of("first", 1, 0, 1).unwrap();
    rig.click(&view, r.x + other.0, r.y + other.1);
    // The anchor, not the heading before it, and nothing at all for a heading in another reply.
    let log = calls.borrow().clone();
    assert_eq!(log.len(), 1, "{log:?}");
    let heading = probe::headings("first")[0].1;
    let reveal: f32 = log[0].strip_prefix("reveal ").unwrap().parse().unwrap();
    assert!(reveal > r.y + heading + 10.0, "{reveal} past {heading}");
}

#[test]
fn a_document_link_goes_to_the_caller() {
    let mut rig = Rig::new();
    let calls: Calls = Rc::default();
    let options = MarkdownOptions { document_links: true, ..link_options(&calls) };
    let view = md_view("[下一篇](other.md)", options);
    rig.settle(&view);
    let p = at(probe::point_of(MD_KEY, 0, 0, 3).unwrap());
    rig.click(&view, p.0, p.1);
    assert_eq!(*calls.borrow(), ["doc other.md"]);
}

#[test]
fn a_fragment_finds_the_heading_inside_its_own_message() {
    let mut rig = Rig::new();
    let calls: Calls = Rc::default();
    let view = md_view("[跳到安装](#安装-步骤)\n\n## 安装 步骤\n\n正文", link_options(&calls));
    rig.settle(&view);
    let heading_y = probe::headings(MD_KEY)[0].1 + 10.0;
    let p = at(probe::point_of(MD_KEY, 0, 0, 3).unwrap());
    rig.click(&view, p.0, p.1);
    assert_eq!(*calls.borrow(), [format!("reveal {}", heading_y.round())]);
}

// ---------------------------------------------------------------- streaming reveal

/// How long one commit's text takes to sweep in, in seconds.
const REVEAL: f32 = mewrk_client::runs::stream_reveal::STREAM_REVEAL_MS as f32 / 1000.0;

#[test]
fn sweeps_only_the_newly_arrived_text_and_stops_marking_settled_text() {
    let mut rig = Rig::new();
    let content = Rc::new(RefCell::new("前半段".to_owned()));
    let streaming = Rc::new(Cell::new(true));
    let view = {
        let (content, streaming) = (content.clone(), streaming.clone());
        move |_: &mut App| col().w_full().child(markdown_content("stream", &content.borrow(), MarkdownOptions { streaming: streaming.get(), ..Default::default() }))
    };
    rig.frame(&view);
    // Nothing was on screen yet, so all of it is new. Only its painting moves, so it asks for frames that only paint.
    assert_eq!(probe::fresh_text("stream"), ["前半段"]);
    assert!(rig.repainting && !rig.animating);
    assert!(rig.faded_glyphs() > 0);

    // Only the continuation moves.
    rig.time += REVEAL;
    rig.frame(&view);
    assert!(probe::fresh_text("stream").is_empty());
    assert_eq!(rig.faded_glyphs(), 0);
    *content.borrow_mut() = "前半段，加上后半段".to_owned();
    rig.frame(&view);
    assert_eq!(probe::fresh_text("stream"), ["，加上后半段"]);
    let faded = rig.faded_glyphs();
    assert!(faded > 0 && faded < rig.scene.layers[0].glyphs.len(), "{faded} of {}", rig.scene.layers[0].glyphs.len());

    // The same text again is not new text: the fade in flight is left alone.
    rig.frame(&view);
    assert_eq!(probe::fresh_text("stream"), ["，加上后半段"]);

    // The stream ending leaves the fade in flight to finish rather than snap, and then nothing is marked.
    streaming.set(false);
    rig.frame(&view);
    assert_eq!(probe::fresh_text("stream"), ["，加上后半段"]);
    rig.time += REVEAL;
    rig.frame(&view);
    assert!(probe::fresh_text("stream").is_empty());
    assert_eq!(rig.faded_glyphs(), 0);
    assert!(!rig.animating && !rig.repainting);
}

#[test]
fn the_text_a_stream_ends_with_fades_in_like_every_commit() {
    let mut rig = Rig::new();
    let content = Rc::new(RefCell::new("流到这里".to_owned()));
    let streaming = Rc::new(Cell::new(true));
    let view = {
        let (content, streaming) = (content.clone(), streaming.clone());
        move |_: &mut App| col().w_full().child(markdown_content("ending", &content.borrow(), MarkdownOptions { streaming: streaming.get(), ..Default::default() }))
    };
    rig.frame(&view);
    rig.time += REVEAL;
    rig.frame(&view);
    // The last commit and the end of the stream arrive together.
    streaming.set(false);
    *content.borrow_mut() = "流到这里，最后一段".to_owned();
    rig.frame(&view);
    assert_eq!(probe::fresh_text("ending"), ["，最后一段"]);
    assert!(rig.faded_glyphs() > 0 && rig.repainting);
    rig.time += REVEAL;
    rig.frame(&view);
    assert_eq!(rig.faded_glyphs(), 0);
    // Text that changes after that, an edit say, is not a stream's, and lands at once.
    *content.borrow_mut() = "流到这里，最后一段，改过".to_owned();
    rig.frame(&view);
    assert!(probe::fresh_text("ending").is_empty());
    assert_eq!(rig.faded_glyphs(), 0);
    // Nor does a message that never streamed fade.
    let settled = move |_: &mut App| col().w_full().child(markdown_content("settled", "从来没有流过", MarkdownOptions::default()));
    rig.frame(&settled);
    assert_eq!(rig.faded_glyphs(), 0);
    assert!(!rig.repainting);
}

#[test]
fn sweeps_text_by_what_renders_across_blocks_code_included() {
    let mut rig = Rig::new();
    let content = Rc::new(RefCell::new("先看 **bo".to_owned()));
    let view = {
        let content = content.clone();
        move |_: &mut App| col().w_full().child(markdown_content("across", &content.borrow(), MarkdownOptions { streaming: true, ..Default::default() }))
    };
    rig.frame(&view);
    rig.time += REVEAL;
    rig.frame(&view);
    // `**bo` was literal text; closed, it renders as bold `bo` plus the rest, and it is the rendered text that is new. Code sweeps in
    // too: arriving whole, it would stand a commit ahead of the text before it.
    *content.borrow_mut() = "先看 **bold** 然后\n\n第二段\n\n```ts\nconst x = 1;\n```".to_owned();
    rig.frame(&view);
    assert_eq!(probe::fresh_text("across"), ["bold 然后", "第二段", "const x = 1;"]);
}

/// The boxes a code block paints, as how wide each is: a code block's is the column's width.
fn wide_boxes(rig: &Rig) -> usize {
    rig.scene.layers[0].rects.iter().filter(|r| r.rect[2] > 300.0 && r.rect[3] > 20.0).count()
}

#[test]
fn nothing_drawn_whole_shows_ahead_of_the_sweep() {
    let mut rig = Rig::new();
    let content = Rc::new(RefCell::new("开头".to_owned()));
    let view = {
        let content = content.clone();
        move |_: &mut App| col().w_full().child(markdown_content("hold", &content.borrow(), MarkdownOptions { streaming: true, ..Default::default() }))
    };
    rig.frame(&view);
    rig.time += REVEAL;
    rig.frame(&view);
    assert_eq!(wide_boxes(&rig), 0);
    // One commit: a long paragraph, then a code block. The block's box waits for the paragraph to have swept in.
    let code: String = (0..10).map(|i| format!("let x{i} = {i};\n")).collect();
    *content.borrow_mut() = format!("开头{}\n\n```rust\n{code}```", "，接着是一段很长的文字".repeat(12));
    rig.frame(&view);
    assert_eq!(wide_boxes(&rig), 0, "the code block is not out ahead of the text before it");
    rig.time += REVEAL * 0.3;
    rig.frame(&view);
    assert_eq!(wide_boxes(&rig), 0, "nor a third of the way through");
    // The code's turn comes about three quarters of the way through: its box is there while its text sweeps in.
    rig.time += REVEAL * 0.55;
    rig.frame(&view);
    assert_eq!(wide_boxes(&rig), 1, "the box arrives with its turn");
    assert!(rig.faded_glyphs() > 0, "while the code is still sweeping in");
    rig.time += REVEAL;
    rig.frame(&view);
    assert_eq!(wide_boxes(&rig), 1, "and is there once the sweep has reached it");
    assert_eq!(rig.faded_glyphs(), 0);
}

/// Opacity by where a glyph is, a line at a time and left to right. A glyph's own box starts where its ink does, so its middle says which line it is on.
fn glyph_lines(rig: &Rig) -> Vec<Vec<u8>> {
    let mut glyphs: Vec<(i32, f32, u8)> = rig.scene.layers[0].glyphs.iter().map(|g| (((g.pos[1] + g.size[1] as f32 * 0.5) / 20.0) as i32, g.pos[0], g.color.a)).collect();
    glyphs.sort_by(|a, b| (a.0, a.1).partial_cmp(&(b.0, b.1)).unwrap());
    let mut lines: Vec<(i32, Vec<u8>)> = Vec::new();
    for (line, _, alpha) in glyphs {
        match lines.last_mut() {
            Some((at, shown)) if *at == line => shown.push(alpha),
            _ => lines.push((line, vec![alpha])),
        }
    }
    lines.into_iter().map(|(_, shown)| shown).collect()
}

fn glyph_alphas(rig: &Rig) -> Vec<u8> {
    glyph_lines(rig).concat()
}

#[test]
fn the_sweep_uncovers_new_text_from_its_left_edge_in_one_commit() {
    let mut rig = Rig::new();
    let content = Rc::new(RefCell::new("已在屏幕上".to_owned()));
    let view = {
        let content = content.clone();
        move |_: &mut App| col().w_full().child(markdown_content("sweep", &content.borrow(), MarkdownOptions { streaming: true, ..Default::default() }))
    };
    rig.frame(&view);
    rig.time += REVEAL;
    rig.frame(&view);
    let settled = rig.scene.layers[0].glyphs.len();
    *content.borrow_mut() = "已在屏幕上的文字后面又来了一段新的文字它正在从左向右被写出来".to_owned();
    rig.frame(&view);
    // Halfway through its one commit.
    rig.time += REVEAL / 2.0 - 0.016;
    rig.frame(&view);
    let alphas = glyph_alphas(&rig);
    assert!(alphas[..settled].iter().all(|a| *a >= 250), "what was on screen stays: {alphas:?}");
    let fresh = &alphas[settled..];
    assert!(fresh.first().unwrap() >= &250, "the left of what is new has been uncovered: {fresh:?}");
    assert!(*fresh.last().unwrap() == 0, "its right has not: {fresh:?}");
    assert!(fresh.windows(2).all(|pair| pair[0] >= pair[1]), "the edge runs one way: {fresh:?}");
    assert!(fresh.iter().any(|a| *a > 0 && *a < 250), "and it is a gradient: {fresh:?}");
    // One commit later it has all landed, and nothing asks for frames over it.
    rig.time += REVEAL / 2.0;
    rig.frame(&view);
    assert_eq!(rig.faded_glyphs(), 0);
    assert!(!rig.animating && !rig.repainting);
}

#[test]
fn the_sweep_follows_the_text_onto_the_next_line_and_across_a_paragraph_in_one_stroke() {
    let mut rig = Rig::new();
    let content = Rc::new(RefCell::new(String::new()));
    let view = {
        let content = content.clone();
        move |_: &mut App| col().w_full().child(div().w(200.0).child(markdown_content("stroke", &content.borrow(), MarkdownOptions { streaming: true, ..Default::default() })))
    };
    rig.frame(&view);
    *content.borrow_mut() = format!("{}\n\n{}", "第一段的文字足够长所以会折成好几行才能放得下".repeat(2), "第二段");
    rig.frame(&view);
    // A little way in: the first line is going, and every line after it has yet to start.
    rig.time += REVEAL * 0.2 - 0.016;
    rig.frame(&view);
    let shown = glyph_lines(&rig);
    assert!(shown.len() >= 4, "{shown:?}");
    assert!(shown[0][0] > 0, "{shown:?}");
    assert!(shown[1].iter().all(|a| *a == 0), "the next line waits for the edge to reach it: {shown:?}");
    assert!(shown[shown.len() - 1].iter().all(|a| *a == 0), "and the second paragraph for its turn: {shown:?}");
    // Most of the way: the lines above are done while the last is still to come.
    rig.time += REVEAL * 0.66 - 0.016;
    rig.frame(&view);
    let shown = glyph_lines(&rig);
    assert!(shown[0].iter().all(|a| *a >= 250), "{shown:?}");
    assert!(shown[shown.len() - 1].iter().any(|a| *a < 250), "{shown:?}");
}

/// The bullets a list's items paint, as how opaque each is.
fn bullet_alphas(rig: &Rig) -> Vec<u8> {
    rig.scene.layers[0].rects.iter().filter(|r| (r.rect[2] - 5.0).abs() < 0.01 && (r.rect[3] - 5.0).abs() < 0.01 && r.radii[0] > 2.0).map(|r| r.fill.a).collect()
}

#[test]
fn a_list_items_bullet_waits_for_the_sweep_to_reach_its_text() {
    let mut rig = Rig::new();
    let content = Rc::new(RefCell::new("- 第一项".to_owned()));
    let view = {
        let content = content.clone();
        move |_: &mut App| col().w_full().child(markdown_content("bullets", &content.borrow(), MarkdownOptions { streaming: true, ..Default::default() }))
    };
    rig.frame(&view);
    rig.time += REVEAL;
    rig.frame(&view);
    assert_eq!(bullet_alphas(&rig), [255]);
    // A commit that brings a long stretch of text and then a new item: the new item's turn comes at the end of the sweep.
    *content.borrow_mut() = format!("- 第一项{}
- 第二项", "，还有很长的一段文字".repeat(6));
    rig.frame(&view);
    assert_eq!(bullet_alphas(&rig), [255], "the second bullet is not out ahead of its text");
    rig.time += REVEAL;
    rig.frame(&view);
    assert_eq!(bullet_alphas(&rig), [255, 255], "and is there once its text is");
}

#[test]
fn a_bullet_fades_in_as_its_item_begins() {
    let mut rig = Rig::new();
    let content = Rc::new(RefCell::new("开头".to_owned()));
    let view = {
        let content = content.clone();
        move |_: &mut App| col().w_full().child(markdown_content("bullet-fade", &content.borrow(), MarkdownOptions { streaming: true, ..Default::default() }))
    };
    rig.frame(&view);
    rig.time += REVEAL;
    rig.frame(&view);
    *content.borrow_mut() = "开头\n\n- 新的一项".to_owned();
    rig.frame(&view);
    assert!(bullet_alphas(&rig).is_empty(), "not before its turn");
    rig.frame(&view);
    let alphas = bullet_alphas(&rig);
    assert!(alphas.len() == 1 && alphas[0] > 0 && alphas[0] < 255, "on its way in: {alphas:?}");
    rig.time += REVEAL;
    rig.frame(&view);
    assert_eq!(bullet_alphas(&rig), [255]);
}

#[test]
fn the_commit_that_ends_a_stream_takes_over_the_sweep_in_flight_rather_than_landing_it() {
    let mut rig = Rig::new();
    let content = Rc::new(RefCell::new("开始".to_owned()));
    let streaming = Rc::new(Cell::new(true));
    let view = {
        let (content, streaming) = (content.clone(), streaming.clone());
        move |_: &mut App| col().w_full().child(markdown_content("takeover", &content.borrow(), MarkdownOptions { streaming: streaming.get(), ..Default::default() }))
    };
    rig.frame(&view);
    rig.time += REVEAL;
    rig.frame(&view);
    *content.borrow_mut() = format!("开始{}", "甲".repeat(40));
    rig.frame(&view);
    // Halfway through that sweep the stream ends, off the beat, with a little more.
    rig.time += REVEAL / 2.0 - 0.016;
    streaming.set(false);
    *content.borrow_mut() = format!("开始{}{}", "甲".repeat(40), "乙".repeat(10));
    rig.frame(&view);
    let alphas = glyph_alphas(&rig);
    assert!(alphas[..2].iter().all(|a| *a >= 250), "{alphas:?}");
    assert!(alphas[2 + 10] >= 250, "what the edge had passed stays: {alphas:?}");
    assert!(alphas[2 + 30] < 250, "what it had not reached yet sweeps in again rather than landing: {alphas:?}");
    rig.time += REVEAL;
    rig.frame(&view);
    assert_eq!(rig.faded_glyphs(), 0);
}

#[test]
fn a_later_commit_leaves_what_the_last_swept_in_settled_and_takes_over_what_it_had_not() {
    let mut rig = Rig::new();
    let content = Rc::new(RefCell::new("第一批文字".to_owned()));
    let view = {
        let content = content.clone();
        move |_: &mut App| col().w_full().child(markdown_content("next", &content.borrow(), MarkdownOptions { streaming: true, ..Default::default() }))
    };
    rig.frame(&view);
    assert!(rig.faded_glyphs() > 0);
    let first = rig.scene.layers[0].glyphs.len();
    // On the beat: the first batch has swept in, and only the second moves.
    rig.time += REVEAL;
    *content.borrow_mut() = "第一批文字和第二批文字".to_owned();
    rig.frame(&view);
    assert_eq!(probe::fresh_text("next"), ["和第二批文字"]);
    let alphas = glyph_alphas(&rig);
    assert!(alphas[..first].iter().all(|a| *a >= 250), "{alphas:?}");
    // Ahead of the beat: what the edge had not reached yet sweeps in with the new text instead of landing at once.
    *content.borrow_mut() = "第一批文字和第二批文字，第三批".to_owned();
    rig.frame(&view);
    assert_eq!(probe::fresh_text("next"), ["和第二批文字，第三批"]);
}

/// Flips a process-wide switch, so it runs on its own: `cargo test -p mewrk-ui-common --test suite -- --ignored --exact the_reveal_is_off_with_motion_reduced`.
#[test]
#[ignore = "flips the process-wide reduced-motion switch"]
fn the_reveal_is_off_with_motion_reduced() {
    fluent::set_reduced_motion(true);
    let mut rig = Rig::new();
    let view = move |_: &mut App| col().w_full().child(markdown_content("still", "刚到的文字", MarkdownOptions { streaming: true, ..Default::default() }));
    rig.frame(&view);
    fluent::set_reduced_motion(false);
    assert_eq!(rig.faded_glyphs(), 0);
    assert!(!rig.animating);
}

#[test]
fn nothing_asks_for_frames_when_idle() {
    let mut rig = Rig::new();
    let view = md_view("just text", MarkdownOptions::default());
    rig.settle(&view);
    assert!(!rig.animating);
    assert_eq!(rig.wake_in, None);
}

// ---------------------------------------------------------------- culling and cost

#[test]
fn only_what_is_in_view_is_painted() {
    let mut rig = Rig::new();
    let mut source = String::new();
    for i in 0..2000 {
        source.push_str(&format!("Paragraph {i} has a few words in it.\n\n"));
    }
    let view = move |_: &mut App| col().w_full().h_full().key("scroller").scroll_y().child(markdown_content("cull", &source, MarkdownOptions::default()));
    rig.settle(&view);
    let glyphs = rig.scene.layers[0].glyphs.len();
    // A 600 pixel window holds a few dozen lines of it.
    assert!(glyphs < 2500, "{glyphs}");
    assert!(glyphs > 100, "{glyphs}");
}

/// Not an assertion about a machine's speed, just the numbers: run with `--nocapture`.
#[test]
fn what_a_large_message_costs() {
    use std::time::{Duration, Instant};
    fn average(rig: &mut Rig, view: &dyn Fn(&mut App) -> E, n: u32) -> Duration {
        let started = Instant::now();
        for _ in 0..n {
            rig.frame(view);
        }
        started.elapsed() / n
    }
    let mut rig = Rig::new();
    rig.size = (900.0, 800.0);
    let mut source = String::new();
    let mut i = 0;
    while source.len() < 200_000 {
        match i % 4 {
            0 => source.push_str(&format!("## Section {i}\n\nA paragraph of prose with **bold**, `code` and a [link](https://example.com/{i}) in it, long enough to wrap onto another line of the column it is in.\n\n")),
            1 => source.push_str(&format!("- item {i}\n- another item with `src/lib/file{i}.rs:12` in it\n- third item\n\n")),
            2 => source.push_str(&format!("```rust\nfn function_{i}(a: u32) -> u32 {{\n    a + {i}\n}}\n```\n\n")),
            _ => source.push_str(&format!("| a | b |\n|---|---|\n| {i} | $x^2$ |\n\n")),
        }
        i += 1;
    }
    let options = MarkdownOptions { linkify_paths: true, streaming: true, ..Default::default() };
    let content = Rc::new(RefCell::new(source));
    let view = {
        let content = content.clone();
        let options = options.clone();
        move |_: &mut App| col().w_full().h_full().key("cost-scroller").scroll_y().child(markdown_content("cost", &content.borrow(), options.clone()))
    };
    let empty = |_: &mut App| col().w_full().h_full().key("empty-scroller").scroll_y().child(markdown_content("nothing", "", MarkdownOptions::default()));
    rig.settle(&empty);
    let baseline = average(&mut rig, &empty, 200);

    let started = Instant::now();
    rig.frame(&view);
    let first = started.elapsed();
    rig.settle(&view);
    rig.time += 1.0;
    let top = average(&mut rig, &view, 200);
    let glyphs_top = rig.scene.layers[0].glyphs.len();

    scroll_request("cost-scroller", ScrollRequest::To(40_000.0));
    for _ in 0..60 {
        rig.frame(&view);
    }
    let (offset, height, _) = scroll_state("cost-scroller").expect("it scrolls");
    let middle = average(&mut rig, &view, 200);
    let glyphs_middle = rig.scene.layers[0].glyphs.len();

    let mut commits = Duration::ZERO;
    for n in 0..100 {
        content.borrow_mut().push_str(&format!("more words {n} "));
        let started = Instant::now();
        rig.frame(&view);
        commits += started.elapsed();
    }
    let commit = commits / 100;

    rig.size = (880.0, 800.0);
    let started = Instant::now();
    rig.frame(&view);
    let resize = started.elapsed();
    rig.frame(&view);

    let hash = {
        let started = Instant::now();
        for _ in 0..100 {
            std::hint::black_box(fluent::text::hash_str(std::hint::black_box(&content.borrow())));
        }
        started.elapsed() / 100
    };
    eprintln!(
        "{} bytes, {} blocks, {height:.0} px tall. An empty leaf's frame {baseline:?}. First frame {first:?}. Steady frame at the top {top:?} ({glyphs_top} glyphs), in the middle at {offset:.0} {middle:?} ({glyphs_middle} glyphs). Frame with a new tail {commit:?}. Frame after a width change {resize:?}. Content hash {hash:?}.",
        content.borrow().len(),
        probe::block_count("cost")
    );

    // One fence that long, still arriving.
    let mut code = String::from("```rust\n");
    while code.len() < 200_000 {
        code.push_str("    let value = compute(argument, another_argument) + 1; // something\n");
    }
    let content = Rc::new(RefCell::new(code));
    let view = {
        let content = content.clone();
        move |_: &mut App| col().w_full().h_full().key("fence-scroller").scroll_y().child(markdown_content("fence", &content.borrow(), MarkdownOptions { streaming: true, ..Default::default() }))
    };
    let started = Instant::now();
    rig.frame(&view);
    let first = started.elapsed();
    rig.settle(&view);
    let mut commits = Duration::ZERO;
    let shaped = probe::shaped_count();
    for n in 0..50 {
        content.borrow_mut().push_str(&format!("    let more_{n} = 1;\n"));
        let started = Instant::now();
        rig.frame(&view);
        commits += started.elapsed();
    }
    eprintln!("One 200 KB fence: first frame {first:?}, frame with a new line {:?}, {} runs shaped for 50 commits.", commits / 50, probe::shaped_count() - shaped);
    assert!(probe::block_count("cost") > 100);
}

// ---------------------------------------------------------------- documentLinks.test.ts, externalLinks.test.ts

fn target(path: &str, line: Option<u32>, fragment: Option<&str>) -> Option<LocalLinkTarget> {
    Some(LocalLinkTarget { path: path.to_owned(), line, fragment: fragment.map(str::to_owned) })
}

#[test]
fn reads_the_line_a_link_names_in_each_of_the_forms_tools_write() {
    for href in ["src/App.tsx:42", "src/App.tsx:42:7", "src/App.tsx#L42", "src/App.tsx#L42-L50"] {
        assert_eq!(local_link_target(href), target("src/App.tsx", Some(42), None), "{href}");
    }
    assert_eq!(local_link_target("src/App.tsx"), target("src/App.tsx", None, None));
}

#[test]
fn keeps_a_heading_fragment_apart_from_the_file_it_is_in() {
    assert_eq!(local_link_target("docs/guide.md#install"), target("docs/guide.md", None, Some("install")));
    assert_eq!(local_link_target("#install"), target("", None, Some("install")));
}

#[test]
fn decodes_the_name_and_drops_a_query() {
    assert_eq!(local_link_target("my%20notes.md?plain=1#L3"), target("my notes.md", Some(3), None));
    assert_eq!(local_link_target("100%.md"), target("100%.md", None, None));
}

#[test]
fn unwraps_file_urls_including_a_windows_drive() {
    assert_eq!(local_link_target("file:///home/me/a.rs:3"), target("/home/me/a.rs", Some(3), None));
    assert_eq!(local_link_target("file:///C:/work/a.rs"), target("C:/work/a.rs", None, None));
    assert_eq!(local_link_target("C:\\work\\a.rs:9"), target("C:\\work\\a.rs", Some(9), None));
}

#[test]
fn leaves_the_web_mail_and_data_to_whoever_handles_them() {
    for href in ["https://example.com/a.md", "mailto:a@b.c", "data:text/plain,x", "//cdn.example/a.js", "javascript:alert(1)", "", "#"] {
        assert_eq!(local_link_target(href), None, "{href}");
    }
}

#[test]
fn slugs_headings_the_way_documents_link_them() {
    assert_eq!(heading_slug("  Install & Run: Step 1 "), "install--run-step-1");
    assert_eq!(heading_slug("安装 步骤"), "安装-步骤");
    assert!(slug_matches("Getting Started!", "getting-started"));
    assert!(slug_matches("Install & Run", "install-run"));
    assert!(!slug_matches("Other", "getting-started"));
}

#[test]
fn accepts_absolute_remote_http_addresses() {
    assert_eq!(external_http_url("https://example.com").as_deref(), Some("https://example.com/"));
    assert_eq!(external_http_url("  https://example.com/docs?a=1#b  ").as_deref(), Some("https://example.com/docs?a=1#b"));
    assert_eq!(external_http_url("http://example.com:8443/p").as_deref(), Some("http://example.com:8443/p"));
    assert_eq!(external_http_url("  https://example.com/a b  ").as_deref(), Some("https://example.com/a%20b"));
}

#[test]
fn rejects_every_scheme_other_than_http_and_relative_links_and_empty_input() {
    for candidate in ["javascript:alert(1)", "data:text/html,<script>alert(1)</script>", "file:///C:/Windows/System32/calc.exe", "vbscript:msgbox(1)", "ms-settings:privacy", "mailto:a@b.c", "about:blank", "blob:https://example.com/x", "/settings", "#section", "docs/readme", "", "   ", "https://"] {
        assert_eq!(external_http_url(candidate), None, "{candidate}");
    }
}

// ---------------------------------------------------------------- remarkPathLinks.test.ts

fn paths(value: &str) -> Vec<String> {
    split_path_runs(value).into_iter().filter_map(|r| if let PathRun::Path { target, .. } = r { Some(target) } else { None }).collect()
}

fn rebuilt(value: &str) -> String {
    split_path_runs(value).into_iter().map(|r| match r { PathRun::Text(t) => t, PathRun::Path { display, .. } => display }).collect()
}

#[test]
fn recognizes_absolute_paths_in_prose() {
    assert_eq!(paths("见 /usr/bin/env 就好"), ["/usr/bin/env"]);
    assert_eq!(paths("打开 C:\\Windows\\System32\\drivers\\etc\\hosts"), ["C:\\Windows\\System32\\drivers\\etc\\hosts"]);
    assert_eq!(paths("D:/projects/mewrk/src"), ["D:/projects/mewrk/src"]);
    assert_eq!(paths("先看 /etc/hosts，再看 /var/log/syslog"), ["/etc/hosts", "/var/log/syslog"]);
}

#[test]
fn leaves_prose_that_merely_contains_a_slash_alone() {
    for value in ["and/or", "24/7", "TypeScript 5.0/6.0", "读写/执行权限", "见 /etc 目录", "a/b", "no separators here"] {
        assert!(paths(value).is_empty(), "{value}");
    }
}

#[test]
fn does_not_match_inside_an_address_that_was_not_autolinked() {
    for value in ["ftp://example.com/a/b", "https://example.com/docs/guide", "参见 mailto://x/y/z"] {
        assert!(paths(value).is_empty(), "{value}");
    }
}

#[test]
fn drops_sentence_punctuation_but_keeps_balanced_brackets() {
    assert_eq!(paths("配置在 /etc/hosts。"), ["/etc/hosts"]);
    assert_eq!(paths("配置在 /etc/hosts, 对吧?"), ["/etc/hosts"]);
    assert_eq!(paths("（见 C:\\Program\\a.exe）"), ["C:\\Program\\a.exe"]);
    assert_eq!(paths("见 (/usr/bin/env)"), ["/usr/bin/env"]);
    assert_eq!(paths("C:\\Program_Files_(x86)\\node\\node.exe"), ["C:\\Program_Files_(x86)\\node\\node.exe"]);
}

#[test]
fn shows_a_line_reference_but_does_not_send_it_to_the_host() {
    let runs = split_path_runs("崩在 /usr/lib/app.js:12:3 这里");
    assert!(runs.contains(&PathRun::Path { display: "/usr/lib/app.js:12:3".into(), target: "/usr/lib/app.js".into(), line: Some(12) }));
}

#[test]
fn never_loses_or_duplicates_text() {
    for value in ["见 /usr/bin/env 就好", "配置在 /etc/hosts。后面还有字", "and/or", "/etc/hosts", "先看 /etc/hosts，再看 /var/log/syslog"] {
        assert_eq!(rebuilt(value), value);
    }
}

#[test]
fn accepts_the_relative_paths_models_put_in_backticks() {
    for value in ["src/lib/foo.ts", "./scripts/run.sh", "../sibling/file.md", "a/b/c"] {
        assert_eq!(inline_code_path(value).unwrap().1, value);
    }
    assert_eq!(inline_code_path("C:\\Program Files (x86)\\node\\node.exe").unwrap().1, "C:\\Program Files (x86)\\node\\node.exe");
    assert_eq!(inline_code_path("/usr/local/bin/node").unwrap().1, "/usr/local/bin/node");
}

#[test]
fn splits_a_line_reference_off_the_target() {
    assert_eq!(inline_code_path("src/App.tsx:12"), Some(("src/App.tsx:12".into(), "src/App.tsx".into(), Some(12))));
    assert_eq!(inline_code_path("src/App.tsx:12:5").unwrap().2, Some(12));
    assert_eq!(inline_code_path("src/App.tsx").unwrap().2, None);
}

#[test]
fn rejects_code_spans_that_are_not_paths() {
    for value in ["npm run dev", "and/or", "useMemo", "5.0/6.0", "npm run dev -- src/foo", "ls -la src/lib", "https://example.com/a/b", "//server/share", "\\\\server\\share\\file.txt", "echo \"a/b\"", ""] {
        assert_eq!(inline_code_path(value), None, "{value}");
    }
}

#[test]
fn a_path_gives_way_in_the_middle_a_directory_at_a_time() {
    let path = "/Users/me/Desktop/projects/mewrk";
    let by = |n: usize| move |s: &str| s.chars().count() <= n;
    assert_eq!(elide_path(path, &mut by(40)), path);
    assert_eq!(elide_path(path, &mut by(24)), "/Users/…/projects/mewrk");
    assert_eq!(elide_path(path, &mut by(15)), "/Users/…/mewrk");
    assert_eq!(elide_path(path, &mut by(12)), "/U…s/…/mewrk");
}

#[test]
fn path_text_fits_its_box_and_leaves_a_press_to_its_row() {
    let mut rig = Rig::new();
    let clicks = Rc::new(Cell::new(0));
    let view = {
        let clicks = clicks.clone();
        move |_: &mut App| {
            let clicks = clicks.clone();
            col().w_full().child(row().key("row").w(120.0).h(24.0).on_click(move |_: &mut App| clicks.set(clicks.get() + 1)).child(path_text("p", "/Users/me/Desktop/projects/mewrk/a/very/long/path/indeed.rs", BODY_SMALL, Color::WHITE).flex1()))
        }
    };
    rig.settle(&view);
    let r = rect_of("row").unwrap();
    rig.click(&view, r.x + 10.0, r.y + 10.0);
    assert_eq!(clicks.get(), 1);
    assert!(rig.scene.layers[0].glyphs.iter().all(|g| g.pos[0] < r.right() + 2.0));
    // As wide as the whole path, where the container allows.
    let wide = |_: &mut App| col().w_full().child(div().key("box").child(path_text("q", "/a/b", BODY_SMALL, Color::WHITE)));
    rig.settle(&wide);
    assert_eq!(rect_of("box").unwrap().h, BODY_SMALL.line);
}

// ---------------------------------------------------------------- menuPlacement.test.ts

#[test]
fn hangs_down_from_a_point_in_the_windows_upper_half() {
    assert_eq!(place_menu(200.0, 300.0, 140.0, 160.0, 1000.0, 800.0), MenuPlacement { left: 200.0, top: 300.0, above: false });
}

#[test]
fn stands_on_a_point_in_the_windows_lower_half_though_there_is_room_below() {
    assert_eq!(place_menu(200.0, 500.0, 140.0, 160.0, 1000.0, 800.0), MenuPlacement { left: 200.0, top: 340.0, above: true });
}

#[test]
fn holds_a_menu_too_tall_for_its_side_inside_the_window() {
    assert_eq!(place_menu(200.0, 500.0, 140.0, 600.0, 1000.0, 800.0), MenuPlacement { left: 200.0, top: 8.0, above: true });
    assert_eq!(place_menu(200.0, 300.0, 140.0, 600.0, 1000.0, 800.0), MenuPlacement { left: 200.0, top: 192.0, above: false });
}

#[test]
fn opens_to_the_left_of_a_point_the_right_has_no_room_past() {
    assert_eq!(place_menu(950.0, 100.0, 140.0, 160.0, 1000.0, 800.0), MenuPlacement { left: 810.0, top: 100.0, above: false });
}

#[test]
fn keeps_a_box_anchors_edge_and_gap() {
    let r = fluent::ui::rect(600.0, 600.0, 40.0, 20.0);
    assert_eq!(place_menu_at(ContextMenuAnchor::Box { rect: r, align_end: true }, 4.0, 180.0, 100.0, 1000.0, 800.0), MenuPlacement { left: 460.0, top: 496.0, above: true });
    let r = fluent::ui::rect(600.0, 100.0, 40.0, 20.0);
    assert_eq!(place_menu_at(ContextMenuAnchor::Box { rect: r, align_end: false }, 4.0, 180.0, 100.0, 1000.0, 800.0), MenuPlacement { left: 600.0, top: 124.0, above: false });
}

#[test]
fn a_submenu_goes_to_the_other_side_rather_than_leave_the_window() {
    let parent = fluent::ui::rect(100.0, 100.0, 200.0, 300.0);
    let row = fluent::ui::rect(103.0, 130.0, 194.0, 24.0);
    // Room on the right: its edge lies over the parent's, and its first row lines up with its row.
    assert_eq!(place_submenu(row, parent, 150.0, 100.0, 4.0, MenuHang::Down, 1000.0, 800.0), (299.0, 126.0));
    // None: over to the left.
    let tight = fluent::ui::rect(700.0, 100.0, 200.0, 300.0);
    assert_eq!(place_submenu(row, tight, 150.0, 100.0, 4.0, MenuHang::Down, 1000.0, 800.0), (551.0, 126.0));
    // Growing up from the row's foot, and pushed back in from the bottom.
    assert_eq!(place_submenu(row, parent, 150.0, 100.0, 4.0, MenuHang::Up, 1000.0, 800.0).1, 58.0);
    assert_eq!(place_submenu(fluent::ui::rect(0.0, 780.0, 10.0, 10.0), parent, 150.0, 100.0, 4.0, MenuHang::Down, 1000.0, 800.0).1, 692.0);
}

// ---------------------------------------------------------------- ContextMenu.test.tsx

struct MenuProbe {
    open: Cell<bool>,
    closed: Cell<u32>,
    chosen: Cell<u32>,
    loads: Cell<u32>,
}

impl MenuProbe {
    fn new() -> Rc<Self> {
        Rc::new(Self { open: Cell::new(true), closed: Cell::new(0), chosen: Cell::new(0), loads: Cell::new(0) })
    }
}

const MENU: &str = "menu";

fn file_menu(probe: &Rc<MenuProbe>, sections: Vec<ContextMenuSection>) -> E {
    let p = probe.clone();
    context_menu(MENU, ContextMenuProps::new(ContextMenuAnchor::Point { x: 10.0, y: 10.0 }, "File actions", move |_: &mut App| {
        p.closed.set(p.closed.get() + 1);
        p.open.set(false);
    }, sections))
}

fn point_on(key: u64) -> (f32, f32) {
    let r = rect_of(key).expect("on screen");
    (r.x + r.w * 0.5, r.y + r.h * 0.5)
}

#[test]
fn opens_a_submenu_as_a_panel_of_its_own_read_from_the_moment_its_row_is_reached() {
    let mut rig = Rig::new();
    let probe = MenuProbe::new();
    let answer: Rc<RefCell<Option<MenuLoader>>> = Rc::default();
    let view = {
        let (probe, answer) = (probe.clone(), answer.clone());
        move |_: &mut App| {
            let (p, a, chosen) = (probe.clone(), answer.clone(), probe.clone());
            let _ = chosen;
            div::<App>().when(probe.open.get(), move |e| {
                let (p2, a2) = (p.clone(), a.clone());
                e.child(file_menu(&p, vec![ContextMenuSection::new("file", vec![ContextMenuItem::new("open-with", "Open With").load_children(move |_: &mut App, loader| {
                    p2.loads.set(p2.loads.get() + 1);
                    *a2.borrow_mut() = Some(loader);
                })])]))
            })
        }
    };
    rig.settle(&view);
    let row = menu_row_key(MENU, &[], "open-with");
    let at_row = point_on(row);

    // Reaching the row starts reading it.
    rig.input.pointer = Some(at_row);
    rig.frame(&view);
    assert_eq!(probe.loads.get(), 1);
    rig.click(&view, at_row.0, at_row.1);
    rig.settle(&view);
    let chosen = probe.clone();
    answer.borrow().as_ref().expect("asked").resolve(vec![ContextMenuSection::new("apps", vec![ContextMenuItem::new("editor", "Editor").on_select(move |_: &mut App| chosen.chosen.set(chosen.chosen.get() + 1))])]);
    rig.settle(&view);

    // A panel of its own, beside the menu.
    let menu_panel = rect_of(menu_panel_key(MENU, &[])).expect("the menu");
    let submenu = rect_of(menu_panel_key(MENU, &["open-with"])).expect("the submenu");
    assert!((submenu.x - (menu_panel.right() - 1.0)).abs() < 1.0, "{submenu:?} beside {menu_panel:?}");

    // A press in the submenu is a press in the menu, not outside it.
    let editor = point_on(menu_row_key(MENU, &["open-with"], "editor"));
    rig.press(&view, editor.0, editor.1);
    assert_eq!(probe.closed.get(), 0);
    rig.release(&view, editor.0, editor.1);
    assert_eq!(probe.chosen.get(), 1);
    assert_eq!(probe.closed.get(), 1);
    assert_eq!(probe.loads.get(), 1);
}

#[test]
fn says_when_a_submenu_is_still_being_read_or_failed() {
    let mut rig = Rig::new();
    let probe = MenuProbe::new();
    let answer: Rc<RefCell<Option<MenuLoader>>> = Rc::default();
    let view = {
        let (probe, answer) = (probe.clone(), answer.clone());
        move |_: &mut App| {
            let a = answer.clone();
            div::<App>().child(file_menu(&probe, vec![ContextMenuSection::new("file", vec![ContextMenuItem::new("open-with", "Open With").load_children(move |_: &mut App, loader| *a.borrow_mut() = Some(loader))])]))
        }
    };
    rig.settle(&view);
    let at_row = point_on(menu_row_key(MENU, &[], "open-with"));
    rig.input.pointer = Some(at_row);
    rig.frame(&view);
    // The pointer rests: after the delay it opens by itself, and says it is reading.
    for _ in 0..12 {
        rig.frame(&view);
    }
    rig.settle(&view);
    assert!(rect_of(menu_panel_key(MENU, &["open-with"])).is_some(), "opened on hover");
    assert!(rig.animating, "the spinner turns");
    answer.borrow().as_ref().expect("asked").reject("no such program");
    rig.settle(&view);
    let panel = rect_of(menu_panel_key(MENU, &["open-with"])).expect("still there");
    assert!(panel.h > 20.0);
}

#[test]
fn takes_the_keyboard_once_it_is_placed_and_gives_focus_back_when_it_closes() {
    let mut rig = Rig::new();
    let probe = MenuProbe::new();
    let view = {
        let probe = probe.clone();
        move |_: &mut App| {
            let p = probe.clone();
            div::<App>().child(div().key("opener").focusable().w(50.0).h(20.0)).when(probe.open.get(), move |e| e.child(file_menu(&p, vec![ContextMenuSection::new("file", vec![ContextMenuItem::new("open", "Open"), ContextMenuItem::new("gone", "Gone").disabled(true).title("Not now"), ContextMenuItem::new("rename", "Rename")])])))
        }
    };
    let opener = hash_key("opener");
    probe.open.set(false);
    rig.settle(&view);
    rig.mem.focus = opener;
    rig.frame(&view);
    probe.open.set(true);
    rig.settle(&view);

    assert_eq!(focused_id(), hash_key(menu_row_key(MENU, &[], "open")));
    rig.key(&view, Key::Down, false);
    rig.frame(&view);
    // The disabled row is skipped.
    assert_eq!(focused_id(), hash_key(menu_row_key(MENU, &[], "rename")));
    rig.key(&view, Key::Down, false);
    rig.frame(&view);
    assert_eq!(focused_id(), hash_key(menu_row_key(MENU, &[], "open")), "wraps round");
    rig.key(&view, Key::End, false);
    rig.frame(&view);
    assert_eq!(focused_id(), hash_key(menu_row_key(MENU, &[], "rename")));
    rig.key(&view, Key::Escape, false);
    assert_eq!(probe.closed.get(), 1);
    rig.settle(&view);
    assert_eq!(rig.mem.focus, opener);
}

#[test]
fn leaves_focus_where_the_chosen_row_put_it() {
    let mut rig = Rig::new();
    let probe = MenuProbe::new();
    let view = {
        let probe = probe.clone();
        move |_: &mut App| {
            let p = probe.clone();
            div::<App>().child(div().key("opener").focusable().w(50.0).h(20.0)).child(div().key("field").focusable().w(50.0).h(20.0)).when(probe.open.get(), move |e| e.child(file_menu(&p, vec![ContextMenuSection::new("file", vec![ContextMenuItem::new("rename", "Rename").on_select(|_: &mut App| focus_request("field"))])])))
        }
    };
    rig.mem.focus = hash_key("opener");
    rig.settle(&view);
    let at_row = point_on(menu_row_key(MENU, &[], "rename"));
    rig.click(&view, at_row.0, at_row.1);
    rig.settle(&view);
    assert!(!probe.open.get());
    assert_eq!(rig.mem.focus, hash_key("field"));
}

#[test]
fn keys_open_and_close_a_submenu_and_a_press_outside_closes_everything() {
    let mut rig = Rig::new();
    let probe = MenuProbe::new();
    let view = {
        let probe = probe.clone();
        move |_: &mut App| {
            let p = probe.clone();
            let c = probe.clone();
            div::<App>().when(probe.open.get(), move |e| e.child(file_menu(&p, vec![ContextMenuSection::new("file", vec![ContextMenuItem::new("more", "More").children(vec![ContextMenuSection::new("sub", vec![ContextMenuItem::new("deep", "Deep").on_select(move |_: &mut App| c.chosen.set(c.chosen.get() + 1))])])])])))
        }
    };
    rig.settle(&view);
    rig.key(&view, Key::Right, false);
    rig.settle(&view);
    let deep = hash_key(menu_row_key(MENU, &["more"], "deep"));
    assert_eq!(focused_id(), deep, "opened from the keyboard, it takes focus");
    rig.key(&view, Key::Left, false);
    rig.settle(&view);
    assert!(rect_of(menu_panel_key(MENU, &["more"])).is_none());
    assert_eq!(focused_id(), hash_key(menu_row_key(MENU, &[], "more")), "back to the row it came from");
    assert_eq!(probe.closed.get(), 0);
    rig.key(&view, Key::Enter, false);
    rig.settle(&view);
    assert!(rect_of(menu_panel_key(MENU, &["more"])).is_some());
    rig.click(&view, 700.0, 500.0);
    assert_eq!(probe.closed.get(), 1);
}

#[test]
fn a_disabled_row_says_why_and_does_nothing_and_a_hint_and_description_show() {
    let mut rig = Rig::new();
    let probe = MenuProbe::new();
    let hit = Rc::new(Cell::new(0));
    let view = {
        let (probe, hit) = (probe.clone(), hit.clone());
        move |_: &mut App| {
            let hit = hit.clone();
            div::<App>().child(file_menu(&probe, vec![ContextMenuSection::new("a", vec![ContextMenuItem::new("no", "No").disabled(true).title("Not now").hint("⌘N").on_select(move |_: &mut App| hit.set(hit.get() + 1))]).label("Heading"), ContextMenuSection::new("b", vec![ContextMenuItem::new("path", "/Users/me/projects/mewrk").label_is_path().description("Somewhere").danger()])]))
        }
    };
    rig.settle(&view);
    let at_row = point_on(menu_row_key(MENU, &[], "no"));
    rig.click(&view, at_row.0, at_row.1);
    assert_eq!(hit.get(), 0);
    assert_eq!(probe.closed.get(), 0);
    let panel = rect_of(menu_panel_key(MENU, &[])).unwrap();
    assert!(panel.w >= 180.0);
}

#[test]
fn a_menu_is_placed_from_its_measured_size_and_held_inside_the_window() {
    let mut rig = Rig::new();
    let probe = MenuProbe::new();
    let view = {
        let probe = probe.clone();
        move |_: &mut App| {
            let p = probe.clone();
            div::<App>().child(context_menu(MENU, ContextMenuProps::new(ContextMenuAnchor::Point { x: 790.0, y: 590.0 }, "m", move |_: &mut App| p.closed.set(1), vec![ContextMenuSection::new("s", vec![ContextMenuItem::new("a", "A row"), ContextMenuItem::new("b", "B row")])])))
        }
    };
    rig.settle(&view);
    let panel = rect_of(menu_panel_key(MENU, &[])).unwrap();
    // The lower half: it stands on the point, and to its left because the right has no room.
    assert!((panel.bottom() - 590.0).abs() < 1.0, "{panel:?}");
    assert!((panel.right() - 790.0).abs() < 1.0, "{panel:?}");
}

#[test]
fn a_placeholder_stands_in_while_there_is_nothing_to_list() {
    let mut rig = Rig::new();
    let probe = MenuProbe::new();
    let view = {
        let probe = probe.clone();
        move |_: &mut App| {
            let p = probe.clone();
            div::<App>().child(context_menu(MENU, ContextMenuProps::new(ContextMenuAnchor::Point { x: 10.0, y: 10.0 }, "m", move |_: &mut App| p.closed.set(p.closed.get() + 1), Vec::new()).placeholder(text("Looking…").key("holder"))))
        }
    };
    rig.settle(&view);
    assert!(rect_of("holder").is_some());
    rig.key(&view, Key::Escape, false);
    assert_eq!(probe.closed.get(), 1);
}

// ---------------------------------------------------------------- a look at it

/// Not a test of anything: `MD_SHOT=/some/dir cargo test -p mewrk-ui-common visual_math -- --ignored` gets pictures of formulas,
/// dark and light, at both scales a screen has.
#[test]
#[ignore]
fn visual_math() {
    let dir = std::env::var("MD_SHOT").expect("where to put the pictures");
    let pages: [(&str, f32, &str); 4] = [
        ("flow", 1000.0, MATH_FLOW),
        ("display", 1500.0, MATH_DISPLAY),
        ("structures", 1500.0, MATH_STRUCTURES),
        ("edges", 1000.0, MATH_EDGES),
    ];
    let wanted = std::env::var("MD_PAGE").ok();
    for (name, height, source) in pages {
        if wanted.as_deref().is_some_and(|w| w != name) {
            continue;
        }
        for (theme_name, theme, scale) in [("dark", Theme::dark(), 2.0), ("light", Theme::light(), 2.0), ("dark1x", Theme::dark(), 1.0), ("noface", Theme::dark(), 2.0)] {
            let mut rig = Rig::new();
            rig.size = (760.0, height);
            rig.scale = scale;
            rig.theme = theme;
            rig.text = TextCtx::new(scale);
            if theme_name == "noface" {
                rig.text.forget_math_face();
            }
            let view = move |_: &mut App| col().w_full().h_full().bg(theme.layer).p(20.0).child(markdown_content(("math", name), source, MarkdownOptions::default()));
            rig.settle(&view);
            rig.shot(&format!("{dir}/math-{name}-{theme_name}.png"));
        }
    }
}

const MATH_FLOW: &str = r#"Inline formulas sit on the line: the energy $E=mc^2$, the identity $e^{i\pi}+1=0$, and the sum of $a_1, a_2, \dots, a_n$ all keep to the baseline of the words around them, while a fraction such as $\frac{a+b}{c}$ or $\tfrac{1}{2}$ grows its line only as far as it has to. A long formula wraps as one item: $\sum_{i=1}^{n} x_i^2 + \int_0^1 f(x)\,dx$, and punctuation stays with it.

中文段落里的公式：设 $f:\mathbb{R}\to\mathbb{R}$ 连续，且 $\lim_{x\to 0}\frac{\sin x}{x}=1$，则对任意 $\varepsilon>0$ 存在 $\delta>0$。这一行的行高应当只在需要时变大。

- A list item with $\alpha+\beta=\gamma$ and $\sqrt{x^2+y^2}$.
- Another with **bold $x_i^{2}$ around it** and a link [to $\pi r^2$](https://example.com).

> A quote keeps its colour: $\nabla\cdot\vec{E}=\frac{\rho}{\varepsilon_0}$.

## A heading with $\hat{y}=X\beta$ in it

| Quantity | Formula |
|----------|---------|
| Mean | $\bar{x}=\frac{1}{n}\sum x_i$ |
| Variance | $\sigma^2=\mathbb{E}[(X-\mu)^2]$ |

Display math is its own block:

$$\sum_{i=1}^{n} i=\frac{n(n+1)}{2}$$

And an error, $\frac{a}{$ and an unknown command $\foo{x}+1$, in the flow of the text.
"#;

const MATH_DISPLAY: &str = r#"$$\int_{-\infty}^{\infty} e^{-x^2}\,dx=\sqrt{\pi}$$

$$x=\frac{-b\pm\sqrt{b^2-4ac}}{2a}$$

$$\frac{\partial \mathcal{L}}{\partial \theta}=\frac{1}{n}\sum_{i=1}^{n}\left(y_i-\hat{y}_i\right)\frac{\partial \hat{y}_i}{\partial \theta}$$

$$\binom{n}{k}=\frac{n!}{k!(n-k)!},\qquad \dbinom{n}{2},\quad \tbinom{n}{2}$$

$$\sqrt[3]{x+1}+\sqrt{\frac{a}{b}}+\sqrt{\sum_{k=1}^{n}k^2}$$

$$\lim_{n\to\infty}\left(1+\frac{1}{n}\right)^{n}=e,\quad \max_{x\in\mathcal{X}} f(x),\quad \prod_{p\ \text{prime}}\frac{1}{1-p^{-s}}=\zeta(s)$$

$$\bigcup_{i=1}^{\infty}A_i,\ \bigcap_{j}B_j,\ \bigoplus_{k}V_k,\ \iint_{D}f\,dA,\ \oint_{\partial S}\mathbf{F}\cdot d\mathbf{r}$$

$$\left[\frac{a}{b}\right]\left\{\frac{a}{b}\right\}\left|\frac{a}{b}\right|\left\|\frac{a}{b}\right\|\left\langle\frac{a}{b}\right\rangle\left\lfloor\frac{a}{b}\right\rfloor\left\lceil\frac{a}{b}\right\rceil\left(\frac{\frac{a}{b}}{\frac{c}{d}}\right)$$

$$\bigl(x\bigr)\Bigl(x\Bigr)\biggl(x\biggr)\Biggl(x\Biggr)\quad\bigl[x\bigr]\Bigl[x\Bigr]\biggl[x\biggr]\Biggl[x\Biggr]\quad\bigl\{x\bigr\}\Bigl\{x\Bigr\}\biggl\{x\biggr\}\Biggl\{x\Biggr\}$$

$$\hat{x}\ \bar{x}\ \vec{x}\ \tilde{x}\ \dot{x}\ \ddot{x}\ \check{x}\ \breve{x}\ \acute{x}\ \grave{x}\ \mathring{x}\quad\overline{abc}\ \underline{abc}\ \widehat{abc}\ \widetilde{abc}\ \overrightarrow{AB}\ \overleftarrow{AB}$$

$$\underbrace{a+b+\cdots+z}_{26\text{ letters}}\quad\overbrace{x+y}^{\text{sum}}\quad x'\ y''\ f'(x)\ g'_n\ x^{2^{2^2}}\ x_{i_{j_k}}\ {}^{14}_{6}\mathrm{C}$$

$$\mathbf{ABCabc123}\ \mathrm{ABCabc}\ \mathbb{RNZQC}\ \mathcal{ABCLM}\ \mathsf{Sans}\ \mathtt{Mono}\ \mathfrak{ABCg}\ \boldsymbol{\alpha x+1}\ \text{plain text}$$

$$\alpha\beta\gamma\delta\epsilon\varepsilon\zeta\eta\theta\vartheta\iota\kappa\lambda\mu\nu\xi\pi\varpi\rho\varrho\sigma\varsigma\tau\upsilon\phi\varphi\chi\psi\omega\ \Gamma\Delta\Theta\Lambda\Xi\Pi\Sigma\Upsilon\Phi\Psi\Omega$$

$$a\le b\ge c\ne d\approx e\equiv f\sim g\propto h\in A\subset B\subseteq C\cup D\cap E\to F\Rightarrow G\iff H\not\in I\not= J$$

$$\forall x\exists y\ \neg p\land q\lor r\quad \infty\ \partial\ \nabla\ \hbar\ \ell\ \aleph\ \angle\ \perp\ \parallel\ \pm\ \mp\ \times\ \div\ \cdot\ \circ\ \oplus\ \otimes$$

$$\textcolor{red}{x+y}+{\color{teal}z}+\colorbox{yellow}{box}+\boxed{E=mc^2}+\cancel{x}+\xrightarrow[below]{above}+\overset{?}{=}+\underset{n}{\sim}$$

$$\operatorname{argmax}_{\theta}L(\theta)\quad\operatorname*{argmin}_{\theta}L(\theta)\quad\sin^2\theta+\cos^2\theta=1\quad\log_2 n\quad a\bmod b\quad a\equiv b\pmod{n}$$

$$a\,b\:c\;d\quad e\qquad f\!g\hspace{1em}h\enspace i$$

$$\ce{2H2 + O2 -> 2H2O}\quad\ce{SO4^2-}\quad\ket{\psi}\ \bra{\phi}\ \braket{\phi|\psi}\ \abs{x}\ \norm{v}\ \dv{f}{x}\ \pdv{f}{x}$$
"#;

const MATH_STRUCTURES: &str = r#"$$\begin{pmatrix}a&b\\c&d\end{pmatrix}\begin{bmatrix}1&0&0\\0&1&0\\0&0&1\end{bmatrix}\begin{vmatrix}a&b\\c&d\end{vmatrix}\begin{Vmatrix}x\\y\end{Vmatrix}\begin{Bmatrix}x\\y\end{Bmatrix}\begin{matrix}a&b\\c&d\end{matrix}$$

$$f(x)=\begin{cases}x^2&\text{if }x\ge 0\\-x&\text{otherwise}\end{cases}$$

$$\begin{aligned}(a+b)^2&=a^2+2ab+b^2\\&=a^2+b^2+2ab\\f(x)&=\int_0^x g(t)\,dt\end{aligned}$$

$$\begin{align}a&=b+c\\d&=e+f&g&=h\end{align}$$

$$\begin{gather}a+b=c\\x=\frac{y}{z}\end{gather}$$

$$\begin{array}{l|c|r}\text{left}&\text{mid}&\text{right}\\\hline 1&2&3\\10&20&30\end{array}\qquad\begin{array}{|c|c|}\hline a&b\\\hline c&d\\\hline\end{array}$$

$$\sum_{\substack{0\le i\le m\\0<j<n}}P(i,j)\qquad\left(\begin{smallmatrix}a&b\\c&d\end{smallmatrix}\right)$$

$$\frac{1}{1+\frac{1}{1+\frac{1}{1+x}}}\qquad\cfrac{1}{1+\cfrac{1}{1+x}}\qquad{n\choose k}\qquad{a\over b}$$

$$\left.\frac{df}{dx}\right|_{x=0}\qquad\left\{x\in\mathbb{R}\,\middle|\,x^2<1\right\}$$

$$a=b\\c=d+e$$

$$\mathbf{A}=\begin{pmatrix}\frac{a}{b}&\sqrt{c}\\\sum_{i}x_i&\int_0^1 f\end{pmatrix},\quad\det(\mathbf{A})\ne 0\tag{1}$$

A formula wider than the column scrolls sideways inside itself:

$$\sum_{i=1}^{n}\sum_{j=1}^{m}\sum_{k=1}^{l}a_{ijk}x_iy_jz_k+\sum_{i=1}^{n}\sum_{j=1}^{m}b_{ij}x_iy_j+\sum_{i=1}^{n}c_ix_i+d+\sum_{i=1}^{n}\sum_{j=1}^{m}\sum_{k=1}^{l}e_{ijk}x_iy_jz_k+\sum_{i=1}^{n}\sum_{j=1}^{m}f_{ij}x_iy_j$$

And the end of the message.
"#;

const MATH_EDGES: &str = r#"Unknown command, red, the rest goes on:

$$\foo{x}+\frac{a}{b}$$

Will not parse:

$$\frac{a}{$$

$$\left(\frac{a}{b}$$

$$x^2^3$$

$$\begin{unknown}a\end{unknown}$$

Text style: $\displaystyle\sum_{i=1}^n$ against $\sum_{i=1}^n$ and $\int_0^1$ against $\displaystyle\int_0^1$; $\begin{pmatrix}a\\b\end{pmatrix}$ inline; and a tall one in the flow $\frac{\frac{a}{b}}{\frac{c}{d}}$ right in a sentence that goes on long enough to make it clear what it does to the line above and to the line below.

A bold sentence **with $x^2$ in it**, and a price of \$5 that stays text.
"#;

/// Not a test of anything: run by hand with `MD_SHOT=/some/dir` and `--ignored` to get pictures.
#[test]
#[ignore]
fn visual_sample() {
    let dir = std::env::var("MD_SHOT").expect("where to put the pictures");
    let mut rig = Rig::new();
    rig.size = (760.0, 1500.0);
    let source = r#"# A heading of the first level

Some prose with **bold**, *italic*, ~~struck~~, `inline code`, a [link](https://example.com), a path `src/lib/main.rs:12` and /usr/local/bin/node in it, long enough that it has to wrap at least once in this column.
A soft break above this line. 中文段落也应该正确换行，并且可以和 English 混排在一起。

## Second level

### Third level

- a bullet
- another with nested
  - nested item
    - deeper item
- [x] done task
- [ ] open task

1. first
2. second

   a loose paragraph
3. third

- [x] a task list
- [ ] with nothing else in it

> A quote with `code` and a second line
> of text in it.
>
> - and a list

---

| Column | Right | Centre |
|--------|------:|:------:|
| one | 1 | a |
| two **bold** | 22 | bb |
| three | 333 | ccc |

```rust
// A comment
fn main() {
    let answer = 42;
    println!("{answer}, and a very long line that does not fit in the width of this column so it must scroll sideways");
}
```

Inline math $E=mc^2$ and $\alpha + \beta$, and display:

$$\sum_{i=1}^{n} i=\frac{n(n+1)}{2}$$

<kbd>Cmd</kbd>+<kbd>C</kbd>, <mark>marked</mark>, H<sub>2</sub>O, x<sup>2</sup>.

<details><summary>Closed disclosure</summary>

Hidden.

</details>

<details open><summary>Open disclosure</summary>

Shown.

</details>

<p align="center">Centred by HTML, across a line or two of text to see that the second line is centred as well as the first one.</p>

![a picture that is not there](pic.png)
"#;
    let view = move |_: &mut App| col().w_full().h_full().bg(Theme::dark().layer).p(16.0).child(markdown_content("sample", source, MarkdownOptions { linkify_paths: true, render_html: true, ..Default::default() }));
    rig.settle(&view);
    rig.shot(&format!("{dir}/markdown.png"));
    // The pointer on the code block, and a selection across blocks.
    let r = rect_of(markdown_key("sample")).unwrap();
    rig.drag(&view, (r.x + 200.0, r.y + 60.0), (r.x + 200.0, r.y + 620.0));
    let code = (0..40).find_map(|block| probe::copy_button("sample", block, 0).and_then(|_| probe::region_rect("sample", block, 0))).expect("the code block");
    rig.input.pointer = Some((r.x + code.x + 300.0, r.y + code.y + 8.0));
    rig.frame(&view);
    rig.frame(&view);
    rig.shot(&format!("{dir}/selected.png"));
    clear_text_selection();

    let mut rig = Rig::new();
    rig.size = (500.0, 400.0);
    let probe_menu = MenuProbe::new();
    let view = move |_: &mut App| {
        let p = probe_menu.clone();
        col().w_full().h_full().bg(Theme::dark().layer).child(context_menu(MENU, ContextMenuProps::new(ContextMenuAnchor::Point { x: 40.0, y: 40.0 }, "File", move |_: &mut App| p.closed.set(1), vec![
            ContextMenuSection::new("a", vec![
                ContextMenuItem::new("open", "Open").icon("Folder").hint("⌘O"),
                ContextMenuItem::new("with", "Open With").children(vec![ContextMenuSection::new("apps", vec![ContextMenuItem::new("editor", "Editor"), ContextMenuItem::new("viewer", "Viewer").disabled(true)])]),
                ContextMenuItem::new("off", "Not available").disabled(true).title("Why not"),
            ]).label("Actions"),
            ContextMenuSection::new("b", vec![
                ContextMenuItem::new("path", "/Users/me/Desktop/projects/mewrk/src/components/Something.tsx").label_is_path().description("A workspace"),
                ContextMenuItem::new("rm", "Delete").danger().icon("Trash2"),
            ]),
        ])))
    };
    rig.settle(&view);
    let row = rect_of(menu_row_key(MENU, &[], "with")).unwrap();
    rig.click(&view, row.x + 20.0, row.y + 10.0);
    rig.settle(&view);
    rig.shot(&format!("{dir}/menu.png"));
}

// ---------------------------------------------------------------- accessibility

/// The whole tree as the frame left it, with a client that has just arrived.
fn a11y_tree(rig: &mut Rig, view: &dyn Fn(&mut App) -> E) -> Vec<(accesskit::NodeId, accesskit::Node)> {
    set_a11y_active(true);
    rig.settle(view);
    take_a11y_update().expect("a client gets the tree").nodes
}

fn a11y_named<'a>(tree: &'a [(accesskit::NodeId, accesskit::Node)], name: &str) -> &'a accesskit::Node {
    tree.iter().map(|(_, node)| node).find(|node| node.label() == Some(name) || (node.role() == Role::Label && node.value() == Some(name))).unwrap_or_else(|| panic!("nothing says {name:?}"))
}

fn a11y_action_on(rig: &mut Rig, view: &dyn Fn(&mut App) -> E, id: u64, action: accesskit::Action) {
    a11y_action(accesskit::ActionRequest { action, target_tree: accesskit::TreeId::ROOT, target_node: accesskit::NodeId(id), data: None });
    rig.settle(view);
}

#[test]
fn a_popover_menu_reads_as_a_menu_and_its_rows_reach_their_handlers() {
    use mewrk_ui_common::popover_menu::{trigger_key, CheckedRole, PopoverMenu, PopoverMenuItem, PopoverMenuSection};
    let picked = Rc::new(Cell::new(0));
    let view = {
        let picked = picked.clone();
        move |_: &mut App| -> E {
            let picked = picked.clone();
            div::<App>().child(
                PopoverMenu::new("a11y-menu", "Model", |_| text("gpt"))
                    .menu_label("Models")
                    .sections(move || {
                        vec![PopoverMenuSection::new("all").items([
                            PopoverMenuItem::new("one", "First").checked(true).on(move |_: &mut App| picked.set(1)),
                            PopoverMenuItem::new("two", "Second").checkbox(false),
                            PopoverMenuItem::new("three", "Third").disabled(true),
                            PopoverMenuItem::new("four", "Fourth").description("With more to say"),
                        ])]
                    })
                    .build(),
            )
        }
    };
    let mut rig = Rig::new();
    let tree = a11y_tree(&mut rig, &view);
    let trigger = a11y_named(&tree, "Model");
    assert_eq!((trigger.role(), trigger.is_expanded(), trigger.has_popup()), (Role::Button, Some(false), Some(HasPopup::Menu)));
    let _ = CheckedRole::Radio;

    a11y_action_on(&mut rig, &view, hash_key(trigger_key("a11y-menu")), accesskit::Action::Click);
    rig.settle(&view);
    let tree = a11y_tree(&mut rig, &view);
    assert_eq!(a11y_named(&tree, "Model").is_expanded(), Some(true));
    assert_eq!(a11y_named(&tree, "Models").role(), Role::Menu);
    let first = a11y_named(&tree, "First");
    assert_eq!((first.role(), first.toggled()), (Role::MenuItemRadio, Some(accesskit::Toggled::True)));
    let second = a11y_named(&tree, "Second");
    assert_eq!((second.role(), second.toggled()), (Role::MenuItemCheckBox, Some(accesskit::Toggled::False)));
    assert!(a11y_named(&tree, "Third").is_disabled());
    assert_eq!(a11y_named(&tree, "Fourth With more to say").role(), Role::MenuItem);
    let id = tree.iter().find(|(_, node)| node.label() == Some("First")).expect("there").0.0;
    a11y_action_on(&mut rig, &view, id, accesskit::Action::Click);
    assert_eq!(picked.get(), 1);
    set_a11y_active(false);
}

#[test]
fn a_context_menu_reads_as_a_menu() {
    let view = |_: &mut App| -> E {
        div::<App>().child(context_menu(
            "a11y-context",
            ContextMenuProps::new(
                ContextMenuAnchor::Point { x: 10.0, y: 10.0 },
                "File",
                |_: &mut App| {},
                vec![ContextMenuSection::new("file", vec![ContextMenuItem::new("open", "Open").hint("Enter"), ContextMenuItem::new("gone", "Gone").disabled(true), ContextMenuItem::new("more", "More").children(vec![ContextMenuSection::new("sub", vec![ContextMenuItem::new("x", "Inside")])])])],
            ),
        ))
    };
    let mut rig = Rig::new();
    let tree = a11y_tree(&mut rig, &view);
    assert_eq!(a11y_named(&tree, "File").role(), Role::Menu);
    assert_eq!(a11y_named(&tree, "Open Enter").role(), Role::MenuItem);
    assert!(a11y_named(&tree, "Gone").is_disabled());
    let more = a11y_named(&tree, "More");
    assert_eq!((more.has_popup(), more.is_expanded()), (Some(HasPopup::Menu), Some(false)));
    set_a11y_active(false);
}

#[test]
fn page_tabs_read_as_tabs() {
    use mewrk_ui_common::page_tabs::{page_tabs, PageTab, PageTabs};
    let chosen = Rc::new(RefCell::new(String::new()));
    let view = {
        let chosen = chosen.clone();
        move |_: &mut App| -> E {
            let chosen = chosen.clone();
            let mut closable = PageTab::new("b", "Beta");
            closable.closable = true;
            col::<App>().w_full().child(page_tabs(PageTabs {
                key: "a11y-tabs".to_owned(),
                tabs: vec![PageTab::new("a", "Alpha"), closable],
                active_id: Some("a".to_owned()),
                aria_label: "Tabs".into(),
                more_label: "More tabs".into(),
                on_select: Rc::new(move |_, id| *chosen.borrow_mut() = id.to_owned()),
                on_close: Some(Rc::new(|_, _| {})),
                close_label: None,
                close_on_delete_key: false,
                on_reorder: None,
                max_tab_width: 200.0,
                on_tab_context_menu: None,
                on_tab_double_click: None,
                editor: None,
                trailing: None,
                trailing_width: 0.0,
            }))
        }
    };
    let mut rig = Rig::new();
    let tree = a11y_tree(&mut rig, &view);
    assert_eq!(a11y_named(&tree, "Tabs").role(), Role::TabList);
    let alpha = a11y_named(&tree, "Alpha");
    assert_eq!((alpha.role(), alpha.is_selected()), (Role::Tab, Some(true)));
    let beta = a11y_named(&tree, "Beta");
    assert_eq!((beta.role(), beta.is_selected()), (Role::Tab, Some(false)));
    assert_eq!(a11y_named(&tree, "Close Beta").role(), Role::Button);
    let id = tree.iter().find(|(_, node)| node.label() == Some("Beta")).expect("there").0.0;
    a11y_action_on(&mut rig, &view, id, accesskit::Action::Click);
    assert_eq!(*chosen.borrow(), "b");
    set_a11y_active(false);
}

#[test]
fn a_message_reads_as_its_words_and_a_path_as_the_whole_path() {
    let long = "/a/very/long/path/that/gets/shortened/in/the/middle/when/it/is/drawn.rs";
    let view = move |_: &mut App| -> E {
        col::<App>().w_full().child(markdown_content("a11y-message", "# Title\n\nSome **bold** words and `code`.\n\n- one\n- two\n\n```\nlet x;\n```\n", MarkdownOptions::default())).child(div().w(100.0).child(path_text("a11y-path", long, BODY_SMALL, Color::WHITE))).child(row().w(100.0).on_click(|_| {}).child(path_text("a11y-path-in-row", "/in/a/row.rs", BODY_SMALL, Color::WHITE)))
    };
    let mut rig = Rig::new();
    let tree = a11y_tree(&mut rig, &view);
    let title = a11y_named(&tree, "Title");
    assert_eq!((title.role(), title.level()), (Role::Heading, Some(1)));
    assert_eq!(a11y_named(&tree, "Some bold words and code.").role(), Role::Label);
    assert_eq!(a11y_named(&tree, "one").role(), Role::Label);
    assert_eq!(a11y_named(&tree, "let x;").role(), Role::Label);
    assert_eq!(a11y_named(&tree, long).role(), Role::Label);
    assert_eq!(a11y_named(&tree, "/in/a/row.rs").role(), Role::Button);
    set_a11y_active(false);
}

fn a11y_of<'a>(tree: &'a [(accesskit::NodeId, accesskit::Node)], role: Role) -> Vec<(u64, &'a accesskit::Node)> {
    tree.iter().filter(|(_, node)| node.role() == role).map(|(id, node)| (id.0, node)).collect()
}

fn a11y_id_of(tree: &[(accesskit::NodeId, accesskit::Node)], name: &str) -> u64 {
    tree.iter().find(|(_, node)| node.label() == Some(name) || (node.role() == Role::Label && node.value() == Some(name))).unwrap_or_else(|| panic!("nothing says {name:?}")).0.0
}

fn a11y_words_under(tree: &[(accesskit::NodeId, accesskit::Node)], node: &accesskit::Node) -> Vec<String> {
    let mut out = Vec::new();
    for child in node.children() {
        let (_, kid) = tree.iter().find(|(id, _)| id == child).expect("every child was sent");
        out.extend(kid.label().or(kid.value()).map(str::to_owned));
        out.extend(a11y_words_under(tree, kid));
    }
    out
}

fn a11y_bounds(node: &accesskit::Node) -> accesskit::Rect {
    node.bounds().expect("everything says where it is")
}

const WALKED: &str = "# Title\n\nSee [the *docs* page](https://example.com/docs) now.\n\n## Second\n\n- one\n- [x] done\n\n3. third\n4. fourth\n\n> quoted\n\n```rust\nlet x;\n```\n\n$$x^2$$\n\nInline $y$ too.\n\n| A | B |\n|---|---|\n| 1 | [two](https://example.com/two) |\n\n![a picture](pic.png)\n";

#[test]
fn a_message_stands_in_the_tree_as_what_it_holds() {
    let view = md_view(WALKED, MarkdownOptions::default());
    let mut rig = Rig::new();
    let tree = a11y_tree(&mut rig, &view);

    // Headings, in order, with their levels.
    let headings: Vec<(Option<&str>, Option<usize>)> = a11y_of(&tree, Role::Heading).into_iter().map(|(_, node)| (node.label(), node.level())).collect();
    assert!(headings.contains(&(Some("Title"), Some(1))) && headings.contains(&(Some("Second"), Some(2))), "{headings:?}");

    // A link is one node however many runs it was written in, between the words around it.
    let link = a11y_named(&tree, "the docs page");
    assert_eq!(link.role(), Role::Link);
    assert_eq!(link.url(), Some("https://example.com/docs"));
    assert!(link.supports_action(accesskit::Action::Click));
    let (_, paragraph) = a11y_of(&tree, Role::Paragraph).into_iter().find(|(_, node)| a11y_words_under(&tree, node).contains(&"the docs page".to_owned())).expect("the paragraph holds it");
    assert_eq!(a11y_words_under(&tree, paragraph), ["See", "the docs page", "now."]);

    // Lists hold items, and an ordered one says its numbers.
    let lists = a11y_of(&tree, Role::List);
    assert_eq!(lists.len(), 2);
    assert_eq!(lists[0].1.children().len(), 2);
    assert!(a11y_of(&tree, Role::CheckBox).iter().any(|(_, node)| node.toggled() == Some(accesskit::Toggled::True)));
    assert_eq!(a11y_words_under(&tree, lists[1].1), ["3.", "third", "4.", "fourth"]);

    let (_, quote) = a11y_of(&tree, Role::Blockquote)[0];
    assert_eq!(a11y_words_under(&tree, quote), ["quoted"]);

    // A code block: its language, its code and the button in its corner.
    let (_, code) = a11y_of(&tree, Role::Code)[0];
    assert_eq!(a11y_words_under(&tree, code), ["rust", "let x;", "Copy code"]);
    assert_eq!(a11y_named(&tree, "Copy code").role(), Role::Button);

    assert_eq!(a11y_named(&tree, "x^2").role(), Role::Math);
    assert_eq!(a11y_named(&tree, "y").role(), Role::Math);

    // A table by its rows and cells, and a link in one is still a link.
    assert_eq!(a11y_of(&tree, Role::Row).len(), 2);
    assert_eq!(a11y_of(&tree, Role::ColumnHeader).len(), 2);
    assert_eq!(a11y_of(&tree, Role::Cell).len(), 2);
    assert_eq!(a11y_named(&tree, "two").role(), Role::Link);

    // With no resolver the picture is its description, and still a picture to a reader.
    assert_eq!(a11y_named(&tree, "a picture").role(), Role::Image);

    // A platform takes a node with two parents, or a child it was never told of, as fatal.
    let mut parents = std::collections::HashMap::new();
    for (id, node) in &tree {
        for child in node.children() {
            assert!(tree.iter().any(|(other, _)| other == child), "a child that is not there");
            assert_eq!(parents.insert(*child, *id), None, "two parents");
        }
    }

    // Nothing that can be pressed goes without a name.
    for (_, node) in &tree {
        if node.supports_action(accesskit::Action::Click) {
            assert!(node.label().is_some_and(|name| !name.trim().is_empty()) || node.value().is_some(), "{:?} has no name", node.role());
        }
    }
    set_a11y_active(false);
}

#[test]
fn what_a_message_holds_is_where_it_is_painted() {
    let view = md_view(WALKED, MarkdownOptions::default());
    let mut rig = Rig::new();
    let tree = a11y_tree(&mut rig, &view);
    let leaf = rect_of(markdown_key(MD_KEY)).expect("painted");
    let (title, second) = (a11y_bounds(a11y_named(&tree, "Title")), a11y_bounds(a11y_named(&tree, "Second")));
    assert!(title.y1 <= second.y0, "in reading order, one under the other");
    assert!((title.x0 - leaf.x as f64).abs() < 1.0);

    // The link's own box: past the word before it and short of the column's end, on the paragraph's line.
    let link = a11y_bounds(a11y_named(&tree, "the docs page"));
    let (_, paragraph) = a11y_of(&tree, Role::Paragraph).into_iter().find(|(_, node)| a11y_words_under(&tree, node).contains(&"the docs page".to_owned())).expect("the paragraph");
    let around = a11y_bounds(paragraph);
    assert!(link.x0 > around.x0 && link.x1 < around.x1, "{link:?} in {around:?}");
    assert!(link.y0 >= around.y0 - 0.5 && link.y1 <= around.y1 + 0.5, "{link:?} in {around:?}");
    let pointed = (((link.x0 + link.x1) / 2.0) as f32, ((link.y0 + link.y1) / 2.0) as f32);
    assert!(leaf.contains(pointed.0, pointed.1));

    // The copy button sits in its code block's corner, as the one the pointer finds does.
    let code = a11y_bounds(a11y_of(&tree, Role::Code)[0].1);
    let copy = a11y_bounds(a11y_named(&tree, "Copy code"));
    assert!(copy.x0 >= code.x0 && copy.x1 <= code.x1 && copy.y0 >= code.y0 && copy.y1 <= code.y1, "{copy:?} in {code:?}");
    // A cell's link is inside its cell, and its cell inside its table.
    let table = a11y_bounds(a11y_of(&tree, Role::Table)[0].1);
    let two = a11y_bounds(a11y_named(&tree, "two"));
    assert!(two.x0 >= table.x0 && two.x1 <= table.x1 && two.y0 >= table.y0 && two.y1 <= table.y1, "{two:?} in {table:?}");
    set_a11y_active(false);
}

#[test]
fn a_press_on_a_link_in_a_message_reaches_what_a_click_reaches() {
    let opened = Rc::new(RefCell::new(Vec::<String>::new()));
    let (external, path) = (opened.clone(), opened.clone());
    let options = MarkdownOptions {
        linkify_paths: true,
        on_open_external: Some(Rc::new(move |_: &mut App, url: &str| external.borrow_mut().push(url.to_owned()))),
        on_open_path: Some(Rc::new(move |_, link: PathLink| path.borrow_mut().push(format!("{} {:?} {}", link.path, link.line, link.anchor.w > 0.0)))),
        ..Default::default()
    };
    let view = md_view("See [the docs](https://example.com/docs), `src/lib.rs:3` and [mail](mailto:a@example.com).", options);
    let mut rig = Rig::new();
    let tree = a11y_tree(&mut rig, &view);
    a11y_action_on(&mut rig, &view, a11y_id_of(&tree, "the docs"), accesskit::Action::Click);
    assert_eq!(*opened.borrow(), ["https://example.com/docs"]);
    let path_link = a11y_named(&tree, "src/lib.rs:3");
    assert_eq!(path_link.role(), Role::Link);
    assert_eq!(path_link.description(), Some("src/lib.rs"));
    a11y_action_on(&mut rig, &view, a11y_id_of(&tree, "src/lib.rs:3"), accesskit::Action::Click);
    assert_eq!(opened.borrow().last().map(String::as_str), Some("src/lib.rs Some(3) true"), "with the link's box, for a menu that opens where it is");
    // Somewhere a click goes nowhere is not on offer.
    let mail = a11y_named(&tree, "mail");
    assert_eq!(mail.role(), Role::Link);
    assert!(!mail.supports_action(accesskit::Action::Click));
    set_a11y_active(false);
}

#[test]
fn a_press_on_a_fragment_link_reveals_the_heading_it_names() {
    let revealed = Rc::new(RefCell::new(None::<Rect>));
    let saw = revealed.clone();
    let options = MarkdownOptions { on_reveal: Some(Rc::new(move |_: &mut App, r: Rect| *saw.borrow_mut() = Some(r))), ..Default::default() };
    let view = md_view("[jump](#far-down)\n\ntext\n\n## Far down\n", options);
    let mut rig = Rig::new();
    let tree = a11y_tree(&mut rig, &view);
    a11y_action_on(&mut rig, &view, a11y_id_of(&tree, "jump"), accesskit::Action::Click);
    let target = revealed.borrow().expect("the same handler a click reaches");
    assert!((target.y as f64) >= a11y_bounds(a11y_named(&tree, "Far down")).y0 - 1.0);
    set_a11y_active(false);
}

#[test]
fn a_code_blocks_copy_button_and_a_disclosure_answer_a_reader() {
    let options = MarkdownOptions { render_html: true, ..Default::default() };
    let view = md_view("```rust\nlet x;\n```\n\n<details><summary>More</summary>\n\nhidden words\n\n</details>", options);
    let mut rig = Rig::new();
    let tree = a11y_tree(&mut rig, &view);
    a11y_action_on(&mut rig, &view, a11y_id_of(&tree, "Copy code"), accesskit::Action::Click);
    assert_eq!(rig.clipboard.0.as_deref(), Some("let x;"));

    let more = a11y_named(&tree, "More");
    assert_eq!((more.role(), more.is_expanded()), (Role::DisclosureTriangle, Some(false)));
    assert!(!tree.iter().any(|(_, node)| node.value() == Some("hidden words")), "what is folded away is not there");
    let id = a11y_id_of(&tree, "More");
    // Asking for what it already is changes nothing.
    a11y_action_on(&mut rig, &view, id, accesskit::Action::Collapse);
    assert_eq!(probe::piece_texts(MD_KEY).last().map(String::as_str), Some("More"));
    a11y_action_on(&mut rig, &view, id, accesskit::Action::Expand);
    rig.settle(&view);
    assert_eq!(probe::piece_texts(MD_KEY).last().map(String::as_str), Some("hidden words"));
    let update = take_a11y_update().expect("the tree changed");
    let tree = update.nodes;
    assert_eq!(tree.iter().find(|(node, _)| node.0 == id).map(|(_, node)| node.is_expanded()), Some(Some(true)));
    assert!(tree.iter().any(|(_, node)| node.value() == Some("hidden words")));
    set_a11y_active(false);
}

#[test]
fn a_message_keeps_its_nodes_while_it_grows_and_says_nothing_while_nobody_listens() {
    let mut rig = Rig::new();
    set_a11y_active(false);
    let view = md_view("# Title\n\n[link](https://example.com)\n\ntail", MarkdownOptions::default());
    rig.settle(&view);
    assert!(!probe::has_reading(MD_KEY), "built only for someone listening");

    let tree = a11y_tree(&mut rig, &view);
    assert!(probe::has_reading(MD_KEY));
    let (title, link) = (a11y_id_of(&tree, "Title"), a11y_id_of(&tree, "link"));
    let longer = md_view("# Title\n\n[link](https://example.com)\n\ntail and more words", MarkdownOptions::default());
    // What did not change kept its id and its place frame by frame, so a reader standing on it stays there.
    let mut said = false;
    for _ in 0..4 {
        rig.frame(&longer);
        let nodes = take_a11y_update().map_or_else(Vec::new, |update| update.nodes);
        said |= nodes.iter().any(|(_, node)| node.value() == Some("tail and more words"));
        assert!(!nodes.iter().any(|(id, _)| id.0 == title || id.0 == link), "unchanged nodes stay as they were");
    }
    assert!(said);
    set_a11y_active(false);
}
