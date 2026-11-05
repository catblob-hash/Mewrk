//! `mhchemParser.ts` 4.2.1, the parser behind `\ce` and `\pu`: chemical notation in, TeX out, by the same state machines. The tables at
//! the end are that file's own, turned into Rust data. What it makes then goes through the replacements `MhchemConfiguration.ts` does.

use regex::Regex;
use std::cell::RefCell;
use std::collections::HashMap;

type Parsed<T> = Result<T, String>;

/// `\ce{\ce{\ce{...`: a message is untrusted, and every level here is a level of stack.
const MAX_LEVEL: u32 = 16;

/// `\ce` or `\pu`, as the TeX `Machine` splices in.
pub(super) fn to_tex(input: &str, pu: bool) -> Parsed<String> {
    let parsed = go(input, if pu { "pu" } else { "ce" }, 0)?;
    let tex = texify(&parsed, true)?;
    Ok(replacements(&tex))
}

fn re(source: &'static str) -> Regex {
    thread_local! {
        static COMPILED: RefCell<HashMap<&'static str, Regex>> = RefCell::new(HashMap::new());
    }
    COMPILED.with(|c| c.borrow_mut().entry(source).or_insert_with(|| Regex::new(source).expect("regex")).clone())
}

/// `MhchemReplacements`: what its font has its own arrows for.
fn replacements(tex: &str) -> String {
    let tex = re(r"\\underset\{\\lower2mu\{(.*?)\}\}\{\\overset\{(.*?)\}\{\\long(.*?)\}\}").replace_all(tex, "\\mhchemx${3}[${1}]{${2}}");
    let tex = re(r"\\overset\{(.*?)\}\{\\long(.*?)\}").replace_all(&tex, "\\mhchemx${2}{${1}}");
    let tex = tex.replace("\\rlap{\\lower.1em{-}}\\raise.1em{\\tripledash}", "\\mhchemBondTD");
    let tex = tex.replace("\\rlap{\\lower.2em{-}}\\rlap{\\raise.2em{\\tripledash}}-", "\\mhchemBondTDD");
    let tex = tex.replace("\\rlap{\\lower.2em{-}}\\rlap{\\raise.2em{-}}\\tripledash", "\\mhchemBondDTD");
    re(r"\\(x?(?:long)?(?:left|right|[Ll]eftright|[Rr]ightleft)(?:arrow|harpoons))")
        .replace_all(&tex, |c: &regex::Captures<'_>| {
            let arrow = &c[1];
            if matches!(arrow, "longleftarrow" | "longrightarrow" | "longleftrightarrow" | "longrightleftharpoons" | "longRightleftharpoons" | "longLeftrightharpoons" | "longleftrightarrows" | "rightarrow" | "leftarrow" | "leftrightarrow" | "xrightarrow" | "xleftarrow" | "xleftrightarrow" | "xleftrightarrows" | "xrightleftharpoons" | "xRightleftharpoons" | "xLeftrightharpoons") {
                format!("\\mhchem{arrow}")
            } else {
                c[0].to_owned()
            }
        })
        .into_owned()
}

// ---------------------------------------------------------------- what comes out

