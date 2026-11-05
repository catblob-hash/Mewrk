//! TeX math read the way MathJax reads it with the packages `lib/mathJaxEngine.ts` loads: source in, a tree of `Node`s out.
//! `math_layout.rs` turns the tree into something to draw. What `tests/mathjax_commands.txt` lists is every name those packages
//! define; the base and the AMS ones are here, the others in `tex/packages.rs`, and `\ce` in `tex/mhchem.rs`.
//!
//! A command this does not know is not an error, as `noundefined` has it: its name shows in red and the rest of the formula goes
//! on. What is an error is what stops MathJax's parser too, with its message, because a half-streamed `\frac{a}{` has to fail
//! rather than draw half a fraction. An argument is read as raw text and parsed on its own and a macro is spliced into the input,
//! both as MathJax does, so anything that breaks there breaks here for the same reason.

use fluent::theme::Color;
use std::cell::RefCell;
use std::collections::HashMap;

mod mhchem;
mod packages;

use packages::{text_accent, text_literal};
pub(super) use packages::COMMANDS;

/// How far groups may nest. Nothing a person or a model writes comes near it; the point is that recursion ends well before the
/// stack does, on the smallest stack this runs on.
const MAX_DEPTH: u32 = 40;
const TOO_DEEP: &str = "Formula is nested too deeply";
/// `MaxMacroSub1`, and the size a macro's expansions may bring the input to: a macro may well call itself.
const MAX_EXPANSIONS: u32 = 10_000;
const MAX_BUFFER: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Class {
    Ord,
    Op,
    Bin,
    Rel,
    Open,
    Close,
    Punct,
    Inner,
}

/// A `\mathbf` and its kind. `Auto` is math's default: letters slant, digits and everything else stand upright.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum MathFont {
    Auto,
    Roman,
    Bold,
    Italic,
    BoldItalic,
    Sans,
    SansBold,
    SansItalic,
    Mono,
    Script,
    BoldScript,
    Fraktur,
    BoldFraktur,
    DoubleStruck,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum MathStyle {
    Display,
    Text,
    Script,
    ScriptScript,
}

