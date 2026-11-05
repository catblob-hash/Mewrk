//! Rasterises an SVG to a square PNG with the renderer `Ui::svg_icon` draws with, so the application icon and the marks inside
//! the window come out of one engine: `render_svg <in.svg> <px> <out.png>`. `scripts/make-icon.sh` runs it for every icon size.

use resvg::{tiny_skia, usvg};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [input, px, output] = args.as_slice() else {
        eprintln!("usage: render_svg <in.svg> <px> <out.png>");
        std::process::exit(2);
    };
    let px: u32 = px.parse().expect("px is a whole number");
    let svg = std::fs::read_to_string(input).expect("the SVG is readable");
    let tree = usvg::Tree::from_str(&svg, &usvg::Options::default()).expect("the SVG parses");
    let mut pixmap = tiny_skia::Pixmap::new(px, px).expect("the size is not zero");
    let k = px as f32 / tree.size().width().max(tree.size().height());
    resvg::render(&tree, tiny_skia::Transform::from_scale(k, k), &mut pixmap.as_mut());
    pixmap.save_png(output).expect("the PNG is writable");
}
