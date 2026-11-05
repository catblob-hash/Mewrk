//! A line-at-a-time syntax colourer, for the diff and file views and for fenced code blocks.
//!
//! This colours lexical shapes (comments, strings, numbers, reserved words) plus a few that are just as local: a word
//! before `(` is a call, a capitalised word is a type. Nothing looks further than its line, so it is either right or
//! plainly off, and the only thing that crosses a newline is the one `u32` in `State`. All delimiters are ASCII, so it
//! scans bytes and token boundaries always fall between characters.

mod tables;

use fluent::{Color, Theme};
use tables::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Token {
    Plain,
    Keyword,
    String,
    Number,
    Comment,
    Function,
    Type,
    Meta,
    Tag,
    Attribute,
    Property,
    Variable,
}

/// What is still open at the end of a line. The default is nothing.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct State(pub u32);

const OPEN_COMMENT: u32 = 1;
const OPEN_STRING: u32 = 2;
const OPEN_TAG: u32 = 3;

impl State {
    fn open(kind: u32, index: usize) -> Self {
        State(kind << 8 | index as u32)
    }
    fn kind(self) -> u32 {
        self.0 >> 8
    }
    fn index(self) -> usize {
        (self.0 & 0xff) as usize
    }
}

/// A language, as `lang_of` found it.
#[derive(Clone, Copy)]
pub struct Lang(&'static Grammar);

impl PartialEq for Lang {
    fn eq(&self, other: &Self) -> bool {
        self.0.name == other.0.name
    }
}
impl Eq for Lang {}

impl std::fmt::Debug for Lang {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.name)
    }
}

/// Finds the language for a file name, a path, a bare extension or a fence name. Case does not matter.
pub fn lang_of(name_or_ext: &str) -> Option<Lang> {
    let name = name_or_ext.trim().rsplit(['/', '\\']).next()?.to_ascii_lowercase();
    if name.is_empty() {
        return None;
    }
    let find = |key: &str| NAMES.binary_search_by(|(k, _)| k.as_bytes().cmp(key.as_bytes())).ok().map(|i| Lang(NAMES[i].1));
    if let Some(lang) = find(&name) {
        return Some(lang);
    }
    // `.env.local` and `Dockerfile.dev`: the name, not the suffix, says what these are.
    if name.starts_with(".env.") {
        return Some(Lang(&SHELL));
    }
    if name.starts_with("dockerfile.") {
        return Some(Lang(&DOCKERFILE));
    }
    find(name.rsplit('.').next()?)
}

/// Readable in both themes: the hues are Mewrk's own code themes, tuned per scheme by hand.
pub fn color(token: Token, theme: &Theme) -> Color {
    // The code theme Appearance chose rides on the theme, so every code surface follows it.
    let p = &theme.code;
    match token {
        Token::Plain => if p.adaptive { theme.text_primary } else { p.foreground },
        Token::Keyword => p.keyword,
        Token::String => p.string,
        Token::Number => p.number,
        Token::Comment => p.comment,
        Token::Function => p.function,
        Token::Type => p.type_,
        Token::Meta => p.meta,
        Token::Tag => p.tag,
        Token::Attribute => p.attribute,
        Token::Property => p.property,
        Token::Variable => p.variable,
    }
}

/// Colours `text`, one line without its newline, continuing from `state` and leaving what it opened there. `out` is
/// replaced with `(byte offset where the token starts, kind)`, in order; a token runs to the next one or to the end.
pub fn line(lang: Lang, state: &mut State, text: &str, out: &mut Vec<(usize, Token)>) {
    out.clear();
    // Minified bundles put a megabyte on a line. Past this it is not worth colouring, and nobody is reading it.
    let mut end = text.len().min(MAX_SCAN);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut scan = Scan { g: lang.0, s: &text.as_bytes()[..end], text: &text[..end], i: 0, at: 0, out, in_tag: false, braces: 0 };
    *state = scan.run(*state);
    scan.close(end);
    if end < text.len() && scan.out.last().is_none_or(|t| t.1 != Token::Plain) {
        scan.out.push((end, Token::Plain));
    }
}

const MAX_SCAN: usize = 16 * 1024;

// ---------------------------------------------------------------- grammars

#[derive(Clone, Copy, PartialEq, Eq)]
enum Keys {
    None,
    Quoted,
    Colon,
    Equals,
    Css,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Whole {
    None,
    Diff,
    Markdown,
}

struct StringRule {
    open: &'static str,
    close: &'static str,
    escape: bool,
    multiline: bool,
}

const fn s(open: &'static str, close: &'static str, escape: bool, multiline: bool) -> StringRule {
    StringRule { open, close, escape, multiline }
}

struct Grammar {
    name: &'static str,
    line_comments: &'static [&'static str],
    /// Comment markers that only count as the first thing on a line: batch `REM`, `::`.
    leading_comments: &'static [&'static str],
    block_comments: &'static [(&'static str, &'static str)],
    /// Longer openers before their prefixes, or `'''` reads as an empty string and then prose.
    strings: &'static [StringRule],
    /// Sorted by bytes. Upper case when `case_insensitive`.
    keywords: &'static [&'static str],
    case_insensitive: bool,
    markup: bool,
    /// `<Tag` is markup only where an expression could start, so `a < b` and `Array<string>` stay what they are.
    jsx: bool,
    /// Characters that belong to a word here and nowhere else: `@font-face`, Ruby's `defined?`.
    word_extra: &'static str,
    calls: bool,
    types: bool,
    decorators: bool,
    /// What opens a variable reference: `$` in the shell, `%` in batch.
    sigils: &'static str,
    directive: &'static str,
    attribute_openers: &'static [&'static str],
    macros: bool,
    command_prefix: &'static str,
    hex_colors: bool,
    keys: Keys,
    /// `[section]` on a line of its own is a heading.
    sections: bool,
    whole: Whole,
    /// Rust's `'`: a char when it closes one character later, a lifetime otherwise.
    rust_quotes: bool,
}

const CODE: Grammar = Grammar {
    name: "",
    line_comments: &[],
    leading_comments: &[],
    block_comments: &[],
    strings: &[],
    keywords: &[],
    case_insensitive: false,
    markup: false,
    jsx: false,
    word_extra: "",
    calls: true,
    types: true,
    decorators: false,
    sigils: "",
    directive: "",
    attribute_openers: &[],
    macros: false,
    command_prefix: "",
    hex_colors: false,
    keys: Keys::None,
    sections: false,
    whole: Whole::None,
    rust_quotes: false,
};

/// Data and configuration: nothing in them is a call or a type.
const DATA: Grammar = Grammar { calls: false, types: false, ..CODE };

const SLASH: &[&str] = &["//"];
const SLASH_BLOCK: &[(&str, &str)] = &[("/*", "*/")];
const HASH: &[&str] = &["#"];
const DASHES: &[&str] = &["--"];