/// A horizontal or vertical measure: in `em`s of whatever size it lands in, which also covers `mu`.
pub(super) type Em = f32;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum AccentKind {
    Hat,
    Check,
    Tilde,
    Acute,
    Grave,
    Dot,
    Ddot,
    Dddot,
    Breve,
    Bar,
    Vec,
    Ring,
    Ddddot,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum OverKind {
    Overline,
    Underline,
    Overbrace,
    Underbrace,
    OverArrow { left: bool, right: bool },
    UnderArrow { left: bool, right: bool },
    /// `\overparen`, `\underparen`.
    OverParen,
    UnderParen,
    /// `\overbracket[thickness][height]` and its mirror.
    OverBracket { thick: Em, height: Em },
    UnderBracket { thick: Em, height: Em },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ArrowHead {
    Single,
    Double,
    Harpoons,
    Mapsto,
    Hook,
    /// `\xlongequal`: two rules, no head.
    Equal,
    /// `\xtofrom`: an arrow each way.
    ToFrom,
    /// `\xtwoheadrightarrow`.
    TwoHead,
    /// `\xrightharpoonup` and its kind: `true` has its barb above the line.
    Harpoon(bool),
    /// `\Newextarrow`, with the glyph it names at either end of a rule.
    Glyph(char),
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct XArrow {
    pub left: bool,
    pub right: bool,
    pub head: ArrowHead,
    /// Extra room on the label's left and right, in `mu`: the numbers MathJax's `xArrow` is given.
    pub pad: (Em, Em),
}

#[derive(Debug)]
pub(super) struct Frac {
    pub num: Vec<Node>,
    pub den: Vec<Node>,
    /// The bar's thickness, or `None` for the default one. Zero is `\atop`.
    pub bar: Option<Em>,
    pub style: Option<MathStyle>,
    /// `\binom`'s brackets. A NUL is none.
    pub left: char,
    pub right: char,
    /// `\cfrac`: both parts stay at full size, whatever the style around them.
    pub continued: bool,
    /// Where the numerator and the denominator stand against the wider of them: -1 left, 0 centred, 1 right.
    pub align: (i8, i8),
    /// `\nicefrac`: `bevelled` in MathML, one part up and one down either side of a slash.
    pub bevelled: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum VAlign {
    Center,
    Top,
    Bottom,
}

#[derive(Debug, Default)]
pub(super) struct Row {
    pub cells: Vec<Vec<Node>>,
    /// `\\[1ex]`.
    pub extra: Em,
    /// A `\hline` above this row.
    pub rule_above: bool,
    /// It was a `\hdashline`.
    pub dashed_above: bool,
    /// `\cellcolor`, by column.
    pub fills: Vec<Option<Color>>,
    /// `\rowcolor`.
    pub fill: Option<Color>,
    /// `\tag` on this row.
    pub tag: Option<Vec<Node>>,
    /// `\shoveleft`, `\shoveright`, or where its place in a `multline` puts it.
    pub shove: Option<char>,
}

#[derive(Debug)]
pub(super) struct Array {
    pub rows: Vec<Row>,
    /// A `\hline` after the last row.
    pub rule_below: bool,
    pub dashed_below: bool,
    /// One letter per column: `l`, `c`, `r`. Past the last, the last goes on repeating, in pairs for an `align`.
    pub columns: Vec<char>,
    /// How many vertical rules stand before each column, and after the last.
    pub rules: Vec<u8>,
    /// Which of them are dashed: `:` in the column spec, by the same place.
    pub dashed: Vec<bool>,
    /// `\columncolor`, by column.
    pub fills: Vec<Option<Color>>,
    /// A column spec's fixed widths, `p{2cm}` and its kind.
    pub widths: Vec<Option<Em>>,
    /// Places where `@{...}` took the gap away, by the column it follows.
    pub no_gap: Vec<bool>,
    pub style: MathStyle,
    /// Between the columns, or by pairs when `paired`.
    pub col_gap: Em,
    pub paired: bool,
    pub baseline: Em,
    /// Added between rows: `\jot`.
    pub jot: Em,
    pub left: char,
    pub right: char,
    pub valign: VAlign,
    /// A `multline`: each row stands on its own, the first left and the last right, across the width of the widest.
    pub multline: bool,
    /// A `\begin{CD}`: its arrows take the size of their column or their row.
    pub diagram: bool,
    /// A `gather`, whose cells may hold another equation structure.
    pub gather: bool,
}

/// One `menclose` notation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Notation {
    LongDiv,
    Actuarial,
    Radical,
    Box,
    RoundedBox,
    Circle,
    Left,
    Right,
    Top,
    Bottom,
    UpDiagonalStrike,
    DownDiagonalStrike,
    VerticalStrike,
    HorizontalStrike,
    UpDiagonalArrow,
    NorthEastArrow,
    SouthEastArrow,
    SouthWestArrow,
    NorthWestArrow,
    UpArrow,
    DownArrow,
    LeftArrow,
    RightArrow,
    UpDownArrow,
    LeftRightArrow,
    PhasorAngle,
    Madruwb,
}

/// `\enclose`, `\cancel`, `\framebox` and whatever else draws around or across what it holds.
#[derive(Debug)]
pub(super) struct Enclose {
    pub notations: Vec<Notation>,
    pub color: Option<Color>,
    pub fill: Option<Color>,
    /// In `em`s: `data-padding`.
    pub padding: Option<Em>,
    /// In `em`s: `data-thickness`.
    pub thickness: Option<Em>,
    /// `data-arrowhead`'s `x`, `y` and `dx`, in units of the default.
    pub arrowhead: (Em, Em, Em),
    pub body: Vec<Node>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum LineStyle {
    Solid,
    Dashed,
    Dotted,
    Double,
}

/// One side of a `\bbox`'s border.
#[derive(Clone, Copy, Debug)]
pub(super) struct Border {
    pub width: Em,
    pub style: LineStyle,
    pub color: Option<Color>,
}

/// `\bbox[5px,#fee,border:1px solid red]{...}`.
#[derive(Debug)]
pub(super) struct BBox {
    /// Padding on the top, the right, the bottom and the left.
    pub padding: [Em; 4],
    pub fill: Option<Color>,
    /// The same way round.
    pub borders: [Option<Border>; 4],
    pub body: Vec<Node>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ProofLine {
    None,
    Solid,
    Dashed,
}

/// A step of a `prooftree`.
#[derive(Debug)]
pub(super) struct Proof {
    pub premises: Vec<Proof>,
    pub conclusion: Vec<Node>,
    /// `\Axiom$A \fCenter B$`: the conclusion in two parts, lined up on the point between them.
    pub centered: Option<Vec<Node>>,
    pub left: Option<Vec<Node>>,
    pub right: Option<Vec<Node>>,
    pub line: ProofLine,
    pub root_at_top: bool,
    /// `false` for what `\AxiomC` leaves: no rule of its own.
    pub inference: bool,
}

/// An arrow of a `\begin{CD}`. One of `>`, `<`, `V`, `A`, `=`, `|`.
#[derive(Debug)]
pub(super) struct CdArrow {
    pub kind: char,
    pub above: Vec<Node>,
    pub below: Vec<Node>,
    /// How long it has to be at least, in `em`s: across and down.
    pub least: (Em, Em),
}

#[derive(Debug)]
pub(super) enum Node {
    /// A symbol, a number, or a word in a font.
    Sym { text: String, class: Class, font: MathFont, bold: bool },
    /// `\text`: its spaces count.
    Text { text: String, font: MathFont },
    /// `noundefined`: the command's own name, in red.
    Undefined(String),
    /// A large or a named operator. `limits` is what `\limits` said; left to itself, it is `display_limits` in display style.
    Op { body: Box<Node>, big: bool, integral: bool, display_limits: bool, limits: Option<bool> },
    Scripts { base: Box<Node>, sup: Option<Box<Node>>, sub: Option<Box<Node>>, primes: u8 },
    Group(Vec<Node>),
    /// `\mathbin` and its kind.
    Classed(Class, Vec<Node>),
    Frac(Box<Frac>),
    /// `up` and `left` are `\uproot` and `\leftroot`, in `em`s.
    Sqrt { index: Option<Vec<Node>>, body: Vec<Node>, up: Em, left: Em },
    /// `\left`, its parts with a `\middle` between each, and `\right`. A NUL is the invisible `.`.
    Fenced { left: char, parts: Vec<Vec<Node>>, middles: Vec<char>, right: char },
    /// `\big` and its family, with the height they stand for.
    Big { delim: char, height: Em, class: Class },
    Accent { kind: AccentKind, wide: bool, body: Vec<Node> },
    Over { kind: OverKind, body: Vec<Node> },
    /// `\overset`, `\underset`, `\stackrel`: the base keeps its class.
    Stack { base: Vec<Node>, above: Option<Vec<Node>>, below: Option<Vec<Node>>, class: Option<Class> },
    /// `\xrightarrow[below]{above}`.
    Arrow { arrow: XArrow, above: Vec<Node>, below: Vec<Node> },
    Array(Box<Array>),
    Space(Em),
    /// `\displaystyle`, `\color`, `\Large`: from here to the end of the group.
    SetStyle(MathStyle),
    SetColor(Color),
    SetSize(f32),
    Colored { color: Color, body: Vec<Node> },
    /// `\colorbox`, `\fcolorbox`.
    ColorBox { fill: Option<Color>, stroke: Option<Color>, body: Vec<Node> },
    Phantom { width: bool, height: bool, body: Vec<Node> },
    Smash { body: Vec<Node> },
    /// `\mathllap` and its kind: -1 sticks out to the left, 0 both ways, 1 to the right.
    Lap { side: i8, body: Vec<Node> },
    Boxed { body: Vec<Node> },
    Enclose(Box<Enclose>),
    BBox(Box<BBox>),
    /// `\mathchoice`: display, text, script, scriptscript.
    Choice(Box<[Vec<Node>; 4]>),
    /// `\vcenter`, `\vtop`, `\vbox`.
    VBox { align: VAlign, body: Vec<Node> },
    /// `\moveleft`, `\moveright`: over by this much, and the width with it.
    Move { by: Em, body: Vec<Node> },
    /// `\makebox[w][pos]`: as wide as asked, -1 left, 0 centred, 1 right.
    Sized { width: Option<Em>, align: i8, body: Vec<Node> },
    /// `\prescript`.
    Prescript { base: Vec<Node>, sup: Vec<Node>, sub: Vec<Node> },
    /// `\sideset`: scripts at the corners, leaving the base's limits where they go.
    SideSet { pre_sup: Vec<Node>, pre_sub: Vec<Node>, post_sup: Vec<Node>, post_sub: Vec<Node>, base: Box<Node> },
    /// `\centerOver`: one thing over the middle of another, and no wider.
    CenterOver { base: Vec<Node>, over: Vec<Node> },
    /// `\cramped`.
    Cramped { style: Option<MathStyle>, body: Vec<Node> },
    Proof(Box<Proof>),
    CdArrow(Box<CdArrow>),
    /// `empheq`: what goes either side of a whole alignment. A `Stretch` in them takes its height.
    Empheq { left: Vec<Node>, right: Vec<Node>, body: Vec<Node> },
    /// `\empheqlbrace` and its kind.
    Stretch(char),
    /// `\nonscript`: the kern that follows is dropped in a script.
    NonScript,
    Rule { width: Em, height: Em, raise: Em },
    Raise { by: Em, body: Vec<Node> },
    /// What a `\\` outside any environment makes.
    Lines(Vec<Vec<Node>>),
    /// A slash through something with no negated form of its own.
    Not(Box<Node>),
    Tag(Vec<Node>),
}

/// What made a run of nodes stop.
#[derive(Debug, PartialEq)]
enum Stop {
    Eof,
    Close,
    Amp,
    Cr,
    Right,
    Middle,
    End(String),
    /// The closer a `physics` command is waiting for.
    AutoClose,
    /// `@>` in a `\begin{CD}`, with what follows the `@`.
    Cd(char),
}

#[derive(Clone, Copy, PartialEq)]
enum Infix {
    Over,
    Atop,
    Above(Em),
    Choose,
    Brace,
    Brack,
    /// `\overwithdelims` and its kind: a bar of this thickness, or `None` for the default one, between these.
    With(Option<Em>, char, char),
}

type Parsed<T> = Result<T, String>;

/// What a defined name does.
#[derive(Clone, Debug)]
pub(self) enum Macro {
    /// `\newcommand`, `\def` with plain parameters. `opt` is the first argument's default, which makes it optional.
    Text { args: u8, opt: Option<String>, body: String },
    /// `\def\a#1.#2;{...}`: what has to stand before each parameter, and after the last.
    Template { before: Vec<String>, body: String },
    /// `\let\a\b`: whatever `\b` is.
    Alias(String),
    /// `\DeclarePairedDelimiterXPP`, with the two shorter forms.
    Paired { open: String, close: String, body: String, args: u8, pre: String, post: String },
    /// `\Newextarrow`.
    Arrow { glyph: char, pad: (Em, Em) },
    /// `\MakeAboxedCommand`: the box it uses, and whether that takes text.
    Aboxed { boxed: String, math: bool },
}

/// `\newenvironment`.
#[derive(Clone, Debug)]
pub(self) struct EnvDef {
    args: u8,
    opt: Option<String>,
    begin: String,
    end: String,
}

/// Macros, and the groups `begingroup` opens around them. There is one TeX input in upstream's engine and it keeps what it was
/// taught, so a `\newcommand` in one formula still holds in the next.
#[derive(Default)]
pub(self) struct Definitions {
    scopes: Vec<Scope>,
    colors: HashMap<String, Color>,
    columns: HashMap<String, (u8, String)>,
    tag_forms: HashMap<String, (String, String, String)>,
    tag_form: Option<(String, String, String)>,
    /// What each `\label` was tagged, for `\ref`.
    labels: HashMap<String, String>,
}

#[derive(Default)]
pub(self) struct Scope {
    macros: HashMap<String, Option<Macro>>,
    envs: HashMap<String, EnvDef>,
    /// `\begingroupSandbox`: what is under this is out of reach, as far as `\global` goes.
    sandbox: bool,
}

thread_local! {
    static DEFINITIONS: RefCell<Definitions> = RefCell::new(Definitions::default());
}

struct Parser {
    text: String,
    at: usize,
    font: MathFont,
    bold: bool,
    depth: u32,
    expansions: u32,
    defs: Definitions,
    /// `\global` was the last thing said.
    global: bool,
    /// The extra height a `\\[1ex]` asked for.
    cr_extra: Em,
    tag: Option<Vec<Node>>,
    /// What the next list started ends at instead of a brace, and what opens it again: `physics`'s `\qty(`.
    auto_close: Option<(char, char)>,
    /// Reading a root's index: `\uproot` and `\leftroot` are allowed, and say this much.
    in_root: bool,
    root_move: (Em, Em),
    /// The array cell being read: what `\cellcolor` and its kind are about.
    cell: Option<CellState>,
    /// A `prooftree` is being read.
    proof: Option<ProofState>,
    /// Reading a `\begin{CD}`, whose `@` starts an arrow. The arrows' least sizes, in `em`s.
    diagram: Option<(Em, Em)>,
    /// `\minCDarrowwidth` and `\minCDarrowheight`, for the next `\begin{CD}`.
    cd_least: (Em, Em),
    /// `\hsize` in a `\vbox`.
    hsize: Option<Em>,
    /// Inside `\vb`: `Some(star)`. Lower-case Greek only goes bold with the star.
    vector: Option<bool>,
    /// An equation environment may stand here though something is already open: `empheq`, `spreadlines`.
    nesting: bool,
    /// The same for the first thing in a list this deep, unless it is a `gather` too.
    nest_depth: Option<u32>,
    /// This formula's `\label` and the text of its `\tag`.
    label: Option<String>,
    tag_text: Option<String>,
}

#[derive(Default)]
pub(self) struct CellState {
    fill: Option<Color>,
    row_fill: Option<Color>,
    /// `\columncolor` writes itself on the array it stands in, for this column.
    column_fill: Option<(usize, Color)>,
    shove: Option<char>,
    tag: Option<Vec<Node>>,
    column: usize,
    /// The array is a `multline` or `multlined`, which is where a `\shoveleft` makes sense.
    multline: bool,
    aligned: bool,
    /// No row has been finished yet.
    first_row: bool,
}

#[derive(Debug)]
pub(self) struct ProofState {
    stack: Vec<Proof>,
    line: ProofLine,
    current: ProofLine,
    root_at_top: bool,
    left: Option<Vec<Node>>,
    right: Option<Vec<Node>>,
}

/// A formula's tree, or MathJax's reason for not having one.
pub(super) fn parse(source: &str) -> Parsed<Vec<Node>> {
    let defs = DEFINITIONS.with(|d| std::mem::take(&mut *d.borrow_mut()));
    let mut parser = Parser { text: source.to_owned(), at: 0, font: MathFont::Auto, bold: false, depth: 0, expansions: 0, defs, global: false, cr_extra: 0.0, tag: None, auto_close: None, in_root: false, root_move: (0.0, 0.0), cell: None, proof: None, diagram: None, cd_least: (2.75, 1.75), hsize: None, vector: None, nesting: false, nest_depth: None, label: None, tag_text: None };
    let result = parser.body_to_end();
    let tag = parser.tag.take();
    if let (Some(label), Some(text)) = (parser.label.take(), parser.tag_text.take()) {
        parser.defs.labels.insert(label, text);
    }
    // What it defined holds whether or not the rest of it parsed: a definition takes effect as it is read.
    DEFINITIONS.with(|d| *d.borrow_mut() = std::mem::take(&mut parser.defs));
    let mut nodes = result?;
    if let Some(tag) = tag {
        nodes.push(Node::Tag(tag));
    }
    Ok(nodes)
}

fn is_letter(c: char) -> bool {
    c.is_ascii_alphabetic()
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.text[self.at..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.at += c.len_utf8();
        Some(c)
    }

    fn skip_space(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.bump();
        }
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.bump();
            return true;
        }
        false
    }

    /// The name after a backslash, and the space a named one swallows.
    fn control_name(&mut self) -> String {
        let start = self.at;
        while self.peek().is_some_and(is_letter) {
            self.bump();
        }
        if self.at == start {
            self.bump();
            return self.text[start..self.at].to_owned();
        }
        let name = self.text[start..self.at].to_owned();
        self.skip_space();
        name
    }

    /// `GetArgument`: a group's inside, or one command, or one character, as raw text.
    fn get_arg(&mut self, name: &str) -> Parsed<String> {
        self.skip_space();
        match self.peek() {
            None => Err(format!("Missing argument for \\{name}")),
            Some('}') => Err("Extra close brace or missing open brace".to_owned()),
            Some('{') => {
                self.bump();
                let start = self.at;
                let mut depth = 1;
                while let Some(c) = self.bump() {
                    match c {
                        '\\' => {
                            self.bump();
                        }
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                return Ok(self.text[start..self.at - 1].to_owned());
                            }
                        }
                        _ => {}
                    }
                }
                Err("Missing close brace".to_owned())
            }
            Some('\\') => {
                let start = self.at;
                self.bump();
                if self.peek().is_some_and(is_letter) {
                    while self.peek().is_some_and(is_letter) {
                        self.bump();
                    }
                } else {
                    self.bump();
                }
                Ok(self.text[start..self.at].to_owned())
            }
            Some(_) => {
                let start = self.at;
                self.bump();
                Ok(self.text[start..self.at].to_owned())
            }
        }
    }

    /// `GetBrackets`: `[...]`, if there is one.
    fn get_brackets(&mut self, name: &str) -> Parsed<Option<String>> {
        self.skip_space();
        if self.peek() != Some('[') {
            return Ok(None);
        }
        self.bump();
        let start = self.at;
        let mut depth = 0;
        while let Some(c) = self.bump() {
            match c {
                '\\' => {
                    self.bump();
                }
                '{' => depth += 1,
                '}' => {
                    if depth == 0 {
                        return Err("Extra close brace while looking for ']'".to_owned());
                    }
                    depth -= 1;
                }
                ']' if depth == 0 => return Ok(Some(self.text[start..self.at - 1].to_owned())),
                _ => {}
            }
        }
        Err(format!("Could not find closing ']' for argument to \\{name}"))
    }

    fn star(&mut self) -> bool {
        self.skip_space();
        self.eat('*')
    }

    /// Parses text apart from the input, keeping the state around it.
    fn parse_string(&mut self, source: &str) -> Parsed<Vec<Node>> {
        let outer = std::mem::replace(&mut self.text, source.to_owned());
        let at = std::mem::replace(&mut self.at, 0);
        let result = self.body_to_end();
        self.text = outer;
        self.at = at;
        result
    }

    /// An argument as nodes, as `ParseArg`.
    fn parse_arg(&mut self, name: &str) -> Parsed<Vec<Node>> {
        let raw = self.get_arg(name)?;
        self.parse_string(&raw)
    }

    fn parse_arg_with(&mut self, name: &str, font: MathFont, bold: bool) -> Parsed<Vec<Node>> {
        let raw = self.get_arg(name)?;
        let saved = (self.font, self.bold);
        (self.font, self.bold) = (font, bold);
        let result = self.parse_string(&raw);
        (self.font, self.bold) = saved;
        result
    }

    /// Everything to the end of the input: what a formula or an argument holds.
    fn body_to_end(&mut self) -> Parsed<Vec<Node>> {
        let mut lines: Vec<Vec<Node>> = Vec::new();
        loop {
            let (nodes, stop) = self.list()?;
            match stop {
                Stop::Cr => lines.push(nodes),
                Stop::Eof => {
                    if lines.is_empty() {
                        return Ok(nodes);
                    }
                    // A last line with nothing on it, which is how models tend to end one, is no line.
                    if !nodes.is_empty() {
                        lines.push(nodes);
                    }
                    return Ok(vec![Node::Lines(lines)]);
                }
                Stop::Close => return Err("Extra close brace or missing open brace".to_owned()),
                Stop::Amp => return Err("Misplaced &".to_owned()),
                Stop::Right => return Err("Missing \\left or extra \\right".to_owned()),
                Stop::Middle => return Err("Extra \\middle".to_owned()),
                Stop::End(name) => return Err(format!("Missing \\begin{{{name}}} or extra \\end{{{name}}}")),
                Stop::AutoClose | Stop::Cd(_) => return Err("Misplaced &".to_owned()),
            }
        }
    }

    /// A `{...}` in the input: the opening brace is already past.
    fn group(&mut self) -> Parsed<Node> {
        let saved = (self.font, self.bold);
        let mut lines: Vec<Vec<Node>> = Vec::new();
        let result = loop {
            let (nodes, stop) = self.list()?;
            match stop {
                Stop::Cr => lines.push(nodes),
                Stop::Close => {
                    if lines.is_empty() {
                        break Node::Group(nodes);
                    }
                    if !nodes.is_empty() {
                        lines.push(nodes);
                    }
                    break Node::Lines(lines);
                }
                Stop::Eof | Stop::End(_) => return Err("Extra open brace or missing close brace".to_owned()),
                Stop::Amp | Stop::AutoClose | Stop::Cd(_) => return Err("Misplaced &".to_owned()),
                Stop::Right => return Err("Missing \\left or extra \\right".to_owned()),
                Stop::Middle => return Err("Extra \\middle".to_owned()),
            }
        };
        (self.font, self.bold) = saved;
        Ok(result)
    }

    fn list(&mut self) -> Parsed<(Vec<Node>, Stop)> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            return Err(TOO_DEEP.to_owned());
        }
        let result = self.list_inner();
        self.depth -= 1;
        result
    }

    fn list_inner(&mut self) -> Parsed<(Vec<Node>, Stop)> {
        let mut nodes = Vec::new();
        let mut infix: Option<(Infix, Vec<Node>)> = None;
        // Only the list a `physics` command started is the one its closer can end: inside a brace or a `\left` it is a character.
        let auto = self.auto_close.take();
        let mut opened = 0u32;
        let stop = loop {
            self.skip_space();
            let Some(c) = self.peek() else { break Stop::Eof };
            if let Some((open, close)) = auto {
                if c == close {
                    if opened == 0 {
                        self.bump();
                        break Stop::AutoClose;
                    }
                    opened -= 1;
                } else if c == open {
                    opened += 1;
                }
            }
            if c == '@' && self.diagram.is_some() && self.text[self.at + 1..].starts_with(['>', '<', 'V', 'A', '.', '|', '=']) {
                self.bump();
                let kind = self.bump().expect("just looked at it");
                break Stop::Cd(kind);
            }
            match c {
                '}' => {
                    self.bump();
                    break Stop::Close;
                }
                '&' => {
                    self.bump();
                    break Stop::Amp;
                }
                '^' | '_' => {
                    self.bump();
                    self.script(&mut nodes, c == '^')?;
                }
                '\'' => self.primes(&mut nodes)?,
                '%' => {
                    while self.peek().is_some_and(|c| c != '\n') {
                        self.bump();
                    }
                }
                '#' => return Err("You can't use 'macro parameter character #' in math mode".to_owned()),
                '{' => {
                    self.bump();
                    let group = self.group()?;
                    nodes.push(group);
                }
                '\\' => {
                    self.bump();
                    let name = self.control_name();
                    match name.as_str() {
                        "\\" | "cr" | "newline" => {
                            self.after_cr()?;
                            break Stop::Cr;
                        }
                        "right" => break Stop::Right,
                        "middle" => break Stop::Middle,
                        "end" => {
                            let env = self.get_arg("end")?;
                            break Stop::End(env);
                        }
                        "over" | "atop" | "above" | "choose" | "brace" | "brack" | "overwithdelims" | "atopwithdelims" | "abovewithdelims" => {
                            if infix.is_some() {
                                return Err(format!("Ambiguous use of \\{name}"));
                            }
                            let kind = self.infix_kind(&name)?;
                            infix = Some((kind, std::mem::take(&mut nodes)));
                        }
                        "limits" | "nolimits" | "displaylimits" => set_limits(&mut nodes, &name)?,
                        "sp" => self.script(&mut nodes, true)?,
                        "sb" => self.script(&mut nodes, false)?,
                        _ => self.command(&name, &mut nodes)?,
                    }
                }
                _ => {
                    self.bump();
                    let node = self.char_atom(c);
                    nodes.push(node);
                }
            }
        };
        if let Some((kind, num)) = infix {
            let (bar, left, right) = match kind {
                Infix::Over => (None, '\0', '\0'),
                Infix::Atop => (Some(0.0), '\0', '\0'),
                Infix::Above(thick) => (Some(thick), '\0', '\0'),
                Infix::Choose => (Some(0.0), '(', ')'),
                Infix::Brace => (Some(0.0), '{', '}'),
                Infix::Brack => (Some(0.0), '[', ']'),
                Infix::With(bar, left, right) => (bar, left, right),
            };
            let den = std::mem::take(&mut nodes);
            nodes.push(Node::Frac(Box::new(Frac { num, den, bar, style: None, left, right, continued: false, align: (0, 0), bevelled: false })));
        }
        Ok((nodes, stop))
    }

    fn infix_kind(&mut self, name: &str) -> Parsed<Infix> {
        Ok(match name {
            "over" => Infix::Over,
            "atop" => Infix::Atop,
            "choose" => Infix::Choose,
            "brace" => Infix::Brace,
            "brack" => Infix::Brack,
            "overwithdelims" | "atopwithdelims" | "abovewithdelims" => {
                let left = self.delimiter(name)?;
                let right = self.delimiter(name)?;
                let bar = match name {
                    "atopwithdelims" => Some(0.0),
                    "abovewithdelims" => Some(self.dimension_here(name)?),
                    _ => None,
                };
                Infix::With(bar, left, right)
            }
            _ => Infix::Above(self.dimension_here(name)?),
        })
    }

    /// `\\*[1ex]`.
    fn after_cr(&mut self) -> Parsed<()> {
        self.cr_extra = 0.0;
        self.eat('*');
        // A bracket on the next line is the next row's text; a bracket right here is a length, if it reads as one.
        if self.peek() == Some('[') {
            let at = self.at;
            let raw = self.get_brackets("\\")?.unwrap_or_default();
            match dimension(&raw) {
                Some(extra) => self.cr_extra = extra,
                None => self.at = at,
            }
        }
        Ok(())
    }

    /// `^` or `_`, with the node before it for a base.
    fn script(&mut self, nodes: &mut Vec<Node>, up: bool) -> Parsed<()> {
        let arg = self.script_arg()?;
        let mut base = match nodes.pop() {
            // Spacing and switches are nothing to hang a script on.
            Some(last @ (Node::Space(_) | Node::SetStyle(_) | Node::SetColor(_) | Node::SetSize(_) | Node::NonScript)) => {
                nodes.push(last);
                Node::Group(Vec::new())
            }
            Some(last) => last,
            None => Node::Group(Vec::new()),
        };
        if let Node::Scripts { sup, sub, .. } = &mut base {
            if up {
                if sup.is_some() {
                    return Err("Double exponent: use braces to clarify".to_owned());
                }
                // After `x'` this is `x'^2`, where the prime and the exponent share one script.
                *sup = Some(Box::new(arg));
            } else {
                if sub.is_some() {
                    return Err("Double subscripts: use braces to clarify".to_owned());
                }
                *sub = Some(Box::new(arg));
            }
            nodes.push(base);
            return Ok(());
        }
        let (sup, sub) = if up { (Some(Box::new(arg)), None) } else { (None, Some(Box::new(arg))) };
        nodes.push(Node::Scripts { base: Box::new(base), sup, sub, primes: 0 });
        Ok(())
    }

    /// What `^` takes: a group, or the next atom whole with its own arguments, as in `x^\frac12`.
    fn script_arg(&mut self) -> Parsed<Node> {
        self.skip_space();
        let Some(c) = self.peek() else { return Err("Missing superscript or subscript argument".to_owned()) };
        match c {
            '{' => {
                self.bump();
                self.group()
            }
            '^' => Err("Missing open brace for superscript".to_owned()),
            '_' => Err("Missing open brace for subscript".to_owned()),
            '}' | '&' | '#' => Err("Missing superscript or subscript argument".to_owned()),
            '\\' => {
                self.bump();
                let name = self.control_name();
                if matches!(name.as_str(), "right" | "middle" | "end" | "\\") {
                    return Err("Missing superscript or subscript argument".to_owned());
                }
                let expansions = self.expansions;
                let mut one = Vec::new();
                self.command(&name, &mut one)?;
                // A macro left its text in the input, and the argument is whatever that starts with.
                if one.is_empty() && self.expansions > expansions {
                    return Ok(Node::Group(self.first_atom()?));
                }
                Ok(match one.len() {
                    1 => one.pop().expect("one"),
                    _ => Node::Group(one),
                })
            }
            _ => {
                self.bump();
                Ok(self.char_atom(c))
            }
        }
    }

    /// One atom from the input, after a macro put it there. Macros it finds are expanded in a loop, since one may lead to another
    /// for as long as the count allows.
    fn first_atom(&mut self) -> Parsed<Vec<Node>> {
        let mut one = Vec::new();
        while one.is_empty() {
            self.skip_space();
            match self.peek() {
                None | Some('}' | '&' | '^' | '_' | '#') => return Ok(one),
                Some('{') => {
                    self.bump();
                    one.push(self.group()?);
                }
                Some('\\') => {
                    self.bump();
                    let name = self.control_name();
                    if matches!(name.as_str(), "right" | "middle" | "end" | "\\") {
                        return Err("Missing superscript or subscript argument".to_owned());
                    }
                    let expansions = self.expansions;
                    self.command(&name, &mut one)?;
                    // A command that draws nothing and expands to nothing is an atom of nothing.
                    if one.is_empty() && self.expansions == expansions {
                        return Ok(one);
                    }
                }
                Some(c) => {
                    self.bump();
                    one.push(self.char_atom(c));
                }
            }
        }
        Ok(one)
    }

    fn primes(&mut self, nodes: &mut Vec<Node>) -> Parsed<()> {
        let mut count = 0u8;
        while self.peek() == Some('\'') {
            self.bump();
            count = count.saturating_add(1);
        }
        let base = nodes.pop().unwrap_or(Node::Group(Vec::new()));
        match base {
            Node::Scripts { base, sup, sub, primes } => {
                if sup.is_some() || primes > 0 {
                    return Err("Prime causes double exponent: use braces to clarify".to_owned());
                }
                nodes.push(Node::Scripts { base, sup, sub, primes: count });
            }
            base => nodes.push(Node::Scripts { base: Box::new(base), sup: None, sub: None, primes: count }),
        }
        Ok(())
    }

    fn sym(&self, text: impl Into<String>, class: Class, font: MathFont) -> Node {
        Node::Sym { text: text.into(), class, font, bold: self.bold }
    }

    /// One character in math mode. A number stays together, as MathJax's `digits` pattern has it, and so does a word in a font.
    fn char_atom(&mut self, c: char) -> Node {
        if c.is_ascii_digit() || (c == '.' && self.peek().is_some_and(|n| n.is_ascii_digit())) {
            let start = self.at - c.len_utf8();
            self.take_number(c);
            let text = self.text[start..self.at].replace("{,}", ",");
            return self.sym(text, Class::Ord, self.font);
        }
        if is_letter(c) {
            if self.font != MathFont::Auto {
                let start = self.at - 1;
                while self.peek().is_some_and(is_letter) {
                    self.bump();
                }
                let word = self.text[start..self.at].to_owned();
                return self.sym(word, Class::Ord, self.font);
            }
            return self.sym(c, Class::Ord, MathFont::Auto);
        }
        match c {
            '-' => self.sym('\u{2212}', Class::Bin, MathFont::Roman),
            '*' => self.sym('\u{2217}', Class::Bin, MathFont::Roman),
            '+' => self.sym('+', Class::Bin, MathFont::Roman),
            '=' | '<' | '>' => self.sym(c, Class::Rel, MathFont::Roman),
            ':' => self.sym(':', Class::Rel, MathFont::Roman),
            ',' | ';' => self.sym(c, Class::Punct, MathFont::Roman),
            '(' | '[' => self.sym(c, Class::Open, MathFont::Roman),
            ')' | ']' | '!' | '?' => self.sym(c, Class::Close, MathFont::Roman),
            '~' => Node::Space(1.0 / 3.0),
            // A letter from any other alphabet or a symbol typed as itself: MathJax's table for it says what it is, and the
            // relations and operators people type are in ours.
            _ => match unicode_class(c) {
                Some(class) => self.sym(c, class, MathFont::Roman),
                None => self.sym(c, Class::Ord, if c.is_alphabetic() { self.font } else { MathFont::Roman }),
            },
        }
    }

    fn take_number(&mut self, first: char) {
        let mut seen_dot = first == '.';
        loop {
            match self.peek() {
                Some(d) if d.is_ascii_digit() => {
                    self.bump();
                }
                Some('.') if !seen_dot => {
                    seen_dot = true;
                    self.bump();
                }
                Some('{') if !seen_dot && self.text[self.at..].starts_with("{,}") && self.text[self.at + 3..].chars().take(3).filter(char::is_ascii_digit).count() == 3 => {
                    self.at += 3;
                }
                _ => return,
            }
        }
    }

    fn expand(&mut self, name: &str, args: u8, body: &str) -> Parsed<()> {
        self.expand_with(name, args, None, body)
    }

    /// A macro's text in the input, with its arguments read. `opt` is what its first one is when no bracket says otherwise.
    fn expand_with(&mut self, name: &str, args: u8, opt: Option<&str>, body: &str) -> Parsed<()> {
        let mut values = Vec::with_capacity(args as usize);
        if args > 0
            && let Some(default) = opt
        {
            values.push(self.get_brackets(name)?.unwrap_or_else(|| default.to_owned()));
        }
        while values.len() < args as usize {
            values.push(self.get_arg(name)?);
        }
        self.splice(&substitute(body, &values))
    }

    /// `checkMaxMacros` and `addArgs`: puts text in the input where the reading is, counting.
    fn splice(&mut self, replaced: &str) -> Parsed<()> {
        self.expansions += 1;
        if self.expansions > MAX_EXPANSIONS {
            return Err("MathJax maximum macro substitution count exceeded; is there a recursive macro call?".to_owned());
        }
        let mut replaced = replaced.to_owned();
        let rest_at = self.at;
        if replaced.len() + self.text.len() - rest_at > MAX_BUFFER {
            return Err("MathJax internal buffer size exceeded; is there a recursive macro call?".to_owned());
        }
        // A space keeps `\foo` from gluing onto the letters after it, as MathJax's `addArgs` does.
        if replaced.ends_with(|c: char| is_letter(c)) && self.peek().is_some_and(is_letter) {
            replaced.push(' ');
        }
        replaced.push_str(&self.text[rest_at..]);
        self.text = replaced;
        self.at = 0;
        Ok(())
    }

    fn command(&mut self, name: &str, nodes: &mut Vec<Node>) -> Parsed<()> {
        match self.lookup_macro(name) {
            // `\let\foo\undefined` took its meaning away, and it is unknown again.
            Some(None) => {
                nodes.push(Node::Undefined(format!("\\{name}")));
                return Ok(());
            }
            Some(Some(definition)) => return self.run_macro(name, definition, nodes),
            None => {}
        }
        if name == "dots" {
            let text = if self.dots_before_operator() { "⋯" } else { "…" };
            nodes.push(self.sym(text, Class::Inner, MathFont::Roman));
            return Ok(());
        }
        // Packages listed later in `TEX_PACKAGES` are asked first: `physics` has its own `\sin` and its own `\div`.
        if self.package_command(name, nodes)? {
            return Ok(());
        }
        if let Some(node) = self.symbol_node(name) {
            nodes.push(node);
            return Ok(());
        }
        if let Some((args, body)) = rewrite(name) {
            // `\abs*` and its like differ only in how they size.
            if args > 0 {
                self.star();
            }
            return self.expand(name, args, body);
        }
        if let Some(node) = self.operator(name)? {
            nodes.push(node);
            return Ok(());
        }
        if self.switch(name, nodes)? {
            return Ok(());
        }
        let node = match self.structure(name)? {
            Some(node) => node,
            None => match self.boxes(name)? {
                Some(node) => node,
                None => match self.definitions(name)? {
                    Some(true) => return Ok(()),
                    _ => match name {
                        "begin" => {
                            self.check_equation_structure(nodes)?;
                            self.environment()?
                        }
                        "left" => self.fenced()?,
                        "not" => self.negated()?,
                        _ => Node::Undefined(format!("\\{name}")),
                    },
                },
            },
        };
        nodes.push(node);
        Ok(())
    }

    /// `\dots`: low in a list, on the axis when an operator comes next, as MathJax's `DotsItem` has it.
    fn dots_before_operator(&mut self) -> bool {
        self.skip_space();
        let rest = &self.text[self.at..];
        match rest.chars().next() {
            Some('+' | '-' | '*' | '=' | '<' | '>') => true,
            Some('\\') => {
                let name: String = rest[1..].chars().take_while(|c| is_letter(*c)).collect();
                binary(&name).is_some() || relation(&name).is_some() || arrow(&name).is_some()
            }
            _ => false,
        }
    }

    fn symbol_node(&self, name: &str) -> Option<Node> {
        if let Some(text) = letter(name) {
            // `createVectorToken`: small Greek stays as it was unless `\vb*` said otherwise.
            let font = if self.vector == Some(false) && text.chars().all(|c| ('α'..='ω').contains(&c)) { MathFont::Auto } else { self.font };
            return Some(self.sym(text, Class::Ord, font));
        }
        if let Some(text) = italic_capital(name) {
            return Some(self.sym(text, Class::Ord, MathFont::Italic));
        }
        if let Some(text) = ordinary(name) {
            return Some(self.sym(text, Class::Ord, MathFont::Roman));
        }
        if let Some(text) = binary(name) {
            return Some(self.sym(text, Class::Bin, MathFont::Roman));
        }
        if let Some(text) = relation(name).or_else(|| arrow(name)) {
            return Some(self.sym(text, Class::Rel, MathFont::Roman));
        }
        let (text, class) = bracket(name)?;
        Some(self.sym(text, class, MathFont::Roman))
    }

    fn operator(&mut self, name: &str) -> Parsed<Option<Node>> {
        if let Some((text, integral)) = big_operator(name) {
            return Ok(Some(Node::Op { body: Box::new(self.sym(text, Class::Op, MathFont::Roman)), big: true, integral, display_limits: !integral, limits: None }));
        }
        if let Some((text, limits)) = named_operator(name) {
            let body = Box::new(Node::Sym { text: text.to_owned(), class: Class::Ord, font: MathFont::Roman, bold: false });
            return Ok(Some(Node::Op { body, big: false, integral: false, display_limits: limits, limits: None }));
        }
        Ok(match name {
            "operatorname" => {
                let limits = self.star();
                // A hyphen in an operator's name is a hyphen.
                let raw = self.get_arg(name)?.replace('-', "\\text{-}");
                let saved = (self.font, self.bold);
                self.font = MathFont::Roman;
                let body = self.parse_string(&raw);
                (self.font, self.bold) = saved;
                Some(Node::Op { body: Box::new(Node::Group(body?)), big: false, integral: false, display_limits: limits, limits: None })
            }
            "mathop" => {
                let body = self.parse_arg(name)?;
                Some(Node::Op { body: Box::new(Node::Group(body)), big: false, integral: false, display_limits: true, limits: None })
            }
            "bmod" => Some(Node::Classed(Class::Bin, vec![self.sym("mod", Class::Ord, MathFont::Roman)])),
            _ => None,
        })
    }

    /// What applies from here to the end of the group. Answers whether the name was one.
    fn switch(&mut self, name: &str, nodes: &mut Vec<Node>) -> Parsed<bool> {
        let style = match name {
            "displaystyle" => Some(MathStyle::Display),
            "textstyle" => Some(MathStyle::Text),
            "scriptstyle" => Some(MathStyle::Script),
            "scriptscriptstyle" => Some(MathStyle::ScriptScript),
            _ => None,
        };
        if let Some(style) = style {
            nodes.push(Node::SetStyle(style));
            return Ok(true);
        }
        let font = match name {
            "rm" => Some(MathFont::Roman),
            "bf" => Some(MathFont::Bold),
            "it" => Some(MathFont::Italic),
            "sf" => Some(MathFont::Sans),
            "tt" => Some(MathFont::Mono),
            "cal" => Some(MathFont::Script),
            "frak" => Some(MathFont::Fraktur),
            "Bbb" => Some(MathFont::DoubleStruck),
            "mit" | "oldstyle" => Some(MathFont::Auto),
            _ => None,
        };
        if let Some(font) = font {
            self.font = font;
            return Ok(true);
        }
        let size = match name {
            "Tiny" => Some(0.5),
            "tiny" => Some(0.6),
            "scriptsize" | "SMALL" => Some(0.7),
            "footnotesize" | "Small" => Some(0.8),
            "small" => Some(0.9),
            "normalsize" => Some(1.0),
            "large" => Some(1.095),
            "Large" => Some(1.2),
            "LARGE" => Some(1.44),
            "huge" => Some(1.728),
            "Huge" => Some(2.074),
            "HUGE" => Some(2.49),
            _ => None,
        };
        if let Some(size) = size {
            nodes.push(Node::SetSize(size));
            return Ok(true);
        }
        if name == "color" {
            let model = self.get_brackets(name)?;
            let spec = self.get_arg(name)?;
            if let Some(color) = self.color(model.as_deref(), &spec) {
                nodes.push(Node::SetColor(color));
            }
            return Ok(true);
        }
        Ok(match name {
            // A line break a display has nowhere to take, and what only says whether it may: nothing to draw here.
            "nonumber" | "notag" | "allowbreak" | "nobreak" | "goodbreak" | "badbreak" | "break" | "hfill" | "hfil" | "hfilll" | "-" => true,
            "label" => {
                self.label = Some(self.get_arg(name)?.trim().to_owned());
                true
            }
            "nonscript" => {
                nodes.push(Node::NonScript);
                true
            }
            _ => false,
        })
    }

    fn font_arg(&mut self, name: &str, font: MathFont) -> Parsed<Node> {
        let bold = self.bold;
        Ok(Node::Group(self.parse_arg_with(name, font, bold)?))
    }

    fn structure(&mut self, name: &str) -> Parsed<Option<Node>> {
        if let Some(font) = font_command(name) {
            return self.font_arg(name, font).map(Some);
        }
        if let Some((em, class)) = big_delimiter(name) {
            return self.big(name, em, class).map(Some);
        }
        if let Some(space) = fixed_space(name) {
            return Ok(Some(Node::Space(space)));
        }
        if let Some((kind, wide)) = accent(name) {
            return Ok(Some(Node::Accent { kind, wide, body: self.parse_arg(name)? }));
        }
        if let Some(kind) = over_kind(name) {
            return self.over(name, kind).map(Some);
        }
        if let Some(arrow) = x_arrow(name) {
            return self.x_arrow_node(name, arrow).map(Some);
        }
        if let Some(node) = self.fractions(name)? {
            return Ok(Some(node));
        }
        Ok(match name {
            "boldsymbol" => {
                let raw = self.get_arg(name)?;
                let saved = (self.font, self.bold);
                (self.font, self.bold) = (if self.font == MathFont::Auto { MathFont::BoldItalic } else { self.font }, true);
                let body = self.parse_string(&raw);
                (self.font, self.bold) = saved;
                Some(Node::Group(body?))
            }
            "sqrt" => {
                let index = self.get_brackets(name)?;
                let body = self.parse_arg(name)?;
                let (index, up, left) = match index {
                    Some(raw) if !raw.trim().is_empty() => {
                        let (index, up, left) = self.root_index(&raw)?;
                        (Some(index), up, left)
                    }
                    _ => (None, 0.0, 0.0),
                };
                Some(Node::Sqrt { index, body, up, left })
            }
            "root" => {
                let at = self.text[self.at..].find("\\of").ok_or_else(|| "Missing \\of in \\root".to_owned())?;
                let raw = self.text[self.at..self.at + at].to_owned();
                self.at += at + 3;
                let (index, up, left) = self.root_index(&raw)?;
                let body = self.parse_arg(name)?;
                Some(Node::Sqrt { index: Some(index), body, up, left })
            }
            "uproot" | "leftroot" => {
                if !self.in_root {
                    return Err(format!("\\{name} can appear only within a root"));
                }
                let raw = self.get_arg(name)?;
                let n: f32 = raw.trim().parse().map_err(|_| format!("The argument to \\{name} must be an integer"))?;
                if name == "uproot" {
                    self.root_move.0 = n / 15.0;
                } else {
                    self.root_move.1 = n / 15.0;
                }
                Some(Node::Group(Vec::new()))
            }
            "overset" | "underset" | "stackrel" | "stackbin" => {
                let script = self.parse_arg(name)?;
                let base = self.parse_arg(name)?;
                match name {
                    "underset" => Some(Node::Stack { base, above: None, below: Some(script), class: None }),
                    "stackrel" => Some(Node::Stack { base, above: Some(script), below: None, class: Some(Class::Rel) }),
                    "stackbin" => Some(Node::Stack { base, above: Some(script), below: None, class: Some(Class::Bin) }),
                    _ => Some(Node::Stack { base, above: Some(script), below: None, class: None }),
                }
            }
            "overunderset" => {
                let over = self.parse_arg(name)?;
                let under = self.parse_arg(name)?;
                let base = self.parse_arg(name)?;
                Some(Node::Stack { base, above: Some(over), below: Some(under), class: None })
            }
            "buildrel" => {
                let at = self.text[self.at..].find("\\over").ok_or_else(|| "Missing \\over in \\buildrel".to_owned())?;
                let raw = self.text[self.at..self.at + at].to_owned();
                self.at += at + 5;
                let top = self.parse_string(&raw)?;
                let base = self.parse_arg(name)?;
                Some(Node::Stack { base, above: Some(top), below: None, class: Some(Class::Rel) })
            }
            "mathord" | "mathbin" | "mathrel" | "mathopen" | "mathclose" | "mathpunct" | "mathinner" => {
                let class = match name {
                    "mathord" => Class::Ord,
                    "mathbin" => Class::Bin,
                    "mathrel" => Class::Rel,
                    "mathopen" => Class::Open,
                    "mathclose" => Class::Close,
                    "mathpunct" => Class::Punct,
                    _ => Class::Inner,
                };
                Some(Node::Classed(class, self.parse_arg(name)?))
            }
            "substack" => {
                let raw = self.get_arg(name)?;
                let outer = std::mem::replace(&mut self.text, raw);
                let at = std::mem::replace(&mut self.at, 0);
                let array = self.array_rows(Array { style: MathStyle::Script, col_gap: 0.0, baseline: 1.0, jot: 0.0, ..plain_array() }, None, &CellWrap::default());
                self.text = outer;
                self.at = at;
                Some(Node::Array(Box::new(array?)))
            }
            "tag" => {
                let star = self.star();
                let raw = self.get_arg(name)?;
                self.tag_text = Some(raw.clone());
                let body = self.parse_text(&raw, MathFont::Roman)?;
                let tag = if star { body } else { self.tag_shown(body)? };
                // In an array it is that row's. A bare one is the formula's.
                match &mut self.cell {
                    Some(cell) => {
                        if cell.tag.is_some() {
                            return Err("Multiple \\tag".to_owned());
                        }
                        cell.tag = Some(tag);
                    }
                    None => {
                        if self.tag.is_some() {
                            return Err("Multiple \\tag".to_owned());
                        }
                        self.tag = Some(tag);
                    }
                }
                Some(Node::Group(Vec::new()))
            }
            _ => None,
        })
    }

    /// A root's index, with what `\uproot` and `\leftroot` said in it.
    fn root_index(&mut self, raw: &str) -> Parsed<(Vec<Node>, Em, Em)> {
        let saved = (self.in_root, self.root_move);
        self.in_root = true;
        self.root_move = (0.0, 0.0);
        let index = self.parse_string(raw);
        let (up, left) = self.root_move;
        (self.in_root, self.root_move) = saved;
        Ok((index?, up, left))
    }

    fn big(&mut self, name: &str, height: Em, class: Class) -> Parsed<Node> {
        let delim = self.delimiter(name)?;
        Ok(Node::Big { delim, height, class })
    }

    fn over(&mut self, name: &str, kind: OverKind) -> Parsed<Node> {
        let body = self.parse_arg(name)?;
        let node = Node::Over { kind, body };
        // A brace's script goes over it or under it, as it does with a `\sum`.
        Ok(match kind {
            OverKind::Overbrace | OverKind::Underbrace => Node::Op { body: Box::new(node), big: false, integral: false, display_limits: true, limits: Some(true) },
            _ => node,
        })
    }

    fn fractions(&mut self, name: &str) -> Parsed<Option<Node>> {
        let (style, bar, left, right, continued) = match name {
            "frac" => (None, None, '\0', '\0', false),
            "dfrac" => (Some(MathStyle::Display), None, '\0', '\0', false),
            "tfrac" => (Some(MathStyle::Text), None, '\0', '\0', false),
            "cfrac" => {
                let side = self.get_brackets(name)?;
                let num = self.parse_arg(name)?;
                let den = self.parse_arg(name)?;
                // `\cfrac[l]`: the numerator against that side.
                let at = match side.as_deref().map(str::trim) {
                    Some("l") => -1,
                    Some("r") => 1,
                    _ => 0,
                };
                return Ok(Some(Node::Frac(Box::new(Frac { num, den, bar: None, style: Some(MathStyle::Display), left: '\0', right: '\0', continued: true, align: (at, 0), bevelled: false }))));
            }
            "binom" => (None, Some(0.0), '(', ')', false),
            "dbinom" => (Some(MathStyle::Display), Some(0.0), '(', ')', false),
            "tbinom" => (Some(MathStyle::Text), Some(0.0), '(', ')', false),
            "genfrac" => return self.genfrac().map(Some),
            _ => return Ok(None),
        };
        let num = self.parse_arg(name)?;
        let den = self.parse_arg(name)?;
        Ok(Some(Node::Frac(Box::new(Frac { num, den, bar, style, left, right, continued, align: (0, 0), bevelled: false }))))
    }

    fn genfrac(&mut self) -> Parsed<Node> {
        let name = "genfrac";
        let left = self.get_arg(name)?;
        let right = self.get_arg(name)?;
        let thick = self.get_arg(name)?;
        let style = self.get_arg(name)?;
        let num = self.parse_arg(name)?;
        let den = self.parse_arg(name)?;
        let style = match style.trim() {
            "0" => Some(MathStyle::Display),
            "1" => Some(MathStyle::Text),
            "2" => Some(MathStyle::Script),
            "3" => Some(MathStyle::ScriptScript),
            _ => None,
        };
        let delim = |raw: &str| lookup_delimiter(raw.trim()).unwrap_or('\0');
        Ok(Node::Frac(Box::new(Frac { num, den, bar: dimension(&thick), style, left: delim(&left), right: delim(&right), continued: false, align: (0, 0), bevelled: false })))
    }

    fn dimension_here(&mut self, name: &str) -> Parsed<Em> {
        self.skip_space();
        let start = self.at;
        while self.peek().is_some_and(|c| c.is_ascii_digit() || c == '.' || c == '-' || c == '+') {
            self.bump();
        }
        self.skip_space();
        // A unit is two letters, and what comes straight after it is the next thing in the formula.
        for _ in 0..2 {
            if self.peek().is_some_and(is_letter) {
                self.bump();
            }
        }
        dimension(&self.text[start..self.at]).ok_or_else(|| format!("Missing dimension or its units for \\{name}"))
    }

    /// Takes the optional brackets and the argument of a space or a box with a length.
    fn dimension_arg(&mut self, name: &str) -> Parsed<Em> {
        let raw = self.get_arg(name)?;
        dimension(&raw).ok_or_else(|| format!("Missing dimension or its units for \\{name}"))
    }

    fn boxes(&mut self, name: &str) -> Parsed<Option<Node>> {
        Ok(match name {
            "text" | "textrm" | "textnormal" | "textup" | "mbox" | "hbox" => Some(self.text_arg(name, MathFont::Roman)?),
            "textbf" => Some(self.text_arg(name, MathFont::Bold)?),
            "textit" => Some(self.text_arg(name, MathFont::Italic)?),
            "textsf" => Some(self.text_arg(name, MathFont::Sans)?),
            "texttt" => Some(self.text_arg(name, MathFont::Mono)?),
            "verb" => {
                let close = self.bump().ok_or_else(|| "Missing argument for \\verb".to_owned())?;
                let start = self.at;
                while self.peek().is_some_and(|c| c != close) {
                    self.bump();
                }
                if self.peek().is_none() {
                    return Err("Can't find closing delimiter for \\verb".to_owned());
                }
                let text = self.text[start..self.at].to_owned();
                self.bump();
                Some(Node::Text { text, font: MathFont::Mono })
            }
            "unicode" => {
                self.get_brackets(name)?;
                let raw = self.get_arg(name)?;
                let raw = raw.trim();
                let number = raw.strip_prefix('x').or_else(|| raw.strip_prefix("0x")).map(|hex| u32::from_str_radix(hex, 16)).unwrap_or_else(|| raw.parse());
                let Some(c) = number.ok().and_then(char::from_u32) else { return Err("Invalid unicode character for \\unicode".to_owned()) };
                Some(self.sym(c, Class::Ord, MathFont::Roman))
            }
            "textcolor" => {
                let model = self.get_brackets(name)?;
                let spec = self.get_arg(name)?;
                let body = self.parse_arg(name)?;
                match self.color(model.as_deref(), &spec) {
                    Some(color) => Some(Node::Colored { color, body }),
                    None => Some(Node::Group(body)),
                }
            }
            "colorbox" => {
                let model = self.get_brackets(name)?;
                let spec = self.get_arg(name)?;
                let raw = self.get_arg(name)?;
                let body = self.parse_text(&raw, MathFont::Roman)?;
                Some(Node::ColorBox { fill: self.color(model.as_deref(), &spec), stroke: None, body })
            }
            "fcolorbox" => {
                let model = self.get_brackets(name)?;
                let stroke = self.get_arg(name)?;
                let fill = self.get_arg(name)?;
                let raw = self.get_arg(name)?;
                let body = self.parse_text(&raw, MathFont::Roman)?;
                Some(Node::ColorBox { fill: self.color(model.as_deref(), &fill), stroke: self.color(model.as_deref(), &stroke), body })
            }
            "definecolor" => {
                let target = self.get_arg(name)?;
                let model = self.get_arg(name)?;
                let spec = self.get_arg(name)?;
                if let Some(color) = parse_color(Some(&model), &spec) {
                    self.defs.colors.insert(target.trim().to_owned(), color);
                }
                Some(Node::Group(Vec::new()))
            }
            "boxed" => Some(Node::Boxed { body: self.parse_arg(name)? }),
            "fbox" => {
                let raw = self.get_arg(name)?;
                Some(Node::Boxed { body: self.parse_text(&raw, MathFont::Roman)? })
            }
            "phantom" => Some(Node::Phantom { width: true, height: true, body: self.parse_arg(name)? }),
            "hphantom" => Some(Node::Phantom { width: true, height: false, body: self.parse_arg(name)? }),
            "vphantom" => Some(Node::Phantom { width: false, height: true, body: self.parse_arg(name)? }),
            "smash" => {
                self.get_brackets(name)?;
                Some(Node::Smash { body: self.parse_arg(name)? })
            }
            "mathstrut" => Some(Node::Phantom { width: false, height: true, body: vec![self.sym('(', Class::Open, MathFont::Roman)] }),
            "strut" => Some(Node::Rule { width: 0.0, height: 1.0, raise: -0.3 }),
            "llap" | "mathllap" => Some(Node::Lap { side: -1, body: self.parse_arg(name)? }),
            "clap" | "mathclap" => Some(Node::Lap { side: 0, body: self.parse_arg(name)? }),
            "rlap" | "mathrlap" => Some(Node::Lap { side: 1, body: self.parse_arg(name)? }),
            "rule" => {
                let raise = self.get_brackets(name)?.and_then(|raw| dimension(&raw)).unwrap_or(0.0);
                let width = self.dimension_arg(name)?;
                let height = self.dimension_arg(name)?;
                Some(Node::Rule { width, height, raise })
            }
            // `\Rule{w}{h}{d}` draws, `\Space{w}{h}{d}` only takes the room.
            "Rule" | "Space" => {
                let width = self.dimension_arg(name)?;
                let height = self.dimension_arg(name)?;
                let depth = self.dimension_arg(name)?;
                if name == "Rule" {
                    Some(Node::Rule { width, height: height + depth, raise: -depth })
                } else {
                    let strut = Node::Rule { width: 0.0, height: height + depth, raise: -depth };
                    Some(Node::Group(vec![Node::Phantom { width: false, height: true, body: vec![strut] }, Node::Space(width)]))
                }
            }
            "raise" | "lower" => {
                let by = self.dimension_here(name)?;
                let body = self.parse_arg(name)?;
                Some(Node::Raise { by: if name == "raise" { by } else { -by }, body })
            }
            "moveleft" | "moveright" => {
                let by = self.dimension_here(name)?;
                let body = self.parse_arg(name)?;
                Some(Node::Move { by: if name == "moveright" { by } else { -by }, body })
            }
            "hspace" | "mspace" => {
                self.star();
                Some(Node::Space(self.dimension_arg(name)?))
            }
            "kern" | "mkern" | "hskip" | "mskip" => Some(Node::Space(self.dimension_here(name)?)),
            _ => None,
        })
    }

    /// `GetDelimiter`.
    fn delimiter(&mut self, name: &str) -> Parsed<char> {
        self.skip_space();
        let Some(c) = self.peek() else { return Err(format!("Missing or unrecognized delimiter for \\{name}")) };
        let token = if c == '\\' {
            let start = self.at;
            self.bump();
            if self.peek().is_some_and(is_letter) {
                while self.peek().is_some_and(is_letter) {
                    self.bump();
                }
            } else {
                self.bump();
            }
            self.text[start..self.at].to_owned()
        } else {
            self.bump();
            c.to_string()
        };
        lookup_delimiter(&token).ok_or_else(|| format!("Missing or unrecognized delimiter for \\{name}"))
    }

    fn fenced(&mut self) -> Parsed<Node> {
        let left = self.delimiter("left")?;
        let saved = (self.font, self.bold);
        let (mut parts, mut middles) = (Vec::new(), Vec::new());
        let right = loop {
            let (nodes, stop) = self.list()?;
            match stop {
                Stop::Middle => {
                    parts.push(nodes);
                    middles.push(self.delimiter("middle")?);
                }
                Stop::Right => {
                    parts.push(nodes);
                    break self.delimiter("right")?;
                }
                // A line break inside delimiters has nowhere to go, and MathJax draws on.
                Stop::Cr => parts.push(nodes),
                Stop::Eof | Stop::End(_) => return Err("Extra \\left or missing \\right".to_owned()),
                Stop::Close => return Err("Extra close brace or missing open brace".to_owned()),
                Stop::Amp | Stop::AutoClose | Stop::Cd(_) => return Err("Misplaced &".to_owned()),
            }
        };
        (self.font, self.bold) = saved;
        Ok(Node::Fenced { left, parts, middles, right })
    }

    fn negated(&mut self) -> Parsed<Node> {
        // `\not\not\not` has no braces to count.
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            return Err(TOO_DEEP.to_owned());
        }
        let one = self.first_atom();
        self.depth -= 1;
        let mut one = one?;
        let inner = if one.len() == 1 { one.pop().expect("one") } else { Node::Group(one) };
        if let Node::Sym { text, class, font, bold } = &inner
            && let Some(composed) = single_char(text).and_then(negation)
        {
            return Ok(Node::Sym { text: composed.to_string(), class: *class, font: *font, bold: *bold });
        }
        Ok(Node::Not(Box::new(inner)))
    }

    // ------------------------------------------------------------ text

    fn text_arg(&mut self, name: &str, font: MathFont) -> Parsed<Node> {
        let raw = self.get_arg(name)?;
        let mut nodes = self.parse_text(&raw, font)?;
        Ok(if nodes.len() == 1 { nodes.pop().expect("one") } else { Node::Group(nodes) })
    }

    /// `textmacros`: `$` goes back to math, `%` is a comment, a space is one space, and `^`, `_`, `&` and `#` are errors. A model
    /// that writes `\text{max_value}` gets what it gets upstream. A command the text parser has no meaning for but math has says
    /// so, as `TextParser`'s fallback does, and one nobody knows is `noundefined`'s red.
    fn parse_text(&mut self, raw: &str, font: MathFont) -> Parsed<Vec<Node>> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            return Err(TOO_DEEP.to_owned());
        }
        let outer = std::mem::replace(&mut self.text, raw.to_owned());
        let at = std::mem::replace(&mut self.at, 0);
        let result = self.text_run(font);
        self.text = outer;
        self.at = at;
        self.depth -= 1;
        result
    }

    /// `$...$` or `\(...\)` in text, from just past its opener.
    fn text_math(&mut self, close: &str) -> Parsed<Node> {
        let start = self.at;
        let mut braces = 0;
        while let Some(c) = self.peek() {
            if braces == 0 && self.text[self.at..].starts_with(close) {
                let inner = self.text[start..self.at].to_owned();
                self.at += close.len();
                let saved = (self.font, self.bold);
                (self.font, self.bold) = (MathFont::Auto, false);
                let parsed = self.parse_string(&inner);
                (self.font, self.bold) = saved;
                return Ok(Node::Group(parsed?));
            }
            self.bump();
            match c {
                '\\' => {
                    self.bump();
                }
                '{' => braces += 1,
                '}' => braces -= 1,
                _ => {}
            }
        }
        Err("Math mode is not properly terminated".to_owned())
    }

    fn text_run(&mut self, font: MathFont) -> Parsed<Vec<Node>> {
        let mut font = font;
        let mut nodes = Vec::new();
        let mut run = String::new();
        macro_rules! flush {
            () => {
                if !run.is_empty() {
                    nodes.push(Node::Text { text: std::mem::take(&mut run), font });
                }
            };
        }
        while let Some(c) = self.bump() {
            match c {
                '$' => {
                    flush!();
                    nodes.push(self.text_math("$")?);
                }
                '%' => {
                    while self.peek().is_some_and(|c| c != '\n') {
                        self.bump();
                    }
                }
                '^' | '_' => return Err(format!("'{c}' allowed only in math mode")),
                '&' | '#' => return Err(format!("Misplaced {c}")),
                '~' | '\u{a0}' => run.push('\u{a0}'),
                '`' => {
                    if self.eat('`') { run.push('\u{201c}') } else { run.push('\u{2018}') }
                }
                '\'' => {
                    if self.eat('\'') { run.push('\u{201d}') } else { run.push('\u{2019}') }
                }
                '{' => {
                    let start = self.at;
                    let mut depth = 1;
                    while let Some(c) = self.bump() {
                        match c {
                            '\\' => {
                                self.bump();
                            }
                            '{' => depth += 1,
                            '}' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    if depth != 0 {
                        return Err("Missing close brace".to_owned());
                    }
                    let inner = self.text[start..self.at - 1].to_owned();
                    flush!();
                    let group = self.parse_text(&inner, font)?;
                    nodes.push(Node::Group(group));
                }
                '}' => return Err("Extra close brace or missing open brace".to_owned()),
                '\\' => {
                    let name = self.text_command_name();
                    match text_literal(&name) {
                        Some(text) => run.push_str(text),
                        None => {
                            flush!();
                            self.depth += packages::DEEP_COMMAND;
                            if self.depth > MAX_DEPTH {
                                self.depth -= packages::DEEP_COMMAND;
                                return Err(TOO_DEEP.to_owned());
                            }
                            let done = self.text_command(&name, &mut font, &mut nodes);
                            self.depth -= packages::DEEP_COMMAND;
                            done?;
                        }
                    }
                }
                c if c.is_whitespace() => {
                    self.skip_space();
                    run.push(' ');
                }
                c => run.push(c),
            }
        }
        flush!();
        Ok(nodes)
    }

    /// What `GetCS` reads in text: a named command keeps the spaces after it, except that one of them goes with it.
    fn text_command_name(&mut self) -> String {
        let start = self.at;
        while self.peek().is_some_and(is_letter) {
            self.bump();
        }
        if self.at == start {
            self.bump();
            return self.text[start..self.at].to_owned();
        }
        let name = self.text[start..self.at].to_owned();
        self.skip_space();
        name
    }

    fn text_arg_nodes(&mut self, name: &str, font: MathFont) -> Parsed<Vec<Node>> {
        let raw = self.get_arg(name)?;
        self.parse_text(&raw, font)
    }

    /// A command inside `\text`.
    fn text_command(&mut self, name: &str, font: &mut MathFont, nodes: &mut Vec<Node>) -> Parsed<()> {
        let set_font = match name {
            "rm" => Some(MathFont::Roman),
            "mit" | "it" => Some(MathFont::Italic),
            "oldstyle" => Some(MathFont::Roman),
            "cal" => Some(MathFont::Script),
            "bf" => Some(MathFont::Bold),
            "sf" => Some(MathFont::Sans),
            "tt" => Some(MathFont::Mono),
            _ => None,
        };
        if let Some(set) = set_font {
            *font = set;
            return Ok(());
        }
        let arg_font = match name {
            "textnormal" | "textup" | "textrm" => Some(MathFont::Roman),
            "textit" => Some(MathFont::Italic),
            "textbf" => Some(MathFont::Bold),
            "textsf" => Some(MathFont::Sans),
            "texttt" => Some(MathFont::Mono),
            "frak" => Some(MathFont::Fraktur),
            "Bbb" => Some(MathFont::DoubleStruck),
            _ => None,
        };
        if let Some(arg_font) = arg_font {
            nodes.push(Node::Group(self.text_arg_nodes(name, arg_font)?));
            return Ok(());
        }
        if let Some(mark) = text_accent(name) {
            let mut base = String::new();
            for node in self.text_arg_nodes(name, *font)? {
                if let Node::Text { text, .. } = node {
                    base.push_str(&text);
                }
            }
            base.push(mark);
            nodes.push(Node::Text { text: base, font: *font });
            return Ok(());
        }
        let size = match name {
            "Tiny" => Some(0.5),
            "tiny" => Some(0.6),
            "scriptsize" | "SMALL" => Some(0.7),
            "Small" | "footnotesize" => Some(0.8),
            "small" => Some(0.9),
            "normalsize" => Some(1.0),
            "large" => Some(1.095),
            "Large" => Some(1.2),
            "LARGE" => Some(1.44),
            "huge" => Some(1.73),
            "Huge" => Some(2.07),
            "HUGE" => Some(2.49),
            _ => None,
        };
        if let Some(size) = size {
            nodes.push(Node::SetSize(size));
            return Ok(());
        }
        if let Some(space) = fixed_space(name) {
            nodes.push(Node::Space(space));
            return Ok(());
        }
        let node = match name {
            "(" => self.text_math("\\)")?,
            "\\" => return Ok(()),
            "emph" => {
                let inner = if *font == MathFont::Italic { MathFont::Roman } else { MathFont::Italic };
                Node::Group(self.text_arg_nodes(name, inner)?)
            }
            "hskip" | "kern" | "mskip" | "mkern" => Node::Space(self.dimension_here(name)?),
            "hspace" | "mspace" => {
                self.star();
                Node::Space(self.dimension_arg(name)?)
            }
            "rule" | "Rule" | "Space" | "unicode" | "U" | "char" | "mmlToken" => {
                let mut one = Vec::new();
                self.command(name, &mut one)?;
                Node::Group(one)
            }
            "color" => {
                let model = self.get_brackets(name)?;
                let spec = self.get_arg(name)?;
                match self.color(model.as_deref(), &spec) {
                    Some(color) => Node::SetColor(color),
                    None => return Ok(()),
                }
            }
            "textcolor" => {
                let model = self.get_brackets(name)?;
                let spec = self.get_arg(name)?;
                let body = self.text_arg_nodes(name, *font)?;
                match self.color(model.as_deref(), &spec) {
                    Some(color) => Node::Colored { color, body },
                    None => Node::Group(body),
                }
            }
            "colorbox" => {
                let model = self.get_brackets(name)?;
                let spec = self.get_arg(name)?;
                let body = self.text_arg_nodes(name, *font)?;
                Node::ColorBox { fill: self.color(model.as_deref(), &spec), stroke: None, body }
            }
            "fcolorbox" => {
                let model = self.get_brackets(name)?;
                let stroke = self.get_arg(name)?;
                let fill = self.get_arg(name)?;
                let body = self.text_arg_nodes(name, *font)?;
                Node::ColorBox { fill: self.color(model.as_deref(), &fill), stroke: self.color(model.as_deref(), &stroke), body }
            }
            "ref" | "eqref" => {
                self.get_arg(name)?;
                // `tags: 'none'`: nothing has a number, and a reference says `###` as MathJax's empty label does.
                Node::Text { text: if name == "eqref" { "(###)" } else { "###" }.to_owned(), font: *font }
            }
            "underline" => Node::Over { kind: OverKind::Underline, body: self.text_arg_nodes(name, *font)? },
            "llap" => Node::Lap { side: -1, body: self.text_arg_nodes(name, *font)? },
            "rlap" => Node::Lap { side: 1, body: self.text_arg_nodes(name, *font)? },
            "phantom" => Node::Phantom { width: true, height: true, body: self.text_arg_nodes(name, *font)? },
            "vphantom" => Node::Phantom { width: false, height: true, body: self.text_arg_nodes(name, *font)? },
            "hphantom" => Node::Phantom { width: true, height: false, body: self.text_arg_nodes(name, *font)? },
            "smash" => {
                self.get_brackets(name)?;
                Node::Smash { body: self.text_arg_nodes(name, *font)? }
            }
            _ => {
                if let Some(text) = packages::text_symbol(name) {
                    Node::Text { text: text.to_owned(), font: *font }
                } else if self.lookup_macro(name).is_some() || packages::defined_in_math(name) {
                    return Err(format!("\\{name} is only supported in math mode"));
                } else {
                    Node::Undefined(format!("\\{name}"))
                }
            }
        };
        nodes.push(node);
        Ok(())
    }

    // ------------------------------------------------------------ environments

    /// `ParseUtil.checkEqnEnv`: `align` and its kind number their rows, and only make sense as the whole of a formula.
    fn check_equation_structure(&mut self, nodes: &[Node]) -> Parsed<()> {
        let at = self.at;
        let name = self.get_arg("begin")?;
        self.at = at;
        let taggable = matches!(name.trim(), "equation" | "equation*" | "displaymath" | "align" | "align*" | "gather" | "gather*" | "multline" | "multline*" | "eqnarray" | "eqnarray*" | "alignat" | "alignat*" | "xalignat" | "xalignat*" | "xxalignat" | "flalign" | "flalign*" | "empheq" | "spreadlines");
        if !taggable || self.lookup_env(name.trim()).is_some() {
            return Ok(());
        }
        let empty = nodes.iter().all(|n| matches!(n, Node::Space(_)));
        if std::mem::take(&mut self.nesting) || (self.depth == 1 && empty) {
            return Ok(());
        }
        if self.nest_depth == Some(self.depth) && empty && !matches!(name.trim(), "gather" | "gather*") {
            return Ok(());
        }
        Err("Erroneous nesting of equation structures".to_owned())
    }

    fn environment(&mut self) -> Parsed<Node> {
        let name = self.get_arg("begin")?;
        self.environment_named(name.trim())
    }

    fn environment_named(&mut self, name: &str) -> Parsed<Node> {
        if let Some(definition) = self.lookup_env(name) {
            return self.defined_environment(name, definition);
        }
        // What reads a whole nest of these has to be small: the shells below are a lot of arrays to hold on the way down.
        match name {
            "equation" | "equation*" | "displaymath" | "math" | "subequations" => return self.plain_environment(name),
            "indentalign" => {
                for _ in 0..3 {
                    self.get_brackets(name)?;
                }
                self.get_arg(name)?;
                return self.plain_environment(name);
            }
            "spreadlines" => {
                self.dimension_arg(name)?;
                self.nesting = true;
                let inner = self.plain_environment(name);
                self.nesting = false;
                return inner;
            }
            "CD" => return self.diagram_environment(),
            "prooftree" => return self.prooftree(),
            "empheq" => return self.empheq(),
            "numcases" | "subnumcases" => {
                let mut out = self.parse_arg(&format!("begin{{{name}}}"))?;
                let wrap = CellWrap { text_from: Some(1), ..CellWrap::default() };
                let shell = Array { columns: vec!['l'], left: '{', ..plain_array() };
                out.push(Node::Array(Box::new(self.array_rows(shell, Some(name), &wrap)?)));
                return Ok(Node::Group(out));
            }
            _ => {}
        }
        let (shell, wrap) = self.array_shell(name)?;
        Ok(Node::Array(Box::new(self.array_rows(*shell, Some(name), &wrap)?)))
    }

    fn diagram_environment(&mut self) -> Parsed<Node> {
        let saved = self.diagram.replace(self.cd_least);
        let shell = Array { style: MathStyle::Display, col_gap: 0.5, baseline: 0.0, jot: 0.5, diagram: true, ..plain_array() };
        let array = self.array_rows(shell, Some("CD"), &CellWrap::default());
        self.diagram = saved;
        Ok(Node::Array(Box::new(array?)))
    }

    /// An array environment's options, read from where the name left off, and the array they make.
    #[inline(never)]
    fn array_shell(&mut self, name: &str) -> Parsed<(Box<Array>, CellWrap)> {
        let base = plain_array();
        let aligned = || Array { style: MathStyle::Display, columns: vec!['r', 'l'], col_gap: 2.0, paired: true, jot: 0.25, ..plain_array() };
        let gathered = || Array { style: MathStyle::Display, jot: 0.25, ..plain_array() };
        let cases = |left, right, style, jot| Array { columns: vec!['l'], left, right, style, jot, ..plain_array() };
        let small = |left, right| Array { style: MathStyle::Script, col_gap: 1.0 / 3.0, baseline: 0.9, left, right, ..plain_array() };
        let mut wrap = CellWrap::default();
        let shell = match name {
            "matrix" => base,
            "pmatrix" => Array { left: '(', right: ')', ..base },
            "bmatrix" => Array { left: '[', right: ']', ..base },
            "Bmatrix" => Array { left: '{', right: '}', ..base },
            "vmatrix" => Array { left: '|', right: '|', ..base },
            "Vmatrix" => Array { left: '\u{2016}', right: '\u{2016}', ..base },
            "smallmatrix" | "smallmatrix*" | "psmallmatrix" | "psmallmatrix*" | "bsmallmatrix" | "bsmallmatrix*" | "Bsmallmatrix" | "Bsmallmatrix*" | "vsmallmatrix" | "vsmallmatrix*" | "Vsmallmatrix" | "Vsmallmatrix*" => {
                let (left, right) = matrix_fences(name);
                let columns = if name.ends_with('*') { self.matrix_star(name)? } else { vec!['c'] };
                Array { columns, ..small(left, right) }
            }
            "matrix*" | "pmatrix*" | "bmatrix*" | "Bmatrix*" | "vmatrix*" | "Vmatrix*" => {
                let (left, right) = matrix_fences(name);
                Array { columns: self.matrix_star(name)?, left, right, ..base }
            }
            "cases" => cases('{', '\0', MathStyle::Text, 0.0),
            "dcases" => cases('{', '\0', MathStyle::Display, 0.25),
            "rcases" => cases('\0', '}', MathStyle::Text, 0.0),
            "drcases" => cases('\0', '}', MathStyle::Display, 0.25),
            // Their second column is text.
            "cases*" | "dcases*" | "rcases*" | "drcases*" => {
                wrap.text_from = Some(1);
                let (left, right) = if matches!(name, "cases*" | "dcases*") { ('{', '\0') } else { ('\0', '}') };
                let (style, jot) = if name.starts_with('d') { (MathStyle::Display, 0.25) } else { (MathStyle::Text, 0.0) };
                cases(left, right, style, jot)
            }
            "aligned" | "align" | "align*" | "flalign" | "flalign*" => Array { valign: self.array_valign_if(name == "aligned")?, ..aligned() },
            "split" => Array { col_gap: 0.0, ..aligned() },
            "eqnarray" | "eqnarray*" => Array { columns: vec!['r', 'c', 'l'], col_gap: 5.0 / 18.0, paired: false, ..aligned() },
            "alignedat" | "alignat" | "alignat*" => {
                let valign = self.array_valign_if(name == "alignedat")?;
                self.get_arg(name)?;
                Array { col_gap: 0.0, valign, ..aligned() }
            }
            "xalignat" | "xalignat*" | "xxalignat" => {
                self.get_arg(name)?;
                aligned()
            }
            "gathered" => Array { valign: self.array_valign()?, ..gathered() },
            "gather" | "gather*" => Array { gather: true, ..gathered() },
            "lgathered" => Array { columns: vec!['l'], valign: self.array_valign_if(true)?, ..gathered() },
            "rgathered" => Array { columns: vec!['r'], valign: self.array_valign_if(true)?, ..gathered() },
            "multline" | "multline*" => Array { multline: true, jot: 0.5, ..gathered() },
            "multlined" => {
                let (valign, width) = self.multlined_options()?;
                Array { multline: true, jot: 0.5, valign, widths: width.map_or(Vec::new(), |w| vec![Some(w)]), ..gathered() }
            }
            "array" | "darray" => {
                let valign = self.array_valign()?;
                let spec = self.get_arg("array")?;
                let columns = self.column_spec(&spec)?;
                let style = if name == "darray" { MathStyle::Display } else { MathStyle::Text };
                wrap.before = columns.before;
                wrap.after = columns.after;
                Array { columns: columns.align, rules: columns.rules, dashed: columns.dashed, fills: columns.fills, widths: columns.widths, no_gap: columns.no_gap, valign, style, ..base }
            }
            "subarray" | "crampedsubarray" => {
                let spec = self.get_arg("array")?;
                let columns = self.column_spec(&spec)?;
                Array { columns: columns.align, rules: columns.rules, dashed: columns.dashed, style: MathStyle::Script, col_gap: 0.0, baseline: 1.0, ..base }
            }
            _ => return Err(format!("Unknown environment '{name}'")),
        };
        Ok((Box::new(shell), wrap))
    }

    /// What holds formula text and nothing else: everything to its `\end`.
    fn plain_environment(&mut self, name: &str) -> Parsed<Node> {
        let saved = (self.font, self.bold);
        let mut lines: Vec<Vec<Node>> = Vec::new();
        let node = loop {
            let (nodes, stop) = self.list()?;
            match stop {
                Stop::Cr => lines.push(nodes),
                Stop::End(found) if found.trim() == name => {
                    if lines.is_empty() {
                        break Node::Group(nodes);
                    }
                    if !nodes.is_empty() {
                        lines.push(nodes);
                    }
                    break Node::Lines(lines);
                }
                Stop::End(found) => return Err(format!("\\begin{{{name}}} ended with \\end{{{found}}}")),
                Stop::Eof => return Err(format!("Missing \\end{{{name}}}")),
                Stop::Close => return Err(format!("Extra close brace while looking for \\end{{{name}}}")),
                Stop::Amp | Stop::AutoClose | Stop::Cd(_) => return Err("Misplaced &".to_owned()),
                Stop::Right => return Err("Missing \\left or extra \\right".to_owned()),
                Stop::Middle => return Err("Extra \\middle".to_owned()),
            }
        };
        (self.font, self.bold) = saved;
        Ok(node)
    }

    /// `matrix*`'s `[r]`.
    fn matrix_star(&mut self, name: &str) -> Parsed<Vec<char>> {
        let side = self.get_brackets(&format!("begin{{{name}}}"))?;
        Ok(vec![match side.as_deref().map(str::trim) {
            Some("l") => 'l',
            Some("r") => 'r',
            _ => 'c',
        }])
    }

    /// `\begin{multlined}[t][4cm]`, in either order of what is given.
    fn multlined_options(&mut self) -> Parsed<(VAlign, Option<Em>)> {
        let mut valign = VAlign::Center;
        let mut width = None;
        if self.peek() == Some('[') {
            let first = self.get_brackets("begin{multlined}")?.unwrap_or_default();
            match first.trim() {
                "t" => valign = VAlign::Top,
                "b" => valign = VAlign::Bottom,
                "c" | "" => {}
                other => width = Some(dimension(other).ok_or_else(|| "Width for \\begin{multlined} must be a dimension".to_owned())?),
            }
            if width.is_none() && self.peek() == Some('[') {
                let second = self.get_brackets("begin{multlined}")?.unwrap_or_default();
                if !second.trim().is_empty() {
                    width = Some(dimension(&second).ok_or_else(|| "Width for \\begin{multlined} must be a dimension".to_owned())?);
                }
            }
        }
        Ok((valign, width))
    }

    fn array_valign(&mut self) -> Parsed<VAlign> {
        Ok(match self.get_brackets("array")?.as_deref().map(str::trim) {
            Some("t") => VAlign::Top,
            Some("b") => VAlign::Bottom,
            _ => VAlign::Center,
        })
    }

    fn array_valign_if(&mut self, takes_one: bool) -> Parsed<VAlign> {
        if takes_one { self.array_valign() } else { Ok(VAlign::Center) }
    }

    /// One cell's worth of text, up to the `&`, `\`, `\end` or brace that ends it: `CasesMethods.Entry`.
    fn text_cell(&mut self) -> Parsed<Vec<Node>> {
        let start = self.at;
        let mut braces = 0;
        while let Some(c) = self.peek() {
            match c {
                '{' => braces += 1,
                '}' if braces == 0 => break,
                '}' => braces -= 1,
                '&' if braces == 0 => return Err("Extra alignment tab in text for numcase environment".to_owned()),
                '\\' if braces == 0 => {
                    let rest = &self.text[self.at + 1..];
                    let word: String = rest.chars().take_while(|c| is_letter(*c)).collect();
                    if rest.starts_with('\\') || word == "cr" || word == "end" || word == "label" || rest.is_empty() {
                        break;
                    }
                    self.bump();
                    if word.is_empty() {
                        self.bump();
                    } else {
                        self.at += word.len();
                    }
                    continue;
                }
                '\\' => {
                    self.bump();
                }
                _ => {}
            }
            self.bump();
        }
        let raw = self.text[start..self.at].trim_start().to_owned();
        self.parse_text(&raw, MathFont::Roman)
    }

    /// Reads cells to `\end{name}`, or to the end of the input for a `\substack`.
    fn array_rows(&mut self, mut array: Array, name: Option<&str>, wrap: &CellWrap) -> Parsed<Array> {
        let saved = (self.font, self.bold);
        let saved_cell = self.cell.take();
        let result = self.array_rows_inner(&mut array, name, wrap);
        self.cell = saved_cell;
        (self.font, self.bold) = saved;
        result?;
        Ok(array)
    }

    fn array_rows_inner(&mut self, array: &mut Array, name: Option<&str>, wrap: &CellWrap) -> Parsed<()> {
        let mut row = Row::default();
        loop {
            let column = row.cells.len();
            self.cell = Some(CellState { column, multline: array.multline, aligned: array.paired, first_row: array.rows.is_empty(), ..CellState::default() });
            // A row can start with rules, and a last one can be nothing but rules.
            loop {
                self.skip_space();
                if self.eat_command("hline") {
                    row.rule_above = true;
                } else if self.eat_command("hdashline") {
                    row.rule_above = true;
                    row.dashed_above = true;
                } else {
                    break;
                }
            }
            // What the column spec's `>{...}` puts before each of its cells is read as part of the cell, so a `\bf` there holds for it.
            if let Some(before) = wrap.before.get(column).filter(|b| !b.is_empty()) {
                let rest_at = self.at;
                self.text.insert_str(rest_at, &format!("{before} "));
            }
            let cell_font = (self.font, self.bold);
            // A `gather`'s cell can hold an `align`, one level down.
            self.nest_depth = array.gather.then_some(self.depth + 1);
            let (mut nodes, stop) = if wrap.text_from.is_some_and(|from| column >= from) {
                let text = self.text_cell()?;
                let (mut rest, stop) = self.list()?;
                let mut all = text;
                all.append(&mut rest);
                (all, stop)
            } else {
                self.list()?
            };
            self.nest_depth = None;
            if let Some(after) = wrap.after.get(column).filter(|a| !a.is_empty()) {
                nodes.extend(self.parse_string(after)?);
            }
            (self.font, self.bold) = cell_font;
            // TeX sets an `align`'s right side as if something stood before it, so `&=` gets a relation's space.
            if array.paired && row.cells.len() % 2 == 1 {
                nodes.insert(0, Node::Group(Vec::new()));
            }
            let cell = self.cell.take().unwrap_or_default();
            if let Some(fill) = cell.fill {
                row.fills.resize(column + 1, None);
                row.fills[column] = Some(fill);
            }
            if cell.row_fill.is_some() {
                row.fill = cell.row_fill;
            }
            if let Some((at, fill)) = cell.column_fill {
                array.fills.resize(array.fills.len().max(at + 1), None);
                array.fills[at] = Some(fill);
            }
            if cell.tag.is_some() {
                row.tag = cell.tag;
            }
            if cell.shove.is_some() {
                row.shove = cell.shove;
            }
            let empty_row = row.cells.is_empty() && nodes.is_empty();
            match stop {
                Stop::Amp => row.cells.push(nodes),
                Stop::Cd(kind) => {
                    if !nodes.is_empty() {
                        row.cells.push(nodes);
                    }
                    // As `AmsCdMethods.arrow`: arrows between objects on an odd row of the table, and in the object's column on
                    // an even one.
                    let arrow_row = array.rows.len() % 2 == 1;
                    let mut pad = (row.cells.len() + if arrow_row { 0 } else { 1 }) % 2;
                    while pad > 0 {
                        row.cells.push(Vec::new());
                        pad -= 1;
                    }
                    let arrow = self.cd_arrow(kind)?;
                    row.cells.push(arrow);
                }
                Stop::Cr => {
                    row.cells.push(nodes);
                    row.extra = self.cr_extra;
                    array.rows.push(std::mem::take(&mut row));
                }
                Stop::End(found) => {
                    let Some(name) = name else { return Err(format!("Missing \\begin{{{found}}} or extra \\end{{{found}}}")) };
                    if found.trim() != name {
                        return Err(format!("\\begin{{{name}}} ended with \\end{{{found}}}"));
                    }
                    finish_row(array, row, nodes, empty_row);
                    break;
                }
                Stop::Eof => {
                    if let Some(name) = name {
                        return Err(format!("Missing \\end{{{name}}}"));
                    }
                    finish_row(array, row, nodes, empty_row);
                    break;
                }
                Stop::Close => {
                    return Err(match name {
                        Some(name) => format!("Extra close brace while looking for \\end{{{name}}}"),
                        None => "Extra close brace or missing open brace".to_owned(),
                    });
                }
                Stop::Right => return Err("Missing \\left or extra \\right".to_owned()),
                Stop::Middle => return Err("Extra \\middle".to_owned()),
                Stop::AutoClose => return Err("Misplaced &".to_owned()),
            }
        }
        Ok(())
    }

    fn eat_command(&mut self, name: &str) -> bool {
        let rest = &self.text[self.at..];
        let Some(after) = rest.strip_prefix('\\').and_then(|r| r.strip_prefix(name)) else { return false };
        if after.chars().next().is_some_and(is_letter) {
            return false;
        }
        self.at += name.len() + 1;
        true
    }
}

fn finish_row(array: &mut Array, mut row: Row, nodes: Vec<Node>, empty: bool) {
    if empty {
        // `a \\ b \\` has two rows, not three; a `\hline` after the last of them is a rule under it.
        array.rule_below = row.rule_above;
        array.dashed_below = row.dashed_above;
    } else {
        row.cells.push(nodes);
        array.rows.push(row);
    }
}

fn set_limits(nodes: &mut [Node], name: &str) -> Parsed<()> {
    let value = match name {
        "limits" => Some(true),
        "nolimits" => Some(false),
        _ => None,
    };
    let target = match nodes.last_mut() {
        Some(Node::Scripts { base, .. }) => Some(&mut **base),
        Some(last) => Some(last),
        None => None,
    };
    if let Some(Node::Op { limits, .. }) = target {
        *limits = value;
        return Ok(());
    }
    Err(format!("\\{name} is allowed only on operators"))
}

fn single_char(text: &str) -> Option<char> {
    let mut chars = text.chars();
    let c = chars.next()?;
    chars.next().is_none().then_some(c)
}

/// `substituteArgs`: `#1` and its kind, and `##` for a `#`. An argument goes in as it stands, so `#1^2` of `xy` is a `y` squared, and
/// `addArgs` keeps a command before it from gluing onto its first letter.
fn substitute(body: &str, values: &[String]) -> String {
    let mut out = String::with_capacity(body.len() + values.iter().map(String::len).sum::<usize>());
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '#' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('#') => {
                chars.next();
                out.push('#');
            }
            Some(d) if d.is_ascii_digit() => {
                chars.next();
                if let Some(value) = d.to_digit(10).filter(|d| *d > 0).and_then(|d| values.get(d as usize - 1)) {
                    if value.starts_with(is_letter) && ends_in_command(&out) {
                        out.push(' ');
                    }
                    out.push_str(value);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// Whether text ends in a named command, whose name what comes next would otherwise run on from.
fn ends_in_command(text: &str) -> bool {
    let letters = text.bytes().rev().take_while(u8::is_ascii_alphabetic).count();
    letters > 0 && text[..text.len() - letters].bytes().rev().take_while(|b| *b == b'\\').count() % 2 == 1
}

/// A length in `em`s. MathJax's table: a point is a tenth of an em at the size it assumes.
pub(super) fn dimension(text: &str) -> Option<Em> {
    let text = text.trim();
    let split = text.char_indices().find(|(_, c)| !(c.is_ascii_digit() || matches!(*c, '.' | '-' | '+'))).map_or(text.len(), |(i, _)| i);
    let number: f32 = text[..split].parse().ok()?;
    let per_unit = match text[split..].trim() {
        "em" => 1.0,
        "ex" => 0.43,
        "pt" => 0.1,
        "pc" => 1.2,
        "px" => 0.1,
        "in" => 7.2,
        "cm" => 7.2 / 2.54,
        "mm" => 7.2 / 25.4,
        "mu" => 1.0 / 18.0,
        // MathJax also takes a bare `0`.
        "" if number == 0.0 => 0.0,
        _ => return None,
    };
    // No length has any business being thousands of screens wide.
    Some((number * per_unit).clamp(-1000.0, 1000.0))
}

/// More columns than this is a `*{9}{*{9}{...}}` going off, not a table.
const MAX_COLUMNS: usize = 256;

/// `ColumnParser`'s answer about a column spec.
#[derive(Default)]
struct ColumnSpec {
    align: Vec<char>,
    rules: Vec<u8>,
    dashed: Vec<bool>,
    fills: Vec<Option<Color>>,
    widths: Vec<Option<Em>>,
    no_gap: Vec<bool>,
    before: Vec<String>,
    after: Vec<String>,
}

/// What a column spec, or a kind of environment, does to a cell as it is read.
#[derive(Default)]
struct CellWrap {
    before: Vec<String>,
    after: Vec<String>,
    /// Columns from here on are text: `cases*` and `numcases`.
    text_from: Option<usize>,
}

/// One braced group from a spec's iterator, without its braces.
fn spec_group(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut raw = String::new();
    let mut depth = 0;
    for c in chars.by_ref() {
        match c {
            '{' => {
                depth += 1;
                if depth == 1 {
                    continue;
                }
            }
            '}' => {
                depth -= 1;
                if depth <= 0 {
                    break;
                }
            }
            _ => {}
        }
        raw.push(c);
    }
    raw
}

impl Parser {
    fn column_spec(&mut self, spec: &str) -> Parsed<ColumnSpec> {
        let mut out = ColumnSpec { rules: vec![0], dashed: vec![false], ..ColumnSpec::default() };
        let mut pending_before = String::new();
        self.read_spec(spec, &mut out, &mut pending_before, 0)?;
        if out.align.is_empty() {
            out.align.push('c');
            out.rules.push(0);
            out.dashed.push(false);
        }
        Ok(out)
    }

    fn read_spec(&mut self, spec: &str, out: &mut ColumnSpec, pending: &mut String, level: u32) -> Parsed<()> {
        // `\newcolumntype` could call itself.
        if level > 8 {
            return Err("Too many column specifiers (perhaps looping column definitions?)".to_owned());
        }
        let mut chars = spec.chars().peekable();
        while let Some(c) = chars.next() {
            let add = |out: &mut ColumnSpec, align: char, width: Option<Em>, pending: &mut String| {
                if out.align.len() >= MAX_COLUMNS {
                    return;
                }
                out.align.push(align);
                out.rules.push(0);
                out.dashed.push(false);
                out.widths.push(width);
                out.no_gap.push(false);
                out.before.push(std::mem::take(pending));
                out.after.push(String::new());
                out.fills.push(None);
            };
            match c {
                'l' | 'c' | 'r' | 'j' => add(out, if c == 'j' { 'l' } else { c }, None, pending),
                'p' | 'm' | 'b' | 'P' | 'M' | 'B' => {
                    let width = dimension(&spec_group(&mut chars));
                    add(out, if c.is_ascii_uppercase() { 'c' } else { 'l' }, width, pending);
                }
                'w' | 'W' => {
                    let align = spec_group(&mut chars).trim().chars().next().unwrap_or('l');
                    let width = dimension(&spec_group(&mut chars));
                    add(out, align, width, pending);
                }
                '|' | ':' => {
                    if let Some(last) = out.rules.last_mut() {
                        *last = last.saturating_add(1);
                    }
                    if c == ':'
                        && let Some(last) = out.dashed.last_mut()
                    {
                        *last = true;
                    }
                }
                '@' | '!' => {
                    let inner = spec_group(&mut chars);
                    if c == '@'
                        && let Some(last) = out.no_gap.last_mut()
                    {
                        *last = true;
                    }
                    match out.after.last_mut() {
                        Some(after) => after.push_str(&inner),
                        None => pending.push_str(&inner),
                    }
                }
                '>' => {
                    let inner = spec_group(&mut chars);
                    pending.push_str(&inner);
                }
                '<' => {
                    let inner = spec_group(&mut chars);
                    if let Some(after) = out.after.last_mut() {
                        after.push_str(&inner);
                    }
                }
                '*' => {
                    let times: usize = spec_group(&mut chars).trim().parse().unwrap_or(0);
                    let body = spec_group(&mut chars);
                    for _ in 0..times {
                        if out.align.len() >= MAX_COLUMNS {
                            break;
                        }
                        self.read_spec(&body, out, pending, level + 1)?;
                    }
                }
                ' ' => {}
                other => {
                    let Some((args, body)) = self.defs.columns.get(&other.to_string()).cloned() else { continue };
                    let values: Vec<String> = (0..args).map(|_| spec_group(&mut chars)).collect();
                    self.read_spec(&substitute(&body, &values), out, pending, level + 1)?;
                }
            }
        }
        Ok(())
    }
}

/// A matrix environment's brackets, from the first letter of its name.
fn matrix_fences(name: &str) -> (char, char) {
    match name.as_bytes()[0] {
        b'p' => ('(', ')'),
        b'b' => ('[', ']'),
        b'B' => ('{', '}'),
        b'v' => ('|', '|'),
        b'V' => ('\u{2016}', '\u{2016}'),
        _ => ('\0', '\0'),
    }
}

fn plain_array() -> Array {
    Array { rows: Vec::new(), rule_below: false, dashed_below: false, columns: vec!['c'], rules: Vec::new(), dashed: Vec::new(), fills: Vec::new(), widths: Vec::new(), no_gap: Vec::new(), style: MathStyle::Text, col_gap: 1.0, paired: false, baseline: 1.2, jot: 0.0, left: '\0', right: '\0', valign: VAlign::Center, multline: false, diagram: false, gather: false }
}

// ---------------------------------------------------------------- what the names mean

/// Greek and what acts like it: these take the current font.
fn letter(name: &str) -> Option<&'static str> {
    Some(match name {
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" => "ϵ",
        "varepsilon" => "ε",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" => "θ",
        "vartheta" => "ϑ",
        "iota" => "ι",
        "kappa" => "κ",
        "varkappa" => "ϰ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "omicron" => "ο",
        "pi" => "π",
        "varpi" => "ϖ",
        "rho" => "ρ",
        "varrho" => "ϱ",
        "sigma" => "σ",
        "varsigma" => "ς",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" => "ϕ",
        "varphi" => "φ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Upsilon" => "Υ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        "partial" => "∂",
        "nabla" => "∇",
        "imath" => "ı",
        "jmath" => "ȷ",
        "digamma" => "ϝ",
        _ => return None,
    })
}

/// `\varGamma` and its kind: capitals, slanted.
fn italic_capital(name: &str) -> Option<&'static str> {
    Some(match name {
        "varGamma" => "Γ",
        "varDelta" => "Δ",
        "varTheta" => "Θ",
        "varLambda" => "Λ",
        "varXi" => "Ξ",
        "varPi" => "Π",
        "varSigma" => "Σ",
        "varUpsilon" => "Υ",
        "varPhi" => "Φ",
        "varPsi" => "Ψ",
        "varOmega" => "Ω",
        _ => return None,
    })
}

fn ordinary(name: &str) -> Option<&'static str> {
    Some(match name {
        "upalpha" => "α",
        "upbeta" => "β",
        "upgamma" => "γ",
        "updelta" => "δ",
        "upepsilon" => "ε",
        "uptheta" => "θ",
        "uplambda" => "λ",
        "upmu" => "μ",
        "uppi" => "π",
        "uprho" => "ρ",
        "upsigma" => "σ",
        "uptau" => "τ",
        "upphi" => "φ",
        "uppsi" => "ψ",
        "upomega" => "ω",
        "upzeta" => "ζ",
        "upeta" => "η",
        "upiota" => "ι",
        "upkappa" => "κ",
        "upnu" => "ν",
        "upxi" => "ξ",
        "upomicron" => "ο",
        "upupsilon" => "υ",
        "upchi" => "χ",
        "upvarepsilon" => "ϵ",
        "upvartheta" => "ϑ",
        "upvarpi" => "ϖ",
        "upvarrho" => "ϱ",
        "upvarsigma" => "ς",
        "upvarphi" => "ϕ",
        "Upgamma" => "Γ",
        "Updelta" => "Δ",
        "Uptheta" => "Θ",
        "Uplambda" => "Λ",
        "Upxi" => "Ξ",
        "Uppi" => "Π",
        "Upsigma" => "Σ",
        "Upupsilon" => "Υ",
        "Upphi" => "Φ",
        "Uppsi" => "Ψ",
        "Upomega" => "Ω",
        "AA" => "Å",
        "And" => "&",
        "diagup" => "╱",
        "diagdown" => "╲",
        "aleph" => "ℵ",
        "beth" => "ℶ",
        "gimel" => "ℷ",
        "daleth" => "ℸ",
        "hbar" | "hslash" => "ℏ",
        "ell" => "ℓ",
        "wp" => "℘",
        "Re" => "ℜ",
        "Im" => "ℑ",
        "infty" => "∞",
        "prime" => "′",
        "backprime" => "‵",
        "emptyset" => "∅",
        "varnothing" => "∅",
        "top" => "⊤",
        "bot" => "⊥",
        "forall" => "∀",
        "exists" => "∃",
        "nexists" => "∄",
        "neg" | "lnot" => "¬",
        "flat" => "♭",
        "natural" => "♮",
        "sharp" => "♯",
        "clubsuit" => "♣",
        "diamondsuit" => "♢",
        "heartsuit" => "♡",
        "spadesuit" => "♠",
        "angle" => "∠",
        "measuredangle" => "∡",
        "sphericalangle" => "∢",
        "triangle" => "△",
        "triangledown" => "▽",
        "blacktriangle" => "▲",
        "blacktriangledown" => "▼",
        "square" | "Box" => "□",
        "blacksquare" => "■",
        "lozenge" => "◊",
        "blacklozenge" => "⧫",
        "bigstar" => "★",
        "surd" => "√",
        "mho" => "℧",
        "eth" => "ð",
        "Finv" => "Ⅎ",
        "Game" => "⅁",
        "complement" => "∁",
        "Bbbk" => "𝕜",
        "backslash" => "\\",
        "vert" | "|" => "|",
        "Vert" => "‖",
        "%" => "%",
        "#" => "#",
        "$" => "$",
        "&" => "&",
        "_" => "_",
        "vdots" => "⋮",
        "checkmark" => "✓",
        "maltese" => "✠",
        "circledR" => "®",
        "circledS" => "Ⓢ",
        "yen" => "¥",
        "S" => "§",
        "degree" => "°",
        "celsius" => "℃",
        "ohm" => "Ω",
        "micro" => "µ",
        "perthousand" => "‰",
        "textbackslash" => "\\",
        "textasciitilde" => "~",
        "textasciicircum" => "^",
        _ => return None,
    })
}

fn binary(name: &str) -> Option<&'static str> {
    Some(match name {
        "pm" => "±",
        "mp" => "∓",
        "times" | "cross" => "×",
        "div" => "÷",
        "ast" => "∗",
        "star" => "⋆",
        "circ" => "∘",
        "bullet" => "∙",
        "cdot" | "centerdot" | "vdot" => "⋅",
        "cap" => "∩",
        "cup" => "∪",
        "uplus" => "⊎",
        "sqcap" => "⊓",
        "sqcup" => "⊔",
        "vee" | "lor" => "∨",
        "wedge" | "land" => "∧",
        "setminus" | "smallsetminus" => "∖",
        "wr" => "≀",
        "diamond" => "⋄",
        "bigtriangleup" => "△",
        "bigtriangledown" => "▽",
        "triangleleft" => "◃",
        "triangleright" => "▹",
        "lhd" => "⊲",
        "rhd" => "⊳",
        "unlhd" => "⊴",
        "unrhd" => "⊵",
        "oplus" => "⊕",
        "ominus" => "⊖",
        "otimes" => "⊗",
        "oslash" => "⊘",
        "odot" => "⊙",
        "bigcirc" => "◯",
        "dagger" => "†",
        "ddagger" => "‡",
        "amalg" => "⨿",
        "dotplus" => "∔",
        "doublebarwedge" => "⩞",
        "barwedge" => "⊼",
        "veebar" => "⊻",
        "Cap" | "doublecap" => "⋒",
        "Cup" | "doublecup" => "⋓",
        "curlywedge" => "⋏",
        "curlyvee" => "⋎",
        "boxminus" => "⊟",
        "boxtimes" => "⊠",
        "boxdot" => "⊡",
        "boxplus" => "⊞",
        "divideontimes" => "⋇",
        "ltimes" => "⋉",
        "rtimes" => "⋊",
        "leftthreetimes" => "⋋",
        "rightthreetimes" => "⋌",
        "circledast" => "⊛",
        "circledcirc" => "⊚",
        "circleddash" => "⊝",
        "intercal" => "⊺",
        "notChar" => "⧸",
        "vartriangle" => "△",
        _ => return None,
    })
}

