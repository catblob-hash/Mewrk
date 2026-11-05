//! Does appending really stay O(new text)? Measures the cost per token as a document grows.

use std::time::Instant;

use fluent::theme::{Color, BODY};
use fluent::{Doc, TextCtx};

fn main() {
    let mut ctx = TextCtx::new(2.0);
    let words = ["stream", "token ", "agent ", "runtime ", "vector ", "流式", "渲染，", "kernel ", "graph ", "node\n", "buffer ", "slot ", "retry ", "数据 ", "end. "];
    let mut doc = Doc::new(1600.0);
    let mut rng = 12345u64;
    let checkpoints = [1_000usize, 10_000, 100_000, 1_000_000];
    let mut total = 0usize;
    println!("{:>12} {:>14} {:>14} {:>12} {:>12}", "tokens", "ns/token", "ns/byte", "lines", "glyphs");
    for &target in &checkpoints {
        let start_tokens = total;
        let mut bytes = 0usize;
        let started = Instant::now();
        while total < target {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let w = words[(rng >> 33) as usize % words.len()];
            doc.push(&mut ctx, w, BODY, Color::WHITE);
            bytes += w.len();
            total += 1;
        }
        let e = started.elapsed().as_nanos() as f64;
        println!("{:>12} {:>14.0} {:>14.1} {:>12} {:>12}", total, e / (total - start_tokens) as f64, e / bytes as f64, doc.lines.len(), doc.glyph_count());
    }
    let mut out = Vec::new();
    let clip = [0, 0, 32767, 32767];
    let started = Instant::now();
    for i in 0..1000 {
        out.clear();
        let y = (doc.height() - 3000.0) * (i as f32 / 1000.0);
        doc.paint(&mut ctx, &mut out, 0.0, -y, clip, y..y + 2000.0);
    }
    println!("paint one screen of a {} glyph document: {:.1} us, {} glyphs", doc.glyph_count(), started.elapsed().as_secs_f64() * 1e3, out.len());
    let started = Instant::now();
    doc.reflow(&mut ctx);
    println!("full reflow of the document after a resize: {:.1} ms", started.elapsed().as_secs_f64() * 1e3);
    println!("{:?}", ctx.stats);
}