const QUOTES: &[StringRule] = &[s("\"", "\"", true, false), s("'", "'", true, false)];
const DOUBLE_QUOTE: &[StringRule] = &[s("\"", "\"", true, false)];
const TRIPLE_THEN_QUOTES: &[StringRule] = &[s("\"\"\"", "\"\"\"", true, true), s("'''", "'''", true, true), s("\"", "\"", true, false), s("'", "'", true, false)];
const JS_STRINGS: &[StringRule] = &[s("`", "`", true, true), s("\"", "\"", true, false), s("'", "'", true, false)];
const TRIPLE_ESC_THEN_QUOTES: &[StringRule] = &[s("\"\"\"", "\"\"\"", true, true), s("\"", "\"", true, false), s("'", "'", true, false)];
const TRIPLE_RAW_THEN_QUOTES: &[StringRule] = &[s("\"\"\"", "\"\"\"", false, true), s("\"", "\"", true, false), s("'", "'", true, false)];

const JAVASCRIPT_BASE: Grammar = Grammar { name: "javascript", line_comments: SLASH, block_comments: SLASH_BLOCK, keywords: JS_KEYWORDS, decorators: true, strings: JS_STRINGS, ..CODE };
static JAVASCRIPT: Grammar = JAVASCRIPT_BASE;
static TYPESCRIPT: Grammar = Grammar { name: "typescript", ..JAVASCRIPT_BASE };
static TSX: Grammar = Grammar { name: "tsx", jsx: true, ..JAVASCRIPT_BASE };
static JSX: Grammar = Grammar { name: "jsx", jsx: true, ..JAVASCRIPT_BASE };
static RUST: Grammar = Grammar {
    name: "rust",
    line_comments: SLASH,
    block_comments: SLASH_BLOCK,
    keywords: RUST_KEYWORDS,
    macros: true,
    attribute_openers: &["#![", "#["],
    strings: &[s("br#\"", "\"#", false, true), s("r#\"", "\"#", false, true), s("r\"", "\"", false, true), s("\"", "\"", true, true)],
    rust_quotes: true,
    ..CODE
};
static GO: Grammar = Grammar { name: "go", line_comments: SLASH, block_comments: SLASH_BLOCK, keywords: GO_KEYWORDS, strings: &[s("`", "`", false, true), s("\"", "\"", true, false), s("'", "'", true, false)], ..CODE };
static JAVA: Grammar = Grammar { name: "java", line_comments: SLASH, block_comments: SLASH_BLOCK, keywords: JAVA_KEYWORDS, decorators: true, strings: TRIPLE_ESC_THEN_QUOTES, ..CODE };
static KOTLIN: Grammar = Grammar { name: "kotlin", line_comments: SLASH, block_comments: SLASH_BLOCK, keywords: KOTLIN_KEYWORDS, decorators: true, strings: TRIPLE_RAW_THEN_QUOTES, ..CODE };
static SCALA: Grammar = Grammar { name: "scala", line_comments: SLASH, block_comments: SLASH_BLOCK, keywords: SCALA_KEYWORDS, decorators: true, strings: TRIPLE_RAW_THEN_QUOTES, ..CODE };
static GROOVY: Grammar = Grammar { name: "groovy", line_comments: SLASH, block_comments: SLASH_BLOCK, keywords: GROOVY_KEYWORDS, decorators: true, strings: TRIPLE_THEN_QUOTES, ..CODE };
static DART: Grammar = Grammar { name: "dart", line_comments: SLASH, block_comments: SLASH_BLOCK, keywords: DART_KEYWORDS, decorators: true, strings: TRIPLE_THEN_QUOTES, ..CODE };
static CSHARP: Grammar = Grammar { name: "csharp", line_comments: SLASH, block_comments: SLASH_BLOCK, keywords: CSHARP_KEYWORDS, directive: "#", strings: TRIPLE_RAW_THEN_QUOTES, ..CODE };
static SWIFT: Grammar = Grammar { name: "swift", line_comments: SLASH, block_comments: SLASH_BLOCK, keywords: SWIFT_KEYWORDS, decorators: true, directive: "#", strings: TRIPLE_ESC_THEN_QUOTES, ..CODE };
const C_BASE: Grammar = Grammar { name: "c", line_comments: SLASH, block_comments: SLASH_BLOCK, keywords: C_KEYWORDS, directive: "#", strings: QUOTES, ..CODE };
static C: Grammar = C_BASE;
static CPP: Grammar = Grammar { name: "cpp", ..C_BASE };
static OBJECTIVEC: Grammar = Grammar { name: "objectivec", keywords: OBJC_KEYWORDS, decorators: true, ..C_BASE };
static SHADER: Grammar = Grammar { name: "shader", keywords: SHADER_KEYWORDS, ..C_BASE };
static SOLIDITY: Grammar = Grammar { name: "solidity", line_comments: SLASH, block_comments: SLASH_BLOCK, keywords: SOLIDITY_KEYWORDS, strings: QUOTES, ..CODE };
static VERILOG: Grammar = Grammar { name: "verilog", line_comments: SLASH, block_comments: SLASH_BLOCK, keywords: VERILOG_KEYWORDS, directive: "`", strings: DOUBLE_QUOTE, ..CODE };
static ZIG: Grammar = Grammar { name: "zig", line_comments: SLASH, block_comments: SLASH_BLOCK, keywords: ZIG_KEYWORDS, decorators: true, strings: QUOTES, ..CODE };
static PHP: Grammar = Grammar { name: "php", line_comments: &["//", "#"], block_comments: SLASH_BLOCK, keywords: PHP_KEYWORDS, sigils: "$", strings: QUOTES, ..CODE };
static PYTHON: Grammar = Grammar { name: "python", line_comments: HASH, keywords: PYTHON_KEYWORDS, decorators: true, strings: TRIPLE_THEN_QUOTES, ..CODE };
static RUBY: Grammar = Grammar { name: "ruby", line_comments: HASH, block_comments: &[("=begin", "=end")], keywords: RUBY_KEYWORDS, word_extra: "?!", sigils: "@", strings: QUOTES, ..CODE };
static PERL: Grammar = Grammar { name: "perl", line_comments: HASH, keywords: PERL_KEYWORDS, types: false, sigils: "$@%", strings: QUOTES, ..CODE };
static LUA: Grammar = Grammar { name: "lua", line_comments: DASHES, block_comments: &[("--[[", "]]")], keywords: LUA_KEYWORDS, strings: &[s("[[", "]]", false, true), s("\"", "\"", true, false), s("'", "'", true, false)], ..CODE };
static R: Grammar = Grammar { name: "r", line_comments: HASH, keywords: R_KEYWORDS, types: false, strings: QUOTES, ..CODE };
static JULIA: Grammar = Grammar { name: "julia", line_comments: HASH, block_comments: &[("#=", "=#")], keywords: JULIA_KEYWORDS, decorators: true, strings: &[s("\"\"\"", "\"\"\"", true, true), s("\"", "\"", true, false)], ..CODE };
static HASKELL: Grammar = Grammar { name: "haskell", line_comments: DASHES, block_comments: &[("{-", "-}")], keywords: HASKELL_KEYWORDS, calls: false, strings: DOUBLE_QUOTE, ..CODE };
static ELM: Grammar = Grammar { name: "elm", line_comments: DASHES, block_comments: &[("{-", "-}")], keywords: ELM_KEYWORDS, calls: false, strings: &[s("\"\"\"", "\"\"\"", true, true), s("\"", "\"", true, false)], ..CODE };
static OCAML: Grammar = Grammar { name: "ocaml", line_comments: SLASH, block_comments: &[("(*", "*)")], keywords: ML_KEYWORDS, calls: false, strings: DOUBLE_QUOTE, ..CODE };
static ELIXIR: Grammar = Grammar { name: "elixir", line_comments: HASH, keywords: ELIXIR_KEYWORDS, decorators: true, word_extra: "?!", strings: TRIPLE_THEN_QUOTES, ..CODE };
static ERLANG: Grammar = Grammar { name: "erlang", line_comments: &["%"], keywords: ERLANG_KEYWORDS, strings: DOUBLE_QUOTE, ..CODE };
static LISP: Grammar = Grammar { name: "lisp", line_comments: &[";"], block_comments: &[("#|", "|#")], keywords: LISP_KEYWORDS, calls: false, types: false, word_extra: "-!?*<>=/+", strings: DOUBLE_QUOTE, ..CODE };
static NIM: Grammar = Grammar { name: "nim", line_comments: HASH, block_comments: &[("#[", "]#")], keywords: NIM_KEYWORDS, strings: TRIPLE_RAW_THEN_QUOTES, ..CODE };
static GLEAM: Grammar = Grammar { name: "gleam", line_comments: SLASH, block_comments: SLASH_BLOCK, keywords: GLEAM_KEYWORDS, decorators: true, strings: DOUBLE_QUOTE, ..CODE };
static TCL: Grammar = Grammar { name: "tcl", line_comments: HASH, keywords: TCL_KEYWORDS, types: false, calls: false, sigils: "$", strings: DOUBLE_QUOTE, ..CODE };
// A single-quoted shell string takes no escapes at all: `'\'` is a backslash.
static SHELL: Grammar = Grammar { name: "shell", line_comments: HASH, keywords: SHELL_KEYWORDS, sigils: "$", strings: &[s("\"", "\"", true, false), s("'", "'", false, false)], ..DATA };
static MAKEFILE: Grammar = Grammar { name: "makefile", line_comments: HASH, keywords: MAKEFILE_KEYWORDS, sigils: "$", keys: Keys::Colon, word_extra: ".", strings: QUOTES, ..DATA };
static DOCKERFILE: Grammar = Grammar { name: "dockerfile", line_comments: HASH, keywords: DOCKER_KEYWORDS, case_insensitive: true, sigils: "$", strings: QUOTES, ..DATA };
static CMAKE: Grammar = Grammar { name: "cmake", line_comments: HASH, keywords: CMAKE_KEYWORDS, case_insensitive: true, sigils: "$", strings: DOUBLE_QUOTE, ..DATA };
// `Get-ChildItem` and `-eq` are single words there, which is why only this grammar lets a hyphen into one.
static POWERSHELL: Grammar = Grammar {
    name: "powershell",
    line_comments: HASH,
    block_comments: &[("<#", "#>")],
    keywords: POWERSHELL_KEYWORDS,
    case_insensitive: true,
    types: false,
    word_extra: "-",
    sigils: "$",
    strings: &[s("@\"", "\"@", false, true), s("\"", "\"", false, false), s("'", "'", false, false)],
    ..CODE
};
static BATCH: Grammar = Grammar { name: "batch", leading_comments: &["REM ", "REM\t", "::", "@REM "], keywords: BATCH_KEYWORDS, case_insensitive: true, sigils: "%", strings: &[s("\"", "\"", false, false)], ..DATA };
static VB: Grammar = Grammar { name: "vb", line_comments: &["'"], leading_comments: &["REM "], keywords: VB_KEYWORDS, case_insensitive: true, types: false, directive: "#", strings: &[s("\"", "\"", false, false)], ..CODE };
static FORTRAN: Grammar = Grammar { name: "fortran", line_comments: &["!"], keywords: FORTRAN_KEYWORDS, case_insensitive: true, types: false, strings: QUOTES, ..CODE };
static VHDL: Grammar = Grammar { name: "vhdl", line_comments: DASHES, block_comments: SLASH_BLOCK, keywords: VHDL_KEYWORDS, case_insensitive: true, types: false, strings: DOUBLE_QUOTE, ..CODE };
static NIX: Grammar = Grammar { name: "nix", line_comments: HASH, block_comments: SLASH_BLOCK, keywords: NIX_KEYWORDS, keys: Keys::Equals, strings: &[s("''", "''", false, true), s("\"", "\"", true, false)], ..DATA };
static HCL: Grammar = Grammar { name: "hcl", line_comments: &["#", "//"], block_comments: SLASH_BLOCK, keywords: HCL_KEYWORDS, keys: Keys::Equals, calls: true, sigils: "$", strings: DOUBLE_QUOTE, ..DATA };
static LATEX: Grammar = Grammar { name: "latex", line_comments: &["%"], command_prefix: "\\", strings: &[s("$$", "$$", true, true), s("$", "$", true, false)], ..DATA };
static ASSEMBLY: Grammar = Grammar { name: "assembly", line_comments: &[";", "#", "//"], block_comments: SLASH_BLOCK, keys: Keys::Colon, word_extra: ".", strings: QUOTES, ..DATA };
static SQL: Grammar = Grammar { name: "sql", line_comments: DASHES, block_comments: SLASH_BLOCK, keywords: SQL_KEYWORDS, case_insensitive: true, calls: true, strings: &[s("'", "'", false, false), s("\"", "\"", false, false)], ..DATA };
static CSS: Grammar = Grammar { name: "css", block_comments: SLASH_BLOCK, keywords: CSS_KEYWORDS, calls: true, word_extra: "@-!", keys: Keys::Css, hex_colors: true, strings: QUOTES, ..DATA };
static SCSS: Grammar = Grammar { name: "scss", line_comments: SLASH, block_comments: SLASH_BLOCK, keywords: CSS_KEYWORDS, calls: true, word_extra: "@-!", keys: Keys::Css, hex_colors: true, sigils: "$", strings: QUOTES, ..DATA };
static JSON: Grammar = Grammar { name: "json", line_comments: SLASH, block_comments: SLASH_BLOCK, keywords: JSON_KEYWORDS, keys: Keys::Quoted, strings: QUOTES, ..DATA };
static YAML: Grammar = Grammar { name: "yaml", line_comments: HASH, keywords: YAML_KEYWORDS, keys: Keys::Colon, strings: QUOTES, ..DATA };
static TOML: Grammar = Grammar { name: "toml", line_comments: HASH, keywords: TOML_KEYWORDS, keys: Keys::Equals, sections: true, strings: &[s("\"\"\"", "\"\"\"", true, true), s("'''", "'''", false, true), s("\"", "\"", true, false), s("'", "'", true, false)], ..DATA };
static INI: Grammar = Grammar { name: "ini", line_comments: &["#", ";"], keywords: INI_KEYWORDS, keys: Keys::Equals, sections: true, strings: QUOTES, ..DATA };
static XML: Grammar = Grammar { name: "xml", block_comments: &[("<!--", "-->")], markup: true, strings: QUOTES, ..DATA };
static GRAPHQL: Grammar = Grammar { name: "graphql", line_comments: &["#", "//"], block_comments: SLASH_BLOCK, keywords: GRAPHQL_KEYWORDS, calls: false, decorators: true, strings: TRIPLE_ESC_THEN_QUOTES, ..CODE };
static DIFF: Grammar = Grammar { name: "diff", whole: Whole::Diff, ..DATA };
static MARKDOWN: Grammar = Grammar { name: "markdown", block_comments: &[("<!--", "-->")], whole: Whole::Markdown, strings: &[s("```", "```", false, true), s("~~~", "~~~", false, true), s("`", "`", false, false)], ..DATA };
static LOG: Grammar = Grammar { name: "log", keywords: LOG_LEVELS, case_insensitive: true, strings: DOUBLE_QUOTE, ..DATA };