fn relation(name: &str) -> Option<&'static str> {
    Some(match name {
        "lt" => "<",
        "gt" => ">",
        "leq" | "le" => "≤",
        "geq" | "ge" => "≥",
        "ll" => "≪",
        "gg" => "≫",
        "lll" | "llless" => "⋘",
        "ggg" | "gggtr" => "⋙",
        "neq" | "ne" => "≠",
        "equiv" => "≡",
        "sim" | "thicksim" => "∼",
        "simeq" => "≃",
        "asymp" => "≍",
        "approx" | "thickapprox" => "≈",
        "cong" => "≅",
        "doteq" => "≐",
        "doteqdot" | "Doteq" => "≑",
        "propto" | "varpropto" => "∝",
        "subset" => "⊂",
        "supset" => "⊃",
        "subseteq" | "subseteqq" => "⊆",
        "supseteq" | "supseteqq" => "⊇",
        "subsetneq" | "subsetneqq" => "⊊",
        "supsetneq" | "supsetneqq" => "⊋",
        "sqsubset" => "⊏",
        "sqsupset" => "⊐",
        "sqsubseteq" => "⊑",
        "sqsupseteq" => "⊒",
        "in" => "∈",
        "ni" | "owns" => "∋",
        "notin" => "∉",
        "vdash" => "⊢",
        "dashv" => "⊣",
        "models" | "vDash" => "⊨",
        "Vdash" => "⊩",
        "Vvdash" => "⊪",
        "mid" | "shortmid" => "∣",
        "nmid" => "∤",
        "parallel" | "shortparallel" => "∥",
        "nparallel" => "∦",
        "perp" => "⊥",
        "smile" | "smallsmile" => "⌣",
        "frown" | "smallfrown" => "⌢",
        "bowtie" | "Join" => "⋈",
        "prec" => "≺",
        "succ" => "≻",
        "preceq" => "⪯",
        "succeq" => "⪰",
        "precsim" => "≾",
        "succsim" => "≿",
        "lesssim" => "≲",
        "gtrsim" => "≳",
        "lessgtr" => "≶",
        "gtrless" => "≷",
        "leqq" => "≦",
        "geqq" => "≧",
        "leqslant" => "⩽",
        "geqslant" => "⩾",
        "eqslantless" => "⪕",
        "eqslantgtr" => "⪖",
        "nless" => "≮",
        "ngtr" => "≯",
        "nleq" => "≰",
        "ngeq" => "≱",
        "nsim" => "≁",
        "ncong" => "≇",
        "nsubseteq" => "⊈",
        "nsupseteq" => "⊉",
        "nvdash" => "⊬",
        "nvDash" => "⊭",
        "therefore" => "∴",
        "because" => "∵",
        "approxeq" => "≊",
        "backsim" => "∽",
        "backsimeq" => "⋍",
        "eqsim" => "≂",
        "triangleq" => "≜",
        "circeq" => "≗",
        "bumpeq" => "≏",
        "Bumpeq" => "≎",
        "coloneqq" | "coloneq" => "≔",
        "eqqcolon" | "eqcolon" => "≕",
        "fallingdotseq" => "≒",
        "risingdotseq" => "≓",
        "lessdot" => "⋖",
        "gtrdot" => "⋗",
        "vartriangleleft" => "⊲",
        "vartriangleright" => "⊳",
        "trianglelefteq" => "⊴",
        "trianglerighteq" => "⊵",
        "ntriangleleft" => "⋪",
        "ntriangleright" => "⋫",
        "between" => "≬",
        "pitchfork" => "⋔",
        "multimap" => "⊸",
        "Diamond" => "◊",
        "lessapprox" => "⪅",
        "gtrapprox" => "⪆",
        "lesseqgtr" => "⋚",
        "gtreqless" => "⋛",
        "lesseqqgtr" => "⪋",
        "gtreqqless" => "⪌",
        "eqcirc" => "≖",
        "Subset" => "⋐",
        "Supset" => "⋑",
        "preccurlyeq" => "≼",
        "succcurlyeq" => "≽",
        "curlyeqprec" => "⋞",
        "curlyeqsucc" => "⋟",
        "precapprox" => "⪷",
        "succapprox" => "⪸",
        "backepsilon" => "∍",
        "blacktriangleleft" => "◂",
        "blacktriangleright" => "▸",
        "nleqslant" | "lneq" => "⪇",
        "ngeqslant" | "gneq" => "⪈",
        "nleqq" => "≰",
        "ngeqq" => "≱",
        "lneqq" | "lvertneqq" => "≨",
        "gneqq" | "gvertneqq" => "≩",
        "lnsim" => "⋦",
        "gnsim" => "⋧",
        "lnapprox" => "⪉",
        "gnapprox" => "⪊",
        "nprec" => "⊀",
        "nsucc" => "⊁",
        "npreceq" => "⋠",
        "nsucceq" => "⋡",
        "precneqq" => "⪵",
        "succneqq" => "⪶",
        "precnsim" => "⋨",
        "succnsim" => "⋩",
        "precnapprox" => "⪹",
        "succnapprox" => "⪺",
        "nshortmid" => "∤",
        "nshortparallel" => "∦",
        "nVdash" => "⊮",
        "nVDash" => "⊯",
        "ntrianglelefteq" => "⋬",
        "ntrianglerighteq" => "⋭",
        "nsubseteqq" => "⊈",
        "nsupseteqq" => "⊉",
        "varsubsetneq" => "⊊",
        "varsupsetneq" => "⊋",
        "varsubsetneqq" => "⫋",
        "varsupsetneqq" => "⫌",
        _ => return None,
    })
}

