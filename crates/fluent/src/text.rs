//! Text: font discovery, word-cached shaping, glyph atlases and incremental layout.
//!
//! Generic text engines re-shape a whole paragraph when it changes, which makes streaming quadratic: every
//! token re-does every token before it. Here the unit of work is the word. A shaped word is cached (browsers
//! call this a word cache), lines that have been completed are never touched again, and an append only redoes
//! the one unfinished word at the tail. Appending is O(new text), however long the document has grown.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use etagere::{size2, BucketedAtlasAllocator};
use swash::scale::image::{Content, Image};
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::shape::ShapeContext;
use swash::text::{Codepoint, Script};
use swash::zeno::{Format, Mask, Transform, Vector};
use swash::{CacheKey, FontDataRef, FontRef, Setting};

use crate::gpu::{AtlasUpload, GlyphInst, Renderer, COLOR_ATLAS_SIZE, GLYPH_COLOR, MASK_ATLAS_SIZE};
use crate::theme::{Color, TypeStyle, Weight};

// ---------------------------------------------------------------- hashing

/// FxHash. Keys here are small integers and short strings from our own UI, so HashDoS is not a concern and
/// SipHash's cost shows up directly in frame time.
#[derive(Default, Clone, Copy)]
pub struct FxHasher(u64);

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.write_u64(u64::from_le_bytes(word));
        }
    }
    #[inline]
    fn write_u64(&mut self, v: u64) {
        self.0 = (self.0.rotate_left(5) ^ v).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
    #[inline]
    fn write_u32(&mut self, v: u32) {
        self.write_u64(v as u64);
    }
    #[inline]
    fn write_u16(&mut self, v: u16) {
        self.write_u64(v as u64);
    }
    #[inline]
    fn write_u8(&mut self, v: u8) {
        self.write_u64(v as u64);
    }
    #[inline]
    fn write_usize(&mut self, v: usize) {
        self.write_u64(v as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

pub type FxBuild = BuildHasherDefault<FxHasher>;
pub type FxMap<K, V> = HashMap<K, V, FxBuild>;

pub fn hash_str(s: &str) -> u64 {
    let mut h = FxHasher::default();
    h.write(s.as_bytes());
    h.write_u64(s.len() as u64);
    h.finish()
}

// ---------------------------------------------------------------- fonts

struct Face {
    data: Arc<memmap2::Mmap>,
    offset: u32,
    key: CacheKey,
    has_wght: bool,
    has_opsz: bool,
    ascent: f32,
    descent: f32,
    units_per_em: f32,
    x_height: f32,
}

impl Face {
    fn font(&self) -> FontRef<'_> {
        FontRef { data: &self.data[..], offset: self.offset, key: self.key }
    }

    fn open(path: &Path, wanted_weight: u16) -> Option<Face> {
        let file = std::fs::File::open(path).ok()?;
        // SAFETY: system font files are not modified while mapped. A concurrent font update would at worst
        // yield wrong glyphs for a frame, and swash bounds-checks everything it reads.
        let data = Arc::new(unsafe { memmap2::Mmap::map(&file).ok()? });
        let font_data = FontDataRef::new(&data[..])?;
        let mut best: Option<(u32, FontRef<'_>)> = None;
        for font in font_data.fonts() {
            let attrs = font.attributes();
            let italic_penalty = if attrs.style() == swash::Style::Normal { 0 } else { 2000 };
            let score = (attrs.weight().0 as i32 - wanted_weight as i32).unsigned_abs() + italic_penalty;
            if best.as_ref().is_none_or(|(s, _)| score < *s) {
                best = Some((score, font));
            }
        }
        let (_, font) = best?;
        let metrics = font.metrics(&[]);
        let has = |tag: &str| font.variations().any(|v| v.tag() == swash::tag_from_str_lossy(tag));
        // Through swash, Segoe UI Variable's kerning comes out wrong once `opsz` is set with a heavier `wght`: at 20 px semibold, `w.` advances 36 px
        // where it should be 15. Its default optical size is the text cut, which reads well at every size this interface uses.
        let (has_wght, has_opsz) = (has("wght"), has("opsz") && !cfg!(windows));
        let (offset, key, ascent, descent, units_per_em, x_height) = (font.offset, font.key, metrics.ascent, metrics.descent, metrics.units_per_em as f32, metrics.x_height);
        Some(Face { data: data.clone(), offset, key, has_wght, has_opsz, ascent, descent, units_per_em, x_height })
    }

    /// The face of a font file that has a `MATH` table: Cambria Math shares its file with Cambria.
    fn open_math(path: &Path) -> Option<Face> {
        let file = std::fs::File::open(path).ok()?;
        // SAFETY: as in `open`.
        let data = Arc::new(unsafe { memmap2::Mmap::map(&file).ok()? });
        let font = FontDataRef::new(&data[..])?.fonts().find(|font| has_table(font, b"MATH"))?;
        let metrics = font.metrics(&[]);
        let (offset, key, ascent, descent, units_per_em, x_height) = (font.offset, font.key, metrics.ascent, metrics.descent, metrics.units_per_em as f32, metrics.x_height);
        Some(Face { data: data.clone(), offset, key, has_wght: false, has_opsz: false, ascent, descent, units_per_em, x_height })
    }

    fn settings(&self, style: StyleKey) -> ([Setting<f32>; 2], usize) {
        let mut out = [Setting { tag: swash::tag_from_bytes(b"wght"), value: 400.0 }, Setting { tag: swash::tag_from_bytes(b"opsz"), value: 14.0 }];
        let mut n = 0;
        if self.has_wght {
            out[n] = Setting { tag: swash::tag_from_bytes(b"wght"), value: if style.semibold { 600.0 } else { 400.0 } };
            n += 1;
        }
        if self.has_opsz {
            out[n] = Setting { tag: swash::tag_from_bytes(b"opsz"), value: (style.size_q as f32 / 8.0 / style.scale_q as f32 * 8.0).clamp(9.0, 96.0) };
            n += 1;
        }
        (out, n)
    }
}

/// Whether a font's table directory lists this table. swash reads tables it knows about and has no way to ask about others.
fn has_table(font: &FontRef<'_>, tag: &[u8; 4]) -> bool {
    let at = font.offset as usize;
    let Some(count) = font.data.get(at + 4..at + 6) else { return false };
    let count = u16::from_be_bytes([count[0], count[1]]) as usize;
    (0..count).any(|i| font.data.get(at + 12 + i * 16..at + 16 + i * 16) == Some(&tag[..]))
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Chain {
    Ui,
    Mono,
    Math,
    Serif,
    /// A family of its own in front of the interface chain, by its place in `family_chains`.
    Family(u16),
}

struct FontBook {
    faces: Vec<Face>,
    ui: Vec<u16>,
    mono: Vec<u16>,
    /// The system's math face, when it has one, in front of the interface chain. Empty until a formula first asks.
    math: Vec<u16>,
    math_face: Option<u16>,
    math_probed: bool,
    /// Messages in a serif face, when that is chosen; empty means the interface chain.
    serif: Vec<u16>,
    chosen_serif: String,
    emoji: Option<u16>,
    /// The real semibold cut of a face without a weight axis, when its collection has one.
    semibold: FxMap<u16, u16>,
    /// Every other font on the system, listed the first time a character turns up that nothing above can
    /// draw. Most sessions never get there, and scanning touches hundreds of files.
    system_scanned: bool,
    system_paths: Vec<(PathBuf, u32)>,
    char_face: FxMap<(char, Chain), u16>,
    /// The chains as they load, for going back to them, and which families sit in front of them now.
    default_ui: Vec<u16>,
    default_mono: Vec<u16>,
    chosen: (String, String),
    /// Families asked for by name, for what shows a family in itself. Their ids count from 1, and 0 is one the system doesn't have.
    family_ids: FxMap<String, u16>,
    family_chains: Vec<Vec<u16>>,
    family_db: Option<fontdb::Database>,
}

fn first_existing(paths: &[&str]) -> Option<PathBuf> {
    paths.iter().map(PathBuf::from).find(|p| p.exists())
}

/// On macOS the nicest Chinese face is delivered as a downloadable asset, at a path with a hash in it.
#[cfg(target_os = "macos")]
fn pingfang() -> Option<PathBuf> {
    for dir in std::fs::read_dir("/System/Library/AssetsV2").ok()?.flatten() {
        if !dir.file_name().to_string_lossy().contains("Font") {
            continue;
        }
        for asset in std::fs::read_dir(dir.path()).ok()?.flatten() {
            let p = asset.path().join("AssetData/PingFang.ttc");
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}

impl FontBook {
    fn load() -> Self {
        // The primary faces are probed by path, which takes microseconds where a full scan takes 100 ms.
        let [ui, mono, cjk, symbols, emoji]: [Option<PathBuf>; 5];
        #[cfg(target_os = "macos")]
        {
            [ui, mono, cjk, symbols, emoji] = [
                first_existing(&["/System/Library/Fonts/SFNS.ttf", "/System/Library/Fonts/Helvetica.ttc"]),
                first_existing(&["/System/Library/Fonts/SFNSMono.ttf", "/System/Library/Fonts/Menlo.ttc"]),
                pingfang().or_else(|| first_existing(&["/System/Library/Fonts/Hiragino Sans GB.ttc", "/System/Library/Fonts/STHeiti Medium.ttc"])),
                first_existing(&["/System/Library/Fonts/CJKSymbolsFallback.ttc", "/System/Library/Fonts/Apple Symbols.ttf"]),
                first_existing(&["/System/Library/Fonts/Apple Color Emoji.ttc"]),
            ];
        }
        #[cfg(target_os = "windows")]
        {
            [ui, mono, cjk, symbols, emoji] = [
                first_existing(&["C:\\Windows\\Fonts\\SegUIVar.ttf", "C:\\Windows\\Fonts\\segoeui.ttf"]),
                first_existing(&["C:\\Windows\\Fonts\\CascadiaMono.ttf", "C:\\Windows\\Fonts\\consola.ttf"]),
                first_existing(&["C:\\Windows\\Fonts\\msyh.ttc", "C:\\Windows\\Fonts\\simsun.ttc"]),
                first_existing(&["C:\\Windows\\Fonts\\seguisym.ttf"]),
                first_existing(&["C:\\Windows\\Fonts\\seguiemj.ttf"]),
            ];
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            [ui, mono, cjk, symbols, emoji] = [
                first_existing(&["/usr/share/fonts/truetype/inter/Inter-Regular.ttf", "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf", "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"]),
                first_existing(&["/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf", "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"]),
                first_existing(&["/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc"]),
                None,
                first_existing(&["/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf"]),
            ];
        }

        let mut faces = Vec::new();
        let add = |path: &Option<PathBuf>, faces: &mut Vec<Face>| -> Option<u16> {
            let face = Face::open(path.as_ref()?, 400)?;
            faces.push(face);
            Some(faces.len() as u16 - 1)
        };
        let ui_id = add(&ui, &mut faces);
        let mono_id = add(&mono, &mut faces);
        let cjk_id = add(&cjk, &mut faces);
        let symbols_id = add(&symbols, &mut faces);
        let emoji_id = add(&emoji, &mut faces);

        let chain = |first: Option<u16>| -> Vec<u16> { [first, cjk_id, symbols_id, emoji_id].into_iter().flatten().collect() };
        let (ui_chain, mono_chain) = (chain(ui_id.or(mono_id)), chain(mono_id.or(ui_id)));

        // CJK text in headings is the main beneficiary: a designed semibold is far crisper than smearing the
        // regular weight.
        let mut semibold = FxMap::default();
        // Windows keeps YaHei's bold in a file of its own. Elsewhere one collection holds the weights.
        let bold_file = if cfg!(target_os = "windows") { first_existing(&["C:\\Windows\\Fonts\\msyhbd.ttc"]) } else { None };
        if let (Some(regular), Some(path)) = (cjk_id, bold_file.as_ref().or(cjk.as_ref()))
            && let Some(face) = Face::open(path, 600)
                && (bold_file.is_some() || face.offset != faces[regular as usize].offset) {
                    faces.push(face);
                    semibold.insert(regular, faces.len() as u16 - 1);
                }

        let mut book = Self { faces, default_ui: ui_chain.clone(), default_mono: mono_chain.clone(), ui: ui_chain, mono: mono_chain, math: Vec::new(), math_face: None, math_probed: false, serif: Vec::new(), chosen_serif: String::new(), emoji: emoji_id, semibold, system_scanned: false, system_paths: Vec::new(), char_face: FxMap::default(), chosen: (String::new(), String::new()), family_ids: FxMap::default(), family_chains: Vec::new(), family_db: None };
        if book.ui.is_empty() {
            // Nothing at a known path, so the scan is the only hope.
            book.scan_system();
            let paths = std::mem::take(&mut book.system_paths);
            for (path, _) in &paths {
                if let Some(face) = Face::open(path, 400) {
                    book.faces.push(face);
                    let id = book.faces.len() as u16 - 1;
                    book.ui.push(id);
                    book.mono.push(id);
                    break;
                }
            }
            book.system_paths = paths;
        }
        book
    }

    /// Finds the math face the first time a formula wants it: most sessions never show one.
    fn ensure_math(&mut self) {
        if !std::mem::replace(&mut self.math_probed, true) {
            let found = MATH_FONT_PATHS.iter().filter(|p| Path::new(p).exists()).find_map(|p| Face::open_math(Path::new(p)));
            self.math_face = found.map(|face| {
                self.faces.push(face);
                self.faces.len() as u16 - 1
            });
            self.rebuild_math_chain();
        }
    }

    fn rebuild_math_chain(&mut self) {
        self.math = self.math_face.into_iter().chain(self.ui.iter().copied()).collect();
    }

    fn chain_list(&self, chain: Chain) -> &[u16] {
        match chain {
            Chain::Ui => &self.ui,
            Chain::Mono => &self.mono,
            Chain::Math => &self.math,
            Chain::Serif if self.serif.is_empty() => &self.ui,
            Chain::Serif => &self.serif,
            Chain::Family(at) => &self.family_chains[at as usize],
        }
    }

    /// The id of a family asked for by name. The system's fonts are listed once, the first time one is asked for.
    fn family_id(&mut self, family: &str) -> u16 {
        if let Some(&id) = self.family_ids.get(family) {
            return id;
        }
        let db = self.family_db.get_or_insert_with(|| {
            let mut db = fontdb::Database::new();
            db.load_system_fonts();
            db
        });
        let path = db.query(&fontdb::Query { families: &[fontdb::Family::Name(family)], ..fontdb::Query::default() }).and_then(|id| db.face(id)).and_then(|face| match &face.source {
            fontdb::Source::File(path) => Some(path.clone()),
            _ => None,
        });
        let id = match path.and_then(|path| Face::open(&path, 400)) {
            Some(face) => {
                self.faces.push(face);
                let at = self.faces.len() as u16 - 1;
                self.family_chains.push(std::iter::once(at).chain(self.ui.iter().copied()).collect());
                self.family_chains.len() as u16
            }
            None => 0,
        };
        self.family_ids.insert(family.to_owned(), id);
        id
    }

    /// The first installed family of a CSS-like stack: `"Fira Code", monospace`. Names nothing here finds are skipped.
    fn open_family(&mut self, stack: &str) -> Option<u16> {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        for name in stack.split(',').map(|n| n.trim().trim_matches(|c| c == '"' || c == '\'')).filter(|n| !n.is_empty()) {
            let query = fontdb::Query { families: &[fontdb::Family::Name(name)], ..fontdb::Query::default() };
            let Some(id) = db.query(&query) else { continue };
            let Some(face_info) = db.face(id) else { continue };
            let fontdb::Source::File(path) = &face_info.source else { continue };
            if let Some(face) = Face::open(path, 400) {
                self.faces.push(face);
                return Some(self.faces.len() as u16 - 1);
            }
        }
        None
    }

    fn scan_system(&mut self) {
        if std::mem::replace(&mut self.system_scanned, true) {
            return;
        }
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        self.system_paths = db
            .faces()
            .filter_map(|f| match &f.source {
                fontdb::Source::File(p) => Some((p.clone(), f.index)),
                _ => None,
            })
            .collect();
    }

    fn covers(&self, face: u16, c: char) -> bool {
        self.faces[face as usize].font().charmap().map(c) != 0
    }

    /// The face that draws `c`, walking the fallback chain once per character per chain.
    fn face_for(&mut self, c: char, chain: Chain) -> u16 {
        if let Some(&f) = self.char_face.get(&(c, chain)) {
            return f;
        }
        let list = self.chain_list(chain);
        let primary = list.first().copied().unwrap_or(0);
        // Mathematical brackets and arrows sit in the ranges `is_emoji` takes in, and a formula means the plain ones.
        let wants_emoji = chain != Chain::Math && is_emoji(c) && self.emoji.is_some_and(|e| self.covers(e, c));
        let found = if wants_emoji { self.emoji } else { list.iter().copied().find(|&f| self.covers(f, c)) };
        let face = found.or_else(|| self.search_system(c)).unwrap_or(primary);
        self.char_face.insert((c, chain), face);
        face
    }

    fn search_system(&mut self, c: char) -> Option<u16> {
        if c.is_control() || c.is_whitespace() {
            return None;
        }
        self.scan_system();
        let mut tried: std::collections::HashSet<PathBuf> = Default::default();
        for i in 0..self.system_paths.len() {
            let path = self.system_paths[i].0.clone();
            if !tried.insert(path.clone()) {
                continue;
            }
            let Some(face) = Face::open(&path, 400) else { continue };
            if face.font().charmap().map(c) != 0 {
                self.faces.push(face);
                let id = self.faces.len() as u16 - 1;
                // The next character from the same script will almost certainly want this face too.
                self.ui.push(id);
                self.mono.push(id);
                self.math.push(id);
                return Some(id);
            }
        }
        None
    }
}

/// Where the common systems keep a font with a `MATH` table and the whole of the mathematical alphabets.
const MATH_FONT_PATHS: &[&str] = &[
    "/System/Library/Fonts/Supplemental/STIXTwoMath.otf",
    "C:\\Windows\\Fonts\\cambria.ttc",
    "/usr/share/fonts/opentype/stix/STIXTwoMath-Regular.otf",
    "/usr/share/fonts/opentype/stix-two/STIXTwoMath-Regular.otf",
    "/usr/share/fonts/stix-two-math/STIXTwoMath-Regular.otf",
    "/usr/share/fonts/opentype/lmodern/lmmath-regular.otf",
    "/usr/share/texmf/fonts/opentype/public/lm-math/latinmodern-math.otf",
    "/usr/share/fonts/truetype/dejavu/DejaVuMathTeXGyre.ttf",
    "/usr/share/fonts/truetype/noto/NotoSansMath-Regular.ttf",
];

pub fn is_emoji(c: char) -> bool {
    matches!(c as u32,
        0x1F300..=0x1FAFF | 0x2600..=0x27BF | 0x2B00..=0x2BFF | 0x1F000..=0x1F2FF | 0x2300..=0x23FF)
}

fn is_emoji_joiner(c: char) -> bool {
    matches!(c as u32, 0x200D | 0xFE0F | 0x1F3FB..=0x1F3FF | 0x20E3 | 0xE0020..=0xE007F)
}

pub fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3000..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x20000..=0x3FFFF)
}

/// Punctuation that must not begin a line.
fn no_line_start(c: char) -> bool {
    matches!(c, '，' | '。' | '、' | '！' | '？' | '：' | '；' | '）' | '】' | '》' | '」' | '』' | '”' | '’' | '…' | ',' | '.' | '!' | '?' | ':' | ';' | ')' | ']' | '}' | '%')
}

// ---------------------------------------------------------------- atlas

#[derive(Clone, Copy, Debug, Default)]
struct AtlasGlyph {
    u: u16,
    v: u16,
    w: u16,
    h: u16,
    left: i16,
    top: i16,
    color: bool,
}

struct Atlas {
    alloc: BucketedAtlasAllocator,
    data: Vec<u8>,
    size: u32,
    bpp: u32,
    dirty: Option<[u32; 4]>,
}

impl Atlas {
    fn new(size: u32, bpp: u32) -> Self {
        Self {
            alloc: BucketedAtlasAllocator::new(size2(size as i32, size as i32)),
            data: vec![0; (size * size * bpp) as usize],
            size,
            bpp,
            dirty: None,
        }
    }

    fn insert(&mut self, w: u32, h: u32, pixels: &[u8]) -> Option<(u16, u16)> {
        let a = self.alloc.allocate(size2(w as i32 + 1, h as i32 + 1))?;
        let (x, y) = (a.rectangle.min.x as u32, a.rectangle.min.y as u32);
        let stride = (w * self.bpp) as usize;
        for row in 0..h as usize {
            let at = ((y as usize + row) * self.size as usize + x as usize) * self.bpp as usize;
            self.data[at..at + stride].copy_from_slice(&pixels[row * stride..(row + 1) * stride]);
        }
        self.dirty = Some(match self.dirty {
            None => [x, y, x + w, y + h],
            Some([x0, y0, x1, y1]) => [x0.min(x), y0.min(y), x1.max(x + w), y1.max(y + h)],
        });
        Some((x as u16, y as u16))
    }

    fn clear(&mut self) {
        self.alloc.clear();
        self.dirty = None;
    }

    fn take_upload(&mut self) -> Option<[u32; 4]> {
        self.dirty.take().map(|[x0, y0, x1, y1]| [x0, y0, x1 - x0, y1 - y0])
    }
}

// ---------------------------------------------------------------- shaping

/// Everything that changes which glyph images a piece of text uses.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct StyleKey {
    /// Physical pixel size times 8.
    size_q: u16,
    /// UI scale times 8, for the optical size axis.
    scale_q: u16,
    semibold: bool,
    mono: bool,
    italic: bool,
    math: bool,
    serif: bool,
    /// A family asked for by name, as `FontBook::family_id` numbers them; 0 is none.
    family: u16,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphKey {
    face: u16,
    gid: u16,
    size_q: u16,
    scale_q: u16,
    bin: u8,
    flags: u8,
}

#[derive(Clone, Copy)]
struct ShapedGlyph {
    face: u16,
    gid: u16,
    x: f32,
    y: f32,
    cluster: u32,
    advance: f32,
}

struct ShapedWord {
    glyphs: Vec<ShapedGlyph>,
    /// Advance including trailing whitespace.
    advance: f32,
    /// Advance without it: trailing spaces may hang past the line's end.
    visible: f32,
}

/// A laid-out single line, ready to copy. Labels get painted every frame, so they must cost next to nothing.
pub struct LineRun {
    glyphs: Vec<GlyphInst>,
    pub width: f32,
    /// Where the next run would start: `width` leaves out trailing spaces, which still push what follows.
    pub advance: f32,
    pub height: f32,
    baseline: f32,
    epoch: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct LineKey {
    text: u64,
    style: StyleKey,
    color: Color2,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Color2(u32);

impl From<Color> for Color2 {
    fn from(c: Color) -> Self {
        Color2(u32::from_le_bytes([c.r, c.g, c.b, c.a]))
    }
}

pub struct TextCtx {
    icons: HashMap<(String, u32, i32), AtlasGlyph>,
    fonts: FontBook,
    shape: ShapeContext,
    scale_ctx: ScaleContext,
    image: Image,
    words: FxMap<(u64, StyleKey), Arc<ShapedWord>>,
    lines: FxMap<LineKey, Arc<LineRun>>,
    glyphs: FxMap<GlyphKey, AtlasGlyph>,
    paths: FxMap<(u64, u32, u32), AtlasGlyph>,
    mask: Atlas,
    color: Atlas,
    /// Bumped whenever atlas coordinates become invalid. Anything holding laid-out glyphs compares against it.
    pub epoch: u32,
    pub scale: f32,
    pub stats: TextStats,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct TextStats {
    pub shaped_words: u64,
    pub word_hits: u64,
    pub rasterized: u64,
    pub atlas_resets: u64,
}

impl TextCtx {
    pub fn new(scale: f32) -> Self {
        Self::with_atlas_sizes(scale, MASK_ATLAS_SIZE, COLOR_ATLAS_SIZE)
    }

    /// Atlases of a particular size. Tests use small ones to make resets happen.
    pub fn with_atlas_sizes(scale: f32, mask: u32, color: u32) -> Self {
        Self {
            fonts: FontBook::load(),
            shape: ShapeContext::new(),
            scale_ctx: ScaleContext::new(),
            image: Image::new(),
            words: FxMap::default(),
            lines: FxMap::default(),
            glyphs: FxMap::default(),
            paths: FxMap::default(),
            icons: HashMap::new(),
            mask: Atlas::new(mask, 1),
            color: Atlas::new(color, 4),
            epoch: 1,
            scale,
            stats: TextStats::default(),
        }
    }

    /// Puts these families in front of the interface and code chains; an empty stack goes back to the default. The system's
    /// fonts are only listed when a family is actually asked for, and a call with what's already in place costs nothing.
    pub fn set_font_families(&mut self, ui: &str, mono: &str) {
        if self.fonts.chosen.0 == ui && self.fonts.chosen.1 == mono {
            return;
        }
        let picked_ui = if ui.trim().is_empty() { None } else { self.fonts.open_family(ui) };
        let picked_mono = if mono.trim().is_empty() { None } else { self.fonts.open_family(mono) };
        let chain = |first: Option<u16>, rest: &[u16]| first.into_iter().chain(rest.iter().copied()).collect::<Vec<u16>>();
        self.fonts.ui = chain(picked_ui, &self.fonts.default_ui);
        self.fonts.mono = chain(picked_mono, &self.fonts.default_mono);
        self.fonts.rebuild_math_chain();
        self.fonts.chosen = (ui.to_owned(), mono.to_owned());
        // What a family falls back through follows the interface font too.
        let fallback = self.fonts.ui.clone();
        for family in &mut self.fonts.family_chains {
            family.truncate(1);
            family.extend_from_slice(&fallback);
        }
        // The serif chain falls back through these, so it follows them.
        if let Some(&serif) = self.fonts.serif.first() {
            self.fonts.serif = chain(Some(serif), &self.fonts.ui);
        }
        self.fonts.char_face.clear();
        self.words.clear();
        self.reset_atlases();
    }

    /// The family `TypeStyle::serif` text is set in, from a CSS-like stack; an empty stack makes it the interface font. Like
    /// [`Self::set_font_families`], this lists the system's fonts only when one is asked for, and repeating itself costs nothing.
    pub fn set_serif_family(&mut self, stack: &str) {
        if self.fonts.chosen_serif == stack {
            return;
        }
        let picked = if stack.trim().is_empty() { None } else { self.fonts.open_family(stack) };
        self.fonts.serif = match picked {
            Some(serif) => std::iter::once(serif).chain(self.fonts.ui.iter().copied()).collect(),
            None => Vec::new(),
        };
        self.fonts.chosen_serif = stack.to_owned();
        self.fonts.char_face.clear();
        self.words.clear();
        self.reset_atlases();
    }

    pub fn set_scale(&mut self, scale: f32) {
        if (scale - self.scale).abs() > 1e-3 {
            self.scale = scale;
            self.words.clear();
            self.lines.clear();
            self.reset_atlases();
        }
    }

    fn reset_atlases(&mut self) {
        self.icons.clear();
        self.glyphs.clear();
        self.paths.clear();
        self.lines.clear();
        self.mask.clear();
        self.color.clear();
        self.epoch += 1;
        self.stats.atlas_resets += 1;
    }

    pub fn style_key(&self, ty: &TypeStyle) -> StyleKey {
        StyleKey {
            size_q: (ty.size * self.scale * 8.0).round() as u16,
            scale_q: (self.scale * 8.0).round() as u16,
            semibold: ty.weight == Weight::Semibold,
            mono: ty.mono,
            italic: ty.italic,
            math: false,
            serif: ty.serif && !ty.mono,
            family: 0,
        }
    }

    fn chain(style: StyleKey) -> Chain {
        if style.family != 0 {
            Chain::Family(style.family - 1)
        } else if style.math {
            Chain::Math
        } else if style.mono {
            Chain::Mono
        } else if style.serif {
            Chain::Serif
        } else {
            Chain::Ui
        }
    }

    /// Ascent and descent of the primary face at this size, in physical pixels.
    fn vertical_metrics(&self, style: StyleKey) -> (f32, f32) {
        let list = self.fonts.chain_list(Self::chain(style));
        let Some(&id) = list.first() else { return (0.0, 0.0) };
        let face = &self.fonts.faces[id as usize];
        let k = style.size_q as f32 / 8.0 / face.units_per_em.max(1.0);
        (face.ascent * k, face.descent * k)
    }

    /// Baseline offset from the top of a line box of height `line_h`, centring the font in it.
    fn baseline_in(&self, style: StyleKey, line_h: f32) -> f32 {
        let (a, d) = self.vertical_metrics(style);
        ((line_h - (a + d)) * 0.5 + a).round()
    }

    fn shape_word(&mut self, word: &str, style: StyleKey) -> Arc<ShapedWord> {
        let key = (hash_str(word), style);
        if let Some(w) = self.words.get(&key) {
            self.stats.word_hits += 1;
            return w.clone();
        }
        self.stats.shaped_words += 1;
        let shaped = Arc::new(self.shape_uncached(word, style));
        // A generous bound that real text never reaches, while random tokens still cannot grow it forever.
        if self.words.len() > 200_000 {
            self.words.clear();
        }
        self.words.insert(key, shaped.clone());
        shaped
    }

    fn shape_uncached(&mut self, word: &str, style: StyleKey) -> ShapedWord {
        let chain = Self::chain(style);
        let px = style.size_q as f32 / 8.0;
        let mut glyphs = Vec::with_capacity(word.len());
        let mut x = 0.0f32;
        let mut visible = 0.0f32;

        // Split into runs that share a face and script.
        let mut runs: Vec<(Range<usize>, u16, Script)> = Vec::new();
        let mut prev_emoji = false;
        for (i, c) in word.char_indices() {
            let joining = prev_emoji && is_emoji_joiner(c);
            let face = if joining {
                runs.last().map_or(0, |r| r.1)
            } else {
                let f = self.fonts.face_for(c, chain);
                if style.semibold { self.fonts.semibold.get(&f).copied().unwrap_or(f) } else { f }
            };
            let script = match c.script() {
                Script::Common | Script::Inherited | Script::Unknown => None,
                s => Some(s),
            };
            match runs.last_mut() {
                Some(run) if run.1 == face && (script.is_none() || script == Some(run.2) || run.2 == Script::Common) => {
                    if run.2 == Script::Common
                        && let Some(s) = script {
                            run.2 = s;
                        }
                    run.0.end = i + c.len_utf8();
                }
                _ => runs.push((i..i + c.len_utf8(), face, script.unwrap_or(Script::Common))),
            }
            prev_emoji = is_emoji(c) || joining;
        }

        for (range, face_id, script) in runs {
            let face = &self.fonts.faces[face_id as usize];
            let (settings, n) = face.settings(style);
            let mut shaper = self.shape.builder(face.font()).script(script).size(px).variations(settings[..n].iter().copied()).build();
            shaper.add_str(&word[range.clone()]);
            let start = range.start as u32;
            shaper.shape_with(|cluster| {
                let source = cluster.source.start + start;
                for g in cluster.glyphs {
                    glyphs.push(ShapedGlyph { face: face_id, gid: g.id, x: x + g.x, y: g.y, cluster: source, advance: g.advance });
                    x += g.advance;
                }
                let is_space = word[source as usize..].chars().next().is_some_and(char::is_whitespace);
                if !is_space {
                    visible = x;
                }
            });
        }
        ShapedWord { glyphs, advance: x, visible }
    }

    fn atlas_glyph(&mut self, key: GlyphKey) -> AtlasGlyph {
        if let Some(g) = self.glyphs.get(&key) {
            return *g;
        }
        self.stats.rasterized += 1;
        let glyph = self.rasterize(key);
        self.glyphs.insert(key, glyph);
        glyph
    }

    fn rasterize(&mut self, key: GlyphKey) -> AtlasGlyph {
        let face = &self.fonts.faces[key.face as usize];
        let style = StyleKey { size_q: key.size_q, scale_q: key.scale_q, semibold: key.flags & 1 != 0, mono: false, italic: key.flags & 2 != 0, math: false, serif: false, family: 0 };
        let (settings, n) = face.settings(style);
        let mut scaler = self.scale_ctx.builder(face.font()).size(key.size_q as f32 / 8.0).hint(false).variations(settings[..n].iter().copied()).build();
        let sources = [Source::ColorOutline(0), Source::ColorBitmap(StrikeWith::BestFit), Source::Outline];
        let mut render = Render::new(&sources);
        render.format(Format::Alpha).offset(Vector::new(key.bin as f32 / 4.0, 0.0));
        if style.italic {
            render.transform(Some(Transform::skew(swash::zeno::Angle::from_degrees(12.0), swash::zeno::Angle::from_degrees(0.0))));
        }
        // A face without a weight axis has no semibold, so fallback faces get it synthetically.
        if style.semibold && !face.has_wght && !self.fonts.semibold.values().any(|&f| f == key.face) {
            render.embolden(key.size_q as f32 / 8.0 * 0.025);
        }
        self.image.clear();
        if !render.render_into(&mut scaler, key.gid, &mut self.image) {
            return AtlasGlyph::default();
        }
        let p = self.image.placement;
        if p.width == 0 || p.height == 0 {
            return AtlasGlyph::default();
        }
        let is_color = self.image.content == Content::Color;
        let inserted = if is_color { self.color.insert(p.width, p.height, &self.image.data) } else { self.mask.insert(p.width, p.height, &self.image.data) };
        let (u, v) = match inserted {
            Some(at) => at,
            None => {
                // Full: start over. Everything holding coordinates re-lays out on the next epoch check.
                self.reset_atlases();
                let inserted = if is_color { self.color.insert(p.width, p.height, &self.image.data) } else { self.mask.insert(p.width, p.height, &self.image.data) };
                match inserted {
                    Some(at) => at,
                    None => return AtlasGlyph::default(),
                }
            }
        };
        AtlasGlyph { u, v, w: p.width as u16, h: p.height as u16, left: p.left as i16, top: p.top as i16, color: is_color }
    }

    /// A coloured SVG in the colour atlas, centred on (cx, cy) in physical pixels. `key` names the drawing: its source is
    /// only read the first time, so a list of a thousand file icons costs a hash lookup per row.
    pub fn svg_icon(&mut self, key: &str, svg: &str, cx: f32, cy: f32, px: u32) -> Option<GlyphInst> {
        let cache = (format!("svg:{key}"), px, 0);
        if !self.icons.contains_key(&cache) {
            let a = self.rasterize_svg(svg, px);
            self.icons.insert(cache.clone(), a);
        }
        let a = self.icons[&cache];
        if a.w == 0 {
            return None;
        }
        Some(GlyphInst { pos: [(cx - a.w as f32 * 0.5).round(), (cy - a.h as f32 * 0.5).round()], size: [a.w, a.h], uv: [a.u, a.v], color: Color::rgb(0xFFFFFF), clip: [0; 4], flags: crate::gpu::GLYPH_COLOR })
    }

    fn rasterize_svg(&mut self, svg: &str, px: u32) -> AtlasGlyph {
        let empty = AtlasGlyph { u: 0, v: 0, w: 0, h: 0, left: 0, top: 0, color: false };
        let Ok(tree) = resvg::usvg::Tree::from_str(svg, &resvg::usvg::Options::default()) else { return empty };
        let Some(mut pixmap) = resvg::tiny_skia::Pixmap::new(px, px) else { return empty };
        let size = tree.size();
        let k = px as f32 / size.width().max(size.height()).max(1.0);
        resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(k, k), &mut pixmap.as_mut());
        // The atlas holds straight alpha, which is what colour bitmaps come with, and the renderer premultiplies.
        let mut rgba = pixmap.take();
        for p in rgba.chunks_exact_mut(4) {
            let a = p[3] as u32;
            if a != 0 && a != 255 {
                for c in &mut p[..3] {
                    *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
                }
            }
        }
        let inserted = self.color.insert(px, px, &rgba).or_else(|| {
            self.reset_atlases();
            self.color.insert(px, px, &rgba)
        });
        let Some((u, v)) = inserted else { return empty };
        AtlasGlyph { u, v, w: px as u16, h: px as u16, left: 0, top: 0, color: true }
    }

    /// One line of text as straight-alpha RGBA in the context's scale: for what leaves the window as a picture, composited
    /// on a thread with no GPU. `None` for a line with nothing to draw.
    pub fn rasterize_line(&mut self, text: &str, ty: &TypeStyle, rgb: [u8; 3]) -> Option<(Vec<u8>, u32, u32)> {
        let color = Color::rgb(((rgb[0] as u32) << 16) | ((rgb[1] as u32) << 8) | rgb[2] as u32);
        let run = self.line(text, ty, color);
        let (w, h) = (run.width.ceil().max(1.0) as u32, run.height.ceil().max(1.0) as u32);
        if run.glyphs.is_empty() {
            return None;
        }
        let mut out = vec![0u8; (w * h * 4) as usize];
        for g in &run.glyphs {
            let color_glyph = g.flags & crate::gpu::GLYPH_COLOR != 0;
            let (atlas, bpp) = if color_glyph { (&self.color, 4usize) } else { (&self.mask, 1usize) };
            for gy in 0..g.size[1] as i32 {
                for gx in 0..g.size[0] as i32 {
                    let (x, y) = (g.pos[0] as i32 + gx, g.pos[1] as i32 + gy);
                    if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
                        continue;
                    }
                    let src = ((g.uv[1] as usize + gy as usize) * atlas.size as usize + g.uv[0] as usize + gx as usize) * bpp;
                    let at = ((y as u32 * w + x as u32) * 4) as usize;
                    if color_glyph {
                        out[at..at + 4].copy_from_slice(&atlas.data[src..src + 4]);
                    } else {
                        let coverage = atlas.data[src];
                        if coverage > out[at + 3] {
                            out[at..at + 3].copy_from_slice(&rgb);
                            out[at + 3] = coverage;
                        }
                    }
                }
            }
        }
        Some((out, w, h))
    }

    /// Sends newly rasterized glyphs to the GPU.
    /// A Lucide icon as a mask in the glyph atlas, centred on (cx, cy) in physical pixels.
    pub fn named_icon(&mut self, name: &str, cx: f32, cy: f32, px: u32, rotate: f32, color: Color) -> Option<GlyphInst> {
        let key = (name.to_owned(), px, (rotate * 4.0) as i32);
        if !self.icons.contains_key(&key) {
            let a = self.rasterize_icon(name, px, rotate);
            self.icons.insert(key.clone(), a);
        }
        let a = self.icons[&key];
        if a.w == 0 {
            return None;
        }
        Some(GlyphInst { pos: [(cx - a.w as f32 * 0.5).round(), (cy - a.h as f32 * 0.5).round()], size: [a.w, a.h], uv: [a.u, a.v], color, clip: [0; 4], flags: 0 })
    }

    fn rasterize_icon(&mut self, name: &str, px: u32, rotate: f32) -> AtlasGlyph {
        let empty = AtlasGlyph { u: 0, v: 0, w: 0, h: 0, left: 0, top: 0, color: false };
        let Ok(i) = crate::lucide::ICONS.binary_search_by(|(n, _)| (*n).cmp(name)) else {
            eprintln!("no such icon: {name}");
            return empty;
        };
        let body = crate::lucide::ICONS[i].1;
        let turn = if rotate != 0.0 { format!(r#"<g transform="rotate({rotate} 12 12)">{body}</g>"#) } else { body.to_owned() };
        let svg = format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">{turn}</svg>"#);
        let Ok(tree) = resvg::usvg::Tree::from_str(&svg, &resvg::usvg::Options::default()) else { return empty };
        let Some(mut pixmap) = resvg::tiny_skia::Pixmap::new(px, px) else { return empty };
        let k = px as f32 / 24.0;
        resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(k, k), &mut pixmap.as_mut());
        let alpha: Vec<u8> = pixmap.data().chunks_exact(4).map(|p| p[3]).collect();
        let Some((u, v)) = self.mask.insert(px, px, &alpha) else {
            self.reset_atlases();
            return empty;
        };
        AtlasGlyph { u, v, w: px as u16, h: px as u16, left: 0, top: 0, color: false }
    }

    pub fn flush_atlases(&mut self, renderer: &Renderer) {
        if let Some(rect) = self.mask.take_upload() {
            renderer.upload_mask_atlas(&AtlasUpload { data: &self.mask.data, bytes_per_pixel: 1, stride: self.mask.size, rect });
        }
        if let Some(rect) = self.color.take_upload() {
            renderer.upload_color_atlas(&AtlasUpload { data: &self.color.data, bytes_per_pixel: 4, stride: self.color.size, rect });
        }
    }

    fn place(&mut self, g: &ShapedGlyph, pen_x: f32, baseline: f32, style: StyleKey, color: Color) -> Option<GlyphInst> {
        let x = pen_x + g.x;
        let floor = x.floor();
        let bin = (((x - floor) * 4.0) as u8).min(3);
        let key = GlyphKey {
            face: g.face,
            gid: g.gid,
            size_q: style.size_q,
            scale_q: style.scale_q,
            bin,
            flags: style.semibold as u8 | (style.italic as u8) << 1,
        };
        let a = self.atlas_glyph(key);
        if a.w == 0 {
            return None;
        }
        Some(GlyphInst {
            pos: [floor + a.left as f32, baseline - g.y.round() - a.top as f32],
            size: [a.w, a.h],
            uv: [a.u, a.v],
            color,
            clip: [0; 4],
            flags: if a.color { GLYPH_COLOR } else { 0 },
        })
    }

    /// A single line of text, laid out once and then fetched from cache. Width and height are physical.
    pub fn line(&mut self, text: &str, ty: &TypeStyle, color: Color) -> Arc<LineRun> {
        let style = self.style_key(ty);
        let line_h = (ty.line * self.scale).round();
        self.line_in(text, style, line_h, color)
    }

    /// A single line in a named family: what a font menu's rows show. A family the system does not have gets the interface font,
    /// as CSS would give it the next one in the stack.
    pub fn family_line(&mut self, text: &str, family: &str, ty: &TypeStyle, color: Color) -> Arc<LineRun> {
        let style = StyleKey { family: self.fonts.family_id(family), ..self.style_key(ty) };
        let line_h = (ty.line * self.scale).round();
        self.line_in(text, style, line_h, color)
    }

    /// Whether the system has a font made for this: when it has not, `math_line` falls back to the interface font, which has
    /// no mathematical alphabets to speak of.
    pub fn has_math_face(&mut self) -> bool {
        self.fonts.ensure_math();
        self.fonts.math_face.is_some()
    }

    /// Behaves from here on as a system with no math face does. For tests, on systems that have one.
    #[doc(hidden)]
    pub fn forget_math_face(&mut self) {
        self.fonts.ensure_math();
        self.fonts.math_face = None;
        self.fonts.rebuild_math_chain();
        self.fonts.char_face.clear();
        self.words.clear();
        self.lines.clear();
    }

    /// How much larger a math face has to be drawn for its letters to stand as tall as the interface font's beside them.
    pub fn math_scale(&mut self) -> f32 {
        self.fonts.ensure_math();
        let x_height = |face: &Face| face.x_height / face.units_per_em.max(1.0);
        let (Some(math), Some(&ui)) = (self.fonts.math_face, self.fonts.ui.first()) else { return 1.0 };
        let (math, ui) = (x_height(&self.fonts.faces[math as usize]), x_height(&self.fonts.faces[ui as usize]));
        if math > 0.0 && ui > 0.0 { (ui / math).clamp(0.9, 1.4) } else { 1.0 }
    }

    /// A run of a formula: the system's math face first, then the interface chain. Size is logical. Colour is the caller's,
    /// through `LineRun::paint_tinted`.
    pub fn math_line(&mut self, text: &str, size: f32, bold: bool, italic: bool) -> Arc<LineRun> {
        self.fonts.ensure_math();
        let style = StyleKey { size_q: (size * self.scale * 8.0).round() as u16, scale_q: (self.scale * 8.0).round() as u16, semibold: bold, mono: false, italic, math: true, serif: false, family: 0 };
        let line_h = (size * self.scale * 1.4).round();
        self.line_in(text, style, line_h, Color::WHITE)
    }

    /// A filled path as a mask in the glyph atlas: what a font has no glyph for at this size, a bracket as tall as a matrix.
    /// `data` is SVG path data in physical pixels from the mask's top left, and names the drawing. `pos`, `color` and `clip`
    /// are the caller's to set. `None` when it could not fit, which must not empty the atlas for everybody else.
    pub fn path_mask(&mut self, data: &str, w: u32, h: u32) -> Option<GlyphInst> {
        if w == 0 || h == 0 || w > self.mask.size / 2 || h > self.mask.size / 2 {
            return None;
        }
        let key = (hash_str(data), w, h);
        let a = match self.paths.get(&key) {
            Some(a) => *a,
            None => {
                let mut pixels = vec![0u8; (w * h) as usize];
                Mask::new(data).size(w, h).format(Format::Alpha).render_into(&mut pixels, None);
                let inserted = self.mask.insert(w, h, &pixels).or_else(|| {
                    self.reset_atlases();
                    self.mask.insert(w, h, &pixels)
                });
                let (u, v) = inserted?;
                let a = AtlasGlyph { u, v, w: w as u16, h: h as u16, left: 0, top: 0, color: false };
                self.paths.insert(key, a);
                a
            }
        };
        Some(GlyphInst { pos: [0.0; 2], size: [a.w, a.h], uv: [a.u, a.v], color: Color::WHITE, clip: [0; 4], flags: 0 })
    }

    fn line_in(&mut self, text: &str, style: StyleKey, line_h: f32, color: Color) -> Arc<LineRun> {
        let key = LineKey { text: hash_str(text), style, color: color.into() };
        if let Some(run) = self.lines.get(&key)
            && run.epoch == self.epoch {
                return run.clone();
            }
        let baseline = self.baseline_in(style, line_h);
        let mut glyphs = Vec::with_capacity(text.len());
        let mut width = 0.0f32;
        let mut advance = 0.0f32;
        // Twice at most: if the atlas was reset halfway, the first glyphs of this line point at old coordinates. After
        // a reset it has room, so a single line cannot cause another one.
        for _ in 0..2 {
            let epoch = self.epoch;
            glyphs.clear();
            let mut pen = 0.0f32;
            let mut rest = text;
            while !rest.is_empty() {
                let n = next_segment(rest);
                let word = self.shape_word(&rest[..n], style);
                for g in &word.glyphs {
                    if let Some(inst) = self.place(g, pen, baseline, style, color) {
                        glyphs.push(inst);
                    }
                }
                width = pen + word.visible;
                pen += word.advance;
                rest = &rest[n..];
            }
            advance = pen;
            if self.epoch == epoch {
                break;
            }
        }
        let run = Arc::new(LineRun { glyphs, width, advance, height: line_h, baseline, epoch: self.epoch });
        if self.lines.len() > 30_000 {
            self.lines.clear();
        }
        self.lines.insert(key, run.clone());
        run
    }
}

impl LineRun {
    /// What the run actually draws around its origin on the baseline, in physical pixels: left, above, right, below.
    /// `None` when it draws nothing. Below is negative for what floats above the baseline, as a minus sign does.
    pub fn ink(&self) -> Option<[f32; 4]> {
        let first = self.glyphs.first()?;
        let mut bounds = [first.pos[0], first.pos[1], first.pos[0], first.pos[1]];
        for g in &self.glyphs {
            bounds[0] = bounds[0].min(g.pos[0]);
            bounds[1] = bounds[1].min(g.pos[1]);
            bounds[2] = bounds[2].max(g.pos[0] + g.size[0] as f32);
            bounds[3] = bounds[3].max(g.pos[1] + g.size[1] as f32);
        }
        Some([bounds[0], self.baseline - bounds[1], bounds[2], bounds[3] - self.baseline])
    }

    /// As `paint`, in a colour of the caller's, so a run shaped once can follow the theme.
    #[inline]
    pub fn paint_tinted(&self, out: &mut Vec<GlyphInst>, x: f32, y: f32, clip: [i16; 4], color: Color) {
        out.reserve(self.glyphs.len());
        for g in &self.glyphs {
            out.push(GlyphInst { pos: [g.pos[0] + x, g.pos[1] + y], clip, color, ..*g });
        }
    }

    #[inline]
    pub fn paint(&self, out: &mut Vec<GlyphInst>, x: f32, y: f32, clip: [i16; 4]) {
        out.reserve(self.glyphs.len());
        for g in &self.glyphs {
            out.push(GlyphInst { pos: [g.pos[0] + x, g.pos[1] + y], clip, ..*g });
        }
    }

    pub fn baseline(&self) -> f32 {
        self.baseline
    }
}

/// Whether a line may break after this character. A no-break space is whitespace that exists to say it may not.
fn breaks_after(c: char) -> bool {
    c.is_whitespace() && !matches!(c, '\u{a0}' | '\u{2007}' | '\u{202f}')
}

/// Length of the next unbreakable piece of `s`: a word with its trailing spaces, one CJK character with any
/// punctuation that must stay attached to it, or a newline on its own.
pub fn next_segment(s: &str) -> usize {
    const MAX_CHARS: usize = 64;
    let mut chars = s.char_indices().peekable();
    let Some((_, first)) = chars.next() else { return 0 };
    if first == '\n' {
        return 1;
    }
    let mut end = first.len_utf8();
    let cjk = is_cjk(first);
    let mut count = 1;
    let mut in_trailing_space = breaks_after(first);
    while let Some(&(i, c)) = chars.peek() {
        if c == '\n' {
            break;
        }
        if breaks_after(c) {
            in_trailing_space = true;
        } else if in_trailing_space {
            break;
        } else if cjk {
            if !no_line_start(c) {
                break;
            }
        } else if (is_cjk(c) && !no_line_start(c)) || count >= MAX_CHARS {
            break;
        }
        end = i + c.len_utf8();
        count += 1;
        chars.next();
    }
    end
}

// ---------------------------------------------------------------- documents

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockKind {
    Text,
    Heading,
    Code,
    Quote,
    Rule,
}

#[derive(Clone, Copy, Debug)]
pub struct Line {
    /// Top of the line box, relative to the document.
    pub y: f32,
    pub h: f32,
    first: u32,
    n: u32,
    first_caret: u32,
    n_carets: u32,
    /// Byte offset in the text where this line starts.
    byte: u32,
    pub kind: BlockKind,
    pub indent: f32,
    pub width: f32,
    block: u32,
    /// How much of `h` an inline box added above the line's text, and below it.
    pub above: f32,
    pub below: f32,
}

/// Room held in the flow of a document for something the owner draws itself.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InlineBox {
    /// Where its stand-in is in the text.
    pub byte: u32,
    /// Its left edge and its baseline, relative to the document, in physical pixels.
    pub x: f32,
    pub baseline: f32,
    line: u32,
}

#[derive(Clone, Copy)]
struct RunStyle {
    ty: TypeStyle,
    color: Color,
}

#[derive(Clone, Copy)]
enum Item {
    Block { kind: BlockKind, indent: f32, outdent: f32, space_before: f32, ty: TypeStyle },
    Text { start: u32, end: u32, style: u16 },
    Box { start: u32, advance: f32, above: f32, below: f32, ty: TypeStyle },
}

#[derive(Clone, Copy)]
struct Caret {
    byte: u32,
    x: f32,
}

/// What a float takes from the lines beside it, in physical pixels: a line that starts above `until` gives up `left` and `right`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Band {
    pub until: f32,
    pub left: f32,
    pub right: f32,
}

/// State of the flow at some point, so that an unfinished tail can be rewound and laid out again.
#[derive(Clone, Copy)]
struct Flow {
    x: f32,
    /// Where the current line starts and how far it may go, which a float beside it decides.
    start: f32,
    limit: f32,
    y: f32,
    line_h: f32,
    baseline: f32,
    indent: f32,
    kind: BlockKind,
    block: u32,
    n_lines: usize,
    n_insts: usize,
    n_carets: usize,
    line_width: f32,
    has_line: bool,
    pending_space: f32,
    /// How far the first line of the block starts left of `indent`, for hanging list bullets.
    pending_outdent: f32,
    /// The block's whole outdent, for a document that hangs every line it was given.
    outdent: f32,
    /// Byte offset of the text being placed.
    byte: u32,
    /// What inline boxes have added to the current line above its text, and below it.
    grow_above: f32,
    grow_below: f32,
    /// The last thing placed was an inline box, which punctuation after it must not be parted from.
    after_box: bool,
}

/// A growing document. Appends cost O(new text), and painting is a copy of the visible lines.
pub struct Doc {
    text: String,
    items: Vec<Item>,
    styles: Vec<RunStyle>,
    pub lines: Vec<Line>,
    insts: Vec<GlyphInst>,
    carets: Vec<Caret>,
    boxes: Vec<InlineBox>,
    flow: Flow,
    /// Bytes of `text` at the end that are laid out provisionally, because the word may still grow.
    tail: u32,
    tail_flow: Flow,
    tail_style: u16,
    /// Layout width in physical pixels.
    width: f32,
    /// Room something beside the text takes from its lines, top down.
    bands: Vec<Band>,
    epoch: u32,
    pub track_carets: bool,
    /// Keep at most this many lines (for live tiles). Zero means unbounded.
    pub max_lines: usize,
    /// A block's outdent applies after each `\n` as well as to its first line: a list drawn as one text, each line an item of its own.
    pub hang_each_line: bool,
    /// Set when the last mutation changed layout since the owner last looked.
    pub generation: u64,
}

const FRESH_FLOW: Flow = Flow { x: 0.0, start: 0.0, limit: 0.0, y: 0.0, line_h: 0.0, baseline: 0.0, indent: 0.0, kind: BlockKind::Text, block: 0, n_lines: 0, n_insts: 0, n_carets: 0, line_width: 0.0, has_line: false, pending_space: 0.0, pending_outdent: 0.0, outdent: 0.0, byte: 0, grow_above: 0.0, grow_below: 0.0, after_box: false };

impl Doc {
    pub fn new(width_physical: f32) -> Self {
        Self {
            text: String::new(),
            items: Vec::new(),
            styles: Vec::new(),
            lines: Vec::new(),
            insts: Vec::new(),
            carets: Vec::new(),
            boxes: Vec::new(),
            flow: FRESH_FLOW,
            tail: 0,
            tail_flow: FRESH_FLOW,
            tail_style: 0,
            width: width_physical.max(1.0),
            bands: Vec::new(),
            epoch: 0,
            track_carets: true,
            max_lines: 0,
            hang_each_line: false,
            generation: 0,
        }
    }

    /// What floats beside this document take from its lines, top down. For a document nothing has been pushed to.
    pub fn set_bands(&mut self, bands: Vec<Band>) {
        self.bands = bands;
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.items.clear();
        self.styles.clear();
        self.reset_layout();
        self.generation += 1;
    }

    fn reset_layout(&mut self) {
        self.lines.clear();
        self.insts.clear();
        self.carets.clear();
        self.boxes.clear();
        self.flow = FRESH_FLOW;
        self.tail = 0;
        self.tail_flow = FRESH_FLOW;
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.items.is_empty()
    }

    pub fn height(&self) -> f32 {
        self.lines.last().map_or(0.0, |l| l.y + l.h)
    }

    pub fn width(&self) -> f32 {
        self.width
    }

    pub fn glyph_count(&self) -> usize {
        self.insts.len()
    }

    /// Starts a new block. Text after this begins on a fresh line.
    pub fn begin_block(&mut self, ctx: &mut TextCtx, kind: BlockKind, indent: f32, space_before: f32, ty: TypeStyle) {
        self.begin_hanging_block(ctx, kind, indent, 0.0, space_before, ty);
    }

    /// As `begin_block`, with the first line starting `outdent` to the left of the following ones.
    pub fn begin_hanging_block(&mut self, ctx: &mut TextCtx, kind: BlockKind, indent: f32, outdent: f32, space_before: f32, ty: TypeStyle) {
        if self.epoch != ctx.epoch {
            self.reflow(ctx);
        }
        self.commit_tail();
        // Part of the text for copying, while layout has no use for it: a block always starts on its own line.
        if !self.text.is_empty() && !self.text.ends_with('\n') {
            self.text.push('\n');
            self.tail = self.text.len() as u32;
        }
        self.items.push(Item::Block { kind, indent, outdent, space_before, ty });
        self.apply_block(ctx, kind, indent, outdent, space_before, &ty);
        self.tail_flow = self.flow;
        self.tail = self.text.len() as u32;
        self.generation += 1;
    }

    fn apply_block(&mut self, ctx: &TextCtx, kind: BlockKind, indent: f32, outdent: f32, space_before: f32, ty: &TypeStyle) {
        let f = &mut self.flow;
        if f.has_line {
            f.y += f.line_h + f.grow_above + f.grow_below;
        }
        f.grow_above = 0.0;
        f.grow_below = 0.0;
        f.after_box = false;
        f.has_line = false;
        f.pending_space = space_before * ctx.scale;
        f.kind = kind;
        f.indent = indent * ctx.scale;
        f.outdent = outdent * ctx.scale;
        f.pending_outdent = f.outdent;
        f.x = f.indent;
        f.block += 1;
        f.line_h = (ty.line * ctx.scale).round();
        f.baseline = ctx.baseline_in(ctx.style_key(ty), f.line_h);
    }

    fn style_id(&mut self, style: RunStyle) -> u16 {
        if let Some(i) = self.styles.iter().position(|s| s.ty == style.ty && s.color == style.color) {
            return i as u16;
        }
        self.styles.push(style);
        self.styles.len() as u16 - 1
    }

    /// Appends text in the given style.
    pub fn push(&mut self, ctx: &mut TextCtx, s: &str, ty: TypeStyle, color: Color) {
        if s.is_empty() {
            return;
        }
        if self.epoch != ctx.epoch {
            self.reflow(ctx);
        }
        let style = self.style_id(RunStyle { ty, color });
        if style != self.tail_style {
            self.commit_tail();
            self.tail_style = style;
        }
        // The tail is rewound to this state before layout, so it has to know the line metrics too.
        self.ensure_line_metrics(ctx, &ty);
        let start = self.text.len() as u32;
        self.text.push_str(s);
        let end = self.text.len() as u32;
        match self.items.last_mut() {
            Some(Item::Text { end: e, style: st, .. }) if *st == style && *e == start => *e = end,
            _ => self.items.push(Item::Text { start, end, style }),
        }
        self.layout_tail(ctx, style);
        self.generation += 1;
        if self.max_lines > 0 && self.lines.len() > self.max_lines * 2 {
            self.trim_front(ctx);
        }
    }

    /// Holds room in the flow for something the owner draws, all in physical pixels: as wide as `advance`, standing `above`
    /// and hanging `below` the baseline. It wraps as one word does, and a line grows to take it in. The text gets U+FFFC in
    /// its place, for selection to count; where it landed is in `boxes`, in the order they were pushed.
    pub fn push_box(&mut self, ctx: &mut TextCtx, advance: f32, above: f32, below: f32, ty: TypeStyle) {
        if self.epoch != ctx.epoch {
            self.reflow(ctx);
        }
        self.commit_tail();
        self.ensure_line_metrics(ctx, &ty);
        let start = self.text.len() as u32;
        self.text.push('\u{FFFC}');
        self.items.push(Item::Box { start, advance, above, below, ty });
        self.place_box(start, advance, above, below);
        self.commit_tail();
        self.generation += 1;
    }

    pub fn boxes(&self) -> &[InlineBox] {
        &self.boxes
    }

    fn ensure_line_metrics(&mut self, ctx: &TextCtx, ty: &TypeStyle) {
        if self.flow.line_h == 0.0 {
            self.flow.line_h = (ty.line * ctx.scale).round();
            self.flow.baseline = ctx.baseline_in(ctx.style_key(ty), self.flow.line_h);
            self.tail_flow.line_h = self.flow.line_h;
            self.tail_flow.baseline = self.flow.baseline;
        }
    }

    fn place_box(&mut self, byte: u32, advance: f32, above: f32, below: f32) {
        self.flow.byte = byte;
        if !self.flow.has_line {
            self.open_line(false);
        }
        if self.flow.x + advance > self.flow.limit && self.flow.x > self.flow.start {
            self.open_line(true);
        }
        let f = &mut self.flow;
        let up = (above - f.baseline - f.grow_above).max(0.0).ceil();
        let down = (below - (f.line_h - f.baseline) - f.grow_below).max(0.0).ceil();
        f.grow_above += up;
        f.grow_below += down;
        let (grow_above, grow_below, line_h) = (f.grow_above, f.grow_below, f.line_h);
        let baseline = f.y + f.grow_above + f.baseline;
        let number = self.lines.len() as u32 - 1;
        if let Some(line) = self.lines.last_mut() {
            line.h = line_h + grow_above + grow_below;
            line.above = grow_above;
            line.below = grow_below;
            // What is already on the line moves down with its baseline.
            if up > 0.0 {
                for g in &mut self.insts[line.first as usize..] {
                    g.pos[1] += up;
                }
                for placed in self.boxes.iter_mut().rev().take_while(|b| b.line == number) {
                    placed.baseline += up;
                }
            }
        }
        let f = &mut self.flow;
        let x = f.x;
        f.x += advance;
        f.line_width = f.line_width.max(f.x);
        f.after_box = true;
        if self.track_carets {
            self.carets.push(Caret { byte, x });
        }
        self.boxes.push(InlineBox { byte, x, baseline, line: number });
    }

    /// Rewinds to the last committed point and lays out everything after it.
    fn layout_tail(&mut self, ctx: &mut TextCtx, style_id: u16) {
        self.flow = self.tail_flow;
        self.lines.truncate(self.flow.n_lines);
        self.insts.truncate(self.flow.n_insts);
        self.carets.truncate(self.flow.n_carets);

        let style = self.styles[style_id as usize];
        let key = ctx.style_key(&style.ty);
        let mut at = self.tail as usize;
        let text = std::mem::take(&mut self.text);
        while at < text.len() {
            let rest = &text[at..];
            let n = next_segment(rest);
            let segment = &rest[..n];
            self.place_segment(ctx, segment, at as u32, key, style.color);
            at += n;
            // A segment is final once something follows it, or if nothing can extend it.
            let last = segment.chars().next_back().unwrap_or(' ');
            if at < text.len() || last.is_whitespace() || is_cjk(last) {
                self.tail = at as u32;
                self.sync_flow();
                self.tail_flow = self.flow;
            }
        }
        self.text = text;
        self.sync_flow();
    }

    fn sync_flow(&mut self) {
        self.flow.n_lines = self.lines.len();
        self.flow.n_insts = self.insts.len();
        self.flow.n_carets = self.carets.len();
        if let Some(line) = self.lines.last_mut() {
            line.n = self.flow.n_insts as u32 - line.first;
            line.n_carets = self.flow.n_carets as u32 - line.first_caret;
            line.width = self.flow.line_width;
        }
    }

    fn commit_tail(&mut self) {
        self.tail = self.text.len() as u32;
        self.sync_flow();
        self.tail_flow = self.flow;
    }

    fn open_line(&mut self, advance: bool) {
        self.sync_flow();
        let f = &mut self.flow;
        if advance && f.has_line {
            f.y += f.line_h + f.grow_above + f.grow_below;
        }
        f.grow_above = 0.0;
        f.grow_below = 0.0;
        f.y += f.pending_space;
        f.pending_space = 0.0;
        let (left, right) = self.bands.iter().find(|band| f.y < band.until).map_or((0.0, 0.0), |band| (band.left, band.right));
        f.start = f.indent + left;
        f.limit = self.width - right;
        f.x = f.start - f.pending_outdent;
        f.pending_outdent = 0.0;
        f.line_width = f.x;
        f.has_line = true;
        self.lines.push(Line {
            y: f.y,
            h: f.line_h,
            first: self.insts.len() as u32,
            n: 0,
            first_caret: self.carets.len() as u32,
            n_carets: 0,
            byte: f.byte,
            kind: f.kind,
            indent: f.start,
            width: f.start,
            block: f.block,
            above: 0.0,
            below: 0.0,
        });
        self.flow.n_lines = self.lines.len();
    }

    fn place_segment(&mut self, ctx: &mut TextCtx, segment: &str, byte: u32, style: StyleKey, color: Color) {
        self.flow.byte = byte;
        if segment == "\n" {
            if !self.flow.has_line {
                self.open_line(false);
            }
            if self.track_carets {
                let x = self.flow.x;
                self.carets.push(Caret { byte, x });
            }
            self.flow.byte = byte + 1;
            if self.hang_each_line {
                self.flow.pending_outdent = self.flow.outdent;
            }
            self.open_line(true);
            return;
        }
        let word = ctx.shape_word(segment, style);
        if !self.flow.has_line {
            self.open_line(false);
        }
        // A comma after a formula stays with it, past the edge if it must, as it would after a word.
        let clings = std::mem::replace(&mut self.flow.after_box, false) && segment.chars().all(|c| no_line_start(c) || c.is_whitespace());
        // Wrap before the word if it does not fit and there is something on this line to wrap away from.
        if self.flow.x + word.visible > self.flow.limit && self.flow.x > self.flow.start && !clings {
            self.open_line(true);
        }
        // Whitespace must not begin a wrapped line's visible text, but it does keep its place in the text.
        let origin = self.flow.x;
        if origin + word.visible <= self.flow.limit || word.visible <= self.flow.limit - self.flow.start {
            for g in &word.glyphs {
                self.put_glyph(ctx, g, origin, byte, style, color);
            }
            self.flow.line_width = self.flow.line_width.max(origin + word.visible);
            self.flow.x = origin + word.advance;
        } else {
            // A single word wider than the line breaks wherever it has to.
            let mut pen = origin;
            for g in &word.glyphs {
                if pen + g.advance > self.flow.limit && pen > self.flow.start {
                    self.open_line(true);
                    pen = self.flow.x;
                }
                let at = ShapedGlyph { x: 0.0, ..*g };
                self.put_glyph(ctx, &at, pen, byte, style, color);
                pen += g.advance;
                self.flow.line_width = self.flow.line_width.max(pen);
            }
            self.flow.x = pen;
        }
    }

    #[inline]
    fn put_glyph(&mut self, ctx: &mut TextCtx, g: &ShapedGlyph, origin: f32, byte: u32, style: StyleKey, color: Color) {
        if self.track_carets {
            self.carets.push(Caret { byte: byte + g.cluster, x: origin + g.x });
        }
        let baseline = self.flow.y + self.flow.grow_above + self.flow.baseline;
        if let Some(inst) = ctx.place(g, origin, baseline, style, color) {
            self.insts.push(inst);
        }
    }

    /// Lays everything out again: after a width change, or when atlas coordinates went stale.
    pub fn reflow(&mut self, ctx: &mut TextCtx) {
        // If the atlas resets during this, the glyphs placed before it are stale, and the reset just made room for the
        // second pass. A document with more distinct glyphs than the atlas can hold can never be fully consistent, so
        // it stops there rather than laying out again on every frame.
        for _ in 0..2 {
            let epoch = ctx.epoch;
            self.layout_all(ctx);
            if ctx.epoch == epoch {
                break;
            }
        }
        self.epoch = ctx.epoch;
        self.generation += 1;
    }

    fn layout_all(&mut self, ctx: &mut TextCtx) {
        self.reset_layout();
        let items = std::mem::take(&mut self.items);
        let text = std::mem::take(&mut self.text);
        for item in &items {
            match *item {
                Item::Block { kind, indent, outdent, space_before, ty } => self.apply_block(ctx, kind, indent, outdent, space_before, &ty),
                Item::Text { start, end, style } => {
                    let spec = self.styles[style as usize];
                    let key = ctx.style_key(&spec.ty);
                    if self.flow.line_h == 0.0 {
                        self.flow.line_h = (spec.ty.line * ctx.scale).round();
                        self.flow.baseline = ctx.baseline_in(key, self.flow.line_h);
                    }
                    let mut at = start as usize;
                    while at < end as usize {
                        let n = next_segment(&text[at..end as usize]);
                        self.place_segment(ctx, &text[at..at + n], at as u32, key, spec.color);
                        at += n;
                    }
                    self.tail_style = style;
                }
                Item::Box { start, advance, above, below, ty } => {
                    self.ensure_line_metrics(ctx, &ty);
                    self.place_box(start, advance, above, below);
                }
            }
        }
        self.items = items;
        self.text = text;
        self.tail = self.text.len() as u32;
        self.sync_flow();
        self.tail_flow = self.flow;
    }

    pub fn set_width(&mut self, ctx: &mut TextCtx, width_physical: f32) {
        let w = width_physical.max(1.0);
        if (w - self.width).abs() > 0.5 {
            self.width = w;
            self.reflow(ctx);
        }
    }

    /// Keeps a live tile's tail cheap: when lines pile up, the oldest half is dropped from text and layout.
    fn trim_front(&mut self, ctx: &mut TextCtx) {
        let drop_to = self.lines.len() - self.max_lines;
        let cut = (self.lines[drop_to].byte as usize).min(self.text.len());
        let cut = (0..=cut).rev().find(|&i| self.text.is_char_boundary(i)).unwrap_or(0);
        self.text.drain(..cut);
        let style = self.tail_style;
        let end = self.text.len() as u32;
        self.items.clear();
        self.items.push(Item::Text { start: 0, end, style });
        self.reflow(ctx);
    }

    fn ensure_current(&mut self, ctx: &mut TextCtx) {
        if self.epoch != ctx.epoch {
            self.reflow(ctx);
        }
    }

    /// Copies visible glyphs into `out`. `y_range` is document-relative, in physical pixels.
    pub fn paint(&mut self, ctx: &mut TextCtx, out: &mut Vec<GlyphInst>, x: f32, y: f32, clip: [i16; 4], y_range: Range<f32>) {
        self.ensure_current(ctx);
        let (a, b) = self.visible_lines(y_range);
        if a >= b {
            return;
        }
        let first = self.lines[a].first as usize;
        let last = self.lines[b - 1];
        let end = (last.first + last.n) as usize;
        let end = end.max(first).min(self.insts.len());
        out.reserve(end - first);
        for g in &self.insts[first..end] {
            out.push(GlyphInst { pos: [g.pos[0] + x, g.pos[1] + y], clip, ..*g });
        }
    }

    /// Lines intersecting `y_range`, found by bisection, so that a long document costs no more than a short one.
    pub fn visible_lines(&self, y_range: Range<f32>) -> (usize, usize) {
        let a = self.lines.partition_point(|l| l.y + l.h <= y_range.start);
        let b = self.lines.partition_point(|l| l.y < y_range.end);
        (a, b.max(a))
    }

    /// Document position of the byte offset nearest to a point, for carets and selection.
    pub fn hit(&self, x: f32, y: f32) -> usize {
        if self.lines.is_empty() {
            return 0;
        }
        let i = self.lines.partition_point(|l| l.y + l.h <= y).min(self.lines.len() - 1);
        let line = self.lines[i];
        let carets = &self.carets[line.first_caret as usize..(line.first_caret + line.n_carets) as usize];
        for pair in carets.windows(2) {
            if x < (pair[0].x + pair[1].x) * 0.5 {
                return pair[0].byte as usize;
            }
        }
        match carets.last() {
            None => self.next_line_start(i),
            Some(c) => {
                // The last glyph's far edge is the line's width, so its midpoint splits the two outcomes.
                if x < (c.x + line.width.max(c.x)) * 0.5 {
                    return c.byte as usize;
                }
                self.next_line_start(i)
            }
        }
    }

    fn next_line_start(&self, i: usize) -> usize {
        match self.lines.get(i + 1) {
            Some(next) if next.n_carets > 0 => {
                let byte = self.carets[next.first_caret as usize].byte as usize;
                // Stepping back over a newline that ended this line keeps the caret on it.
                if self.text[..byte].ends_with('\n') { byte - 1 } else { byte }
            }
            _ => self.text.len(),
        }
    }

    /// Position of a caret at byte offset `byte`: (x, y, height).
    pub fn caret_pos(&self, byte: usize) -> (f32, f32, f32) {
        let Some(last_line) = self.lines.last() else {
            return (0.0, 0.0, self.flow.line_h);
        };
        for line in self.lines.iter() {
            let carets = &self.carets[line.first_caret as usize..(line.first_caret + line.n_carets) as usize];
            if let Some(c) = carets.iter().find(|c| c.byte as usize >= byte) {
                return (c.x, line.y, line.h);
            }
        }
        (self.flow.x.max(last_line.indent), last_line.y, last_line.h)
    }

    /// Highlight rectangles for a byte range, as (x, y, w, h).
    pub fn selection_rects(&self, range: Range<usize>, y_range: Range<f32>, out: &mut Vec<[f32; 4]>) {
        let (a, b) = self.visible_lines(y_range);
        for line in &self.lines[a..b] {
            let carets = &self.carets[line.first_caret as usize..(line.first_caret + line.n_carets) as usize];
            let mut x0 = None;
            let mut x1 = line.indent;
            for c in carets {
                let byte = c.byte as usize;
                if byte >= range.start && byte < range.end {
                    x0 = Some(x0.unwrap_or(c.x));
                }
                if byte >= range.end {
                    x1 = c.x;
                    break;
                }
                x1 = line.width;
            }
            if let Some(x0) = x0 {
                out.push([x0, line.y, (x1 - x0).max(2.0), line.h]);
            }
        }
    }

    /// Block backgrounds: for each contiguous run of `kind` lines in view, its (first y, last y + h, indent).
    pub fn block_spans(&self, kind: BlockKind, y_range: Range<f32>, out: &mut Vec<[f32; 3]>) {
        let (a, b) = self.visible_lines(y_range);
        // Start from the block's true top even if that line has scrolled out, so its rounded corners stay put.
        let mut i = a;
        while i < b {
            let line = self.lines[i];
            if line.kind != kind {
                i += 1;
                continue;
            }
            let block = line.block;
            let mut start = i;
            while start > 0 && self.lines[start - 1].block == block {
                start -= 1;
            }
            let mut end = i;
            while end + 1 < self.lines.len() && self.lines[end + 1].block == block {
                end += 1;
            }
            out.push([self.lines[start].y, self.lines[end].y + self.lines[end].h, line.indent]);
            i = end + 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{BODY, BODY_STRONG};

    fn ctx() -> TextCtx {
        TextCtx::new(2.0)
    }

    #[test]
    fn segments_keep_words_and_split_cjk() {
        fn split(s: &str) -> Vec<&str> {
            let mut out = Vec::new();
            let mut rest = s;
            while !rest.is_empty() {
                let n = next_segment(rest);
                out.push(&rest[..n]);
                rest = &rest[n..];
            }
            out
        }
        assert_eq!(split("hello  world\nnext"), vec!["hello  ", "world", "\n", "next"]);
        assert_eq!(split("流式渲染，很快。"), vec!["流", "式", "渲", "染，", "很", "快。"]);
        assert_eq!(split("abc中文def"), vec!["abc", "中", "文", "def"]);
        assert_eq!(split("a\n\nb"), vec!["a", "\n", "\n", "b"]);
    }

    #[test]
    fn segmentation_never_loses_or_reorders_bytes() {
        let s = "mix of 中文，words\n\n  spaces🦀 and a_very_long_word_that_goes_on_and_on_and_on_and_on_and_on_and_on_and_on_and_on_and_on!";
        let mut rest = s;
        let mut rebuilt = String::new();
        while !rest.is_empty() {
            let n = next_segment(rest);
            assert!(n > 0);
            rebuilt.push_str(&rest[..n]);
            rest = &rest[n..];
        }
        assert_eq!(rebuilt, s);
    }

    /// The invariant that makes streaming safe: however the text is split into tokens, the result is the same.
    #[test]
    fn appending_in_pieces_equals_laying_out_at_once() {
        let mut ctx = ctx();
        let text = "Streaming text arrives in arbitrary pieces, 中文也一样，and  double  spaces\nwith newlines and a_rather_long_identifier_name_here too. 🦀 End";
        let mut whole = Doc::new(600.0);
        whole.push(&mut ctx, text, BODY, Color::WHITE);
        for chunk_len in [1, 2, 3, 5, 7, 13] {
            let mut doc = Doc::new(600.0);
            let chars: Vec<char> = text.chars().collect();
            for piece in chars.chunks(chunk_len) {
                let s: String = piece.iter().collect();
                doc.push(&mut ctx, &s, BODY, Color::WHITE);
            }
            assert_eq!(doc.lines.len(), whole.lines.len(), "chunk {chunk_len}");
            assert_eq!(doc.insts.len(), whole.insts.len(), "chunk {chunk_len}");
            for (a, b) in doc.insts.iter().zip(&whole.insts) {
                assert_eq!(a.pos, b.pos, "chunk {chunk_len}");
                assert_eq!(a.uv, b.uv, "chunk {chunk_len}");
            }
        }
    }

    #[test]
    fn reflow_matches_incremental_layout() {
        let mut ctx = ctx();
        let mut doc = Doc::new(500.0);
        for word in "one two three four five six seven eight nine ten eleven twelve thirteen".split_inclusive(' ') {
            doc.push(&mut ctx, word, BODY, Color::WHITE);
        }
        let before: Vec<[f32; 2]> = doc.insts.iter().map(|g| g.pos).collect();
        doc.reflow(&mut ctx);
        let after: Vec<[f32; 2]> = doc.insts.iter().map(|g| g.pos).collect();
        assert_eq!(before, after);
    }

    #[test]
    fn long_lines_wrap_inside_the_width() {
        let mut ctx = ctx();
        let width = 300.0;
        let mut doc = Doc::new(width);
        doc.push(&mut ctx, &"word ".repeat(100), BODY, Color::WHITE);
        doc.push(&mut ctx, &"x".repeat(300), BODY, Color::WHITE);
        assert!(doc.lines.len() > 5);
        for g in &doc.insts {
            assert!(g.pos[0] + g.size[0] as f32 <= width + 4.0, "glyph at {} past the width", g.pos[0]);
        }
        let ys: Vec<f32> = doc.lines.iter().map(|l| l.y).collect();
        assert!(ys.windows(2).all(|w| w[1] > w[0]), "lines must stack downwards");
    }

    #[test]
    fn appending_does_not_reshape_earlier_words() {
        let mut ctx = ctx();
        let mut doc = Doc::new(800.0);
        for i in 0..2000 {
            doc.push(&mut ctx, &format!("word{} ", i % 50), BODY, Color::WHITE);
        }
        assert!(ctx.stats.shaped_words <= 51, "shaped {} words for 50 distinct ones", ctx.stats.shaped_words);
        assert!(ctx.stats.word_hits > 1900);
    }

    #[test]
    fn every_line_given_to_a_hanging_document_starts_at_the_edge_and_what_wraps_stands_in() {
        let mut ctx = ctx();
        let mut doc = Doc::new(600.0);
        doc.hang_each_line = true;
        doc.begin_hanging_block(&mut ctx, BlockKind::Text, 20.0, 20.0, 0.0, BODY);
        let line = "word ".repeat(40);
        doc.push(&mut ctx, &format!("{}\n{}", line.trim_end(), line.trim_end()), BODY, Color::WHITE);
        let starts = |doc: &Doc| doc.lines.iter().map(|l| (l.byte as usize, doc.carets[l.first_caret as usize].x)).collect::<Vec<_>>();
        let check = |doc: &Doc| {
            let second = line.trim_end().len() + 1;
            let lines = starts(doc);
            assert!(lines.len() > 4, "{lines:?}");
            for (byte, x) in lines {
                let expected = if byte == 0 || byte == second { 0.0 } else { 40.0 };
                assert_eq!(x, expected, "the line at byte {byte}");
            }
        };
        check(&doc);
        // A narrower box lays it all out again the same way.
        doc.set_width(&mut ctx, 400.0);
        check(&doc);
        assert!(doc.lines.iter().all(|l| l.width <= 400.0));
    }

    #[test]
    fn styled_runs_and_blocks_stack_correctly() {
        let mut ctx = ctx();
        let mut doc = Doc::new(800.0);
        doc.push(&mut ctx, "plain ", BODY, Color::WHITE);
        doc.push(&mut ctx, "bold", BODY_STRONG, Color::WHITE);
        let one_line = doc.lines.len();
        doc.begin_block(&mut ctx, BlockKind::Code, 16.0, 8.0, crate::theme::MONO);
        doc.push(&mut ctx, "let x = 1;", crate::theme::MONO, Color::WHITE);
        assert_eq!(one_line, 1);
        assert_eq!(doc.lines.len(), 2);
        assert_eq!(doc.lines[1].kind, BlockKind::Code);
        assert!(doc.lines[1].y >= doc.lines[0].y + doc.lines[0].h + 8.0 * ctx.scale - 0.5);
        assert!((doc.lines[1].indent - 32.0).abs() < 0.1);
    }

    /// An atlas reset during layout used to be hidden by the document recording the new epoch as it went: glyphs placed
    /// before the reset kept coordinates that now belong to other glyphs, so the wrong characters were drawn.
    #[test]
    fn a_reset_during_layout_does_not_leave_wrong_glyphs_behind() {
        let mut ctx = TextCtx::with_atlas_sizes(2.0, 256, 64);
        let distinct = |from: u32, n: u32| -> String { (from..from + n).filter_map(char::from_u32).collect() };
        // Fills most of the atlas.
        let mut first = Doc::new(100_000.0);
        first.push(&mut ctx, &distinct(0x4E00, 60), BODY, Color::WHITE);
        let before = ctx.stats.atlas_resets;
        // This one no longer fits, so a reset happens partway through its layout.
        let mut second = Doc::new(100_000.0);
        let text = distinct(0x5000, 40);
        for c in text.chars() {
            second.push(&mut ctx, &c.to_string(), BODY, Color::WHITE);
        }
        assert!(ctx.stats.atlas_resets > before, "the test needs a reset to happen");

        let mut out = Vec::new();
        second.paint(&mut ctx, &mut out, 0.0, 0.0, [0, 0, i16::MAX, i16::MAX], 0.0..10_000.0);
        assert_eq!(out.len(), 40);
        let mut cells: Vec<[u16; 2]> = out.iter().map(|g| g.uv).collect();
        cells.sort_unstable();
        cells.dedup();
        assert_eq!(cells.len(), 40, "forty different characters must occupy forty different atlas cells");
    }

    #[test]
    fn a_document_larger_than_the_atlas_does_not_reflow_forever() {
        let mut ctx = TextCtx::with_atlas_sizes(2.0, 128, 64);
        let mut doc = Doc::new(100_000.0);
        let text: String = (0x4E00u32..0x4E00 + 200).filter_map(char::from_u32).collect();
        doc.push(&mut ctx, &text, BODY, Color::WHITE);
        let mut out = Vec::new();
        doc.paint(&mut ctx, &mut out, 0.0, 0.0, [0, 0, i16::MAX, i16::MAX], 0.0..10_000.0);
        let generation = doc.generation;
        for _ in 0..3 {
            out.clear();
            doc.paint(&mut ctx, &mut out, 0.0, 0.0, [0, 0, i16::MAX, i16::MAX], 0.0..10_000.0);
        }
        assert_eq!(doc.generation, generation, "painting must settle instead of laying out on every frame");
    }

    #[test]
    fn blocks_separate_lines_in_the_copied_text() {
        let mut ctx = ctx();
        let mut doc = Doc::new(800.0);
        doc.push(&mut ctx, "one", BODY, Color::WHITE);
        doc.begin_block(&mut ctx, BlockKind::Text, 0.0, 0.0, BODY);
        doc.push(&mut ctx, "two", BODY, Color::WHITE);
        assert_eq!(doc.text(), "one\ntwo");
        assert_eq!(doc.lines.len(), 2, "the separator must not add a line of its own");
        doc.reflow(&mut ctx);
        assert_eq!(doc.lines.len(), 2);
    }

    #[test]
    fn an_inline_box_holds_room_in_the_line_and_says_where_it_landed() {
        let mut ctx = ctx();
        let mut doc = Doc::new(10_000.0);
        doc.push(&mut ctx, "ab ", BODY, Color::WHITE);
        let before = doc.lines[0].width;
        let x_after_text = doc.caret_pos(3).0;
        doc.push_box(&mut ctx, 50.0, 10.0, 2.0, BODY);
        doc.push(&mut ctx, "cd", BODY, Color::WHITE);
        assert_eq!(doc.text(), "ab \u{FFFC}cd");
        assert_eq!(doc.lines.len(), 1);
        assert!(doc.lines[0].width > before + 50.0);
        let placed = doc.boxes()[0];
        assert_eq!(placed.byte, 3);
        assert!((placed.x - x_after_text).abs() < 0.01 || placed.x >= before);
        // What follows starts past it, and a click on either half lands on either side of it.
        assert!((doc.caret_pos(3 + '\u{FFFC}'.len_utf8()).0 - (placed.x + 50.0)).abs() < 0.01);
        let y = doc.lines[0].y + 1.0;
        assert_eq!(doc.hit(placed.x + 10.0, y), 3);
        assert_eq!(doc.hit(placed.x + 40.0, y), 3 + '\u{FFFC}'.len_utf8());
    }

    #[test]
    fn a_tall_box_grows_its_line_and_keeps_every_baseline_together() {
        let mut ctx = ctx();
        let mut doc = Doc::new(10_000.0);
        doc.push(&mut ctx, "low", BODY, Color::WHITE);
        let flat_h = doc.lines[0].h;
        let glyph_y = doc.insts[0].pos[1];
        let baseline = doc.flow.baseline;
        doc.push_box(&mut ctx, 20.0, 200.0, 100.0, BODY);
        let line = doc.lines[0];
        assert_eq!(line.h, line.above + flat_h + line.below);
        assert!(line.h >= 300.0, "{line:?}");
        assert_eq!(doc.boxes()[0].baseline, line.above + baseline);
        // The text already on the line came down with the baseline.
        assert_eq!(doc.insts[0].pos[1], glyph_y + line.above);
        doc.push(&mut ctx, "\nnext", BODY, Color::WHITE);
        assert_eq!(doc.lines[1].y, line.y + line.h);
        assert_eq!(doc.lines[1].h, flat_h);
    }

    #[test]
    fn a_box_wraps_as_a_word_and_survives_a_reflow() {
        let mut ctx = ctx();
        let mut doc = Doc::new(300.0);
        doc.push(&mut ctx, "words ", BODY, Color::WHITE);
        doc.push_box(&mut ctx, 280.0, 60.0, 20.0, BODY);
        doc.push(&mut ctx, ", then", BODY, Color::WHITE);
        doc.push_box(&mut ctx, 280.0, 60.0, 20.0, BODY);
        let before: Vec<InlineBox> = doc.boxes().to_vec();
        let lines: Vec<(f32, f32)> = doc.lines.iter().map(|l| (l.y, l.h)).collect();
        assert_eq!(before.len(), 2);
        assert_eq!(before[0].x, 0.0, "the first box did not fit after the word");
        assert_eq!(before[1].x, 0.0, "nor did the second after the first");
        assert!(before[1].baseline > before[0].baseline);
        // The comma that follows stays on the box's line.
        assert_eq!(doc.lines.iter().position(|l| l.byte == doc.text().find('\u{FFFC}').unwrap() as u32), Some(1));
        doc.reflow(&mut ctx);
        assert_eq!(doc.boxes(), &before[..]);
        assert_eq!(doc.lines.iter().map(|l| (l.y, l.h)).collect::<Vec<_>>(), lines);
    }

    #[test]
    fn a_float_takes_room_from_the_lines_beside_it_and_gives_it_back_under_it() {
        let mut ctx = ctx();
        let mut doc = Doc::new(400.0);
        // BODY's lines are 40 physical pixels or so here: this reaches past the second.
        doc.set_bands(vec![Band { until: 70.0, left: 100.0, right: 0.0 }, Band { until: 140.0, left: 0.0, right: 200.0 }]);
        doc.push(&mut ctx, &"word ".repeat(100), BODY, Color::WHITE);
        let beside: Vec<Line> = doc.lines.iter().copied().filter(|l| l.y < 70.0).collect();
        assert!(beside.iter().all(|l| l.indent == 100.0 && l.width <= 400.0), "from the left: {beside:?}");
        let narrower: Vec<Line> = doc.lines.iter().copied().filter(|l| l.y >= 70.0 && l.y < 140.0).collect();
        assert!(!narrower.is_empty() && narrower.iter().all(|l| l.indent == 0.0 && l.width <= 200.0), "from the right: {narrower:?}");
        let under: Vec<Line> = doc.lines.iter().copied().filter(|l| l.y >= 140.0).collect();
        assert!(under.iter().any(|l| l.width > 300.0), "and all of it back: {under:?}");
        // A reflow keeps to them.
        let lines = doc.lines.len();
        doc.reflow(&mut ctx);
        assert_eq!(doc.lines.len(), lines);
    }

    #[test]
    fn a_line_in_a_family_of_its_own_differs_from_the_interface_font_and_a_missing_one_does_not() {
        let mut ctx = ctx();
        let plain = ctx.line("Illumination", &BODY, Color::WHITE).width;
        assert_eq!(ctx.family_line("Illumination", "No Such Family Anywhere", &BODY, Color::WHITE).width, plain, "the next in the stack");
        // Whatever monospace face this system has: not every family is installed everywhere, and one that is differs.
        let installed = ["Menlo", "Courier New", "DejaVu Sans Mono", "Consolas"].iter().find(|family| ctx.family_line("Illumination", family, &BODY, Color::WHITE).width != plain);
        assert!(installed.is_some(), "a monospace family sets this text wider or narrower");
    }

    #[test]
    fn a_math_run_reports_its_ink_and_a_path_lands_in_the_atlas() {
        let mut ctx = ctx();
        let x = ctx.math_line("x", 40.0, false, false);
        let [left, above, right, below] = x.ink().expect("x draws something");
        assert!(right > left && above > 0.0 && below.abs() < 8.0, "{:?}", x.ink());
        let minus = ctx.math_line("\u{2212}", 40.0, false, false);
        let [_, above, _, below] = minus.ink().expect("a minus draws something");
        assert!(above > 0.0 && below < 0.0, "a minus floats above the baseline: {above} {below}");
        assert!(ctx.math_line(" ", 40.0, false, false).ink().is_none());
        assert!(ctx.path_mask("M 0 0 L 10 0 L 10 10 Z", 10, 10).is_some());
        assert!(ctx.path_mask("M 0 0 L 10 0 L 10 10 Z", 100_000, 10).is_none(), "no drawing may empty the atlas");
        let resets = ctx.stats.atlas_resets;
        assert_eq!(ctx.path_mask("M 0 0 L 10 0 L 10 10 Z", 10, 10).map(|g| g.size), Some([10, 10]));
        assert_eq!(ctx.stats.atlas_resets, resets);
    }

    #[test]
    fn hit_testing_round_trips_through_caret_positions() {
        let mut ctx = ctx();
        let mut doc = Doc::new(10_000.0);
        let text = "hello world";
        doc.push(&mut ctx, text, BODY, Color::WHITE);
        for byte in 0..text.len() {
            let (x, y, h) = doc.caret_pos(byte);
            assert_eq!(doc.hit(x + 0.01, y + h * 0.5), byte, "byte {byte}");
        }
    }

    #[test]
    fn trimmed_tile_keeps_the_end_of_the_text() {
        // Both ways, because tiles turn caret tracking off and that once made a trim wipe everything.
        for track in [true, false] {
            let mut ctx = ctx();
            let mut doc = Doc::new(200.0);
            doc.track_carets = track;
            doc.max_lines = 4;
            let mut min_lines_after_start = usize::MAX;
            for i in 0..1000 {
                doc.push(&mut ctx, &format!("tok{i} "), BODY, Color::WHITE);
                if i > 100 {
                    min_lines_after_start = min_lines_after_start.min(doc.lines.len());
                }
            }
            assert!(doc.lines.len() <= 8, "{} lines retained", doc.lines.len());
            assert!(min_lines_after_start >= 4, "track={track}: a trim emptied the tile down to {min_lines_after_start} lines");
            assert!(doc.text().ends_with("tok999 "), "lost the tail: {:?}", &doc.text()[doc.text().len().saturating_sub(20)..]);
            assert!(doc.text().len() < 400);
        }
    }
}