// ---------------------------------------------------------------- the scanner

struct Scan<'a> {
    g: &'static Grammar,
    s: &'a [u8],
    text: &'a str,
    i: usize,
    /// Where the last coloured token ended. Whatever lies between here and the next one is plain.
    at: usize,
    out: &'a mut Vec<(usize, Token)>,
    in_tag: bool,
    /// Braces opened inside a JSX tag: `onClick={() => x}` holds a `>` that does not close the tag.
    braces: i32,
}

fn find_close(s: &[u8], from: usize, close: &[u8], escapes: bool) -> Option<usize> {
    let mut i = from;
    while i < s.len() {
        if escapes && s[i] == b'\\' {
            i += 2;
            continue;
        }
        if s[i..].starts_with(close) {
            return Some(i + close.len());
        }
        i += 1;
    }
    None
}

fn is_blank(b: u8) -> bool {
    matches!(b, b' ' | b'\t')
}

fn skip_blank(s: &[u8], mut i: usize) -> usize {
    while i < s.len() && s[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// `HashMap`, `Promise`, `T`: a capital followed by anything that is not all capitals.
fn is_type_name(word: &[u8]) -> bool {
    word[0].is_ascii_uppercase() && (word.len() == 1 || word.iter().any(u8::is_ascii_lowercase))
}

fn is_keyword(g: &Grammar, word: &[u8]) -> bool {
    let search = |key: &[u8]| g.keywords.binary_search_by(|k| k.as_bytes().cmp(key)).is_ok();
    if !g.case_insensitive {
        return search(word);
    }
    let mut upper = [0u8; 40];
    if word.len() > upper.len() {
        return false;
    }
    upper[..word.len()].copy_from_slice(word);
    upper[..word.len()].make_ascii_uppercase();
    search(&upper[..word.len()])
}

impl Scan<'_> {
    fn is_word(&self, b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || self.g.word_extra.as_bytes().contains(&b)
    }

    fn is_word_start(&self, b: u8) -> bool {
        b.is_ascii_alphabetic() || b == b'_' || b == b'$' || self.g.word_extra.as_bytes().contains(&b)
    }

    fn mark(&mut self, at: usize, kind: Token) {
        if self.out.last().is_none_or(|t| t.1 != kind) {
            self.out.push((at, kind));
        }
    }

    fn push(&mut self, kind: Token, from: usize, to: usize) {
        if from > self.at {
            self.mark(self.at, Token::Plain);
        }
        self.mark(from, kind);
        self.at = to;
    }

    fn close(&mut self, end: usize) {
        if end > self.at {
            self.mark(self.at, Token::Plain);
            self.at = end;
        }
    }

    fn run(&mut self, state: State) -> State {
        let g = self.g;
        match state.kind() {
            OPEN_TAG => self.in_tag = true,
            OPEN_COMMENT | OPEN_STRING => {
                let string = state.kind() == OPEN_STRING;
                let rule = if string {
                    g.strings.get(state.index()).map(|r| (r.close, r.escape))
                } else {
                    g.block_comments.get(state.index()).map(|r| (r.1, false))
                };
                // A state this grammar never made opens nothing.
                if let Some((close, escape)) = rule {
                    let kind = if string { Token::String } else { Token::Comment };
                    match find_close(self.s, 0, close.as_bytes(), escape) {
                        None => {
                            if !self.s.is_empty() {
                                self.push(kind, 0, self.s.len());
                            }
                            return state;
                        }
                        Some(end) => {
                            self.push(kind, 0, end);
                            self.i = end;
                        }
                    }
                }
            }
            _ => {
                if self.starts_the_line() {
                    return State::default();
                }
            }
        }
        self.scan_rest()
    }

    /// Whatever only the start of a line can decide. True when that was the whole line.
    fn starts_the_line(&mut self) -> bool {
        let (g, s) = (self.g, self.s);
        // `#!/usr/bin/env node` says how to run the file, whatever the file is in.
        if s.starts_with(b"#!/") {
            self.push(Token::Meta, 0, s.len());
            return true;
        }
        match g.whole {
            Whole::Diff => return self.diff_line(),
            Whole::Markdown if self.markdown_line() => return true,
            _ => {}
        }
        let indent = s.iter().position(|b| !is_blank(*b)).unwrap_or(s.len());
        let lead = &s[indent..];
        if g.leading_comments.iter().any(|p| {
            let p = p.as_bytes();
            let bare = p.trim_ascii_end();
            (lead.len() >= p.len() && lead[..p.len()].eq_ignore_ascii_case(p)) || lead.eq_ignore_ascii_case(bare)
        }) {
            self.push(Token::Comment, indent, s.len());
            return true;
        }
        if g.sections && lead.first() == Some(&b'[') && let Some(end) = section_end(lead) {
            self.push(Token::Type, indent, indent + end);
            self.i = indent + end;
        } else if !g.directive.is_empty() && lead.starts_with(g.directive.as_bytes()) {
            let mut c = indent + 1;
            c = skip_blank(s, c);
            let name = c;
            while c < s.len() && (s[c].is_ascii_alphanumeric() || s[c] == b'_') {
                c += 1;
            }
            if c > name && !s[name].is_ascii_digit() {
                self.push(Token::Meta, indent, c);
                self.i = c;
                // `#include <stdio.h>`: the bracketed name is the directive's argument.
                let open = skip_blank(s, c);
                if s.get(open) == Some(&b'<') && let Some(close) = s[open..].iter().position(|b| *b == b'>') {
                    self.push(Token::String, open, open + close + 1);
                    self.i = open + close + 1;
                }
            }
        } else if g.whole == Whole::Markdown {
            let mut c = skip_blank(s, 0);
            let start = c;
            if matches!(s.get(c), Some(b'-' | b'*' | b'+')) {
                c += 1;
            } else {
                while c < s.len() && c - start < 9 && s[c].is_ascii_digit() {
                    c += 1;
                }
                if c > start && matches!(s.get(c), Some(b'.' | b')')) {
                    c += 1;
                } else {
                    c = start;
                }
            }
            if c > start && s.get(c).is_some_and(|b| b.is_ascii_whitespace()) {
                self.push(Token::Meta, start, c);
                self.i = c;
            }
        } else if let Some((from, to)) = leading_key(g.keys, s) {
            self.push(Token::Property, from, to);
            self.i = to;
        }
        false
    }

    fn diff_line(&mut self) -> bool {
        const HEADERS: [&[u8]; 10] = [b"diff ", b"index ", b"+++ ", b"--- ", b"new file mode", b"deleted file mode", b"similarity index", b"rename from ", b"rename to ", b"Binary files"];
        let s = self.s;
        if s.is_empty() {
            return true;
        }
        if HEADERS.iter().any(|h| s.starts_with(h)) {
            self.push(Token::Meta, 0, s.len());
        } else if s.starts_with(b"@@") {
            let end = s[2..].windows(2).position(|w| w == b"@@").map_or(s.len(), |p| p + 4);
            self.push(Token::Meta, 0, end);
        } else if s[0] == b'+' {
            self.push(Token::String, 0, s.len());
        } else if s[0] == b'-' {
            self.push(Token::Tag, 0, s.len());
        }
        true
    }

    fn markdown_line(&mut self) -> bool {
        let s = self.s;
        let indent = s.iter().take(4).position(|b| *b != b' ').unwrap_or(s.len().min(4));
        if indent > 3 {
            return false;
        }
        let lead = &s[indent..];
        let hashes = lead.iter().position(|b| *b != b'#').unwrap_or(lead.len());
        if (1..=6).contains(&hashes) && lead.get(hashes).is_none_or(|b| b.is_ascii_whitespace()) {
            self.push(Token::Keyword, 0, s.len());
            return true;
        }
        if lead.first() == Some(&b'>') {
            self.push(Token::Comment, 0, s.len());
            return true;
        }
        if let Some(mark @ (b'-' | b'*' | b'_')) = lead.first().copied() {
            let solid = lead.iter().all(|b| *b == mark || b.is_ascii_whitespace());
            if solid && lead.iter().filter(|b| **b == mark).count() >= 3 {
                self.push(Token::Meta, 0, s.len());
                return true;
            }
        }
        false
    }

    /// Where JSX may open a tag: after what ends a statement or starts an expression, never after an identifier or a
    /// closing bracket, which is where `<` compares or opens a type argument list.
    fn jsx_may_open(&self, at: usize) -> bool {
        let before = self.s[..at].trim_ascii_end();
        match before.last() {
            None => true,
            Some(b) if b"([{,;:=?!&|>".contains(b) => true,
            _ => before.ends_with(b"return") && before.len().checked_sub(7).is_none_or(|i| !self.is_word(before[i])),
        }
    }

    fn scan_rest(&mut self) -> State {
        let (g, s) = (self.g, self.s);
        while self.i < s.len() {
            let i = self.i;
            let c = s[i];

            if self.in_tag && g.jsx && (c == b'{' || c == b'}') {
                self.braces = (self.braces + if c == b'{' { 1 } else { -1 }).max(0);
            }
            if self.in_tag && self.braces == 0 && (c == b'>' || s[i..].starts_with(b"/>")) {
                let end = i + if c == b'>' { 1 } else { 2 };
                self.push(Token::Tag, i, end);
                self.i = end;
                self.in_tag = false;
                continue;
            }

            // Before line comments: Lua's `--[[` opens with its line comment's marker.
            if let Some(n) = g.block_comments.iter().position(|(open, _)| s[i..].starts_with(open.as_bytes())) {
                let (open, close) = g.block_comments[n];
                match find_close(s, i + open.len(), close.as_bytes(), false) {
                    None => {
                        self.push(Token::Comment, i, s.len());
                        return State::open(OPEN_COMMENT, n);
                    }
                    Some(end) => {
                        self.push(Token::Comment, i, end);
                        self.i = end;
                        continue;
                    }
                }
            }

            if let Some(marker) = g.line_comments.iter().find(|m| s[i..].starts_with(m.as_bytes())) {
                // A `#` in the middle of a word (`C#`, a URL fragment) is no comment in the shell.
                let in_word = *marker == "#" && i > 0 && self.is_word(s[i - 1]) && g.sigils.contains('$');
                if !in_word {
                    self.push(Token::Comment, i, s.len());
                    return State::default();
                }
            }

            if let Some(opener) = g.attribute_openers.iter().find(|o| s[i..].starts_with(o.as_bytes())) {
                let mut depth = 1;
                let mut end = s.len();
                for (n, b) in s.iter().enumerate().skip(i + opener.len()) {
                    match b {
                        b'[' => depth += 1,
                        b']' => {
                            depth -= 1;
                            if depth == 0 {
                                end = n + 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                self.push(Token::Meta, i, end);
                self.i = end;
                continue;
            }

            if g.rust_quotes && c == b'\'' {
                let end = self.rust_quote(i);
                let kind = if s[end - 1] == b'\'' && end > i + 1 { Token::String } else { Token::Variable };
                self.push(kind, i, end);
                self.i = end;
                continue;
            }

            if let Some(n) = g.strings.iter().position(|r| s[i..].starts_with(r.open.as_bytes())) {
                let rule = &g.strings[n];
                match find_close(s, i + rule.open.len(), rule.close.as_bytes(), rule.escape) {
                    None => {
                        self.push(Token::String, i, s.len());
                        // An unterminated one-line literal is a typo: the next line starts clean rather than colouring the
                        // rest of the file.
                        return if rule.multiline { State::open(OPEN_STRING, n) } else { State::default() };
                    }
                    Some(end) => {
                        // `"key": value`: the string before a colon is the key it names.
                        let key = g.keys == Keys::Quoted && s.get(skip_blank(s, end)) == Some(&b':');
                        self.push(if key { Token::Property } else { Token::String }, i, end);
                        self.i = end;
                        continue;
                    }
                }
            }

            if c == b'<' && (g.markup || (g.jsx && (self.jsx_may_open(i) || (s.get(i + 1) == Some(&b'/') && s.get(i + 2).is_some_and(u8::is_ascii_alphabetic))))) {
                let mut cur = i + 1;
                if s.get(cur) == Some(&b'/') {
                    cur += 1;
                }
                let start = cur;
                while cur < s.len() && (s[cur].is_ascii_alphanumeric() || b"_:.!?-".contains(&s[cur])) {
                    cur += 1;
                }
                // `<>` and `</>` are JSX fragments; everything else needs a name.
                let fragment = g.jsx && s.get(cur) == Some(&b'>') && cur == start;
                if (cur > start && (s[start].is_ascii_alphabetic() || s[start] == b'!' || s[start] == b'?')) || fragment {
                    self.push(Token::Tag, i, cur);
                    self.i = cur;
                    self.in_tag = !fragment;
                    self.braces = 0;
                    continue;
                }
            }

            if g.sigils.as_bytes().contains(&c) {
                let cur = self.sigil_end(i, c);
                if cur > i + 1 {
                    self.push(Token::Variable, i, cur);
                    self.i = cur;
                    continue;
                }
            }

            if !g.command_prefix.is_empty() && s[i..].starts_with(g.command_prefix.as_bytes()) {
                let mut cur = i + g.command_prefix.len();
                if s.get(cur).is_some_and(|b| b.is_ascii_alphabetic() || *b == b'@') {
                    while cur < s.len() && (s[cur].is_ascii_alphabetic() || s[cur] == b'@') {
                        cur += 1;
                    }
                } else if cur < s.len() {
                    cur += 1;
                    while !self.text.is_char_boundary(cur) {
                        cur += 1;
                    }
                }
                self.push(Token::Keyword, i, cur);
                self.i = cur;
                continue;
            }

            if g.decorators && c == b'@' && s.get(i + 1).is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_') {
                let mut cur = i + 1;
                while cur < s.len() && (s[cur].is_ascii_alphanumeric() || s[cur] == b'_' || s[cur] == b'.') {
                    cur += 1;
                }
                self.push(Token::Meta, i, cur);
                self.i = cur;
                continue;
            }

            if g.hex_colors && c == b'#' {
                let digits = s[i + 1..].iter().take_while(|b| b.is_ascii_hexdigit()).count();
                let boundary = s.get(i + 1 + digits).is_none_or(|b| !b.is_ascii_alphanumeric() && *b != b'_');
                if (3..=8).contains(&digits) && boundary {
                    self.push(Token::Number, i, i + 1 + digits);
                    self.i = i + 1 + digits;
                    continue;
                }
            }

            let word_before = i > 0 && self.is_word(s[i - 1]);
            if c.is_ascii_digit() && !word_before {
                let mut cur = i;
                while cur < s.len() && (s[cur].is_ascii_hexdigit() || b"xXoO_.".contains(&s[cur])) {
                    cur += 1;
                }
                self.push(Token::Number, i, cur);
                self.i = cur;
                continue;
            }

            if self.is_word_start(c) && !word_before {
                self.word(i);
                continue;
            }

            self.i += 1;
        }
        if self.in_tag { State::open(OPEN_TAG, 0) } else { State::default() }
    }

    /// `'x'` and `'\n'` close themselves; anything else is a lifetime and ends with its name.
    fn rust_quote(&self, i: usize) -> usize {
        let s = self.s;
        if s.get(i + 1) == Some(&b'\\') {
            return find_close(s, i + 1, b"'", true).unwrap_or(s.len());
        }
        let next = self.text[i + 1..].chars().next().map_or(0, char::len_utf8);
        if next > 0 && s.get(i + 1 + next) == Some(&b'\'') {
            return i + 2 + next;
        }
        let mut cur = i + 1;
        while cur < s.len() && (s[cur].is_ascii_alphanumeric() || s[cur] == b'_') {
            cur += 1;
        }
        cur
    }

    fn sigil_end(&self, i: usize, c: u8) -> usize {
        let (g, s) = (self.g, self.s);
        let mut cur = i + 1;
        let next = s.get(cur).copied().unwrap_or(0);
        if next == b'{' || (next == b'(' && g.keys == Keys::Colon) {
            let close = if next == b'{' { b'}' } else { b')' };
            cur = s[cur..].iter().position(|b| *b == close).map_or(s.len(), |p| cur + p + 1);
        } else if next.is_ascii_alphabetic() || next == b'_' {
            while cur < s.len() && (s[cur].is_ascii_alphanumeric() || s[cur] == b'_') {
                cur += 1;
            }
            // `%PATH%` closes itself.
            if c == b'%' && s.get(cur) == Some(&b'%') {
                cur += 1;
            }
        } else if c != b'%' && (next.is_ascii_digit() || b"@*#?$!-".contains(&next)) {
            cur += 1;
        } else if c == b'%' && next == b'%' && s.get(cur + 1).is_some_and(u8::is_ascii_alphabetic) {
            cur += 2;
        } else if c == b'%' && (next.is_ascii_digit() || next == b'*') {
            cur += 1;
        }
        cur
    }

    fn word(&mut self, i: usize) {
        let (g, s) = (self.g, self.s);
        let mut cur = i;
        while cur < s.len() && self.is_word(s[cur]) {
            cur += 1;
        }
        let word = &s[i..cur];
        let next = s.get(cur).copied();
        let after = skip_blank(s, cur);
        let assign = s.get(after) == Some(&b'=') && s.get(after + 1) != Some(&b'=');
        if self.in_tag && self.braces == 0 && assign {
            self.push(Token::Attribute, i, cur);
        } else if is_keyword(g, word) {
            self.push(Token::Keyword, i, cur);
        } else if g.macros && next == Some(b'!') && s.get(cur + 1).is_some_and(|b| b"([{".contains(b)) {
            cur += 1;
            self.push(Token::Function, i, cur);
        } else if g.calls && next == Some(b'(') {
            self.push(Token::Function, i, cur);
        } else if g.types && is_type_name(word) {
            self.push(Token::Type, i, cur);
        } else if self.in_tag && self.braces == 0 && g.jsx && i > 0 && s[i - 1].is_ascii_whitespace() {
            // A bare attribute in JSX, `<input disabled />`, has no `=` to go by.
            self.push(Token::Attribute, i, cur);
        }
        self.i = cur;
    }
}

/// The end of `[section]` or `[[section]]`, when the rest of the line holds nothing but a comment.
fn section_end(lead: &[u8]) -> Option<usize> {
    let close = lead.iter().position(|b| *b == b']')?;
    let end = if lead.get(close + 1) == Some(&b']') { close + 2 } else { close + 1 };
    let rest = skip_blank(lead, end);
    (rest == lead.len() || matches!(lead[rest], b'#' | b';')).then_some(end)
}

/// The key a line of configuration opens with. Only the start of a line is looked at: that is where a key is written,
/// and a colon or an equals sign further along is a value's business.
fn leading_key(keys: Keys, s: &[u8]) -> Option<(usize, usize)> {
    let quoted = |from: usize| -> Option<usize> {
        let q = *s.get(from)?;
        if q != b'"' && q != b'\'' {
            return None;
        }
        s[from + 1..].iter().position(|b| *b == q).map(|p| from + p + 2)
    };
    match keys {
        Keys::None | Keys::Quoted => None,
        Keys::Colon => {
            let mut from = skip_blank(s, 0);
            if s.get(from) == Some(&b'-') && s.get(from + 1).is_some_and(u8::is_ascii_whitespace) {
                from = skip_blank(s, from + 1);
            }
            let first = *s.get(from)?;
            if let Some(end) = quoted(from) {
                let colon = skip_blank(s, end);
                return (s.get(colon) == Some(&b':') && s.get(colon + 1).is_none_or(u8::is_ascii_whitespace)).then_some((from, end));
            }
            if first.is_ascii_whitespace() || b"#:\"'{}[],&*!|>%@`".contains(&first) {
                return None;
            }
            let colon = from + s[from..].iter().position(|b| *b == b':' || *b == b'#')?;
            if s[colon] != b':' || !s.get(colon + 1).is_none_or(u8::is_ascii_whitespace) {
                return None;
            }
            Some((from, from + s[from..colon].trim_ascii_end().len()))
        }
        Keys::Equals => {
            let from = skip_blank(s, 0);
            let end = quoted(from).or_else(|| {
                let n = s[from..].iter().take_while(|b| b.is_ascii_alphanumeric() || b"_.-".contains(b)).count();
                (n > 0).then_some(from + n)
            })?;
            (s.get(skip_blank(s, end)) == Some(&b'=')).then_some((from, end))
        }
        Keys::Css => {
            // A declaration, not a selector: nothing on the line opens a block.
            if s.contains(&b'{') {
                return None;
            }
            let from = skip_blank(s, 0);
            let rest = &s[from..];
            let n = rest.iter().take_while(|b| b.is_ascii_alphanumeric() || b"_-".contains(b)).count();
            let plain = rest.first().is_some_and(|b| b.is_ascii_lowercase() || *b == b'-');
            (n > 0 && plain && s.get(skip_blank(s, from + n)) == Some(&b':')).then_some((from, from + n))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts<'a>(lang: &str, state: &mut State, text: &'a str) -> Vec<(&'a str, Token)> {
        let mut out = Vec::new();
        line(lang_of(lang).unwrap(), state, text, &mut out);
        // The contract the painters lean on: starts at zero, in order, and every piece is valid text.
        if !text.is_empty() {
            assert_eq!(out.first().map(|t| t.0), Some(0), "{text}");
        }
        assert!(out.windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 != w[1].1), "{out:?}");
        (0..out.len()).map(|i| (&text[out[i].0..out.get(i + 1).map_or(text.len(), |n| n.0)], out[i].1)).collect()
    }

    fn one(lang: &str, text: &str) -> Vec<(String, Token)> {
        parts(lang, &mut State::default(), text).into_iter().filter(|p| !p.0.trim().is_empty() || p.1 != Token::Plain).map(|(s, t)| (s.trim().to_owned(), t)).collect()
    }

    fn has(lang: &str, text: &str, piece: &str, kind: Token) {
        let got = one(lang, text);
        assert!(got.iter().any(|(s, t)| s == piece && *t == kind), "{piece:?} as {kind:?} in {got:?}");
    }

    #[test]
    fn tables_are_sorted_for_bisection() {
        assert!(NAMES.windows(2).all(|w| w[0].0.as_bytes() < w[1].0.as_bytes()));
        for (_, g) in NAMES {
            assert!(g.keywords.windows(2).all(|w| w[0].as_bytes() < w[1].as_bytes()), "{}", g.name);
            if g.case_insensitive {
                assert!(g.keywords.iter().all(|k| k.bytes().all(|b| !b.is_ascii_lowercase())), "{}", g.name);
            }
        }
    }

    #[test]
    fn languages_come_from_names_extensions_and_fences() {
        let name = |n: &str| format!("{:?}", lang_of(n).unwrap());
        for (n, want) in [("rs", "rust"), ("rust", "rust"), ("main.RS", "rust"), ("src/lib/a.tsx", "tsx"), ("ts", "typescript"), ("typescript", "typescript"), ("py", "python"), ("python3", "python"), ("sh", "shell"), ("bash", "shell"), ("Makefile", "makefile"), ("Dockerfile", "dockerfile"), ("Dockerfile.dev", "dockerfile"), (".env.local", "shell"), ("Cargo.lock", "toml"), ("c++", "cpp"), ("md", "markdown"), ("README.md", "markdown"), ("json", "json")] {
            assert_eq!(name(n), want, "{n}");
        }
        for n in ["", "notes.xyz", "go.sum", "LICENSE"] {
            assert!(lang_of(n).is_none(), "{n}");
        }
    }

    #[test]
    fn rust() {
        has("rs", "pub fn main() {", "pub", Token::Keyword);
        has("rs", "pub fn main() {", "main", Token::Function);
        has("rs", "let v: Vec<u8> = Vec::new();", "Vec", Token::Type);
        has("rs", "let v: Vec<u8> = Vec::new();", "u8", Token::Keyword);
        has("rs", "    println!(\"hi {}\", 1);", "println!", Token::Function);
        has("rs", "    println!(\"hi {}\", 1);", "\"hi {}\"", Token::String);
        has("rs", "    println!(\"hi {}\", 1);", "1", Token::Number);
        has("rs", "#[derive(Clone, Copy)]", "#[derive(Clone, Copy)]", Token::Meta);
        has("rs", "#![allow(dead_code)]", "#![allow(dead_code)]", Token::Meta);
        has("rs", "// note", "// note", Token::Comment);
        has("rs", "let s = r#\"a \"quoted\" b\"#;", "r#\"a \"quoted\" b\"#", Token::String);
    }

    #[test]
    fn rust_lifetimes_are_not_strings() {
        let text = "fn f<'a>(x: &'a str) -> char { 'x' }";
        has("rs", text, "'a", Token::Variable);
        has("rs", text, "'x'", Token::String);
        has("rs", text, "str", Token::Keyword);
        has("rs", "let c = '\\n';", "'\\n'", Token::String);
        has("rs", "let c = '中';", "'中'", Token::String);
    }

    #[test]
    fn rust_strings_and_comments_cross_lines() {
        let mut st = State::default();
        assert_eq!(parts("rs", &mut st, "let s = \"one"), vec![("let", Token::Keyword), (" s = ", Token::Plain), ("\"one", Token::String)]);
        assert_ne!(st, State::default());
        assert_eq!(parts("rs", &mut st, "two"), vec![("two", Token::String)]);
        assert_eq!(parts("rs", &mut st, "three\"; fn"), vec![("three\"", Token::String), ("; ", Token::Plain), ("fn", Token::Keyword)]);
        assert_eq!(st, State::default());

        assert_eq!(parts("rs", &mut st, "a /* b"), vec![("a ", Token::Plain), ("/* b", Token::Comment)]);
        assert_eq!(parts("rs", &mut st, ""), vec![], "an empty line keeps the comment open");
        assert_eq!(parts("rs", &mut st, "c */ fn"), vec![("c */", Token::Comment), (" ", Token::Plain), ("fn", Token::Keyword)]);
        assert_eq!(st, State::default());
    }

    #[test]
    fn typescript() {
        has("ts", "export const a: Promise<string> = load(1);", "export", Token::Keyword);
        has("ts", "export const a: Promise<string> = load(1);", "Promise", Token::Type);
        has("ts", "export const a: Promise<string> = load(1);", "load", Token::Function);
        has("ts", "@Injectable()", "@Injectable", Token::Meta);
        let mut st = State::default();
        assert_eq!(parts("ts", &mut st, "const s = `a"), vec![("const", Token::Keyword), (" s = ", Token::Plain), ("`a", Token::String)]);
        assert_eq!(parts("ts", &mut st, "b` + 1"), vec![("b`", Token::String), (" + ", Token::Plain), ("1", Token::Number)]);
        // An unterminated one-line string does not colour the rest of the file.
        parts("ts", &mut st, "const s = 'oops");
        assert_eq!(st, State::default());
        parts("ts", &mut st, "/** doc");
        assert_eq!(parts("ts", &mut st, " * more */"), vec![(" * more */", Token::Comment)]);
    }

    #[test]
    fn tsx_tells_markup_from_comparison() {
        has("tsx", "return <Foo bar={1} baz=\"x\" disabled />;", "<Foo", Token::Tag);
        has("tsx", "return <Foo bar={1} baz=\"x\" disabled />;", "bar", Token::Attribute);
        has("tsx", "return <Foo bar={1} baz=\"x\" disabled />;", "\"x\"", Token::String);
        has("tsx", "return <Foo bar={1} baz=\"x\" disabled />;", "disabled", Token::Attribute);
        has("tsx", "return <Foo bar={1} baz=\"x\" disabled />;", "/>", Token::Tag);
        let got = one("tsx", "if (a < b) { Array<string>(); }");
        assert!(!got.iter().any(|(_, t)| *t == Token::Tag), "{got:?}");
        let mut st = State::default();
        parts("tsx", &mut st, "<Foo");
        assert_ne!(st, State::default(), "a tag whose attributes run on stays open");
        assert_eq!(parts("tsx", &mut st, "  bar=\"1\">"), vec![("  ", Token::Plain), ("bar", Token::Attribute), ("=", Token::Plain), ("\"1\"", Token::String), (">", Token::Tag)]);
        assert_eq!(st, State::default());
    }

    #[test]
    fn python() {
        has("py", "def run(self, x=1):", "def", Token::Keyword);
        has("py", "def run(self, x=1):", "run", Token::Function);
        has("py", "def run(self, x=1):", "self", Token::Keyword);
        has("py", "@dataclass", "@dataclass", Token::Meta);
        has("py", "x = 1  # note", "# note", Token::Comment);
        let mut st = State::default();
        assert_eq!(parts("py", &mut st, "\"\"\"Doc"), vec![("\"\"\"Doc", Token::String)]);
        assert_eq!(parts("py", &mut st, "# not a comment here"), vec![("# not a comment here", Token::String)]);
        assert_eq!(parts("py", &mut st, "end\"\"\" # now"), vec![("end\"\"\"", Token::String), (" ", Token::Plain), ("# now", Token::Comment)]);
        assert_eq!(st, State::default());
    }

    #[test]
    fn json() {
        has("json", "  \"name\": \"mewrk\",", "\"name\"", Token::Property);
        has("json", "  \"name\": \"mewrk\",", "\"mewrk\"", Token::String);
        has("json", "  \"ok\": true, \"n\": 12.5", "true", Token::Keyword);
        has("json", "  \"ok\": true, \"n\": 12.5", "12.5", Token::Number);
    }

    #[test]
    fn shell() {
        has("sh", "#!/bin/bash", "#!/bin/bash", Token::Meta);
        has("sh", "if [ -f \"$HOME/x\" ]; then", "if", Token::Keyword);
        has("sh", "echo $HOME ${PATH} # done", "$HOME", Token::Variable);
        has("sh", "echo $HOME ${PATH} # done", "${PATH}", Token::Variable);
        has("sh", "echo $HOME ${PATH} # done", "# done", Token::Comment);
        has("sh", "echo 'a\\' b", "'a\\'", Token::String);
        // A `#` inside a word is not a comment there.
        let got = one("sh", "echo c#sharp");
        assert!(!got.iter().any(|(_, t)| *t == Token::Comment), "{got:?}");
    }

    #[test]
    fn markdown() {
        has("md", "## Heading here", "## Heading here", Token::Keyword);
        assert_eq!(one("md", "#hashtag"), vec![("#hashtag".to_owned(), Token::Plain)]);
        has("md", "> quoted", "> quoted", Token::Comment);
        has("md", "- item with `code` in it", "-", Token::Meta);
        has("md", "- item with `code` in it", "`code`", Token::String);
        has("md", "12. twelfth", "12.", Token::Meta);
        has("md", "---", "---", Token::Meta);
        let mut st = State::default();
        parts("md", &mut st, "```rust");
        assert_eq!(parts("md", &mut st, "# not a heading"), vec![("# not a heading", Token::String)]);
        assert_eq!(parts("md", &mut st, "```"), vec![("```", Token::String)]);
        assert_eq!(st, State::default());
    }

    #[test]
    fn other_shapes_from_the_table() {
        has("yaml", "- name: value # note", "name", Token::Property);
        has("yaml", "- name: value # note", "# note", Token::Comment);
        has("toml", "[workspace.package]", "[workspace.package]", Token::Type);
        has("toml", "version = \"1\"", "version", Token::Property);
        has("css", "  color: #fff;", "color", Token::Property);
        has("css", "  color: #fff;", "#fff", Token::Number);
        has("html", "<a href=\"x\">text</a>", "<a", Token::Tag);
        has("html", "<a href=\"x\">text</a>", "href", Token::Attribute);
        has("html", "<a href=\"x\">text</a>", "</a>", Token::Tag);
        has("sql", "select id FROM t", "select", Token::Keyword);
        has("sql", "select id FROM t", "FROM", Token::Keyword);
        has("c", "#include <stdio.h>", "#include", Token::Meta);
        has("c", "#include <stdio.h>", "<stdio.h>", Token::String);
        has("diff", "@@ -1,2 +1,3 @@ fn main", "@@ -1,2 +1,3 @@", Token::Meta);
        has("diff", "+added", "+added", Token::String);
        has("diff", "-removed", "-removed", Token::Tag);
        has("lua", "--[[ block", "--[[ block", Token::Comment);
        has("bat", "%PATH% rem", "%PATH%", Token::Variable);
        has("bat", "rem a comment", "rem a comment", Token::Comment);
    }

    #[test]
    fn a_huge_line_stops_being_scanned_but_stays_valid() {
        let text = "let 中 = 1; ".repeat(10_000);
        let mut out = Vec::new();
        line(lang_of("rs").unwrap(), &mut State::default(), &text, &mut out);
        assert!(out.iter().all(|t| text.is_char_boundary(t.0)));
        assert!(out.last().unwrap().0 <= MAX_SCAN + 8);
        assert_eq!(out.last().unwrap().1, Token::Plain);
    }

    #[test]
    fn a_state_this_grammar_never_made_opens_nothing() {
        let mut st = State::open(OPEN_STRING, 200);
        assert_eq!(parts("json", &mut st, "1"), vec![("1", Token::Number)]);
        assert_eq!(st, State::default());
    }

    /// Painters slice the text at these offsets on the UI thread, so a bad one would take the app down.
    #[test]
    fn any_text_in_any_language_gives_valid_offsets() {
        const BITS: [&str; 32] = ["\\", "'", "\"", "`", "<", ">", "/>", "</", "#", "$", "%", "@", "{", "}", "[", "]", "/*", "*/", "//", "--", "=", ":", " ", "\t", "中", "é", "🐱", "a", "Z9", "fn", "1.5", "r#\""];
        let mut rng = 0x1234_5678_u64;
        let mut out = Vec::new();
        let mut text = String::new();
        for (_, g) in NAMES {
            let mut state = State::default();
            for _ in 0..300 {
                text.clear();
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                for i in 0..(rng >> 60) {
                    text.push_str(BITS[((rng >> (i * 5 % 40)) % 32) as usize]);
                }
                line(Lang(g), &mut state, &text, &mut out);
                assert!(out.windows(2).all(|w| w[0].0 < w[1].0), "{} on {text:?}: {out:?}", g.name);
                assert!(out.iter().all(|t| t.0 <= text.len() && text.is_char_boundary(t.0)), "{} on {text:?}: {out:?}", g.name);
            }
        }
    }

    #[test]
    fn every_token_reads_on_both_themes() {
        fn lum(c: Color) -> f32 {
            let f = |v: u8| {
                let v = v as f32 / 255.0;
                if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
            };
            0.2126 * f(c.r) + 0.7152 * f(c.g) + 0.0722 * f(c.b)
        }
        let all = [Token::Plain, Token::Keyword, Token::String, Token::Number, Token::Comment, Token::Function, Token::Type, Token::Meta, Token::Tag, Token::Attribute, Token::Property, Token::Variable];
        for theme in [Theme::dark(), Theme::light()] {
            for t in all {
                let (fg, bg) = (lum(color(t, &theme)), lum(theme.mica_base));
                let ratio = (fg.max(bg) + 0.05) / (fg.min(bg) + 0.05);
                assert!(ratio > 3.0, "{t:?} on dark={}: {ratio}", theme.dark);
            }
        }
    }
}