fn arrow(name: &str) -> Option<&'static str> {
    Some(match name {
        "to" | "rightarrow" => "→",
        "leftarrow" | "gets" => "←",
        "leftrightarrow" => "↔",
        "Rightarrow" => "⇒",
        "Leftarrow" => "⇐",
        "Leftrightarrow" => "⇔",
        "mapsto" => "↦",
        "longmapsto" => "⟼",
        "longrightarrow" => "⟶",
        "longleftarrow" => "⟵",
        "longleftrightarrow" => "⟷",
        "Longrightarrow" => "⟹",
        "Longleftarrow" => "⟸",
        "Longleftrightarrow" => "⟺",
        "uparrow" => "↑",
        "downarrow" => "↓",
        "updownarrow" => "↕",
        "Uparrow" => "⇑",
        "Downarrow" => "⇓",
        "Updownarrow" => "⇕",
        "nearrow" => "↗",
        "searrow" => "↘",
        "swarrow" => "↙",
        "nwarrow" => "↖",
        "hookleftarrow" => "↩",
        "hookrightarrow" => "↪",
        "leftharpoonup" => "↼",
        "leftharpoondown" => "↽",
        "rightharpoonup" => "⇀",
        "rightharpoondown" => "⇁",
        "rightleftharpoons" => "⇌",
        "leftrightharpoons" => "⇋",
        "rightsquigarrow" | "leadsto" => "⇝",
        "leftrightsquigarrow" => "↭",
        "twoheadrightarrow" => "↠",
        "twoheadleftarrow" => "↞",
        "rightarrowtail" => "↣",
        "leftarrowtail" => "↢",
        "looparrowright" => "↬",
        "looparrowleft" => "↫",
        "curvearrowright" => "↷",
        "curvearrowleft" => "↶",
        "circlearrowright" => "↻",
        "circlearrowleft" => "↺",
        "Rsh" => "↱",
        "Lsh" => "↰",
        "nrightarrow" => "↛",
        "nleftarrow" => "↚",
        "nRightarrow" => "⇏",
        "nLeftarrow" => "⇍",
        "nleftrightarrow" => "↮",
        "nLeftrightarrow" => "⇎",
        "rightrightarrows" => "⇉",
        "leftleftarrows" => "⇇",
        "upuparrows" => "⇈",
        "downdownarrows" => "⇊",
        "rightleftarrows" => "⇄",
        "leftrightarrows" => "⇆",
        "Rrightarrow" => "⇛",
        "Lleftarrow" => "⇚",
        "dashrightarrow" => "⇢",
        "dashleftarrow" => "⇠",
        "upharpoonleft" => "↿",
        "upharpoonright" | "restriction" => "↾",
        "downharpoonleft" => "⇃",
        "downharpoonright" => "⇂",
        _ => return None,
    })
}