#[derive(Clone, Debug)]
enum P {
    Text(String),
    /// What has nothing but its name to say.
    Plain(&'static str),
    One(&'static str, String),
    List(&'static str, Vec<P>),
    Two(&'static str, Vec<P>, Vec<P>),
    Frac(String, String),
    Five { a: Vec<P>, b: Vec<P>, p: Vec<P>, o: Vec<P>, q: Vec<P>, d: Vec<P>, kind: Option<&'static str> },
    Arrow { r: String, rd: Vec<P>, rq: Vec<P> },
    Color(String, Vec<P>),
}

/// A pattern's `match_`.
#[derive(Clone, Debug)]
enum M {
    S(String),
    L(Vec<String>),
}

impl M {
    fn s(&self) -> &str {
        match self {
            M::S(s) => s,
            M::L(l) => l.first().map_or("", String::as_str),
        }
    }

    fn at(&self, i: usize) -> &str {
        match self {
            M::S(s) if i == 0 => s,
            M::S(_) => "",
            M::L(l) => l.get(i).map_or("", String::as_str),
        }
    }
}

type Hit = Option<(M, usize)>;

fn plain(m: Option<regex::Captures<'_>>) -> Hit {
    let m = m?;
    let whole = m.get(0).expect("whole").len();
    if m.len() > 2 {
        let list = (1..m.len()).map(|i| m.get(i).map_or(String::new(), |g| g.as_str().to_owned())).collect();
        return Some((M::L(list), whole));
    }
    let shown = m.get(1).map(|g| g.as_str()).filter(|g| !g.is_empty()).unwrap_or_else(|| m.get(0).expect("whole").as_str());
    Some((M::S(shown.to_owned()), whole))
}

/// A literal or a pattern at the start of the text.
#[derive(Clone, Copy)]
enum Pat {
    Lit(&'static str),
    Re(&'static str),
}

const NONE: Pat = Pat::Lit("");

fn at_start(input: &str, pattern: Pat) -> Option<usize> {
    match pattern {
        Pat::Lit(lit) => input.starts_with(lit).then_some(lit.len()),
        Pat::Re(source) => re(source).find(input).map(|m| m.len()),
    }
}

fn pat_is_empty(pattern: Pat) -> bool {
    matches!(pattern, Pat::Lit(""))
}

#[allow(clippy::too_many_arguments)]
fn observe(input: &str, beg_excl: Pat, beg_incl: Pat, end_incl: Pat, end_excl: Pat) -> Parsed<Option<(String, usize)>> {
    let Some(skip) = at_start(input, beg_excl) else { return Ok(None) };
    let rest = &input[skip..];
    let Some(open) = at_start(rest, beg_incl) else { return Ok(None) };
    let end = if pat_is_empty(end_incl) { end_excl } else { end_incl };
    let mut braces = 0;
    let mut at = open;
    while at < rest.len() {
        let here = &rest[at..];
        if braces == 0
            && let Some(len) = at_start(here, end)
        {
            let shown_end = if pat_is_empty(end_incl) { at } else { at + len };
            return Ok(Some((rest[..shown_end].to_owned(), skip + at + len)));
        }
        let c = here.chars().next().expect("not at the end");
        if c == '{' {
            braces += 1;
        } else if c == '}' {
            if braces == 0 {
                return Err("Extra close brace or missing open brace".to_owned());
            }
            braces -= 1;
        }
        at += c.len_utf8();
    }
    Ok(None)
}

/// With a second group after the first.
#[allow(clippy::too_many_arguments)]
fn observe2(input: &str, a: (Pat, Pat, Pat, Pat), b: (Pat, Pat, Pat, Pat), combine: bool) -> Parsed<Hit> {
    let Some((first, used)) = observe(input, a.0, a.1, a.2, a.3)? else { return Ok(None) };
    let Some((second, more)) = observe(&input[used..], b.0, b.1, b.2, b.3)? else { return Ok(None) };
    let total = used + more;
    Ok(Some(if combine { (M::S(format!("{first}{second}")), total) } else { (M::L(vec![first, second]), total) }))
}

fn observed(hit: Option<(String, usize)>) -> Hit {
    hit.map(|(s, used)| (M::S(s), used))
}

const GREEK: &[&str] = &["alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa", "lambda", "mu", "nu", "xi", "omicron", "pi", "rho", "sigma", "tau", "upsilon", "phi", "chi", "psi", "omega", "Gamma", "Delta", "Theta", "Lambda", "Xi", "Pi", "Sigma", "Upsilon", "Phi", "Psi", "Omega"];

/// `\\(?:alpha|...)(?:\s+|\{\}|(?![a-zA-Z]))`, answering its length.
fn greek_command(input: &str) -> Option<usize> {
    let rest = input.strip_prefix('\\')?;
    let run = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
    if !GREEK.contains(&&rest[..run]) {
        return None;
    }
    let after = &rest[run..];
    let spaces = after.len() - after.trim_start().len();
    let tail = if spaces > 0 {
        spaces
    } else if after.starts_with("{}") {
        2
    } else {
        0
    };
    Some(1 + run + tail)
}

/// What `(?=...)` looks at, in a pattern the regex engine here cannot say.
fn followed(input: &str, used: usize, what: &'static str) -> bool {
    re(what).is_match(&input[used..])
}

fn with_look(input: &str, head: &'static str, look: &'static str) -> Hit {
    let used = re(head).find(input)?.len();
    followed(input, used, look).then(|| (M::S(input[..used].to_owned()), used))
}

fn pattern(name: &str, input: &str) -> Parsed<Hit> {
    let lit = Pat::Lit;
    let pat = Pat::Re;
    Ok(match name {
        "empty" => input.is_empty().then(|| (M::S(String::new()), 0)),
        "else" | "else2" => input.chars().next().map(|c| (M::S(c.to_string()), c.len_utf8())),
        "space" => plain(re(r"^\s").captures(input)),
        "space A" => with_look(input, r"^\s", r"^[A-Z\\$]"),
        "space$" => plain(re(r"^\s$").captures(input)),
        "a-z" => plain(re(r"^[a-z]").captures(input)),
        "x" => plain(re(r"^x").captures(input)),
        "x$" => plain(re(r"^x$").captures(input)),
        "i$" => plain(re(r"^i$").captures(input)),
        "letters" => {
            let mut at = 0;
            while at < input.len() {
                let rest = &input[at..];
                let c = rest.chars().next().expect("not at the end");
                if c.is_ascii_alphabetic() || ('\u{3b1}'..='\u{3c9}').contains(&c) || ('\u{391}'..='\u{3a9}').contains(&c) || c == '?' || c == '@' {
                    at += c.len_utf8();
                } else if let Some(len) = greek_command(rest) {
                    at += len;
                } else {
                    break;
                }
            }
            (at > 0).then(|| (M::S(input[..at].to_owned()), at))
        }
        "\\greek" => greek_command(input).map(|len| (M::S(input[..len].to_owned()), len)),
        "one lowercase latin letter $" => plain(re(r"^(?:([a-z])(?:$|[^a-zA-Z]))$").captures(input)),
        "$one lowercase latin letter$ $" => plain(re(r"^\$(?:([a-z])(?:$|[^a-zA-Z]))\$$").captures(input)),
        "one lowercase greek letter $" => {
            let body = input.strip_prefix('$').unwrap_or(input);
            let first = body.chars().next();
            let used = match first {
                Some(c) if ('\u{3b1}'..='\u{3c9}').contains(&c) => c.len_utf8(),
                Some('\\') => {
                    let rest = &body[1..];
                    let run = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
                    if !GREEK[..24].contains(&&rest[..run]) {
                        return Ok(None);
                    }
                    1 + run
                }
                _ => return Ok(None),
            };
            let tail = body[used..].trim_start().trim_start_matches('$');
            (tail.is_empty() || tail == "{}").then(|| (M::S(input.to_owned()), input.len()))
        }
        "digits" => plain(re(r"^[0-9]+").captures(input)),
        "-9.,9" => plain(re(r"^[+\-]?(?:[0-9]+(?:[,.][0-9]+)?|[0-9]*(?:\.[0-9]+))").captures(input)),
        "-9.,9 no missing 0" => plain(re(r"^[+\-]?[0-9]+(?:[.,][0-9]+)?").captures(input)),
        "(-)(9.,9)(e)(99)" => {
            let m = re(r"^(\+\-|\+/\-|\+|\-|\\pm\s?)?([0-9]+(?:[,.][0-9]+)?|[0-9]*(?:\.[0-9]+))?(\((?:[0-9]+(?:[,.][0-9]+)?|[0-9]*(?:\.[0-9]+))\))?(?:(?:([eE])|\s*(\*|x|\\times|\x{00D7})\s*10\^)([+\-]?[0-9]+|\{[+\-]?[0-9]+\}))?").captures(input);
            match m {
                Some(m) if !m.get(0).expect("whole").is_empty() => plain(Some(m)),
                _ => None,
            }
        }
        "(-)(9)^(-9)" => plain(re(r"^(\+\-|\+/\-|\+|\-|\\pm\s?)?([0-9]+(?:[,.][0-9]+)?|[0-9]*(?:\.[0-9]+)?)\^([+\-]?[0-9]+|\{[+\-]?[0-9]+\})").captures(input)),
        "state of aggregation $" => {
            let opener = |input: &str| -> Option<usize> {
                let used = re(r"^\([a-z]{1,3}").find(input)?.len();
                matches!(input[used..].chars().next(), Some(')' | ',')).then_some(used)
            };
            if let Some(open) = opener(input)
                && let Some(close) = find_close(&input[open..], ')')?
            {
                let used = open + close;
                if re(r"^($|[\s,;\)\]\}])").is_match(&input[used..]) {
                    return Ok(Some((M::S(input[..used].to_owned()), used)));
                }
            }
            plain(re(r"^(?:\((?:\\ca\s?)?\$[amothc]\$\))").captures(input))
        }
        "_{(state of aggregation)}$" => plain(re(r"^_\{(\([a-z]{1,3}\))\}").captures(input)),
        "{[(" => plain(re(r"^(?:\\\{|\[|\()").captures(input)),
        ")]}" => plain(re(r"^(?:\)|\]|\\\})").captures(input)),
        ", " => plain(re(r"^[,;]\s*").captures(input)),
        "," => plain(re(r"^[,;]").captures(input)),
        "." => plain(re(r"^[.]").captures(input)),
        ". __* " => plain(re(r"^([.\x{22C5}\x{00B7}\x{2022}]|[*])\s*").captures(input)),
        "..." => with_look(input, r"^\.\.\.", r"^($|[^.])"),
        "^{(...)}" => observed(observe(input, lit("^{"), NONE, NONE, lit("}"))?),
        "^($...$)" => observed(observe(input, lit("^"), lit("$"), lit("$"), NONE)?),
        "^a" => plain(re(r"^\^([0-9]+|[^\\_])").captures(input)),
        "^\\x{}{}" => observe2(input, (lit("^"), pat(r"^\\[a-zA-Z]+\{"), lit("}"), NONE), (NONE, lit("{"), lit("}"), NONE), true)?,
        "^\\x{}" => observed(observe(input, lit("^"), pat(r"^\\[a-zA-Z]+\{"), lit("}"), NONE)?),
        "^\\x" => plain(re(r"^\^(\\[a-zA-Z]+)\s*").captures(input)),
        "^(-1)" => plain(re(r"^\^(-?\d+)").captures(input)),
        "'" => plain(re(r"^'").captures(input)),
        "_{(...)}" => observed(observe(input, lit("_{"), NONE, NONE, lit("}"))?),
        "_($...$)" => observed(observe(input, lit("_"), lit("$"), lit("$"), NONE)?),
        "_9" => plain(re(r"^_([+\-]?[0-9]+|[^\\])").captures(input)),
        "_\\x{}{}" => observe2(input, (lit("_"), pat(r"^\\[a-zA-Z]+\{"), lit("}"), NONE), (NONE, lit("{"), lit("}"), NONE), true)?,
        "_\\x{}" => observed(observe(input, lit("_"), pat(r"^\\[a-zA-Z]+\{"), lit("}"), NONE)?),
        "_\\x" => plain(re(r"^_(\\[a-zA-Z]+)\s*").captures(input)),
        "^_" => {
            if input.starts_with("^_") || input.starts_with("_^") {
                Some((M::S(input[..1].to_owned()), 1))
            } else if input == "^" || input == "_" {
                Some((M::S(input.to_owned()), 1))
            } else {
                None
            }
        }
        "{}^" => input.starts_with("{}^").then(|| (M::S("{}".to_owned()), 2)),
        "{}" => input.starts_with("{}").then(|| (M::S("{}".to_owned()), 2)),
        "{...}" => observed(observe(input, NONE, lit("{"), lit("}"), NONE)?),
        "{(...)}" => observed(observe(input, lit("{"), NONE, NONE, lit("}"))?),
        "$...$" => observed(observe(input, NONE, lit("$"), lit("$"), NONE)?),
        "${(...)}$__$(...)$" => match observe(input, lit("${"), NONE, NONE, lit("}$"))? {
            Some(found) => observed(Some(found)),
            None => observed(observe(input, lit("$"), NONE, NONE, lit("$"))?),
        },
        "=<>" => plain(re(r"^[=<>]").captures(input)),
        "#" => plain(re(r"^[#\x{2261}]").captures(input)),
        "+" => plain(re(r"^\+").captures(input)),
        "-$" => with_look(input, r"^-", r"^([\s_},;\]/]|$|\([a-z]+\))"),
        "-9" => with_look(input, r"^-", r"^[0-9]"),
        "- orbital overlap" => with_look(input, r"^-", r"^(?:[spd]|sp)(?:$|[\s,;\)\]\}])"),
        "-" => plain(re(r"^-").captures(input)),
        "pm-operator" => plain(re(r"^(?:\\pm|\$\\pm\$|\+-|\+/-)").captures(input)),
        "operator" => {
            if input.starts_with('+') {
                return Ok(Some((M::S("+".to_owned()), 1)));
            }
            let look = r"^(?:\s|$|-?[0-9])";
            for head in [r"^[\-=<>]", "^<<", "^>>", r"^\\approx", r"^\$\\approx\$"] {
                if let Some(hit) = with_look(input, head, look) {
                    return Ok(Some(hit));
                }
            }
            None
        }
        "arrowUpDown" => with_look(input, r"^(?:v|\(v\)|\^|\(\^\))", r"^($|[\s,;\)\]\}])"),
        "\\bond{(...)}" => observed(observe(input, lit("\\bond{"), NONE, NONE, lit("}"))?),
        "->" => plain(re(r"^(?:<->|<-->|->|<-|<=>>|<<=>|<=>|[\x{2192}\x{27F6}\x{21CC}])").captures(input)),
        "CMT" => with_look(input, r"^[CMT]", r"^\["),
        "[(...)]" => observed(observe(input, lit("["), NONE, NONE, lit("]"))?),
        "1st-level escape" => plain(re(r"^(&|\\\\|\\hline)\s*").captures(input)),
        "\\," => plain(re(r"^(?:\\[, ;:])").captures(input)),
        "\\x{}{}" => observe2(input, (NONE, pat(r"^\\[a-zA-Z]+\{"), lit("}"), NONE), (NONE, lit("{"), lit("}"), NONE), true)?,
        "\\x{}" => observed(observe(input, NONE, pat(r"^\\[a-zA-Z]+\{"), lit("}"), NONE)?),
        "\\ca" => {
            let Some(rest) = input.strip_prefix("\\ca") else { return Ok(None) };
            let spaces = rest.len() - rest.trim_start().len();
            if spaces == 0 && rest.starts_with(|c: char| c.is_ascii_alphabetic()) {
                return Ok(None);
            }
            Some((M::S(input[..3 + spaces].to_owned()), 3 + spaces))
        }
        "\\x" => plain(re(r"^(?:\\[a-zA-Z]+\s*|\\[_&{}%])").captures(input)),
        "orbital" => with_look(input, r"^(?:[0-9]{1,2}[spdfgh]|[0-9]{0,2}sp)", r"^($|[^a-zA-Z])"),
        "others" => plain(re(r"^[/~|]").captures(input)),
        "\\frac{(...)}" => observe2(input, (lit("\\frac{"), NONE, NONE, lit("}")), (lit("{"), NONE, NONE, lit("}")), false)?,
        "\\overset{(...)}" => observe2(input, (lit("\\overset{"), NONE, NONE, lit("}")), (lit("{"), NONE, NONE, lit("}")), false)?,
        "\\underset{(...)}" => observe2(input, (lit("\\underset{"), NONE, NONE, lit("}")), (lit("{"), NONE, NONE, lit("}")), false)?,
        "\\underbrace{(...)}" => observe2(input, (lit("\\underbrace{"), NONE, NONE, lit("}_")), (lit("{"), NONE, NONE, lit("}")), false)?,
        "\\color{(...)}" => observed(observe(input, lit("\\color{"), NONE, NONE, lit("}"))?),
        "\\color{(...)}{(...)}" => match observe2(input, (lit("\\color{"), NONE, NONE, lit("}")), (lit("{"), NONE, NONE, lit("}")), false)? {
            Some(hit) => Some(hit),
            None => {
                // `\color\red{...}`.
                let Some(rest) = input.strip_prefix("\\color\\") else { return Ok(None) };
                let run = rest.find('{').unwrap_or(rest.len());
                if run == rest.len() {
                    return Ok(None);
                }
                let Some((second, more)) = observe(&rest[run..], lit("{"), NONE, NONE, lit("}"))? else { return Ok(None) };
                Some((M::L(vec![format!("\\{}", &rest[..run]), second]), 7 + run + more))
            }
        },
        "\\ce{(...)}" => observed(observe(input, lit("\\ce{"), NONE, NONE, lit("}"))?),
        "\\pu{(...)}" => observed(observe(input, lit("\\pu{"), NONE, NONE, lit("}"))?),
        "oxidation$" => plain(re(r"^(?:[+-][IVX]+|(?:\\pm|\$\\pm\$|\+-|\+/-)\s*0)$").captures(input)),
        "d-oxidation$" => plain(re(r"^(?:[+-]?[IVX]+|(?:\\pm|\$\\pm\$|\+-|\+/-)\s*0)$").captures(input)),
        "1/2$" => plain(re(r"^[+\-]?(?:[0-9]+|\$[a-z]\$|[a-z])/[0-9]+(?:\$[a-z]\$|[a-z])?$").captures(input)),
        "amount" | "amount2" => {
            if let Some(m) = re(r"^(?:\([+\-]?[0-9]+/[0-9]+\)|[+\-]?(?:[0-9]+|\$[a-z]\$|[a-z])/[0-9]+|[+\-]?[0-9]+[.,][0-9]+|[+\-]?\.[0-9]+|[+\-]?[0-9]+)").find(input) {
                let mut used = m.len();
                if input[used..].starts_with(|c: char| c.is_ascii_lowercase()) && followed(input, used + 1, r"^\s*[A-Z]") {
                    used += 1;
                }
                return Ok(Some((M::S(input[..used].to_owned()), used)));
            }
            if let Some(hit) = with_look(input, r"^[+\-]?[a-z]", r"^\s*[A-Z]") {
                return Ok(Some(hit));
            }
            if input.starts_with('+') && !input[1..].starts_with(char::is_whitespace) {
                return Ok(Some((M::S("+".to_owned()), 1)));
            }
            let Some((found, used)) = observe(input, NONE, lit("$"), lit("$"), NONE)? else { return Ok(None) };
            re(r"^\$(?:\(?[+\-]?(?:[0-9]*[a-z]?[+\-])?[0-9]*[a-z](?:[+\-][0-9]*[a-z]?)?\)?|\+|-)\$$").is_match(&found).then_some((M::S(found), used))
        }
        "(KV letters)," => with_look(input, r"^(?:[A-Z][a-z]{0,2}|i)", r"^,"),
        "formula$" => {
            if re(r"^\([a-z]+\)$").is_match(input) {
                return Ok(None);
            }
            plain(re(r"^(?:[a-z]|(?:[0-9 +\-,.()]+[a-z])+[0-9 +\-,.()]*|(?:[a-z][0-9 +\-,.()]+)+[a-z]?)$").captures(input))
        }
        "uprightEntities" => with_look(input, r"^(?:pH|pOH|pC|pK|iPr|iBu)", r"^($|[^a-zA-Z])"),
        "/" => plain(re(r"^\s*(/)\s*").captures(input)),
        "//" => plain(re(r"^\s*(//)\s*").captures(input)),
        "*" => plain(re(r"^\s*[*.]\s*").captures(input)),
        _ => return Err(format!("mhchem bug P. Please report. ({name})")),
    })
}

/// `_findObserveGroups` for one closing character: where past it the text goes on.
fn find_close(input: &str, close: char) -> Parsed<Option<usize>> {
    let mut braces = 0;
    for (at, c) in input.char_indices() {
        if braces == 0 && c == close {
            return Ok(Some(at + c.len_utf8()));
        }
        if c == '{' {
            braces += 1;
        } else if c == '}' {
            if braces == 0 {
                return Err("Extra close brace or missing open brace".to_owned());
            }
            braces -= 1;
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------- the machines

#[derive(Default)]
struct Buffer {
    a: Option<String>,
    b: Option<String>,
    p: Option<String>,
    o: Option<String>,
    q: Option<String>,
    d: Option<String>,
    rm: Option<String>,
    text: Option<String>,
    r: Option<String>,
    rdt: Option<String>,
    rd: Option<String>,
    rqt: Option<String>,
    rq: Option<String>,
    d_type: Option<&'static str>,
    sb: bool,
    parens: i32,
    begins_with_bond: bool,
}

impl Buffer {
    /// `for (const p in buffer) delete buffer[p]`.
    fn clear(&mut self) {
        let keep = (self.parens, self.begins_with_bond);
        *self = Buffer::default();
        (self.parens, self.begins_with_bond) = keep;
    }
}

fn add(slot: &mut Option<String>, text: &str) {
    slot.get_or_insert_with(String::new).push_str(text);
}

fn truthy(slot: &Option<String>) -> bool {
    slot.as_deref().is_some_and(|s| !s.is_empty())
}

fn table(machine: &str) -> Parsed<&'static [Row]> {
    Ok(match machine {
        "ce" => M_CE,
        "a" => M_A,
        "o" => M_O,
        "text" => M_TEXT,
        "pq" => M_PQ,
        "bd" => M_BD,
        "oxidation" => M_OXIDATION,
        "tex-math" => M_TEX_MATH,
        "tex-math tight" => M_TEX_MATH_TIGHT,
        "9,9" => M_9_9,
        "pu" => M_PU,
        "pu-2" => M_PU_2,
        "pu-9,9" => M_PU_9_9,
        _ => return Err(format!("mhchem bug A. Please report. ({machine})")),
    })
}

fn go_opt(input: &Option<String>, machine: &str, level: u32) -> Parsed<Vec<P>> {
    match input.as_deref() {
        Some(text) if !text.is_empty() => go(text, machine, level),
        _ => Ok(Vec::new()),
    }
}

fn go(input: &str, machine: &str, level: u32) -> Parsed<Vec<P>> {
    if input.is_empty() {
        return Ok(Vec::new());
    }
    if level > MAX_LEVEL {
        return Err("Formula is nested too deeply".to_owned());
    }
    let rows = table(machine)?;
    let cleaned: String = input.chars().map(|c| match c {
        '\n' => ' ',
        '\u{2212}' | '\u{2013}' | '\u{2014}' | '\u{2010}' => '-',
        c => c,
    }).collect::<String>().replace('\u{2026}', "...");
    let mut input: &str = &cleaned;
    let mut state: &str = "0";
    let mut buffer = Buffer::default();
    let mut output = Vec::new();
    let mut last_len = usize::MAX;
    let mut watchdog = 10;
    loop {
        if input.len() != last_len {
            watchdog = 10;
            last_len = input.len();
        } else {
            watchdog -= 1;
        }
        // The transitions of the state this round started in, for all of this round.
        let from = state;
        'round: for row in rows {
            for entry in row.1 {
                if !entry.0.split('|').any(|s| s == from || s == "*") {
                    continue;
                }
                for alternative in split_patterns(row.0) {
                    let Some((matched, used)) = pattern(alternative, input)? else { continue };
                    for action in entry.1 {
                        run(machine, action, &mut buffer, &matched, &mut output, level)?;
                    }
                    if let Some(next) = entry.2 {
                        state = next;
                    }
                    if input.is_empty() {
                        return Ok(output);
                    }
                    if !entry.3 {
                        input = &input[used..];
                    }
                    if !entry.4 {
                        break 'round;
                    }
                }
            }
        }
        if watchdog <= 0 {
            return Err("mhchem bug U. Please report.".to_owned());
        }
    }
}

fn split_patterns(key: &'static str) -> Vec<&'static str> {
    // The one name with a `|` of its own is the two alternatives it reads as, which is how the original's `split` reads it too.
    key.split('|').collect()
}

fn run(machine: &str, action: &A, buffer: &mut Buffer, m: &M, out: &mut Vec<P>, level: u32) -> Parsed<()> {
    let next = level + 1;
    let name = action.0;
    let option = action.1;
    // Each machine's own actions come first.
    match (machine, name) {
        ("ce", "o after d") => {
            if re(r"^[1-9][0-9]*$").is_match(buffer.d.as_deref().unwrap_or("")) {
                let tmp = buffer.d.take();
                output_ce(buffer, O::N, out, next)?;
                out.push(P::Plain("tinySkip"));
                buffer.b = tmp;
            } else {
                output_ce(buffer, O::N, out, next)?;
            }
            add(&mut buffer.o, m.s());
        }
        ("ce", "d= kv") => {
            buffer.d = Some(m.s().to_owned());
            buffer.d_type = Some("kv");
        }
        ("ce", "charge or bond") => {
            if buffer.begins_with_bond {
                output_ce(buffer, O::N, out, next)?;
                out.push(P::One("bond", "-".to_owned()));
            } else {
                buffer.d = Some(m.s().to_owned());
            }
        }
        ("ce", "- after o/d") => {
            let o = buffer.o.clone().unwrap_or_default();
            let c1 = pattern("orbital", &o)?;
            let c2 = pattern("one lowercase greek letter $", &o)?.is_some();
            let c3 = pattern("one lowercase latin letter $", &o)?.is_some();
            let c4 = pattern("$one lowercase latin letter$ $", &o)?.is_some();
            let orbital_whole = c1.as_ref().is_some_and(|(_, used)| *used == o.len());
            let hyphen_follows = m.s() == "-" && (orbital_whole || c2 || c3 || c4);
            if hyphen_follows && !truthy(&buffer.a) && !truthy(&buffer.b) && !truthy(&buffer.p) && !truthy(&buffer.d) && !truthy(&buffer.q) && c1.is_none() && c3 {
                buffer.o = Some(format!("${o}$"));
            }
            if hyphen_follows {
                output_ce(buffer, O::N, out, next)?;
                out.push(P::Plain("hyphen"));
            } else {
                let d = buffer.d.clone().unwrap_or_default();
                let digits = pattern("digits", &d)?.is_some_and(|(_, used)| used == d.len());
                if matches!(option, O::B(true)) && digits {
                    add(&mut buffer.d, m.s());
                    output_ce(buffer, O::N, out, next)?;
                } else {
                    output_ce(buffer, O::N, out, next)?;
                    out.push(P::One("bond", "-".to_owned()));
                }
            }
        }
        ("ce", "a to o") => buffer.o = buffer.a.take(),
        ("ce", "sb=true") => buffer.sb = true,
        ("ce", "sb=false") => buffer.sb = false,
        ("ce", "beginsWithBond=true") => buffer.begins_with_bond = true,
        ("ce", "beginsWithBond=false") => buffer.begins_with_bond = false,
        ("ce", "parenthesisLevel++") => buffer.parens += 1,
        ("ce", "parenthesisLevel--") => buffer.parens -= 1,
        ("ce", "state of aggregation") => out.push(P::List("state of aggregation", go(m.s(), "o", next)?)),
        ("ce", "comma") => {
            let shown = m.s().trim_end();
            let with_space = shown.len() != m.s().len();
            out.push(P::One(if with_space && buffer.parens == 0 { "comma enumeration L" } else { "comma enumeration M" }, shown.to_owned()));
        }
        ("ce", "output") => output_ce(buffer, option, out, next)?,
        ("ce", "oxidation-output") => {
            out.push(P::Text("{".to_owned()));
            out.extend(go(m.s(), "oxidation", next)?);
            out.push(P::Text("}".to_owned()));
        }
        ("ce", "frac-output") => out.push(P::Two("frac-ce", go(m.at(0), "ce", next)?, go(m.at(1), "ce", next)?)),
        ("ce", "overset-output") => out.push(P::Two("overset", go(m.at(0), "ce", next)?, go(m.at(1), "ce", next)?)),
        ("ce", "underset-output") => out.push(P::Two("underset", go(m.at(0), "ce", next)?, go(m.at(1), "ce", next)?)),
        ("ce", "underbrace-output") => out.push(P::Two("underbrace", go(m.at(0), "ce", next)?, go(m.at(1), "ce", next)?)),
        ("ce", "color-output") => out.push(P::Color(m.at(0).to_owned(), go(m.at(1), "ce", next)?)),
        ("ce", "r=") => buffer.r = Some(m.s().to_owned()),
        ("ce", "rdt=") => buffer.rdt = Some(m.s().to_owned()),
        ("ce", "rd=") => buffer.rd = Some(m.s().to_owned()),
        ("ce", "rqt=") => buffer.rqt = Some(m.s().to_owned()),
        ("ce", "rq=") => buffer.rq = Some(m.s().to_owned()),
        ("ce" | "pu", "operator") => out.push(P::One("operator", if let O::S(kind) = option { kind.to_owned() } else { m.s().to_owned() })),
        ("text", "output") => {
            if truthy(&buffer.text) {
                out.push(P::One("text", buffer.text.take().unwrap_or_default()));
                buffer.clear();
            }
        }
        ("pq", "state of aggregation") => out.push(P::List("state of aggregation subscript", go(m.s(), "o", next)?)),
        ("pq", "color-output") => out.push(P::Color(m.at(0).to_owned(), go(m.at(1), "pq", next)?)),
        ("bd", "color-output") => out.push(P::Color(m.at(0).to_owned(), go(m.at(1), "bd", next)?)),
        ("oxidation", "roman-numeral") => out.push(P::One("roman numeral", buffer.o.clone().unwrap_or_default())),
        ("tex-math" | "tex-math tight", "output") => {
            if truthy(&buffer.o) {
                out.push(P::One("tex-math", buffer.o.take().unwrap_or_default()));
                buffer.clear();
            }
        }
        ("tex-math tight", "tight operator") => add(&mut buffer.o, &format!("{{{}}}", m.s())),
        ("9,9" | "pu-9,9", "comma") => out.push(P::Plain("commaDecimal")),
        ("pu", "enumber") => {
            if m.at(0) == "+-" || m.at(0) == "+/-" {
                out.push(P::Text("\\pm ".to_owned()));
            } else if !m.at(0).is_empty() {
                out.push(P::Text(m.at(0).to_owned()));
            }
            if !m.at(1).is_empty() {
                out.extend(go(m.at(1), "pu-9,9", next)?);
                if !m.at(2).is_empty() {
                    if m.at(2).contains([',', '.']) {
                        out.extend(go(m.at(2), "pu-9,9", next)?);
                    } else {
                        out.push(P::Text(m.at(2).to_owned()));
                    }
                }
                if !m.at(3).is_empty() || !m.at(4).is_empty() {
                    if m.at(3) == "e" || m.at(4) == "*" {
                        out.push(P::Plain("cdot"));
                    } else {
                        out.push(P::Plain("times"));
                    }
                }
            }
            if !m.at(5).is_empty() {
                out.push(P::Text(format!("10^{{{}}}", m.at(5))));
            }
        }
        ("pu", "number^") => {
            if m.at(0) == "+-" || m.at(0) == "+/-" {
                out.push(P::Text("\\pm ".to_owned()));
            } else if !m.at(0).is_empty() {
                out.push(P::Text(m.at(0).to_owned()));
            }
            out.extend(go(m.at(1), "pu-9,9", next)?);
            out.push(P::Text(format!("^{{{}}}", m.at(2))));
        }
        ("pu", "space") => out.push(P::Plain("pu-space-1")),
        ("pu", "output") => {
            let unwrap = |slot: &mut Option<String>| -> Parsed<()> {
                let text = slot.clone().unwrap_or_default();
                if let Some((found, used)) = pattern("{(...)}", &text)?
                    && used == text.len()
                {
                    *slot = Some(found.s().to_owned());
                }
                Ok(())
            };
            unwrap(&mut buffer.d)?;
            unwrap(&mut buffer.q)?;
            let degrees = |slot: &mut Option<String>| {
                if let Some(text) = slot {
                    let t = re(r"\x{00B0}C|\^oC|\^\{o\}C").replace_all(text, "{}^{\\circ}C").into_owned();
                    *text = re(r"\x{00B0}F|\^oF|\^\{o\}F").replace_all(&t, "{}^{\\circ}F").into_owned();
                }
            };
            degrees(&mut buffer.d);
            if truthy(&buffer.q) {
                degrees(&mut buffer.q);
                let d = go_opt(&buffer.d, "pu", next)?;
                let q = go_opt(&buffer.q, "pu", next)?;
                if buffer.o.as_deref() == Some("//") {
                    out.push(P::Two("pu-frac", d, q));
                } else {
                    let wide = d.len() > 1 || q.len() > 1;
                    out.extend(d);
                    out.push(P::Plain(if wide { " / " } else { "/" }));
                    out.extend(q);
                }
            } else {
                out.extend(go_opt(&buffer.d, "pu-2", next)?);
            }
            buffer.clear();
        }
        ("pu-2", "cdot") => out.push(P::Plain("tight cdot")),
        ("pu-2", "^(-1)") => add(&mut buffer.rm, &format!("^{{{}}}", m.s())),
        ("pu-2", "space") => out.push(P::Plain("pu-space-2")),
        ("pu-2", "output") => {
            if truthy(&buffer.rm) {
                let rm = buffer.rm.clone().unwrap_or_default();
                match pattern("{(...)}", &rm)? {
                    Some((found, used)) if used == rm.len() => out.extend(go(found.s(), "pu", next)?),
                    _ => out.push(P::One("rm", rm)),
                }
            }
            buffer.clear();
        }
        ("pu-9,9", "output-0") => {
            let text = buffer.text.take().unwrap_or_default();
            if text.len() > 4 {
                let mut a = text.len() % 3;
                if a == 0 {
                    a = 3;
                }
                let mut parts = Vec::new();
                let mut i = text.len() as isize - 3;
                while i > 0 {
                    parts.push(P::Text(text[i as usize..i as usize + 3].to_owned()));
                    parts.push(P::Plain("1000 separator"));
                    i -= 3;
                }
                parts.push(P::Text(text[..a].to_owned()));
                parts.reverse();
                out.extend(parts);
            } else {
                out.push(P::Text(text));
            }
            buffer.clear();
        }
        ("pu-9,9", "output-o") => {
            let text = buffer.text.take().unwrap_or_default();
            if text.len() > 4 {
                let a = text.len() - 3;
                let mut i = 0;
                while i < a {
                    out.push(P::Text(text[i..i + 3].to_owned()));
                    out.push(P::Plain("1000 separator"));
                    i += 3;
                }
                out.push(P::Text(text[i..].to_owned()));
            } else {
                out.push(P::Text(text));
            }
            buffer.clear();
        }
        // What every machine shares.
        (_, "a=") => add(&mut buffer.a, m.s()),
        (_, "b=") => add(&mut buffer.b, m.s()),
        (_, "p=") => add(&mut buffer.p, m.s()),
        (_, "o=") => add(&mut buffer.o, m.s()),
        (_, "o=+p1") => {
            if let O::S(text) = option {
                add(&mut buffer.o, text);
            }
        }
        (_, "q=") => add(&mut buffer.q, m.s()),
        (_, "d=") => add(&mut buffer.d, m.s()),
        (_, "rm=") => add(&mut buffer.rm, m.s()),
        (_, "text=") => add(&mut buffer.text, m.s()),
        (_, "insert") => {
            if let O::S(kind) = option {
                out.push(P::Plain(kind));
            }
        }
        (_, "insert+p1") => {
            if let O::S(kind) = option {
                out.push(P::One(kind, m.s().to_owned()));
            }
        }
        (_, "copy") => out.push(P::Text(m.s().to_owned())),
        (_, "write") => {
            if let O::S(text) = option {
                out.push(P::Text(text.to_owned()));
            }
        }
        (_, "rm") => out.push(P::One("rm", m.s().to_owned())),
        (_, "text") => out.extend(go(m.s(), "text", next)?),
        (_, "tex-math") => out.extend(go(m.s(), "tex-math", next)?),
        (_, "tex-math tight") => out.extend(go(m.s(), "tex-math tight", next)?),
        (_, "bond") => out.push(P::One("bond", if let O::S(kind) = option { kind.to_owned() } else { m.s().to_owned() })),
        (_, "color0-output") => out.push(P::One("color0", m.s().to_owned())),
        (_, "ce") => out.extend(go(m.s(), "ce", next)?),
        (_, "pu") => out.extend(go(m.s(), "pu", next)?),
        (_, "1/2") => {
            let mut text = m.s();
            if text.starts_with(['+', '-']) {
                out.push(P::Text(text[..1].to_owned()));
                text = &text[1..];
            }
            if let Some(n) = re(r"^([0-9]+|\$[a-z]\$|[a-z])/([0-9]+)(\$[a-z]\$|[a-z])?$").captures(text) {
                out.push(P::Frac(n[1].replace('$', ""), n[2].to_owned()));
                if let Some(rest) = n.get(3) {
                    out.push(P::One("tex-math", rest.as_str().replace('$', "")));
                }
            }
        }
        (_, "9,9") => out.extend(go(m.s(), "9,9", next)?),
        _ => return Err(format!("mhchem bug A. Please report. ({name})")),
    }
    Ok(())
}

/// The `ce` machine's `output`: what has gathered in the buffer, as one formula or one arrow.
fn output_ce(buffer: &mut Buffer, entity_follows: O, out: &mut Vec<P>, level: u32) -> Parsed<()> {
    let follows = if let O::I(n) = entity_follows { n } else { 0 };
    if buffer.r.is_none() {
        if !truthy(&buffer.a) && !truthy(&buffer.b) && !truthy(&buffer.p) && !truthy(&buffer.o) && !truthy(&buffer.q) && !truthy(&buffer.d) && follows == 0 {
            return Ok(());
        }
        if buffer.sb {
            out.push(P::Plain("entitySkip"));
        }
        if !truthy(&buffer.o) && !truthy(&buffer.q) && !truthy(&buffer.d) && !truthy(&buffer.b) && !truthy(&buffer.p) && follows != 2 {
            buffer.o = buffer.a.take();
        } else if !truthy(&buffer.o) && !truthy(&buffer.q) && !truthy(&buffer.d) && (truthy(&buffer.b) || truthy(&buffer.p)) {
            buffer.o = buffer.a.take();
            buffer.d = buffer.b.take();
            buffer.q = buffer.p.take();
        } else if truthy(&buffer.o) && buffer.d_type == Some("kv") && pattern("d-oxidation$", buffer.d.as_deref().unwrap_or(""))?.is_some() {
            buffer.d_type = Some("oxidation");
        } else if truthy(&buffer.o) && buffer.d_type == Some("kv") && !truthy(&buffer.q) {
            buffer.d_type = None;
        }
        out.push(P::Five {
            a: go_opt(&buffer.a, "a", level)?,
            b: go_opt(&buffer.b, "bd", level)?,
            p: go_opt(&buffer.p, "pq", level)?,
            o: go_opt(&buffer.o, "o", level)?,
            q: go_opt(&buffer.q, "pq", level)?,
            d: go_opt(&buffer.d, if buffer.d_type == Some("oxidation") { "oxidation" } else { "bd" }, level)?,
            kind: buffer.d_type,
        });
    } else {
        let label = |text: &Option<String>, kind: &Option<String>| -> Parsed<Vec<P>> {
            match kind.as_deref() {
                Some("M") => go_opt(text, "tex-math", level),
                Some("T") => Ok(vec![P::One("text", text.clone().unwrap_or_default())]),
                _ => go_opt(text, "ce", level),
            }
        };
        let rd = label(&buffer.rd, &buffer.rdt)?;
        let rq = label(&buffer.rq, &buffer.rqt)?;
        out.push(P::Arrow { r: buffer.r.clone().unwrap_or_default(), rd, rq });
    }
    buffer.clear();
    Ok(())
}

// ---------------------------------------------------------------- `_mhchemTexify`

fn texify(input: &[P], outer: bool) -> Parsed<String> {
    let mut res = String::new();
    let mut escape = false;
    for item in input {
        match item {
            P::Text(text) => res.push_str(text),
            item => {
                res.push_str(&texify_one(item)?);
                if matches!(item, P::One("1st-level escape", _)) {
                    escape = true;
                }
            }
        }
    }
    if outer && !escape && !res.is_empty() {
        res = format!("{{{res}}}");
    }
    Ok(res)
}

fn inner(input: &[P]) -> Parsed<String> {
    texify(input, false)
}

fn texify_one(item: &P) -> Parsed<String> {
    Ok(match item {
        P::Text(text) => text.clone(),
        P::Five { a, b, p, o, q, d, kind } => {
            let (mut a, b, p, mut o, q, d) = (inner(a)?, inner(b)?, inner(p)?, inner(o)?, inner(q)?, inner(d)?);
            let mut res = String::new();
            if !a.is_empty() {
                if a.starts_with(['+', '-']) {
                    a = format!("{{{a}}}");
                }
                res.push_str(&a);
                res.push_str("\\,");
            }
            if !b.is_empty() || !p.is_empty() {
                res.push_str("{\\vphantom{A}}");
                res.push_str(&format!("^{{\\hphantom{{{b}}}}}_{{\\hphantom{{{p}}}}}"));
                res.push_str("\\mkern-1.5mu");
                res.push_str("{\\vphantom{A}}");
                res.push_str(&format!("^{{\\smash[t]{{\\vphantom{{2}}}}\\llap{{{b}}}}}"));
                res.push_str(&format!("_{{\\vphantom{{2}}\\llap{{\\smash[t]{{{p}}}}}}}"));
            }
            if !o.is_empty() {
                if o.starts_with(['+', '-']) {
                    o = format!("{{{o}}}");
                }
                res.push_str(&o);
            }
            match kind {
                Some("kv") => {
                    if !d.is_empty() || !q.is_empty() {
                        res.push_str("{\\vphantom{A}}");
                    }
                    if !d.is_empty() {
                        res.push_str(&format!("^{{{d}}}"));
                    }
                    if !q.is_empty() {
                        res.push_str(&format!("_{{\\smash[t]{{{q}}}}}"));
                    }
                }
                Some("oxidation") => {
                    if !d.is_empty() {
                        res.push_str("{\\vphantom{A}}");
                        res.push_str(&format!("^{{{d}}}"));
                    }
                    if !q.is_empty() {
                        res.push_str("{\\vphantom{A}}");
                        res.push_str(&format!("_{{\\smash[t]{{{q}}}}}"));
                    }
                }
                _ => {
                    if !q.is_empty() {
                        res.push_str("{\\vphantom{A}}");
                        res.push_str(&format!("_{{\\smash[t]{{{q}}}}}"));
                    }
                    if !d.is_empty() {
                        res.push_str("{\\vphantom{A}}");
                        res.push_str(&format!("^{{{d}}}"));
                    }
                }
            }
            res
        }
        P::One("rm", text) | P::One("roman numeral", text) => format!("\\mathrm{{{text}}}"),
        P::One("text", text) => {
            if text.contains(['^', '_']) {
                format!("\\mathrm{{{}}}", text.replacen(' ', "~", 1).replacen('-', "\\text{-}", 1))
            } else {
                format!("\\text{{{text}}}")
            }
        }
        P::List("state of aggregation", list) => format!("\\mskip2mu {}", inner(list)?),
        P::List("state of aggregation subscript", list) => format!("\\mskip1mu {}", inner(list)?),
        P::One("bond", kind) => bond(kind)?.to_owned(),
        P::Frac(a, b) => {
            let c = format!("\\frac{{{a}}}{{{b}}}");
            format!("\\mathchoice{{\\textstyle{c}}}{{{c}}}{{{c}}}{{{c}}}")
        }
        P::Two("pu-frac", a, b) => {
            let d = format!("\\frac{{{}}}{{{}}}", inner(a)?, inner(b)?);
            format!("\\mathchoice{{\\textstyle{d}}}{{{d}}}{{{d}}}{{{d}}}")
        }
        P::One("tex-math", text) => format!("{text} "),
        P::Two("frac-ce", a, b) => format!("\\frac{{{}}}{{{}}}", inner(a)?, inner(b)?),
        P::Two("overset", a, b) => format!("\\overset{{{}}}{{{}}}", inner(a)?, inner(b)?),
        P::Two("underset", a, b) => format!("\\underset{{{}}}{{{}}}", inner(a)?, inner(b)?),
        P::Two("underbrace", a, b) => format!("\\underbrace{{{}}}_{{{}}}", inner(a)?, inner(b)?),
        P::Color(color, list) => format!("{{\\color{{{color}}}{{{}}}}}", inner(list)?),
        P::One("color0", color) => format!("\\color{{{color}}}"),
        P::Arrow { r, rd, rq } => {
            let (rd, rq) = (inner(rd)?, inner(rq)?);
            let mut arrow = arrow_name(r)?.to_owned();
            if !rd.is_empty() || !rq.is_empty() {
                if matches!(r.as_str(), "<=>" | "<=>>" | "<<=>" | "<-->") {
                    arrow = format!("\\long{arrow}");
                    if !rd.is_empty() {
                        arrow = format!("\\overset{{{rd}}}{{{arrow}}}");
                    }
                    if !rq.is_empty() {
                        let drop = if r == "<-->" { "2mu" } else { "6mu" };
                        arrow = format!("\\underset{{\\lower{drop}{{{rq}}}}}{{{arrow}}}");
                    }
                    format!(" {{}}\\mathrel{{{arrow}}}{{}} ")
                } else {
                    if !rq.is_empty() {
                        arrow.push_str(&format!("[{{{rq}}}]"));
                    }
                    format!(" {{}}\\mathrel{{\\x{arrow}{{{rd}}}}}{{}} ")
                }
            } else {
                format!(" {{}}\\mathrel{{\\long{arrow}}}{{}} ")
            }
        }
        P::One("operator", kind) => operator(kind)?.to_owned(),
        P::One("1st-level escape", text) => format!("{text} "),
        P::Plain("space") => " ".to_owned(),
        P::Plain("tinySkip") => "\\mkern2mu".to_owned(),
        P::Plain("entitySkip") | P::Plain("pu-space-1") => "~".to_owned(),
        P::Plain("pu-space-2") => "\\mkern3mu ".to_owned(),
        P::Plain("1000 separator") => "\\mkern2mu ".to_owned(),
        P::Plain("commaDecimal") => "{,}".to_owned(),
        P::One("comma enumeration L", text) => format!("{{{text}}}\\mkern6mu "),
        P::One("comma enumeration M", text) => format!("{{{text}}}\\mkern3mu "),
        P::One("comma enumeration S", text) => format!("{{{text}}}\\mkern1mu "),
        P::Plain("hyphen") => "\\text{-}".to_owned(),
        P::Plain("addition compound") => "\\,{\\cdot}\\,".to_owned(),
        P::Plain("electron dot") => "\\mkern1mu \\bullet\\mkern1mu ".to_owned(),
        P::Plain("KV x") => "{\\times}".to_owned(),
        P::Plain("prime") => "\\prime ".to_owned(),
        P::Plain("cdot") => "\\cdot ".to_owned(),
        P::Plain("tight cdot") => "\\mkern1mu{\\cdot}\\mkern1mu ".to_owned(),
        P::Plain("times") => "\\times ".to_owned(),
        P::Plain("circa") => "{\\sim}".to_owned(),
        P::Plain("ellipsis") => "\\ldots ".to_owned(),
        P::Plain("/") => "/".to_owned(),
        P::Plain(" / ") => "\\,/\\,".to_owned(),
        _ => return Err("mhchem bug T. Please report.".to_owned()),
    })
}

fn arrow_name(arrow: &str) -> Parsed<&'static str> {
    Ok(match arrow {
        "->" | "\u{2192}" | "\u{27F6}" => "rightarrow",
        "<-" => "leftarrow",
        "<->" => "leftrightarrow",
        "<-->" => "leftrightarrows",
        "<=>" | "\u{21CC}" => "rightleftharpoons",
        "<=>>" => "Rightleftharpoons",
        "<<=>" => "Leftrightharpoons",
        _ => return Err("mhchem bug T. Please report.".to_owned()),
    })
}

fn bond(kind: &str) -> Parsed<&'static str> {
    Ok(match kind {
        "-" | "1" => "{-}",
        "=" | "2" => "{=}",
        "#" | "3" | "\u{2261}" => "{\\equiv}",
        "~" => "{\\tripledash}",
        "~-" => "{\\rlap{\\lower.1em{-}}\\raise.1em{\\tripledash}}",
        "~=" | "~--" => "{\\rlap{\\lower.2em{-}}\\rlap{\\raise.2em{\\tripledash}}-}",
        "-~-" => "{\\rlap{\\lower.2em{-}}\\rlap{\\raise.2em{-}}\\tripledash}",
        "..." => "{{\\cdot}{\\cdot}{\\cdot}}",
        "...." => "{{\\cdot}{\\cdot}{\\cdot}{\\cdot}}",
        "->" => "{\\rightarrow}",
        "<-" => "{\\leftarrow}",
        "<" => "{<}",
        ">" => "{>}",
        other => return Err(format!("mhchem Error. Unknown bond type ({other})")),
    })
}

fn operator(kind: &str) -> Parsed<&'static str> {
    Ok(match kind {
        "+" => " {}+{} ",
        "-" => " {}-{} ",
        "=" => " {}={} ",
        "<" => " {}<{} ",
        ">" => " {}>{} ",
        "<<" => " {}\\ll{} ",
        ">>" => " {}\\gg{} ",
        "\\pm" => " {}\\pm{} ",
        "\\approx" | "$\\approx$" => " {}\\approx{} ",
        "v" | "(v)" => " \\downarrow{} ",
        "^" | "(^)" => " \\uparrow{} ",
        _ => return Err("mhchem bug T. Please report.".to_owned()),
    })
}

// ---------------------------------------------------------------- the tables, from `_mhchemParser.stateMachines`

#[derive(Clone, Copy)]
enum O {
    N,
    S(&'static str),
    I(i32),
    B(bool),
}

struct A(&'static str, O);

/// States, actions, next state, revisit, toContinue.
struct E(&'static str, &'static [A], Option<&'static str>, bool, bool);

/// A pattern, or several with `|`, and what to do with it in each state.
struct Row(&'static str, &'static [E]);

static M_CE: &[Row] = &[
    Row("empty", &[
        E("*", &[A("output", O::N)], None, false, false),
    ]),
    Row("else", &[
        E("0|1|2", &[A("beginsWithBond=false", O::N)], None, true, true),
    ]),
    Row("oxidation$", &[
        E("0", &[A("oxidation-output", O::N)], None, false, false),
    ]),
    Row("CMT", &[
        E("r", &[A("rdt=", O::N)], Some("rt"), false, false),
        E("rd", &[A("rqt=", O::N)], Some("rdt"), false, false),
    ]),
    Row("arrowUpDown", &[
        E("0|1|2|as", &[A("sb=false", O::N), A("output", O::N), A("operator", O::N)], Some("1"), false, false),
    ]),
    Row("uprightEntities", &[
        E("0|1|2", &[A("o=", O::N), A("output", O::N)], Some("1"), false, false),
    ]),
    Row("orbital", &[
        E("0|1|2|3", &[A("o=", O::N)], Some("o"), false, false),
    ]),
    Row("->", &[
        E("0|1|2|3", &[A("r=", O::N)], Some("r"), false, false),
        E("a|as", &[A("output", O::N), A("r=", O::N)], Some("r"), false, false),
        E("*", &[A("output", O::N), A("r=", O::N)], Some("r"), false, false),
    ]),
    Row("+", &[
        E("o", &[A("d= kv", O::N)], Some("d"), false, false),
        E("d|D", &[A("d=", O::N)], Some("d"), false, false),
        E("q", &[A("d=", O::N)], Some("qd"), false, false),
        E("qd|qD", &[A("d=", O::N)], Some("qd"), false, false),
        E("dq", &[A("output", O::N), A("d=", O::N)], Some("d"), false, false),
        E("3", &[A("sb=false", O::N), A("output", O::N), A("operator", O::N)], Some("0"), false, false),
    ]),
    Row("amount", &[
        E("0|2", &[A("a=", O::N)], Some("a"), false, false),
    ]),
    Row("pm-operator", &[
        E("0|1|2|a|as", &[A("sb=false", O::N), A("output", O::N), A("operator", O::S("\\pm"))], Some("0"), false, false),
    ]),
    Row("operator", &[
        E("0|1|2|a|as", &[A("sb=false", O::N), A("output", O::N), A("operator", O::N)], Some("0"), false, false),
    ]),
    Row("-$", &[
        E("o|q", &[A("charge or bond", O::N), A("output", O::N)], Some("qd"), false, false),
        E("d", &[A("d=", O::N)], Some("d"), false, false),
        E("D", &[A("output", O::N), A("bond", O::S("-"))], Some("3"), false, false),
        E("q", &[A("d=", O::N)], Some("qd"), false, false),
        E("qd", &[A("d=", O::N)], Some("qd"), false, false),
        E("qD|dq", &[A("output", O::N), A("bond", O::S("-"))], Some("3"), false, false),
    ]),
    Row("-9", &[
        E("3|o", &[A("output", O::N), A("insert", O::S("hyphen"))], Some("3"), false, false),
    ]),
    Row("- orbital overlap", &[
        E("o", &[A("output", O::N), A("insert", O::S("hyphen"))], Some("2"), false, false),
        E("d", &[A("output", O::N), A("insert", O::S("hyphen"))], Some("2"), false, false),
    ]),
    Row("-", &[
        E("0|1|2", &[A("output", O::I(1)), A("beginsWithBond=true", O::N), A("bond", O::S("-"))], Some("3"), false, false),
        E("3", &[A("bond", O::S("-"))], None, false, false),
        E("a", &[A("output", O::N), A("insert", O::S("hyphen"))], Some("2"), false, false),
        E("as", &[A("output", O::I(2)), A("bond", O::S("-"))], Some("3"), false, false),
        E("b", &[A("b=", O::N)], None, false, false),
        E("o", &[A("- after o/d", O::B(false))], Some("2"), false, false),
        E("q", &[A("- after o/d", O::B(false))], Some("2"), false, false),
        E("d|qd|dq", &[A("- after o/d", O::B(true))], Some("2"), false, false),
        E("D|qD|p", &[A("output", O::N), A("bond", O::S("-"))], Some("3"), false, false),
    ]),
    Row("amount2", &[
        E("1|3", &[A("a=", O::N)], Some("a"), false, false),
    ]),
    Row("letters", &[
        E("0|1|2|3|a|as|b|p|bp|o", &[A("o=", O::N)], Some("o"), false, false),
        E("q|dq", &[A("output", O::N), A("o=", O::N)], Some("o"), false, false),
        E("d|D|qd|qD", &[A("o after d", O::N)], Some("o"), false, false),
    ]),
    Row("digits", &[
        E("o", &[A("q=", O::N)], Some("q"), false, false),
        E("d|D", &[A("q=", O::N)], Some("dq"), false, false),
        E("q", &[A("output", O::N), A("o=", O::N)], Some("o"), false, false),
        E("a", &[A("o=", O::N)], Some("o"), false, false),
    ]),
    Row("space A", &[
        E("b|p|bp", &[], None, false, false),
    ]),
    Row("space", &[
        E("a", &[], Some("as"), false, false),
        E("0", &[A("sb=false", O::N)], None, false, false),
        E("1|2", &[A("sb=true", O::N)], None, false, false),
        E("r|rt|rd|rdt|rdq", &[A("output", O::N)], Some("0"), false, false),
        E("*", &[A("output", O::N), A("sb=true", O::N)], Some("1"), false, false),
    ]),
    Row("1st-level escape", &[
        E("1|2", &[A("output", O::N), A("insert+p1", O::S("1st-level escape"))], None, false, false),
        E("*", &[A("output", O::N), A("insert+p1", O::S("1st-level escape"))], Some("0"), false, false),
    ]),
    Row("[(...)]", &[
        E("r|rt", &[A("rd=", O::N)], Some("rd"), false, false),
        E("rd|rdt", &[A("rq=", O::N)], Some("rdq"), false, false),
    ]),
    Row("...", &[
        E("o|d|D|dq|qd|qD", &[A("output", O::N), A("bond", O::S("..."))], Some("3"), false, false),
        E("*", &[A("output", O::I(1)), A("insert", O::S("ellipsis"))], Some("1"), false, false),
    ]),
    Row(". __* ", &[
        E("*", &[A("output", O::N), A("insert", O::S("addition compound"))], Some("1"), false, false),
    ]),
    Row("state of aggregation $", &[
        E("*", &[A("output", O::N), A("state of aggregation", O::N)], Some("1"), false, false),
    ]),
    Row("{[(", &[
        E("a|as|o", &[A("o=", O::N), A("output", O::N), A("parenthesisLevel++", O::N)], Some("2"), false, false),
        E("0|1|2|3", &[A("o=", O::N), A("output", O::N), A("parenthesisLevel++", O::N)], Some("2"), false, false),
        E("*", &[A("output", O::N), A("o=", O::N), A("output", O::N), A("parenthesisLevel++", O::N)], Some("2"), false, false),
    ]),
    Row(")]}", &[
        E("0|1|2|3|b|p|bp|o", &[A("o=", O::N), A("parenthesisLevel--", O::N)], Some("o"), false, false),
        E("a|as|d|D|q|qd|qD|dq", &[A("output", O::N), A("o=", O::N), A("parenthesisLevel--", O::N)], Some("o"), false, false),
    ]),
    Row(", ", &[
        E("*", &[A("output", O::N), A("comma", O::N)], Some("0"), false, false),
    ]),
    Row("^_", &[
        E("*", &[], None, false, false),
    ]),
    Row("^{(...)}|^($...$)", &[
        E("0|1|2|as", &[A("b=", O::N)], Some("b"), false, false),
        E("p", &[A("b=", O::N)], Some("bp"), false, false),
        E("3|o", &[A("d= kv", O::N)], Some("D"), false, false),
        E("q", &[A("d=", O::N)], Some("qD"), false, false),
        E("d|D|qd|qD|dq", &[A("output", O::N), A("d=", O::N)], Some("D"), false, false),
    ]),
    Row("^a|^\\x{}{}|^\\x{}|^\\x|'", &[
        E("0|1|2|as", &[A("b=", O::N)], Some("b"), false, false),
        E("p", &[A("b=", O::N)], Some("bp"), false, false),
        E("3|o", &[A("d= kv", O::N)], Some("d"), false, false),
        E("q", &[A("d=", O::N)], Some("qd"), false, false),
        E("d|qd|D|qD", &[A("d=", O::N)], None, false, false),
        E("dq", &[A("output", O::N), A("d=", O::N)], Some("d"), false, false),
    ]),
    Row("_{(state of aggregation)}$", &[
        E("d|D|q|qd|qD|dq", &[A("output", O::N), A("q=", O::N)], Some("q"), false, false),
    ]),
    Row("_{(...)}|_($...$)|_9|_\\x{}{}|_\\x{}|_\\x", &[
        E("0|1|2|as", &[A("p=", O::N)], Some("p"), false, false),
        E("b", &[A("p=", O::N)], Some("bp"), false, false),
        E("3|o", &[A("q=", O::N)], Some("q"), false, false),
        E("d|D", &[A("q=", O::N)], Some("dq"), false, false),
        E("q|qd|qD|dq", &[A("output", O::N), A("q=", O::N)], Some("q"), false, false),
    ]),
    Row("=<>", &[
        E("0|1|2|3|a|as|o|q|d|D|qd|qD|dq", &[A("output", O::I(2)), A("bond", O::N)], Some("3"), false, false),
    ]),
    Row("#", &[
        E("0|1|2|3|a|as|o", &[A("output", O::I(2)), A("bond", O::S("#"))], Some("3"), false, false),
    ]),
    Row("{}^", &[
        E("*", &[A("output", O::I(1)), A("insert", O::S("tinySkip"))], Some("1"), false, false),
    ]),
    Row("{}", &[
        E("*", &[A("output", O::I(1))], Some("1"), false, false),
    ]),
    Row("{...}", &[
        E("0|1|2|3|a|as|b|p|bp", &[A("o=", O::N)], Some("o"), false, false),
        E("o|d|D|q|qd|qD|dq", &[A("output", O::N), A("o=", O::N)], Some("o"), false, false),
    ]),
    Row("$...$", &[
        E("a", &[A("a=", O::N)], None, false, false),
        E("0|1|2|3|as|b|p|bp|o", &[A("o=", O::N)], Some("o"), false, false),
        E("as|o", &[A("o=", O::N)], None, false, false),
        E("q|d|D|qd|qD|dq", &[A("output", O::N), A("o=", O::N)], Some("o"), false, false),
    ]),
    Row("\\bond{(...)}", &[
        E("*", &[A("output", O::I(2)), A("bond", O::N)], Some("3"), false, false),
    ]),
    Row("\\frac{(...)}", &[
        E("*", &[A("output", O::I(1)), A("frac-output", O::N)], Some("3"), false, false),
    ]),
    Row("\\overset{(...)}", &[
        E("*", &[A("output", O::I(2)), A("overset-output", O::N)], Some("3"), false, false),
    ]),
    Row("\\underset{(...)}", &[
        E("*", &[A("output", O::I(2)), A("underset-output", O::N)], Some("3"), false, false),
    ]),
    Row("\\underbrace{(...)}", &[
        E("*", &[A("output", O::I(2)), A("underbrace-output", O::N)], Some("3"), false, false),
    ]),
    Row("\\color{(...)}{(...)}", &[
        E("*", &[A("output", O::I(2)), A("color-output", O::N)], Some("3"), false, false),
    ]),
    Row("\\color{(...)}", &[
        E("*", &[A("output", O::I(2)), A("color0-output", O::N)], None, false, false),
    ]),
    Row("\\ce{(...)}", &[
        E("*", &[A("output", O::I(2)), A("ce", O::N)], Some("3"), false, false),
    ]),
    Row("\\,", &[
        E("*", &[A("output", O::I(1)), A("copy", O::N)], Some("1"), false, false),
    ]),
    Row("\\pu{(...)}", &[
        E("*", &[A("output", O::N), A("write", O::S("{")), A("pu", O::N), A("write", O::S("}"))], Some("3"), false, false),
    ]),
    Row("\\x{}{}|\\x{}|\\x", &[
        E("0|1|2|3|a|as|b|p|bp|o|c0", &[A("o=", O::N), A("output", O::N)], Some("3"), false, false),
        E("*", &[A("output", O::N), A("o=", O::N), A("output", O::N)], Some("3"), false, false),
    ]),
    Row("others", &[
        E("*", &[A("output", O::I(1)), A("copy", O::N)], Some("3"), false, false),
    ]),
    Row("else2", &[
        E("a", &[A("a to o", O::N)], Some("o"), true, false),
        E("as", &[A("output", O::N), A("sb=true", O::N)], Some("1"), true, false),
        E("r|rt|rd|rdt|rdq", &[A("output", O::N)], Some("0"), true, false),
        E("*", &[A("output", O::N), A("copy", O::N)], Some("3"), false, false),
    ]),
];

static M_A: &[Row] = &[
    Row("empty", &[
        E("*", &[], None, false, false),
    ]),
    Row("1/2$", &[
        E("0", &[A("1/2", O::N)], None, false, false),
    ]),
    Row("else", &[
        E("0", &[], Some("1"), true, false),
    ]),
    Row("${(...)}$__$(...)$", &[
        E("*", &[A("tex-math tight", O::N)], Some("1"), false, false),
    ]),
    Row(",", &[
        E("*", &[A("insert", O::S("commaDecimal"))], None, false, false),
    ]),
    Row("else2", &[
        E("*", &[A("copy", O::N)], None, false, false),
    ]),
];

static M_O: &[Row] = &[
    Row("empty", &[
        E("*", &[], None, false, false),
    ]),
    Row("1/2$", &[
        E("0", &[A("1/2", O::N)], None, false, false),
    ]),
    Row("else", &[
        E("0", &[], Some("1"), true, false),
    ]),
    Row("letters", &[
        E("*", &[A("rm", O::N)], None, false, false),
    ]),
    Row("\\ca", &[
        E("*", &[A("insert", O::S("circa"))], None, false, false),
    ]),
    Row("\\pu{(...)}", &[
        E("*", &[A("write", O::S("{")), A("pu", O::N), A("write", O::S("}"))], None, false, false),
    ]),
    Row("\\x{}{}|\\x{}|\\x", &[
        E("*", &[A("copy", O::N)], None, false, false),
    ]),
    Row("${(...)}$__$(...)$", &[
        E("*", &[A("tex-math", O::N)], None, false, false),
    ]),
    Row("{(...)}", &[
        E("*", &[A("write", O::S("{")), A("text", O::N), A("write", O::S("}"))], None, false, false),
    ]),
    Row("else2", &[
        E("*", &[A("copy", O::N)], None, false, false),
    ]),
];

static M_TEXT: &[Row] = &[
    Row("empty", &[
        E("*", &[A("output", O::N)], None, false, false),
    ]),
    Row("{...}", &[
        E("*", &[A("text=", O::N)], None, false, false),
    ]),
    Row("${(...)}$__$(...)$", &[
        E("*", &[A("tex-math", O::N)], None, false, false),
    ]),
    Row("\\greek", &[
        E("*", &[A("output", O::N), A("rm", O::N)], None, false, false),
    ]),
    Row("\\pu{(...)}", &[
        E("*", &[A("output", O::N), A("write", O::S("{")), A("pu", O::N), A("write", O::S("}"))], None, false, false),
    ]),
    Row("\\,|\\x{}{}|\\x{}|\\x", &[
        E("*", &[A("output", O::N), A("copy", O::N)], None, false, false),
    ]),
    Row("else", &[
        E("*", &[A("text=", O::N)], None, false, false),
    ]),
];

static M_PQ: &[Row] = &[
    Row("empty", &[
        E("*", &[], None, false, false),
    ]),
    Row("state of aggregation $", &[
        E("*", &[A("state of aggregation", O::N)], None, false, false),
    ]),
    Row("i$", &[
        E("0", &[], Some("!f"), true, false),
    ]),
    Row("(KV letters),", &[
        E("0", &[A("rm", O::N)], Some("0"), false, false),
    ]),
    Row("formula$", &[
        E("0", &[], Some("f"), true, false),
    ]),
    Row("1/2$", &[
        E("0", &[A("1/2", O::N)], None, false, false),
    ]),
    Row("else", &[
        E("0", &[], Some("!f"), true, false),
    ]),
    Row("${(...)}$__$(...)$", &[
        E("*", &[A("tex-math", O::N)], None, false, false),
    ]),
    Row("{(...)}", &[
        E("*", &[A("text", O::N)], None, false, false),
    ]),
    Row("a-z", &[
        E("f", &[A("tex-math", O::N)], None, false, false),
    ]),
    Row("letters", &[
        E("*", &[A("rm", O::N)], None, false, false),
    ]),
    Row("-9.,9", &[
        E("*", &[A("9,9", O::N)], None, false, false),
    ]),
    Row(",", &[
        E("*", &[A("insert+p1", O::S("comma enumeration S"))], None, false, false),
    ]),
    Row("\\color{(...)}{(...)}", &[
        E("*", &[A("color-output", O::N)], None, false, false),
    ]),
    Row("\\color{(...)}", &[
        E("*", &[A("color0-output", O::N)], None, false, false),
    ]),
    Row("\\ce{(...)}", &[
        E("*", &[A("ce", O::N)], None, false, false),
    ]),
    Row("\\pu{(...)}", &[
        E("*", &[A("write", O::S("{")), A("pu", O::N), A("write", O::S("}"))], None, false, false),
    ]),
    Row("\\,|\\x{}{}|\\x{}|\\x", &[
        E("*", &[A("copy", O::N)], None, false, false),
    ]),
    Row("else2", &[
        E("*", &[A("copy", O::N)], None, false, false),
    ]),
];

static M_BD: &[Row] = &[
    Row("empty", &[
        E("*", &[], None, false, false),
    ]),
    Row("x$", &[
        E("0", &[], Some("!f"), true, false),
    ]),
    Row("formula$", &[
        E("0", &[], Some("f"), true, false),
    ]),
    Row("else", &[
        E("0", &[], Some("!f"), true, false),
    ]),
    Row("-9.,9 no missing 0", &[
        E("*", &[A("9,9", O::N)], None, false, false),
    ]),
    Row(".", &[
        E("*", &[A("insert", O::S("electron dot"))], None, false, false),
    ]),
    Row("a-z", &[
        E("f", &[A("tex-math", O::N)], None, false, false),
    ]),
    Row("x", &[
        E("*", &[A("insert", O::S("KV x"))], None, false, false),
    ]),
    Row("letters", &[
        E("*", &[A("rm", O::N)], None, false, false),
    ]),
    Row("'", &[
        E("*", &[A("insert", O::S("prime"))], None, false, false),
    ]),
    Row("${(...)}$__$(...)$", &[
        E("*", &[A("tex-math", O::N)], None, false, false),
    ]),
    Row("{(...)}", &[
        E("*", &[A("text", O::N)], None, false, false),
    ]),
    Row("\\color{(...)}{(...)}", &[
        E("*", &[A("color-output", O::N)], None, false, false),
    ]),
    Row("\\color{(...)}", &[
        E("*", &[A("color0-output", O::N)], None, false, false),
    ]),
    Row("\\ce{(...)}", &[
        E("*", &[A("ce", O::N)], None, false, false),
    ]),
    Row("\\pu{(...)}", &[
        E("*", &[A("write", O::S("{")), A("pu", O::N), A("write", O::S("}"))], None, false, false),
    ]),
    Row("\\,|\\x{}{}|\\x{}|\\x", &[
        E("*", &[A("copy", O::N)], None, false, false),
    ]),
    Row("else2", &[
        E("*", &[A("copy", O::N)], None, false, false),
    ]),
];

static M_OXIDATION: &[Row] = &[
    Row("empty", &[
        E("*", &[A("roman-numeral", O::N)], None, false, false),
    ]),
    Row("pm-operator", &[
        E("*", &[A("o=+p1", O::S("\\pm"))], None, false, false),
    ]),
    Row("else", &[
        E("*", &[A("o=", O::N)], None, false, false),
    ]),
];

static M_TEX_MATH: &[Row] = &[
    Row("empty", &[
        E("*", &[A("output", O::N)], None, false, false),
    ]),
    Row("\\ce{(...)}", &[
        E("*", &[A("output", O::N), A("ce", O::N)], None, false, false),
    ]),
    Row("\\pu{(...)}", &[
        E("*", &[A("output", O::N), A("write", O::S("{")), A("pu", O::N), A("write", O::S("}"))], None, false, false),
    ]),
    Row("{...}|\\,|\\x{}{}|\\x{}|\\x", &[
        E("*", &[A("o=", O::N)], None, false, false),
    ]),
    Row("else", &[
        E("*", &[A("o=", O::N)], None, false, false),
    ]),
];

static M_TEX_MATH_TIGHT: &[Row] = &[
    Row("empty", &[
        E("*", &[A("output", O::N)], None, false, false),
    ]),
    Row("\\ce{(...)}", &[
        E("*", &[A("output", O::N), A("ce", O::N)], None, false, false),
    ]),
    Row("\\pu{(...)}", &[
        E("*", &[A("output", O::N), A("write", O::S("{")), A("pu", O::N), A("write", O::S("}"))], None, false, false),
    ]),
    Row("{...}|\\,|\\x{}{}|\\x{}|\\x", &[
        E("*", &[A("o=", O::N)], None, false, false),
    ]),
    Row("-|+", &[
        E("*", &[A("tight operator", O::N)], None, false, false),
    ]),
    Row("else", &[
        E("*", &[A("o=", O::N)], None, false, false),
    ]),
];

static M_9_9: &[Row] = &[
    Row("empty", &[
        E("*", &[], None, false, false),
    ]),
    Row(",", &[
        E("*", &[A("comma", O::N)], None, false, false),
    ]),
    Row("else", &[
        E("*", &[A("copy", O::N)], None, false, false),
    ]),
];

static M_PU: &[Row] = &[
    Row("empty", &[
        E("*", &[A("output", O::N)], None, false, false),
    ]),
    Row("space$", &[
        E("*", &[A("output", O::N), A("space", O::N)], None, false, false),
    ]),
    Row("{[(|)]}", &[
        E("0|a", &[A("copy", O::N)], None, false, false),
    ]),
    Row("(-)(9)^(-9)", &[
        E("0", &[A("number^", O::N)], Some("a"), false, false),
    ]),
    Row("(-)(9.,9)(e)(99)", &[
        E("0", &[A("enumber", O::N)], Some("a"), false, false),
    ]),
    Row("space", &[
        E("0|a", &[], None, false, false),
    ]),
    Row("pm-operator", &[
        E("0|a", &[A("operator", O::S("\\pm"))], Some("0"), false, false),
    ]),
    Row("operator", &[
        E("0|a", &[A("copy", O::N)], Some("0"), false, false),
    ]),
    Row("//", &[
        E("d", &[A("o=", O::N)], Some("/"), false, false),
    ]),
    Row("/", &[
        E("d", &[A("o=", O::N)], Some("/"), false, false),
    ]),
    Row("{...}|else", &[
        E("0|d", &[A("d=", O::N)], Some("d"), false, false),
        E("a", &[A("space", O::N), A("d=", O::N)], Some("d"), false, false),
        E("/|q", &[A("q=", O::N)], Some("q"), false, false),
    ]),
];

static M_PU_2: &[Row] = &[
    Row("empty", &[
        E("*", &[A("output", O::N)], None, false, false),
    ]),
    Row("*", &[
        E("*", &[A("output", O::N), A("cdot", O::N)], Some("0"), false, false),
    ]),
    Row("\\x", &[
        E("*", &[A("rm=", O::N)], None, false, false),
    ]),
    Row("space", &[
        E("*", &[A("output", O::N), A("space", O::N)], Some("0"), false, false),
    ]),
    Row("^{(...)}|^(-1)", &[
        E("1", &[A("^(-1)", O::N)], None, false, false),
    ]),
    Row("-9.,9", &[
        E("0", &[A("rm=", O::N)], Some("0"), false, false),
        E("1", &[A("^(-1)", O::N)], Some("0"), false, false),
    ]),
    Row("{...}|else", &[
        E("*", &[A("rm=", O::N)], Some("1"), false, false),
    ]),
];

static M_PU_9_9: &[Row] = &[
    Row("empty", &[
        E("0", &[A("output-0", O::N)], None, false, false),
        E("o", &[A("output-o", O::N)], None, false, false),
    ]),
    Row(",", &[
        E("0", &[A("output-0", O::N), A("comma", O::N)], Some("o"), false, false),
    ]),
    Row(".", &[
        E("0", &[A("output-0", O::N), A("copy", O::N)], Some("o"), false, false),
    ]),
    Row("else", &[
        E("*", &[A("text=", O::N)], None, false, false),
    ]),
];