fn bracket(name: &str) -> Option<(&'static str, Class)> {
    Some(match name {
        "{" | "lbrace" => ("{", Class::Open),
        "}" | "rbrace" => ("}", Class::Close),
        "lbrack" => ("[", Class::Open),
        "rbrack" => ("]", Class::Close),
        "langle" => ("⟨", Class::Open),
        "rangle" => ("⟩", Class::Close),
        "lfloor" => ("⌊", Class::Open),
        "rfloor" => ("⌋", Class::Close),
        "lceil" => ("⌈", Class::Open),
        "rceil" => ("⌉", Class::Close),
        "ulcorner" => ("⌜", Class::Open),
        "urcorner" => ("⌝", Class::Close),
        "llcorner" => ("⌞", Class::Open),
        "lrcorner" => ("⌟", Class::Close),
        "lgroup" => ("⟮", Class::Open),
        "rgroup" => ("⟯", Class::Close),
        "lmoustache" => ("⎰", Class::Open),
        "rmoustache" => ("⎱", Class::Close),
        "lvert" => ("|", Class::Open),
        "rvert" => ("|", Class::Close),
        "lVert" => ("‖", Class::Open),
        "rVert" => ("‖", Class::Close),
        "ldots" | "dotsc" | "dotso" => ("…", Class::Inner),
        "cdots" | "dotsb" | "dotsm" | "dotsi" => ("⋯", Class::Inner),
        "ddots" => ("⋱", Class::Inner),
        "iddots" => ("⋰", Class::Inner),
        "colon" => (":", Class::Punct),
        "ldotp" => (".", Class::Punct),
        "cdotp" => ("⋅", Class::Punct),
        _ => return None,
    })
}

/// The symbol and whether it is an integral, which keeps its scripts beside it even in display.
fn big_operator(name: &str) -> Option<(&'static str, bool)> {
    Some(match name {
        "sum" => ("∑", false),
        "prod" => ("∏", false),
        "coprod" => ("∐", false),
        "int" | "intop" | "smallint" => ("∫", true),
        "ointop" => ("∮", true),
        "iint" => ("∬", true),
        "iiint" => ("∭", true),
        "iiiint" => ("⨌", true),
        "oint" => ("∮", true),
        "oiint" => ("∯", true),
        "oiiint" => ("∰", true),
        "bigcup" => ("⋃", false),
        "bigcap" => ("⋂", false),
        "bigvee" => ("⋁", false),
        "bigwedge" => ("⋀", false),
        "bigoplus" => ("⨁", false),
        "bigotimes" => ("⨂", false),
        "bigodot" => ("⨀", false),
        "biguplus" => ("⨄", false),
        "bigsqcup" => ("⨆", false),
        _ => return None,
    })
}

/// The name and whether its scripts go over and under it in display.
fn named_operator(name: &str) -> Option<(&'static str, bool)> {
    Some(match name {
        "lim" => ("lim", true),
        "limsup" => ("lim\u{2009}sup", true),
        "liminf" => ("lim\u{2009}inf", true),
        "injlim" => ("inj\u{2009}lim", true),
        "projlim" => ("proj\u{2009}lim", true),
        "max" => ("max", true),
        "min" => ("min", true),
        "sup" => ("sup", true),
        "inf" => ("inf", true),
        "det" => ("det", true),
        "gcd" => ("gcd", true),
        "Pr" => ("Pr", true),
        "sin" => ("sin", false),
        "cos" => ("cos", false),
        "tan" => ("tan", false),
        "cot" => ("cot", false),
        "sec" => ("sec", false),
        "csc" => ("csc", false),
        "sinh" => ("sinh", false),
        "cosh" => ("cosh", false),
        "tanh" => ("tanh", false),
        "coth" => ("coth", false),
        "sech" => ("sech", false),
        "csch" => ("csch", false),
        "arcsin" => ("arcsin", false),
        "arccos" => ("arccos", false),
        "arctan" => ("arctan", false),
        "arccot" => ("arccot", false),
        "arcsec" => ("arcsec", false),
        "arccsc" => ("arccsc", false),
        "log" => ("log", false),
        "ln" => ("ln", false),
        "lg" => ("lg", false),
        "exp" => ("exp", false),
        "arg" => ("arg", false),
        "deg" => ("deg", false),
        "dim" => ("dim", false),
        "hom" => ("hom", false),
        "ker" => ("ker", false),
        "tr" => ("tr", false),
        "Tr" => ("Tr", false),
        "rank" => ("rank", false),
        "erf" => ("erf", false),
        "Res" => ("Res", false),
        _ => return None,
    })
}

fn font_command(name: &str) -> Option<MathFont> {
    Some(match name {
        "mathrm" | "mathup" => MathFont::Roman,
        "mathbf" | "mathbfup" => MathFont::Bold,
        "mathit" => MathFont::Italic,
        "mathnormal" => MathFont::Auto,
        "mathbfit" => MathFont::BoldItalic,
        "mathsf" | "mathsfup" => MathFont::Sans,
        "mathbfsf" | "mathbfsfup" | "symbfsf" | "symbfsfup" => MathFont::SansBold,
        "mathsfit" | "symsfit" => MathFont::SansItalic,
        "mathbfsfit" | "symbfsfit" => MathFont::SansBold,
        "mathtt" | "symtt" => MathFont::Mono,
        "mathcal" | "mathscr" | "symcal" | "symscr" => MathFont::Script,
        "mathbfcal" | "mathbfscr" | "symbfcal" | "symbfscr" => MathFont::BoldScript,
        "mathfrak" | "symfrak" => MathFont::Fraktur,
        "mathbffrak" | "symbffrak" => MathFont::BoldFraktur,
        "mathbb" | "symbb" => MathFont::DoubleStruck,
        "symrm" | "symup" => MathFont::Roman,
        "symnormal" => MathFont::Auto,
        "symbf" => MathFont::Bold,
        "symbfup" => MathFont::Bold,
        "symit" => MathFont::Italic,
        "symbfit" => MathFont::BoldItalic,
        "symsf" | "symsfup" => MathFont::Sans,
        _ => return None,
    })
}

/// `HandleSize`: MathJax's sizes, times its `P_HEIGHT`.
fn big_delimiter(name: &str) -> Option<(Em, Class)> {
    const SIZES: [Em; 4] = [1.2, 1.623, 2.047, 2.471];
    let (size, rest) = if let Some(rest) = name.strip_prefix("Bigg") {
        (3, rest)
    } else if let Some(rest) = name.strip_prefix("bigg") {
        (2, rest)
    } else if let Some(rest) = name.strip_prefix("Big") {
        (1, rest)
    } else {
        (0, name.strip_prefix("big")?)
    };
    let class = match rest {
        "" => Class::Ord,
        "l" => Class::Open,
        "r" => Class::Close,
        "m" => Class::Rel,
        _ => return None,
    };
    Some((SIZES[size], class))
}

fn fixed_space(name: &str) -> Option<Em> {
    const MU: Em = 1.0 / 18.0;
    Some(match name {
        "," | "thinspace" => 3.0 * MU,
        ":" | ">" => 4.0 * MU,
        ";" => 5.0 * MU,
        "!" | "negthinspace" => -3.0 * MU,
        "negmedspace" => -4.0 * MU,
        "negthickspace" => -5.0 * MU,
        // A backslash at the very end of the input is a space to MathJax's `GetCS` too.
        " " | "" | "nobreakspace" | "space" => 1.0 / 3.0,
        "enspace" => 0.5,
        "quad" => 1.0,
        "qquad" => 2.0,
        _ => return None,
    })
}

fn accent(name: &str) -> Option<(AccentKind, bool)> {
    Some(match name {
        "hat" => (AccentKind::Hat, false),
        "widehat" => (AccentKind::Hat, true),
        "check" => (AccentKind::Check, false),
        "tilde" => (AccentKind::Tilde, false),
        "widetilde" => (AccentKind::Tilde, true),
        "acute" => (AccentKind::Acute, false),
        "grave" => (AccentKind::Grave, false),
        "dot" => (AccentKind::Dot, false),
        "ddot" => (AccentKind::Ddot, false),
        "dddot" => (AccentKind::Dddot, false),
        "ddddot" => (AccentKind::Ddddot, false),
        "breve" => (AccentKind::Breve, false),
        "bar" => (AccentKind::Bar, false),
        "vec" => (AccentKind::Vec, false),
        "mathring" => (AccentKind::Ring, false),
        _ => return None,
    })
}

fn over_kind(name: &str) -> Option<OverKind> {
    Some(match name {
        "overline" => OverKind::Overline,
        "underline" => OverKind::Underline,
        "overbrace" => OverKind::Overbrace,
        "underbrace" => OverKind::Underbrace,
        "overrightarrow" => OverKind::OverArrow { left: false, right: true },
        "overleftarrow" => OverKind::OverArrow { left: true, right: false },
        "overleftrightarrow" => OverKind::OverArrow { left: true, right: true },
        "underrightarrow" => OverKind::UnderArrow { left: false, right: true },
        "underleftarrow" => OverKind::UnderArrow { left: true, right: false },
        "underleftrightarrow" => OverKind::UnderArrow { left: true, right: true },
        "overparen" => OverKind::OverParen,
        "underparen" => OverKind::UnderParen,
        _ => return None,
    })
}

fn x_arrow(name: &str) -> Option<XArrow> {
    let arrow = |left, right, head, pad: (Em, Em)| XArrow { left, right, head, pad };
    Some(match name {
        "xrightarrow" => arrow(false, true, ArrowHead::Single, (5.0, 10.0)),
        "xleftarrow" => arrow(true, false, ArrowHead::Single, (10.0, 5.0)),
        "xleftrightarrow" => arrow(true, true, ArrowHead::Single, (10.0, 10.0)),
        "xlongrightarrow" => arrow(false, true, ArrowHead::Single, (7.0, 12.0)),
        "xlongleftarrow" => arrow(true, false, ArrowHead::Single, (12.0, 7.0)),
        "xRightarrow" => arrow(false, true, ArrowHead::Double, (7.0, 12.0)),
        "xLeftarrow" => arrow(true, false, ArrowHead::Double, (12.0, 7.0)),
        "xLeftrightarrow" => arrow(true, true, ArrowHead::Double, (12.0, 12.0)),
        "xLongrightarrow" => arrow(false, true, ArrowHead::Double, (7.0, 12.0)),
        "xLongleftarrow" => arrow(true, false, ArrowHead::Double, (12.0, 7.0)),
        "xrightleftharpoons" | "xleftrightharpoons" => arrow(true, true, ArrowHead::Harpoons, (10.0, 10.0)),
        "xmapsto" => arrow(false, true, ArrowHead::Mapsto, (10.0, 10.0)),
        "xhookrightarrow" => arrow(false, true, ArrowHead::Hook, (5.0, 10.0)),
        "xhookleftarrow" => arrow(true, false, ArrowHead::Hook, (10.0, 5.0)),
        "xtwoheadrightarrow" => arrow(false, true, ArrowHead::TwoHead, (12.0, 16.0)),
        "xtwoheadleftarrow" => arrow(true, false, ArrowHead::TwoHead, (17.0, 13.0)),
        "xrightharpoonup" => arrow(false, true, ArrowHead::Harpoon(true), (5.0, 10.0)),
        "xrightharpoondown" => arrow(false, true, ArrowHead::Harpoon(false), (5.0, 10.0)),
        "xleftharpoonup" => arrow(true, false, ArrowHead::Harpoon(true), (10.0, 5.0)),
        "xleftharpoondown" => arrow(true, false, ArrowHead::Harpoon(false), (10.0, 5.0)),
        "xlongequal" => arrow(true, true, ArrowHead::Equal, (7.0, 7.0)),
        "xtofrom" => arrow(true, true, ArrowHead::ToFrom, (12.0, 12.0)),
        _ => return None,
    })
}

/// Commands that are other TeX: macros in MathJax too, the argument count first.
fn rewrite(name: &str) -> Option<(u8, &'static str)> {
    Some(match name {
        "implies" => (0, "\\;\\Longrightarrow\\;"),
        "impliedby" => (0, "\\;\\Longleftarrow\\;"),
        "iff" => (0, "\\;\\Longleftrightarrow\\;"),
        "pmod" => (1, "\\pod{{\\rm mod}\\mkern6mu #1}"),
        "pod" => (1, "\\mathchoice{\\kern18mu}{\\kern8mu}{\\kern8mu}{\\kern8mu}(#1)"),
        "mod" => (1, "\\mathchoice{\\kern18mu}{\\kern12mu}{\\kern12mu}{\\kern12mu}{\\rm mod}\\,\\,#1"),
        "skew" => (3, "{{#2{#3\\mkern#1mu}\\mkern-#1mu}{}}"),
        "pmb" => (1, "\\rlap{#1}\\kern1px{#1}"),
        "TeX" => (0, "T\\kern-.14em\\lower.5ex{E}\\kern-.115em X"),
        "LaTeX" => (0, "L\\kern-.325em\\raise.21em{\\scriptstyle{A}}\\kern-.17em\\TeX"),
        "varliminf" => (0, "\\mathop{\\underline{\\mathrm{lim}}}"),
        "varlimsup" => (0, "\\mathop{\\overline{\\mathrm{lim}}}"),
        "varinjlim" => (0, "\\mathop{\\underrightarrow{\\mathrm{lim}}}"),
        "varprojlim" => (0, "\\mathop{\\underleftarrow{\\mathrm{lim}}}"),
        _ => return None,
    })
}

/// A typed symbol's class, for the ones people paste as themselves.
fn unicode_class(c: char) -> Option<Class> {
    Some(match c {
        '±' | '∓' | '×' | '÷' | '∗' | '⋆' | '∘' | '∙' | '⋅' | '·' | '∩' | '∪' | '⊎' | '⊓' | '⊔' | '∨' | '∧' | '∖' | '⊕' | '⊖' | '⊗' | '⊘' | '⊙' | '†' | '‡' | '−' => Class::Bin,
        '≤' | '≥' | '≪' | '≫' | '≠' | '≡' | '∼' | '≃' | '≈' | '≅' | '≐' | '∝' | '⊂' | '⊃' | '⊆' | '⊇' | '∈' | '∋' | '∉' | '⊢' | '⊣' | '⊨' | '∣' | '∥' | '⊥' | '→' | '←' | '↔' | '⇒' | '⇐' | '⇔' | '↦' | '⟶' | '⟵' | '⟹' | '⟸' | '⟺' | '↑' | '↓' | '∴' | '∵' => Class::Rel,
        '⟨' | '⌊' | '⌈' | '（' | '【' | '《' => Class::Open,
        '⟩' | '⌋' | '⌉' | '）' | '】' | '》' => Class::Close,
        '，' | '；' | '、' => Class::Punct,
        '∑' | '∏' | '∫' | '∮' | '⋃' | '⋂' => Class::Op,
        _ => return None,
    })
}

/// A delimiter from what `\left` was followed by. A NUL is the invisible `.`.
fn lookup_delimiter(token: &str) -> Option<char> {
    Some(match token {
        "." | "" => '\0',
        "(" | "\\lparen" => '(',
        ")" | "\\rparen" => ')',
        "[" | "\\lbrack" => '[',
        "]" | "\\rbrack" => ']',
        "\\{" | "\\lbrace" => '{',
        "\\}" | "\\rbrace" => '}',
        "|" | "\\vert" | "\\lvert" | "\\rvert" | "\\mid" | "∣" => '|',
        "\\|" | "\\Vert" | "\\lVert" | "\\rVert" | "\\parallel" | "‖" | "∥" => '\u{2016}',
        "/" => '/',
        "\\backslash" | "\\setminus" => '\\',
        "<" | "\\lt" | "\\langle" | "⟨" | "〈" | "\\lang" => '⟨',
        ">" | "\\gt" | "\\rangle" | "⟩" | "〉" | "\\rang" => '⟩',
        "\\arrowvert" => '|',
        "\\Arrowvert" => '\u{2016}',
        "\\bracevert" => '|',
        "\\lfloor" | "⌊" => '⌊',
        "\\rfloor" | "⌋" => '⌋',
        "\\lceil" | "⌈" => '⌈',
        "\\rceil" | "⌉" => '⌉',
        "\\lgroup" | "⟮" => '(',
        "\\rgroup" | "⟯" => ')',
        "\\lmoustache" => '{',
        "\\rmoustache" => '}',
        "\\ulcorner" => '⌈',
        "\\urcorner" => '⌉',
        "\\llcorner" => '⌊',
        "\\lrcorner" => '⌋',
        "\\uparrow" | "↑" => '↑',
        "\\downarrow" | "↓" => '↓',
        "\\updownarrow" | "↕" => '↕',
        "\\Uparrow" | "⇑" => '⇑',
        "\\Downarrow" | "⇓" => '⇓',
        "\\Updownarrow" | "⇕" => '⇕',
        _ => return None,
    })
}

/// `\not=`: the symbol that already says so, where there is one. The rest get a slash through them.
fn negation(c: char) -> Option<char> {
    Some(match c {
        '=' => '≠',
        '≡' => '≢',
        '∈' => '∉',
        '∋' => '∌',
        '∼' => '≁',
        '≃' => '≄',
        '≅' => '≇',
        '≈' => '≉',
        '<' => '≮',
        '>' => '≯',
        '≤' => '≰',
        '≥' => '≱',
        '⊂' => '⊄',
        '⊃' => '⊅',
        '⊆' => '⊈',
        '⊇' => '⊉',
        '∃' => '∄',
        '∣' => '∤',
        '∥' => '∦',
        '⊢' => '⊬',
        '⊨' => '⊭',
        '→' => '↛',
        '←' => '↚',
        '⇒' => '⇏',
        '⇐' => '⇍',
        '↔' => '↮',
        '⇔' => '⇎',
        '≺' => '⊀',
        '≻' => '⊁',
        _ => return None,
    })
}

// ---------------------------------------------------------------- colours

fn hex_color(hex: &str) -> Option<Color> {
    let hex = hex.trim();
    if !hex.is_ascii() {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    match hex.len() {
        3 => {
            let (r, g, b) = ((value >> 8) & 15, (value >> 4) & 15, value & 15);
            Some(Color::rgb((r * 17) << 16 | (g * 17) << 8 | (b * 17)))
        }
        6 => Some(Color::rgb(value)),
        _ => None,
    }
}

impl Parser {
    /// `\color`'s answer, with what `\definecolor` taught it.
    fn color(&self, model: Option<&str>, spec: &str) -> Option<Color> {
        if model.is_none()
            && let Some(color) = self.defs.colors.get(spec.trim())
        {
            return Some(*color);
        }
        parse_color(model, spec)
    }
}

/// `\color[model]{spec}`: the `color` package's models, its names, and what CSS would take for the rest, which is where a name
/// it does not list ends up upstream. One nothing can read leaves the colour as it was.
pub(super) fn parse_color(model: Option<&str>, spec: &str) -> Option<Color> {
    let spec = spec.trim();
    let numbers = || spec.split(',').map(|n| n.trim().parse::<f32>().ok()).collect::<Option<Vec<f32>>>();
    let byte = |n: f32| (n.clamp(0.0, 1.0) * 255.0).round() as u32;
    match model.map(str::trim) {
        Some("rgb") => {
            let n = numbers()?;
            return (n.len() == 3).then(|| Color::rgb(byte(n[0]) << 16 | byte(n[1]) << 8 | byte(n[2])));
        }
        Some("RGB") => {
            let n = numbers()?;
            return (n.len() == 3).then(|| Color::rgb((n[0].clamp(0.0, 255.0) as u32) << 16 | (n[1].clamp(0.0, 255.0) as u32) << 8 | n[2].clamp(0.0, 255.0) as u32));
        }
        Some("gray") => {
            let n = numbers()?;
            return (n.len() == 1).then(|| Color::rgb(byte(n[0]) * 0x010101));
        }
        Some("HTML") => return hex_color(spec),
        _ => {}
    }
    if let Some(hex) = spec.strip_prefix('#') {
        return hex_color(hex);
    }
    named_color(spec).or_else(|| css_color(&spec.to_ascii_lowercase())).map(Color::rgb)
}

/// The `color` package's own table.
fn named_color(name: &str) -> Option<u32> {
    Some(match name {
        "Apricot" => 0xF8B878,
        "Aquamarine" => 0x00B5BE,
        "Bittersweet" => 0xC04F17,
        "Black" => 0x221E1F,
        "Blue" => 0x2D2F92,
        "BlueGreen" => 0x00B3B8,
        "BlueViolet" => 0x473992,
        "BrickRed" => 0xB6321C,
        "Brown" => 0x792500,
        "BurntOrange" => 0xF7921D,
        "CadetBlue" => 0x74729A,
        "CarnationPink" => 0xF282B4,
        "Cerulean" => 0x00A2E3,
        "CornflowerBlue" => 0x41B0E4,
        "Cyan" => 0x00AEEF,
        "Dandelion" => 0xFDBC42,
        "DarkOrchid" => 0xA4538A,
        "Emerald" => 0x00A99D,
        "ForestGreen" => 0x009B55,
        "Fuchsia" => 0x8C368C,
        "Goldenrod" => 0xFFDF42,
        "Gray" => 0x949698,
        "Green" => 0x00A64F,
        "GreenYellow" => 0xDFE674,
        "JungleGreen" => 0x00A99A,
        "Lavender" => 0xF49EC4,
        "LimeGreen" => 0x8DC73E,
        "Magenta" => 0xEC008C,
        "Mahogany" => 0xA9341F,
        "Maroon" => 0xAF3235,
        "Melon" => 0xF89E7B,
        "MidnightBlue" => 0x006795,
        "Mulberry" => 0xA93C93,
        "NavyBlue" => 0x006EB8,
        "OliveGreen" => 0x3C8031,
        "Orange" => 0xF58137,
        "OrangeRed" => 0xED135A,
        "Orchid" => 0xAF72B0,
        "Peach" => 0xF7965A,
        "Periwinkle" => 0x7977B8,
        "PineGreen" => 0x008B72,
        "Plum" => 0x92268F,
        "ProcessBlue" => 0x00B0F0,
        "Purple" => 0x99479B,
        "RawSienna" => 0x974006,
        "Red" => 0xED1B23,
        "RedOrange" => 0xF26035,
        "RedViolet" => 0xA1246B,
        "Rhodamine" => 0xEF559F,
        "RoyalBlue" => 0x0071BC,
        "RoyalPurple" => 0x613F99,
        "RubineRed" => 0xED017D,
        "Salmon" => 0xF69289,
        "SeaGreen" => 0x3FBC9D,
        "Sepia" => 0x671800,
        "SkyBlue" => 0x46C5DD,
        "SpringGreen" => 0xC6DC67,
        "Tan" => 0xDA9D76,
        "TealBlue" => 0x00AEB3,
        "Thistle" => 0xD883B7,
        "Turquoise" => 0x00B4CE,
        "Violet" => 0x58429B,
        "VioletRed" => 0xEF58A0,
        "White" => 0xFFFFFF,
        "WildStrawberry" => 0xEE2967,
        "Yellow" => 0xFFF200,
        "YellowGreen" => 0x98CC70,
        "YellowOrange" => 0xFAA21A,
        _ => return None,
    })
}

fn css_color(name: &str) -> Option<u32> {
    Some(match name {
        "red" => 0xFF0000,
        "green" => 0x008000,
        "blue" => 0x0000FF,
        "black" => 0x000000,
        "white" => 0xFFFFFF,
        "gray" | "grey" => 0x808080,
        "silver" => 0xC0C0C0,
        "orange" => 0xFFA500,
        "yellow" => 0xFFFF00,
        "purple" => 0x800080,
        "magenta" | "fuchsia" => 0xFF00FF,
        "cyan" | "aqua" => 0x00FFFF,
        "pink" => 0xFFC0CB,
        "brown" => 0xA52A2A,
        "teal" => 0x008080,
        "navy" => 0x000080,
        "lime" => 0x00FF00,
        "olive" => 0x808000,
        "maroon" => 0x800000,
        "violet" => 0xEE82EE,
        "indigo" => 0x4B0082,
        "gold" => 0xFFD700,
        "tomato" => 0xFF6347,
        "coral" => 0xFF7F50,
        "salmon" => 0xFA8072,
        "crimson" => 0xDC143C,
        "turquoise" => 0x40E0D0,
        "skyblue" => 0x87CEEB,
        "steelblue" => 0x4682B4,
        "royalblue" => 0x4169E1,
        "forestgreen" => 0x228B22,
        "seagreen" => 0x2E8B57,
        "darkgreen" => 0x006400,
        "darkblue" => 0x00008B,
        "darkred" => 0x8B0000,
        "lightblue" => 0xADD8E6,
        "lightgreen" => 0x90EE90,
        "lightgray" | "lightgrey" => 0xD3D3D3,
        "darkgray" | "darkgrey" => 0xA9A9A9,
        _ => return None,
    })
}
